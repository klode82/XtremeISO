//! Lettura, verifica, estrazione e modifica di immagini.

mod common;

use common::*;
use std::{
    collections::BTreeMap, fs, io::Cursor, os::unix::fs::PermissionsExt, path::Path,
    process::Command,
};
use xtremeiso_core::{Editor, Image, IsoOptions, Kind, Level, Names};

/// path -> (tipo, dimensione, destinazione symlink) di una cartella locale.
fn snapshot(root: &Path) -> BTreeMap<String, (Kind, u64, Option<String>)> {
    fn rec(root: &Path, dir: &Path, m: &mut BTreeMap<String, (Kind, u64, Option<String>)>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            let md = fs::symlink_metadata(&p).unwrap();
            let rel = format!("/{}", p.strip_prefix(root).unwrap().to_string_lossy());
            if md.is_dir() {
                m.insert(rel, (Kind::Dir, 0, None));
                rec(root, &p, m);
            } else if md.is_symlink() {
                m.insert(
                    rel,
                    (
                        Kind::Symlink,
                        0,
                        Some(fs::read_link(&p).unwrap().to_string_lossy().into_owned()),
                    ),
                );
            } else {
                m.insert(rel, (Kind::File, md.len(), None));
            }
        }
    }
    let mut m = BTreeMap::new();
    rec(root, root, &mut m);
    m
}

fn listing<R: std::io::Read + std::io::Seek>(
    img: &mut Image<R>,
    n: Names,
) -> BTreeMap<String, (Kind, u64, Option<String>)> {
    img.entries(n)
        .unwrap()
        .into_iter()
        .map(|e| (e.path, (e.kind, e.size, e.target)))
        .collect()
}

fn open(iso: &Path) -> Image<Cursor<Vec<u8>>> {
    Image::open(Cursor::new(fs::read(iso).unwrap())).unwrap()
}

