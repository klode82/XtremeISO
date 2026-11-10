//! Layout e scrittura dell'immagine ISO9660 (+ Joliet, + Rock Ridge).
//!
//! Ordine dei settori: system area (16) | PVD | [SVD Joliet] | terminatore |
//! path table (L,M per vista) | directory (per vista) | area CE Rock Ridge | dati file.

use crate::{
    b16, b32,
    boot::{self, BootEntry, Emulation, Platform, Slot},
    names, rec_time, rr,
    tree::{self, Data, Kind, Tree},
    udf::{self, UdfLayout, UdfParams, UdfRevision},
    Error, Result, SS,
};
use std::{
    collections::HashMap,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

/// Massimo di un singolo extent multiplo di settore (livello 3, file > 4 GiB).
const CHUNK: u64 = 0xFFFF_F800;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    L1 = 1,
    L2 = 2,
    L3 = 3,
}

#[derive(Clone, Debug)]
pub struct IsoOptions {
    pub level: Level,
    pub joliet: bool,
    pub rock_ridge: bool,
    pub volume_id: String,
    pub system_id: String,
    pub volume_set_id: String,
    pub publisher: String,
    pub preparer: String,
    pub application: String,
    /// Voci El Torito; la prima è quella predefinita (BIOS di norma).
    pub boot: Vec<BootEntry>,
    /// Se presente, il primo settore diventa un MBR ibrido (avvio da USB) con questo codice (può essere vuoto).
    pub hybrid_mbr: Option<Vec<u8>>,
    /// Aggiunge UDF (immagine bridge con ISO9660, o UDF pura con `iso9660: false`).
    pub udf: Option<UdfRevision>,
    /// Scrive la struttura ISO9660 (default). Senza, serve `udf`; Joliet, Rock Ridge e boot El Torito richiedono ISO9660.
    pub iso9660: bool,
}

impl Default for IsoOptions {
    fn default() -> Self {
        Self {
            level: Level::L3,
            joliet: false,
            rock_ridge: false,
            volume_id: "CDROM".into(),
            system_id: "LINUX".into(),
            volume_set_id: String::new(),
            publisher: String::new(),
            preparer: "XTREMEISO".into(),
            application: "XTREMEISO".into(),
            boot: vec![],
            hybrid_mbr: None,
            udf: None,
            iso9660: true,
        }
    }
}

/// Un directory record (uno per ogni extent del file, `chunks` > 1 solo per file > 4 GiB).
struct Rec {
    node: usize,
    id: Vec<u8>,
    su: Vec<u8>,   // system use inline (senza CE)
    cont: Vec<u8>, // system use in area di continuazione
    ce: (u32, u32),
    chunks: u64,
    len: usize,
}

/// Una "vista" dell'albero: 0 = ISO9660 (+RR), 1 = Joliet.
struct View {
    order: Vec<usize>, // directory in ordine path table
    num: Vec<u16>,     // nodo -> numero directory (1-based)
    recs: Vec<Vec<Rec>>,
    pt_size: usize,
    pt_l: u32,
    pt_m: u32,
}

fn nchunks(size: u64) -> u64 {
    if size <= u32::MAX as u64 {
        1
    } else {
        size.div_ceil(CHUNK)
    }
}

fn rec_len(idl: usize, su: usize) -> usize {
    let a = 33 + idl;
    let a = a + (a & 1);
    a + su + (su & 1)
}

fn dir_rec(
    extent: u32,
    len: u32,
    flags: u8,
    id: &[u8],
    su: &[u8],
    ce: Option<(u32, u32, u32)>,
    time: [u8; 7],
) -> Vec<u8> {
    let mut su = su.to_vec();
    if let Some((blk, off, l)) = ce {
        su.extend([b'C', b'E', 28, 1]);
        su.extend(b32(blk));
        su.extend(b32(off));
        su.extend(b32(l));
    }
    let total = rec_len(id.len(), su.len());
    let mut v = Vec::with_capacity(total);
    v.extend([total as u8, 0]);
    v.extend(b32(extent));
    v.extend(b32(len));
    v.extend(time);
    v.extend([flags, 0, 0]);
    v.extend(b16(1));
    v.push(id.len() as u8);
    v.extend(id);
    v.resize(total - su.len() - (su.len() & 1), 0);
    v.extend(&su);
    v.resize(total, 0);
    v
}

