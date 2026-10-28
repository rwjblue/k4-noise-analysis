#![allow(clippy::unwrap_used)]

//! End to end: shards written by the real store writer with known
//! synthetic structure, analyzed through the read-only reader.

use std::path::Path;

use k4na_analysis::{Filter, Params, band_hours_csv, carriers_csv, run};
use k4na_core::aggregate::VisitStats;
use k4na_core::config::{AntennaConfig, K4Port, Mode};
use k4na_core::model::{SessionInfo, TileRef, VisitAntenna, VisitRecord, VisitStatus};
use k4na_core::time::{NS_PER_HOUR, NS_PER_SEC, Timestamp};
use k4na_core::units::{BinGeometry, HzRange};
use k4na_protocol::PanId;
use k4na_store::codec::Compression;
use k4na_store::reader::{DataSet, VisitRow};
use k4na_store::{SpaceProbe, Store, StoreOptions};

const T0: i64 = 1_793_059_200 * NS_PER_SEC; // 2026-10-27T00:00:00Z
const CENTER: u64 = 14_045_000;
const STEADY_HZ: f64 = 14_030_000.0; // bin 704 at 96 kHz / 2048
const SOMETIMES_HZ: f64 = 14_060_000.0; // bin 1344
const ONCE_HZ: f64 = 14_070_000.0; // bin 1557

#[derive(Debug)]
struct BigDisk;

impl SpaceProbe for BigDisk {
    fn available_bytes(&self, _: &Path) -> std::io::Result<u64> {
        Ok(1 << 40)
    }
}

fn antenna() -> AntennaConfig {
    AntennaConfig {
        id: "rx1".into(),
        port: K4Port::RxAntIn1,
        description: String::new(),
        revision: "r1".into(),
        notes: String::new(),
    }
}

fn ts(offset_ns: i64) -> Timestamp {
    Timestamp {
        utc_ns: T0 + offset_ns,
        mono_ns: offset_ns as u64,
    }
}

fn bin(hz: f64) -> usize {
    ((hz - CENTER as f64) / 46.875 + 1024.0) as usize
}

/// Floor -130 dB (+5 dB during hour 3) with small deterministic ripple,
/// a carrier in every visit, one in every fourth visit and one once.
fn stats(n: u64, hour: i64) -> VisitStats {
    let floor = if hour == 3 { -1250 } else { -1300 };
    let mut p50: Vec<i16> = (0..2048)
        .map(|i| floor + ((i * 37 + n as usize * 11) % 21) as i16 - 10)
        .collect();
    let mut p10: Vec<i16> = p50.iter().map(|v| v - 15).collect();
    p50[bin(STEADY_HZ)] = floor + 200;
    p10[bin(STEADY_HZ)] = floor + 190;
    if n.is_multiple_of(4) {
        p50[bin(SOMETIMES_HZ)] = floor + 120;
    }
    if n == 5 {
        p50[bin(ONCE_HZ)] = floor + 300;
    }
    let p90: Vec<i16> = p50.iter().map(|v| v + 15).collect();
    VisitStats {
        frame_count: 75,
        stats: [
            p10.iter().map(|v| v - 15).collect(),
            p10,
            p50.clone(),
            p90.clone(),
            p90.iter().map(|v| v + 15).collect(),
        ],
        noise_field_min: 0,
        noise_field_median: 0,
        noise_field_max: 0,
        sequence_repeats: 0,
        identical_frames: false,
    }
}

fn visit(n: u64, at: i64, status: VisitStatus) -> VisitRecord {
    let g = BinGeometry::new(CENTER, 96_000, 2048);
    let hour = at / NS_PER_HOUR;
    VisitRecord {
        visit_id: format!("visit-{n}"),
        session_id: "s1".into(),
        pan: PanId::A,
        band_id: Some("20m".into()),
        tile: Some(TileRef {
            index: 0,
            id: "20m/1of1".into(),
            target: HzRange::new(14_010_000, 14_080_000),
        }),
        requested_vfo_hz: Some(CENTER),
        requested_span_hz: Some(40_000),
        geometry: g,
        usable_fraction: 0.8,
        full: g.full_range(),
        usable: g.usable_range(0.8),
        started: ts(at),
        first_frame: Some(ts(at + NS_PER_SEC)),
        last_frame: Some(ts(at + 5 * NS_PER_SEC)),
        ended: ts(at + 6 * NS_PER_SEC),
        settle_ms: 700,
        settle_discarded_frames: 10,
        rejected_frames: 0,
        status,
        flags: vec![],
        end_reason: "dwell complete".into(),
        antenna: VisitAntenna::configured(&antenna(), Some(K4Port::RxAntIn1)),
        radio_state: [("PA".to_string(), "PA1".to_string())].into(),
        measurement: [("preamp".to_string(), "pre1".to_string())].into(),
        stats: Some(stats(n, hour)),
    }
}

/// Six hours, a visit every 5 minutes, plus one partial visit.
fn write_root(root: &Path) -> u64 {
    write_root_with(root, |_| 75)
}

