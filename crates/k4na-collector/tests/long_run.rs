#![allow(clippy::unwrap_used)]

//! Accelerated (paused-time) 24-hour survey under the minimum disk cap.

mod common;

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use common::*;
use k4na_collector::cat::{ControlCat, RetuneConsent};
use k4na_collector::net::Connector;
use k4na_collector::runtime::stop_pair;
use k4na_collector::spectrum::{SpectrumOptions, StreamSpectrum};
use k4na_collector::survey::{begin_journal, preflight, run_survey};
use k4na_core::Config;
use k4na_core::config::Mode;
use k4na_core::time::{Clock, NS_PER_SEC};
use k4na_sim::SimRadio;
use k4na_store::StoreOptions;
use k4na_store::estimate::{admission, estimate};
use k4na_store::reader::DataSet;

fn dir_bytes(p: &Path) -> u64 {
    std::fs::read_dir(p).map_or(0, |rd| {
        rd.flatten()
            .map(|e| match e.metadata() {
                Ok(m) if m.is_dir() => dir_bytes(&e.path()),
                Ok(m) => m.len(),
                Err(_) => 0,
            })
            .sum()
    })
}

fn walkdir(p: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(p).unwrap().flatten() {
        let m = e.metadata().unwrap();
        if m.is_dir() {
            out.extend(walkdir(&e.path()));
        } else {
            out.push(format!("{:>10} {}", m.len(), e.path().display()));
        }
    }
    out.sort();
    out
}

fn tight_config(root: &Path) -> Config {
    // Back-to-back cycles (no cadence idle) are the worst case for disk.
    let text = station(root, "survey")
        .replace("max_total_bytes = 4294967296", "max_total_bytes = 67108864")
        .replace("raw_retention_hours = 48", "raw_retention_hours = 3")
        .replace("summary_retention_days = 30", "summary_retention_days = 1")
        .replace("protected_summary_hours = 24", "protected_summary_hours = 1")
        .replace("shard_duration_minutes = 60", "shard_duration_minutes = 15");
    let profile = HF_FULL
        .replace("min_cycle_ms = 300000", "min_cycle_ms = 0")
        .replace("dwell_ms = 5000", "dwell_ms = 3000");
    let cfg = Config::from_parts(&text, Some(&profile)).unwrap();
    // The same admission check the CLI runs before opening the store.
    let opts = StoreOptions::from_config(&cfg.storage);
    let p = cfg.profile.as_ref().unwrap();
    let est = estimate(
        &opts,
        1,
        p.settle_ms + p.dwell_ms + 250,
        p.dwell_ms,
        cfg.collection.raw_sample_interval_ms,
    );
    admission(&opts, &est, 0, 1 << 40).unwrap();
    cfg
}

