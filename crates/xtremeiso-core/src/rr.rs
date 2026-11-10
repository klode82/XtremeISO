//! Entry SUSP / Rock Ridge (RRIP 1.10): SP, PX, TF, NM, SL.

use crate::{
    b32, rec_time,
    tree::{Kind, Node},
};

fn entry(sig: &[u8; 2], flags: u8, body: &[u8]) -> Vec<u8> {
    let mut e = vec![sig[0], sig[1], (5 + body.len()) as u8, 1, flags];
    e.extend(body);
    e
}

/// `named`: aggiunge NM (record di file/cartella, non "." e ".."); `sp`: aggiunge SP (solo "." della radice).
pub fn entries(n: &Node, named: bool, sp: bool) -> Vec<Vec<u8>> {
    let mut v = vec![];
    if sp {
        v.push(vec![b'S', b'P', 7, 1, 0xBE, 0xEF, 0]);
        v.push(er()); // senza ER libisofs/xorriso ignorano il Rock Ridge
    }
    let mut px = vec![b'P', b'X', 36, 1];
    for x in [n.mode, n.nlink, n.uid, n.gid] {
        px.extend(b32(x));
    }
    v.push(px);
    // TF: modifica, accesso, cambio attributi
    v.push(entry(
        b"TF",
        0x0E,
        &[n.mtime, n.atime, n.ctime].map(rec_time).concat(),
    ));
    if named {
        let mut chunks = n.name.chunks(200).peekable();
        while let Some(c) = chunks.next() {
            v.push(entry(b"NM", chunks.peek().is_some() as u8, c));
        }
    }
    if n.kind == Kind::Symlink {
        v.extend(sl(&n.target));
    }
    v
}

fn er() -> Vec<u8> {
    const ID: &[u8] = b"RRIP_1991A";
    const DES: &[u8] =
        b"THE ROCK RIDGE INTERCHANGE PROTOCOL PROVIDES SUPPORT FOR POSIX FILE SYSTEM SEMANTICS";
    const SRC: &[u8] = b"PLEASE CONTACT DISC PUBLISHER FOR SPECIFICATION SOURCE.  SEE PUBLISHER IDENTIFIER IN PRIMARY VOLUME DESCRIPTOR FOR CONTACT INFORMATION.";
    let mut e = vec![b'E', b'R', (8 + ID.len() + DES.len() + SRC.len()) as u8, 1];
    e.extend([ID.len() as u8, DES.len() as u8, SRC.len() as u8, 1]);
    [ID, DES, SRC].iter().for_each(|x| e.extend(*x));
    e
}

fn sl(target: &[u8]) -> Vec<Vec<u8>> {
    let mut comps: Vec<Vec<u8>> = vec![];
    if target.first() == Some(&b'/') {
        comps.push(vec![8, 0]);
    }
    for part in target.split(|&b| b == b'/').filter(|p| !p.is_empty()) {
        match part {
            b"." => comps.push(vec![2, 0]),
            b".." => comps.push(vec![4, 0]),
            _ => {
                let mut chunks = part.chunks(200).peekable();
                while let Some(c) = chunks.next() {
                    let mut x = vec![chunks.peek().is_some() as u8, c.len() as u8];
                    x.extend(c);
                    comps.push(x);
                }
            }
        }
    }
    let mut out = vec![];
    let mut cur: Vec<u8> = vec![];
    for c in comps {
        if cur.len() + c.len() > 240 {
            out.push(entry(b"SL", 1, &cur));
            cur.clear();
        }
        cur.extend(c);
    }
    out.push(entry(b"SL", 0, &cur));
    out
}
