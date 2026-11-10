# XtremeISO — ROADMAP

Software per creare, leggere, convertire e masterizzare immagini disco, scritto in Rust. Ispirato a PowerISO.

**Assunzioni (da confermare):** target iniziale Linux; CLI prima, GUI dopo; solo formati aperti/documentati.

Legenda: `[ ]` da fare · `[~]` in corso · `[x]` fatto

---

## Fase 0 — Fondamenta
- [x] Workspace Cargo: `xtremeiso-core` (libreria), `xtremeiso-cli` (binario)
- [x] Gestione errori (`thiserror`), logging (`tracing`)
- [x] CI (fmt, clippy, test)
- [~] Strategia di test: validare output con `xorriso`, `isoinfo`, `7z` (ok); `udfinfo` non installato, serve per Fase 3

## Fase 1 — Scrittura ISO9660 (MVP)
- [x] Builder: cartella locale → `.iso`
- [x] ISO9660 Level 1/2/3 (nomi 8.3 / 30 / lunghi, file > 4 GiB via multi-extent: testato solo il calcolo dei chunk, non con un file reale)
- [x] Estensione Joliet (nomi Unicode)
- [x] Estensione Rock Ridge (permessi POSIX, symlink, nomi 255 byte, area CE)
- [x] Label volume, publisher, preparer, data (API completa; CLI: vedi Fase 9)
- [x] CLI: `xtremeiso create <dir> -o out.iso [--joliet] [--rock-ridge] [--level N]`
- [x] Test di conformità con strumenti esterni (xorriso roundtrip + `diff -r`, isoinfo, 7z)
- [x] Gerarchia oltre 8 livelli: errore esplicito col percorso troppo profondo (limite ISO9660, non si implementa la relocation Rock Ridge; UDF non ha il limite)
- [x] Hard link copiati come file distinti; socket/FIFO/device saltati con avviso (scelta voluta)

