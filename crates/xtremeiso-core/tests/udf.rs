//! Scrittura UDF: validata con udfinfo, 7z, xorriso e soprattutto montando l'immagine col kernel Linux.

mod common;

use common::*;
use std::{fs, os::unix::fs::symlink, path::Path, process::Command};
use xtremeiso_core::{create_iso, IsoOptions, UdfRevision};

const REVS: [(&str, UdfRevision); 6] = [
    ("1.02", UdfRevision::V102),
    ("1.50", UdfRevision::V150),
    ("2.00", UdfRevision::V200),
    ("2.01", UdfRevision::V201),
    ("2.50", UdfRevision::V250),
    ("2.60", UdfRevision::V260),
];

/// Albero ricco: profondo (12 livelli), Unicode, vuoti, symlink, permessi, nome lungo.
fn rich(root: &Path) -> std::path::PathBuf {
    let d = root.join("rich");
    fs::create_dir_all(d.join("a/b/c/d/e/f/g/h/i/j/k/l")).unwrap();
    fs::create_dir_all(d.join("empty_dir")).unwrap();
    fs::create_dir_all(d.join("dir con spazi/è-ü-€")).unwrap();
    fs::write(d.join("a/b/c/d/e/f/g/h/i/j/k/l/leaf.txt"), "deep").unwrap();
    fs::write(d.join("empty_file"), "").unwrap();
    fs::write(d.join("dir con spazi/è-ü-€/日本語.txt"), "uni").unwrap();
    fs::write(d.join("p".repeat(200)), "long").unwrap();
    fs::write(
        d.join("big.bin"),
        (0..300_000u32).map(|i| (i * 31) as u8).collect::<Vec<_>>(),
    )
    .unwrap();
    fs::write(d.join("A.txt"), "upper").unwrap();
    symlink("a/b/c", d.join("ln_dir")).unwrap();
    symlink("/etc/hostname", d.join("ln_abs")).unwrap();
    symlink("../../x", d.join("a/ln_rel")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(d.join("empty_file"), fs::Permissions::from_mode(0o600)).unwrap();
    d
}

fn same(a: &Path, b: &Path) {
    run(Command::new("diff")
        .args(["-r", "--no-dereference"])
        .arg(a)
        .arg(b));
}

#[test]
fn pure_udf_all_revisions_mounts_in_kernel() {
    let root = tmp("udf-pure");
    let src = rich(&root);
    for (name, rev) in REVS {
        let iso = root.join(format!("p{name}.iso"));
        build(
            &src,
            &iso,
            &IsoOptions {
                udf: Some(rev),
                iso9660: false,
                volume_id: "RICH".into(),
                ..Default::default()
            },
        );
        if have("udfinfo") {
            let info = out(Command::new("udfinfo").arg(&iso));
            assert!(info.contains(&format!("udfrev={name}")), "{info}");
            assert!(
                info.contains("lvid=RICH") && info.contains("integrity=closed"),
                "{info}"
            );
            assert!(
                info.contains("numfiles=9") && info.contains("numdirs=16"),
                "{info}"
            );
            assert!(info.contains("start=256, blocks=1, type=ANCHOR"), "{info}");
        }
        if let Some(m) = kernel_mount(&iso, "udf") {
            same(&src, &m.path);
        }
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn pure_udf_extracts_with_7z() {
    if !have("7z") {
        return;
    }
    let root = tmp("udf-7z");
    let src = rich(&root);
    // 7z rifiuta i symlink UDF (limite suo): si prova su una copia senza
    for l in ["ln_dir", "ln_abs", "a/ln_rel"] {
        fs::remove_file(src.join(l)).unwrap();
    }
    let iso = root.join("p.iso");
    build(
        &src,
        &iso,
        &IsoOptions {
            udf: Some(UdfRevision::V201),
            iso9660: false,
            ..Default::default()
        },
    );
    let listing = out(Command::new("7z").arg("l").arg(&iso));
    assert!(listing.contains("Type = Udf"), "{listing}");
    let x = root.join("x");
    run(Command::new("7z")
        .arg("x")
        .arg("-y")
        .arg(format!("-o{}", x.display()))
        .arg(&iso));
    same(&src, &x);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn bridge_is_valid_udf_and_iso9660() {
    let root = tmp("udf-bridge");
    let src = rich(&root);
    // ISO9660 vieta oltre 8 livelli: il bridge usa un albero meno profondo
    fs::remove_dir_all(src.join("a/b/c/d/e/f/g/h")).unwrap();
    let iso = root.join("b.iso");
    build(
        &src,
        &iso,
        &IsoOptions {
            udf: Some(UdfRevision::V201),
            rock_ridge: true,
            joliet: true,
            volume_id: "BR".into(),
            ..Default::default()
        },
    );
    if have("udfinfo") {
        let info = out(Command::new("udfinfo").arg(&iso));
        assert!(
            info.contains("lvid=BR") && info.contains("udfrev=2.01"),
            "{info}"
        );
    }
    if have("xorriso") {
        let ext = root.join("ext");
        run(Command::new("xorriso")
            .args(["-osirrox", "on", "-indev"])
            .arg(&iso)
            .args(["-extract", "/"])
            .arg(&ext));
        same(&src, &ext);
    }
    for fs_type in ["udf", "iso9660"] {
        if let Some(m) = kernel_mount(&iso, fs_type) {
            same(&src, &m.path);
        }
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn deep_tree_is_ok_in_udf_but_not_in_iso9660() {
    let root = tmp("udf-deep");
    let src = rich(&root); // 12 livelli
    let bridge = create_iso(
        &src,
        std::io::sink(),
        &IsoOptions {
            udf: Some(UdfRevision::V201),
            ..Default::default()
        },
    )
    .unwrap_err()
    .to_string();
    assert!(
        bridge.contains("oltre 8 livelli") && bridge.contains("--no-iso9660"),
        "{bridge}"
    );
    create_iso(
        &src,
        std::io::sink(),
        &IsoOptions {
            udf: Some(UdfRevision::V201),
            iso9660: false,
            ..Default::default()
        },
    )
    .unwrap();
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn invalid_combinations_are_rejected() {
    let root = tmp("udf-bad");
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    let e = |o: IsoOptions| {
        create_iso(&src, std::io::sink(), &o)
            .unwrap_err()
            .to_string()
    };
    assert!(e(IsoOptions {
        iso9660: false,
        ..Default::default()
    })
    .contains("servono ISO9660 o UDF"));
    let udf = Some(UdfRevision::V201);
    assert!(e(IsoOptions {
        iso9660: false,
        udf,
        joliet: true,
        ..Default::default()
    })
    .contains("richiedono ISO9660"));
    assert!(e(IsoOptions {
        iso9660: false,
        udf,
        rock_ridge: true,
        ..Default::default()
    })
    .contains("richiedono ISO9660"));
    let _ = fs::remove_dir_all(&root);
}

/// Metadati oltre il settore 256: l'AVDP resta a 256 e nessuna struttura lo calpesta.
#[test]
fn large_metadata_skips_the_anchor_sector() {
    let root = tmp("udf-meta");
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    for i in 0..9000 {
        fs::write(
            src.join(format!("file-with-a-fairly-long-name-{i:05}.txt")),
            format!("{i}"),
        )
        .unwrap();
    }
    let iso = root.join("m.iso");
    build(
        &src,
        &iso,
        &IsoOptions {
            udf: Some(UdfRevision::V201),
            rock_ridge: true,
            joliet: true,
            ..Default::default()
        },
    );
    let bytes = fs::read(&iso).unwrap();
    let sector = |n: usize| &bytes[n * 2048..(n + 1) * 2048];
    assert_eq!(
        u16::from_le_bytes([sector(256)[0], sector(256)[1]]),
        2,
        "AVDP a 256"
    );
    let n = bytes.len() / 2048;
    assert_eq!(
        u16::from_le_bytes([sector(n - 1)[0], sector(n - 1)[1]]),
        2,
        "AVDP finale"
    );
    assert!(n > 1000, "i metadati dovevano superare 256 settori");
    if have("udfinfo") {
        let info = out(Command::new("udfinfo").arg(&iso));
        assert!(info.contains("numfiles=9000"), "{info}");
    }
    for fs_type in ["udf", "iso9660"] {
        if let Some(m) = kernel_mount(&iso, fs_type) {
            same(&src, &m.path);
        }
    }
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn empty_tree_and_odd_names() {
    let root = tmp("udf-odd");
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    let iso = root.join("e.iso");
    build(
        &src,
        &iso,
        &IsoOptions {
            udf: Some(UdfRevision::V260),
            iso9660: false,
            ..Default::default()
        },
    );
    if let Some(m) = kernel_mount(&iso, "udf") {
        assert_eq!(fs::read_dir(&m.path).unwrap().count(), 0);
    }
    // nomi che differiscono solo per maiuscole: UDF li distingue con un suffisso (Windows non può averli entrambi)
    fs::write(src.join("Readme"), "1").unwrap();
    fs::write(src.join("README"), "2").unwrap();
    build(
        &src,
        &iso,
        &IsoOptions {
            udf: Some(UdfRevision::V260),
            iso9660: false,
            ..Default::default()
        },
    );
    if let Some(m) = kernel_mount(&iso, "udf") {
        let mut names: Vec<String> = fs::read_dir(&m.path)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, ["README", "Readme1"]);
    }
    let _ = fs::remove_dir_all(&root);
}
