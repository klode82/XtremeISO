//! Lettura di immagini ISO9660 (+ Joliet, + Rock Ridge incluse le relocation CL/RE di altri tool).

use crate::{format_unix_time, parse_rec_time, tree::Kind, Error, Result, SS};
use std::{
    collections::HashSet,
    fs::{self, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::{symlink, PermissionsExt},
    path::Path,
    time::{Duration, UNIX_EPOCH},
};

/// Una cartella più grande è quasi certamente un'immagine corrotta.
const MAX_DIR: usize = 64 << 20;
const MAX_DEPTH: usize = 128;

/// Quali nomi usare nell'elenco.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Names {
    /// Rock Ridge se presente, altrimenti Joliet, altrimenti ISO9660.
    Auto,
    Iso,
    Joliet,
    RockRidge,
}

#[derive(Clone, Debug)]
pub struct Info {
    pub system_id: String,
    pub volume_id: String,
    pub volume_set_id: String,
    pub publisher: String,
    pub preparer: String,
    pub application: String,
    pub created: Option<String>,
    pub modified: Option<String>,
    pub blocks: u32,
    pub image_len: u64,
    pub joliet: bool,
    pub rock_ridge: bool,
    /// Presente un catalogo El Torito.
    pub bootable: bool,
}

#[derive(Clone, Debug)]
pub struct Entry {
    /// Percorso assoluto nell'immagine, es. "/dir/file.txt".
    pub path: String,
    pub kind: Kind,
    pub size: u64,
    /// Solo con Rock Ridge.
    pub mode: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub mtime: i64,
    pub target: Option<String>,
    /// (settore di inizio, byte). Per le cartelle: la cartella stessa.
    pub extents: Vec<(u32, u64)>,
}

impl Entry {
    pub fn mode_string(&self) -> String {
        let t = match self.kind {
            Kind::Dir => 'd',
            Kind::Symlink => 'l',
            Kind::File => '-',
        };
        let Some(m) = self.mode else {
            return format!("{t}?????????");
        };
        let bits = "rwxrwxrwx"
            .chars()
            .enumerate()
            .map(|(i, c)| if m & (0o400 >> i) != 0 { c } else { '-' });
        std::iter::once(t).chain(bits).collect()
    }

    pub fn date_string(&self) -> String {
        format_unix_time(self.mtime)
    }
}

fn bad<T>(m: impl Into<String>) -> Result<T> {
    Err(Error::Invalid(m.into()))
}

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}

fn rd<R: Read + Seek>(r: &mut R, total: u64, off: u64, n: usize) -> Result<Vec<u8>> {
    if off.checked_add(n as u64).is_none_or(|e| e > total) {
        return bad(format!(
            "lettura oltre la fine dell'immagine (offset {off}, {n} byte)"
        ));
    }
    r.seek(SeekFrom::Start(off))?;
    let mut v = vec![0; n];
    r.read_exact(&mut v)?;
    Ok(v)
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b)
        .trim_end_matches([' ', '\0'])
        .to_string()
}

fn date17(b: &[u8]) -> Option<String> {
    if !b[..16].iter().all(u8::is_ascii_digit) || b[..16].iter().all(|&c| c == b'0') {
        return None;
    }
    let s = std::str::from_utf8(&b[..14]).ok()?;
    Some(format!(
        "{}-{}-{} {}:{}:{}",
        &s[..4],
        &s[4..6],
        &s[6..8],
        &s[8..10],
        &s[10..12],
        &s[12..14]
    ))
}

struct Rec {
    len: usize,
    extent: u32,
    size: u32,
    flags: u8,
    id: Vec<u8>,
    su: Vec<u8>,
    time: i64,
}

fn parse_rec(b: &[u8]) -> Result<Rec> {
    let len = b[0] as usize;
    if len < 34 || len > b.len() {
        return bad("directory record con lunghezza non valida");
    }
    let idl = b[32] as usize;
    if 33 + idl > len {
        return bad("directory record: identificatore oltre la fine");
    }
    let su_start = (33 + idl + (idl & 1 ^ 1)).min(len);
    Ok(Rec {
        len,
        extent: le32(&b[2..]),
        size: le32(&b[10..]),
        flags: b[25],
        id: b[33..33 + idl].to_vec(),
        su: b[su_start..len].to_vec(),
        time: parse_rec_time(&b[18..25]),
    })
}

