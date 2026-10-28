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
        let sub = cfg.collection.allow_sub_receiver && (s.starts_with("SB") || s.starts_with("FB"));
        assert!(
            rx_input || measuring || sub || ALLOWED_SETTERS.iter().any(|p| s.starts_with(p)),
            "unexpected setter {s}"
        );
        assert!(!s.starts_with("AN"), "the TX antenna must never be set: {s}");
        for p in ["BI", "DV", "FT", "BN$"] {
            assert!(!s.starts_with(p), "{p} must never be set: {s}");
        }
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
    // The changes are journaled and block new surveys; once CAT is back,
    // an explicit recovery restores them.
    assert_eq!(run.journal_status, JournalStatus::Pending);
    assert_eq!(run.report.restore_outcome, "not_restored");
    assert!(!run.report.manual.is_empty());
    let info = recovery::list(&run.cfg.storage.recovery_dir()).unwrap().remove(0);
    let mut cat = control(&run.radio).await;
    let mut txn = recovery::Transaction::resume(&info.path, clock()).unwrap();
    let report = recovery::restore(&mut txn, &mut cat).await;
    assert_eq!(report.outcome, recovery::Outcome::Complete, "{report:?}");
    let m = run.radio.model();
    assert_eq!((m.bn, m.fa), (5, RadioModel::default().fa));
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
    // Journals are outside the quota: storage pressure never prevents
    // journaling the restoration.
    assert_eq!(run.journal_status, JournalStatus::Resolved);
    assert!(run.journal.dirty().is_empty());
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

/// The example station opted in to paired surveys.
fn paired_config(root: &Path, profile: &str) -> Config {
    let station = station(root, "survey")
        .replace("allow_rx_antenna_switch = false", "allow_rx_antenna_switch = true")
        .replace("allow_sub_receiver = false", "allow_sub_receiver = true");
    Config::from_parts(&station, Some(profile)).unwrap()
}

/// The sub receiver as found: off, VFO B elsewhere in the band, its pan
/// centered off `FB` by a different amount than pan A off `FA`, and reading
/// 3 dB high (a receiver difference the swap must separate out).
fn paired_radio() -> SimRadio {
    let radio = SimRadio::new(15);
    radio.faults(|f| {
        f.pan_center_offset_hz = -11_240;
        f.sub_pan_center_offset_hz = 7_000;
    });
    radio.update(|m| {
        m.fb = 14_060_000;
        m.sub_bias = 30;
    });
    radio
}

