#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use k4na_core::aggregate::VisitStats;
use k4na_core::config::{AntennaConfig, K4Port, Mode};
use k4na_core::model::{
    Event, EventKind, RawFrameRecord, SessionInfo, TileRef, VisitAntenna, VisitRecord, VisitStatus,
};
use k4na_core::time::{NS_PER_HOUR, NS_PER_SEC, Timestamp};
use k4na_core::units::{BinGeometry, HzRange};
use k4na_protocol::PanId;
use k4na_store::codec::Compression;
use k4na_store::reader::DataSet;
use k4na_store::{SpaceProbe, Store, StoreError, StoreOptions};

const T0: i64 = 1_793_059_200 * NS_PER_SEC; // 2026-10-27T00:00:00Z

#[derive(Debug, Clone)]
struct FakeDisk(Arc<AtomicU64>);

impl SpaceProbe for FakeDisk {
    fn available_bytes(&self, _: &Path) -> std::io::Result<u64> {
        Ok(self.0.load(Ordering::SeqCst))
    }
}

fn opts(root: &Path) -> StoreOptions {
    StoreOptions {
        root: root.to_owned(),
        max_total_bytes: 1 << 30,
        min_free_bytes: 0,
        raw_retention_ns: 48 * NS_PER_HOUR,
        summary_retention_ns: 30 * 24 * NS_PER_HOUR,
        protected_summary_ns: 24 * NS_PER_HOUR,
        shard_duration_ns: NS_PER_HOUR,
        compression: Compression::Zstd(3),
        soft_limit_fraction: 0.85,
        raw_resume_fraction: 0.75,
        headroom_bytes: 64 * 1024,
    }
}

fn log_antenna() -> AntennaConfig {
    AntennaConfig {
        id: "log".into(),
        port: K4Port::RxAntIn1,
        description: String::new(),
        revision: "r1".into(),
        notes: String::new(),
    }
}

fn session(id: &str) -> SessionInfo {
    SessionInfo {
        session_id: id.into(),
        mode: Mode::Survey,
        started: ts(0),
        program_version: "test".into(),
        decoder_version: k4na_protocol::DECODER_VERSION.into(),
        mapping_version: k4na_core::units::MAPPING_VERSION.into(),
        config_digest: "digest".into(),
        config_toml: "".into(),
        profile_id: Some("test".into()),
        antennas: vec![log_antenna()],
        receiver: "main".into(),
        firmware: vec!["FP-1".into()],
        snapshot: Default::default(),
    }
}

fn ts(offset_ns: i64) -> Timestamp {
    Timestamp {
        utc_ns: T0 + offset_ns,
        mono_ns: offset_ns as u64,
    }
}

/// Pseudo-noise stats so compression behaves like real data.
fn stats(seed: u64) -> VisitStats {
    let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    let mut next = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x % 200) as i16
    };
    let base: Vec<i16> = (0..2048).map(|_| -1400 + next()).collect();
    VisitStats {
        frame_count: 45,
        stats: std::array::from_fn(|k| base.iter().map(|v| v + 10 * k as i16).collect()),
        noise_field_min: -1320,
        noise_field_median: -1310,
        noise_field_max: -1300,
        sequence_repeats: 0,
        identical_frames: false,
    }
}

fn visit(session_id: &str, n: u64, at: i64) -> VisitRecord {
    let g = BinGeometry::new(14_045_000, 96_000, 2048);
    VisitRecord {
        visit_id: format!("visit-{n}"),
        session_id: session_id.into(),
        pan: PanId::A,
        band_id: Some("20m".into()),
        tile: Some(TileRef {
            index: 0,
            id: "20m/1of5".into(),
            target: HzRange::new(14_010_000, 14_080_000),
        }),
        requested_vfo_hz: Some(14_045_000),
        requested_span_hz: Some(40_000),
        geometry: g,
        usable_fraction: 0.8,
        full: g.full_range(),
        usable: g.usable_range(0.8),
        started: ts(at - 3_700_000_000),
        first_frame: Some(ts(at - 3_000_000_000)),
        last_frame: Some(ts(at - 10_000_000)),
        ended: ts(at),
        settle_ms: 700,
        settle_discarded_frames: 10,
        rejected_frames: 0,
        status: VisitStatus::Complete,
        flags: vec![],
        end_reason: "dwell complete".into(),
        antenna: VisitAntenna::configured(&log_antenna(), Some(K4Port::RxAntIn1)),
        radio_state: [("PA".to_string(), "PA1".to_string())].into(),
        measurement: [("preamp".to_string(), "pre1".to_string())].into(),
        stats: Some(stats(n)),
    }
}