#[test]
fn reads_back_our_rock_ridge_image() {
    let root = tmp("rd-rr");
    let src = fixture(&root);
    let iso = root.join("t.iso");
    build(
        &src,
        &iso,
        &IsoOptions {
            rock_ridge: true,
            joliet: true,
            volume_id: "TESTVOL".into(),
            ..Default::default()
        },
    );
    let mut img = open(&iso);
    let i = img.info().clone();
    assert!(i.rock_ridge && i.joliet);
    assert_eq!(i.volume_id, "TESTVOL");
    assert_eq!(listing(&mut img, Names::Auto), snapshot(&src));
    assert_eq!(img.verify().unwrap(), Vec::<String>::new());
    let out = root.join("out");
    img.extract(Names::Auto, &out, &[]).unwrap();
    run(Command::new("diff")
        .args(["-r", "--no-dereference"])
        .arg(&src)
        .arg(&out));
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&out.join("hello.txt")), mode(&src.join("hello.txt")));
    // estrazione parziale
    let part = root.join("part");
    assert_eq!(
        img.extract(Names::Auto, &part, &["/sub/deeper".into()])
            .unwrap(),
        2
    );
    assert_eq!(
        fs::read_to_string(part.join("sub/deeper/file.with.dots.tar.gz")).unwrap(),
        "y"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reads_joliet_and_plain_views() {
    let root = tmp("rd-views");
    let src = fixture(&root);
    let iso = root.join("t.iso");
    // senza Rock Ridge i symlink non entrano; Joliet tronca i nomi a 64 caratteri
    let expect: BTreeMap<_, _> = snapshot(&src)
        .into_iter()
        .filter(|(_, v)| v.0 != Kind::Symlink)
        .map(|(k, v)| (k.chars().take(65).collect::<String>(), v))
        .collect();
    build(
        &src,
        &iso,
        &IsoOptions {
            joliet: true,
            ..Default::default()
        },
    );
    let mut img = open(&iso);
    assert!(!img.info().rock_ridge);
    assert_eq!(listing(&mut img, Names::Auto), expect); // Joliet: nomi originali
    assert!(img.entries(Names::RockRidge).is_err());
    assert_eq!(img.verify().unwrap(), Vec::<String>::new());

    build(
        &src,
        &iso,
        &IsoOptions {
            level: Level::L1,
            ..Default::default()
        },
    );
    let mut img = open(&iso);
    let names: Vec<String> = img
        .entries(Names::Auto)
        .unwrap()
        .into_iter()
        .map(|e| e.path)
        .collect();
    assert!(
        names.contains(&"/HELLO.TXT".to_string()) && names.contains(&"/EMPTY".to_string()),
        "{names:?}"
    );
    assert!(img.entries(Names::Joliet).is_err());
    // contenuto leggibile anche senza estensioni
    let e = img
        .entries(Names::Auto)
        .unwrap()
        .into_iter()
        .find(|e| e.path == "/BIG.BIN")
        .unwrap();
    let mut buf = vec![];
    img.read_entry(&e, &mut buf).unwrap();
    assert_eq!(buf, fs::read(src.join("big.bin")).unwrap());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reads_xorriso_image_with_deep_tree_relocation() {
    if !have("xorriso") {
        return;
    }
    let root = tmp("rd-deep");
    let mut d = root.join("src");
    for c in "abcdefghijk".chars() {
        d.push(c.to_string());
    }
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("leaf.txt"), "deep").unwrap();
    fs::write(root.join("src/top.txt"), "top").unwrap();
    let iso = root.join("x.iso");
    run(Command::new("xorriso")
        .args(["-as", "mkisofs", "-R", "-J", "-o"])
        .arg(&iso)
        .arg(root.join("src")));
    let mut img = open(&iso);
    assert_eq!(listing(&mut img, Names::Auto), snapshot(&root.join("src")));
    assert_eq!(img.verify().unwrap(), Vec::<String>::new());
    let out = root.join("out");
    img.extract(Names::Auto, &out, &[]).unwrap();
    run(Command::new("diff")
        .args(["-r", "--no-dereference"])
        .arg(root.join("src"))
        .arg(&out));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn verify_reports_truncated_and_rejects_garbage() {
    let root = tmp("rd-bad");
    let src = fixture(&root);
    let iso = root.join("t.iso");
    build(
        &src,
        &iso,
        &IsoOptions {
            rock_ridge: true,
            ..Default::default()
        },
    );
    let bytes = fs::read(&iso).unwrap();
    let mut img = Image::open(Cursor::new(bytes[..bytes.len() - 5 * 2048].to_vec())).unwrap();
    assert!(!img.verify().unwrap().is_empty());
    assert!(Image::open(Cursor::new(vec![0u8; 100_000])).is_err());
    assert!(Image::open(Cursor::new(vec![0u8; 10])).is_err());
    // path table corrotta: la L diventa diversa dalla M
    let mut bad = bytes.clone();
    let l = u32::from_le_bytes(bad[16 * 2048 + 140..16 * 2048 + 144].try_into().unwrap()) as usize;
    bad[l * 2048 + 2] ^= 0xFF;
    let problems = Image::open(Cursor::new(bad)).unwrap().verify().unwrap();
    assert!(
        problems.iter().any(|p| p.contains("path table")),
        "{problems:?}"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn extract_never_writes_through_symlinks() {
    let root = tmp("rd-escape");
    let src = root.join("src");
    fs::create_dir_all(src.join("a")).unwrap();
    fs::write(src.join("a/x"), "pwn").unwrap();
    let iso = root.join("t.iso");
    build(
        &src,
        &iso,
        &IsoOptions {
            rock_ridge: true,
            ..Default::default()
        },
    );
    let (dest, outside) = (root.join("dest"), root.join("outside"));
    fs::create_dir_all(&dest).unwrap();
    fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, dest.join("a")).unwrap();
    assert!(open(&iso).extract(Names::Auto, &dest, &[]).is_err());
    assert!(fs::read_dir(&outside).unwrap().next().is_none());
    // non sovrascrive file esistenti
    fs::remove_file(dest.join("a")).unwrap();
    fs::create_dir_all(dest.join("a")).unwrap();
    fs::write(dest.join("a/x"), "mine").unwrap();
    assert!(open(&iso).extract(Names::Auto, &dest, &[]).is_err());
    assert_eq!(fs::read_to_string(dest.join("a/x")).unwrap(), "mine");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn edit_remove_rename_add() {
    let root = tmp("edit");
    let src = fixture(&root);
    let iso = root.join("t.iso");
    build(
        &src,
        &iso,
        &IsoOptions {
            rock_ridge: true,
            joliet: true,
            volume_id: "KEEPME".into(),
            ..Default::default()
        },
    );

    let extra = root.join("extra");
    fs::create_dir_all(extra.join("inner")).unwrap();
    fs::write(extra.join("inner/new.txt"), "nuovo").unwrap();
    fs::write(root.join("single.txt"), "uno").unwrap();

    let mut ed = Editor::open(Cursor::new(fs::read(&iso).unwrap()), Names::Auto).unwrap();
    ed.remove("/sub/deeper").unwrap();
    ed.rename("/hello.txt", "hi.txt").unwrap();
    ed.add(&extra, "/added/here").unwrap(); // crea anche /added/here
    ed.add(&root.join("single.txt"), "/").unwrap();
    assert!(ed.rename("/hi.txt", "empty").is_err(), "collisione di nome");
    assert!(ed.rename("/hi.txt", "a/b").is_err());
    assert!(ed.remove("/nope").is_err());
    assert!(
        ed.add(&root.join("single.txt"), "/").is_err(),
        "già presente"
    );
    let out = root.join("edited.iso");
    ed.write(std::io::BufWriter::new(fs::File::create(&out).unwrap()))
        .unwrap();

    // atteso: copia dell'originale con le stesse modifiche applicate a mano
    let exp = root.join("expected");
    run(Command::new("cp").arg("-a").arg(&src).arg(&exp));
    fs::remove_dir_all(exp.join("sub/deeper")).unwrap();
    fs::rename(exp.join("hello.txt"), exp.join("hi.txt")).unwrap();
    fs::create_dir_all(exp.join("added/here")).unwrap();
    run(Command::new("cp")
        .arg("-a")
        .arg(&extra)
        .arg(exp.join("added/here/extra")));
    fs::write(exp.join("single.txt"), "uno").unwrap();

    let mut img = open(&out);
    assert_eq!(img.info().volume_id, "KEEPME");
    assert!(img.info().rock_ridge && img.info().joliet);
    assert_eq!(img.verify().unwrap(), Vec::<String>::new());
    assert_eq!(listing(&mut img, Names::Auto), snapshot(&exp));
    let ext = root.join("ext");
    img.extract(Names::Auto, &ext, &[]).unwrap();
    run(Command::new("diff")
        .args(["-r", "--no-dereference"])
        .arg(&exp)
        .arg(&ext));
    if have("xorriso") {
        let ext2 = root.join("ext2");
        run(Command::new("xorriso")
            .args(["-osirrox", "on", "-indev"])
            .arg(&out)
            .args(["-extract", "/"])
            .arg(&ext2));
        run(Command::new("diff")
            .args(["-r", "--no-dereference"])
            .arg(&exp)
            .arg(&ext2));
    }
    let _ = fs::remove_dir_all(&root);
}
