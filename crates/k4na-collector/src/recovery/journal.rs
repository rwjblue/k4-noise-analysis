//! Durable, append-only journal files: `<dir>/<session_id>.jsonl`.
//!
//! Each line is `<check> <json>\n`, where `check` is the first 16 hex
//! digits of SHA-256 over the JSON text. Every append is fsynced before
//! the call returns, so an intent is on disk before its setters are sent.
//! A torn final line (crash mid-write) is detected and dropped; a bad line
//! followed by good ones means corruption, and automatic recovery refuses.
//! A running survey holds an exclusive OS lock on its journal, so a
//! recovery cannot race it and a crashed process never leaves a stale lock.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use k4na_core::time::Clock;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Entry, Ledger, Outcome};

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
    started_utc_ns: Option<i64>,
    next_seq: u64,
    valid_len: u64,
    torn_tail: bool,
}

fn parse(path: &Path, bytes: &[u8]) -> Result<Loaded, JournalError> {
    let mut l = Loaded {
        ledger: Ledger::default(),
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
        let rec = Record {
            seq: self.next_seq,
            utc_ns: self.clock.now().utc_ns,
            entry: e,
        };
        let json = serde_json::to_string(&rec).map_err(|e| JournalError::Invalid(e.to_string()))?;
        let line = format!("{} {json}\n", check(&json));
        if let Err(e) = self.sink.append(line.as_bytes()) {
            self.failed = true;
            return Err(e.into());
        }
        self.next_seq += 1;
        self.ledger = next;
        Ok(())
    }
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
}