#[derive(Default)]
struct RrInfo {
    nm: Option<Vec<u8>>,
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    mtime: Option<i64>,
    sl: Option<Vec<u8>>,
    cl: Option<u32>,
    re: bool,
}

/// Interpreta le entry SUSP/Rock Ridge, seguendo le aree di continuazione (CE).
fn parse_su<R: Read + Seek>(r: &mut R, total: u64, su: &[u8]) -> Result<RrInfo> {
    let mut info = RrInfo::default();
    let (mut area, mut hops) = (su.to_vec(), 0);
    let (mut segs, mut cur, mut abs, mut has_sl) =
        (Vec::<Vec<u8>>::new(), Vec::<u8>::new(), false, false);
    loop {
        let mut ce = None;
        let mut i = 0;
        while i + 4 <= area.len() {
            let len = area[i + 2] as usize;
            if len < 4 || i + len > area.len() {
                break;
            }
            let b = &area[i..i + len];
            match (&b[..2], len) {
                (b"CE", 28..) => ce = Some((le32(&b[4..]), le32(&b[12..]), le32(&b[20..]))),
                (b"NM", 5..) if b[4] & 6 == 0 => {
                    info.nm.get_or_insert_with(Vec::new).extend(&b[5..])
                }
                (b"PX", 36..) => {
                    info.mode = Some(le32(&b[4..]));
                    info.uid = Some(le32(&b[20..]));
                    info.gid = Some(le32(&b[28..]));
                }
                (b"TF", 5..) => {
                    let stride = if b[4] & 0x80 != 0 { 17 } else { 7 };
                    let mut at = 5;
                    for bit in 0..7 {
                        if b[4] & (1 << bit) != 0 {
                            if bit == 1 && stride == 7 && at + 7 <= len {
                                info.mtime = Some(parse_rec_time(&b[at..at + 7]));
                            }
                            at += stride;
                        }
                    }
                }
                (b"SL", 5..) => {
                    has_sl = true;
                    let mut j = 5;
                    while j + 2 <= len && j + 2 + b[j + 1] as usize <= len {
                        let (cf, cl) = (b[j], b[j + 1] as usize);
                        match cf {
                            f if f & 8 != 0 => abs = true,
                            f if f & 2 != 0 => segs.push(b".".to_vec()),
                            f if f & 4 != 0 => segs.push(b"..".to_vec()),
                            _ => {
                                cur.extend(&b[j + 2..j + 2 + cl]);
                                if cf & 1 == 0 {
                                    segs.push(std::mem::take(&mut cur));
                                }
                            }
                        }
                        j += 2 + cl;
                    }
                }
                (b"CL", 12..) => info.cl = Some(le32(&b[4..])),
                (b"RE", _) => info.re = true,
                _ => {}
            }
            i += len;
        }
        match ce {
            Some((blk, off, l)) if hops < 32 => {
                hops += 1;
                area = rd(r, total, blk as u64 * SS as u64 + off as u64, l as usize)?;
            }
            _ => break,
        }
    }
    if !cur.is_empty() {
        segs.push(cur);
    }
    if has_sl {
        let mut t = if abs { b"/".to_vec() } else { vec![] };
        t.extend(segs.join(&b'/'));
        info.sl = Some(t);
    }
    Ok(info)
}

fn clean_iso_name(s: &str) -> String {
    let s = match s.rsplit_once(';') {
        Some((n, v)) if !v.is_empty() && v.bytes().all(|c| c.is_ascii_digit()) => n,
        _ => s,
    };
    s.strip_suffix('.')
        .filter(|n| !n.is_empty())
        .unwrap_or(s)
        .to_string()
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n != "." && n != ".." && !n.contains(['/', '\0'])
}

pub struct Image<R> {
    r: R,
    len: u64,
    info: Info,
    pvd: Vec<u8>,
    root: (u32, u32),
    jol: Option<(u32, u32)>,
    root_entry: Entry,
    catalog: Option<u32>,
}

