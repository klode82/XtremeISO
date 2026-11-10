use clap::{Args, Parser, Subcommand, ValueEnum};
use std::{
    error::Error,
    fs::{self, File},
    io::{BufReader, BufWriter},
    path::{Path, PathBuf},
    process::ExitCode,
};
use xtremeiso_core::{
    create_iso, BootEntry, Editor, Emulation, Image, IsoOptions, Kind, Level, Names, Platform,
    UdfRevision,
};

#[derive(Parser)]
#[command(name = "xtremeiso", version, about = "Crea e gestisce immagini disco")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum NamesArg {
    /// Rock Ridge se presente, altrimenti Joliet, altrimenti ISO9660
    Auto,
    Iso,
    Joliet,
    Rr,
}

impl From<NamesArg> for Names {
    fn from(n: NamesArg) -> Names {
        match n {
            NamesArg::Auto => Names::Auto,
            NamesArg::Iso => Names::Iso,
            NamesArg::Joliet => Names::Joliet,
            NamesArg::Rr => Names::RockRidge,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum EmuArg {
    None,
    /// Il tipo (1.2/1.44/2.88 MB) si deduce dalla dimensione del file
    Floppy,
    Hd,
}

#[derive(Args, Clone)]
struct UdfArgs {
    /// Aggiunge UDF (immagine bridge con ISO9660): 1.02, 1.50, 2.00, 2.01, 2.50 o 2.60
    #[arg(long, value_parser = parse_udf)]
    udf: Option<UdfRevision>,
    /// Immagine solo UDF, senza struttura ISO9660 (richiede --udf)
    #[arg(long, requires = "udf")]
    no_iso9660: bool,
}

fn parse_udf(s: &str) -> Result<UdfRevision, String> {
    UdfRevision::parse(s).ok_or_else(|| {
        format!("revisione UDF sconosciuta: {s} (1.02, 1.50, 2.00, 2.01, 2.50, 2.60)")
    })
}

#[derive(Args, Clone)]
struct BootArgs {
    /// Immagine di boot BIOS: percorso dentro l'ISO, es. isolinux/isolinux.bin
    #[arg(long)]
    boot: Option<String>,
    /// Immagine di boot UEFI (es. EFI/boot/efiboot.img)
    #[arg(long)]
    boot_efi: Option<String>,
    /// Scrive la boot-info-table nell'immagine BIOS (richiesta da isolinux)
    #[arg(long)]
    boot_info_table: bool,
    /// Settori da 512 byte caricati dall'immagine BIOS (default 4)
    #[arg(long)]
    boot_load_size: Option<u16>,
    #[arg(long, value_enum)]
    boot_emulation: Option<EmuArg>,
    /// File con il codice MBR (max 440 byte, es. isohdpfx.bin): rende l'immagine avviabile da USB
    #[arg(long)]
    isohybrid_mbr: Option<PathBuf>,
}

/// Applica le opzioni di boot: sostituisce le voci BIOS/UEFI indicate, lascia le altre.
fn apply_boot(o: &mut IsoOptions, b: &BootArgs, clear: bool) -> Result<(), Box<dyn Error>> {
    if clear {
        o.boot.clear();
    }
    if let Some(p) = &b.boot {
        o.boot.retain(|e| e.platform != Platform::Bios);
        o.boot.insert(0, BootEntry::bios(p.trim_start_matches('/')));
    }
    if let Some(p) = &b.boot_efi {
        o.boot.retain(|e| e.platform != Platform::Efi);
        o.boot.push(BootEntry::efi(p.trim_start_matches('/')));
    }
    if b.boot_info_table || b.boot_load_size.is_some() || b.boot_emulation.is_some() {
        let e = o
            .boot
            .iter_mut()
            .find(|e| e.platform == Platform::Bios)
            .ok_or("--boot-info-table/--boot-load-size/--boot-emulation richiedono un'immagine BIOS (--boot)")?;
        e.info_table |= b.boot_info_table;
        e.load_size = b.boot_load_size.or(e.load_size);
        if let Some(m) = b.boot_emulation {
            e.emulation = match m {
                EmuArg::None => Emulation::None,
                EmuArg::Floppy => Emulation::Floppy,
                EmuArg::Hd => Emulation::HardDisk,
            };
        }
    }
    if let Some(f) = &b.isohybrid_mbr {
        o.hybrid_mbr = Some(fs::read(f).map_err(|e| format!("{}: {e}", f.display()))?);
    }
    Ok(())
}

fn level_of(l: u8) -> Level {
    match l {
        1 => Level::L1,
        2 => Level::L2,
        _ => Level::L3,
    }
}

fn open(iso: &Path) -> Result<Image<BufReader<File>>, Box<dyn Error>> {
    let f = File::open(iso).map_err(|e| format!("{}: {e}", iso.display()))?;
    Ok(Image::open(BufReader::new(f))?)
}

#[derive(Subcommand)]
enum Cmd {
    /// Elenca il contenuto di un'immagine
    List {
        iso: PathBuf,
        /// Formato lungo: permessi, dimensione, data
        #[arg(short, long)]
        long: bool,
        #[arg(long, value_enum, default_value = "auto")]
        names: NamesArg,
    },
    /// Mostra le informazioni del volume
    Info { iso: PathBuf },
    /// Estrae file e cartelle (tutto se non si indicano percorsi)
    Extract {
        iso: PathBuf,
        /// Percorsi dentro l'immagine, es. /dir/file.txt
        paths: Vec<String>,
        /// Cartella di destinazione
        #[arg(short, long)]
        output: PathBuf,
        #[arg(long, value_enum, default_value = "auto")]
        names: NamesArg,
    },
    /// Controlla la coerenza della struttura (esce con errore se trova problemi)
    Verify { iso: PathBuf },
    /// Modifica un'immagine: riscrive tutto l'ISO (ordine: remove, rename, add). Il boot non viene conservato.
    Edit {
        iso: PathBuf,
        /// Nuovo file; se omesso l'immagine viene sostituita sul posto
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Aggiunge un file/cartella locale: LOCALE oppure LOCALE:/cartella/destinazione
        #[arg(long)]
        add: Vec<String>,
        /// Rimuove un percorso dell'immagine
        #[arg(long)]
        remove: Vec<String>,
        /// Rinomina: /percorso/vecchio=nuovo_nome
        #[arg(long)]
        rename: Vec<String>,
        #[arg(long, value_enum, default_value = "auto")]
        names: NamesArg,
        /// Livello ISO9660 (default 3)
        #[arg(long, value_parser = clap::value_parser!(u8).range(1..=3))]
        level: Option<u8>,
        /// Forza Joliet anche se l'originale non lo aveva
        #[arg(long)]
        joliet: bool,
        /// Forza Rock Ridge anche se l'originale non lo aveva
        #[arg(long)]
        rock_ridge: bool,
        /// Rimuove il boot (El Torito); di default viene conservato
        #[arg(long)]
        no_boot: bool,
        #[command(flatten)]
        boot: BootArgs,
    },
    /// Mostra le voci di boot (El Torito) di un'immagine
    BootInfo { iso: PathBuf },
    /// Estrae un'immagine di boot
    BootExtract {
        iso: PathBuf,
        /// Numero della voce (da boot-info, parte da 1)
        #[arg(long, default_value_t = 1)]
        index: usize,
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Crea un'immagine ISO da una cartella
    Create {
        /// Cartella sorgente
        dir: PathBuf,
        /// File ISO di output
        #[arg(short, long)]
        output: PathBuf,
        /// Livello ISO9660 (1: 8.3, 2/3: nomi fino a 30 caratteri, 3: file > 4 GiB)
        #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u8).range(1..=3))]
        level: u8,
        /// Aggiunge l'estensione Joliet (nomi Unicode)
        #[arg(long)]
        joliet: bool,
        /// Aggiunge l'estensione Rock Ridge (permessi POSIX, symlink, nomi lunghi)
        #[arg(long)]
        rock_ridge: bool,
        #[arg(long, default_value = "CDROM")]
        volume_id: String,
        #[arg(long, default_value = "")]
        publisher: String,
        #[command(flatten)]
        boot: BootArgs,
        #[command(flatten)]
        udf: UdfArgs,
    },
}

#[allow(clippy::too_many_arguments)]
fn create(
    dir: PathBuf,
    output: PathBuf,
    level: u8,
    joliet: bool,
    rock_ridge: bool,
    volume_id: String,
    publisher: String,
    boot: BootArgs,
    udf: UdfArgs,
) -> Result<(), Box<dyn Error>> {
    let src = fs::canonicalize(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let out_abs =
        fs::canonicalize(parent)?.join(output.file_name().ok_or("nome di output non valido")?);
    if out_abs.starts_with(&src) {
        return Err("l'output non può stare dentro la cartella sorgente".into());
    }
    let mut opts = IsoOptions {
        level: level_of(level),
        joliet,
        rock_ridge,
        volume_id,
        publisher,
        ..Default::default()
    };
    opts.udf = udf.udf;
    opts.iso9660 = !udf.no_iso9660;
    apply_boot(&mut opts, &boot, false)?;
    let res = create_iso(&src, BufWriter::new(File::create(&output)?), &opts);
    if res.is_err() {
        let _ = fs::remove_file(&output);
    }
    res?;
    println!("creato {}", output.display());
    Ok(())
}

fn info(iso: &Path) -> Result<(), Box<dyn Error>> {
    let mut img = open(iso)?;
    let i = img.info().clone();
    let e = img.entries(Names::Auto)?;
    let count = |k: Kind| e.iter().filter(|x| x.kind == k).count();
    let bytes: u64 = e.iter().map(|x| x.size).sum();
    let yn = |b: bool| if b { "sì" } else { "no" };
    println!("Volume id:    {}", i.volume_id);
    println!("System id:    {}", i.system_id);
    println!("Volume set:   {}", i.volume_set_id);
    println!("Publisher:    {}", i.publisher);
    println!("Preparer:     {}", i.preparer);
    println!("Application:  {}", i.application);
    println!("Creato:       {}", i.created.as_deref().unwrap_or("-"));
    println!("Modificato:   {}", i.modified.as_deref().unwrap_or("-"));
    println!(
        "Dimensione:   {} settori ({} byte)",
        i.blocks,
        i.blocks as u64 * 2048
    );
    println!(
        "Estensioni:   Joliet {}, Rock Ridge {}",
        yn(i.joliet),
        yn(i.rock_ridge)
    );
    println!(
        "Contenuto:    {} file ({bytes} byte), {} cartelle, {} link",
        count(Kind::File),
        count(Kind::Dir),
        count(Kind::Symlink)
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn edit(
    iso: PathBuf,
    output: Option<PathBuf>,
    add: Vec<String>,
    remove: Vec<String>,
    rename: Vec<String>,
    names: NamesArg,
    level: Option<u8>,
    joliet: bool,
    rock_ridge: bool,
    no_boot: bool,
    boot: BootArgs,
) -> Result<(), Box<dyn Error>> {
    let target = output.unwrap_or_else(|| iso.clone());
    let src = File::open(&iso).map_err(|e| format!("{}: {e}", iso.display()))?;
    let mut ed = Editor::open(BufReader::new(src), names.into())?;
    let o = ed.options_mut();
    o.level = level.map_or(Level::L3, level_of);
    o.joliet |= joliet;
    o.rock_ridge |= rock_ridge;
    let mut opts = o.clone();
    apply_boot(&mut opts, &boot, no_boot)?;
    *ed.options_mut() = opts;
    for p in &remove {
        ed.remove(p)?;
    }
    for r in &rename {
        let (old, new) = r
            .split_once('=')
            .ok_or("--rename richiede /percorso/vecchio=nuovo_nome")?;
        ed.rename(old, new)?;
    }
    for a in &add {
        // "LOCALE:/dest": si separa solo sull'ultimo ':' seguito da '/'
        let (local, dest) = match a.rsplit_once(':') {
            Some((l, d)) if d.starts_with('/') => (l, d),
            _ => (a.as_str(), "/"),
        };
        ed.add(Path::new(local), dest)?;
    }
    // file temporaneo + rename: l'originale resta intatto in caso di errore
    let name = target
        .file_name()
        .ok_or("nome di output non valido")?
        .to_string_lossy();
    let tmp = target.with_file_name(format!(".{name}.xtremeiso-tmp"));
    let res = File::create(&tmp)
        .map_err(Into::into)
        .and_then(|f| ed.write(BufWriter::new(f)));
    if let Err(e) = res {
        let _ = fs::remove_file(&tmp);
        return Err(e.into());
    }
    fs::rename(&tmp, &target)?;
    println!("scritto {}", target.display());
    Ok(())
}

fn run(cli: Cli) -> Result<(), Box<dyn Error>> {
    match cli.cmd {
        Cmd::Create {
            dir,
            output,
            level,
            joliet,
            rock_ridge,
            volume_id,
            publisher,
            boot,
            udf,
        } => create(
            dir, output, level, joliet, rock_ridge, volume_id, publisher, boot, udf,
        ),
        Cmd::List { iso, long, names } => {
            for e in open(&iso)?.entries(names.into())? {
                if long {
                    println!(
                        "{} {:>12} {} {}",
                        e.mode_string(),
                        e.size,
                        e.date_string(),
                        e.path
                    );
                } else {
                    println!("{}", e.path);
                }
            }
            Ok(())
        }
        Cmd::Info { iso } => info(&iso),
        Cmd::Extract {
            iso,
            paths,
            output,
            names,
        } => {
            let n = open(&iso)?.extract(names.into(), &output, &paths)?;
            println!("estratte {n} voci in {}", output.display());
            Ok(())
        }
        Cmd::Verify { iso } => {
            let problems = open(&iso)?.verify()?;
            if problems.is_empty() {
                println!("ok: nessun problema trovato");
                return Ok(());
            }
            for p in &problems {
                println!("PROBLEMA: {p}");
            }
            Err(format!("{} problemi trovati", problems.len()).into())
        }
        Cmd::Edit {
            iso,
            output,
            add,
            remove,
            rename,
            names,
            level,
            joliet,
            rock_ridge,
            no_boot,
            boot,
        } => edit(
            iso, output, add, remove, rename, names, level, joliet, rock_ridge, no_boot, boot,
        ),
        Cmd::BootInfo { iso } => {
            let mut img = open(&iso)?;
            let v = img.boot_entries()?;
            if v.is_empty() {
                println!("nessun boot El Torito");
            }
            for (i, b) in v.iter().enumerate() {
                println!(
                    "{}: {} {} {}, caricamento {} settori da 512 B a 0x{:X}, LBA {}{}{}",
                    i + 1,
                    b.platform_name(),
                    b.media_name(),
                    if b.bootable {
                        "avviabile"
                    } else {
                        "non avviabile"
                    },
                    b.sector_count,
                    if b.load_segment == 0 {
                        0x7C0
                    } else {
                        b.load_segment as u32
                    } * 16,
                    b.rba,
                    b.path
                        .as_deref()
                        .map_or(String::new(), |p| format!(", file {p}")),
                    if b.info_table {
                        ", boot-info-table"
                    } else {
                        ""
                    },
                );
            }
            Ok(())
        }
        Cmd::BootExtract { iso, index, output } => {
            let mut img = open(&iso)?;
            let v = img.boot_entries()?;
            let b = index.checked_sub(1).and_then(|i| v.get(i)).ok_or_else(|| {
                format!("voce di boot {index} inesistente ({} presenti)", v.len())
            })?;
            img.read_boot_image(b, &mut File::create(&output)?)?;
            println!("estratta in {}", output.display());
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .init();
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("errore: {e}");
            ExitCode::FAILURE
        }
    }
}