/// Posizionamento dei record: nessun record attraversa un confine di settore.
fn place(lens: impl Iterator<Item = usize>) -> usize {
    let mut pos = 0;
    for l in lens {
        if pos % SS + l > SS {
            pos = pos.next_multiple_of(SS);
        }
        pos += l;
    }
    pos.next_multiple_of(SS)
}

/// Divide le entry SUSP tra record (`budget` byte) e area di continuazione.
fn split_su(entries: Vec<Vec<u8>>, budget: usize) -> Result<(Vec<u8>, Vec<u8>)> {
    if entries.iter().map(Vec::len).sum::<usize>() <= budget {
        return Ok((entries.concat(), vec![]));
    }
    let (mut inl, mut cont) = (vec![], vec![]);
    for e in entries {
        if cont.is_empty() && inl.len() + e.len() + 28 <= budget {
            inl.extend(e);
        } else {
            cont.extend(e);
        }
    }
    if cont.len() > SS {
        // ponytail: una sola area CE per record; link con destinazione enorme non gestiti (niente CE a catena)
        return Err(Error::Unsupported(
            "nome o destinazione di link troppo lunghi (Rock Ridge)".into(),
        ));
    }
    Ok((inl, cont))
}

fn mk(tree: &Tree, node: usize, id: Vec<u8>, named: bool, sp: bool, rock: bool) -> Result<Rec> {
    let n = &tree.nodes[node];
    let (su, cont) = if rock {
        let base = 33 + id.len();
        split_su(rr::entries(n, named, sp), 254 - (base + (base & 1)))
            .map_err(|e| Error::Unsupported(format!("{}: {e}", n.src.display())))?
    } else {
        (vec![], vec![])
    };
    let chunks = if n.kind == Kind::File {
        nchunks(n.size)
    } else {
        1
    };
    let len = rec_len(id.len(), su.len() + if cont.is_empty() { 0 } else { 28 });
    Ok(Rec {
        node,
        id,
        su,
        cont,
        ce: (0, 0),
        chunks,
        len,
    })
}

fn build_view(tree: &Tree, t: usize, rock: bool) -> Result<View> {
    let rock = rock && t == 0;
    let mut order = vec![0usize];
    let mut i = 0;
    while i < order.len() {
        let d = order[i];
        i += 1;
        order.extend(
            tree.nodes[d].kids[t]
                .iter()
                .copied()
                .filter(|&c| tree.nodes[c].kind == Kind::Dir),
        );
    }
    if order.len() > 0xFFFF {
        return Err(Error::Unsupported("più di 65535 cartelle".into()));
    }
    let mut num = vec![0u16; tree.nodes.len()];
    for (i, &d) in order.iter().enumerate() {
        num[d] = (i + 1) as u16;
    }
    let idlen = |d: usize| {
        if d == 0 {
            1
        } else {
            tree.nodes[d].ids[t].len()
        }
    };
    let pt_size = order.iter().map(|&d| 8 + idlen(d) + (idlen(d) & 1)).sum();
    let mut recs = Vec::with_capacity(order.len());
    for &d in &order {
        let mut v = vec![
            mk(tree, d, vec![0], false, d == 0, rock)?,
            mk(tree, tree.nodes[d].parent, vec![1], false, false, rock)?,
        ];
        for &c in &tree.nodes[d].kids[t] {
            v.push(mk(
                tree,
                c,
                tree.nodes[c].ids[t].clone(),
                true,
                false,
                rock,
            )?);
        }
        recs.push(v);
    }
    Ok(View {
        order,
        num,
        recs,
        pt_size,
        pt_l: 0,
        pt_m: 0,
    })
}

/// Assegna gli extent. Ritorna (settori totali, primo settore area CE, settori area CE).
struct Layout {
    total: u32,
    ce_base: u32,
    ce_secs: u32,
    /// Settore del catalogo di boot (se presente).
    catalog: u32,
    /// Primo settore del VRS UDF.
    vrs: u32,
    /// Primo e ultimo+1 settore dei dati dei file.
    data_start: u64,
    data_end: u64,
    udf: Option<UdfLayout>,
}