fn raw(session_id: &str, at: i64, seed: u8) -> RawFrameRecord {
    let s = k4na_protocol::synth::SyntheticFrame {
        sequence: seed,
        ..Default::default()
    };
    let bins: Vec<i16> = (0..2048)
        .map(|i| -1400 + ((i * 7 + seed as usize * 13) % 211) as i16)
        .collect();
    RawFrameRecord {
        session_id: session_id.into(),
        visit_id: None,
        received: ts(at),
        pan: PanId::A,
        sequence: seed,
        center_hz: 14_045_000,
        sample_rate_hz: 96_000,
        sampling_policy: "test".into(),
        bytes: s.encode(&bins).to_vec(),
    }
}

fn big_disk() -> Box<dyn SpaceProbe> {
    Box::new(FakeDisk(Arc::new(AtomicU64::new(1 << 40))))
}

#[test]
fn rotates_hourly_and_round_trips_summaries() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    let sid = "0192f3a4-0000-7000-8000-00000000abcd";
    let (mut store, report) = Store::open(opts(&root), big_disk(), session(sid), ts(0)).unwrap();
    assert!(report.recovered.is_empty());
    for n in 0..30u64 {
        let at = (n as i64) * 10 * 60 * NS_PER_SEC + 5 * NS_PER_SEC; // every 10 min for 5 h
        store.write_visit(&visit(sid, n, at), &[]).unwrap();
        store.write_raw(&raw(sid, at, n as u8)).unwrap();
    }
    let gap = Event::new(sid, ts(NS_PER_HOUR), EventKind::Gap, "test gap")
        .interval(T0 + NS_PER_HOUR, T0 + NS_PER_HOUR + 30 * NS_PER_SEC);
    store.write_events(&[gap], ts(NS_PER_HOUR)).unwrap();
    let status = store.close(ts(5 * NS_PER_HOUR), "duration reached").unwrap();
    assert_eq!(status.visits_written, 30);
    assert_eq!(status.raw_written, 30);

    let ds = DataSet::open(&root).unwrap();
    let shards: Vec<_> = ds.shards().collect();
    assert_eq!(shards.iter().filter(|s| s.kind == "summary").count(), 5);
    assert_eq!(shards.iter().filter(|s| s.kind == "raw").count(), 5);
    assert!(shards.iter().all(|s| s.finalized && s.error.is_none()));
    // Finalized shards are single files (no WAL left behind).
    for s in &shards {
        assert!(!Path::new(&format!("{}-wal", s.path.display())).exists());
    }
    let r = ds.inspect().unwrap();
    assert_eq!(r.visits_total, 30);
    assert_eq!(r.raw_frames, 30);
    assert_eq!(r.gap_events, 1);
    assert_eq!(r.gap_seconds, 30.0);
    assert_eq!(r.sessions.len(), 1);
    assert_eq!(r.sessions[0].end_reason.as_deref(), Some("duration reached"));
    assert_eq!(r.segments, 1);
    assert_eq!(r.bands[0].band_id, "20m");
    assert_eq!(r.bands[0].antenna_id, "log");
    assert_eq!(r.sessions[0].antennas, ["log"]);
    assert_eq!(r.bands[0].covered_hz, 70_000);
    assert_eq!(r.bands[0].max_revisit_s, Some(600.0));

    // Identical arrays come back, validated by codec and checksum.
    assert_eq!(ds.summary("visit-7").unwrap().unwrap(), stats(7).stats);
    // Bulk iteration across shards yields the same validated arrays.
    let mut seen = Vec::new();
    let n = ds
        .for_each_summary(|v, s| {
            let i: u64 = v.visit_id.trim_start_matches("visit-").parse().unwrap();
            assert_eq!(s, stats(i).stats);
            seen.push(i);
            Ok(())
        })
        .unwrap();
    assert_eq!(n, 30);
    seen.sort_unstable();
    assert_eq!(seen, (0..30).collect::<Vec<_>>());
    let mut replayed = 0;
    ds.for_each_raw(|row| {
        let frames = k4na_protocol::Decoder::new().decode(&row.bytes);
        assert_eq!(frames.len(), 1);
        replayed += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(replayed, 30);
}

#[test]
fn second_writer_is_refused_and_foreign_roots_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    let (store, _) = Store::open(opts(&root), big_disk(), session("aaaaaaaa"), ts(0)).unwrap();
    let err = Store::open(opts(&root), big_disk(), session("bbbbbbbb"), ts(0)).unwrap_err();
    assert!(matches!(err, StoreError::Locked(_)), "{err}");
    drop(store);

    let foreign = dir.path().join("home");
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(foreign.join("thesis.docx"), b"precious").unwrap();
    let err = Store::open(opts(&foreign), big_disk(), session("cccccccc"), ts(0)).unwrap_err();
    assert!(matches!(err, StoreError::BadRoot(_)), "{err}");
    assert!(foreign.join("thesis.docx").exists());
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let dst = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_tree(&e.path(), &dst);
        } else if e.file_name() != ".k4na-writer.lock" {
            std::fs::copy(e.path(), dst).unwrap();
        }
    }
}