/// As `write_root`, with `frames(hour)` accepted frames per visit over
/// the same 4 s (75 frames = 19 fps).
fn write_root_with(root: &Path, frames: impl Fn(i64) -> u32) -> u64 {
    let opts = StoreOptions {
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
    };
    let session = SessionInfo {
        session_id: "s1".into(),
        mode: Mode::Survey,
        started: ts(0),
        program_version: "test".into(),
        decoder_version: k4na_protocol::DECODER_VERSION.into(),
        mapping_version: k4na_core::units::MAPPING_VERSION.into(),
        config_digest: "digest".into(),
        config_toml: String::new(),
        profile_id: Some("test".into()),
        antennas: vec![antenna()],
        receiver: "main".into(),
        firmware: vec![],
        snapshot: Default::default(),
    };
    let (mut store, _) = Store::open(opts, Box::new(BigDisk), session, ts(0)).unwrap();
    let n = 6 * 12;
    for i in 0..n {
        let at = i as i64 * 300 * NS_PER_SEC + NS_PER_SEC;
        let mut v = visit(i, at, VisitStatus::Complete);
        if let Some(s) = v.stats.as_mut() {
            s.frame_count = frames(at / NS_PER_HOUR);
        }
        store.write_visit(&v, &[]).unwrap();
    }
    store
        .write_visit(&visit(999, 1000 * NS_PER_SEC, VisitStatus::Partial), &[])
        .unwrap();
    store.close(ts(6 * NS_PER_HOUR), "end").unwrap();
    n
}

#[test]
fn finds_floors_by_hour_and_classifies_carriers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    let n = write_root(&root);
    let ds = DataSet::open(&root).unwrap();
    let mut observed = 0;
    let a = run(&ds, &Filter::default(), &Params::default(), |s, m, bg| {
        assert_eq!(s.len(), bg.len());
        assert!(s.low_hz >= 14_010_000 && s.high_hz <= 14_080_000);
        assert!(!m.peaks.is_empty());
        observed += 1;
    })
    .unwrap();
    assert_eq!(observed, n);
    assert_eq!(a.provenance.visits_analyzed, n);
    assert_eq!(
        a.provenance.visits_skipped, 1,
        "the partial visit is not an observation"
    );
    assert_eq!(a.provenance.shards.len(), 6);
    assert_eq!(a.provenance.sessions, ["s1"]);
    assert!(a.provenance.calibration[0].contains("uncalibrated"));

    // One row per UTC hour; hour 3 is 5 dB higher.
    assert_eq!(a.band_hours.len(), 6);
    for (h, r) in a.band_hours.iter().enumerate() {
        assert_eq!(r.hour_start_utc_ns, T0 + h as i64 * NS_PER_HOUR);
        assert_eq!(r.visits, 12);
        let want = if h == 3 { -125.0 } else { -130.0 };
        assert!((r.floor_db - want).abs() <= 1.0, "hour {h}: {}", r.floor_db);
        assert_eq!(r.measurement.as_deref(), Some(r#"{"preamp":"pre1"}"#));
    }

    // The single-visit peak is below min_carrier_detections.
    assert_eq!(a.carriers.len(), 2, "{:#?}", a.carriers);
    let steady = &a.carriers[0];
    assert!((steady.hz - STEADY_HZ).abs() < 47.0);
    assert_eq!((steady.detections, steady.exposure), (n, n));
    assert_eq!(steady.class, "stationary");
    assert_eq!(steady.steady_fraction, 1.0);
    assert!((steady.median_excess_db - 20.0).abs() <= 1.5);
    assert_eq!(steady.hour_detections[..6], [12; 6]);
    let sometimes = &a.carriers[1];
    assert!((sometimes.hz - SOMETIMES_HZ).abs() < 47.0);
    // At its usual level (-118 dB) it would stand only 7 dB over hour 3's
    // -125 dB floor, under the 8 dB threshold: hour 3's nine visits
    // without it are masked, not absences.
    assert_eq!(
        (sometimes.detections, sometimes.exposure, sometimes.masked),
        (n / 4, n - 9, 9)
    );
    assert!((sometimes.median_level_db - -118.0).abs() <= 0.5);
    assert_eq!(steady.masked, 0, "20 dB over the floor stays visible");
    assert_eq!(sometimes.class, "recurring");
    assert_eq!(sometimes.steady_fraction, 0.0);

    // Hour 3's +5 dB is one floor episode bounded by the quiet visits on
    // either side; the 5-minute cadence is the revisit interval.
    assert_eq!(a.floor_activity.len(), 1);
    let act = &a.floor_activity[0];
    assert_eq!((act.tiles, act.visits, act.episodes), (1, n, 1));
    assert_eq!(act.revisit_s, 300.0);
    assert!((act.elevated_fraction - 12.0 / n as f64).abs() < 1e-9);
    assert_eq!(a.floor_episodes.len(), 1, "{:#?}", a.floor_episodes);
    let e = &a.floor_episodes[0];
    let visit_at = |i: i64| T0 + i * 300 * NS_PER_SEC + NS_PER_SEC;
    assert_eq!(e.start_utc_ns, visit_at(36));
    assert_eq!(e.start_after_utc_ns, Some(visit_at(35) + 6 * NS_PER_SEC));
    assert_eq!(e.end_utc_ns, visit_at(47) + 6 * NS_PER_SEC);
    assert_eq!(e.end_before_utc_ns, Some(visit_at(48)));
    assert_eq!((e.visits, e.tiles, e.tiles_analyzed), (12, 1, 1));
    assert!((e.median_rise_db - 5.0).abs() <= 1.0, "{}", e.median_rise_db);
    assert_eq!(k4na_analysis::floor_episodes_csv(&a.floor_episodes).lines().count(), 2);
    assert_eq!(k4na_analysis::floor_activity_csv(&a.floor_activity).lines().count(), 2);

    // Exports carry one header plus a row per result.
    assert_eq!(band_hours_csv(&a.band_hours).lines().count(), 7);
    let csv = carriers_csv(&a.carriers);
    assert_eq!(csv.lines().count(), 3);
    assert!(csv.lines().nth(1).unwrap().starts_with("20m,rx1,14030"));
}

#[test]
fn visits_at_different_frame_rates_are_not_one_segment() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    // 37 fps in hours 4-5, as when a second client stops sharing the radio.
    let n = write_root_with(&root, |h| if h >= 4 { 149 } else { 75 });
    let ds = DataSet::open(&root).unwrap();
    let a = run(&ds, &Filter::default(), &Params::default(), |_, _, _| {}).unwrap();
    let segs: Vec<&str> = a.band_hours.iter().map(|r| r.segment_id.as_str()).collect();
    assert!(segs[..4].iter().all(|s| s.ends_with("@19fps")), "{segs:?}");
    assert!(segs[4..].iter().all(|s| s.ends_with("@37fps")), "{segs:?}");
    let stored = |s: &str| s.split_once('@').unwrap().0.to_owned();
    assert_ne!(
        stored(segs[0]),
        stored(segs[4]),
        "the writer keys segments by frame rate"
    );
    assert_eq!(a.carriers[0].detections, n);
    assert_eq!(a.carriers[0].segments, 2);
    assert_eq!(k4na_analysis::short_segment(segs[0]).len(), "01234567@19fps".len());
}

