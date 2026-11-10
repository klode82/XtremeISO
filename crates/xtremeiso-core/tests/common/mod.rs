//! Helper condivisi dai test di integrazione.
#![allow(dead_code)]

use std::{fs, os::unix::fs::symlink, path::PathBuf, process::Command};
use xtremeiso_core::{create_iso, IsoOptions};

pub fn have(tool: &str) -> bool {
    Command::new("which")
        .arg(tool)
        .output()
        .is_ok_and(|o| o.status.success())
}

pub fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("xtremeiso-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

pub fn fixture(root: &std::path::Path) -> PathBuf {
    let d = root.join("src");
    fs::create_dir_all(d.join("sub/deeper")).unwrap();
    fs::write(d.join("hello.txt"), "ciao\n").unwrap();
    fs::write(d.join("empty"), "").unwrap();
    fs::write(
        d.join("big.bin"),
        (0..100_000u32).map(|i| (i * 7) as u8).collect::<Vec<_>>(),
    )
    .unwrap();
    fs::write(
        d.join("a-very-long-file-name-that-exceeds-the-iso-limits-by-far.txt"),
        "long",
    )
    .unwrap();
    fs::write(
        d.join("collide-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-1.txt"),
        "one",
    )
    .unwrap();
    fs::write(
        d.join("collide-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-2.txt"),
        "two",
    )
    .unwrap();
    fs::write(d.join("sub/Mixed Case è.TXT"), "x").unwrap();
    fs::write(d.join("sub/deeper/file.with.dots.tar.gz"), "y").unwrap();
    symlink("hello.txt", d.join("link")).unwrap();
    // nome massimo (255) e symlink lungo: forzano l'area di continuazione Rock Ridge
    fs::write(d.join("n".repeat(255)), "max").unwrap();
    symlink(
        "sub/deeper/".repeat(30) + "../../../../../hello.txt",
        d.join("longlink"),
    )
    .unwrap();
    d
}

pub fn build(src: &std::path::Path, iso: &std::path::Path, o: &IsoOptions) {
    create_iso(
        src,
        std::io::BufWriter::new(fs::File::create(iso).unwrap()),
        o,
    )
    .unwrap();
    assert_eq!(fs::metadata(iso).unwrap().len() % 2048, 0);
}

pub fn out(cmd: &mut Command) -> String {
    let o = cmd.output().unwrap();
    assert!(
        o.status.success(),
        "{cmd:?}: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// Esegue un comando e fallisce se l'exit code non è 0.
pub fn run(cmd: &mut Command) {
    out(cmd);
}

/// Immagine di boot di prova: salta la boot-info-table (byte 8..64), poi scrive "XOK" su COM1 e si ferma.
pub fn boot_image() -> Vec<u8> {
    let code = [
        0xBA, 0xF8, 0x03, 0xB0, 0x58, 0xEE, 0xB0, 0x4F, 0xEE, 0xB0, 0x4B, 0xEE, 0xF4, 0xEB, 0xFD,
    ];
    let mut img = vec![0u8; 2048];
    img[..2].copy_from_slice(&[0xEB, 0x3E]);
    img[0x40..0x40 + code.len()].copy_from_slice(&code);
    img
}

/// Avvia qemu (BIOS) con gli argomenti dati e aspetta `marker` sulla porta seriale.
/// Ritorna true anche se qemu non è installato (test saltato).
pub fn boots(args: &[&str], marker: &str) -> bool {
    use std::{
        process::Stdio,
        time::{Duration, Instant},
    };
    if !have("qemu-system-x86_64") {
        return true;
    }
    let serial = std::env::temp_dir().join(format!(
        "xtremeiso-serial-{}-{}",
        std::process::id(),
        marker
    ));
    let _ = fs::remove_file(&serial);
    let mut child = Command::new("qemu-system-x86_64")
        .args(["-display", "none", "-m", "16", "-no-reboot", "-serial"])
        .arg(format!("file:{}", serial.display()))
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    let mut ok = false;
    while start.elapsed() < Duration::from_secs(40) {
        if fs::read_to_string(&serial).is_ok_and(|s| s.contains(marker)) {
            ok = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_file(&serial);
    ok
}

/// Immagine montata dal kernel (via udisks, senza root); smontata alla fine.
pub struct Mounted {
    dev: String,
    pub path: PathBuf,
}

impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = Command::new("udisksctl")
            .args(["unmount", "-b", &self.dev])
            .output();
        let _ = Command::new("udisksctl")
            .args(["loop-delete", "-b", &self.dev])
            .output();
    }
}

/// Monta `iso` con il kernel come `fstype` ("udf", "iso9660"). None se non è possibile (test saltato).
pub fn kernel_mount(iso: &std::path::Path, fstype: &str) -> Option<Mounted> {
    if !have("udisksctl") {
        return None;
    }
    let o = Command::new("udisksctl")
        .args(["loop-setup", "-r", "-f"])
        .arg(iso)
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    let dev = text
        .split(" as ")
        .nth(1)?
        .trim()
        .trim_end_matches('.')
        .to_string();
    if !o.status.success() || !dev.starts_with("/dev/") {
        return None;
    }
    let mut m = Mounted {
        dev: dev.clone(),
        path: PathBuf::new(),
    };
    let mounted = Command::new("udisksctl")
        .args(["mount", "-b", &dev, "-t", fstype])
        .output()
        .ok()?;
    if !mounted.status.success() {
        return None; // il Drop libera il loop device
    }
    let t = Command::new("findmnt")
        .args(["-n", "-o", "TARGET", &dev])
        .output()
        .ok()?;
    m.path = PathBuf::from(String::from_utf8_lossy(&t.stdout).trim());
    (!m.path.as_os_str().is_empty()).then_some(m)
}
