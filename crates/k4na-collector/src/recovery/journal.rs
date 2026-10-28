//! Durable, append-only journal files: `<dir>/<session_id>.jsonl`.
//!
//! Each line is `<check> <json>\n`, where `check` is the first 16 hex
//! digits of SHA-256 over the JSON text. Every append is fsynced before
//! the call returns, so an intent is on disk before its setters are sent.
//! A torn final line (crash mid-write) is detected and dropped; a bad line
//! followed by good ones means corruption, and automatic recovery refuses.
//! A running survey holds an exclusive OS lock on its journal, so a
//! recovery cannot race it and a crashed process never leaves a stale lock.
//!
//! Journals live outside the measurement quotas, so they must stay small on
//! their own: past [`COMPACT_BYTES`] the journal is rewritten as the minimal
//! entry sequence that replays to the same ledger (verified before use),
//! written to a locked side file, fsynced and renamed over the original.
//! A crash at any point leaves either the old or the new journal, both
//! complete.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use k4na_core::time::Clock;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Change, Entry, Ledger, Outcome, Purpose};

/// Journal size that triggers compaction. A compacted journal holds one
/// capture per visited scope and at most [`super::OURS_KEPT`] values per
/// field, so it stays far below this.
pub const COMPACT_BYTES: u64 = 256 * 1024;

/// Destination for journal lines. `append` must be durable when it
/// returns `Ok`.
pub trait JournalSink: Send + std::fmt::Debug {
    fn append(&mut self, line: &[u8]) -> io::Result<()>;
}

#[derive(Debug)]
struct FileSink {
    file: File,
}