#[test]
fn crash_mid_transaction_recovers_committed_data_only() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    let sid = "dddddddd";
    let (mut store, _) = Store::open(opts(&root), big_disk(), session(sid), ts(0)).unwrap();
    for n in 0..3 {
        store
            .write_visit(&visit(sid, n, (n as i64 + 1) * 4 * NS_PER_SEC), &[])
            .unwrap();
    }
    // A second connection starts a transaction that never commits, as if the
    // process died mid-write; then the files are copied ("crash image").
    let shard = k4na_store::shard::discover(&root)
        .unwrap()
        .into_iter()
        .find(|f| f.name.kind == k4na_store::shard::ShardKind::Summary)
        .unwrap();
    let conn = rusqlite::Connection::open(&shard.path).unwrap();
    conn.execute_batch("BEGIN IMMEDIATE; DELETE FROM visits; INSERT INTO events (session_id, utc_ns, mono_ns, kind, message) VALUES ('x', 0, 0, 'warning', 'uncommitted');").unwrap();
    let crashed = dir.path().join("crashed");
    copy_tree(&root, &crashed);
    drop(conn);
    drop(store);

    // The crash image has a dirty (unfinalized) shard with a WAL.
    let before = DataSet::open(&crashed).unwrap();
    assert!(before.shards().any(|s| !s.finalized));

    let (store2, report) = Store::open(opts(&crashed), big_disk(), session("eeeeeeee"), ts(60 * NS_PER_SEC)).unwrap();
    assert_eq!(report.recovered.len(), 1);
    store2.close(ts(61 * NS_PER_SEC), "test end").unwrap();

    let after = DataSet::open(&crashed).unwrap();
    assert!(after.shards().all(|s| s.finalized));
    assert!(after.shards().any(|s| s.recovered));
    let r = after.inspect().unwrap();
    assert_eq!(
        r.visits_total, 3,
        "committed visits survive, uncommitted delete does not"
    );
    assert!(after.events().unwrap().iter().all(|e| e.message != "uncommitted"));
    let crashed_session = r.sessions.iter().find(|s| s.session_id == sid).unwrap();
    assert!(
        crashed_session
            .end_reason
            .as_deref()
            .unwrap()
            .contains("abnormal termination")
    );
    assert!(r.events_by_kind.get("shard_recovered").copied().unwrap_or(0) >= 1);
    assert!(r.gap_events >= 1, "the crash is an explicit gap");
    // Recovery is idempotent.
    let (store3, report3) = Store::open(opts(&crashed), big_disk(), session("ffffffff"), ts(70 * NS_PER_SEC)).unwrap();
    assert!(report3.recovered.is_empty());
    drop(store3);
}

