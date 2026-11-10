//! Generazione degli identificatori ISO9660 e Joliet (unici per cartella).

use crate::{
    tree::{Kind, Tree},
    writer::{IsoOptions, Level},
    Error, Result,
};
use std::collections::HashSet;

/// Solo d-characters: A-Z 0-9 _
pub(crate) fn dchars(s: &str) -> String {
    s.chars()
        .map(|c| match c.to_ascii_uppercase() {
            c if c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' => c,
            _ => '_',
        })
        .collect()
}

pub fn iso_name(name: &str, dir: bool, level: Level, taken: &mut HashSet<String>) -> String {
    let (base, ext) = match name.rfind('.') {
        Some(i) if i > 0 && !dir => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    };
    let (mut base, mut ext) = (dchars(base), dchars(ext));
    let bmax = match (level, dir) {
        (Level::L1, true) => 8,
        (Level::L1, false) => {
            ext.truncate(3);
            8
        }
        (_, true) => 31,
        (_, false) => {
            ext.truncate(29);
            30 - ext.len()
        }
    };
    if base.is_empty() {
        base.push('_');
    }
    for n in 0u32.. {
        let suffix = if n == 0 { String::new() } else { n.to_string() };
        let keep = bmax.saturating_sub(suffix.len()).min(base.len());
        let mut id = format!("{}{}", &base[..keep], suffix);
        if !dir {
            id = format!("{id}.{ext};1");
        }
        if taken.insert(id.clone()) {
            return id;
        }
    }
    unreachable!()
}

/// Nome Joliet in UCS-2 big endian, max 64 unità, caratteri vietati -> '_'.
pub fn joliet_name(name: &str, taken: &mut HashSet<String>) -> Vec<u8> {
    let clean: String = name
        .chars()
        .map(|c| {
            if c < ' ' || "*/:;?\\".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let units = |s: &str| s.encode_utf16().count();
    let (base, mut ext) = match clean.rfind('.') {
        Some(i) if i > 0 => (&clean[..i], &clean[i..]),
        _ => (&clean[..], ""),
    };
    if units(ext) > 32 {
        ext = "";
    }
    for n in 0u32.. {
        let suffix = if n == 0 { String::new() } else { n.to_string() };
        let limit = 64 - units(ext) - suffix.len();
        let mut b = String::new();
        for c in base.chars() {
            if units(&b) + c.len_utf16() > limit {
                break;
            }
            b.push(c);
        }
        let id = format!("{b}{suffix}{ext}");
        if taken.insert(id.to_lowercase()) {
            return id.encode_utf16().flat_map(u16::to_be_bytes).collect();
        }
    }
    unreachable!()
}

/// Assegna `ids` e `kids` (ordinati per identificatore) a ogni nodo.
pub fn assign(tree: &mut Tree, o: &IsoOptions) -> Result<()> {
    let mut stack = vec![0usize];
    while let Some(d) = stack.pop() {
        let children = tree.nodes[d].children.clone();
        let (mut iso_taken, mut jol_taken, mut udf_taken) =
            (HashSet::new(), HashSet::new(), HashSet::new());
        for &c in &children {
            let n = &mut tree.nodes[c];
            if o.iso9660 && n.kind == Kind::File && n.size > u32::MAX as u64 && o.level != Level::L3
            {
                return Err(Error::Unsupported(format!(
                    "{}: file oltre 4 GiB richiede il livello 3",
                    n.src.display()
                )));
            }
            let name = String::from_utf8_lossy(&n.name).into_owned();
            if o.iso9660 {
                n.ids[0] =
                    iso_name(&name, n.kind == Kind::Dir, o.level, &mut iso_taken).into_bytes();
            }
            if o.udf.is_some() {
                n.udf.name = crate::udf::udf_name(&name, &mut udf_taken);
            }
            if o.joliet && n.kind != Kind::Symlink {
                n.ids[1] = joliet_name(&name, &mut jol_taken);
            }
        }
        for t in 0..1 + o.joliet as usize {
            let mut k: Vec<usize> = children
                .iter()
                .copied()
                // i symlink stanno solo nella vista ISO9660 con Rock Ridge (Joliet non li ha, UDF usa la propria lista)
                .filter(|&c| tree.nodes[c].kind != Kind::Symlink || (t == 0 && o.rock_ridge))
                .collect();
            k.sort_by(|&a, &b| tree.nodes[a].ids[t].cmp(&tree.nodes[b].ids[t]));
            tree.nodes[d].kids[t] = k;
        }
        if o.udf.is_some() {
            let mut k = children.clone();
            k.sort_by(|&a, &b| tree.nodes[a].udf.name.cmp(&tree.nodes[b].udf.name));
            tree.nodes[d].udf.kids = k;
        }
        stack.extend(
            children
                .iter()
                .copied()
                .filter(|&c| tree.nodes[c].kind == Kind::Dir),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_levels() {
        let mut t = HashSet::new();
        assert_eq!(
            iso_name("hello world.txt", false, Level::L1, &mut t),
            "HELLO_WO.TXT;1"
        );
        assert_eq!(
            iso_name("hello world.text", false, Level::L1, &mut t),
            "HELLO_WO.TEX;1"
        );
        assert_eq!(iso_name("README", false, Level::L3, &mut t), "README.;1");
        assert_eq!(iso_name("my.dir", true, Level::L1, &mut t), "MY_DIR");
        let long = format!("{}.txt", "a".repeat(40));
        assert_eq!(
            iso_name(&long, false, Level::L2, &mut t),
            format!("{}.TXT;1", "A".repeat(27))
        );
    }

    #[test]
    fn iso_collisions() {
        let mut t = HashSet::new();
        let a = iso_name("longname1.txt", false, Level::L1, &mut t);
        let b = iso_name("longname2.txt", false, Level::L1, &mut t);
        assert_eq!(
            (a.as_str(), b.as_str()),
            ("LONGNAME.TXT;1", "LONGNAM1.TXT;1")
        );
    }

    #[test]
    fn joliet() {
        let mut t = HashSet::new();
        let n = joliet_name("a:b?.txt", &mut t);
        assert_eq!(
            n,
            "a_b_.txt"
                .encode_utf16()
                .flat_map(u16::to_be_bytes)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            joliet_name(&format!("{}.rs", "x".repeat(100)), &mut t).len(),
            128
        );
        assert_ne!(joliet_name("A_B_.TXT", &mut t), n); // case-insensitive collision
    }
}
