//! Single-writer shard store: rotation, atomic visit commits, crash
//! recovery, retention and hard byte budgets.

use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};

use k4na_core::aggregate::{QUANTILE_DEFINITION, STAT_NAMES};
use k4na_core::model::{CALIBRATION, Event, EventKind, RawFrameRecord, SessionInfo, UNITS, VisitRecord};
use k4na_core::time::{NS_PER_HOUR, Timestamp, format_utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::StoreError;
use crate::codec::{Compression, SUMMARY_CODEC, encode_raw, encode_stats};
use crate::schema::{FORMAT, SCHEMA_VERSION, ddl};
use crate::shard::{ShardFile, ShardKind, ShardName, discover, read_meta, shard_bytes};

pub const ROOT_MARKER: &str = "K4NA-DATA-ROOT";
pub const LOCK_FILE: &str = ".k4na-writer.lock";

/// Source of free-space readings (replaceable to simulate disk pressure).
pub trait SpaceProbe: Send + std::fmt::Debug {
    fn available_bytes(&self, path: &Path) -> std::io::Result<u64>;
}

#[derive(Debug, Default)]
pub struct FsSpaceProbe;

impl SpaceProbe for FsSpaceProbe {
    fn available_bytes(&self, path: &Path) -> std::io::Result<u64> {
        fs4::available_space(path)
    }
}

#[derive(Debug, Clone)]
pub struct StoreOptions {
    pub root: PathBuf,
    pub max_total_bytes: u64,
    pub min_free_bytes: u64,
    pub raw_retention_ns: i64,
    pub summary_retention_ns: i64,
    pub protected_summary_ns: i64,
    pub shard_duration_ns: i64,
    pub compression: Compression,
    pub soft_limit_fraction: f64,
    pub raw_resume_fraction: f64,
    /// Space reserved for the largest pending transaction, WAL growth and a
    /// rotation (new shard creation plus checkpoint).
    pub headroom_bytes: u64,
}

impl StoreOptions {
    pub fn from_config(c: &k4na_core::config::StorageConfig) -> Self {
        Self {
            root: c.root.clone(),
            max_total_bytes: c.max_total_bytes,
            min_free_bytes: c.min_free_bytes,
            raw_retention_ns: c.raw_retention_hours as i64 * NS_PER_HOUR,
            summary_retention_ns: c.summary_retention_days as i64 * 24 * NS_PER_HOUR,
            protected_summary_ns: c.protected_summary_hours as i64 * NS_PER_HOUR,
            shard_duration_ns: c.shard_duration_minutes as i64 * 60 * 1_000_000_000,
            compression: match c.compression {
                k4na_core::config::Compression::Zstd => Compression::Zstd(c.zstd_level),
                k4na_core::config::Compression::None => Compression::None,
            },
            soft_limit_fraction: c.soft_limit_fraction,
            raw_resume_fraction: c.raw_resume_fraction,
            headroom_bytes: 16 * 1024 * 1024,
        }
    }
}

/// Live counters, also persisted into logs and the CLI's health output.
#[derive(Debug, Clone, Default, Serialize)]
pub struct StoreStatus {
    pub usage_bytes: u64,
    pub budget_bytes: u64,
    pub free_bytes: u64,
    pub summary_bytes: u64,
    pub raw_bytes: u64,
    pub raw_paused: bool,
    pub suspended: Option<String>,
    pub visits_written: u64,
    pub raw_written: u64,
    pub raw_dropped: u64,
    pub events_written: u64,
    pub pruned_files: u64,
    pub pruned_bytes: u64,
    pub shards_finalized: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RecoveryReport {
    pub recovered: Vec<PathBuf>,
    /// Shards that could not be opened or failed integrity checks. They
    /// still count toward the byte budget and are never auto-deleted.
    pub unreadable: Vec<(PathBuf, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawOutcome {
    Written,
    Dropped,
}

#[derive(Debug, Clone)]
struct CatalogEntry {
    file: ShardFile,
    period_end_utc_ns: i64,
    /// Finalized with a valid format marker: eligible for pruning.
    prunable: bool,
}

#[derive(Debug)]
struct OpenShard {
    name: ShardName,
    path: PathBuf,
    period_end_utc_ns: i64,
    conn: Connection,
    segments: HashSet<String>,
}

#[derive(Debug)]
pub struct Store {
    opts: StoreOptions,
    probe: Box<dyn SpaceProbe>,
    _lock: File,
    session: SessionInfo,
    session_tag: String,
    summary: Option<OpenShard>,
    raw: Option<OpenShard>,
    catalog: Vec<CatalogEntry>,
    status: StoreStatus,
    pending: Vec<Event>,
    last_time: Timestamp,
    /// Per kind (summary, raw): earliest period a new shard may use, so a
    /// finalized period is never reopened.
    next_period: [i64; 2],
}

fn kind_slot(kind: ShardKind) -> usize {
    match kind {
        ShardKind::Summary => 0,
        ShardKind::Raw => 1,
    }
}

fn session_tag(session_id: &str) -> String {
    let hex: String = session_id
        .chars()
        .filter(char::is_ascii_hexdigit)
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if hex.len() >= 8 {
        hex[hex.len() - 8..].to_owned()
    } else {
        hex::encode(&Sha256::digest(session_id.as_bytes())[..4])
    }
}

impl Store {
    /// Opens (creating if needed) the data root, takes the exclusive writer
    /// lock, recovers dirty shards from earlier crashed sessions and applies
    /// retention once.
    pub fn open(
        opts: StoreOptions,
        probe: Box<dyn SpaceProbe>,
        session: SessionInfo,
        now: Timestamp,
    ) -> Result<(Self, RecoveryReport), StoreError> {
        prepare_root(&opts.root)?;
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(opts.root.join(LOCK_FILE))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Err(StoreError::Locked(opts.root.clone())),
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        let session_tag = session_tag(&session.session_id);
        let mut store = Self {
            opts,
            probe,
            _lock: lock,
            session,
            session_tag,
            summary: None,
            raw: None,
            catalog: Vec::new(),
            status: StoreStatus::default(),
            pending: Vec::new(),
            last_time: now,
            next_period: [i64::MIN; 2],
        };
        let report = store.recover_and_catalog(now)?;
        store.enforce(now.utc_ns)?;
        Ok((store, report))
    }

    pub fn status(&self) -> &StoreStatus {
        &self.status
    }

    pub fn session(&self) -> &SessionInfo {
        &self.session
    }

    pub fn options(&self) -> &StoreOptions {
        &self.opts
    }

    /// Queues an event to be committed with the next transaction.
    pub fn queue_event(&mut self, event: Event) {
        self.pending.push(event);
    }

    fn recover_and_catalog(&mut self, now: Timestamp) -> Result<RecoveryReport, StoreError> {
        let mut report = RecoveryReport::default();
        for file in discover(&self.opts.root)? {
            let path = file.path.clone();
            match recover_one(&path, file.name.kind, &self.session.session_id, now) {
                Ok((recovered, period_end)) => {
                    if recovered {
                        report.recovered.push(path.clone());
                    }
                    let bytes = shard_bytes(&path);
                    self.catalog.push(CatalogEntry {
                        file: ShardFile { bytes, ..file },
                        period_end_utc_ns: period_end,
                        prunable: true,
                    });
                }
                Err(e) => {
                    report.unreadable.push((path.clone(), e.to_string()));
                    self.catalog.push(CatalogEntry {
                        period_end_utc_ns: i64::MAX,
                        prunable: false,
                        file,
                    });
                }
            }
        }
        for p in &report.recovered {
            self.pending.push(Event::new(
                &self.session.session_id,
                now,
                EventKind::ShardRecovered,
                format!(
                    "recovered and finalized dirty shard {} left by an abnormal exit",
                    p.display()
                ),
            ));
        }
        for (p, e) in &report.unreadable {
            self.pending.push(Event::new(
                &self.session.session_id,
                now,
                EventKind::Warning,
                format!("unreadable shard {} kept (counts toward budget): {e}", p.display()),
            ));
        }
        Ok(report)
    }

    fn period_start(&self, utc_ns: i64) -> i64 {
        utc_ns.div_euclid(self.opts.shard_duration_ns) * self.opts.shard_duration_ns
    }

    /// Ensures the open shard of `kind` covers `utc_ns`, rotating if needed.
    /// Periods never move backwards: after a backwards clock step, writes
    /// continue in the current shard (the clock jump is recorded separately).
    fn shard_for(&mut self, kind: ShardKind, utc_ns: i64) -> Result<&mut OpenShard, StoreError> {
        let want = self.period_start(utc_ns).max(self.next_period[kind_slot(kind)]);
        let slot_period = match kind {
            ShardKind::Summary => self.summary.as_ref().map(|s| s.name.period_start_utc_ns),
            ShardKind::Raw => self.raw.as_ref().map(|s| s.name.period_start_utc_ns),
        };
        let need_new = match slot_period {
            None => true,
            Some(p) => want > p,
        };
        if need_new {
            if let Some(old) = self.take_slot(kind) {
                self.finalize_shard(old, None)?;
            }
            let shard = self.create_shard(kind, want)?;
            match kind {
                ShardKind::Summary => self.summary = Some(shard),
                ShardKind::Raw => self.raw = Some(shard),
            }
        }
        let slot = match kind {
            ShardKind::Summary => self.summary.as_mut(),
            ShardKind::Raw => self.raw.as_mut(),
        };
        slot.ok_or_else(|| StoreError::Codec("shard slot unexpectedly empty".into()))
    }

    fn take_slot(&mut self, kind: ShardKind) -> Option<OpenShard> {
        match kind {
            ShardKind::Summary => self.summary.take(),
            ShardKind::Raw => self.raw.take(),
        }
    }

    fn create_shard(&mut self, kind: ShardKind, period_start: i64) -> Result<OpenShard, StoreError> {
        let name = ShardName {
            period_start_utc_ns: period_start,
            kind,
            session_tag: self.session_tag.clone(),
        };
        let path = self.opts.root.join(name.relative_path());
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if path.exists() {
            // Same session and period: only possible after a clock step
            // forward-then-back; refuse to append to a finalized shard.
            return Err(StoreError::Codec(format!("shard {} already exists", path.display())));
        }
        let conn = Connection::open(&path)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(
            None,
            "synchronous",
            if kind == ShardKind::Summary { "FULL" } else { "NORMAL" },
        )?;
        conn.pragma_update(None, "journal_size_limit", 4 * 1024 * 1024)?;
        conn.pragma_update(None, "wal_autocheckpoint", 256)?;
        conn.execute_batch(&ddl(kind))?;
        let period_end = period_start + self.opts.shard_duration_ns;
        let s = &self.session;
        let meta: Vec<(&str, String)> = vec![
            ("format", FORMAT.into()),
            ("schema_version", SCHEMA_VERSION.to_string()),
            ("shard_kind", kind.as_str().into()),
            ("created_utc_ns", k4na_core::time::system_utc_ns().to_string()),
            ("period_start_utc_ns", period_start.to_string()),
            ("period_end_utc_ns", period_end.to_string()),
            ("session_id", s.session_id.clone()),
            ("program_version", s.program_version.clone()),
            ("decoder_version", s.decoder_version.clone()),
            ("mapping_version", s.mapping_version.clone()),
            ("config_digest", s.config_digest.clone()),
            ("summary_codec", SUMMARY_CODEC.into()),
            ("units", UNITS.into()),
            ("calibration", CALIBRATION.into()),
            (
                "clock",
                "collector host UTC (SystemTime) plus per-session monotonic ns".into(),
            ),
            ("frequency_intervals", "half-open [low_hz, high_hz)".into()),
        ];
        let tx = conn.unchecked_transaction()?;
        for (k, v) in meta {
            tx.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
                params![k, v],
            )?;
        }
        insert_session(&tx, s)?;
        tx.commit()?;
        self.catalog.retain(|c| c.file.path != path);
        Ok(OpenShard {
            name,
            path,
            period_end_utc_ns: period_end,
            conn,
            segments: HashSet::new(),
        })
    }

    fn finalize_shard(&mut self, shard: OpenShard, session_end: Option<(&Timestamp, &str)>) -> Result<(), StoreError> {
        let OpenShard {
            name,
            path,
            period_end_utc_ns,
            conn,
            ..
        } = shard;
        let slot = kind_slot(name.kind);
        self.next_period[slot] = self.next_period[slot].max(period_end_utc_ns);
        if let Some((end, reason)) = session_end {
            conn.execute(
                "UPDATE sessions SET ended_utc_ns = ?1, end_reason = ?2 WHERE session_id = ?3",
                params![end.utc_ns, reason, self.session.session_id],
            )?;
        }
        finalize_conn(&conn, false)?;
        drop(conn);
        self.status.shards_finalized += 1;
        let bytes = shard_bytes(&path);
        self.catalog.push(CatalogEntry {
            file: ShardFile { name, path, bytes },
            period_end_utc_ns,
            prunable: true,
        });
        Ok(())
    }

    fn check_writable(&self) -> Result<(), StoreError> {
        match &self.status.suspended {
            Some(reason) => Err(StoreError::Suspended(reason.clone())),
            None => Ok(()),
        }
    }

    /// Commits a visit, its summary and any events in one transaction.
    pub fn write_visit(&mut self, visit: &VisitRecord, events: &[Event]) -> Result<(), StoreError> {
        self.check_writable()?;
        let at = visit.ended;
        self.last_time = self.last_time.max(at);
        self.enforce(at.utc_ns)?;
        self.check_writable()?;
        let compression = self.opts.compression;
        let session = self.session.clone();
        let mut pending = std::mem::take(&mut self.pending);
        pending.extend_from_slice(events);
        let shard = self.shard_for(ShardKind::Summary, at.utc_ns)?;
        let key = visit.segment_key(&session);
        let segment_id = hex::encode(&Sha256::digest(key.as_bytes())[..12]);
        let tx = shard.conn.unchecked_transaction()?;
        if shard.segments.insert(segment_id.clone()) {
            tx.execute(
                "INSERT OR IGNORE INTO segments (segment_id, session_id, pan, antenna_id, segment_key, state_json, first_utc_ns)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    segment_id,
                    session.session_id,
                    visit.pan.as_str(),
                    session.antenna_id,
                    key,
                    serde_json::to_string(&visit.radio_state)?,
                    visit.started.utc_ns
                ],
            )?;
        }
        insert_visit(&tx, visit, &segment_id)?;
        if let Some(stats) = &visit.stats {
            let enc = encode_stats(&stats.stats, compression)?;
            tx.execute(
                "INSERT INTO summaries (visit_id, pan, codec, compression, quantile_definition, stat_names, bin_count,
                    frame_count, units, uncompressed_len, sha256, data)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    visit.visit_id,
                    visit.pan.as_str(),
                    SUMMARY_CODEC,
                    enc.compression,
                    QUANTILE_DEFINITION,
                    STAT_NAMES.join(","),
                    enc.bin_count,
                    stats.frame_count,
                    UNITS,
                    enc.uncompressed_len,
                    enc.sha256,
                    enc.data
                ],
            )?;
        }
        for e in &pending {
            insert_event(&tx, e)?;
        }
        tx.commit()?;
        self.status.visits_written += 1;
        self.status.events_written += pending.len() as u64;
        Ok(())
    }

    /// Commits events (gaps, interlocks, connection changes) immediately.
    pub fn write_events(&mut self, events: &[Event], now: Timestamp) -> Result<(), StoreError> {
        self.check_writable()?;
        self.last_time = self.last_time.max(now);
        self.enforce(now.utc_ns)?;
        self.check_writable()?;
        let mut pending = std::mem::take(&mut self.pending);
        pending.extend_from_slice(events);
        if pending.is_empty() {
            return Ok(());
        }
        let shard = self.shard_for(ShardKind::Summary, now.utc_ns)?;
        let tx = shard.conn.unchecked_transaction()?;
        for e in &pending {
            insert_event(&tx, e)?;
        }
        tx.commit()?;
        self.status.events_written += pending.len() as u64;
        Ok(())
    }

    /// Stores a sampled original frame unless raw sampling is paused by
    /// storage pressure. Raw data is expendable; drops are counted.
    pub fn write_raw(&mut self, raw: &RawFrameRecord) -> Result<RawOutcome, StoreError> {
        if self.status.suspended.is_some() || self.status.raw_paused {
            self.status.raw_dropped += 1;
            return Ok(RawOutcome::Dropped);
        }
        let (compression, data) = encode_raw(&raw.bytes, self.opts.compression)?;
        let shard = self.shard_for(ShardKind::Raw, raw.received.utc_ns)?;
        shard.conn.execute(
            "INSERT INTO raw_frames (session_id, visit_id, pan, received_utc_ns, received_mono_ns, sequence, center_hz,
                sample_rate_hz, sampling_policy, compression, byte_len, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                raw.session_id,
                raw.visit_id,
                raw.pan.as_str(),
                raw.received.utc_ns,
                raw.received.mono_ns as i64,
                raw.sequence,
                raw.center_hz as i64,
                raw.sample_rate_hz,
                raw.sampling_policy,
                compression,
                raw.bytes.len() as i64,
                data
            ],
        )?;
        self.status.raw_written += 1;
        Ok(RawOutcome::Written)
    }

    /// Periodic housekeeping: time-based rotation and budget enforcement.
    pub fn maintain(&mut self, now: Timestamp) -> Result<(), StoreError> {
        self.last_time = self.last_time.max(now);
        for kind in [ShardKind::Summary, ShardKind::Raw] {
            let expired = match kind {
                ShardKind::Summary => self.summary.as_ref().is_some_and(|s| now.utc_ns >= s.period_end_utc_ns),
                ShardKind::Raw => self.raw.as_ref().is_some_and(|s| now.utc_ns >= s.period_end_utc_ns),
            };
            if expired && let Some(old) = self.take_slot(kind) {
                self.finalize_shard(old, None)?;
            }
        }
        self.enforce(now.utc_ns)?;
        if !self.pending.is_empty() && self.status.suspended.is_none() {
            self.write_events(&[], now)?;
        }
        Ok(())
    }

    /// Recomputes usage and applies the eviction order from docs/storage.md.
    fn enforce(&mut self, now_utc_ns: i64) -> Result<(), StoreError> {
        // 1-2. Retention, independent of pressure.
        let raw_cutoff = now_utc_ns - self.opts.raw_retention_ns;
        let summary_cutoff = now_utc_ns - self.opts.summary_retention_ns;
        self.prune_where(
            |c| match c.file.name.kind {
                ShardKind::Raw => c.period_end_utc_ns <= raw_cutoff,
                ShardKind::Summary => c.period_end_utc_ns <= summary_cutoff,
            },
            "retention expired",
            u64::MAX,
        )?;

        self.measure()?;
        // Thresholds are fractions of what may be used before the hard
        // ceiling (budget minus headroom) trips; otherwise a soft limit could
        // sit above the ceiling and pruning would never start.
        let effective = self.status.budget_bytes.saturating_sub(self.opts.headroom_bytes) as f64;
        let soft = (effective * self.opts.soft_limit_fraction) as u64;
        let resume = (effective * self.opts.raw_resume_fraction) as u64;

        if self.status.usage_bytes > soft {
            if !self.status.raw_paused {
                self.status.raw_paused = true;
                let msg = format!(
                    "storage at {} of {} budget bytes; raw sampling paused",
                    self.status.usage_bytes, self.status.budget_bytes
                );
                self.pending.push(Event::new(
                    &self.session.session_id,
                    self.last_time,
                    EventKind::RawSamplingPaused,
                    msg,
                ));
            }
            // Oldest finalized raw shards first, regardless of nominal retention.
            self.prune_where(|c| c.file.name.kind == ShardKind::Raw, "storage pressure (raw)", resume)?;
        }
        if self.status.usage_bytes > soft {
            // Then oldest summaries outside the protected window.
            let protect_from = now_utc_ns - self.opts.protected_summary_ns;
            self.prune_where(
                |c| c.file.name.kind == ShardKind::Summary && c.period_end_utc_ns <= protect_from,
                "storage pressure (summary outside protected window)",
                resume,
            )?;
        }
        if self.status.raw_paused && self.status.usage_bytes < resume {
            self.status.raw_paused = false;
            self.pending.push(Event::new(
                &self.session.session_id,
                self.last_time,
                EventKind::RawSamplingResumed,
                "storage below resume threshold; raw sampling resumed",
            ));
        }
        if self.status.usage_bytes + self.opts.headroom_bytes > self.status.budget_bytes {
            let reason = format!(
                "storage ceiling: usage {} + headroom {} exceeds effective budget {} (max_total_bytes {}, free {}, min_free_bytes {}); nothing prunable outside the protected summary window",
                self.status.usage_bytes,
                self.opts.headroom_bytes,
                self.status.budget_bytes,
                self.opts.max_total_bytes,
                self.status.free_bytes,
                self.opts.min_free_bytes
            );
            if self.status.suspended.is_none() {
                // Best effort: record why, without growing past the ceiling much.
                let ev = Event::new(
                    &self.session.session_id,
                    self.last_time,
                    EventKind::StorageSuspended,
                    reason.clone(),
                );
                if let Some(s) = self.summary.as_mut() {
                    let _ = insert_event(&s.conn, &ev);
                }
                self.status.suspended = Some(reason);
            }
        }
        Ok(())
    }

    fn measure(&mut self) -> Result<(), StoreError> {
        let mut summary = 0;
        let mut raw = 0;
        for c in &self.catalog {
            match c.file.name.kind {
                ShardKind::Summary => summary += c.file.bytes,
                ShardKind::Raw => raw += c.file.bytes,
            }
        }
        if let Some(s) = &self.summary {
            summary += shard_bytes(&s.path);
        }
        if let Some(s) = &self.raw {
            raw += shard_bytes(&s.path);
        }
        let other = std::fs::metadata(self.opts.root.join(LOCK_FILE))
            .map(|m| m.len())
            .unwrap_or(0)
            + std::fs::metadata(self.opts.root.join(ROOT_MARKER))
                .map(|m| m.len())
                .unwrap_or(0);
        let usage = summary + raw + other;
        let free = self.probe.available_bytes(&self.opts.root)?;
        let by_free = (usage + free).saturating_sub(self.opts.min_free_bytes);
        self.status.summary_bytes = summary;
        self.status.raw_bytes = raw;
        self.status.usage_bytes = usage;
        self.status.free_bytes = free;
        self.status.budget_bytes = self.opts.max_total_bytes.min(by_free);
        Ok(())
    }

    /// Deletes prunable catalog shards matching `pred`, oldest first, until
    /// usage is at or below `target` (use `u64::MAX` to delete all matches).
    fn prune_where(&mut self, pred: impl Fn(&CatalogEntry) -> bool, why: &str, target: u64) -> Result<(), StoreError> {
        let mut candidates: Vec<usize> = (0..self.catalog.len())
            .filter(|&i| self.catalog[i].prunable && pred(&self.catalog[i]))
            .collect();
        candidates.sort_by_key(|&i| (self.catalog[i].period_end_utc_ns, self.catalog[i].file.name.clone()));
        let mut removed = Vec::new();
        for i in candidates {
            if target != u64::MAX && self.status.usage_bytes <= target {
                break;
            }
            let entry = &self.catalog[i];
            // Re-verify the file is a finalized shard of ours right before deletion.
            if !verify_finalized(&entry.file.path, entry.file.name.kind) {
                continue;
            }
            let bytes = shard_bytes(&entry.file.path);
            delete_shard_files(&entry.file.path)?;
            self.status.pruned_files += 1;
            self.status.pruned_bytes += bytes;
            self.status.usage_bytes = self.status.usage_bytes.saturating_sub(bytes);
            self.pending.push(
                Event::new(
                    &self.session.session_id,
                    self.last_time,
                    EventKind::Pruned,
                    format!(
                        "pruned {} ({why})",
                        entry
                            .file
                            .path
                            .strip_prefix(&self.opts.root)
                            .unwrap_or(&entry.file.path)
                            .display()
                    ),
                )
                .count(bytes),
            );
            remove_empty_day_dirs(&self.opts.root, &entry.file.path);
            removed.push(i);
        }
        for i in removed.into_iter().rev() {
            self.catalog.remove(i);
        }
        Ok(())
    }

    /// Ends the session: records the end reason, finalizes open shards.
    pub fn close(mut self, end: Timestamp, reason: &str) -> Result<StoreStatus, StoreError> {
        let ev = Event::new(&self.session.session_id, end, EventKind::SessionEnd, reason.to_owned());
        self.pending.push(ev);
        // Even when suspended, record the end in the existing shard.
        let pending = std::mem::take(&mut self.pending);
        let shard = match self.summary.as_mut() {
            Some(s) => Some(s),
            None if self.status.suspended.is_none() => Some(self.shard_for(ShardKind::Summary, end.utc_ns)?),
            None => None,
        };
        if let Some(shard) = shard {
            let tx = shard.conn.unchecked_transaction()?;
            for e in &pending {
                insert_event(&tx, e)?;
            }
            tx.commit()?;
        }
        if let Some(s) = self.summary.take() {
            self.finalize_shard(s, Some((&end, reason)))?;
        }
        if let Some(s) = self.raw.take() {
            self.finalize_shard(s, Some((&end, reason)))?;
        }
        self.measure()?;
        Ok(self.status.clone())
    }
}

