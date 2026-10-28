#![allow(clippy::unwrap_used)]

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use common::*;
use k4na_collector::cat::{ControlCat, RetuneConsent};
use k4na_collector::net::Connector;
use k4na_collector::recovery::{self, JournalStatus, Ledger};
use k4na_collector::runtime::stop_pair;
use k4na_collector::spectrum::{SpectrumOptions, StreamSpectrum};
use k4na_collector::survey::{SurveyReport, begin_journal, preflight, run_survey};
use k4na_core::Config;
use k4na_core::config::Mode;
use k4na_core::time::Clock;
use k4na_sim::{RadioModel, SimRadio};
use k4na_store::SpaceProbe;
use k4na_store::reader::DataSet;

const ALLOWED_SETTERS: [&str; 4] = ["BN", "FA", "#SPN", "#FXT"];
/// Receiver measurement setters, sent only when a profile asks for them.
const MEASUREMENT_SETTERS: [&str; 8] = ["PA", "RA", "RG", "GT", "NB", "NR", "#NBL", "#AVG"];

/// The example station and `profile` (with test cadence edits applied).
fn config_with(root: &Path, station_edit: &str, profile: &str) -> Config {
    let station = station(root, "survey").replace("allow_rx_antenna_switch = false", station_edit);
    Config::from_parts(&station, Some(profile)).unwrap()
}

fn fast(profile: &str) -> String {
    profile
        .replace("min_cycle_ms = 300000", "min_cycle_ms = 0")
        .replace("dwell_ms = 5000", "dwell_ms = 3000")
}

struct Run {
    cfg: Config,
    radio: SimRadio,
    report: SurveyReport,
    /// The session's recovery journal, re-read after the run.
    journal: Ledger,
    journal_status: JournalStatus,
    _dir: tempfile::TempDir,
}

async fn control(radio: &SimRadio) -> ControlCat {
    let stream = SimConnector {
        radio: radio.clone(),
        port: Port::Cat,
    }
    .connect()
    .await
    .unwrap();
    ControlCat::new(
        stream,
        Duration::from_millis(500),
        RetuneConsent::new(true, true).unwrap(),
    )
}

/// Runs a survey; `script` may inject operator activity or faults.
async fn survey(radio: SimRadio, secs: u64, script: impl FnOnce(SimRadio) + Send + 'static) -> Run {
    survey_with(radio, secs, None, tracking_config, script).await
}

/// The default survey config, opted in to switching a fixed pan to track
/// (some scenarios start from a fixed pan).
fn tracking_config(root: &Path) -> Config {
    let mut cfg = config(root, "survey");
    cfg.collection.set_pan_track_mode = true;
    cfg
}

async fn survey_with(
    radio: SimRadio,
    secs: u64,
    probe: Option<Box<dyn SpaceProbe>>,
    make_config: impl FnOnce(&Path) -> Config,
    script: impl FnOnce(SimRadio) + Send + 'static,
) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let cfg = make_config(&dir.path().join("data"));
    let clock: Arc<dyn Clock> = clock();
    let mut cat = control(&radio).await;
    let pre = preflight(&mut cat, &cfg).await.unwrap();
    let sid = "0190bbbb-0000-7000-8000-000000000001";
    let store = match probe {
        Some(p) => open_store_with(&cfg, sid, Mode::Survey, &clock, p),
        None => open_store(&cfg, sid, Mode::Survey, &clock),
    };
    let opts = SpectrumOptions {
        stall_timeout: Duration::from_secs(2),
        reconnect_initial: Duration::from_millis(500),
        reconnect_max: Duration::from_secs(4),
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
    let mut txn = begin_journal(&cfg, sid, &pre, clock.clone()).unwrap();
    let journal_path = txn.path().unwrap().to_owned();
    script(radio.clone());
    let report = run_survey(
        ctx(&cfg, &store, &clock, sid, stop),
        &mut spec,
        &mut cat,
        pre,
        &mut txn,
        Duration::from_secs(secs),
    )
    .await
    .unwrap();
    drop(txn);
    store.close(clock.now(), &report.summary.end_reason).await.unwrap();
    let journal = recovery::load(&journal_path).unwrap();
    let journal_status = recovery::list(&cfg.storage.recovery_dir())
        .unwrap()
        .into_iter()
        .find(|j| j.session_id == sid)
        .unwrap()
        .status;
    assert!(
        radio.forbidden_commands().is_empty(),
        "forbidden: {:?}",
        radio.forbidden_commands()
    );
    for s in radio.setters_received() {
        let rx_input = s.starts_with("AR") && cfg.collection.allow_rx_antenna_switch;
        let measuring = MEASUREMENT_SETTERS.iter().any(|p| s.starts_with(p));
        assert!(
            rx_input || measuring || ALLOWED_SETTERS.iter().any(|p| s.starts_with(p)),
            "unexpected setter {s}"
        );
        assert!(!s.starts_with("AN"), "the TX antenna must never be set: {s}");
    }
    Run {
        cfg,
        radio,
        report,
        journal,
        journal_status,
        _dir: dir,
    }
}

/// Applies `act` after `secs` and remembers how many CAT commands the
/// radio had received at that moment.
fn after(
    secs: u64,
    act: impl FnOnce(&SimRadio) + Send + 'static,
) -> (Arc<OnceLock<usize>>, impl FnOnce(SimRadio) + Send + 'static) {
    let mark = Arc::new(OnceLock::new());
    let m = mark.clone();
    let script = move |r: SimRadio| {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(secs)).await;
            act(&r);
            m.set(r.commands().len()).unwrap();
        });
    };
    (mark, script)
}

