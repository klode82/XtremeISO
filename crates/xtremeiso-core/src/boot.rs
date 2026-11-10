//! El Torito (catalogo di boot, boot-info-table) e MBR ibrido per l'avvio da USB.

use crate::{Error, Result, SS};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Bios,
    Ppc,
    Mac,
    Efi,
}

impl Platform {
    pub fn id(self) -> u8 {
        match self {
            Platform::Bios => 0,
            Platform::Ppc => 1,
            Platform::Mac => 2,
            Platform::Efi => 0xEF,
        }
    }

    pub fn from_id(id: u8) -> Option<Self> {
        Some(match id {
            0 => Platform::Bios,
            1 => Platform::Ppc,
            2 => Platform::Mac,
            0xEF => Platform::Efi,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Emulation {
    /// L'immagine viene caricata e avviata così com'è (isolinux, GRUB, EFI).
    None,
    /// Floppy: il tipo (1.2/1.44/2.88 MB) si deduce dalla dimensione del file.
    Floppy,
    HardDisk,
}

#[derive(Clone, Debug)]
pub struct BootEntry {
    /// Percorso del file nell'immagine, es. "/isolinux/isolinux.bin".
    pub path: String,
    pub platform: Platform,
    pub emulation: Emulation,
    pub bootable: bool,
    pub load_segment: u16,
    /// Settori da 512 byte caricati. Default: 4 (BIOS), dimensione del file (EFI), 1 (emulazione).
    pub load_size: Option<u16>,
    /// Scrive la boot-info-table nei byte 8..64 (richiesta da isolinux).
    pub info_table: bool,
}

impl BootEntry {
    pub fn bios(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            platform: Platform::Bios,
            emulation: Emulation::None,
            bootable: true,
            load_segment: 0,
            load_size: None,
            info_table: false,
        }
    }

    pub fn efi(path: impl Into<String>) -> Self {
        Self {
            platform: Platform::Efi,
            ..Self::bios(path)
        }
    }
}

/// Voce del catalogo già risolta (indirizzi noti).
pub(crate) struct Slot {
    pub platform: u8,
    pub media: u8,
    pub bootable: bool,
    pub seg: u16,
    pub sys: u8,
    pub count: u16,
    pub rba: u32,
    pub size: u64,
}

/// Catalogo di boot (un settore): validation entry, voce iniziale, poi una sezione per ogni voce successiva.
pub(crate) fn catalog(slots: &[Slot]) -> Result<Vec<u8>> {
    if slots.is_empty() || slots.len() * 64 > SS {
        return Err(Error::Unsupported(
            "numero di voci di boot non valido (1..32)".into(),
        ));
    }
    let mut c = vec![0u8; SS];
    c[0] = 1;
    c[1] = slots[0].platform;
    c[4..13].copy_from_slice(b"XTREMEISO");
    c[30] = 0x55;
    c[31] = 0xAA;
    let sum = c[..32].chunks(2).fold(0u16, |a, w| {
        a.wrapping_add(u16::from_le_bytes([w[0], w[1]]))
    });
    c[28..30].copy_from_slice(&0u16.wrapping_sub(sum).to_le_bytes());
    let mut off = 32;
    for (i, s) in slots.iter().enumerate() {
        if i > 0 {
            c[off] = if i == slots.len() - 1 { 0x91 } else { 0x90 };
            c[off + 1] = s.platform;
            c[off + 2..off + 4].copy_from_slice(&1u16.to_le_bytes());
            off += 32;
        }
        c[off] = if s.bootable { 0x88 } else { 0 };
        c[off + 1] = s.media;
        c[off + 2..off + 4].copy_from_slice(&s.seg.to_le_bytes());
        c[off + 4] = s.sys;
        c[off + 6..off + 8].copy_from_slice(&s.count.to_le_bytes());
        c[off + 8..off + 12].copy_from_slice(&s.rba.to_le_bytes());
        off += 32;
    }
    Ok(c)
}

/// Boot-info-table di isolinux: LBA del PVD, LBA e lunghezza del file, checksum dei dword da 64 in poi.
pub(crate) fn patch_info_table(buf: &mut [u8], pvd_lba: u32, lba: u32) -> Result<()> {
    if buf.len() < 64 {
        return Err(Error::Unsupported(
            "immagine di boot sotto i 64 byte: niente boot-info-table".into(),
        ));
    }
    let sum = buf[64..].chunks(4).fold(0u32, |a, c| {
        let mut w = [0u8; 4];
        w[..c.len()].copy_from_slice(c);
        a.wrapping_add(u32::from_le_bytes(w))
    });
    buf[8..12].copy_from_slice(&pvd_lba.to_le_bytes());
    buf[12..16].copy_from_slice(&lba.to_le_bytes());
    let len = buf.len() as u32;
    buf[16..20].copy_from_slice(&len.to_le_bytes());
    buf[20..24].copy_from_slice(&sum.to_le_bytes());
    buf[24..64].fill(0);
    Ok(())
}

fn chs(lba: u64) -> [u8; 3] {
    // geometria 64 testine x 32 settori, come isohybrid/xorriso
    let (c, h, s) = (lba / 2048, (lba / 32) % 64, lba % 32 + 1);
    if c > 1023 {
        return [0xFE, 0xFF, 0xFF];
    }
    [h as u8, (s as u8 & 0x3F) | ((c >> 8) as u8) << 6, c as u8]
}

fn part(e: &mut [u8], active: bool, ty: u8, start: u64, len: u64) {
    let len = len.max(1);
    e[0] = if active { 0x80 } else { 0 };
    e[1..4].copy_from_slice(&chs(start));
    e[4] = ty;
    e[5..8].copy_from_slice(&chs(start + len - 1));
    e[8..12].copy_from_slice(&(start.min(u32::MAX as u64) as u32).to_le_bytes());
    e[12..16].copy_from_slice(&(len.min(u32::MAX as u64) as u32).to_le_bytes());
}

/// Primo settore ibrido: codice MBR dell'utente (max 440 byte) + partizione 1 = tutta l'ISO,
/// partizione 2 (tipo 0xEF) = immagine EFI. Se il codice è ≤ 432 byte (stile isohdpfx.bin) negli
/// 8 byte 432..440 va l'LBA, in settori da 512, della prima immagine di boot BIOS.
pub(crate) fn hybrid_mbr(
    code: &[u8],
    total: u32,
    bios_rba: Option<u32>,
    efi: Option<(u32, u64)>,
) -> Result<Vec<u8>> {
    if code.len() > 440 {
        return Err(Error::Unsupported(
            "il codice MBR non può superare 440 byte".into(),
        ));
    }
    let mut m = vec![0u8; 512];
    m[..code.len()].copy_from_slice(code);
    if let (Some(l), true) = (bios_rba, code.len() <= 432) {
        m[432..440].copy_from_slice(&(l as u64 * 4).to_le_bytes());
    }
    part(&mut m[446..462], true, 0x83, 0, total as u64 * 4);
    if let Some((rba, size)) = efi {
        part(
            &mut m[462..478],
            false,
            0xEF,
            rba as u64 * 4,
            size.div_ceil(512),
        );
    }
    m[510] = 0x55;
    m[511] = 0xAA;
    Ok(m)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_checksum_and_sections() {
        let s = |p, r| Slot {
            platform: p,
            media: 0,
            bootable: true,
            seg: 0,
            sys: 0,
            count: 4,
            rba: r,
            size: 0,
        };
        let c = catalog(&[s(0, 30), s(0xEF, 50), s(0xEF, 60)]).unwrap();
        assert_eq!(
            c[..32].chunks(2).fold(0u16, |a, w| a
                .wrapping_add(u16::from_le_bytes([w[0], w[1]]))),
            0
        );
        assert_eq!((c[30], c[31]), (0x55, 0xAA));
        assert_eq!(c[32], 0x88);
        assert_eq!(c[64], 0x90);
        assert_eq!(c[96], 0x88);
        assert_eq!(c[128], 0x91);
    }

    #[test]
    fn info_table_fields() {
        let mut b = vec![0xAB; 100];
        patch_info_table(&mut b, 16, 77).unwrap();
        assert_eq!(u32::from_le_bytes(b[12..16].try_into().unwrap()), 77);
        assert_eq!(u32::from_le_bytes(b[16..20].try_into().unwrap()), 100);
        assert!(b[24..64].iter().all(|&x| x == 0));
        assert!(patch_info_table(&mut [0; 10], 16, 1).is_err());
    }

    #[test]
    fn mbr_layout() {
        let m = hybrid_mbr(&[0xEB, 0xFE], 1000, Some(25), Some((40, 4096))).unwrap();
        assert_eq!((m[510], m[511]), (0x55, 0xAA));
        assert_eq!(u64::from_le_bytes(m[432..440].try_into().unwrap()), 100);
        assert_eq!((m[446], m[450]), (0x80, 0x83));
        assert_eq!(u32::from_le_bytes(m[458..462].try_into().unwrap()), 4000);
        assert_eq!(
            (m[466], u32::from_le_bytes(m[470..474].try_into().unwrap())),
            (0xEF, 160)
        );
        assert!(hybrid_mbr(&[0; 441], 1, None, None).is_err());
    }
}
