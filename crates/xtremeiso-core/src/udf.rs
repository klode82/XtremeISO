//! UDF (ECMA-167 / OSTA UDF 1.02 .. 2.60): descrittori di volume, partizione, File Entry, directory.
//!
//! Schema (come genisoimage): VRS dopo i descrittori ISO9660 | VDS principale (16) | VDS di riserva (16) |
//! LVID (2) | AVDP a 256 | partizione da 257: FSD, TD, File Entry e directory, poi i dati dei file
//! (gli stessi extent di ISO9660) | AVDP finale. Partizione di tipo 1, niente metadata partition.

use crate::{
    civil,
    tree::{Kind, Tree},
    Error, Result, SS,
};
use std::collections::HashSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UdfRevision {
    V102,
    V150,
    V200,
    V201,
    V250,
    V260,
}

impl UdfRevision {
    pub fn code(self) -> u16 {
        match self {
            UdfRevision::V102 => 0x0102,
            UdfRevision::V150 => 0x0150,
            UdfRevision::V200 => 0x0200,
            UdfRevision::V201 => 0x0201,
            UdfRevision::V250 => 0x0250,
            UdfRevision::V260 => 0x0260,
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "1.02" => UdfRevision::V102,
            "1.50" => UdfRevision::V150,
            "2.00" => UdfRevision::V200,
            "2.01" => UdfRevision::V201,
            "2.50" => UdfRevision::V250,
            "2.60" => UdfRevision::V260,
            _ => return None,
        })
    }

    /// NSR02 per UDF <= 1.50, NSR03 da 2.00.
    fn nsr3(self) -> bool {
        self.code() >= 0x0200
    }

    fn dver(self) -> u16 {
        if self.nsr3() {
            3
        } else {
            2
        }
    }
}

#[derive(Default, Clone)]
pub struct UdfNode {
    /// Blocco logico (relativo alla partizione) del File Entry.
    pub fe: u32,
    /// Blocco logico dei dati della cartella (FID).
    pub data: u32,
    pub name: Vec<u8>,
    pub uid: u64,
    /// Figli ordinati per nome.
    pub kids: Vec<usize>,
}

pub(crate) struct UdfLayout {
    pub rev: UdfRevision,
    pub mvds: u32,
    pub rvds: u32,
    pub lvid: u32,
    pub p_start: u32,
    pub p_len: u32,
    pub avdp2: u32,
    pub order: Vec<usize>,
    pub nfiles: u32,
    pub ndirs: u32,
    pub next_uid: u64,
}

// ---------- primitive ----------