fn setters_after(radio: &SimRadio, mark: &OnceLock<usize>) -> Vec<String> {
    let idx = *mark.get().expect("script ran");
    radio.commands()[idx..]
        .iter()
        .filter(|c| radio.setters_received().contains(c) && !c.ends_with("TQ;"))
        .cloned()
        .collect()
}

#[tokio::test(start_paused = true)]
async fn survey_covers_every_band_and_restores_the_radio() {
    let radio = SimRadio::new(15);
    radio.faults(|f| f.pan_center_offset_hz = -11_240);
    radio.update(|m| {
        m.fixed = true;
        m.span = 20_000;
    });
    let run = survey(radio, 600, |_| {}).await;
    let r = &run.report;
    assert_eq!(r.halt, "stop", "{r:?}");
    assert!(r.restore.starts_with("restored"), "{}", r.restore);
    assert!(r.cycles >= 1, "{r:?}");
    let m = run.radio.model();
    assert_eq!((m.fa, m.bn, m.span, m.fixed), (14_074_000, 5, 20_000, true));

    let ds = DataSet::open(&run.cfg.storage.root).unwrap();
    let visits = ds.visits().unwrap();
    for band in run.cfg.ranges() {
        let complete: Vec<_> = visits
            .iter()
            .filter(|v| v.band_id.as_deref() == Some(&band.id) && v.status == "complete")
            .collect();
        assert!(!complete.is_empty(), "no complete visits for {}", band.id);
    }
    for v in &visits {
        assert_eq!(v.antenna_id, "log-near-house");
        assert_eq!(v.antenna_port, "rx-ant-in1");
        assert_eq!(v.antenna_observed_port.as_deref(), Some("rx-ant-in1"));
    }
    assert!(!run.radio.setters_received().iter().any(|s| s.starts_with("AR")));
    for v in visits.iter().filter(|v| v.status == "complete") {
        assert_eq!(v.sample_rate_hz, 96_000, "#SPN40000 maps to 96 kHz in the simulator");
        assert!(
            v.actual_low_hz <= v.target_low_hz.unwrap() && v.actual_high_hz >= v.target_high_hz.unwrap(),
            "{v:?}"
        );
        // The pan is offset from the VFO; coverage came from the header.
        assert_eq!(v.center_hz, v.requested_vfo_hz.unwrap() - 11_240);
        assert!(v.settle_discarded > 0);
    }
    let insp = ds.inspect().unwrap();
    assert_eq!(insp.events_by_kind.get("plan_built"), Some(&1));
    assert_eq!(insp.events_by_kind.get("restore_done"), Some(&1));
}

#[tokio::test(start_paused = true)]
async fn transmit_stops_the_survey_without_further_setters() {
    let radio = SimRadio::new(15);
    let (mark, script) = after(40, |r| r.set_transmitting(true));
    let run = survey(radio, 600, script).await;
    assert_eq!(run.report.halt, "conflict");
    assert_eq!(run.report.summary.end_reason, "transmit detected");
    assert!(run.report.restore.starts_with("not restoring"));
    assert_eq!(
        setters_after(&run.radio, &mark),
        Vec::<String>::new(),
        "no setter once transmitting"
    );
    let m = run.radio.model();
    assert!(m.transmitting);
    assert_ne!(m.fa, RadioModel::default().fa, "left where the operator took over");
    let insp = DataSet::open(&run.cfg.storage.root).unwrap().inspect().unwrap();
    assert_eq!(insp.events_by_kind.get("transmit_detected"), Some(&1));
    assert_eq!(insp.events_by_kind.get("restore_skipped"), Some(&1));
    assert!(insp.visits_by_status.get("aborted").copied().unwrap_or(0) >= 1);
}

#[tokio::test(start_paused = true)]
async fn external_retune_is_operator_activity() {
    let radio = SimRadio::new(15);
    let (mark, script) = after(30, |r| r.external_retune(3_600_000));
    let run = survey(radio, 600, script).await;
    assert_eq!(
        setters_after(&run.radio, &mark),
        Vec::<String>::new(),
        "no setter after the operator retuned"
    );
    assert_eq!(run.report.halt, "conflict");
    assert!(
        run.report.summary.end_reason.contains("changed outside the collector"),
        "{}",
        run.report.summary.end_reason
    );
    assert_eq!(run.radio.model().fa, 3_600_000, "operator's frequency is left alone");
    let setters = run.radio.setters_received();
    assert!(!setters.iter().any(|s| s.starts_with("FA00003600000")));
}

#[tokio::test(start_paused = true)]
async fn refused_setter_stops_on_readback_mismatch() {
    let radio = SimRadio::new(15);
    radio.faults(|f| f.refuse_fa = true);
    let run = survey(radio, 600, |_| {}).await;
    assert_eq!(run.report.halt, "conflict");
    assert!(run.report.summary.end_reason.contains("not confirmed by readback"));
    let fa_setters = run
        .radio
        .setters_received()
        .into_iter()
        .filter(|s| s.starts_with("FA"))
        .count();
    assert_eq!(fa_setters, 1, "one attempt, then stop");
}