#[test]
fn disk_cap_is_never_exceeded_and_foreign_files_survive_pruning() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    std::fs::create_dir_all(root.join("2026/10/26")).unwrap();
    std::fs::write(root.join("K4NA-DATA-ROOT"), "marker").unwrap();
    // Foreign files, including ones that mimic our naming.
    let foreign = [
        root.join("notes.txt"),
        root.join("2026/10/26/summary-foreign.sqlite"),
        root.join("2026/10/26/summary-20261026T0000Z-deadbeef.sqlite"),
        root.join("2026/10/26/raw-20261026T0100Z-deadbeef.sqlite-wal"),
    ];
    for f in &foreign {
        std::fs::write(f, b"not ours").unwrap();
    }
    std::fs::create_dir_all(root.join("elsewhere")).unwrap();
    std::fs::write(root.join("elsewhere/keep.bin"), b"keep").unwrap();

    let mut o = opts(&root);
    o.max_total_bytes = 6 * 1024 * 1024;
    o.protected_summary_ns = 3 * NS_PER_HOUR;
    o.headroom_bytes = 128 * 1024;
    let sid = "0a0a0a0a";
    let (mut store, report) = Store::open(o.clone(), big_disk(), session(sid), ts(0)).unwrap();
    assert_eq!(
        report.unreadable.len(),
        1,
        "garbage file with our name is reported, not deleted"
    );

    let mut max_usage = 0;
    let mut suspended_at = None;
    // 48 simulated hours: a visit every 60 s with a raw frame every 20 s.
    'outer: for minute in 0..(48 * 60) {
        let at = minute as i64 * 60 * NS_PER_SEC + NS_PER_SEC;
        for k in 0..3 {
            store
                .write_raw(&raw(sid, at + k * 20 * NS_PER_SEC, (minute % 251) as u8))
                .unwrap();
        }
        match store.write_visit(&visit(sid, minute, at), &[]) {
            Ok(()) => {}
            Err(StoreError::Suspended(_)) => {
                suspended_at = Some(minute);
                break 'outer;
            }
            Err(e) => panic!("{e}"),
        }
        let s = store.status();
        max_usage = max_usage.max(s.usage_bytes);
        assert!(
            s.usage_bytes <= o.max_total_bytes,
            "usage {} exceeded cap",
            s.usage_bytes
        );
    }
    let status = store.close(ts(49 * NS_PER_HOUR), "end").unwrap();
    assert!(suspended_at.is_none(), "cap holds >3 h of summaries; pruning keeps up");
    assert!(status.pruned_files > 0);
    assert!(
        status.raw_dropped > 0 || status.raw_paused,
        "raw sampling paused under pressure"
    );
    assert!(max_usage <= o.max_total_bytes);
    for f in &foreign {
        assert!(f.exists(), "{} deleted", f.display());
    }
    assert!(root.join("elsewhere/keep.bin").exists());

    let ds = DataSet::open(&root).unwrap();
    let r = ds.inspect().unwrap();
    assert!(r.events_by_kind.get("pruned").copied().unwrap_or(0) > 0);
    assert!(r.events_by_kind.get("raw_sampling_paused").copied().unwrap_or(0) > 0);
    // The protected window (last 3 h) is intact.
    let newest = r.last_utc_ns.unwrap();
    let visits = ds.visits().unwrap();
    let recent = visits
        .iter()
        .filter(|v| v.ended_utc_ns > newest - 3 * NS_PER_HOUR)
        .count();
    assert!(recent >= 179, "protected window kept {recent} visits");
}

