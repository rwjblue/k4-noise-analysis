#![allow(clippy::unwrap_used)]

mod common;

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use common::*;
use k4na_collector::cat::{ControlCat, RetuneConsent};
use k4na_collector::net::Connector;
use k4na_collector::runtime::stop_pair;
use k4na_collector::spectrum::{SpectrumOptions, StreamSpectrum};
use k4na_collector::survey::{SurveyReport, preflight, run_survey};
use k4na_core::Config;
use k4na_core::config::Mode;
use k4na_core::time::Clock;
use k4na_sim::{RadioModel, SimRadio};
use k4na_store::SpaceProbe;
use k4na_store::reader::DataSet;

const ALLOWED_SETTERS: [&str; 4] = ["BN", "FA", "#SPN", "#FXT"];

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
    survey_with(radio, secs, None, |root| config(root, "survey"), script).await
}

async fn survey_with(
    radio: SimRadio,
    secs: u64,
    probe: Option<Box<dyn SpaceProbe>>,
    make_config: impl FnOnce(&Path) -> Config,
    script: impl FnOnce(SimRadio) + Send + 'static,
) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = make_config(&dir.path().join("data"));
    // Some scenarios start from a fixed pan; opt in to switching it.
    cfg.collection.set_pan_track_mode = true;
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
    script(radio.clone());
    let report = run_survey(
        ctx(&cfg, &store, &clock, sid, stop),
        &mut spec,
        &mut cat,
        pre,
        Duration::from_secs(secs),
    )
    .await
    .unwrap();
    store.close(clock.now(), &report.summary.end_reason).await.unwrap();
    assert!(
        radio.forbidden_commands().is_empty(),
        "forbidden: {:?}",
        radio.forbidden_commands()
    );
    for s in radio.setters_received() {
        let rx_input = s.starts_with("AR") && cfg.collection.allow_rx_antenna_switch;
        assert!(
            rx_input || ALLOWED_SETTERS.iter().any(|p| s.starts_with(p)),
            "unexpected setter {s}"
        );
        assert!(!s.starts_with("AN"), "the TX antenna must never be set: {s}");
    }
    Run {
        cfg,
        radio,
        report,
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
    let run = survey_with(
        radio,
        600,
        Some(Box::new(FakeDisk(free))),
        |root| config(root, "survey"),
        move |_| {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(60)).await;
                // Leave less than the configured headroom.
                f2.store(1 << 20, Ordering::SeqCst);
            });
        },
    )
    .await;
    assert_eq!(run.report.halt, "storage", "{:?}", run.report);
    assert!(run.report.summary.end_reason.contains("storage"));
    assert!(run.report.restore.starts_with("restored"), "{}", run.report.restore);
    assert_eq!(run.radio.model().fa, RadioModel::default().fa);
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
    // Holding between cycles sends no setters.
    let fa = run
        .radio
        .setters_received()
        .iter()
        .filter(|s| s.starts_with("FA"))
        .count();
    assert!(fa <= starts.len() + 3, "{fa} FA setters");
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
