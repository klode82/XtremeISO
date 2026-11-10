//! XtremeISO core: creazione, lettura e conversione di immagini disco.

mod boot;
mod edit;
mod names;
mod reader;
mod rr;
mod tree;
mod udf;
mod writer;

pub use boot::{BootEntry, Emulation, Platform};
pub use edit::Editor;
pub use reader::{BootInfo, Entry, Image, Info, Names};
pub use tree::Kind;
pub use udf::UdfRevision;
pub use writer::{create_iso, IsoOptions, Level};

use std::{io, path::Path};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O: {0}")]
    Io(#[from] io::Error),
    #[error("{0}: {1}")]
    Path(String, io::Error),
    #[error("non supportato: {0}")]
    Unsupported(String),
    #[error("immagine non valida: {0}")]
    Invalid(String),
    #[error("errore interno: {0}")]
    Internal(String),
}

impl Error {
    pub(crate) fn path(p: &Path, e: io::Error) -> Self {
        Error::Path(p.display().to_string(), e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Dimensione settore logico ISO9660.
pub(crate) const SS: usize = 2048;

/// u32 in formato "both-endian" (LE poi BE), 8 byte.
pub(crate) fn b32(v: u32) -> [u8; 8] {
    let mut o = [0; 8];
    o[..4].copy_from_slice(&v.to_le_bytes());
    o[4..].copy_from_slice(&v.to_be_bytes());
    o
}

/// u16 in formato "both-endian", 4 byte.
pub(crate) fn b16(v: u16) -> [u8; 4] {
    let mut o = [0; 4];
    o[..2].copy_from_slice(&v.to_le_bytes());
    o[2..].copy_from_slice(&v.to_be_bytes());
    o
}

/// Secondi Unix (UTC) -> (anno, mese, giorno, ore, minuti, secondi).
pub(crate) fn civil(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z.rem_euclid(146097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + (m <= 2) as i64;
    (
        y,
        m,
        d,
        (rem / 3600) as u32,
        (rem % 3600 / 60) as u32,
        (rem % 60) as u32,
    )
}

/// Timestamp a 7 byte dei directory record (fuso UTC).
pub(crate) fn rec_time(secs: i64) -> [u8; 7] {
    let (y, m, d, h, mi, s) = civil(secs);
    [
        (y - 1900).clamp(0, 255) as u8,
        m as u8,
        d as u8,
        h as u8,
        mi as u8,
        s as u8,
        0,
    ]
}

/// (anno, mese, giorno) -> giorni dal 1970-01-01.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let (era, yoe) = (y.div_euclid(400), y.rem_euclid(400));
    let doy = (153 * ((m as i64 + 9) % 12) + 2) / 5 + d as i64 - 1;
    era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719468
}

/// Inverso di `rec_time` (tiene conto del fuso in quarti d'ora).
pub(crate) fn parse_rec_time(b: &[u8]) -> i64 {
    if b.len() < 7 || b[1] == 0 || b[1] > 12 || b[2] == 0 {
        return 0;
    }
    let days = days_from_civil(1900 + b[0] as i64, b[1] as u32, b[2] as u32);
    days * 86400 + b[3] as i64 * 3600 + b[4] as i64 * 60 + b[5] as i64 - (b[6] as i8) as i64 * 900
}

/// "YYYY-MM-DD HH:MM" (UTC)
pub fn format_unix_time(secs: i64) -> String {
    let (y, m, d, h, mi, _) = civil(secs);
    format!("{y:04}-{m:02}-{d:02} {h:02}:{mi:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil(1_700_000_000), (2023, 11, 14, 22, 13, 20));
        assert_eq!(civil(-1), (1969, 12, 31, 23, 59, 59));
        for t in [0, 1_700_000_000, 951_782_400, 1_234_567_890] {
            assert_eq!(parse_rec_time(&rec_time(t)), t);
        }
    }
}