#[tokio::test(start_paused = true)]
async fn lost_cat_is_unknown_state_and_never_restored() {
    let radio = SimRadio::new(15);
    let run = survey(radio, 600, |r| {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(25)).await;
            r.disconnect_cat();
        });
    })
    .await;
    assert_eq!(run.report.halt, "cat_lost", "{:?}", run.report);
    assert!(run.report.restore.starts_with("not restoring"));
}

#[tokio::test(start_paused = true)]
async fn sample_rate_change_rebuilds_the_plan() {
    let radio = SimRadio::new(15);
    let run = survey(radio, 300, |r| {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            r.faults(|f| f.rate_override = Some(48_000));
        });
    })
    .await;
    assert_eq!(run.report.halt, "stop", "{:?}", run.report);
    let ds = DataSet::open(&run.cfg.storage.root).unwrap();
    assert_eq!(ds.inspect().unwrap().events_by_kind.get("plan_rebuilt"), Some(&1));
    let rates: std::collections::BTreeSet<i64> = ds
        .visits()
        .unwrap()
        .iter()
        .filter(|v| v.status == "complete")
        .map(|v| v.sample_rate_hz)
        .collect();
    assert_eq!(rates, [48_000, 96_000].into());
}

#[tokio::test(start_paused = true)]
async fn preflight_refuses_unsafe_or_unknown_states_without_setters() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir.path().join("data"), "survey");
    for (name, setup) in [
        (
            "split",
            Box::new(|r: &SimRadio| r.update(|m| m.split = true)) as Box<dyn Fn(&SimRadio)>,
        ),
        ("transmitting", Box::new(|r: &SimRadio| r.set_transmitting(true))),
        ("locked", Box::new(|r: &SimRadio| r.update(|m| m.lock = true))),
        ("no TQ", Box::new(|r: &SimRadio| r.faults(|f| f.no_tq = true))),
        ("no RV", Box::new(|r: &SimRadio| r.faults(|f| f.no_revisions = true))),
        ("frozen pan", Box::new(|r: &SimRadio| r.update(|m| m.freeze = true))),
        (
            "fixed pan without opt-in",
            Box::new(|r: &SimRadio| r.update(|m| m.fixed = true)),
        ),
    ] {
        let radio = SimRadio::new(15);
        setup(&radio);
        let mut cat = control(&radio).await;
        let err = preflight(&mut cat, &cfg).await.unwrap_err();
        assert!(err.contains("refused"), "{name}: {err}");
        assert!(radio.setters_received().is_empty(), "{name}");
    }
}

#[tokio::test(start_paused = true)]
async fn freezing_the_pan_mid_survey_is_operator_activity() {
    let radio = SimRadio::new(15);
    let (mark, script) = after(30, |r| r.update(|m| m.freeze = true));
    let run = survey(radio, 600, script).await;
    assert_eq!(run.report.halt, "conflict");
    assert!(
        run.report.summary.end_reason.contains("#FRZ"),
        "{}",
        run.report.summary.end_reason
    );
    assert_eq!(setters_after(&run.radio, &mark), Vec::<String>::new());
}

#[tokio::test(start_paused = true)]
async fn mislabeled_pan_headers_never_count_as_coverage() {
    let radio = SimRadio::new(15);
    radio.faults(|f| f.wrong_pan = true);
    radio.update(|m| m.fixed = true);
    let run = survey(radio, 600, |_| {}).await;
    assert_eq!(run.report.halt, "no_spectrum", "{:?}", run.report);
    assert!(run.report.summary.ignored_frames.get("B").copied().unwrap_or(0) > 0);
    assert!(run.report.restore.starts_with("restored"), "{}", run.report.restore);
    assert!(run.radio.model().fixed, "#FXT restored");
    let insp = DataSet::open(&run.cfg.storage.root).unwrap().inspect().unwrap();
    assert_eq!(insp.visits_total, 0);
}

#[tokio::test(start_paused = true)]
async fn low_disk_mid_survey_stops_and_restores() {
    let free = Arc::new(AtomicU64::new(1 << 40));
    let f2 = free.clone();
    let radio = SimRadio::new(15);
    let run = survey_with(radio, 600, Some(Box::new(FakeDisk(free))), tracking_config, move |_| {
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            // Leave less than the configured headroom.
            f2.store(1 << 20, Ordering::SeqCst);
        });
    })
    .await;
    assert_eq!(run.report.halt, "storage", "{:?}", run.report);
    assert!(run.report.summary.end_reason.contains("storage"));
    assert!(run.report.restore.starts_with("restored"), "{}", run.report.restore);
    assert_eq!(run.radio.model().fa, RadioModel::default().fa);
}

