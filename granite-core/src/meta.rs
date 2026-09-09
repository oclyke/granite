//! `scan.meta`: volatile provenance of the last scan. Informational only.

use std::fs;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::fsutil::write_atomic;
use crate::header::Header;
use crate::index::PRODUCER;

pub const META_VERSION: u64 = 0;

#[derive(Debug, Clone)]
pub struct ScanMeta {
    pub started_at: String,
    pub finished_at: String,
    pub complete: bool,
    pub files_indexed: u64,
    pub bytes_indexed: u64,
    pub files_skipped: u64,
    pub index_fingerprint: String,
}

impl ScanMeta {
    pub fn write(&self, path: &Path) -> io::Result<()> {
        let mut h = Header::default();
        h.push("granite_meta_version", META_VERSION.to_string());
        h.push("producer", PRODUCER);
        h.push("started_at", &self.started_at);
        h.push("finished_at", &self.finished_at);
        h.push("complete", self.complete.to_string());
        h.push("files_indexed", self.files_indexed.to_string());
        h.push("bytes_indexed", self.bytes_indexed.to_string());
        h.push("files_skipped", self.files_skipped.to_string());
        h.push("index_fingerprint", &self.index_fingerprint);
        let mut body = Vec::new();
        h.emit_body(&mut body);
        write_atomic(path, |w| io::Write::write_all(w, &body))
    }

    /// Best-effort: returns `None` for a missing or unparseable file.
    pub fn read(path: &Path) -> Option<ScanMeta> {
        let text = fs::read_to_string(path).ok()?;
        let h = Header::parse_body(&text).ok()?;
        let s = |k: &str| h.scalar(k).map(str::to_owned);
        let n = |k: &str| h.scalar(k)?.parse().ok();
        Some(ScanMeta {
            started_at: s("started_at")?,
            finished_at: s("finished_at")?,
            complete: h.scalar("complete")? == "true",
            files_indexed: n("files_indexed")?,
            bytes_indexed: n("bytes_indexed")?,
            files_skipped: n("files_skipped")?,
            index_fingerprint: s("index_fingerprint")?,
        })
    }
}

pub fn rfc3339_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

// Howard Hinnant's days-to-civil algorithm.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    #[test]
    fn civil_dates() {
        assert_eq!(super::civil_from_days(0), (1970, 1, 1));
        assert_eq!(super::civil_from_days(20_705), (2026, 9, 9));
    }
}