impl<R: Read + Seek> Image<R> {
    pub fn open(mut r: R) -> Result<Self> {
        let len = r.seek(SeekFrom::End(0))?;
        let (mut pvd, mut jol, mut catalog) = (None, None, None);
        for i in 16..16 + 64u64 {
            let s = rd(&mut r, len, i * SS as u64, SS)
                .or_else(|_| bad("descrittori di volume non trovati"))?;
            if &s[1..6] != b"CD001" {
                return bad("firma CD001 non trovata (non è una ISO9660 a settori da 2048 byte)");
            }
            match s[0] {
                1 if pvd.is_none() => pvd = Some(s),
                2 if matches!(&s[88..91], b"%/@" | b"%/C" | b"%/E") => jol = Some(s),
                0 if &s[7..30] == b"EL TORITO SPECIFICATION" => catalog = Some(le32(&s[71..])),
                255 => break,
                _ => {}
            }
        }
        let Some(pvd) = pvd else {
            return bad("volume descriptor primario mancante");
        };
        let root_rec = parse_rec(&pvd[156..190])?;
        let jol_root = match &jol {
            Some(j) => {
                let r = parse_rec(&j[156..190])?;
                Some((r.extent, r.size))
            }
            None => None,
        };
        // Rock Ridge presente se il "." della radice ha l'entry SP
        let first = rd(
            &mut r,
            len,
            root_rec.extent as u64 * SS as u64,
            SS.min(root_rec.size as usize),
        )?;
        let dot = parse_rec(&first)?;
        let rock_ridge = dot.su.len() >= 7 && &dot.su[..2] == b"SP" && dot.su[4..6] == [0xBE, 0xEF];
        let rri = if rock_ridge {
            parse_su(&mut r, len, &dot.su)?
        } else {
            RrInfo::default()
        };
        let info = Info {
            system_id: text(&pvd[8..40]),
            volume_id: text(&pvd[40..72]),
            volume_set_id: text(&pvd[190..318]),
            publisher: text(&pvd[318..446]),
            preparer: text(&pvd[446..574]),
            application: text(&pvd[574..702]),
            created: date17(&pvd[813..830]),
            modified: date17(&pvd[830..847]),
            blocks: le32(&pvd[80..]),
            image_len: len,
            joliet: jol.is_some(),
            bootable: catalog.is_some(),
            rock_ridge,
        };
        let root_entry = Entry {
            path: "/".into(),
            kind: Kind::Dir,
            size: 0,
            mode: rri.mode,
            uid: rri.uid,
            gid: rri.gid,
            mtime: rri.mtime.unwrap_or(dot.time),
            target: None,
            extents: vec![(root_rec.extent, root_rec.size as u64)],
        };
        Ok(Self {
            r,
            len,
            info,
            pvd,
            root: (root_rec.extent, root_rec.size),
            jol: jol_root,
            root_entry,
            catalog,
        })
    }

    pub fn info(&self) -> &Info {
        &self.info
    }

    pub fn root_entry(&self) -> &Entry {
        &self.root_entry
    }

    /// Settore del catalogo di boot, se presente.
    pub fn catalog_lba(&self) -> Option<u32> {
        self.catalog
    }

    /// Legge byte arbitrari dell'immagine (con controllo dei limiti).
    pub fn read_bytes(&mut self, offset: u64, n: usize) -> Result<Vec<u8>> {
        rd(&mut self.r, self.len, offset, n)
    }

    /// Voci El Torito (vuoto se l'immagine non è avviabile), con il file corrispondente se è nell'albero.
    pub fn boot_entries(&mut self) -> Result<Vec<BootInfo>> {
        let Some(cat) = self.catalog else {
            return Ok(vec![]);
        };
        let avail = self
            .len
            .saturating_sub(cat as u64 * SS as u64)
            .min(SS as u64) as usize;
        let mut v = parse_catalog(&rd(&mut self.r, self.len, cat as u64 * SS as u64, avail)?)?;
        let files = self.entries(Names::Auto)?;
        for b in &mut v {
            if let Some(e) = files
                .iter()
                .find(|e| e.kind == Kind::File && e.extents.first().is_some_and(|x| x.0 == b.rba))
            {
                b.path = Some(e.path.clone());
                b.size = Some(e.size);
            }
            // boot-info-table presente: PVD a 16, LBA del file e (se nota) lunghezza coerenti
            if let Ok(h) = rd(&mut self.r, self.len, b.rba as u64 * SS as u64, 64) {
                b.info_table = le32(&h[8..]) == 16
                    && le32(&h[12..]) == b.rba
                    && b.size.is_none_or(|n| le32(&h[16..]) as u64 == n);
            }
        }
        Ok(v)
    }