#[tokio::test(start_paused = true)]
async fn per_band_span_track_and_vfo_memories_are_put_back() {
    // As observed on a K4D: BN recalls the band's own span (and so packet
    // rate) and pan mode, alongside the band-stack VFO.
    let radio = SimRadio::new(15);
    radio.update(|m| {
        m.fixed = true;
        m.band_span.insert(3, 92_000);
        m.band_fixed.insert(3, true);
        m.band_stack.insert(3, 7_030_000);
    });
    let run = survey_with(
        radio,
        600,
        None,
        |root| {
            let station = station(root, "survey").replace("set_pan_track_mode = false", "set_pan_track_mode = true");
            Config::from_parts(&station, Some(&fast(HF_FULL))).unwrap()
        },
        |_| {},
    )
    .await;
    let r = &run.report;
    assert_eq!(r.halt, "stop", "{r:?}");
    assert!(r.cycles >= 1, "{r:?}");
    assert!(r.restore.starts_with("restored"), "{}", r.restore);
    let m = run.radio.model();
    assert_eq!((m.bn, m.fa, m.span, m.fixed), (5, 14_074_000, 40_000, true));
    assert_eq!(m.band_span.get(&3), Some(&92_000), "40 m keeps its span");
    assert_eq!(m.band_fixed.get(&3), Some(&true), "40 m keeps its fixed pan");
    assert_eq!(m.band_stack.get(&3), Some(&7_030_000), "40 m keeps its VFO");
    assert!(m.band_span.values().all(|s| *s == 40_000 || *s == 92_000));
    let ds = DataSet::open(&run.cfg.storage.root).unwrap();
    let insp = ds.inspect().unwrap();
    assert_eq!(
        insp.events_by_kind.get("plan_rebuilt"),
        None,
        "span re-applied per band"
    );
    let rates: std::collections::BTreeSet<i64> = ds.visits().unwrap().iter().map(|v| v.sample_rate_hz).collect();
    assert_eq!(rates, [96_000].into());
    assert!(insp.events_by_kind.get("state_changed").copied().unwrap_or(0) >= 8);
}

#[tokio::test(start_paused = true)]
async fn a_recalled_span_outside_the_whitelist_is_reported_not_fatal() {
    // A K4D on R39 stored #SPN5000 for 30 m (K4 band 4), below the
    // documented 6000..=368000 the whitelist allows.
    let radio = SimRadio::new(15);
    radio.update(|m| {
        m.band_span.insert(4, 5_000);
    });
    let run = survey(radio, 600, |_| {}).await;
    let r = &run.report;
    assert_eq!(r.halt, "stop", "{r:?}");
    assert!(r.restore.starts_with("restored"), "{}", r.restore);
    assert!(!run.radio.setters_received().iter().any(|s| s == "#SPN5000;"));
    let m = run.radio.model();
    assert_eq!((m.bn, m.fa, m.span), (5, 14_074_000, 40_000));
    // The span could not be put back, so it is never changed: the band is
    // surveyed at its own span and its memory is left as found.
    assert_eq!(m.band_span.get(&4), Some(&5_000), "30 m memory untouched");
    assert!(run.journal.dirty().is_empty(), "{:?}", run.journal.dirty());
    let ds = DataSet::open(&run.cfg.storage.root).unwrap();
    let insp = ds.inspect().unwrap();
    assert!(insp.events_by_kind.get("warning").copied().unwrap_or(0) >= 1);
    assert!(
        ds.visits()
            .unwrap()
            .iter()
            .any(|v| v.band_id.as_deref() == Some("30m") && v.status == "complete")
    );
}

#[tokio::test(start_paused = true)]
async fn a_band_recalling_a_fixed_pan_without_opt_in_ends_and_restores() {
    let radio = SimRadio::new(15);
    radio.update(|m| {
        m.band_fixed.insert(3, true);
    });
    // Not opted in to switching a fixed pan.
    let run = survey_with(radio, 600, None, |root| config(root, "survey"), |_| {}).await;
    assert_eq!(run.report.halt, "no_spectrum", "{:?}", run.report);
    assert!(
        run.report.summary.end_reason.contains("#FXT1"),
        "{}",
        run.report.summary.end_reason
    );
    assert!(run.report.restore.starts_with("restored"), "{}", run.report.restore);
    let m = run.radio.model();
    assert_eq!((m.bn, m.fa, m.fixed), (5, 14_074_000, false));
    assert!(!run.radio.setters_received().iter().any(|s| s.starts_with("#FXT0")));
}

fn mean(v: &[i16]) -> f64 {
    v.iter().map(|x| f64::from(*x)).sum::<f64>() / v.len() as f64
}