impl JournalSink for FileSink {
    fn append(&mut self, line: &[u8]) -> io::Result<()> {
        self.file.write_all(line)?;
        self.file.sync_data()
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Record {
    seq: u64,
    utc_ns: i64,
    #[serde(flatten)]
    entry: Entry,
}

fn check(json: &str) -> String {
    hex::encode(&Sha256::digest(json.as_bytes())[..8])
}

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("recovery journal I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("recovery journal {path} is corrupt at line {line}; inspect it manually")]
    Corrupt { path: PathBuf, line: usize },
    #[error("recovery journal is inconsistent: {0}")]
    Invalid(String),
    #[error("{0} is in use by a running survey")]
    Locked(PathBuf),
    #[error("{}", pending_message(.0))]
    Pending(Vec<JournalInfo>),
}

fn pending_message(p: &[JournalInfo]) -> String {
    let list: Vec<String> = p
        .iter()
        .map(|j| format!("{} ({})", j.session_id, j.status.as_str()))
        .collect();
    format!(
        "unresolved survey recovery journal(s): {}. Resolve with `k4na recovery show/restore`, or accept the \
         radio's current state with `k4na recovery acknowledge <session>`",
        list.join(", ")
    )
}

/// What a journal needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalStatus {
    /// A survey holds it open right now.
    Running,
    /// Restoration completed and verified.
    Resolved,
    /// The operator accepted the radio's state.
    Acknowledged,
    /// Changes may still be in the radio.
    Pending,
    /// Unreadable; inspect manually.
    Corrupt,
}

impl JournalStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            JournalStatus::Running => "running",
            JournalStatus::Resolved => "resolved",
            JournalStatus::Acknowledged => "acknowledged",
            JournalStatus::Pending => "pending",
            JournalStatus::Corrupt => "corrupt",
        }
    }

    pub fn blocks_surveys(self) -> bool {
        matches!(
            self,
            JournalStatus::Running | JournalStatus::Pending | JournalStatus::Corrupt
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct JournalInfo {
    pub path: PathBuf,
    pub session_id: String,
    pub profile: Option<String>,
    pub started_utc_ns: Option<i64>,
    pub status: JournalStatus,
    pub outcome: Option<Outcome>,
    pub message: Option<String>,
    /// Changed settings not known to be back.
    pub dirty: usize,
    pub torn_tail: bool,
}

struct Loaded {
    ledger: Ledger,
    begin: Option<Entry>,
    started_utc_ns: Option<i64>,
    next_seq: u64,
    valid_len: u64,
    torn_tail: bool,
}

fn parse(path: &Path, bytes: &[u8]) -> Result<Loaded, JournalError> {
    let mut l = Loaded {
        ledger: Ledger::default(),
        begin: None,
        started_utc_ns: None,
        next_seq: 0,
        valid_len: 0,
        torn_tail: false,
    };
    let mut offset = 0usize;
    let mut lines = bytes.split_inclusive(|b| *b == b'\n').enumerate().peekable();
    while let Some((i, line)) = lines.next() {
        let last = lines.peek().is_none();
        let rec = std::str::from_utf8(line)
            .ok()
            .and_then(|s| s.strip_suffix('\n'))
            .and_then(|s| s.split_once(' '))
            .filter(|(c, json)| *c == check(json))
            .and_then(|(_, json)| serde_json::from_str::<Record>(json).ok());
        let Some(rec) = rec else {
            if last {
                // Crash mid-append: the setter it announced was never sent.
                l.torn_tail = true;
                break;
            }
            return Err(JournalError::Corrupt {
                path: path.to_owned(),
                line: i + 1,
            });
        };
        if rec.seq != l.next_seq {
            return Err(JournalError::Corrupt {
                path: path.to_owned(),
                line: i + 1,
            });
        }
        l.ledger
            .apply(&rec.entry)
            .map_err(|e| JournalError::Invalid(format!("{} line {}: {e}", path.display(), i + 1)))?;
        if l.started_utc_ns.is_none() {
            l.started_utc_ns = Some(rec.utc_ns);
            l.begin = Some(rec.entry);
        }
        l.next_seq += 1;
        offset += line.len();
        l.valid_len = offset as u64;
    }
    Ok(l)
}

/// Reads a journal without locking or modifying it.
pub fn load(path: &Path) -> Result<Ledger, JournalError> {
    Ok(parse(path, &std::fs::read(path)?)?.ledger)
}

fn journal_paths(dir: &Path) -> Result<Vec<PathBuf>, JournalError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for e in entries {
        let e = e?;
        let p = e.path();
        if e.file_type()?.is_file() && p.extension().is_some_and(|x| x == "jsonl") {
            out.push(p);
        }
    }
    out.sort();
    Ok(out)
}

fn locked_elsewhere(path: &Path) -> bool {
    File::open(path).is_ok_and(|f| matches!(f.try_lock_shared(), Err(std::fs::TryLockError::WouldBlock)))
}

/// Summaries of every journal in `dir`, oldest first.
pub fn list(dir: &Path) -> Result<Vec<JournalInfo>, JournalError> {
    let mut out = Vec::new();
    for path in journal_paths(dir)? {
        let session_id = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let running = locked_elsewhere(&path);
        let info = match std::fs::read(&path)
            .map_err(JournalError::from)
            .and_then(|b| parse(&path, &b))
        {
            Ok(l) => JournalInfo {
                status: if running {
                    JournalStatus::Running
                } else if l.ledger.acknowledged {
                    JournalStatus::Acknowledged
                } else if l.ledger.resolved() {
                    JournalStatus::Resolved
                } else {
                    JournalStatus::Pending
                },
                session_id: if l.ledger.session_id.is_empty() {
                    session_id
                } else {
                    l.ledger.session_id.clone()
                },
                profile: l.ledger.profile.clone(),
                started_utc_ns: l.started_utc_ns,
                outcome: l.ledger.outcome,
                message: l.ledger.message.clone(),
                dirty: l.ledger.dirty().len(),
                torn_tail: l.torn_tail,
                path,
            },
            Err(e) => JournalInfo {
                path,
                session_id,
                profile: None,
                started_utc_ns: None,
                status: if running {
                    JournalStatus::Running
                } else {
                    JournalStatus::Corrupt
                },
                outcome: None,
                message: Some(e.to_string()),
                dirty: 0,
                torn_tail: false,
            },
        };
        out.push(info);
    }
    out.sort_by_key(|j| (j.started_utc_ns, j.session_id.clone()));
    Ok(out)
}

/// An open journal plus its replayed ledger. Every change to the ledger
/// goes through [`Transaction::record`], which is durable first.
#[derive(Debug)]
pub struct Transaction {
    sink: Box<dyn JournalSink>,
    ledger: Ledger,
    path: Option<PathBuf>,
    next_seq: u64,
    clock: Arc<dyn Clock>,
    /// Set after a failed append: nothing more may be changed.
    failed: bool,
    /// The begin entry and its time, carried into compacted journals.
    begin: Option<Entry>,
    started_utc_ns: Option<i64>,
    /// Bytes in the journal file and the size that triggers compaction.
    bytes: u64,
    compact_at: u64,
}

impl Transaction {
    /// Starts a survey journal in `dir`. Refuses while another journal in
    /// `dir` is unresolved, so changes from an interrupted survey are never
    /// buried under a new one.
    pub fn begin(dir: &Path, begin: Entry, clock: Arc<dyn Clock>) -> Result<Self, JournalError> {
        let Entry::Begin { session_id, .. } = &begin else {
            return Err(JournalError::Invalid("a journal must start with a begin entry".into()));
        };
        let pending: Vec<JournalInfo> = list(dir)?.into_iter().filter(|j| j.status.blocks_surveys()).collect();
        if !pending.is_empty() {
            return Err(JournalError::Pending(pending));
        }
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{session_id}.jsonl"));
        let file = OpenOptions::new().append(true).create_new(true).open(&path)?;
        file.try_lock().map_err(|_| JournalError::Locked(path.clone()))?;
        // Make the new directory entry durable too.
        File::open(dir)?.sync_all()?;
        let mut t = Self::with_sink(Box::new(FileSink { file }), clock);
        t.path = Some(path);
        t.record(begin)?;
        Ok(t)
    }

    /// A journal on an arbitrary sink (tests, failure injection).
    pub fn with_sink(sink: Box<dyn JournalSink>, clock: Arc<dyn Clock>) -> Self {
        Self {
            sink,
            ledger: Ledger::default(),
            path: None,
            next_seq: 0,
            clock,
            failed: false,
            begin: None,
            started_utc_ns: None,
            bytes: 0,
            compact_at: COMPACT_BYTES,
        }
    }

    /// Reopens an existing journal for recovery, holding its lock. A torn
    /// final line is truncated away before anything is appended.
    pub fn resume(path: &Path, clock: Arc<dyn Clock>) -> Result<Self, JournalError> {
        let mut file = OpenOptions::new().read(true).write(true).open(path)?;
        file.try_lock().map_err(|_| JournalError::Locked(path.to_owned()))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let loaded = parse(path, &bytes)?;
        if loaded.torn_tail {
            file.set_len(loaded.valid_len)?;
            file.sync_all()?;
        }
        file.seek(SeekFrom::End(0))?;
        Ok(Self {
            sink: Box::new(FileSink { file }),
            ledger: loaded.ledger,
            path: Some(path.to_owned()),
            next_seq: loaded.next_seq,
            clock,
            failed: false,
            begin: loaded.begin,
            started_utc_ns: loaded.started_utc_ns,
            bytes: loaded.valid_len,
            compact_at: loaded.valid_len + COMPACT_BYTES,
        })
    }

    pub fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Validates `e` against the ledger, makes it durable, then applies it.
    /// After a failed append every later call fails: the caller must not
    /// change the radio without a journal.
    pub fn record(&mut self, e: Entry) -> Result<(), JournalError> {
        if self.failed {
            return Err(JournalError::Invalid(
                "an earlier journal write failed; no further changes may be journaled".into(),
            ));
        }
        let mut next = self.ledger.clone();
        next.apply(&e).map_err(JournalError::Invalid)?;
        let utc_ns = self.clock.now().utc_ns;
        let begin = matches!(e, Entry::Begin { .. }).then(|| e.clone());
        let line = line(self.next_seq, utc_ns, e)?;
        if let Err(e) = self.sink.append(line.as_bytes()) {
            self.failed = true;
            return Err(e.into());
        }
        if begin.is_some() {
            self.begin = begin;
            self.started_utc_ns = Some(utc_ns);
        }
        self.next_seq += 1;
        self.bytes += line.len() as u64;
        self.ledger = next;
        if self.bytes >= self.compact_at {
            // The entry is already durable; a failed compaction before the
            // rename leaves the old journal in use and is retried later.
            let _ = self.compact();
            self.compact_at = self.bytes + COMPACT_BYTES;
        }
        Ok(())
    }

    /// Bytes in the journal file.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    /// The minimal entry sequence that replays to the current ledger, or
    /// `None` if this ledger cannot be reproduced exactly.
    fn compacted(&self) -> Option<Vec<Entry>> {
        let l = &self.ledger;
        let mut out = vec![self.begin.clone()?];
        for (scope, fields) in &l.obligations {
            out.push(Entry::Capture {
                scope: scope.clone(),
                values: fields.iter().map(|(f, ob)| (f.clone(), ob.original.clone())).collect(),
            });
            let changes: Vec<Change> = fields
                .iter()
                .flat_map(|(f, ob)| {
                    ob.ours.iter().map(move |v| Change {
                        scope: scope.clone(),
                        field: f.clone(),
                        from: Some(ob.original.clone()),
                        to: v.clone(),
                        wire: String::new(),
                    })
                })
                .collect();
            if !changes.is_empty() {
                out.push(Entry::Intent {
                    purpose: Purpose::Survey,
                    changes,
                });
            }
        }
        out.push(Entry::Verified {
            state: l.last_state.clone(),
        });
        if !l.in_flight.is_empty() {
            out.push(Entry::Intent {
                purpose: Purpose::Survey,
                changes: l.in_flight.clone(),
            });
        }
        for (scope, fields) in &l.obligations {
            let restored: Vec<String> = fields
                .iter()
                .filter(|(_, ob)| ob.restored)
                .map(|(f, _)| f.clone())
                .collect();
            if !restored.is_empty() {
                out.push(Entry::Restored {
                    scope: scope.clone(),
                    fields: restored,
                });
            }
        }
        if let Some(outcome) = l.outcome {
            out.push(Entry::End {
                outcome,
                message: l.message.clone().unwrap_or_default(),
                manual: Vec::new(),
            });
        }
        if l.acknowledged {
            out.push(Entry::Acknowledged {
                note: "carried over by compaction".into(),
            });
        }
        out.push(Entry::Note {
            message: format!("compacted from {} entries", self.next_seq),
        });
        let mut replay = Ledger::default();
        for e in &out {
            replay.apply(e).ok()?;
        }
        (replay == *l).then_some(out)
    }

    /// Rewrites a file-backed journal as [`Self::compacted`]. Returns
    /// whether it was replaced. An error after the rename means later
    /// entries might not be durable, so the transaction refuses them.
    fn compact(&mut self) -> Result<bool, JournalError> {
        let Some(path) = self.path.clone() else {
            return Ok(false);
        };
        let Some(entries) = self.compacted() else {
            return Ok(false);
        };
        let now = self.clock.now().utc_ns;
        let mut text = String::new();
        let n = entries.len() as u64;
        for (seq, e) in (0..).zip(entries) {
            let utc_ns = if seq == 0 {
                self.started_utc_ns.unwrap_or(now)
            } else {
                now
            };
            text.push_str(&line(seq, utc_ns, e)?);
        }
        // The rewrite must replay to the same ledger before it replaces
        // anything.
        let side = path.with_extension("jsonl.compact");
        if parse(&side, text.as_bytes()).map(|l| l.ledger).ok().as_ref() != Some(&self.ledger) {
            return Ok(false);
        }
        let mut file = OpenOptions::new().write(true).create(true).truncate(true).open(&side)?;
        // Locked before the rename, so the journal is never seen unlocked.
        file.try_lock().map_err(|_| JournalError::Locked(side.clone()))?;
        if let Err(e) = file.write_all(text.as_bytes()).and_then(|()| file.sync_all()) {
            let _ = std::fs::remove_file(&side);
            return Err(e.into());
        }
        std::fs::rename(&side, &path)?;
        // Appends must go to the new file from here on.
        self.sink = Box::new(FileSink { file });
        self.next_seq = n;
        self.bytes = text.len() as u64;
        if let Some(dir) = path.parent()
            && let Err(e) = File::open(dir).and_then(|d| d.sync_all())
        {
            self.failed = true;
            return Err(e.into());
        }
        Ok(true)
    }
}

fn line(seq: u64, utc_ns: i64, entry: Entry) -> Result<String, JournalError> {
    let rec = Record { seq, utc_ns, entry };
    let json = serde_json::to_string(&rec).map_err(|e| JournalError::Invalid(e.to_string()))?;
    Ok(format!("{} {json}\n", check(&json)))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use k4na_core::time::SystemClock;

    use super::*;
    use crate::measure::MetaMode;
    use crate::recovery::{Change, Purpose, Scope};

    fn begin(id: &str) -> Entry {
        Entry::Begin {
            session_id: id.into(),
            profile: Some("p".into()),
            program_version: "t".into(),
            meta_mode: MetaMode::Basic,
            original: BTreeMap::from([("BN".into(), "03".into())]),
        }
    }

    fn clock() -> Arc<dyn Clock> {
        Arc::new(SystemClock::default())
    }

    fn intent() -> Entry {
        Entry::Intent {
            purpose: Purpose::Survey,
            changes: vec![Change {
                scope: Scope::Session,
                field: "BN".into(),
                from: Some("03".into()),
                to: "05".into(),
                wire: "BN05;".into(),
            }],
        }
    }

    #[test]
    fn round_trip_status_and_pending_gate() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = Transaction::begin(dir.path(), begin("a"), clock()).unwrap();
        t.record(intent()).unwrap();
        let path = t.path().unwrap().to_owned();
        assert_eq!(list(dir.path()).unwrap()[0].status, JournalStatus::Running);
        assert!(matches!(
            Transaction::resume(&path, clock()),
            Err(JournalError::Locked(_))
        ));
        drop(t); // crash: the lock goes with the process
        let info = &list(dir.path()).unwrap()[0];
        assert_eq!((info.status, info.dirty), (JournalStatus::Pending, 1));
        let err = Transaction::begin(dir.path(), begin("b"), clock()).unwrap_err();
        assert!(
            err.to_string()
                .contains("unresolved survey recovery journal(s): a (pending)"),
            "{err}"
        );

        let mut r = Transaction::resume(&path, clock()).unwrap();
        assert!(r.ledger().is_dirty());
        r.record(Entry::Acknowledged { note: "ok".into() }).unwrap();
        drop(r);
        assert_eq!(list(dir.path()).unwrap()[0].status, JournalStatus::Acknowledged);
        Transaction::begin(dir.path(), begin("b"), clock()).unwrap();
    }

    #[test]
    fn torn_tail_is_dropped_and_truncated_but_corruption_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = Transaction::begin(dir.path(), begin("a"), clock()).unwrap();
        t.record(intent()).unwrap();
        let path = t.path().unwrap().to_owned();
        drop(t);
        let good = std::fs::read(&path).unwrap();
        let mut torn = good.clone();
        torn.extend_from_slice(b"0123456789abcdef {\"seq\":2,\"utc_n");
        std::fs::write(&path, &torn).unwrap();
        let info = &list(dir.path()).unwrap()[0];
        assert!(info.torn_tail);
        assert_eq!(info.status, JournalStatus::Pending);
        let mut r = Transaction::resume(&path, clock()).unwrap();
        r.record(Entry::Note {
            message: "after".into(),
        })
        .unwrap();
        drop(r);
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.starts_with(std::str::from_utf8(&good).unwrap()));
        assert_eq!(after.lines().count(), 3);
        assert!(!list(dir.path()).unwrap()[0].torn_tail);

        // Flip a byte in the first line: corruption, not a torn tail.
        let mut bad = std::fs::read(&path).unwrap();
        let i = bad.iter().position(|b| *b == b'{').unwrap() + 2;
        bad[i] ^= 1;
        std::fs::write(&path, &bad).unwrap();
        assert_eq!(list(dir.path()).unwrap()[0].status, JournalStatus::Corrupt);
        assert!(matches!(
            Transaction::resume(&path, clock()),
            Err(JournalError::Corrupt { line: 1, .. })
        ));
        assert!(Transaction::begin(dir.path(), begin("b"), clock()).is_err());
    }

    #[derive(Debug)]
    struct FailAfter(usize);

    impl JournalSink for FailAfter {
        fn append(&mut self, _: &[u8]) -> io::Result<()> {
            if self.0 == 0 {
                return Err(io::Error::other("disk full"));
            }
            self.0 -= 1;
            Ok(())
        }
    }

    #[test]
    fn a_failed_append_poisons_the_transaction() {
        let mut t = Transaction::with_sink(Box::new(FailAfter(1)), clock());
        t.record(begin("a")).unwrap();
        assert!(t.record(intent()).is_err());
        assert!(!t.ledger().is_dirty(), "the failed intent is not applied");
        assert!(t.record(Entry::Note { message: "x".into() }).is_err());
    }

    #[test]
    fn invalid_entries_are_rejected_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = Transaction::begin(dir.path(), begin("a"), clock()).unwrap();
        let bad = Entry::Intent {
            purpose: Purpose::Survey,
            changes: vec![Change {
                scope: Scope::Band(3),
                field: "FA".into(),
                from: None,
                to: "1".into(),
                wire: String::new(),
            }],
        };
        assert!(t.record(bad).is_err());
        let path = t.path().unwrap().to_owned();
        drop(t);
        assert_eq!(std::fs::read_to_string(path).unwrap().lines().count(), 1);
    }

    /// A long survey: one FA intent and verification per tile.
    fn tile(t: &mut Transaction, band: u8, fa: u64) {
        let scope = Scope::Band(band);
        if !t.ledger().has_capture(&scope, "FA") {
            t.record(Entry::Capture {
                scope: scope.clone(),
                values: BTreeMap::from([("FA".into(), "00007000000".into())]),
            })
            .unwrap();
        }
        t.record(Entry::Intent {
            purpose: Purpose::Survey,
            changes: vec![Change {
                scope,
                field: "FA".into(),
                from: None,
                to: format!("{fa:011}"),
                wire: format!("FA{fa:011};"),
            }],
        })
        .unwrap();
        t.record(Entry::Verified {
            state: BTreeMap::from([("BN".into(), format!("{band:02}")), ("FA".into(), format!("{fa:011}"))]),
        })
        .unwrap();
    }

    #[test]
    fn long_journals_are_compacted_to_an_equivalent_locked_file() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = Transaction::begin(dir.path(), begin("a"), clock()).unwrap();
        let path = t.path().unwrap().to_owned();
        let mut peak = 0;
        // About 450 bytes per tile: several compactions.
        for i in 0..1500u64 {
            tile(&mut t, 3 + (i % 5) as u8, 7_000_000 + i * 1000);
            peak = peak.max(std::fs::metadata(&path).unwrap().len());
        }
        // Compacted size plus one threshold's worth of appends.
        assert!(peak <= COMPACT_BYTES + 16 * 1024, "peak {peak}");
        assert!(t.bytes() < COMPACT_BYTES, "{}", t.bytes());
        assert_eq!(&load(&path).unwrap(), t.ledger(), "the file replays to the ledger");
        // Only the last two (normalized) values per field are kept.
        let ob = t.ledger().obligation(&Scope::Band(7), "FA").unwrap();
        assert_eq!(ob.ours, vec!["8494000".to_owned(), "8499000".to_owned()]);
        // Still locked by this transaction after the rename, and no side
        // file is left behind.
        assert_eq!(list(dir.path()).unwrap()[0].status, JournalStatus::Running);
        assert!(matches!(
            Transaction::resume(&path, clock()),
            Err(JournalError::Locked(_))
        ));
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        // The original start time survives compaction.
        let started = list(dir.path()).unwrap()[0].started_utc_ns;
        let expected = t.ledger().clone();
        drop(t);
        let r = Transaction::resume(&path, clock()).unwrap();
        assert_eq!(r.ledger(), &expected);
        assert_eq!(list(dir.path()).unwrap()[0].started_utc_ns, started);
        assert!(serde_json::to_value(r.ledger()).is_ok(), "ledger is JSON-serializable");
    }
}
