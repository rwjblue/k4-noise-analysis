//! Shard naming, discovery and metadata. Discovery only descends into
//! `YYYY/MM/DD` directories under the data root and only recognizes file
//! names this application generates; everything else is ignored.

use std::path::{Path, PathBuf};

use k4na_core::time::{NS_PER_SEC, civil_from_days};
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::StoreError;
use crate::schema::{FORMAT, SCHEMA_VERSION};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ShardKind {
    Summary,
    Raw,
}

impl ShardKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::Raw => "raw",
        }
    }
}

/// Parsed shard identity: `<kind>-YYYYMMDDTHHMMZ-<session8>.sqlite`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ShardName {
    pub period_start_utc_ns: i64,
    pub kind: ShardKind,
    pub session_tag: String,
}

impl ShardName {
    pub fn file_name(&self) -> String {
        let (y, mo, d, h, mi) = civil(self.period_start_utc_ns);
        format!(
            "{}-{y:04}{mo:02}{d:02}T{h:02}{mi:02}Z-{}.sqlite",
            self.kind.as_str(),
            self.session_tag
        )
    }

    /// Path relative to the data root: `YYYY/MM/DD/<file>`.
    pub fn relative_path(&self) -> PathBuf {
        let (y, mo, d, _, _) = civil(self.period_start_utc_ns);
        PathBuf::from(format!("{y:04}"))
            .join(format!("{mo:02}"))
            .join(format!("{d:02}"))
            .join(self.file_name())
    }

    pub fn parse(file_name: &str) -> Option<Self> {
        let stem = file_name.strip_suffix(".sqlite")?;
        let (kind, rest) = match stem.strip_prefix("summary-") {
            Some(r) => (ShardKind::Summary, r),
            None => (ShardKind::Raw, stem.strip_prefix("raw-")?),
        };
        let (stamp, tag) = rest.split_once('-')?;
        if tag.len() != 8 || !tag.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()) {
            return None;
        }
        let b = stamp.as_bytes();
        if b.len() != 14 || b[8] != b'T' || b[13] != b'Z' {
            return None;
        }
        let digits = |r: std::ops::Range<usize>| -> Option<i64> {
            let s = stamp.get(r)?;
            s.bytes().all(|c| c.is_ascii_digit()).then(|| s.parse().ok()).flatten()
        };
        let (y, mo, d, h, mi) = (
            digits(0..4)?,
            digits(4..6)?,
            digits(6..8)?,
            digits(9..11)?,
            digits(11..13)?,
        );
        if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 {
            return None;
        }
        let days = days_from_civil(y, mo as u32, d as u32);
        let period_start_utc_ns = ((days * 86_400) + h * 3600 + mi * 60) * NS_PER_SEC;
        let name = Self {
            period_start_utc_ns,
            kind,
            session_tag: tag.to_owned(),
        };
        // Reject non-canonical dates such as Feb 30.
        (name.file_name() == file_name).then_some(name)
    }
}