fn layout(tree: &mut Tree, views: &mut [View], o: &IsoOptions) -> Result<Layout> {
    let (iso, boot) = (o.iso9660, !o.boot.is_empty());
    let skip = o.udf.map(|_| 256u64);
    // alloca `n` settori contigui evitando il settore 256, riservato all'AVDP UDF
    let alloc = |cur: &mut u64, n: u64| -> u64 {
        if skip.is_some_and(|k| *cur <= k && *cur + n > k) {
            *cur = 257;
        }
        let s = *cur;
        *cur += n;
        s
    };
    let sec = |n: usize| n.div_ceil(SS) as u64;
    let mut cur: u64 = 16;
    if iso {
        cur += 2 + boot as u64 + o.joliet as u64; // PVD, [boot record], [SVD Joliet], terminatore
    }
    let vrs = cur as u32;
    if o.udf.is_some() {
        cur += 3; // BEA01, NSR0x, TEA01
    }
    let catalog = cur as u32;
    cur += boot as u64;
    let (mut ce_base, mut ce_secs) = (0u64, 0u64);
    if iso {
        for v in views.iter_mut() {
            v.pt_l = alloc(&mut cur, sec(v.pt_size)) as u32;
            v.pt_m = alloc(&mut cur, sec(v.pt_size)) as u32;
        }
        for (t, v) in views.iter().enumerate() {
            for (i, &d) in v.order.iter().enumerate() {
                let size = place(
                    v.recs[i]
                        .iter()
                        .flat_map(|r| std::iter::repeat_n(r.len, r.chunks as usize)),
                );
                tree.nodes[d].dir_extent[t] = alloc(&mut cur, (size / SS) as u64) as u32;
                tree.nodes[d].dir_size[t] = size as u32;
            }
        }
        let mut off = 0usize;
        for r in views[0]
            .recs
            .iter_mut()
            .flatten()
            .filter(|r| !r.cont.is_empty())
        {
            let l = r.cont.len();
            if off + l > SS {
                ce_secs += 1;
                off = 0;
            }
            r.ce = (ce_secs as u32, off as u32); // settore relativo, sistemato sotto
            off += l + (l & 1);
        }
        if off > 0 {
            ce_secs += 1;
        }
        ce_base = alloc(&mut cur, ce_secs);
        for r in views[0]
            .recs
            .iter_mut()
            .flatten()
            .filter(|r| !r.cont.is_empty())
        {
            r.ce.0 += ce_base as u32;
        }
    }
    let mut ul = None;
    let mut p_start = 0u64;
    if let Some(rev) = o.udf {
        let mvds = alloc(&mut cur, 16) as u32;
        let rvds = alloc(&mut cur, 16) as u32;
        let lvid = alloc(&mut cur, 2) as u32;
        // partizione subito dopo l'AVDP a 256 (o dopo i metadati se sono più grandi)
        p_start = cur.max(257);
        cur = p_start;
        let (order, meta, nfiles, ndirs) = udf::plan(tree);
        cur += meta as u64;
        ul = Some(UdfLayout {
            rev,
            mvds,
            rvds,
            lvid,
            p_start: p_start as u32,
            p_len: 0,
            avdp2: 0,
            order,
            nfiles,
            ndirs,
            next_uid: 15 + (nfiles + ndirs) as u64,
        });
    }
    let data_start = cur;
    for &d in &views[0].order {
        for k in 0..tree.nodes[d].kids[0].len() {
            let c = tree.nodes[d].kids[0][k];
            let n = &mut tree.nodes[c];
            if n.kind != Kind::Dir {
                n.extent = cur as u32;
                cur += n.size.div_ceil(SS as u64);
            }
        }
    }
    let data_end = cur;
    if let Some(u) = &mut ul {
        u.p_len = u32::try_from(cur - p_start)
            .map_err(|_| Error::Unsupported("partizione UDF oltre 8 TiB".into()))?;
        cur += 1; // AVDP finale
        u.avdp2 = (cur - 1) as u32;
    }
    let total =
        u32::try_from(cur).map_err(|_| Error::Unsupported("immagine oltre 8 TiB".into()))?;
    Ok(Layout {
        total,
        ce_base: ce_base as u32,
        ce_secs: ce_secs as u32,
        catalog,
        vrs,
        data_start,
        data_end,
        udf: ul,
    })
}