#[tokio::test(start_paused = true)]
async fn two_antennas_are_surveyed_band_by_band_and_inputs_put_back() {
    let radio = SimRadio::new(15);
    let run = survey_with(
        radio,
        600,
        None,
        |root| config_with(root, "allow_rx_antenna_switch = true", &fast(RX_VS_DXC)),
        |_| {},
    )
    .await;
    let r = &run.report;
    assert_eq!(r.halt, "stop", "{r:?}");
    assert!(r.cycles >= 2, "{r:?}");
    assert_eq!(r.antenna_skips, 0);
    // Only the reviewed RX inputs were selected: ANT2 via the ATU (AR6)
    // and back to RX ANT IN1 (AR4). AN is never sent (checked above).
    let ar: std::collections::BTreeSet<_> = run
        .radio
        .setters_received()
        .into_iter()
        .filter(|s| s.starts_with("AR"))
        .collect();
    assert_eq!(ar, ["AR4;".to_owned(), "AR6;".to_owned()].into());
    // Every band's input, and the current one, are as found.
    let m = run.radio.model();
    assert_eq!((m.rx_input, m.tx_antenna, m.fa, m.bn), (4, 1, 14_074_000, 5));
    assert!(m.band_rx_input.values().all(|v| *v == 4), "{:?}", m.band_rx_input);

    let ds = DataSet::open(&run.cfg.storage.root).unwrap();
    let visits = ds.visits().unwrap();
    let insp = ds.inspect().unwrap();
    for band in ["40m", "20m"] {
        for (antenna, port) in [("log-near-house", "rx-ant-in1"), ("dxc", "ant2")] {
            let cov = insp
                .bands
                .iter()
                .find(|b| b.band_id == band && b.antenna_id == antenna)
                .unwrap_or_else(|| panic!("no coverage row for {band} on {antenna}"));
            assert!(cov.complete_visits > 0, "{cov:?}");
            assert_eq!(cov.covered_hz, cov.target_hz, "{cov:?}");
            assert!(
                visits
                    .iter()
                    .filter(|v| v.antenna_id == antenna)
                    .all(|v| v.antenna_observed_port.as_deref() == Some(port)),
                "{antenna}"
            );
        }
    }
    // Within a band, all tiles on one antenna come before the next.
    let order: Vec<(String, String)> = visits
        .iter()
        .map(|v| (v.band_id.clone().unwrap(), v.antenna_id.clone()))
        .collect();
    let mut runs = order.clone();
    runs.dedup();
    assert!(runs.len() <= 4 * (r.cycles as usize + 1), "{runs:?}");
    // Different antennas are different segments and different data: the
    // simulator's ANT2 floor is 6 dB (60 native tenths) higher.
    let level = |antenna: &str| {
        let v = visits
            .iter()
            .find(|v| v.antenna_id == antenna && v.status == "complete" && v.band_id.as_deref() == Some("40m"))
            .unwrap();
        mean(&ds.summary(&v.visit_id).unwrap().unwrap()[0])
    };
    let delta = level("dxc") - level("log-near-house");
    assert!((delta - 60.0).abs() < 15.0, "delta {delta}");
    let segments: std::collections::BTreeSet<_> = visits.iter().map(|v| (&v.segment_id, &v.antenna_id)).collect();
    let seg_ids: std::collections::BTreeSet<_> = segments.iter().map(|(s, _)| *s).collect();
    assert_eq!(seg_ids.len(), segments.len(), "a segment never mixes antennas");
    assert!(insp.events_by_kind.get("antenna_selected").copied().unwrap_or(0) >= 4);
}

#[tokio::test(start_paused = true)]
async fn a_band_recalling_another_input_is_skipped_without_switch_consent() {
    let radio = SimRadio::new(15);
    // 40 m (K4 band 3) recalls ANT2 when selected.
    radio.update(|m| {
        m.band_rx_input.insert(3, 6);
    });
    let run = survey(radio, 600, |_| {}).await;
    let r = &run.report;
    assert_eq!(r.halt, "stop", "{r:?}");
    assert!(r.antenna_skips > 0, "{r:?}");
    assert!(!run.radio.setters_received().iter().any(|s| s.starts_with("AR")));
    let ds = DataSet::open(&run.cfg.storage.root).unwrap();
    let visits = ds.visits().unwrap();
    assert!(
        !visits.iter().any(|v| v.band_id.as_deref() == Some("40m")),
        "nothing is attributed on the wrong input"
    );
    assert!(
        visits
            .iter()
            .any(|v| v.band_id.as_deref() == Some("20m") && v.status == "complete")
    );
    let insp = ds.inspect().unwrap();
    assert!(insp.events_by_kind.get("antenna_mismatch").copied().unwrap_or(0) > 0);
    // The radio's own per-band memory was not touched.
    assert_eq!(run.radio.model().band_rx_input.get(&3), Some(&6));
}

#[tokio::test(start_paused = true)]
async fn a_band_recalling_another_input_is_selected_and_put_back_with_consent() {
    let radio = SimRadio::new(15);
    radio.update(|m| {
        m.band_rx_input.insert(3, 6);
    });
    let run = survey_with(
        radio,
        600,
        None,
        |root| config_with(root, "allow_rx_antenna_switch = true", &fast(HF_FULL)),
        |_| {},
    )
    .await;
    assert_eq!(run.report.halt, "stop", "{:?}", run.report);
    assert_eq!(run.report.antenna_skips, 0);
    let ds = DataSet::open(&run.cfg.storage.root).unwrap();
    let forty: Vec<_> = ds
        .visits()
        .unwrap()
        .into_iter()
        .filter(|v| v.band_id.as_deref() == Some("40m"))
        .collect();
    assert!(forty.iter().any(|v| v.status == "complete"));
    assert!(
        forty
            .iter()
            .all(|v| v.antenna_observed_port.as_deref() == Some("rx-ant-in1"))
    );
    let m = run.radio.model();
    assert_eq!(m.band_rx_input.get(&3), Some(&6), "40 m keeps the operator's ANT2");
    assert_eq!(m.rx_input, 4);
}

#[tokio::test(start_paused = true)]
async fn changing_the_antenna_mid_survey_is_operator_activity() {
    let radio = SimRadio::new(15);
    let (mark, script) = after(30, |r| r.update(|m| m.rx_input = 6));
    let run = survey(radio, 600, script).await;
    assert_eq!(run.report.halt, "conflict", "{:?}", run.report);
    assert!(
        run.report.summary.end_reason.contains("AR"),
        "{}",
        run.report.summary.end_reason
    );
    assert_eq!(setters_after(&run.radio, &mark), Vec::<String>::new());
    assert_eq!(run.radio.model().rx_input, 6, "left as the operator set it");
}