fn civil(utc_ns: i64) -> (i64, u32, u32, i64, i64) {
    let secs = utc_ns.div_euclid(NS_PER_SEC);
    let (y, mo, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    (y, mo, d, sod / 3600, (sod / 60) % 60)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// SQLite side files that count toward the byte budget.
pub const SIDE_SUFFIXES: [&str; 3] = ["-wal", "-shm", "-journal"];

/// Total bytes of a shard file plus any side files.
pub fn shard_bytes(path: &Path) -> u64 {
    let mut total = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    for s in SIDE_SUFFIXES {
        let mut p = path.as_os_str().to_owned();
        p.push(s);
        total += std::fs::metadata(PathBuf::from(p)).map(|m| m.len()).unwrap_or(0);
    }
    total
}

#[derive(Debug, Clone)]
pub struct ShardFile {
    pub name: ShardName,
    pub path: PathBuf,
    pub bytes: u64,
}

/// Lists shard files under `root` without following symlinks and without
/// descending anywhere but numeric `YYYY/MM/DD` directories.
pub fn discover(root: &Path) -> Result<Vec<ShardFile>, StoreError> {
    let mut out = Vec::new();
    for (year, _) in numeric_dirs(root, 4)? {
        for (month, _) in numeric_dirs(&year, 2)? {
            for (day, _) in numeric_dirs(&month, 2)? {
                for entry in std::fs::read_dir(&day)? {
                    let entry = entry?;
                    if !entry.file_type()?.is_file() {
                        continue;
                    }
                    let Some(file) = entry.file_name().to_str().map(str::to_owned) else {
                        continue;
                    };
                    if let Some(name) = ShardName::parse(&file) {
                        let path = entry.path();
                        out.push(ShardFile {
                            bytes: shard_bytes(&path),
                            name,
                            path,
                        });
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

fn numeric_dirs(dir: &Path, width: usize) -> Result<Vec<(PathBuf, u32)>, StoreError> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue; // also skips symlinks
        }
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if name.len() == width && name.bytes().all(|b| b.is_ascii_digit()) {
            out.push((entry.path(), name.parse().unwrap_or(0)));
        }
    }
    out.sort();
    Ok(out)
}

/// Metadata read from a shard's `meta` table.
#[derive(Debug, Clone, Default)]
pub struct ShardMeta {
    pub format: Option<String>,
    pub schema_version: Option<u32>,
    pub kind: Option<String>,
    pub finalized_utc_ns: Option<i64>,
    pub recovered: bool,
}

impl ShardMeta {
    pub fn is_ours(&self, kind: ShardKind) -> bool {
        self.format.as_deref() == Some(FORMAT) && self.kind.as_deref() == Some(kind.as_str())
    }
}

pub fn read_meta(conn: &Connection) -> Result<ShardMeta, StoreError> {
    let get = |k: &str| -> Result<Option<String>, StoreError> {
        Ok(conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [k], |r| r.get(0))
            .optional()?)
    };
    Ok(ShardMeta {
        format: get("format")?,
        schema_version: get("schema_version")?.and_then(|v| v.parse().ok()),
        kind: get("shard_kind")?,
        finalized_utc_ns: get("finalized_utc_ns")?.and_then(|v| v.parse().ok()),
        recovered: get("recovered")?.as_deref() == Some("1"),
    })
}

/// Opens a shard read-only and checks format and schema version.
pub fn open_read_only(path: &Path) -> Result<(Connection, ShardMeta), StoreError> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let meta = read_meta(&conn).map_err(|_| StoreError::NotAShard(path.to_owned()))?;
    if meta.format.as_deref() != Some(FORMAT) {
        return Err(StoreError::NotAShard(path.to_owned()));
    }
    match meta.schema_version {
        Some(SCHEMA_VERSION) => Ok((conn, meta)),
        other => Err(StoreError::UnsupportedSchema {
            path: path.to_owned(),
            found: other,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        let n = ShardName {
            period_start_utc_ns: 1_792_634_400 * NS_PER_SEC,
            kind: ShardKind::Summary,
            session_tag: "0123abcd".into(),
        };
        let file = n.file_name();
        assert_eq!(ShardName::parse(&file), Some(n.clone()));
        assert!(n.relative_path().starts_with("2026"));
        for bad in [
            "summary-20260230T0000Z-0123abcd.sqlite",
            "summary-20261027T18Z-0123abcd.sqlite",
            "notes-20261027T1800Z-0123abcd.sqlite",
            "summary-20261027T1800Z-0123ABCD.sqlite",
            "summary-20261027T1800Z-0123abcd.sqlite-wal",
            "summary-20261027T1800Z-0123abcd.db",
        ] {
            assert_eq!(ShardName::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn days_round_trip() {
        for days in [-1000, 0, 1, 20_000, 20_753] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }
}