## Fase 2 — Lettura e modifica
- [x] Parser ISO9660/Joliet/Rock Ridge (incluse le relocation CL/RE degli alberi profondi scritti da altri tool, aree CE, file multi-extent)
- [x] `list [-l]`, `info`, `extract [percorsi]` con scelta dei nomi (`--names auto|iso|joliet|rr`)
- [x] Estrazione sicura: mai scritture attraverso symlink, mai sovrascritture, nomi `..` o con `/` scartati, niente setuid/sticky
- [x] Modifica immagine esistente: `edit --add LOCALE[:/dest] --remove --rename vecchio=nuovo` (riscrive tutta l'immagine; sul posto via file temporaneo)
- [x] `verify`: dimensioni, endianness, extent nei limiti, path table L/M e coerenza con l'albero, confronto Joliet
- [x] Test con immagini di xorriso (albero a 11 livelli) e roundtrip con le nostre
- Limiti noti: `edit` non conserva boot/El Torito né le aree di sistema (Fase 3); nomi non UTF-8 diventano `U+FFFD`; mtime delle cartelle non ripristinato all'estrazione; dati oltre il volume dichiarato non rilevati

## Fase 3 — UDF e boot (in corso)
Strumenti di validazione disponibili: `udfinfo`/`mkudffs` (udftools 2.3), `genisoimage -udf`, `xorriso`, `7z`, `qemu-system-x86_64` (prova di avvio reale BIOS).
Ordine di lavoro: 3a → 3b → 3c → 3d.

**3a — El Torito e immagine ibrida USB** (completato)
- [x] El Torito: boot BIOS e UEFI, no-emulation / floppy / hard disk, boot-info-table (isolinux)
- [x] `create`/`edit`: `--boot`, `--boot-efi`, `--boot-info-table`, `--boot-load-size`, `--boot-emulation`, `--no-boot`
- [x] Lettura catalogo di boot: `boot-info`, `boot-extract`; `edit` conserva il boot e ricalcola la boot-info-table
- [x] Sostituzione immagine di boot (via `edit --remove/--add` + `--boot`)
- [x] Ibrido isohybrid: MBR fornito dall'utente (`--isohybrid-mbr`, conservato da `edit`) + tabella partizioni (ISO + partizione EFI 0xEF)
- [x] Test: xorriso (`-report_el_torito`), genisoimage, avvio reale con qemu (CD e disco USB, anche dopo `edit`)
- Limiti: niente MBR di isolinux incluso (va fornito: il file `isohdpfx.bin` non è distribuibile qui); niente GPT; il boot UEFI non è provato (manca OVMF); rinominare/spostare il file di boot con `edit` richiede di riindicarlo con `--boot`

**3b — Scrittura UDF**
- [ ] UDF 1.02, 1.50, 2.00, 2.01, 2.50, 2.60 (partizione tipo 1, senza metadata partition)
- [ ] Immagine bridge UDF/ISO9660 (dati condivisi) e UDF pura (`--no-iso9660`)
- [ ] AVDP a 256 e a fine volume, VDS principale e di riserva, LVID, File Set, File Entry, FID, symlink
- [ ] Nessun limite di profondità delle cartelle
- [ ] Test: `udfinfo`, `7z` (estrazione completa), confronto con `genisoimage -udf`

**3c — Lettura UDF**
- [ ] Parser UDF: VDS, partizione, File Set, File Entry (short/long/embedded), FID, symlink
- [ ] `list`/`info`/`extract`/`verify`/`edit` anche su immagini solo UDF (`--names udf`)
- [ ] Test: immagini di `genisoimage -udf` e `mkudffs`
- [ ] Partizione di metadati (UDF 2.50+ BD): da valutare

**3d — Chiusura**
- [ ] Documentazione dei limiti (un solo extent CE per record, max ~234 estensioni per file in UDF, ecc.)

## Fase 4 — Altri formati e conversione
- [ ] BIN/CUE (lettura/scrittura)
- [ ] IMG/raw
- [ ] NRG (Nero)
- [ ] MDF/MDS (Alcohol)
- [ ] Conversione tra formati: `xtremeiso convert in.X out.Y`
- [ ] Raw sector 2352 / modi CD (Mode1, Mode2, XA), tracce audio
- [ ] Fuori scope: DAA/UIF (proprietari), rivalutare su richiesta

## Fase 5 — Dischi fisici
- [ ] Rilevamento unità ottiche (Linux: `/dev/sr*`, ioctl SCSI/MMC)
- [ ] Rip CD/DVD/BD → immagine, con gestione errori di lettura e retry
- [ ] Masterizzazione immagine → disco (CD-R/RW, DVD±R/RW, BD-R/RE)
- [ ] Masterizzazione da file locali (on-the-fly)
- [ ] Verifica post-scrittura, cancellazione RW
- [ ] Progresso e velocità selezionabile

## Fase 5b — Windows / macOS (se confermato)
- [ ] Astrazione `trait` accesso al disco (SPTI su Windows, IOKit su macOS)
- [ ] Build e test multipiattaforma in CI

## Fase 6 — Funzionalità extra
- [ ] Checksum (MD5, SHA-1, SHA-256) di immagine e file
- [ ] Montaggio immagini (FUSE su Linux)
- [ ] Compressione immagine (formato aperto, es. zstd) e cifratura (age/AES)
- [ ] Split / merge immagini
- [ ] Edit del contenuto con anteprima albero
- [ ] Copia disco-a-disco
- [ ] Gestione "bootable USB" (scrittura immagine su chiavetta)

## Fase 7 — Interfaccia grafica
- [ ] Scelta toolkit (`egui` vs `iced`)
- [ ] Vista albero file, drag & drop, finestra proprietà ISO
- [ ] Wizard: crea / apri / converti / masterizza / rip
- [ ] Barra di progresso, log, annulla operazione

## Fase 8 — Rilascio
- [ ] Documentazione utente e API
- [ ] Packaging: `.deb`, AppImage, `cargo install`
- [ ] Benchmark e fuzzing dei parser (`cargo-fuzz`)
- [ ] Versione 1.0

## Fase 9 — Rifiniture
- [ ] CLI `create`: barra/percentuale di progresso su stderr durante la scrittura
- [ ] CLI `create`: flag per system id, volume set id, preparer, application (oggi solo `--volume-id` e `--publisher`)

---

## Rischi
- **UDF** è complesso: partizioni virtuali/sparable, descrittori numerosi. Fare dopo ISO9660 stabile.
- **Accesso al disco** dipende da OS e permessi; isolare dietro `trait`.
- **Formati proprietari** senza specifiche ufficiali: reverse engineering, solo se richiesto.
- **Compatibilità**: ogni immagine prodotta va validata con strumenti di terze parti.

## Decisioni aperte
1. Piattaforme target oltre Linux?
2. GUI subito o dopo il core? (consigliato: dopo)
3. Licenza del progetto.