/// Production headroom (16 MiB) is 25% of the minimum 64 MiB cap. The soft
/// limit must sit below the hard ceiling (budget - headroom), or pressure
/// pruning never starts and the store suspends with prunable data on disk.
/// Found by the accelerated 24 h collector test.
#[test]
fn large_headroom_still_prunes_before_the_ceiling() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    let mut o = opts(&root);
    o.max_total_bytes = 6 * 1024 * 1024;
    o.headroom_bytes = 1536 * 1024;
    o.protected_summary_ns = NS_PER_HOUR;
    let sid = "0b0b0b0b";
    let (mut store, _) = Store::open(o.clone(), big_disk(), session(sid), ts(0)).unwrap();
    for minute in 0..(24 * 60) {
        let at = minute as i64 * 60 * NS_PER_SEC + NS_PER_SEC;
        for k in 0..3 {
            store
                .write_raw(&raw(sid, at + k * 20 * NS_PER_SEC, (minute % 251) as u8))
                .unwrap();
        }
        store
            .write_visit(&visit(sid, minute, at), &[])
            .unwrap_or_else(|e| panic!("minute {minute}: {e}"));
        store.maintain(ts(at)).unwrap();
        let s = store.status();
        assert!(
            s.usage_bytes + o.headroom_bytes <= o.max_total_bytes,
            "minute {minute}: {s:?}"
        );
    }
    let status = store.close(ts(25 * NS_PER_HOUR), "end").unwrap();
    assert!(status.pruned_files > 0);
    assert!(status.suspended.is_none());
}

#[test]
fn shrinking_free_space_suspends_instead_of_filling_disk() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    let free = Arc::new(AtomicU64::new(50 * 1024 * 1024));
    let mut o = opts(&root);
    o.min_free_bytes = 10 * 1024 * 1024;
    let sid = "0b0b0b0b";
    let (mut store, _) = Store::open(o, Box::new(FakeDisk(free.clone())), session(sid), ts(0)).unwrap();
    for n in 0..5 {
        store
            .write_visit(&visit(sid, n, (n as i64 + 1) * 4 * NS_PER_SEC), &[])
            .unwrap();
    }
    // Another program fills the volume to below min_free_bytes.
    free.store(9 * 1024 * 1024, Ordering::SeqCst);
    let err = store.write_visit(&visit(sid, 99, 60 * NS_PER_SEC), &[]).unwrap_err();
    assert!(
        matches!(err, StoreError::Suspended(ref m) if m.contains("storage ceiling")),
        "{err}"
    );
    assert!(store.write_events(&[], ts(61 * NS_PER_SEC)).is_err());
    assert_eq!(
        store.write_raw(&raw(sid, 62 * NS_PER_SEC, 1)).unwrap(),
        k4na_store::RawOutcome::Dropped
    );
    store.close(ts(63 * NS_PER_SEC), "storage suspended").unwrap();
    let r = DataSet::open(&root).unwrap().inspect().unwrap();
    assert_eq!(r.visits_total, 5);
    assert_eq!(r.events_by_kind.get("storage_suspended"), Some(&1));
}

#[test]
fn retention_prunes_old_raw_independently() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    let mut o = opts(&root);
    o.raw_retention_ns = 2 * NS_PER_HOUR;
    let sid = "0c0c0c0c";
    let (mut store, _) = Store::open(o, big_disk(), session(sid), ts(0)).unwrap();
    for h in 0..6 {
        let at = h * NS_PER_HOUR + NS_PER_SEC;
        store.write_raw(&raw(sid, at, h as u8)).unwrap();
        store.write_visit(&visit(sid, h as u64, at), &[]).unwrap();
    }
    store.close(ts(6 * NS_PER_HOUR), "end").unwrap();
    let ds = DataSet::open(&root).unwrap();
    let raw_shards = ds.shards().filter(|s| s.kind == "raw").count();
    let summary_shards = ds.shards().filter(|s| s.kind == "summary").count();
    assert!(raw_shards <= 3, "{raw_shards}");
    assert_eq!(summary_shards, 6);
}