fn prepare_root(root: &Path) -> Result<(), StoreError> {
    let marker = root.join(ROOT_MARKER);
    if root.exists() {
        if !root.is_dir() {
            return Err(StoreError::BadRoot(format!("{} is not a directory", root.display())));
        }
        if !marker.exists() && std::fs::read_dir(root)?.next().is_some() {
            return Err(StoreError::BadRoot(format!(
                "{} is not empty and has no {ROOT_MARKER} marker; choose an empty or k4na-created directory",
                root.display()
            )));
        }
    }
    std::fs::create_dir_all(root)?;
    if !marker.exists() {
        std::fs::write(
            &marker,
            "This directory is a k4na measurement data root.\nk4na only creates, finalizes and prunes YYYY/MM/DD/{summary,raw}-*.sqlite shards here.\n",
        )?;
    }
    Ok(())
}

fn insert_session(conn: &Connection, s: &SessionInfo) -> Result<(), StoreError> {
    conn.execute(
        "INSERT OR IGNORE INTO sessions (session_id, mode, started_utc_ns, program_version, decoder_version, mapping_version,
            config_digest, config_toml, antenna_id, antenna_revision, antenna_description, antenna_notes, receiver,
            firmware_json, snapshot_json, units, calibration)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
            s.session_id,
            s.mode.as_str(),
            s.started.utc_ns,
            s.program_version,
            s.decoder_version,
            s.mapping_version,
            s.config_digest,
            s.config_toml,
            s.antenna_id,
            s.antenna_revision,
            s.antenna_description,
            s.antenna_notes,
            s.receiver,
            serde_json::to_string(&s.firmware)?,
            serde_json::to_string(&s.snapshot)?,
            UNITS,
            CALIBRATION
        ],
    )?;
    Ok(())
}