#[test]
fn rows_from_shards_without_a_frame_rate_key_are_split_on_read() {
    let row = |frames: i64, last_mono_ns: i64| VisitRow {
        visit_id: "v".into(),
        session_id: "s".into(),
        segment_id: "0123456789abcdef01234567".into(),
        pan: "A".into(),
        band_id: Some("20m".into()),
        tile_id: None,
        target_low_hz: None,
        target_high_hz: None,
        requested_vfo_hz: None,
        center_hz: 0,
        sample_rate_hz: 0,
        bin_count: 0,
        actual_low_hz: 0,
        actual_high_hz: 0,
        started_utc_ns: 0,
        ended_utc_ns: 0,
        first_mono_ns: Some(1_000_000_000),
        last_mono_ns: Some(last_mono_ns),
        accepted_frames: frames,
        rejected_frames: 0,
        settle_discarded: 0,
        status: "complete".into(),
        flags: String::new(),
        end_reason: String::new(),
        antenna_id: "rx1".into(),
        antenna_port: "rx_ant_in1".into(),
        antenna_observed_port: None,
        measurement_json: None,
    };
    // The K4D's two observed rates in one stored segment.
    let slow = k4na_analysis::analysis_segment(&row(75, 5_900_000_000));
    let fast = k4na_analysis::analysis_segment(&row(151, 5_934_000_000));
    assert_eq!(slow, "0123456789abcdef01234567@15fps");
    assert_eq!(fast, "0123456789abcdef01234567@30fps");
    assert_eq!(
        k4na_analysis::analysis_segment(&row(1, 1_000_000_000)),
        "0123456789abcdef01234567@?fps"
    );
    assert_eq!(k4na_analysis::short_segment(&fast), "01234567@30fps");
    assert_eq!(k4na_analysis::short_segment("0123456789"), "01234567");
}

#[test]
fn filters_select_visits_and_results_are_deterministic() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    write_root(&root);
    let ds = DataSet::open(&root).unwrap();
    let f = Filter {
        since_utc_ns: Some(T0 + 2 * NS_PER_HOUR),
        until_utc_ns: Some(T0 + 4 * NS_PER_HOUR),
        ..Default::default()
    };
    let a = run(&ds, &f, &Params::default(), |_, _, _| {}).unwrap();
    assert_eq!(a.provenance.visits_analyzed, 24);
    assert_eq!(a.band_hours.len(), 2);
    let b = run(&ds, &f, &Params::default(), |_, _, _| {}).unwrap();
    assert_eq!(serde_json::to_string(&a).unwrap(), serde_json::to_string(&b).unwrap());

    let none = Filter {
        antenna: Some("dxc".into()),
        ..Default::default()
    };
    let a = run(&ds, &none, &Params::default(), |_, _, _| {}).unwrap();
    assert_eq!(a.provenance.visits_analyzed, 0);
    assert!(a.band_hours.is_empty() && a.carriers.is_empty());
}