#[test]
fn measurement_settings_round_trip_and_v2_shards_stay_readable() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    let sid = "0e0e0e0e";
    let (mut store, _) = Store::open(opts(&root), big_disk(), session(sid), ts(0)).unwrap();
    store.write_visit(&visit(sid, 1, NS_PER_SEC), &[]).unwrap();
    store.close(ts(2 * NS_PER_SEC), "end").unwrap();
    let ds = DataSet::open(&root).unwrap();
    let v = ds.visits().unwrap().remove(0);
    assert_eq!(v.measurement_json.as_deref(), Some(r#"{"preamp":"pre1"}"#));

    // A version 2 shard (no measurement_json column) written before this
    // release must still read.
    let shard = k4na_store::shard::discover(&root).unwrap().remove(0);
    let conn = rusqlite::Connection::open(&shard.path).unwrap();
    conn.execute_batch(
        "DROP INDEX IF EXISTS visits_antenna_band_time; ALTER TABLE visits DROP COLUMN measurement_json;
         UPDATE meta SET value = '2' WHERE key = 'schema_version';",
    )
    .unwrap();
    drop(conn);
    let ds = DataSet::open(&root).unwrap();
    assert!(ds.shards().all(|s| s.error.is_none()));
    let v = ds.visits().unwrap().remove(0);
    assert_eq!(v.measurement_json, None);
    assert_eq!(v.antenna_id, "log");
    let n = ds
        .for_each_summary(|v, _| {
            assert_eq!(v.measurement_json, None);
            Ok(())
        })
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn verified_receiver_settings_decide_the_comparability_segment() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    let at = |n: i64| n * NS_PER_SEC;
    let with = |sid: &str, n: u64, pa: &str| {
        let mut v = visit(sid, n, at(n as i64));
        v.radio_state = [("PA".to_string(), pa.to_string()), ("FA".to_string(), format!("{n}"))].into();
        v
    };
    let (mut store, _) = Store::open(opts(&root), big_disk(), session("s1"), ts(0)).unwrap();
    store.write_visit(&with("s1", 1, "1"), &[]).unwrap();
    store.write_visit(&with("s1", 2, "1"), &[]).unwrap();
    store.write_visit(&with("s1", 3, "0"), &[]).unwrap();
    store.close(ts(at(4)), "end").unwrap();
    // A later session with the same verified settings joins the segment.
    let (mut store, _) = Store::open(opts(&root), big_disk(), session("s2"), ts(at(5))).unwrap();
    store.write_visit(&with("s2", 6, "1"), &[]).unwrap();
    store.close(ts(at(7)), "end").unwrap();

    let ds = DataSet::open(&root).unwrap();
    let seg: std::collections::BTreeMap<String, String> = ds
        .visits()
        .unwrap()
        .into_iter()
        .map(|v| (v.visit_id, v.segment_id))
        .collect();
    assert_eq!(seg.len(), 4, "history is kept");
    assert_eq!(seg["visit-1"], seg["visit-2"], "FA is not a comparability key");
    assert_ne!(seg["visit-1"], seg["visit-3"], "a preamp change starts a new segment");
    assert_eq!(
        seg["visit-1"], seg["visit-6"],
        "segment ids are deterministic across sessions"
    );
}

#[test]
fn unsupported_schema_versions_are_rejected_by_readers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    let sid = "0d0d0d0d";
    let (mut store, _) = Store::open(opts(&root), big_disk(), session(sid), ts(0)).unwrap();
    store.write_visit(&visit(sid, 1, NS_PER_SEC), &[]).unwrap();
    store.close(ts(2 * NS_PER_SEC), "end").unwrap();
    let shard = k4na_store::shard::discover(&root).unwrap().remove(0);
    let conn = rusqlite::Connection::open(&shard.path).unwrap();
    conn.execute("UPDATE meta SET value = '99' WHERE key = 'schema_version'", [])
        .unwrap();
    drop(conn);
    let ds = DataSet::open(&root).unwrap();
    let info = ds.shards().next().unwrap();
    assert!(
        info.error.as_deref().unwrap().contains("unsupported schema version"),
        "{info:?}"
    );
    assert_eq!(ds.inspect().unwrap().unreadable_shards.len(), 1);
}