#[tokio::test(start_paused = true)]
async fn paired_visits_measure_both_antennas_at_once_and_put_the_sub_back() {
    let before = paired_radio().model();
    let run = survey_with(
        paired_radio(),
        600,
        None,
        |root| paired_config(root, &fast(RX_VS_DXC_PAIRED)),
        |_| {},
    )
    .await;
    let r = &run.report;
    assert_eq!(r.halt, "stop", "{r:?}");
    assert!(r.restore.starts_with("restored"), "{}", r.restore);
    assert!(r.cycles >= 2, "{r:?}");
    assert!(r.pairs > 0, "{r:?}");
    assert_eq!(r.coverage_unverified, 0, "each pan's offset is learned: {r:?}");
    assert!(r.restore.contains("SB0"), "{}", r.restore);
    assert_eq!((r.antenna_skips, r.measurement_skips), (0, 0), "{r:?}");

    // Everything the survey touched is as found: the sub is off again and
    // VFO B, the sub input and every band's sub memory are back.
    let m = run.radio.model();
    assert!(!m.sub_rx);
    assert_eq!((m.fa, m.fb, m.bn), (before.fa, before.fb, before.bn));
    assert_eq!((m.rx_input, m.sub_input, m.tx_antenna), (4, 4, 1));
    assert!(m.band_rx_input.values().all(|v| *v == 4), "{:?}", m.band_rx_input);
    assert!(m.band_sub.values().all(|s| s.2 == 4), "{:?}", m.band_sub);
    assert!(!m.split && !m.diversity && !m.band_independent);
    let setters = run.radio.setters_received();
    assert!(setters.contains(&"SB1;".to_owned()) && setters.last() != Some(&"SB1;".to_owned()));
    let inputs: std::collections::BTreeSet<_> = setters.iter().filter(|s| s.starts_with("AR")).collect();
    assert!(
        inputs
            .iter()
            .all(|s| ["AR4;", "AR6;", "AR$4;", "AR$6;"].contains(&s.as_str())),
        "{inputs:?}"
    );

    let ds = DataSet::open(&run.cfg.storage.root).unwrap();
    let visits = ds.visits().unwrap();
    let by_id: std::collections::BTreeMap<_, _> = visits.iter().map(|v| (v.visit_id.as_str(), v)).collect();
    let mut orientations = std::collections::BTreeSet::new();
    let mut pairs = 0;
    for v in visits.iter().filter(|v| v.pan == "A") {
        let p = v.pair().unwrap_or_else(|| panic!("unpaired pan A visit {v:?}"));
        let peer = by_id[p.peer_visit_id.as_str()];
        let back = peer.pair().unwrap();
        assert_eq!(
            (peer.pan.as_str(), back.peer_visit_id.as_str()),
            ("B", v.visit_id.as_str())
        );
        assert_eq!((&back.pair_id, back.orientation), (&p.pair_id, p.orientation));
        assert_eq!(p.peer_antenna.id, peer.antenna_id);
        assert_ne!(v.antenna_id, peer.antenna_id, "a pair measures two antennas");
        // Same dwell window and same tile.
        assert_eq!((v.started_utc_ns, &v.tile_id), (peer.started_utc_ns, &peer.tile_id));
        let main_antenna = if p.orientation == 0 { "log-near-house" } else { "dxc" };
        assert_eq!(v.antenna_id, main_antenna);
        for (x, port) in [(v, &v.antenna_port), (peer, &peer.antenna_port)] {
            assert_eq!(x.antenna_observed_port.as_ref(), Some(port), "{x:?}");
        }
        if v.status == "complete" && peer.status == "complete" {
            pairs += 1;
            // Each pan's coverage came from its own headers and learned offset.
            assert_eq!(v.center_hz, v.requested_vfo_hz.unwrap() - 11_240);
            assert_eq!(peer.center_hz, peer.requested_vfo_hz.unwrap() + 7_000);
            for x in [v, peer] {
                assert!(x.actual_low_hz <= x.target_low_hz.unwrap() && x.actual_high_hz >= x.target_high_hz.unwrap());
            }
            assert!(
                peer.measurement_json
                    .as_deref()
                    .unwrap_or("")
                    .contains(r#""receiver":"sub""#),
                "{peer:?}"
            );
        }
        orientations.insert(p.orientation);
    }
    assert!(pairs > 0);
    assert_eq!(
        orientations,
        [0, 1].into(),
        "swap exchanges the receivers every other cycle"
    );
    assert_eq!(visits.iter().filter(|v| v.pan == "B").count(), visits.len() / 2);
    let segments: std::collections::BTreeSet<_> = visits.iter().map(|v| (&v.segment_id, &v.pan)).collect();
    let seg_ids: std::collections::BTreeSet<_> = segments.iter().map(|(s, _)| *s).collect();
    assert_eq!(seg_ids.len(), segments.len(), "a segment never mixes receivers");
}

#[tokio::test(start_paused = true)]
async fn paired_preflight_refuses_without_krx4_band_independence_off_or_consent() {
    let dir = tempfile::tempdir().unwrap();
    let profile = fast(RX_VS_DXC_PAIRED);
    for (edit, consent, needle) in [
        (
            (|m: &mut RadioModel| m.krx4 = false) as fn(&mut RadioModel),
            true,
            "no KRX4",
        ),
        (|m: &mut RadioModel| m.band_independent = true, true, "BI1"),
        (|m: &mut RadioModel| m.split = true, true, "FT"),
        (|_: &mut RadioModel| {}, false, "allow_sub_receiver"),
    ] {
        let radio = paired_radio();
        radio.update(edit);
        let root = dir.path().join("data");
        let station =
            station(&root, "survey").replace("allow_rx_antenna_switch = false", "allow_rx_antenna_switch = true");
        let station = if consent {
            station.replace("allow_sub_receiver = false", "allow_sub_receiver = true")
        } else {
            station
        };
        let problem = match Config::from_parts(&station, Some(&profile)) {
            Err(e) => e.to_string(),
            Ok(cfg) => preflight(&mut control(&radio).await, &cfg).await.unwrap_err(),
        };
        assert!(problem.contains(needle), "{needle}: {problem}");
        assert!(radio.setters_received().is_empty(), "{:?}", radio.setters_received());
    }
}

#[tokio::test(start_paused = true)]
async fn a_sub_retune_during_a_paired_survey_is_operator_activity() {
    let (mark, script) = after(20, |r| r.update(|m| m.fb = 14_200_000));
    let run = survey_with(
        paired_radio(),
        600,
        None,
        |root| paired_config(root, &fast(RX_VS_DXC_PAIRED)),
        script,
    )
    .await;
    let r = &run.report;
    assert_eq!(r.halt, "conflict", "{r:?}");
    assert!(r.summary.end_reason.contains("FB"), "{}", r.summary.end_reason);
    assert!(r.restore.starts_with("not restoring"), "{}", r.restore);
    // The operator's VFO B is theirs: never overwritten after the change.
    assert!(
        !setters_after(&run.radio, &mark).iter().any(|s| s.starts_with("FB")),
        "{:?}",
        setters_after(&run.radio, &mark)
    );
    assert_eq!(run.radio.model().fb, 14_200_000);
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

/// A two-band, two-antenna radio with per-band-and-input receiver
/// memories, 30 m-style surprises left out.
fn recovery_radio() -> SimRadio {
    let radio = SimRadio::new(15);
    radio.update(|m| {
        m.rx_scope = k4na_sim::RxScope::BandInput;
        m.rx = operator_rx();
    });
    radio
}

struct Interrupted {
    cfg: Config,
    clock: Arc<dyn Clock>,
    /// `None` when the survey was killed (its future dropped).
    report: Option<SurveyReport>,
    _dir: tempfile::TempDir,
}

/// Runs a two-antenna survey that is killed after `kill_after` (the
/// future is dropped: no restoration code runs, as with SIGKILL), or
/// cancelled through its stop handle after `cancel_after`.
async fn interrupted_survey(radio: &SimRadio, kill_after: Duration, cancel_after: Option<Duration>) -> Interrupted {
    let profile = two_band_profile(r#""log-near-house", "dxc""#, "");
    interrupted_with(radio, kill_after, cancel_after, |root| {
        config_with(root, "allow_rx_antenna_switch = true", &profile)
    })
    .await
}

async fn interrupted_with(
    radio: &SimRadio,
    kill_after: Duration,
    cancel_after: Option<Duration>,
    make_config: impl FnOnce(&Path) -> Config,
) -> Interrupted {
    let dir = tempfile::tempdir().unwrap();
    let cfg = make_config(&dir.path().join("data"));
    let clock: Arc<dyn Clock> = clock();
    let mut cat = control(radio).await;
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
    let (handle, stop) = stop_pair();
    if let Some(after) = cancel_after {
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            handle.stop("signal");
        });
    } else {
        std::mem::forget(handle);
    }
    let mut txn = begin_journal(&cfg, sid, &pre, clock.clone()).unwrap();
    let report = tokio::time::timeout(kill_after, async {
        run_survey(
            ctx(&cfg, &store, &clock, sid, stop),
            &mut spec,
            &mut cat,
            pre,
            &mut txn,
            Duration::from_secs(600),
        )
        .await
        .unwrap()
    })
    .await
    .ok();
    Interrupted {
        cfg,
        clock,
        report,
        _dir: dir,
    }
}

/// The only journal in the run's recovery directory.
fn journal_info(cfg: &Config) -> recovery::JournalInfo {
    let mut all = recovery::list(&cfg.storage.recovery_dir()).unwrap();
    assert_eq!(all.len(), 1, "{all:?}");
    all.remove(0)
}

/// `k4na recovery restore`, in-process, on a fresh CAT connection.
async fn recover(run: &Interrupted, radio: &SimRadio) -> recovery::RestoreReport {
    let info = journal_info(&run.cfg);
    let mut cat = control(radio).await;
    let mut txn = recovery::Transaction::resume(&info.path, run.clock.clone()).unwrap();
    recovery::restore(&mut txn, &mut cat).await
}

/// Every memory the survey could touch is as the operator left it.
fn assert_as_found(radio: &SimRadio, before: &RadioModel) {
    let m = radio.model();
    assert_eq!(
        (m.bn, m.fa, m.span, m.fixed, m.rx_input, m.tx_antenna, m.rx),
        (
            before.bn,
            before.fa,
            before.span,
            before.fixed,
            before.rx_input,
            before.tx_antenna,
            before.rx
        )
    );
    for band in [3, 5] {
        for input in [4, 6] {
            assert_eq!(m.rx_settings(band, input), before.rx, "band {band} input {input}");
        }
        assert_eq!(m.band_rx_input.get(&band).copied().unwrap_or(4), 4, "band {band} input");
        if let Some(span) = before.band_span.get(&band) {
            assert_eq!(m.band_span.get(&band), Some(span), "band {band} span");
        }
    }
    assert!(radio.forbidden_commands().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_survey_killed_mid_run_is_restored_from_its_journal_alone() {
    let radio = recovery_radio();
    let before = radio.model();
    // Killed once the survey has moved to the second antenna.
    let run = interrupted_survey(&radio, Duration::from_secs(60), None).await;
    assert!(run.report.is_none(), "the survey was still running");
    let changed = radio.model();
    assert!(
        (changed.bn, changed.fa, changed.rx) != (before.bn, before.fa, before.rx) || changed.rx_input != 4,
        "the kill happened mid-survey"
    );

    let info = journal_info(&run.cfg);
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

    let report = recover(&run, &radio).await;
    assert_eq!(report.outcome, recovery::Outcome::Complete, "{report:?}");
    assert_as_found(&radio, &before);
    assert_eq!(journal_info(&run.cfg).status, JournalStatus::Resolved);
    // Restoring again changes nothing: the engine is idempotent.
    let setters = radio.setters_received().len();
    assert_eq!(recover(&run, &radio).await.outcome, recovery::Outcome::Complete);
    assert_eq!(radio.setters_received().len(), setters);
}

/// [`recovery_radio`] with the sub receiver off, its VFO B elsewhere and
/// operator settings in its own per-band-and-input memories.
fn paired_recovery_radio() -> SimRadio {
    let radio = recovery_radio();
    radio.update(|m| {
        m.fb = 14_060_000;
        m.sub = operator_rx();
    });
    radio
}

#[tokio::test(start_paused = true)]
async fn a_paired_survey_killed_at_any_point_is_recoverable() {
    let profile = format!(
        "paired = true\nswap = true\n{}",
        two_band_profile(r#""log-near-house", "dxc""#, "")
    );
    let (mut sub_on, mut sub_changed) = (0, 0);
    for secs in [1, 2, 4, 7, 11, 16, 22, 30, 45, 66] {
        let radio = paired_recovery_radio();
        let before = radio.model();
        let profile = profile.clone();
        let run = interrupted_with(&radio, Duration::from_secs(secs), None, move |root| {
            paired_config(root, &profile)
        })
        .await;
        assert!(run.report.is_none(), "{secs} s: still running");
        let killed = radio.model();
        sub_on += usize::from(killed.sub_rx);
        sub_changed += usize::from(killed.sub != before.sub || killed.sub_input != before.sub_input);
        let report = recover(&run, &radio).await;
        assert_eq!(report.outcome, recovery::Outcome::Complete, "{secs} s: {report:?}");
        assert_as_found(&radio, &before);
        let m = radio.model();
        assert_eq!(
            (m.sub_rx, m.fb, m.sub_input, m.sub_span, m.sub),
            (before.sub_rx, before.fb, before.sub_input, before.sub_span, before.sub),
            "{secs} s"
        );
        for band in [3, 5] {
            for input in [4, 6] {
                assert_eq!(
                    m.sub_settings(band, input),
                    before.sub,
                    "{secs} s: sub band {band} input {input}"
                );
            }
            assert_eq!(
                m.band_sub.get(&band).map_or(4, |s| s.2),
                4,
                "{secs} s: band {band} sub input"
            );
        }
        assert_eq!(journal_info(&run.cfg).status, JournalStatus::Resolved);
    }
    assert!(
        sub_on >= 5 && sub_changed >= 5,
        "kills mid-pair: {sub_on} on, {sub_changed} changed"
    );
}

#[tokio::test(start_paused = true)]
async fn a_kill_at_any_point_is_recoverable() {
    // From before the first change, through band, antenna and settings
    // changes, to the second cycle. Each kill leaves a journal from which
    // the radio is fully restored.
    for secs in [1, 2, 3, 5, 8, 11, 15, 19, 24, 30, 37, 45, 54, 66, 80, 100] {
        let radio = recovery_radio();
        let before = radio.model();
        let run = interrupted_survey(&radio, Duration::from_millis(secs * 1000 + 137), None).await;
        assert!(run.report.is_none(), "{secs} s: still running");
        let info = journal_info(&run.cfg);
        assert_eq!(info.status, JournalStatus::Pending, "{secs} s");
        let report = recover(&run, &radio).await;
        assert_eq!(report.outcome, recovery::Outcome::Complete, "{secs} s: {report:?}");
        assert_as_found(&radio, &before);
        assert_eq!(journal_info(&run.cfg).status, JournalStatus::Resolved, "{secs} s");
    }
}

#[tokio::test(start_paused = true)]
async fn cancellation_restores_everything_and_resolves_the_journal() {
    let radio = recovery_radio();
    let before = radio.model();
    let run = interrupted_survey(&radio, Duration::from_secs(600), Some(Duration::from_secs(45))).await;
    let r = run.report.as_ref().expect("cancelled, not killed");
    assert_eq!(
        (r.halt.as_str(), r.restore_outcome.as_str()),
        ("stop", "complete"),
        "{r:?}"
    );
    assert_eq!(r.summary.end_reason, "signal");
    assert_as_found(&radio, &before);
    let info = journal_info(&run.cfg);
    assert_eq!((info.status, info.dirty), (JournalStatus::Resolved, 0));
}

#[tokio::test(start_paused = true)]
async fn a_partial_restore_is_reported_and_a_retry_completes_it() {
    let radio = recovery_radio();
    let before = radio.model();
    let run = interrupted_survey(&radio, Duration::from_secs(20), None).await;
    assert!(run.report.is_none());
    // The radio ignores RF gain setters during the first attempt (the
    // kill at 20 s leaves 40 m's RF gain changed on ANT2).
    radio.faults(|f| f.ignore_setters = vec!["RG".into()]);
    let first = recover(&run, &radio).await;
    assert_eq!(first.outcome, recovery::Outcome::Partial, "{first:?}");
    assert!(
        first.fields.iter().any(|f| f.field == "RG" && f.result == "failed"),
        "{first:?}"
    );
    assert!(first.manual.iter().any(|m| m.contains("RF gain")), "{:?}", first.manual);
    assert_eq!(journal_info(&run.cfg).status, JournalStatus::Pending);
    radio.faults(|f| f.ignore_setters.clear());
    let retry = recover(&run, &radio).await;
    assert_eq!(retry.outcome, recovery::Outcome::Complete, "{retry:?}");
    assert_as_found(&radio, &before);
    assert_eq!(journal_info(&run.cfg).status, JournalStatus::Resolved);
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
        // The effective values, as verified by readback.
        assert_eq!(s["verified.PA"], "0", "{s:?}");
        assert_eq!(s["verified.RG"], "-00", "{s:?}");
        assert_eq!(s["verified.NB"], "0", "{s:?}");
    }
    // One deterministic segment per band across cycles.
    for band in ["40m", "20m"] {
        let segs: std::collections::BTreeSet<_> = complete
            .iter()
            .filter(|v| v.band_id.as_deref() == Some(band))
            .map(|v| &v.segment_id)
            .collect();
        assert_eq!(segs.len(), 1, "{band}: {segs:?}");
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