#[tokio::test(start_paused = true)]
async fn twenty_four_hour_survey_stays_under_the_cap() {
    // 4 h keeps CI fast and already crosses the soft limit; run the full day
    // with `mise run long-run` (K4NA_LONG_RUN_HOURS=24).
    let hours = std::env::var("K4NA_LONG_RUN_HOURS")
        .ok()
        .and_then(|h| h.parse().ok())
        .unwrap_or(4u64);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("data");
    let cfg = tight_config(&root);
    let radio = SimRadio::new(3);
    radio.faults(|f| {
        f.pan_center_offset_hz = -11_240;
        f.garbage_every = Some(97);
    });
    let clock: Arc<dyn Clock> = clock();
    let stream = SimConnector {
        radio: radio.clone(),
        port: Port::Cat,
    }
    .connect()
    .await
    .unwrap();
    let mut cat = ControlCat::new(
        stream,
        Duration::from_millis(500),
        RetuneConsent::new(true, true).unwrap(),
    );
    let pre = preflight(&mut cat, &cfg).await.unwrap();
    let sid = "0190cccc-0000-7000-8000-000000000001";
    let store = open_store(&cfg, sid, Mode::Survey, &clock);
    let mut txn = begin_journal(&cfg, sid, &pre, clock.clone()).unwrap();
    let journal = txn.path().unwrap().to_owned();

    // Faults: drop the spectrum stream every 3 h; sample disk usage.
    let peak = Arc::new(AtomicU64::new(0));
    let (p, r, rt) = (peak.clone(), radio.clone(), root.clone());
    tokio::spawn(async move {
        let mut minutes = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(600)).await;
            minutes += 10;
            p.fetch_max(dir_bytes(&rt), Ordering::Relaxed);
            if minutes.is_multiple_of(180) {
                r.disconnect_pan();
            }
        }
    });
    let opts = SpectrumOptions {
        stall_timeout: Duration::from_secs(5),
        reconnect_initial: Duration::from_secs(1),
        reconnect_max: Duration::from_secs(8),
    };
    let mut spec = StreamSpectrum::new(
        SimConnector {
            radio: radio.clone(),
            port: Port::Pan,
        },
        opts,
        clock.clone(),
    );
    let (_h, stop) = stop_pair();
    let report = run_survey(
        ctx(&cfg, &store, &clock, sid, stop),
        &mut spec,
        &mut cat,
        pre,
        &mut txn,
        Duration::from_secs(hours * 3600),
    )
    .await
    .unwrap();
    drop(txn);
    let journal_bytes = std::fs::metadata(&journal).unwrap().len();
    eprintln!("recovery journal: {journal_bytes} bytes");
    // Outside the quotas, so it must stay bounded by compaction.
    assert!(
        journal_bytes <= k4na_collector::recovery::COMPACT_BYTES + 64 * 1024,
        "journal grew to {journal_bytes} bytes"
    );
    let end = clock.now();
    let status = store.close(end, &report.summary.end_reason).await.unwrap();

    if report.halt != "stop" {
        for e in walkdir(&root) {
            eprintln!("{e}");
        }
    }
    assert_eq!(report.halt, "stop", "{report:?}");
    assert!(report.restore.starts_with("restored"), "{}", report.restore);
    assert!(radio.forbidden_commands().is_empty());
    let cap = cfg.storage.max_total_bytes;
    let peak = peak.load(Ordering::Relaxed).max(dir_bytes(&root));
    assert!(peak <= cap, "peak {peak} > cap {cap}");
    assert!(report.cycles as f64 >= hours as f64 * 3600.0 / 250.0, "{report:?}");

    let ds = DataSet::open(&root).unwrap();
    let insp = ds.inspect().unwrap();
    assert_eq!(insp.dirty_shards, 0);
    assert!(insp.unreadable_shards.is_empty());
    assert!(insp.summary_shards >= 2, "{insp:?}");
    // Older gap events may have been pruned with their shards; count the
    // collector's own tally (initial connect + one per forced disconnect).
    assert!(
        report.summary.gaps >= hours / 3,
        "disconnects recorded as gaps: {report:?}"
    );
    assert!(insp.gap_events >= 1);
    assert!(insp.events_by_kind.contains_key("raw_sampling_paused"), "{insp:?}");
    if hours >= 6 {
        assert!(status.pruned_files > 0, "{status:?}");
    }
    // Every complete visit in the protected window survived.
    let protected_start = end.utc_ns - 3600 * NS_PER_SEC;
    let recent = ds
        .visits()
        .unwrap()
        .into_iter()
        .filter(|v| v.started_utc_ns >= protected_start && v.status == "complete")
        .count();
    let per_hour = 3600.0 / (0.7 + 3.0 + 0.3);
    assert!(
        recent as f64 >= per_hour * 0.8,
        "only {recent} complete visits in the protected window"
    );
    eprintln!(
        "hours={hours} peak={peak} visits={} cycles={} raw_dropped={} events={:?}",
        report.summary.visits, report.cycles, status.raw_dropped, insp.events_by_kind
    );
}