/// Legge fino a `limit` byte di un file dell'albero (da disco o dall'immagine sorgente).
fn read_node<R: Read + Seek>(tree: &Tree, idx: usize, img: &mut R, limit: u64) -> Result<Vec<u8>> {
    let n = &tree.nodes[idx];
    if n.size > limit && limit == MAX_BOOT_IMAGE {
        return Err(Error::Unsupported(format!(
            "{}: troppo grande per la boot-info-table",
            n.src.display()
        )));
    }
    let want = n.size.min(limit);
    let mut buf = Vec::with_capacity(want as usize);
    match &n.data {
        Data::Path => {
            File::open(&n.src)
                .and_then(|f| f.take(want).read_to_end(&mut buf))
                .map_err(|e| Error::path(&n.src, e))?;
        }
        Data::Image(ex) => {
            for &(off, len) in ex {
                let left = want - buf.len() as u64;
                if left == 0 {
                    break;
                }
                img.seek(SeekFrom::Start(off))?;
                (&mut *img).take(len.min(left)).read_to_end(&mut buf)?;
            }
        }
    }
    Ok(buf)
}

const MAX_BOOT_IMAGE: u64 = 256 << 20;

/// Voci del catalogo + dati patchati (nodo -> contenuto) da scrivere al posto dell'originale.
type Resolved = (Vec<Slot>, HashMap<usize, Vec<u8>>);

/// Risolve le voci di boot sull'albero già posizionato. Ritorna le voci del catalogo e i dati
/// patchati (boot-info-table) da scrivere al posto del file originale.
fn resolve_boot<R: Read + Seek>(tree: &Tree, o: &IsoOptions, img: &mut R) -> Result<Resolved> {
    let (mut slots, mut patched) = (vec![], HashMap::new());
    for b in &o.boot {
        let idx = tree
            .find(&b.path)
            .filter(|&i| tree.nodes[i].kind == Kind::File)
            .ok_or_else(|| {
                Error::Unsupported(format!(
                    "immagine di boot non trovata nell'albero: {} (aggiornala con --boot)",
                    b.path
                ))
            })?;
        let n = &tree.nodes[idx];
        if n.size == 0 {
            return Err(Error::Unsupported(format!(
                "{}: immagine di boot vuota",
                b.path
            )));
        }
        let (media, sys, default_count) = match b.emulation {
            Emulation::None => (
                0,
                0,
                if b.platform == Platform::Efi {
                    n.size.div_ceil(512).min(0xFFFF) as u16
                } else {
                    4
                },
            ),
            Emulation::Floppy => match n.size {
                1_228_800 => (1, 0, 1),
                1_474_560 => (2, 0, 1),
                2_949_120 => (3, 0, 1),
                s => {
                    return Err(Error::Unsupported(format!(
                        "{}: {s} byte non è un floppy valido (1228800, 1474560 o 2949120)",
                        b.path
                    )))
                }
            },
            Emulation::HardDisk => {
                let head = read_node(tree, idx, img, 512)?;
                (4, head.get(450).copied().unwrap_or(0), 1)
            }
        };
        if b.info_table {
            let mut buf = read_node(tree, idx, img, MAX_BOOT_IMAGE)?;
            boot::patch_info_table(&mut buf, 16, n.extent)?;
            patched.insert(idx, buf);
        }
        slots.push(Slot {
            platform: b.platform.id(),
            media,
            bootable: b.bootable,
            seg: b.load_segment,
            sys,
            count: b.load_size.unwrap_or(default_count),
            rba: n.extent,
            size: n.size,
        });
    }
    Ok((slots, patched))
}

