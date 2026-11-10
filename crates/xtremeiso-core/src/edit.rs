//! Modifica di un'immagine esistente: si rilegge l'albero, si applicano le modifiche e si riscrive tutto.

use crate::{
    boot::{BootEntry, Emulation, Platform},
    reader::{Image, Names},
    tree::{Kind, Node, Tree},
    writer::{write_tree, IsoOptions},
    Error, Result,
};
use std::{
    io::{Read, Seek, Write},
    path::Path,
};

pub struct Editor<R> {
    img: R,
    tree: Tree,
    opts: IsoOptions,
}

fn check_name(n: &str) -> Result<()> {
    if n.is_empty() || n == "." || n == ".." || n.contains(['/', '\0']) {
        return Err(Error::Unsupported(format!("nome non valido: {n:?}")));
    }
    Ok(())
}

impl<R: Read + Seek> Editor<R> {
    /// Opzioni iniziali: quelle dell'immagine (volume id, Joliet, Rock Ridge); livello 3.
    pub fn open(r: R, names: Names) -> Result<Self> {
        let mut img = Image::open(r)?;
        let entries = img.entries(names)?;
        let mut tree = Tree::from_image(img.root_entry(), &entries);
        // alcuni tool (genisoimage) espongono il catalogo come file: ne scriviamo uno nuovo, il vecchio sarebbe orfano
        if let Some(cat) = img.catalog_lba() {
            let stale = entries
                .iter()
                .find(|e| e.kind == Kind::File && e.extents.first().is_some_and(|x| x.0 == cat));
            if let Some(n) = stale.and_then(|e| tree.find(&e.path)) {
                let p = tree.nodes[n].parent;
                tree.nodes[p].children.retain(|&c| c != n);
            }
        }
        let i = img.info().clone();
        let mut boot = vec![];
        for b in img.boot_entries()? {
            let (Some(path), Some(platform)) = (b.path.clone(), Platform::from_id(b.platform))
            else {
                tracing::warn!("voce di boot (LBA {}) non conservata: immagine fuori dall'albero o piattaforma sconosciuta", b.rba);
                continue;
            };
            boot.push(BootEntry {
                path,
                platform,
                emulation: match b.media & 0xF {
                    0 => Emulation::None,
                    4 => Emulation::HardDisk,
                    _ => Emulation::Floppy,
                },
                bootable: b.bootable,
                load_segment: b.load_segment,
                load_size: Some(b.sector_count),
                info_table: b.info_table,
            });
        }
        // MBR ibrido (avvio da USB): si conserva il codice; se l'LBA a 432..440 punta alla prima
        // immagine BIOS la si ricalcola (code <= 432 byte), altrimenti si tengono 440 byte
        let m = img.read_bytes(0, 512)?;
        let hybrid_mbr = (m[510..512] == [0x55, 0xAA] && (m[450] != 0 || m[466] != 0)).then(|| {
            let bios = boot
                .iter()
                .find(|b| b.platform == Platform::Bios)
                .map(|b| b.path.clone());
            let lba = u64::from_le_bytes(m[432..440].try_into().unwrap());
            let is_patched = bios.is_some() && lba != 0;
            m[..if is_patched { 432 } else { 440 }].to_vec()
        });
        let opts = IsoOptions {
            hybrid_mbr,
            boot,
            joliet: i.joliet,
            rock_ridge: i.rock_ridge,
            volume_id: i.volume_id,
            system_id: i.system_id,
            volume_set_id: i.volume_set_id,
            publisher: i.publisher,
            preparer: i.preparer,
            application: i.application,
            ..Default::default()
        };
        Ok(Self {
            img: img.into_inner(),
            tree,
            opts,
        })
    }

    /// Da impostare prima di `add` (Rock Ridge decide se i symlink locali vengono inclusi).
    pub fn options_mut(&mut self) -> &mut IsoOptions {
        &mut self.opts
    }

    fn lookup(&self, path: &str) -> Result<usize> {
        self.tree
            .find(path)
            .ok_or_else(|| Error::Unsupported(format!("{path}: non presente nell'immagine")))
    }

    pub fn remove(&mut self, path: &str) -> Result<()> {
        let n = self.lookup(path)?;
        if n == 0 {
            return Err(Error::Unsupported("la radice non si può rimuovere".into()));
        }
        let p = self.tree.nodes[n].parent;
        self.tree.nodes[p].children.retain(|&c| c != n);
        Ok(())
    }

    pub fn rename(&mut self, path: &str, new_name: &str) -> Result<()> {
        check_name(new_name)?;
        let n = self.lookup(path)?;
        if n == 0 {
            return Err(Error::Unsupported("la radice non si può rinominare".into()));
        }
        let p = self.tree.nodes[n].parent;
        if self.tree.nodes[p]
            .children
            .iter()
            .any(|&c| c != n && self.tree.nodes[c].name == new_name.as_bytes())
        {
            return Err(Error::Unsupported(format!("{new_name}: esiste già")));
        }
        self.tree.nodes[n].name = new_name.as_bytes().to_vec();
        Ok(())
    }

    /// Aggiunge un file o una cartella locale dentro `dest_dir` (le cartelle mancanti vengono create).
    pub fn add(&mut self, local: &Path, dest_dir: &str) -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let name = local
            .file_name()
            .ok_or_else(|| Error::Unsupported(format!("{}: nome non valido", local.display())))?;
        let mut cur = 0;
        for c in dest_dir.split('/').filter(|c| !c.is_empty()) {
            check_name(c)?;
            cur = match self.tree.nodes[cur]
                .children
                .iter()
                .find(|&&k| self.tree.nodes[k].name == c.as_bytes())
            {
                Some(&k) if self.tree.nodes[k].kind == Kind::Dir => k,
                Some(_) => {
                    return Err(Error::Unsupported(format!(
                        "{c}: esiste ed è un file, non una cartella"
                    )))
                }
                None => self.mkdir(cur, c),
            };
        }
        if self.tree.nodes[cur]
            .children
            .iter()
            .any(|&k| self.tree.nodes[k].name == name.as_bytes())
        {
            return Err(Error::Unsupported(format!(
                "{}: esiste già in {dest_dir}",
                name.to_string_lossy()
            )));
        }
        let depth = self.tree.depth(cur) + 1;
        self.tree.add_fs(
            cur,
            local,
            name.as_bytes().to_vec(),
            depth,
            self.opts.rock_ridge,
        )
    }

    fn mkdir(&mut self, parent: usize, name: &str) -> usize {
        let mtime = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        let id = self.tree.nodes.len();
        self.tree
            .nodes
            .push(Node::new_dir(name.as_bytes().to_vec(), parent, mtime));
        self.tree.nodes[parent].children.push(id);
        id
    }

    /// Scrive la nuova immagine (non sovrascrivere il file sorgente: serve un file diverso).
    pub fn write<W: Write>(mut self, out: W) -> Result<()> {
        write_tree(self.tree, out, &self.opts, &mut self.img)
    }
}
