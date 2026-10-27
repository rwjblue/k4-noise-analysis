#![allow(clippy::unwrap_used)]

mod common;

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
    survey_with(radio, secs, None, script).await
}

async fn survey_with(
    radio: SimRadio,
    secs: u64,
    probe: Option<Box<dyn SpaceProbe>>,
    script: impl FnOnce(SimRadio) + Send + 'static,
) -> Run {
    let dir = tempfile::tempdir().unwrap();
    let cfg = config(&dir.path().join("data"), "survey");
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
        assert!(
            ALLOWED_SETTERS.iter().any(|p| s.starts_with(p)),
            "unexpected setter {s}"
        );
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
    for band in &run.cfg.bands {
        let complete: Vec<_> = visits
            .iter()
            .filter(|v| v.band_id.as_deref() == Some(&band.id) && v.status == "complete")
            .collect();
        assert!(!complete.is_empty(), "no complete visits for {}", band.id);
    }
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
    let run = survey_with(radio, 600, Some(Box::new(FakeDisk(free))), move |_| {
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