    /// Scrive in `out` il contenuto di un'immagine di boot (il file se è nell'albero, altrimenti
    /// `sector_count` settori da 512 byte dal suo indirizzo).
    pub fn read_boot_image(&mut self, b: &BootInfo, out: &mut impl Write) -> Result<()> {
        let n = b.size.unwrap_or(b.sector_count as u64 * 512);
        out.write_all(&rd(
            &mut self.r,
            self.len,
            b.rba as u64 * SS as u64,
            n as usize,
        )?)?;
        Ok(())
    }

    pub fn into_inner(self) -> R {
        self.r
    }

    fn read_extent(&mut self, extent: u32, size: usize) -> Result<Vec<u8>> {
        if size > MAX_DIR {
            return bad("cartella troppo grande");
        }
        rd(&mut self.r, self.len, extent as u64 * SS as u64, size)
    }

    /// Elenco completo (depth-first, la radice esclusa).
    pub fn entries(&mut self, names: Names) -> Result<Vec<Entry>> {
        let (joliet, rr) = match names {
            Names::Auto if self.info.rock_ridge => (false, true),
            Names::Auto if self.jol.is_some() => (true, false),
            Names::Auto | Names::Iso => (false, false),
            Names::Joliet if self.jol.is_some() => (true, false),
            Names::RockRidge if self.info.rock_ridge => (false, true),
            Names::Joliet => return bad("l'immagine non ha Joliet"),
            Names::RockRidge => return bad("l'immagine non ha Rock Ridge"),
        };
        let root = if joliet { self.jol.unwrap() } else { self.root };
        let (mut out, mut seen) = (vec![], HashSet::from([root.0]));
        self.walk(joliet, rr, root, "", &mut seen, &mut out, 0)?;
        Ok(out)
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(
        &mut self,
        joliet: bool,
        rr: bool,
        dir: (u32, u32),
        prefix: &str,
        seen: &mut HashSet<u32>,
        out: &mut Vec<Entry>,
        depth: usize,
    ) -> Result<()> {
        if depth > MAX_DEPTH {
            return bad("gerarchia troppo profonda");
        }
        let data = self.read_extent(dir.0, dir.1 as usize)?;
        let (mut pos, mut multi) = (0, None::<Entry>);
        while pos < data.len() {
            if data[pos] == 0 {
                pos = (pos / SS + 1) * SS;
                continue;
            }
            let rec = parse_rec(&data[pos..])?;
            pos += rec.len;
            if rec.id == [0] || rec.id == [1] {
                continue;
            }
            let rri = if rr {
                parse_su(&mut self.r, self.len, &rec.su)?
            } else {
                RrInfo::default()
            };
            if rri.re {
                continue; // già mostrata al posto originale tramite CL
            }
            let name = match (&rri.nm, joliet) {
                (Some(nm), _) => String::from_utf8_lossy(nm).into_owned(),
                (None, true) => clean_iso_name(
                    &char::decode_utf16(
                        rec.id
                            .chunks_exact(2)
                            .map(|c| u16::from_be_bytes([c[0], c[1]])),
                    )
                    .map(|c| c.unwrap_or('\u{FFFD}'))
                    .collect::<String>(),
                ),
                (None, false) => clean_iso_name(&String::from_utf8_lossy(&rec.id)),
            };
            if !valid_name(&name) {
                tracing::warn!("nome non valido in {prefix}/: saltato");
                continue;
            }
            if rri
                .mode
                .is_some_and(|m| matches!(m & 0o170000, 0o020000 | 0o060000 | 0o010000 | 0o140000))
            {
                tracing::warn!("{prefix}/{name}: file speciale, saltato");
                continue;
            }
            let mut e = Entry {
                path: format!("{prefix}/{name}"),
                kind: Kind::File,
                size: rec.size as u64,
                mode: rri.mode,
                uid: rri.uid,
                gid: rri.gid,
                mtime: rri.mtime.unwrap_or(rec.time),
                target: None,
                extents: vec![(rec.extent, rec.size as u64)],
            };
            if let Some(cl) = rri.cl {
                let first = self.read_extent(
                    cl,
                    SS.min(self.len.saturating_sub(cl as u64 * SS as u64) as usize),
                )?;
                e.kind = Kind::Dir;
                e.extents = vec![(cl, parse_rec(&first)?.size as u64)];
            } else if rec.flags & 2 != 0 {
                e.kind = Kind::Dir;
            } else if let Some(t) = rri.sl {
                e.kind = Kind::Symlink;
                e.size = 0;
                e.extents = vec![];
                e.target = Some(String::from_utf8_lossy(&t).into_owned());
            }
            if let Some(m) = &mut multi {
                if m.path == e.path && e.kind == Kind::File {
                    m.extents.extend(e.extents);
                    m.size += e.size;
                    if rec.flags & 0x80 == 0 {
                        out.push(multi.take().unwrap());
                    }
                    continue;
                }
                out.push(multi.take().unwrap());
            }
            match e.kind {
                Kind::Dir => {
                    let ext = e.extents[0];
                    e.size = 0;
                    let path = e.path.clone();
                    out.push(e);
                    if seen.insert(ext.0) {
                        self.walk(
                            joliet,
                            rr,
                            (ext.0, ext.1 as u32),
                            &path,
                            seen,
                            out,
                            depth + 1,
                        )?;
                    } else {
                        tracing::warn!("{path}: cartella già visitata (ciclo?), non esplorata");
                    }
                }
                Kind::File if rec.flags & 0x80 != 0 => multi = Some(e),
                _ => out.push(e),
            }
        }
        out.extend(multi);
        Ok(())
    }

    /// Scrive il contenuto di un file in `out`.
    pub fn read_entry(&mut self, e: &Entry, out: &mut impl Write) -> Result<()> {
        if e.kind != Kind::File {
            return Ok(());
        }
        for &(lba, n) in &e.extents {
            let off = lba as u64 * SS as u64;
            if off.checked_add(n).is_none_or(|end| end > self.len) {
                return bad(format!("{}: dati oltre la fine dell'immagine", e.path));
            }
            self.r.seek(SeekFrom::Start(off))?;
            if io::copy(&mut (&mut self.r).take(n), out)? != n {
                return bad(format!("{}: lettura incompleta", e.path));
            }
        }
        Ok(())
    }

    /// Estrae tutto (o solo i percorsi in `only`) sotto `dest`. Ritorna il numero di voci create.
    pub fn extract(&mut self, names: Names, dest: &Path, only: &[String]) -> Result<usize> {
        let norm: Vec<String> = only
            .iter()
            .map(|p| format!("/{}", p.trim_matches('/')))
            .collect();
        let sel = |e: &Entry| {
            norm.is_empty()
                || norm
                    .iter()
                    .any(|p| p == "/" || e.path == *p || e.path.starts_with(&format!("{p}/")))
        };
        let entries: Vec<Entry> = self.entries(names)?.into_iter().filter(sel).collect();
        fs::create_dir_all(dest)?;
        let (mut n, mut dir_modes) = (0, vec![]);
        for e in &entries {
            let target = dest.join(e.path.trim_start_matches('/'));
            safe_mkdirs(dest, target.parent().unwrap_or(dest))?;
            match e.kind {
                Kind::Dir => {
                    safe_mkdirs(dest, &target)?;
                    if let Some(m) = e.mode {
                        dir_modes.push((target, m));
                    }
                }
                Kind::File => {
                    let mut f = OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(&target)
                        .map_err(|x| Error::path(&target, x))?;
                    self.read_entry(e, &mut f)?;
                    if e.mtime >= 0 {
                        f.set_modified(UNIX_EPOCH + Duration::from_secs(e.mtime as u64))?;
                    }
                    drop(f);
                    if let Some(m) = e.mode {
                        // niente setuid/setgid/sticky dall'immagine
                        fs::set_permissions(&target, fs::Permissions::from_mode(m & 0o777))?;
                    }
                }
                Kind::Symlink => symlink(e.target.as_deref().unwrap_or(""), &target)
                    .map_err(|x| Error::path(&target, x))?,
            }
            n += 1;
        }
        for (p, m) in dir_modes.into_iter().rev() {
            fs::set_permissions(&p, fs::Permissions::from_mode(m & 0o777))?;
        }
        Ok(n)
    }

    /// Controlli di coerenza; ritorna l'elenco dei problemi (vuoto = ok).
    pub fn verify(&mut self) -> Result<Vec<String>> {
        let mut p = vec![];
        let blocks = self.info.blocks as u64;
        if self.len < blocks * SS as u64 {
            p.push(format!(
                "immagine troncata: {} byte, il volume ne dichiara {}",
                self.len,
                blocks * SS as u64
            ));
        }
        for (name, off, n) in [
            ("dimensione volume", 80, 4),
            ("dimensione path table", 132, 4),
        ] {
            if self.pvd[off..off + n]
                != self.pvd[off + 4..off + 4 + n]
                    .iter()
                    .rev()
                    .copied()
                    .collect::<Vec<_>>()
            {
                p.push(format!(
                    "PVD: {name} con parte little/big endian incoerente"
                ));
            }
        }
        let entries = match self.entries(Names::Iso) {
            Ok(e) => e,
            Err(e) => {
                p.push(format!("albero ISO9660: {e}"));
                return Ok(p);
            }
        };
        let mut dirs = HashSet::from([self.root.0]);
        for e in &entries {
            for &(lba, n) in &e.extents {
                if lba as u64 + n.div_ceil(SS as u64) > blocks {
                    p.push(format!("{}: extent oltre la fine del volume", e.path));
                }
            }
            if e.kind == Kind::Dir {
                dirs.insert(e.extents[0].0);
                if e.extents[0].1 % SS as u64 != 0 {
                    p.push(format!(
                        "{}: dimensione cartella non multipla del settore",
                        e.path
                    ));
                }
            }
        }
        let size = le32(&self.pvd[132..]) as usize;
        let tables = [
            le32(&self.pvd[140..]),
            u32::from_be_bytes(self.pvd[148..152].try_into().unwrap()),
        ];
        let mut parsed = vec![];
        for (big, &lba) in tables.iter().enumerate() {
            match rd(&mut self.r, self.len, lba as u64 * SS as u64, size)
                .and_then(|b| parse_pt(&b, big == 1))
            {
                Ok(t) => parsed.push(t),
                Err(e) => p.push(format!("path table {}: {e}", ["L", "M"][big])),
            }
        }
        if parsed.len() == 2 && parsed[0] != parsed[1] {
            p.push("path table L e M diverse".into());
        }
        if let Some(t) = parsed.first() {
            let ext: HashSet<u32> = t.iter().map(|x| x.0).collect();
            if ext != dirs {
                p.push(format!(
                    "path table: {} cartelle, l'albero ne ha {}",
                    ext.len(),
                    dirs.len()
                ));
            }
            if t.iter()
                .enumerate()
                .any(|(i, x)| x.1 == 0 || x.1 as usize > i.max(1))
            {
                p.push("path table: numero della cartella padre non valido".into());
            }
        }
        if self.jol.is_some() {
            // Joliet non ha symlink: si confrontano solo file e cartelle (la vista ISO pura li vede come file vuoti)
            let primary = if self.info.rock_ridge {
                self.entries(Names::RockRidge)?
            } else {
                entries
            };
            let n = primary.iter().filter(|e| e.kind != Kind::Symlink).count();
            match self.entries(Names::Joliet) {
                Ok(j) if j.len() != n => {
                    p.push(format!("Joliet: {} voci, ISO9660 ne ha {n}", j.len()))
                }
                Ok(_) => {}
                Err(e) => p.push(format!("albero Joliet: {e}")),
            }
        }
        Ok(p)
    }
}

/// Una voce del catalogo El Torito.
#[derive(Clone, Debug)]
pub struct BootInfo {
    pub platform: u8,
    pub bootable: bool,
    pub media: u8,
    pub load_segment: u16,
    pub system_type: u8,
    pub sector_count: u16,
    pub rba: u32,
    /// Solo se l'immagine è un file visibile nell'albero.
    pub path: Option<String>,
    pub size: Option<u64>,
    pub info_table: bool,
}

impl BootInfo {
    pub fn platform_name(&self) -> &'static str {
        match self.platform {
            0 => "BIOS",
            1 => "PPC",
            2 => "Mac",
            0xEF => "UEFI",
            _ => "?",
        }
    }

