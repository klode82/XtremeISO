//! El Torito e immagini ibride: validazione con xorriso/genisoimage e avvio reale con qemu.

mod common;

use common::*;
use std::{fs, io::Cursor, path::Path, process::Command};
use xtremeiso_core::{BootEntry, Editor, Emulation, Image, IsoOptions, Names, Platform};

fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b[..4].try_into().unwrap())
}

fn boot_src(root: &Path) -> std::path::PathBuf {
    let src = root.join("src");
    fs::create_dir_all(src.join("boot")).unwrap();
    fs::create_dir_all(src.join("EFI")).unwrap();
    fs::write(src.join("boot/boot.bin"), boot_image()).unwrap();
    fs::write(
        src.join("EFI/efi.img"),
        (0..65536u32).map(|i| i as u8).collect::<Vec<_>>(),
    )
    .unwrap();
    fs::write(src.join("hello.txt"), "ciao\n").unwrap();
    src
}

fn opts() -> IsoOptions {
    let mut b = BootEntry::bios("/boot/boot.bin");
    b.info_table = true;
    IsoOptions {
        rock_ridge: true,
        joliet: true,
        boot: vec![b, BootEntry::efi("EFI/efi.img")],
        ..Default::default()
    }
}

fn open(p: &Path) -> Image<Cursor<Vec<u8>>> {
    Image::open(Cursor::new(fs::read(p).unwrap())).unwrap()
}

