//! Scansione della cartella sorgente in un albero di nodi (arena, radice = 0).

use crate::{reader::Entry, udf::UdfNode, Error, Result};
use std::{
    fs,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Dir,
    File,
    Symlink,
}

/// Da dove arrivano i byte del file.
pub enum Data {
    /// File locale (`Node::src`).
    Path,
    /// Intervalli (offset, lunghezza) in byte di un'immagine esistente.
    Image(Vec<(u64, u64)>),
}

pub struct Node {
    pub data: Data,
    pub udf: UdfNode,
    pub name: Vec<u8>,
    pub kind: Kind,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub mtime: i64,
    pub atime: i64,
    pub ctime: i64,
    pub size: u64,
    pub src: PathBuf,
    pub target: Vec<u8>,
    pub parent: usize,
    pub children: Vec<usize>,
    // calcolati dopo: indice 0 = ISO9660, 1 = Joliet
    pub kids: [Vec<usize>; 2],
    pub ids: [Vec<u8>; 2],
    pub extent: u32,
    pub dir_extent: [u32; 2],
    pub dir_size: [u32; 2],
}

pub struct Tree {
    pub nodes: Vec<Node>,
    /// Profondità massima delle cartelle (radice = 1): 8 con ISO9660, illimitata con UDF puro.
    pub max_depth: usize,
}

/// ISO9660: la gerarchia non può superare 8 livelli (radice = 1).
/// Limite della gerarchia ISO9660 (radice = 1).
pub const ISO_MAX_DEPTH: usize = 8;

impl Node {
    /// Cartella vuota creata a mano (non esiste su disco).
    pub fn new_dir(name: Vec<u8>, parent: usize, mtime: i64) -> Node {
        Node {
            data: Data::Path,
            udf: UdfNode::default(),
            name,
            kind: Kind::Dir,
            mode: 0o040755,
            nlink: 2,
            uid: 0,
            gid: 0,
            mtime,
            atime: mtime,
            ctime: mtime,
            size: 0,
            src: PathBuf::new(),
            target: vec![],
            parent,
            children: vec![],
            kids: [vec![], vec![]],
            ids: [vec![], vec![]],
            extent: 0,
            dir_extent: [0; 2],
            dir_size: [0; 2],
        }
    }
}

impl Tree {
    pub fn scan(root: &Path, rock_ridge: bool, max_depth: usize) -> Result<Tree> {
        let meta = fs::metadata(root).map_err(|e| Error::path(root, e))?;
        if !meta.is_dir() {
            return Err(Error::Unsupported(format!(
                "{}: non è una cartella",
                root.display()
            )));
        }
        let mut t = Tree {
            nodes: vec![],
            max_depth: ISO_MAX_DEPTH,
        };
        t.max_depth = max_depth;
        t.push(root, Vec::new(), &meta, Kind::Dir, vec![], 0);
        t.fill(0, 1, rock_ridge)?;
        Ok(t)
    }

    fn push(
        &mut self,
        src: &Path,
        name: Vec<u8>,
        m: &fs::Metadata,
        kind: Kind,
        target: Vec<u8>,
        parent: usize,
    ) -> usize {
        self.nodes.push(Node {
            name,
            kind,
            mode: m.mode(),
            nlink: m.nlink() as u32,
            uid: m.uid(),
            gid: m.gid(),
            mtime: m.mtime(),
            atime: m.atime(),
            ctime: m.ctime(),
            data: Data::Path,
            udf: UdfNode::default(),
            size: if kind == Kind::File { m.len() } else { 0 },
            src: src.to_path_buf(),
            target,
            parent,
            children: vec![],
            kids: [vec![], vec![]],
            ids: [vec![], vec![]],
            extent: 0,
            dir_extent: [0; 2],
            dir_size: [0; 2],
        });
        self.nodes.len() - 1
    }