fn opt_i64(v: Option<u64>) -> Option<i64> {
    v.map(|x| x as i64)
}

fn insert_visit(conn: &Connection, v: &VisitRecord, segment_id: &str) -> Result<(), StoreError> {
    let s = v.stats.as_ref();
    conn.execute(
        "INSERT INTO visits (visit_id, session_id, segment_id, pan, band_id, tile_index, tile_id, target_low_hz, target_high_hz,
            requested_vfo_hz, requested_span_hz, center_hz, sample_rate_hz, bin_count, usable_fraction, full_low_hz, full_high_hz,
            actual_low_hz, actual_high_hz, started_utc_ns, started_mono_ns, first_utc_ns, first_mono_ns, last_utc_ns, last_mono_ns,
            ended_utc_ns, ended_mono_ns, settle_ms, settle_discarded, accepted_frames, rejected_frames, status, flags, end_reason,
            noise_field_min, noise_field_median, noise_field_max, sequence_repeats, radio_state_json)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24,
            ?25, ?26, ?27, ?28, ?29, ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39)",
        params![
            v.visit_id,
            v.session_id,
            segment_id,
            v.pan.as_str(),
            v.band_id,
            v.tile.as_ref().map(|t| t.index as i64),
            v.tile.as_ref().map(|t| t.id.clone()),
            v.tile.as_ref().map(|t| t.target.low_hz as i64),
            v.tile.as_ref().map(|t| t.target.high_hz as i64),
            opt_i64(v.requested_vfo_hz),
            v.requested_span_hz,
            v.geometry.center_hz as i64,
            v.geometry.sample_rate_hz,
            v.geometry.bin_count,
            v.usable_fraction,
            v.full.low_hz as i64,
            v.full.high_hz as i64,
            v.usable.low_hz as i64,
            v.usable.high_hz as i64,
            v.started.utc_ns,
            v.started.mono_ns as i64,
            v.first_frame.map(|t| t.utc_ns),
            v.first_frame.map(|t| t.mono_ns as i64),
            v.last_frame.map(|t| t.utc_ns),
            v.last_frame.map(|t| t.mono_ns as i64),
            v.ended.utc_ns,
            v.ended.mono_ns as i64,
            v.settle_ms as i64,
            v.settle_discarded_frames,
            v.accepted_frames(),
            v.rejected_frames,
            v.status.as_str(),
            v.flags.join(","),
            v.end_reason,
            s.map(|s| s.noise_field_min),
            s.map(|s| s.noise_field_median),
            s.map(|s| s.noise_field_max),
            s.map(|s| s.sequence_repeats),
            serde_json::to_string(&v.radio_state)?
        ],
    )?;
    Ok(())
}