pub(crate) fn crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &b in data {
        crc ^= (b as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn w16(b: &mut [u8], o: usize, v: u16) {
    b[o..o + 2].copy_from_slice(&v.to_le_bytes());
}
fn w32(b: &mut [u8], o: usize, v: u32) {
    b[o..o + 4].copy_from_slice(&v.to_le_bytes());
}
fn w64(b: &mut [u8], o: usize, v: u64) {
    b[o..o + 8].copy_from_slice(&v.to_le_bytes());
}
fn put(b: &mut [u8], o: usize, v: &[u8]) {
    b[o..o + v.len()].copy_from_slice(v);
}

/// Descrittore con tag: `len` = byte significativi (tag incluso), il resto del blocco è zero.
fn desc(id: u16, rev: UdfRevision, loc: u32, len: usize, f: impl FnOnce(&mut [u8])) -> Vec<u8> {
    let mut b = vec![0u8; len.div_ceil(SS).max(1) * SS];
    f(&mut b);
    finish(&mut b, id, rev.dver(), loc, len);
    b
}

fn finish(b: &mut [u8], id: u16, ver: u16, loc: u32, len: usize) {
    w16(b, 0, id);
    w16(b, 2, ver);
    w16(b, 10, (len - 16) as u16);
    let crc = crc16(&b[16..len]);
    w16(b, 8, crc);
    w32(b, 12, loc);
    b[4] = b[..16]
        .iter()
        .enumerate()
        .filter(|&(i, _)| i != 4)
        .fold(0u8, |a, (_, &x)| a.wrapping_add(x));
}

fn regid(flags: u8, id: &str, suffix: [u8; 8]) -> [u8; 32] {
    let mut r = [0u8; 32];
    r[0] = flags;
    r[1..1 + id.len()].copy_from_slice(id.as_bytes());
    r[24..].copy_from_slice(&suffix);
    r
}

fn udf_suffix(rev: UdfRevision) -> [u8; 8] {
    let c = rev.code();
    [c as u8, (c >> 8) as u8, 4, 0, 0, 0, 0, 0] // classe OS 4 = UNIX
}

fn domain_id(rev: UdfRevision) -> [u8; 32] {
    let c = rev.code();
    // flag di dominio 3: immagine protetta in scrittura (hard + soft)
    regid(
        0,
        "*OSTA UDF Compliant",
        [c as u8, (c >> 8) as u8, 3, 0, 0, 0, 0, 0],
    )
}

fn impl_id() -> [u8; 32] {
    regid(0, "*XtremeISO", [4, 0, 0, 0, 0, 0, 0, 0])
}

fn charspec() -> [u8; 64] {
    let mut c = [0u8; 64];
    c[1..1 + 23].copy_from_slice(b"OSTA Compressed Unicode");
    c
}

fn timestamp(secs: i64) -> [u8; 12] {
    let (y, mo, d, h, mi, s) = civil(secs);
    let mut t = [0u8; 12];
    t[..2].copy_from_slice(&0x1000u16.to_le_bytes()); // ora locale, fuso 0 = UTC
    t[2..4].copy_from_slice(&(y.clamp(1, 9999) as u16).to_le_bytes());
    t[4..9].copy_from_slice(&[mo as u8, d as u8, h as u8, mi as u8, s as u8]);
    t
}

/// OSTA compressed unicode: id 8 (latin1) o 16 (UTF-16 BE).
pub(crate) fn cs0(s: &str) -> Vec<u8> {
    if s.chars().all(|c| c as u32 <= 0xFF) {
        std::iter::once(8)
            .chain(s.chars().map(|c| c as u8))
            .collect()
    } else {
        std::iter::once(16)
            .chain(s.encode_utf16().flat_map(u16::to_be_bytes))
            .collect()
    }
}

/// dstring di `n` byte: contenuto compresso, zeri, ultimo byte = lunghezza usata.
fn dstring(s: &str, n: usize) -> Vec<u8> {
    let mut c = cs0(s);
    if c.len() > n - 1 {
        // taglia sul confine di carattere
        c = if c[0] == 8 {
            c[..n - 1].to_vec()
        } else {
            c[..(n - 2) & !1 | 1].to_vec()
        };
    }
    let len = c.len();
    c.resize(n, 0);
    c[n - 1] = len as u8;
    c
}

/// Nome UDF unico nella cartella (confronto senza maiuscole, per Windows), max 254 caratteri o 127 unità UTF-16.
pub fn udf_name(name: &str, taken: &mut HashSet<String>) -> Vec<u8> {
    let clean: String = name
        .chars()
        .map(|c| if c == '\0' { '_' } else { c })
        .collect();
    let wide = clean.chars().any(|c| c as u32 > 0xFF);
    let (limit, units) = if wide { (127, true) } else { (254, false) };
    let count = |s: &str| {
        if units {
            s.encode_utf16().count()
        } else {
            s.chars().count()
        }
    };
    let (base, ext) = match clean.rfind('.') {
        Some(i) if i > 0 && count(&clean[i..]) <= 32 => (&clean[..i], &clean[i..]),
        _ => (&clean[..], ""),
    };
    for n in 0u32.. {
        let suffix = if n == 0 { String::new() } else { n.to_string() };
        let room = limit - count(ext) - suffix.len();
        let mut b = String::new();
        for c in base.chars() {
            if count(&b) + if units { c.len_utf16() } else { 1 } > room {
                break;
            }
            b.push(c);
        }
        let id = format!("{b}{suffix}{ext}");
        if taken.insert(id.to_lowercase()) {
            return cs0(&id);
        }
    }
    unreachable!()
}

fn perms(mode: u32) -> u32 {
    let class = |m: u32| {
        (if m & 4 != 0 { 0x04 } else { 0 })
            | (if m & 2 != 0 { 0x02 | 0x08 | 0x10 } else { 0 })
            | (m & 1)
    };
    class(mode) | class(mode >> 3) << 5 | class(mode >> 6) << 10
}

fn long_ad(len: u32, lb: u32, uid: u32) -> [u8; 16] {
    let mut a = [0u8; 16];
    a[..4].copy_from_slice(&len.to_le_bytes());
    a[4..8].copy_from_slice(&lb.to_le_bytes());
    a[12..16].copy_from_slice(&uid.to_le_bytes()); // impl use: byte 10..12 flags, 12..16 unique id (basso)
    a
}

// ---------- pianificazione della partizione ----------

/// Assegna blocchi logici (relativi alla partizione) a File Entry e dati delle cartelle.
/// Ritorna (ordine DFS, blocchi di metadati, numero file, numero cartelle).
pub(crate) fn plan(tree: &mut Tree) -> (Vec<usize>, u32, u32, u32) {
    let mut order = vec![];
    let mut stack = vec![0usize];
    while let Some(n) = stack.pop() {
        order.push(n);
        if tree.nodes[n].kind == Kind::Dir {
            stack.extend(tree.nodes[n].udf.kids.iter().rev().copied());
        }
    }
    let (mut lb, mut files, mut dirs) = (2u32, 0u32, 0u32); // 0 = FSD, 1 = TD
    for (i, &n) in order.iter().enumerate() {
        tree.nodes[n].udf.uid = if n == 0 { 0 } else { 15 + i as u64 };
        tree.nodes[n].udf.fe = lb;
        lb += 1;
        if tree.nodes[n].kind == Kind::Dir {
            dirs += 1;
            tree.nodes[n].udf.data = lb;
            lb += fid_size(tree, n).div_ceil(SS) as u32;
        } else {
            files += 1;
        }
    }
    (order, lb, files, dirs)
}

fn fid_len(l_fi: usize) -> usize {
    (38 + l_fi + 3) & !3
}

fn fid_size(tree: &Tree, d: usize) -> usize {
    fid_len(0)
        + tree.nodes[d]
            .udf
            .kids
            .iter()
            .map(|&k| fid_len(tree.nodes[k].udf.name.len()))
            .sum::<usize>()
}

// ---------- descrittori ----------

fn fid(tree: &Tree, target: usize, name: &[u8], chars: u8, loc: u32, ver: u16) -> Vec<u8> {
    let t = &tree.nodes[target];
    let len = fid_len(name.len());
    let mut b = vec![0u8; len];
    w16(&mut b, 16, 1); // versione
    b[18] = chars;
    b[19] = name.len() as u8;
    put(&mut b, 20, &long_ad(SS as u32, t.udf.fe, t.udf.uid as u32));
    put(&mut b, 38, name);
    finish(&mut b, 257, ver, loc, len);
    b
}

/// Contenuto dei blocchi dati di una cartella: FID del padre, poi uno per figlio.
fn dir_data(tree: &Tree, d: usize, ver: u16) -> Vec<u8> {
    let n = &tree.nodes[d];
    let parent = if d == 0 { 0 } else { n.parent };
    let mut out = vec![];
    let mut emit = |target: usize, name: &[u8], chars: u8| {
        let loc = n.udf.data + (out.len() / SS) as u32;
        out.extend(fid(tree, target, name, chars, loc, ver));
    };
    emit(parent, &[], 0x0A);
    for &k in &n.udf.kids {
        let c = &tree.nodes[k];
        emit(k, &c.udf.name, if c.kind == Kind::Dir { 0x02 } else { 0 });
    }
    out.resize(out.len().div_ceil(SS) * SS, 0);
    out
}

/// Path component di un symlink (OSTA 2.3.10): la destinazione intera, da incorporare nel File Entry.
fn symlink_data(target: &[u8]) -> Vec<u8> {
    let mut out = vec![];
    let comp = |out: &mut Vec<u8>, ty: u8, id: &[u8]| {
        out.extend([ty, id.len() as u8, 0, 0]);
        out.extend(id);
    };
    if target.first() == Some(&b'/') {
        comp(&mut out, 2, &[]);
    }
    for part in target.split(|&b| b == b'/').filter(|p| !p.is_empty()) {
        match part {
            b"." => comp(&mut out, 4, &[]),
            b".." => comp(&mut out, 3, &[]),
            p => comp(&mut out, 5, &cs0(&String::from_utf8_lossy(p))),
        }
    }
    out
}

fn file_entry(tree: &Tree, n: usize, ul: &UdfLayout) -> Result<Vec<u8>> {
    let node = &tree.nodes[n];
    let (ftype, info_len, blocks, ads, embedded) = match node.kind {
        Kind::Dir => {
            let size = fid_size(tree, n);
            let mut ad = vec![];
            ad.extend((size as u32).to_le_bytes());
            ad.extend(node.udf.data.to_le_bytes());
            (4u8, size as u64, size.div_ceil(SS) as u64, ad, false)
        }
        Kind::Symlink => {
            let d = symlink_data(&node.target);
            if d.len() > SS - 176 {
                return Err(Error::Unsupported(format!(
                    "{}: destinazione del link troppo lunga per UDF",
                    node.src.display()
                )));
            }
            (12, d.len() as u64, 0, d, true)
        }
        Kind::File => {
            let mut ad = vec![];
            let (mut left, mut lb) = (node.size, node.extent.wrapping_sub(ul.p_start));
            while left > 0 {
                let l = left.min(0x3FFF_F800);
                ad.extend((l as u32).to_le_bytes());
                ad.extend(lb.to_le_bytes());
                lb += (l / SS as u64) as u32;
                left -= l;
            }
            if ad.len() > SS - 176 {
                // ponytail: un solo File Entry per file, niente AD di continuazione (limite ~234 GiB per file)
                return Err(Error::Unsupported(format!(
                    "{}: file troppo grande per UDF",
                    node.src.display()
                )));
            }
            (5, node.size, node.size.div_ceil(SS as u64), ad, false)
        }
    };
    let nsub = node
        .udf
        .kids
        .iter()
        .filter(|&&k| tree.nodes[k].kind == Kind::Dir)
        .count() as u16;
    let len = 176 + ads.len();
    Ok(desc(261, ul.rev, node.udf.fe, len, |b| {
        w32(b, 16, 0);
        w16(b, 20, 4); // strategia ICB 4
        w16(b, 24, 1);
        b[27] = ftype;
        // flags: tipo di AD (0 short, 3 incorporato) + non rilocabile + archivio + contiguo
        w16(b, 34, if embedded { 3 } else { 0 } | 0x230);
        w32(b, 36, u32::MAX);
        w32(b, 40, u32::MAX);
        w32(b, 44, perms(node.mode & 0o777));
        w16(b, 48, if node.kind == Kind::Dir { 1 + nsub } else { 1 });
        w64(b, 56, info_len);
        w64(b, 64, blocks);
        put(b, 72, &timestamp(node.atime));
        put(b, 84, &timestamp(node.mtime));
        put(b, 96, &timestamp(node.ctime));
        w32(b, 108, 1);
        put(b, 128, &impl_id());
        w64(b, 160, node.udf.uid);
        w32(b, 172, ads.len() as u32);
        put(b, 176, &ads);
    }))
}

// ---------- tutti i blocchi UDF ----------

pub(crate) struct UdfParams<'a> {
    pub volume_id: &'a str,
    pub publisher: &'a str,
    pub now: i64,
}

/// Tutti i blocchi UDF come (settore fisico, contenuto): VRS, VDS, LVID, AVDP e la partizione.
pub(crate) fn blobs(
    tree: &Tree,
    ul: &UdfLayout,
    vrs: u32,
    p: &UdfParams,
) -> Result<Vec<(u64, Vec<u8>)>> {
    let rev = ul.rev;
    let mut out: Vec<(u64, Vec<u8>)> = vec![];
    let vrs_sec = |id: &[u8; 5]| {
        let mut b = vec![0u8; SS];
        b[1..6].copy_from_slice(id);
        b[6] = 1;
        b
    };
    out.push((vrs as u64, vrs_sec(b"BEA01")));
    out.push((
        vrs as u64 + 1,
        vrs_sec(if rev.nsr3() { b"NSR03" } else { b"NSR02" }),
    ));
    out.push((vrs as u64 + 2, vrs_sec(b"TEA01")));

    let vol = if p.volume_id.is_empty() {
        "UDF"
    } else {
        p.volume_id
    };
    let set_id = format!("{:016X}{}", fnv(&format!("{}{}", p.now, vol)), p.publisher);
    let now = timestamp(p.now);
    let nsr = if rev.nsr3() { "+NSR03" } else { "+NSR02" };

    // Volume Descriptor Sequence (due copie identiche a parte il tag)
    for base in [ul.mvds, ul.rvds] {
        let mut s = base;
        let mut push = |d: Vec<u8>| {
            out.push((s as u64, d));
            s += 1;
        };
        push(desc(1, rev, base, 512, |b| {
            w32(b, 16, 1);
            w32(b, 20, 0);
            put(b, 24, &dstring(vol, 32));
            w16(b, 56, 1);
            w16(b, 58, 1);
            w16(b, 60, 2);
            w16(b, 62, 3);
            w32(b, 64, 1);
            w32(b, 68, 1);
            put(b, 72, &dstring(&set_id, 128));
            put(b, 200, &charspec());
            put(b, 264, &charspec());
            put(b, 344, &regid(0, "*XtremeISO", [0; 8]));
            put(b, 376, &now);
            put(b, 388, &impl_id());
            w16(b, 488, 0);
        }));
        push(desc(4, rev, base + 1, 512, |b| {
            w32(b, 16, 2);
            put(b, 20, &regid(0, "*UDF LV Info", udf_suffix(rev)));
            put(b, 52, &charspec());
            put(b, 116, &dstring(vol, 128));
            put(b, 352, &impl_id());
        }));
        push(desc(5, rev, base + 2, 512, |b| {
            w32(b, 16, 3);
            w16(b, 20, 1); // partizione allocata
            w16(b, 22, 0);
            put(b, 24, &regid(2, nsr, [0; 8]));
            w32(b, 184, 1); // sola lettura
            w32(b, 188, ul.p_start);
            w32(b, 192, ul.p_len);
            put(b, 196, &impl_id());
        }));
        push(desc(6, rev, base + 3, 512 + 6, |b| {
            w32(b, 16, 4);
            put(b, 20, &charspec());
            put(b, 84, &dstring(vol, 128));
            w32(b, 212, SS as u32);
            put(b, 216, &domain_id(rev));
            put(b, 248, &long_ad(2 * SS as u32, 0, 0)); // FSD + TD nel blocco 0 della partizione
            w32(b, 264, 6);
            w32(b, 268, 1);
            put(b, 272, &impl_id());
            w32(b, 432, 2 * SS as u32);
            w32(b, 436, ul.lvid);
            put(b, 440, &[1, 6, 1, 0, 0, 0]); // mappa partizione tipo 1
        }));
        push(desc(7, rev, base + 4, 24, |b| w32(b, 16, 5)));
        push(desc(8, rev, base + 5, 512, |_| {}));
    }

    // Logical Volume Integrity + terminatore
    out.push((
        ul.lvid as u64,
        desc(9, rev, ul.lvid, 134, |b| {
            put(b, 16, &now);
            w32(b, 28, 1); // chiuso
            w64(b, 40, ul.next_uid);
            w32(b, 72, 1);
            w32(b, 76, 46);
            w32(b, 80, u32::MAX); // spazio libero sconosciuto
            w32(b, 84, ul.p_len);
            put(b, 88, &impl_id());
            w32(b, 120, ul.nfiles);
            w32(b, 124, ul.ndirs);
            let c = rev.code();
            w16(b, 128, c);
            w16(b, 130, c);
            w16(b, 132, c);
        }),
    ));
    out.push((ul.lvid as u64 + 1, desc(8, rev, ul.lvid + 1, 512, |_| {})));

    // AVDP a 256 e a fine volume
    for loc in [256, ul.avdp2] {
        out.push((
            loc as u64,
            desc(2, rev, loc, 512, |b| {
                w32(b, 16, 16 * SS as u32);
                w32(b, 20, ul.mvds);
                w32(b, 24, 16 * SS as u32);
                w32(b, 28, ul.rvds);
            }),
        ));
    }

    // partizione: FSD + TD, poi File Entry e dati delle cartelle
    let ps = ul.p_start as u64;
    let vol_fs = dstring(vol, 32);
    out.push((
        ps,
        desc(256, rev, 0, 512, |b| {
            put(b, 16, &now);
            w16(b, 28, 3);
            w16(b, 30, 3);
            w32(b, 32, 1);
            w32(b, 36, 1);
            put(b, 48, &charspec());
            put(b, 112, &dstring(vol, 128));
            put(b, 240, &charspec());
            put(b, 304, &vol_fs);
            put(b, 400, &long_ad(SS as u32, tree.nodes[0].udf.fe, 0));
            put(b, 416, &domain_id(rev));
        }),
    ));
    out.push((ps + 1, desc(8, rev, 1, 512, |_| {})));
    for &n in &ul.order {
        let node = &tree.nodes[n];
        out.push((ps + node.udf.fe as u64, file_entry(tree, n, ul)?));
        if node.kind == Kind::Dir {
            out.push((ps + node.udf.data as u64, dir_data(tree, n, rev.dver())));
        }
    }
    Ok(out)
}

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100000001b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_known_vectors() {
        assert_eq!(crc16(b"123456789"), 0x31C3); // CRC-16/XMODEM
        assert_eq!(crc16(&[0x70, 0x6A, 0x77]), 0x3299); // esempio di ECMA-167
    }

    #[test]
    fn tag_checksum_and_crc() {
        let d = desc(8, UdfRevision::V102, 7, 512, |_| {});
        let sum = d[..16]
            .iter()
            .enumerate()
            .filter(|&(i, _)| i != 4)
            .fold(0u8, |a, (_, &x)| a.wrapping_add(x));
        assert_eq!(d[4], sum);
        assert_eq!(u16::from_le_bytes([d[8], d[9]]), crc16(&d[16..512]));
        assert_eq!(
            (
                u16::from_le_bytes([d[0], d[1]]),
                u32::from_le_bytes(d[12..16].try_into().unwrap())
            ),
            (8, 7)
        );
    }

    #[test]
    fn strings() {
        assert_eq!(cs0("ab"), [8, b'a', b'b']);
        assert_eq!(cs0("è€"), [16, 0, 0xE8, 0x20, 0xAC]);
        let d = dstring("abc", 32);
        assert_eq!((d[0], d[3], d[31]), (8, b'c', 4));
        assert_eq!(dstring(&"x".repeat(100), 32)[31], 31);
    }

    #[test]
    fn names_unique_and_truncated() {
        let mut t = HashSet::new();
        assert_eq!(udf_name("A.txt", &mut t), cs0("A.txt"));
        assert_eq!(udf_name("a.TXT", &mut t), cs0("a1.TXT"));
        let long = udf_name(&format!("{}.rs", "n".repeat(300)), &mut t);
        assert_eq!(long.len(), 255); // 8 + 254 caratteri
        assert!(long.ends_with(b".rs"));
    }

    #[test]
    fn permission_bits() {
        // proprietario rw- = lettura 0x04 + scrittura 0x02|0x08|0x10; gruppo e altri r--
        assert_eq!(perms(0o644), 0x04 | (0x04 << 5) | (0x1E << 10));
        assert_eq!(perms(0o755) & 0x1F, 0x05);
    }
}