    /// Aggiunge sotto `parent` il file/cartella/symlink locale `path`. `depth` = livello del nuovo nodo (radice = 1).
    pub fn add_fs(
        &mut self,
        parent: usize,
        path: &Path,
        name: Vec<u8>,
        depth: usize,
        rr: bool,
    ) -> Result<()> {
        let meta = fs::symlink_metadata(path).map_err(|e| Error::path(path, e))?;
        let ft = meta.file_type();
        let (kind, target) = if ft.is_dir() {
            (Kind::Dir, vec![])
        } else if ft.is_file() {
            (Kind::File, vec![])
        } else if ft.is_symlink() && rr {
            let t = fs::read_link(path).map_err(|e| Error::path(path, e))?;
            (Kind::Symlink, t.as_os_str().as_bytes().to_vec())
        } else {
            tracing::warn!("{}: saltato (tipo non supportato)", path.display());
            return Ok(());
        };
        if kind == Kind::Dir && depth > self.max_depth {
            // ISO9660 vieta oltre 8 livelli; niente relocation Rock Ridge (scelta voluta, vedi ROADMAP)
            return Err(Error::Unsupported(format!(
                "{}: gerarchia oltre 8 livelli (limite di ISO9660; con UDF puro, --no-iso9660, non c'è)",
                path.display()
            )));
        }
        let id = self.push(path, name, &meta, kind, target, parent);
        self.nodes[parent].children.push(id);
        if kind == Kind::Dir {
            self.fill(id, depth, rr)?;
        }
        Ok(())
    }

    fn fill(&mut self, dir: usize, depth: usize, rr: bool) -> Result<()> {
        let path = self.nodes[dir].src.clone();
        let mut ents = fs::read_dir(&path)
            .and_then(|r| r.collect::<std::io::Result<Vec<_>>>())
            .map_err(|e| Error::path(&path, e))?;
        ents.sort_by_key(|e| e.file_name());
        for e in ents {
            self.add_fs(
                dir,
                &e.path(),
                e.file_name().as_bytes().to_vec(),
                depth + 1,
                rr,
            )?;
        }
        Ok(())
    }

    pub fn depth(&self, mut n: usize) -> usize {
        let mut d = 1;
        while n != 0 {
            n = self.nodes[n].parent;
            d += 1;
        }
        d
    }

    /// Cerca un nodo per percorso ("/a/b"); "" e "/" = radice.
    pub fn find(&self, path: &str) -> Option<usize> {
        let mut cur = 0;
        for c in path.split('/').filter(|c| !c.is_empty()) {
            cur = *self.nodes[cur]
                .children
                .iter()
                .find(|&&k| self.nodes[k].name == c.as_bytes())?;
        }
        Some(cur)
    }

    /// Costruisce l'albero da un'immagine esistente (voci in ordine depth-first).
    pub fn from_image(root: &Entry, entries: &[Entry]) -> Tree {
        let mut t = Tree {
            nodes: vec![],
            max_depth: ISO_MAX_DEPTH,
        };
        let mut index = std::collections::HashMap::new();
        t.push_entry(root, Vec::new(), 0);
        index.insert(String::new(), 0usize);
        for e in entries {
            let (parent, name) = e.path.rsplit_once('/').unwrap_or(("", &e.path));
            let Some(&p) = index.get(parent) else {
                tracing::warn!("{}: cartella padre mancante, saltato", e.path);
                continue;
            };
            let id = t.push_entry(e, name.as_bytes().to_vec(), p);
            t.nodes[p].children.push(id);
            index.insert(e.path.clone(), id);
        }
        t
    }

    fn push_entry(&mut self, e: &Entry, name: Vec<u8>, parent: usize) -> usize {
        let ty = match e.kind {
            Kind::Dir => 0o040755,
            Kind::File => 0o100644,
            Kind::Symlink => 0o120777,
        };
        self.nodes.push(Node {
            udf: UdfNode::default(),
            data: Data::Image(
                e.extents
                    .iter()
                    .map(|&(l, n)| (l as u64 * crate::SS as u64, n))
                    .collect(),
            ),
            name,
            kind: e.kind,
            mode: e.mode.unwrap_or(ty),
            nlink: if e.kind == Kind::Dir { 2 } else { 1 },
            uid: e.uid.unwrap_or(0),
            gid: e.gid.unwrap_or(0),
            mtime: e.mtime,
            atime: e.mtime,
            ctime: e.mtime,
            size: if e.kind == Kind::File { e.size } else { 0 },
            src: PathBuf::from(&e.path),
            target: e.target.clone().unwrap_or_default().into_bytes(),
            parent,
            children: vec![],
            kids: [vec![], vec![]],
            ids: [vec![], vec![]],
            extent: 0,
            dir_extent: [0; 2],
            dir_size: [0; 2],
        });
        self.nodes.len() - 1
    }
}