    pub fn media_name(&self) -> &'static str {
        match self.media & 0xF {
            0 => "no-emulation",
            1 => "floppy 1.2M",
            2 => "floppy 1.44M",
            3 => "floppy 2.88M",
            4 => "hard disk",
            _ => "?",
        }
    }
}

fn parse_catalog(c: &[u8]) -> Result<Vec<BootInfo>> {
    if c.len() < 64 || c[0] != 1 || c[30] != 0x55 || c[31] != 0xAA {
        return bad("catalogo di boot: validation entry non valida");
    }
    if c[..32].chunks(2).fold(0u16, |a, w| {
        a.wrapping_add(u16::from_le_bytes([w[0], w[1]]))
    }) != 0
    {
        return bad("catalogo di boot: checksum errato");
    }
    let (mut platform, mut out) = (c[1], vec![]);
    for e in c[32..].chunks_exact(32) {
        match e[0] {
            0x90 | 0x91 => platform = e[1],
            0x88 | 0x00 => {
                let (count, rba) = (u16::from_le_bytes([e[6], e[7]]), le32(&e[8..]));
                if e[0] == 0 && rba == 0 && count == 0 {
                    break; // fine del catalogo
                }
                out.push(BootInfo {
                    platform,
                    bootable: e[0] == 0x88,
                    media: e[1],
                    load_segment: u16::from_le_bytes([e[2], e[3]]),
                    system_type: e[4],
                    sector_count: count,
                    rba,
                    path: None,
                    size: None,
                    info_table: false,
                });
            }
            _ => {} // estensioni (0x44) e voci sconosciute
        }
    }
    Ok(out)
}