fn insert_event(conn: &Connection, e: &Event) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO events (session_id, utc_ns, mono_ns, kind, start_utc_ns, end_utc_ns, band_id, tile_id, count, message)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            e.session_id,
            e.at.utc_ns,
            e.at.mono_ns as i64,
            e.kind.as_str(),
            e.start_utc_ns,
            e.end_utc_ns,
            e.band_id,
            e.tile_id,
            e.count.map(|c| c as i64),
            e.message
        ],
    )?;
    Ok(())
}

/// Marks a shard finalized and leaves it as a single file. Idempotent.
fn finalize_conn(conn: &Connection, recovered: bool) -> Result<(), StoreError> {
    let tx = conn.unchecked_transaction()?;
    let already: Option<String> = tx
        .query_row("SELECT value FROM meta WHERE key = 'finalized_utc_ns'", [], |r| {
            r.get(0)
        })
        .optional()?;
    if already.is_none() {
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('finalized_utc_ns', ?1)",
            [k4na_core::time::system_utc_ns().to_string()],
        )?;
    }
    if recovered {
        tx.execute("INSERT OR REPLACE INTO meta (key, value) VALUES ('recovered', '1')", [])?;
    }
    tx.commit()?;
    conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))?;
    let _: String = conn.query_row("PRAGMA journal_mode=DELETE", [], |r| r.get(0))?;
    Ok(())
}

