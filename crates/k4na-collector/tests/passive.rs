#![allow(clippy::unwrap_used)]

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::*;
use k4na_collector::passive::{CatStateCache, poll_cat_state, run_passive};
use k4na_collector::runtime::stop_pair;
use k4na_collector::spectrum::{SpectrumOptions, StreamSpectrum};
use k4na_core::config::Mode;
use k4na_sim::SimRadio;
use k4na_store::reader::DataSet;

fn spectrum(radio: &SimRadio, clock: &Arc<dyn k4na_core::time::Clock>) -> StreamSpectrum<SimConnector> {
    let opts = SpectrumOptions {
        stall_timeout: Duration::from_secs(2),
        reconnect_initial: Duration::from_millis(500),
        reconnect_max: Duration::from_secs(4),
    };
    StreamSpectrum::new(
        SimConnector {
            radio: radio.clone(),
            port: Port::Pan,
        },
        opts,
        clock.clone(),
    )
}

#[tokio::test(start_paused = true)]
async fn passive_capture_records_windows_and_never_sends_setters() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir.path().join("data"), "passive");
    let radio = SimRadio::new(15);
    radio.faults(|f| f.extra_pans = true);
    let clock = clock();
    let store = open_store(&cfg, "0190aaaa-0000-7000-8000-000000000001", Mode::Passive, &clock);
    let (stop_handle, stop) = stop_pair();
    let cache = Arc::new(Mutex::new(CatStateCache::default()));
    let poller = tokio::spawn(poll_cat_state(
        SimConnector {
            radio: radio.clone(),
            port: Port::Cat,
        },
        Duration::from_millis(500),
        Duration::from_secs(5),
        cache.clone(),
        stop.clone(),
    ));
    let mut spec = spectrum(&radio, &clock);
    let ctx = ctx(&cfg, &store, &clock, "0190aaaa-0000-7000-8000-000000000001", stop);
    let summary = run_passive(ctx, &mut spec, Some(cache), Duration::from_secs(60))
        .await
        .unwrap();
    stop_handle.stop("test done");
    poller.await.unwrap();
    store.close(clock.now(), &summary.end_reason).await.unwrap();

    assert_eq!(summary.end_reason, "duration reached");
    assert!(!summary.failed);
    assert!(summary.visits >= 5, "{summary:?}");
    assert!(summary.ignored_frames.contains_key("B") && summary.ignored_frames.contains_key("Y"));
    assert!(
        radio.setters_received().is_empty(),
        "passive sent setters: {:?}",
        radio.setters_received()
    );
    assert!(radio.forbidden_commands().is_empty());
    assert!(!radio.commands().is_empty(), "GET-only readback was used");

    let ds = DataSet::open(&cfg.storage.root).unwrap();
    let r = ds.inspect().unwrap();
    assert!(r.visits_by_status.get("complete").copied().unwrap_or(0) >= 5, "{r:?}");
    assert_eq!(r.bands[0].band_id, "20m");
    assert!(r.raw_frames >= 50, "about one raw frame per second: {}", r.raw_frames);
    let visits = ds.visits().unwrap();
    let v = &visits[0];
    assert_eq!(v.pan, "A");
    assert_eq!(v.sample_rate_hz, 96_000);
    assert_eq!(v.center_hz, 14_074_000);
    // ~10 s window at 15 fps.
    assert!((140..=152).contains(&v.accepted_frames), "{}", v.accepted_frames);
    let stats = ds.summary(&v.visit_id).unwrap().unwrap();
    assert!(stats[0].iter().zip(&stats[4]).all(|(min, max)| min <= max));
    // The synthetic 14.100 MHz tone survives in the median.
    let bin = ((14_100_000.0_f64 - 14_074_000.0) / (96_000.0 / 2048.0) + 1024.0).round() as usize;
    assert_eq!(stats[2][bin], -800);
}

#[tokio::test(start_paused = true)]
async fn disconnects_stalls_and_corruption_become_explicit_gaps() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir.path().join("data"), "passive");
    let radio = SimRadio::new(15);
    radio.faults(|f| {
        f.corrupt_every = Some(10);
        f.garbage_every = Some(7);
    });
    let clock = clock();
    let sid = "0190aaaa-0000-7000-8000-000000000002";
    let store = open_store(&cfg, sid, Mode::Passive, &clock);
    let (_h, stop) = stop_pair();
    let r2 = radio.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(12)).await;
        r2.disconnect_pan();
        tokio::time::sleep(Duration::from_secs(10)).await;
        r2.faults(|f| f.stall_pan = true);
        tokio::time::sleep(Duration::from_secs(6)).await;
        r2.faults(|f| f.stall_pan = false);
        r2.disconnect_pan();
    });
    let mut spec = spectrum(&radio, &clock);
    let summary = run_passive(
        ctx(&cfg, &store, &clock, sid, stop),
        &mut spec,
        None,
        Duration::from_secs(45),
    )
    .await
    .unwrap();
    store.close(clock.now(), &summary.end_reason).await.unwrap();

    assert!(summary.decoder.invalid_checksums > 0);
    assert!(summary.decoder.discarded_bytes > 0);
    let r = DataSet::open(&cfg.storage.root).unwrap().inspect().unwrap();
    assert!(
        r.events_by_kind.get("spectrum_disconnected").copied().unwrap_or(0) >= 1,
        "{:?}",
        r.events_by_kind
    );
    assert!(
        r.events_by_kind.get("spectrum_stalled").copied().unwrap_or(0) >= 1,
        "{:?}",
        r.events_by_kind
    );
    assert!(r.events_by_kind.get("decoder_rejects").copied().unwrap_or(0) >= 1);
    assert!(r.gap_events >= 2, "{r:?}");
    assert!(r.gap_seconds >= 2.0);
    assert!(r.visits_by_status.get("partial").copied().unwrap_or(0) >= 1);
}

#[tokio::test(start_paused = true)]
async fn operator_retune_splits_passive_visits() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir.path().join("data"), "passive");
    let radio = SimRadio::new(15);
    let clock = clock();
    let sid = "0190aaaa-0000-7000-8000-000000000003";
    let store = open_store(&cfg, sid, Mode::Passive, &clock);
    let (_h, stop) = stop_pair();
    let r2 = radio.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(5)).await;
        r2.external_retune(7_074_000);
    });
    let mut spec = spectrum(&radio, &clock);
    let summary = run_passive(
        ctx(&cfg, &store, &clock, sid, stop),
        &mut spec,
        None,
        Duration::from_secs(20),
    )
    .await
    .unwrap();
    store.close(clock.now(), &summary.end_reason).await.unwrap();
    let visits = DataSet::open(&cfg.storage.root).unwrap().visits().unwrap();
    assert_eq!(visits[0].status, "partial");
    assert!(visits[0].flags.contains("geometry_changed"));
    assert_eq!(visits[0].band_id.as_deref(), Some("20m"));
    assert_eq!(visits[1].band_id.as_deref(), Some("40m"));
    assert!(radio.setters_received().is_empty());
}