#[tokio::test(start_paused = true)]
async fn a_missing_atu_input_stops_on_readback_mismatch() {
    let radio = SimRadio::new(15);
    radio.update(|m| m.atu = false);
    let run = survey_with(
        radio,
        600,
        None,
        |root| config_with(root, "allow_rx_antenna_switch = true", &fast(RX_VS_DXC)),
        |_| {},
    )
    .await;
    assert_eq!(run.report.halt, "conflict", "{:?}", run.report);
    assert!(
        run.report.summary.end_reason.contains("AR6"),
        "{}",
        run.report.summary.end_reason
    );
    let ds = DataSet::open(&run.cfg.storage.root).unwrap();
    assert!(!ds.visits().unwrap().iter().any(|v| v.antenna_id == "dxc"));
}

#[tokio::test(start_paused = true)]
async fn cycles_start_at_most_every_min_cycle_ms() {
    let radio = SimRadio::new(15);
    // One 30 m tile (~4 s of work) per cycle, cycles every 60 s.
    let profile = r#"
id = "cadence"
antennas = ["log-near-house"]
dwell_ms = 3000
min_cycle_ms = 60000
[[ranges]]
id = "30m"
low_hz = 10100000
high_hz = 10150000
"#;
    let run = survey_with(
        radio,
        250,
        None,
        |root| config_with(root, "allow_rx_antenna_switch = false", profile),
        |_| {},
    )
    .await;
    let r = &run.report;
    assert_eq!(r.halt, "stop", "{r:?}");
    // Cycles start at ~0, 60, 120, 180 and 240 s; each finishes ~4 s later.
    assert!((4..=5).contains(&r.cycles), "{r:?}");
    assert!(r.idle_s > 150.0, "{r:?}");
    assert!(r.last_cycle_s.unwrap() < 10.0, "{r:?}");
    let ds = DataSet::open(&run.cfg.storage.root).unwrap();
    let starts: Vec<i64> = ds.visits().unwrap().iter().map(|v| v.started_utc_ns).collect();
    assert!((4..=5).contains(&starts.len()), "{starts:?}");
    for w in starts.windows(2) {
        let gap = (w[1] - w[0]) as f64 / 1e9;
        assert!((59.0..62.0).contains(&gap), "visits {gap} s apart");
    }
    let insp = ds.inspect().unwrap();
    assert_eq!(insp.events_by_kind.get("cycle_complete").copied(), Some(r.cycles));
    // Holding puts the band's own FA back once, then retunes nothing.
    let fa = run
        .radio
        .setters_received()
        .iter()
        .filter(|s| s.starts_with("FA"))
        .count();
    assert!(fa <= 2 * starts.len() + 3, "{fa} FA setters");
}

#[tokio::test(start_paused = true)]
async fn operator_activity_while_holding_between_cycles_stops_the_survey() {
    let radio = SimRadio::new(15);
    let profile = r#"
id = "cadence"
antennas = ["log-near-house"]
dwell_ms = 3000
min_cycle_ms = 120000
[[ranges]]
id = "30m"
low_hz = 10100000
high_hz = 10150000
"#;
    let (mark, script) = after(40, |r| r.external_retune(10_120_000));
    let run = survey_with(
        radio,
        600,
        None,
        |root| config_with(root, "allow_rx_antenna_switch = false", profile),
        script,
    )
    .await;
    assert_eq!(run.report.halt, "conflict", "{:?}", run.report);
    assert_eq!(setters_after(&run.radio, &mark), Vec::<String>::new());
    assert_eq!(run.radio.model().fa, 10_120_000);
}