fn dir_bytes(tree: &Tree, t: usize, d: usize, recs: &[Rec]) -> Vec<u8> {
    let mut buf = vec![0u8; tree.nodes[d].dir_size[t] as usize];
    let mut pos = 0;
    for r in recs {
        let m = &tree.nodes[r.node];
        let (extent, size, flags) = match m.kind {
            Kind::Dir => (m.dir_extent[t], m.dir_size[t] as u64, 2u8),
            _ => (m.extent, m.size, 0),
        };
        let ce = (!r.cont.is_empty()).then_some((r.ce.0, r.ce.1, r.cont.len() as u32));
        for i in 0..r.chunks {
            let (ex, len, fl) = if r.chunks == 1 {
                (extent, size, flags)
            } else {
                let off = i * CHUNK;
                (
                    extent + (off / SS as u64) as u32,
                    (size - off).min(CHUNK),
                    if i + 1 < r.chunks { 0x80 } else { 0 },
                )
            };
            let rec = dir_rec(ex, len as u32, fl, &r.id, &r.su, ce, rec_time(m.mtime));
            if pos % SS + rec.len() > SS {
                pos = pos.next_multiple_of(SS);
            }
            buf[pos..pos + rec.len()].copy_from_slice(&rec);
            pos += rec.len();
        }
    }
    buf
}

fn path_table(tree: &Tree, v: &View, t: usize, big: bool) -> Vec<u8> {
    let w32 = |x: u32| {
        if big {
            x.to_be_bytes()
        } else {
            x.to_le_bytes()
        }
    };
    let w16 = |x: u16| {
        if big {
            x.to_be_bytes()
        } else {
            x.to_le_bytes()
        }
    };
    let mut out = vec![];
    for &d in &v.order {
        let n = &tree.nodes[d];
        let id: &[u8] = if d == 0 { &[0] } else { &n.ids[t] };
        out.extend([id.len() as u8, 0]);
        out.extend(w32(n.dir_extent[t]));
        out.extend(w16(if d == 0 { 1 } else { v.num[n.parent] }));
        out.extend(id);
        if id.len() & 1 == 1 {
            out.push(0);
        }
    }
    out
}

fn field(dst: &mut [u8], s: &str, ucs2: bool) {
    if ucs2 {
        let units: Vec<u16> = s.encode_utf16().take(dst.len() / 2).collect();
        for (i, c) in dst.chunks_exact_mut(2).enumerate() {
            c.copy_from_slice(&units.get(i).copied().unwrap_or(0x20).to_be_bytes());
        }
    } else {
        let bytes: Vec<u8> = s
            .chars()
            .map(|c| {
                if c.is_ascii_graphic() || c == ' ' {
                    c.to_ascii_uppercase() as u8
                } else {
                    b'_'
                }
            })
            .collect();
        for (i, b) in dst.iter_mut().enumerate() {
            *b = bytes.get(i).copied().unwrap_or(b' ');
        }
    }
}

fn date17(secs: Option<i64>) -> [u8; 17] {
    let mut d = [b'0'; 17];
    d[16] = 0;
    if let Some(s) = secs {
        let (y, m, dd, h, mi, se) = crate::civil(s);
        d[..16].copy_from_slice(format!("{y:04}{m:02}{dd:02}{h:02}{mi:02}{se:02}00").as_bytes());
    }
    d
}

/// PVD (t = 0) o SVD Joliet (t = 1).
fn descriptor(o: &IsoOptions, tree: &Tree, v: &View, t: usize, total: u32, now: i64) -> Vec<u8> {
    let uc = t == 1;
    let mut b = vec![0u8; SS];
    b[0] = 1 + t as u8;
    b[1..6].copy_from_slice(b"CD001");
    b[6] = 1;
    let vid = if uc {
        o.volume_id.clone()
    } else {
        names::dchars(&o.volume_id)
    };
    field(&mut b[8..40], &o.system_id, uc);
    field(&mut b[40..72], &vid, uc);
    b[80..88].copy_from_slice(&b32(total));
    if uc {
        b[88..91].copy_from_slice(b"%/E");
    }
    b[120..124].copy_from_slice(&b16(1));
    b[124..128].copy_from_slice(&b16(1));
    b[128..132].copy_from_slice(&b16(SS as u16));
    b[132..140].copy_from_slice(&b32(v.pt_size as u32));
    b[140..144].copy_from_slice(&v.pt_l.to_le_bytes());
    b[148..152].copy_from_slice(&v.pt_m.to_be_bytes());
    let root = &tree.nodes[0];
    let rr = dir_rec(
        root.dir_extent[t],
        root.dir_size[t],
        2,
        &[0],
        &[],
        None,
        rec_time(root.mtime),
    );
    b[156..190].copy_from_slice(&rr);
    for (off, len, s) in [
        (190, 128, &o.volume_set_id),
        (318, 128, &o.publisher),
        (446, 128, &o.preparer),
        (574, 128, &o.application),
        (702, 111, &String::new()),
    ] {
        field(&mut b[off..off + len], s, uc);
    }
    b[813..830].copy_from_slice(&date17(Some(now)));
    b[830..847].copy_from_slice(&date17(Some(now)));
    b[847..864].copy_from_slice(&date17(None));
    b[864..881].copy_from_slice(&date17(Some(now)));
    b[881] = 1;
    b
}

