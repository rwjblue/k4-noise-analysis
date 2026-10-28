//! Operator annotations: timestamped notes such as "dehumidifier off".
//!
//! They live in `<root>/annotations.jsonl`, one JSON object per line,
//! appended and fsynced by `k4na mark`. They are operator input, not
//! measurements: never pruned, never written by the collector (so marking
//! is safe while a survey runs), and kept out of every shard. Readers skip
//! lines they cannot parse and report how many.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::StoreError;
use crate::writer::ROOT_MARKER;

pub const ANNOTATIONS_FILE: &str = "annotations.jsonl";
/// Longest accepted note, in characters.
pub const MAX_TEXT_CHARS: usize = 500;

/// A note about a moment (`end_utc_ns` unset) or an interval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Annotation {
    pub start_utc_ns: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_utc_ns: Option<i64>,
    pub text: String,
    /// When it was written; later than `start_utc_ns` for a note made
    /// after the fact.
    pub created_utc_ns: i64,
}

impl Annotation {
    /// Whether it touches `[lo, hi]`.
    pub fn overlaps(&self, lo: i64, hi: i64) -> bool {
        self.start_utc_ns <= hi && self.end_utc_ns.unwrap_or(self.start_utc_ns) >= lo
    }
}

pub fn path(root: &Path) -> PathBuf {
    root.join(ANNOTATIONS_FILE)
}

/// Appends one annotation to an existing k4na data root.
pub fn append(root: &Path, a: &Annotation) -> Result<(), StoreError> {
    if !root.join(ROOT_MARKER).is_file() {
        return Err(StoreError::BadRoot(format!(
            "{} is not a k4na data root (no {ROOT_MARKER}); annotations are only written beside measurements",
            root.display()
        )));
    }
    let text = a.text.trim();
    if text.is_empty() || text.chars().count() > MAX_TEXT_CHARS {
        return Err(StoreError::BadRoot(format!(
            "an annotation needs 1 to {MAX_TEXT_CHARS} characters of text"
        )));
    }
    if a.end_utc_ns.is_some_and(|e| e < a.start_utc_ns) {
        return Err(StoreError::BadRoot("an annotation cannot end before it starts".into()));
    }
    let a = Annotation {
        text: text.to_owned(),
        ..a.clone()
    };
    let mut line = serde_json::to_string(&a)?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path(root))?;
    // One write per line, so a concurrent `k4na mark` cannot interleave.
    f.write_all(line.as_bytes())?;
    f.sync_all()?;
    Ok(())
}

/// Every readable annotation, by start time, and the count of lines
/// skipped as unreadable. A missing file is no annotations.
pub fn read(root: &Path) -> Result<(Vec<Annotation>, u64), StoreError> {
    let text = match std::fs::read_to_string(path(root)) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), 0)),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    let mut bad = 0;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<Annotation>(line) {
            Ok(a) => out.push(a),
            Err(_) => bad += 1,
        }
    }
    out.sort_by_key(|a| (a.start_utc_ns, a.created_utc_ns));
    Ok((out, bad))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(start: i64, end: Option<i64>, text: &str) -> Annotation {
        Annotation {
            start_utc_ns: start,
            end_utc_ns: end,
            text: text.into(),
            created_utc_ns: 100,
        }
    }

    #[test]
    fn appends_reads_back_and_skips_damaged_lines() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        assert!(append(root, &note(5, None, "x")).is_err(), "needs a data root");
        std::fs::write(root.join(ROOT_MARKER), "").unwrap();
        assert_eq!(read(root).unwrap(), (vec![], 0));
        append(root, &note(20, Some(30), "  dehumidifier off  ")).unwrap();
        append(root, &note(10, None, "furnace \"on\"\nsecond line")).unwrap();
        assert!(append(root, &note(10, Some(5), "backwards")).is_err());
        assert!(append(root, &note(10, None, "   ")).is_err());
        assert!(append(root, &note(10, None, &"x".repeat(MAX_TEXT_CHARS + 1))).is_err());
        // A torn last line, as after a crash mid-write.
        let mut f = std::fs::OpenOptions::new().append(true).open(path(root)).unwrap();
        f.write_all(b"{\"start_utc_ns\":4").unwrap();
        let (notes, bad) = read(root).unwrap();
        assert_eq!(bad, 1);
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[0].text, "furnace \"on\"\nsecond line");
        assert_eq!(notes[1].text, "dehumidifier off");
        assert!(notes[1].overlaps(25, 40) && notes[1].overlaps(0, 20) && !notes[1].overlaps(31, 40));
        assert!(notes[0].overlaps(10, 10) && !notes[0].overlaps(11, 20));
    }
}
