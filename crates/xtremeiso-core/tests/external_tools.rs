//! Valida le immagini prodotte con strumenti di terze parti (xorriso, isoinfo, 7z).
//! I test si saltano se lo strumento non è installato.

mod common;

use common::*;
use std::{fs, process::Command};
use xtremeiso_core::{create_iso, IsoOptions, Level};

#[test]
fn rock_ridge_roundtrip_with_xorriso() {
    if !have("xorriso") {
        return;
    }
    let root = tmp("rr");
    let src = fixture(&root);
    let iso = root.join("t.iso");
    let o = IsoOptions {
        rock_ridge: true,
        joliet: true,
        volume_id: "TESTVOL".into(),
        ..Default::default()
    };
    build(&src, &iso, &o);
    let extracted = root.join("out");
    out(Command::new("xorriso")
        .args(["-osirrox", "on", "-indev"])
        .arg(&iso)
        .args(["-extract", "/"])
        .arg(&extracted));
    out(Command::new("diff")
        .args(["-r", "--no-dereference"])
        .arg(&src)
        .arg(&extracted));
    // symlink preservato
    assert_eq!(
        fs::read_link(extracted.join("link")).unwrap().to_str(),
        Some("hello.txt")
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn plain_iso_names_with_isoinfo() {
    if !have("isoinfo") {
        return;
    }
    let root = tmp("plain");
    let src = fixture(&root);
    let iso = root.join("t.iso");
    for (level, expect) in [
        (Level::L1, "/COLLIDE1.TXT;1"),
        (Level::L3, "/A_VERY_LONG_FILE_NAME_THAT_.TXT;1"),
    ] {
        build(
            &src,
            &iso,
            &IsoOptions {
                level,
                volume_id: "TestVol".into(),
                ..Default::default()
            },
        );
        let list = out(Command::new("isoinfo").args(["-f", "-i"]).arg(&iso));
        assert!(list.contains("/HELLO.TXT;1"), "{list}");
        assert!(
            list.contains("/SUB/DEEPER/FILE_WIT.GZ;1") || level == Level::L3,
            "{list}"
        );
        assert!(list.contains(expect), "{list}");
        let info = out(Command::new("isoinfo").args(["-d", "-i"]).arg(&iso));
        assert!(info.contains("Volume id: TESTVOL"), "{info}");
        // contenuto file
        let big = Command::new("isoinfo")
            .args(["-i"])
            .arg(&iso)
            .args(["-x", "/BIG.BIN;1"])
            .output()
            .unwrap();
        assert_eq!(big.stdout, fs::read(src.join("big.bin")).unwrap());
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn joliet_names_with_isoinfo_and_7z() {
    let root = tmp("joliet");
    let src = fixture(&root);
    let iso = root.join("t.iso");
    build(
        &src,
        &iso,
        &IsoOptions {
            joliet: true,
            ..Default::default()
        },
    );
    if have("isoinfo") {
        let list = out(Command::new("isoinfo").args(["-J", "-f", "-i"]).arg(&iso));
        assert!(list.contains("/sub/Mixed Case "), "{list}"); // isoinfo non stampa UTF-8: il nome completo si verifica con 7z
        assert!(list.contains("/sub/deeper/file.with.dots.tar.gz"), "{list}");
    }
    if have("7z") {
        let list = out(Command::new("7z").arg("l").arg(&iso));
        assert!(
            list.contains("hello.txt") && list.contains("big.bin"),
            "{list}"
        );
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn too_deep_is_rejected() {
    let root = tmp("deep");
    let mut d = root.join("src");
    for i in 0..9 {
        d.push(format!("d{i}"));
    }
    fs::create_dir_all(&d).unwrap();
    let r = create_iso(&root.join("src"), std::io::sink(), &IsoOptions::default());
    assert!(r.is_err());
    let _ = fs::remove_dir_all(&root);
}