struct Sink<W> {
    w: W,
    pos: u64,
}

impl<W: Write> Write for Sink<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.w.write(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

impl<W: Write> Sink<W> {
    /// Riempie di zeri fino all'inizio del settore; errore se si è già oltre (strutture sovrapposte).
    fn pad_to(&mut self, sector: u64) -> Result<()> {
        let target = sector * SS as u64;
        if self.pos > target {
            return Err(Error::Internal(format!(
                "strutture sovrapposte al settore {sector}"
            )));
        }
        let zeros = [0u8; 65536];
        while self.pos < target {
            let n = ((target - self.pos) as usize).min(zeros.len());
            self.write_all(&zeros[..n])?;
        }
        Ok(())
    }

    fn align(&mut self) -> io::Result<()> {
        let r = (self.pos % SS as u64) as usize;
        if r != 0 {
            self.write_all(&vec![0; SS - r])?;
        }
        Ok(())
    }
}

/// Crea un'immagine ISO9660 da una cartella locale.
///
/// Limiti noti: gerarchia max 8 livelli; hard link copiati come file distinti;
/// socket/FIFO/device saltati; Rock Ridge e Joliet opzionali.
pub fn create_iso<W: Write>(src: &Path, out: W, o: &IsoOptions) -> Result<()> {
    let max_depth = if o.iso9660 {
        tree::ISO_MAX_DEPTH
    } else {
        usize::MAX
    };
    let tree = Tree::scan(src, o.rock_ridge || o.udf.is_some(), max_depth)?;
    write_tree(tree, out, o, &mut io::Cursor::new(Vec::new()))
}

/// Scrive un albero; `img` fornisce i byte dei nodi `Data::Image`.
pub(crate) fn write_tree<W: Write, R: Read + Seek>(
    mut tree: Tree,
    out: W,
    o: &IsoOptions,
    img: &mut R,
) -> Result<()> {
    if !o.iso9660 {
        if o.udf.is_none() {
            return Err(Error::Unsupported("servono ISO9660 o UDF".into()));
        }
        if o.joliet || o.rock_ridge || !o.boot.is_empty() {
            return Err(Error::Unsupported(
                "Joliet, Rock Ridge e El Torito richiedono ISO9660".into(),
            ));
        }
    }
    names::assign(&mut tree, o)?;
    let mut views = (0..1 + o.joliet as usize)
        .map(|t| build_view(&tree, t, o.rock_ridge))
        .collect::<Result<Vec<_>>>()?;
    let lay = layout(&mut tree, &mut views, o)?;
    let total = lay.total;
    let (slots, patched) = resolve_boot(&tree, o, img)?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);

    // tutto tranne i dati dei file: (settore, contenuto), poi scritto in ordine di settore
    let mut blobs: Vec<(u64, Vec<u8>)> = vec![];
    let mut sys = match &o.hybrid_mbr {
        Some(code) => {
            let bios = slots.iter().find(|s| s.platform == 0).map(|s| s.rba);
            let efi = slots
                .iter()
                .find(|s| s.platform == 0xEF)
                .map(|s| (s.rba, s.size));
            boot::hybrid_mbr(code, total, bios, efi)?
        }
        None => vec![],
    };
    sys.resize(16 * SS, 0);
    blobs.push((0, sys));
    if o.iso9660 {
        let mut s = 16u64;
        for (t, v) in views.iter().enumerate() {
            blobs.push((s, descriptor(o, &tree, v, t, total, now)));
            s += 1;
            if t == 0 && !slots.is_empty() {
                let mut br = vec![0u8; SS];
                br[1..6].copy_from_slice(b"CD001");
                br[6] = 1;
                br[7..30].copy_from_slice(b"EL TORITO SPECIFICATION");
                br[71..75].copy_from_slice(&lay.catalog.to_le_bytes());
                blobs.push((s, br));
                s += 1;
            }
        }
        let mut term = vec![0u8; SS];
        term[0] = 255;
        term[1..6].copy_from_slice(b"CD001");
        term[6] = 1;
        blobs.push((s, term));
        if !slots.is_empty() {
            blobs.push((lay.catalog as u64, boot::catalog(&slots)?));
        }
        for (t, v) in views.iter().enumerate() {
            for (big, at) in [(false, v.pt_l), (true, v.pt_m)] {
                let mut b = path_table(&tree, v, t, big);
                b.resize(b.len().div_ceil(SS) * SS, 0);
                blobs.push((at as u64, b));
            }
            for (i, &d) in v.order.iter().enumerate() {
                blobs.push((
                    tree.nodes[d].dir_extent[t] as u64,
                    dir_bytes(&tree, t, d, &v.recs[i]),
                ));
            }
        }
        if lay.ce_secs > 0 {
            let mut ce = vec![0u8; lay.ce_secs as usize * SS];
            for r in views[0]
                .recs
                .iter()
                .flatten()
                .filter(|r| !r.cont.is_empty())
            {
                let s = (r.ce.0 - lay.ce_base) as usize * SS + r.ce.1 as usize;
                ce[s..s + r.cont.len()].copy_from_slice(&r.cont);
            }
            blobs.push((lay.ce_base as u64, ce));
        }
    }
    if let Some(ul) = &lay.udf {
        let p = UdfParams {
            volume_id: &o.volume_id,
            publisher: &o.publisher,
            now,
        };
        blobs.extend(udf::blobs(&tree, ul, lay.vrs, &p)?);
    }
    blobs.sort_by_key(|b| b.0);
    let (head, tail): (Vec<_>, Vec<_>) = blobs.into_iter().partition(|b| b.0 < lay.data_start);

    let mut w = Sink { w: out, pos: 0 };
    for (s, b) in &head {
        w.pad_to(*s)?;
        w.write_all(b)?;
    }
    for &d in &views[0].order {
        for &c in &tree.nodes[d].kids[0] {
            let n = &tree.nodes[c];
            if n.kind != Kind::File {
                continue;
            }
            if n.size > 0 {
                w.pad_to(n.extent as u64)?;
            }
            let copied = match &n.data {
                _ if patched.contains_key(&c) => {
                    w.write_all(&patched[&c])?;
                    patched[&c].len() as u64
                }
                Data::Path => {
                    let f = File::open(&n.src).map_err(|e| Error::path(&n.src, e))?;
                    io::copy(&mut f.take(n.size), &mut w).map_err(|e| Error::path(&n.src, e))?
                }
                Data::Image(ex) => {
                    let mut c = 0;
                    for &(off, len) in ex {
                        img.seek(SeekFrom::Start(off))?;
                        c += io::copy(&mut (&mut *img).take(len), &mut w)?;
                    }
                    c
                }
            };
            if copied != n.size {
                return Err(Error::Unsupported(format!(
                    "{}: file modificato durante la lettura",
                    n.src.display()
                )));
            }
            w.align()?;
        }
    }
    w.pad_to(lay.data_end)?;
    for (s, b) in &tail {
        w.pad_to(*s)?;
        w.write_all(b)?;
    }
    w.flush()?;
    if w.pos != total as u64 * SS as u64 {
        return Err(Error::Internal(format!(
            "scritti {} byte, attesi {}",
            w.pos,
            total as u64 * SS as u64
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunking() {
        assert_eq!(nchunks(0), 1);
        assert_eq!(nchunks(u32::MAX as u64), 1);
        assert_eq!(nchunks(u32::MAX as u64 + 1), 2);
        assert_eq!(nchunks(5 << 30), 2);
    }

    #[test]
    fn record_lengths_even() {
        for idl in 1..40 {
            for su in 0..40 {
                assert_eq!(rec_len(idl, su) % 2, 0);
                assert_eq!(
                    dir_rec(0, 0, 0, &vec![b'A'; idl], &vec![0; su], None, [0; 7]).len(),
                    rec_len(idl, su)
                );
            }
        }
    }
}