/// Opens one discovered shard; if it was left dirty by a crashed writer,
/// records the abnormal end and finalizes it. Returns (recovered, period_end).
fn recover_one(
    path: &Path,
    kind: ShardKind,
    recovering_session: &str,
    now: Timestamp,
) -> Result<(bool, i64), StoreError> {
    let conn = Connection::open(path)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    let meta = read_meta(&conn).map_err(|_| StoreError::NotAShard(path.to_owned()))?;
    if !meta.is_ours(kind) {
        return Err(StoreError::NotAShard(path.to_owned()));
    }
    if meta.schema_version != Some(SCHEMA_VERSION) {
        return Err(StoreError::UnsupportedSchema {
            path: path.to_owned(),
            found: meta.schema_version,
        });
    }
    let period_end: i64 = conn
        .query_row("SELECT value FROM meta WHERE key = 'period_end_utc_ns'", [], |r| {
            r.get::<_, String>(0)
        })
        .optional()?
        .and_then(|v| v.parse().ok())
        .unwrap_or(i64::MAX);
    if meta.finalized_utc_ns.is_some() {
        return Ok((false, period_end));
    }
    let check: String = conn.query_row("PRAGMA quick_check", [], |r| r.get(0))?;
    if check != "ok" {
        return Err(StoreError::Codec(format!("integrity check failed: {check}")));
    }
    let last: Option<i64> = match kind {
        ShardKind::Summary => conn.query_row(
            "SELECT max(t) FROM (SELECT max(ended_utc_ns) AS t FROM visits UNION ALL SELECT max(utc_ns) FROM events)",
            [],
            |r| r.get(0),
        )?,
        ShardKind::Raw => conn.query_row("SELECT max(received_utc_ns) FROM raw_frames", [], |r| r.get(0))?,
    };
    let reason = format!(
        "abnormal termination; shard recovered at {} by session {recovering_session}",
        format_utc(now.utc_ns)
    );
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "UPDATE sessions SET end_reason = ?1, ended_utc_ns = ?2 WHERE ended_utc_ns IS NULL",
        params![reason, last],
    )?;
    if kind == ShardKind::Summary {
        let sessions: Vec<String> = {
            let mut st = tx.prepare("SELECT session_id FROM sessions")?;
            st.query_map([], |r| r.get(0))?.collect::<Result<_, _>>()?
        };
        for sid in sessions {
            let mut ev = Event::new(
                &sid,
                now,
                EventKind::Gap,
                format!("no data after last record: {reason}"),
            );
            if let Some(last) = last {
                ev = ev.interval(last, now.utc_ns.min(period_end).max(last));
            }
            insert_event(&tx, &ev)?;
        }
    }
    tx.commit()?;
    finalize_conn(&conn, true)?;
    Ok((true, period_end))
}

fn verify_finalized(path: &Path, kind: ShardKind) -> bool {
    match crate::shard::open_read_only(path) {
        Ok((_, meta)) => meta.is_ours(kind) && meta.finalized_utc_ns.is_some(),
        Err(_) => false,
    }
}

fn delete_shard_files(path: &Path) -> Result<(), StoreError> {
    for s in crate::shard::SIDE_SUFFIXES {
        let mut p = path.as_os_str().to_owned();
        p.push(s);
        match std::fs::remove_file(PathBuf::from(p)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    std::fs::remove_file(path)?;
    Ok(())
}

/// Removes now-empty `YYYY/MM/DD` ancestors of a pruned shard (never the root).
fn remove_empty_day_dirs(root: &Path, shard: &Path) {
    let mut dir = shard.parent();
    for _ in 0..3 {
        let Some(d) = dir else { break };
        if d == root || !d.starts_with(root) {
            break;
        }
        // remove_dir fails on non-empty directories, which is what we want.
        if std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
}