#[tokio::test(start_paused = true)]
async fn a_survey_killed_mid_run_is_restored_from_its_journal_alone() {
    let radio = SimRadio::new(15);
    radio.update(|m| {
        m.rx_scope = k4na_sim::RxScope::BandInput;
        m.rx = operator_rx();
    });
    let before = radio.model();
    let dir = tempfile::tempdir().unwrap();
    let profile = two_band_profile(r#""log-near-house", "dxc""#, "");
    let cfg = config_with(&dir.path().join("data"), "allow_rx_antenna_switch = true", &profile);
    let clock: Arc<dyn Clock> = clock();
    let mut cat = control(&radio).await;
    let pre = preflight(&mut cat, &cfg).await.unwrap();
    let sid = "0190bbbb-0000-7000-8000-00000000000a";
    let store = open_store(&cfg, sid, Mode::Survey, &clock);
    let mut spec = StreamSpectrum::new(
        SimConnector {
            radio: radio.clone(),
            port: Port::Pan,
        },
        SpectrumOptions::from_config(&cfg.radio),
        clock.clone(),
    );
    let (_h, stop) = stop_pair();
    let mut txn = begin_journal(&cfg, sid, &pre, clock.clone()).unwrap();
    // "Kill" the process once the survey has moved to the second antenna:
    // the future is dropped, so no restoration code runs.
    let killed = tokio::time::timeout(Duration::from_secs(60), async {
        run_survey(
            ctx(&cfg, &store, &clock, sid, stop),
            &mut spec,
            &mut cat,
            pre,
            &mut txn,
            Duration::from_secs(600),
        )
        .await
    })
    .await;
    assert!(killed.is_err(), "the survey was still running");
    drop(txn);
    drop(cat);
    let changed = radio.model();
    assert!(
        (changed.bn, changed.fa, changed.rx) != (before.bn, before.fa, before.rx) || changed.rx_input != 4,
        "the kill happened mid-survey"
    );

    let dir = cfg.storage.recovery_dir();
    let info = recovery::list(&dir).unwrap().remove(0);
    assert_eq!(info.status, JournalStatus::Pending);
    assert!(info.dirty > 0);
    // The dry run is GET-only.
    let mut cat = control(&radio).await;
    let setters = radio.setters_received().len();
    let plan = recovery::plan(&recovery::load(&info.path).unwrap(), &mut cat)
        .await
        .unwrap();
    assert!(plan.dry_run && !plan.fields.is_empty(), "{plan:?}");
    assert_eq!(radio.setters_received().len(), setters, "dry run sent a setter");

    let mut txn = recovery::Transaction::resume(&info.path, clock.clone()).unwrap();
    let report = recovery::restore(&mut txn, &mut cat).await;
    assert_eq!(report.outcome, recovery::Outcome::Complete, "{report:?}");
    drop(txn);
    let m = radio.model();
    assert_eq!(
        (m.bn, m.fa, m.span, m.rx_input, m.rx),
        (before.bn, before.fa, before.span, 4, operator_rx())
    );
    for band in [3, 5] {
        for input in [4, 6] {
            assert_eq!(m.rx_settings(band, input), operator_rx(), "band {band} input {input}");
        }
        assert_eq!(m.band_rx_input.get(&band).copied().unwrap_or(4), 4);
    }
    assert_eq!(recovery::list(&dir).unwrap()[0].status, JournalStatus::Resolved);
    // A second restore is refused as already resolved by the CLI; the
    // engine itself is idempotent.
    let mut txn = recovery::Transaction::resume(&info.path, clock).unwrap();
    assert_eq!(
        recovery::restore(&mut txn, &mut cat).await.outcome,
        recovery::Outcome::Complete
    );
    assert!(radio.forbidden_commands().is_empty());
}

/// Two short bands; `extra` is appended (antennas, `[measurement]`).
fn two_band_profile(antennas: &str, extra: &str) -> String {
    format!(
        r#"
id = "measure"
antennas = [{antennas}]
dwell_ms = 3000
min_cycle_ms = 0
[[ranges]]
id = "40m"
low_hz = 7000000
high_hz = 7060000
[[ranges]]
id = "20m"
low_hz = 14000000
high_hz = 14060000
{extra}
"#
    )
}

/// The operator's receiver state, unlike the built-in defaults (RG0, NB
/// off): every survey must change it, then put it back.
fn operator_rx() -> k4na_sim::RxSettings {
    k4na_sim::RxSettings {
        rf_gain_db: 10,
        nb_on: true,
        ..Default::default()
    }
}

fn measurement_of(v: &k4na_store::reader::VisitRow) -> std::collections::BTreeMap<String, String> {
    serde_json::from_str(v.measurement_json.as_deref().expect("survey visits record settings")).unwrap()
}

#[tokio::test(start_paused = true)]
async fn measurement_settings_are_applied_recorded_and_put_back() {
    let radio = SimRadio::new(15);
    radio.update(|m| m.rx = operator_rx());
    let profile = two_band_profile(r#""log-near-house""#, "[measurement]\npreamp = \"off\"\n");
    let run = survey_with(
        radio,
        120,
        None,
        |root| config_with(root, "allow_rx_antenna_switch = false", &profile),
        |_| {},
    )
    .await;
    let r = &run.report;
    assert_eq!((r.halt.as_str(), r.measurement_skips), ("stop", 0), "{r:?}");
    assert_eq!(r.restore_outcome, "complete", "{r:?}");
    assert!(r.manual.is_empty());
    let setters = run.radio.setters_received();
    for s in ["PA0;", "RG-00;", "NB0;"] {
        assert!(setters.contains(&s.to_owned()), "{s} not in {setters:?}");
    }
    // Every band's memory, and the current state, are as the operator left
    // them.
    let m = run.radio.model();
    assert_eq!(m.rx, operator_rx());
    for band in [3, 5] {
        assert_eq!(m.rx_settings(band, 4), operator_rx(), "band {band}");
    }
    assert_eq!(run.journal_status, JournalStatus::Resolved);
    assert!(run.journal.dirty().is_empty());

    let ds = DataSet::open(&run.cfg.storage.root).unwrap();
    let complete: Vec<_> = ds
        .visits()
        .unwrap()
        .into_iter()
        .filter(|v| v.status == "complete")
        .collect();
    assert!(!complete.is_empty());
    for v in &complete {
        let s = measurement_of(v);
        assert_eq!(s["preamp"], "off", "{s:?}");
        assert_eq!(s["rf_gain_db"], "0", "{s:?}");
        assert_eq!(s["cat_meta_mode"], "basic", "{s:?}");
    }
    let insp = ds.inspect().unwrap();
    assert!(insp.events_by_kind.get("measurement_applied").copied().unwrap_or(0) >= 2);
}

#[tokio::test(start_paused = true)]
async fn per_input_receiver_memories_are_put_back_before_switching_antennas() {
    let radio = SimRadio::new(15);
    radio.update(|m| {
        m.rx_scope = k4na_sim::RxScope::BandInput;
        m.rx = operator_rx();
    });
    let extra = "[[measurement.override]]\nantenna = \"dxc\"\npreamp = \"off\"\n";
    let profile = two_band_profile(r#""log-near-house", "dxc""#, extra);
    let run = survey_with(
        radio,
        240,
        None,
        |root| config_with(root, "allow_rx_antenna_switch = true", &profile),
        |_| {},
    )
    .await;
    let r = &run.report;
    assert_eq!(
        (r.halt.as_str(), r.restore_outcome.as_str()),
        ("stop", "complete"),
        "{r:?}"
    );
    let m = run.radio.model();
    assert_eq!((m.rx_input, m.rx), (4, operator_rx()));
    for band in [3, 5] {
        for input in [4, 6] {
            assert_eq!(m.rx_settings(band, input), operator_rx(), "band {band} input {input}");
        }
    }
    let ds = DataSet::open(&run.cfg.storage.root).unwrap();
    for v in ds.visits().unwrap().iter().filter(|v| v.status == "complete") {
        let want = if v.antenna_id == "dxc" { "off" } else { "as-found" };
        assert_eq!(measurement_of(v)["preamp"], want, "{}", v.antenna_id);
    }
}

#[tokio::test(start_paused = true)]
async fn a_setting_the_cat_mode_cannot_express_skips_only_that_band() {
    let radio = SimRadio::new(15);
    // Basic CAT mode cannot set an attenuator level.
    let extra = "[[measurement.override]]\nband = \"40m\"\nattenuator_db = 6\n";
    let profile = two_band_profile(r#""log-near-house""#, extra);
    let run = survey_with(
        radio,
        120,
        None,
        |root| config_with(root, "allow_rx_antenna_switch = false", &profile),
        |_| {},
    )
    .await;
    let r = &run.report;
    assert_eq!(r.halt, "stop", "{r:?}");
    assert!(r.measurement_skips > 0, "{r:?}");
    assert!(!run.radio.setters_received().iter().any(|s| s.starts_with("RA")));
    let ds = DataSet::open(&run.cfg.storage.root).unwrap();
    let visits = ds.visits().unwrap();
    assert!(!visits.iter().any(|v| v.band_id.as_deref() == Some("40m")));
    assert!(
        visits
            .iter()
            .any(|v| v.band_id.as_deref() == Some("20m") && v.status == "complete")
    );
    assert_eq!(run.journal_status, JournalStatus::Resolved);
}

#[tokio::test(start_paused = true)]
async fn an_operator_change_is_left_alone_and_the_journal_blocks_until_settled() {
    let radio = SimRadio::new(15);
    radio.update(|m| m.rx = operator_rx());
    // The operator turns RF gain down right after the survey set it.
    let script = |r: SimRadio| {
        tokio::spawn(async move {
            while !r.setters_received().iter().any(|s| s == "RG-00;") {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            r.update(|m| m.rx.rf_gain_db = 20);
        });
    };
    let profile = two_band_profile(r#""log-near-house""#, "");
    let run = survey_with(
        radio,
        120,
        None,
        |root| config_with(root, "allow_rx_antenna_switch = false", &profile),
        script,
    )
    .await;
    let r = &run.report;
    assert_eq!(
        (r.halt.as_str(), r.restore_outcome.as_str()),
        ("conflict", "not_restored"),
        "{r:?}"
    );
    assert!(r.manual.iter().any(|m| m.contains("RF gain")), "{:?}", r.manual);
    assert_eq!(run.journal_status, JournalStatus::Pending);
    let dir = run.cfg.storage.recovery_dir();
    let clock: Arc<dyn Clock> = clock();

    // A new survey is refused while the journal is unresolved.
    let mut cat = control(&run.radio).await;
    let pre = preflight(&mut cat, &run.cfg).await.unwrap();
    let next = "0190bbbb-0000-7000-8000-000000000002";
    let err = begin_journal(&run.cfg, next, &pre, clock.clone()).unwrap_err();
    assert!(err.to_string().contains("unresolved"), "{err}");

    // Recovery restores what is still ours and leaves the operator's RF
    // gain alone.
    let info = recovery::list(&dir).unwrap().remove(0);
    let mut txn = recovery::Transaction::resume(&info.path, clock.clone()).unwrap();
    let report = recovery::restore(&mut txn, &mut cat).await;
    assert_eq!(report.outcome, recovery::Outcome::Conflict, "{report:?}");
    // The change was made on 40 m (K4 band 3), surveyed first; back on the
    // original band the radio recalls that band's own settings.
    let m = run.radio.model();
    let forty = m.rx_settings(3, 4);
    assert_eq!(forty.rf_gain_db, 20, "the operator's value is never overwritten");
    assert!(forty.nb_on, "the collector's NB change was put back");
    assert_eq!((m.bn, m.fa, m.rx), (5, 14_074_000, operator_rx()));
    drop(txn);
    assert_eq!(recovery::list(&dir).unwrap()[0].status, JournalStatus::Pending);

    // The operator accepts the radio as it is; surveys may start again.
    let mut txn = recovery::Transaction::resume(&info.path, clock.clone()).unwrap();
    txn.record(recovery::Entry::Acknowledged { note: "test".into() })
        .unwrap();
    drop(txn);
    assert_eq!(recovery::list(&dir).unwrap()[0].status, JournalStatus::Acknowledged);
    begin_journal(&run.cfg, next, &pre, clock).unwrap();
}