type PtEntry = (u32, u16, Vec<u8>);

fn parse_pt(b: &[u8], big: bool) -> Result<Vec<PtEntry>> {
    let (mut i, mut v) = (0, vec![]);
    while i < b.len() {
        let l = b[i] as usize;
        if l == 0 || i + 8 + l > b.len() {
            return bad("voce non valida");
        }
        let (ext, par) = (&b[i + 2..i + 6], &b[i + 6..i + 8]);
        let (e, p) = if big {
            (
                u32::from_be_bytes(ext.try_into().unwrap()),
                u16::from_be_bytes(par.try_into().unwrap()),
            )
        } else {
            (
                u32::from_le_bytes(ext.try_into().unwrap()),
                u16::from_le_bytes(par.try_into().unwrap()),
            )
        };
        v.push((e, p, b[i + 8..i + 8 + l].to_vec()));
        i += 8 + l + (l & 1);
    }
    Ok(v)
}

/// Crea le cartelle di `path` sotto `base`, rifiutando symlink e file lungo il percorso
/// (un'immagine ostile non deve poter scrivere fuori da `base` passando da un link).
fn safe_mkdirs(base: &Path, path: &Path) -> Result<()> {
    let rel = path
        .strip_prefix(base)
        .map_err(|_| Error::Internal("percorso fuori dalla destinazione".into()))?;
    let mut cur = base.to_path_buf();
    for c in rel.components() {
        cur.push(c);
        match fs::symlink_metadata(&cur) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                return Err(Error::Unsupported(format!(
                    "{}: esiste e non è una cartella",
                    cur.display()
                )))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                fs::create_dir(&cur).map_err(|e| Error::path(&cur, e))?
            }
            Err(e) => return Err(Error::path(&cur, e)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(clean_iso_name("HELLO.TXT;1"), "HELLO.TXT");
        assert_eq!(clean_iso_name("README.;1"), "README");
        assert_eq!(clean_iso_name("DIR"), "DIR");
        assert!(!valid_name("..") && !valid_name("a/b") && !valid_name("") && valid_name("ok.txt"));
    }
}