#[test]
fn el_torito_bios_and_uefi() {
    let root = tmp("et");
    let iso = root.join("t.iso");
    build(&boot_src(&root), &iso, &opts());

    let mut img = open(&iso);
    assert!(img.info().bootable);
    let b = img.boot_entries().unwrap();
    assert_eq!(b.len(), 2);
    assert_eq!(
        (
            b[0].platform_name(),
            b[0].path.as_deref(),
            b[0].sector_count,
            b[0].info_table
        ),
        ("BIOS", Some("/boot/boot.bin"), 4, true)
    );
    assert_eq!(
        (
            b[1].platform_name(),
            b[1].path.as_deref(),
            b[1].sector_count,
            b[1].info_table
        ),
        ("UEFI", Some("/EFI/efi.img"), 128, false)
    );
    assert_eq!(img.verify().unwrap(), Vec::<String>::new());

    // boot-info-table scritta davvero: PVD=16, LBA, lunghezza
    let mut data = vec![];
    img.read_boot_image(&b[0], &mut data).unwrap();
    assert_eq!(
        (le32(&data[8..]), le32(&data[12..]), le32(&data[16..])),
        (16, b[0].rba, 2048)
    );
    assert_eq!(&data[..2], &[0xEB, 0x3E]);

    if have("xorriso") {
        let rep = out(Command::new("xorriso")
            .args(["-indev"])
            .arg(&iso)
            .args(["-report_el_torito", "plain"]));
        let lines: Vec<&str> = rep
            .lines()
            .filter(|l| l.starts_with("El Torito boot img"))
            .collect();
        assert_eq!(lines.len(), 2, "{rep}");
        assert!(
            lines[0].contains("BIOS") && lines[0].trim_end().ends_with(&b[0].rba.to_string()),
            "{rep}"
        );
        assert!(
            lines[1].contains("UEFI") && lines[1].trim_end().ends_with(&b[1].rba.to_string()),
            "{rep}"
        );
        assert!(rep.contains("boot-info-table"), "{rep}");
    }
    assert!(
        boots(&["-cdrom", iso.to_str().unwrap(), "-boot", "d"], "XOK"),
        "l'ISO non si avvia in qemu"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn reads_genisoimage_boot() {
    if !have("genisoimage") {
        return;
    }
    let root = tmp("et-gen");
    let src = boot_src(&root);
    let iso = root.join("g.iso");
    run(Command::new("genisoimage")
        .args([
            "-quiet",
            "-R",
            "-J",
            "-b",
            "boot/boot.bin",
            "-no-emul-boot",
            "-boot-load-size",
            "4",
            "-boot-info-table",
            "-eltorito-catalog",
            "boot/boot.cat",
            "-o",
        ])
        .arg(&iso)
        .arg(&src));
    let mut img = open(&iso);
    let b = img.boot_entries().unwrap();
    assert_eq!(b.len(), 1);
    assert_eq!(
        (
            b[0].platform_name(),
            b[0].media_name(),
            b[0].sector_count,
            b[0].info_table
        ),
        ("BIOS", "no-emulation", 4, true)
    );
    assert_eq!(b[0].path.as_deref(), Some("/boot/boot.bin"));
    assert_eq!(img.verify().unwrap(), Vec::<String>::new());

    // si modifica l'immagine di un altro tool: il boot sopravvive e la tabella segue il nuovo LBA
    let extra = root.join("extra");
    fs::create_dir_all(extra.join("d")).unwrap();
    fs::create_dir_all(extra.join("e")).unwrap(); // +2 settori di cartelle, -1 per il catalogo esposto come file
    fs::write(extra.join("d/f"), "x").unwrap();
    let mut ed = Editor::open(Cursor::new(fs::read(&iso).unwrap()), Names::Auto).unwrap();
    ed.add(&extra, "/").unwrap();
    let edited = root.join("e.iso");
    ed.write(std::io::BufWriter::new(fs::File::create(&edited).unwrap()))
        .unwrap();
    let mut img2 = open(&edited);
    let b2 = img2.boot_entries().unwrap();
    assert_eq!(b2.len(), 1);
    assert!(
        img2.entries(Names::Auto)
            .unwrap()
            .iter()
            .all(|e| e.path != "/boot/boot.cat"),
        "catalogo orfano"
    );
    assert_ne!(
        b2[0].rba, b[0].rba,
        "il file di boot doveva cambiare posizione"
    );
    assert!(b2[0].info_table);
    assert_eq!(img2.verify().unwrap(), Vec::<String>::new());
    assert!(boots(
        &["-cdrom", edited.to_str().unwrap(), "-boot", "d"],
        "XOK"
    ));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn edit_keeps_boot_and_can_replace_or_drop_it() {
    let root = tmp("et-edit");
    let src = boot_src(&root);
    let iso = root.join("t.iso");
    build(&src, &iso, &opts());
    let bytes = fs::read(&iso).unwrap();
    let write = |ed: Editor<Cursor<Vec<u8>>>, name: &str| {
        let p = root.join(name);
        ed.write(std::io::BufWriter::new(fs::File::create(&p).unwrap()))
            .unwrap();
        p
    };

    // 1) modifica qualsiasi: il boot resta, con la tabella ricalcolata
    let mut ed = Editor::open(Cursor::new(bytes.clone()), Names::Auto).unwrap();
    fs::create_dir_all(root.join("more/dir")).unwrap();
    ed.add(&root.join("more"), "/").unwrap();
    let p1 = write(ed, "1.iso");
    let mut i1 = open(&p1);
    let b = i1.boot_entries().unwrap();
    assert_eq!(b.len(), 2);
    assert!(b[0].info_table);
    assert!(boots(
        &["-cdrom", p1.to_str().unwrap(), "-boot", "d"],
        "XOK"
    ));

    // 2) sostituzione dell'immagine: rimuovi, riaggiungi con lo stesso percorso
    let newboot = root.join("new/boot.bin");
    fs::create_dir_all(root.join("new")).unwrap();
    let mut code = boot_image();
    code[0x40 + 4] = 0x59; // 'Y' invece di 'X'... ma lo stesso marcatore "XOK" non basta: cambia il secondo carattere
    code[0x40 + 7] = 0x5A; // 'Z'
    fs::write(&newboot, &code).unwrap();
    let mut ed = Editor::open(Cursor::new(bytes.clone()), Names::Auto).unwrap();
    ed.remove("/boot/boot.bin").unwrap();
    ed.add(&newboot, "/boot").unwrap();
    let p2 = write(ed, "2.iso");
    let mut i2 = open(&p2);
    let b = i2.boot_entries().unwrap();
    let mut data = vec![];
    i2.read_boot_image(&b[0], &mut data).unwrap();
    assert_eq!(data[0x40 + 7], 0x5A);
    assert_eq!(le32(&data[12..]), b[0].rba, "boot-info-table aggiornata");
    assert!(boots(
        &["-cdrom", p2.to_str().unwrap(), "-boot", "d"],
        "YZK"
    ));

    // 3) rimuovere il file di boot senza sostituirlo è un errore chiaro
    let mut ed = Editor::open(Cursor::new(bytes.clone()), Names::Auto).unwrap();
    ed.remove("/boot").unwrap();
    let e = ed.write(std::io::sink()).unwrap_err().to_string();
    assert!(e.contains("immagine di boot non trovata"), "{e}");

    // 4) togliere il boot
    let mut ed = Editor::open(Cursor::new(bytes), Names::Auto).unwrap();
    ed.options_mut().boot.clear();
    let p4 = write(ed, "4.iso");
    let mut i4 = open(&p4);
    assert!(!i4.info().bootable && i4.boot_entries().unwrap().is_empty());
    assert_eq!(i4.verify().unwrap(), Vec::<String>::new());
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn bad_boot_configs_are_rejected() {
    let root = tmp("et-bad");
    let src = boot_src(&root);
    let try_with = |b: BootEntry| {
        create_err(
            &src,
            IsoOptions {
                boot: vec![b],
                ..Default::default()
            },
        )
    };
    assert!(try_with(BootEntry::bios("/nope.bin")).contains("non trovata"));
    assert!(try_with(BootEntry::bios("/boot")).contains("non trovata")); // è una cartella
    let mut e = BootEntry::bios("/boot/boot.bin");
    e.emulation = Emulation::Floppy; // 2048 byte non è un floppy
    assert!(try_with(e).contains("floppy"));
    fs::write(src.join("tiny"), "abc").unwrap();
    let mut e = BootEntry::bios("/tiny");
    e.info_table = true;
    assert!(try_with(e).contains("64 byte"));
    let _ = fs::remove_dir_all(&root);
}

fn create_err(src: &Path, o: IsoOptions) -> String {
    xtremeiso_core::create_iso(src, std::io::sink(), &o)
        .unwrap_err()
        .to_string()
}

#[test]
fn floppy_and_hard_disk_emulation() {
    let root = tmp("et-emu");
    let src = root.join("src");
    fs::create_dir_all(&src).unwrap();
    let mut floppy = vec![0u8; 1_474_560];
    floppy[510..512].copy_from_slice(&[0x55, 0xAA]);
    fs::write(src.join("fd.img"), &floppy).unwrap();
    let mut hd = vec![0u8; 8192];
    hd[450] = 0x83; // tipo della prima partizione
    fs::write(src.join("hd.img"), &hd).unwrap();
    let iso = root.join("t.iso");
    let mut f = BootEntry::bios("/fd.img");
    f.emulation = Emulation::Floppy;
    let mut h = BootEntry::bios("/hd.img");
    h.emulation = Emulation::HardDisk;
    h.platform = Platform::Mac; // piattaforma qualsiasi: la prima voce decide la validation entry
    build(
        &src,
        &iso,
        &IsoOptions {
            boot: vec![f, h],
            ..Default::default()
        },
    );
    let b = open(&iso).boot_entries().unwrap();
    assert_eq!((b[0].media_name(), b[0].sector_count), ("floppy 1.44M", 1));
    assert_eq!(
        (b[1].media_name(), b[1].system_type, b[1].platform_name()),
        ("hard disk", 0x83, "Mac")
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn hybrid_mbr_boots_as_disk_and_survives_edit() {
    let root = tmp("hy");
    let src = boot_src(&root);
    // codice MBR: scrive "HY" su COM1 e si ferma
    let mbr = [
        0xBA, 0xF8, 0x03, 0xB0, 0x48, 0xEE, 0xB0, 0x59, 0xEE, 0xF4, 0xEB, 0xFD,
    ];
    let iso = root.join("h.iso");
    let mut o = opts();
    o.hybrid_mbr = Some(mbr.to_vec());
    build(&src, &iso, &o);
    let bytes = fs::read(&iso).unwrap();
    let total = bytes.len() as u64 / 2048;
    assert_eq!((bytes[510], bytes[511]), (0x55, 0xAA));
    assert_eq!(&bytes[..mbr.len()], &mbr);
    // partizione 1: tutta l'immagine; partizione 2: EFI
    assert_eq!(
        (
            bytes[446],
            bytes[450],
            le32(&bytes[454..]),
            le32(&bytes[458..]) as u64
        ),
        (0x80, 0x83, 0, total * 4)
    );
    let mut img = open(&iso);
    let b = img.boot_entries().unwrap();
    assert_eq!(
        (bytes[466], le32(&bytes[470..]), le32(&bytes[474..])),
        (0xEF, b[1].rba * 4, 128)
    );
    // codice <= 432 byte: negli 8 byte 432..440 c'è l'LBA (settori da 512) dell'immagine BIOS
    assert_eq!(
        u64::from_le_bytes(bytes[432..440].try_into().unwrap()),
        b[0].rba as u64 * 4
    );
    assert!(
        boots(
            &[
                "-drive",
                &format!("file={},format=raw,if=ide", iso.display())
            ],
            "HY"
        ),
        "l'ISO ibrida non parte come disco"
    );
    // ancora avviabile come CD
    assert!(boots(
        &["-cdrom", iso.to_str().unwrap(), "-boot", "d"],
        "XOK"
    ));

    // l'MBR sopravvive a una modifica
    let mut ed = Editor::open(Cursor::new(bytes), Names::Auto).unwrap();
    fs::create_dir_all(root.join("zz")).unwrap();
    ed.add(&root.join("zz"), "/").unwrap();
    let e = root.join("e.iso");
    ed.write(std::io::BufWriter::new(fs::File::create(&e).unwrap()))
        .unwrap();
    let eb = fs::read(&e).unwrap();
    assert_eq!(&eb[..mbr.len()], &mbr);
    assert_eq!((eb[510], eb[511], eb[450]), (0x55, 0xAA, 0x83));
    assert!(boots(
        &["-drive", &format!("file={},format=raw,if=ide", e.display())],
        "HY"
    ));
    let _ = fs::remove_dir_all(&root);
}
