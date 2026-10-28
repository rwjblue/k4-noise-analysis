//! `k4na`: receive-only K4 spectrum collection.
//!
//! Exit codes: 0 success; 1 runtime failure (storage, CAT loss, no
//! spectrum); 2 refused before collection (config, consent, preflight,
//! admission, unresolved recovery journal); 3 stopped for operator activity
//! (transmit or external change) or a restore conflict.

mod recovery;

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use k4na_collector::cat::{CatQuery, ControlCat, Query, ReadOnlyCat, RetuneConsent};
use k4na_collector::net::{Connector, TcpConnector};
use k4na_collector::passive::{CatStateCache, RunSummary, poll_cat_state, run_passive};
use k4na_collector::runtime::{RunContext, StopHandle, TokioClock, stop_pair};
use k4na_collector::spectrum::{SpectrumEvent, SpectrumOptions, SpectrumSource, StreamSpectrum};
use k4na_collector::store_task::StoreHandle;
use k4na_collector::survey::{SurveyReport, begin_journal, preflight, run_survey};
use k4na_core::Config;
use k4na_core::config::Mode;
use k4na_core::plan::SurveyPlan;
use k4na_core::time::Clock;
use k4na_store::estimate::{Estimate, admission, estimate};
use k4na_store::reader::DataSet;
use k4na_store::{FsSpaceProbe, SpaceProbe, Store, StoreOptions};

/// Survey per-visit CAT/bookkeeping allowance used for estimates.
const VISIT_OVERHEAD_MS: u64 = 250;

#[derive(Debug, Parser)]
#[command(name = "k4na", version, about = "Receive-only Elecraft K4 spectrum noise collector")]
struct Cli {
    /// Print machine-readable JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, clap::Args)]
struct ConfigArgs {
    /// Station config file.
    #[arg(long)]
    config: PathBuf,
    /// Survey profile; overrides `collection.survey_profile`.
    #[arg(long)]
    profile: Option<PathBuf>,
}

impl ConfigArgs {
    fn load(&self) -> Result<Config> {
        Config::load(&self.config, self.profile.as_deref())
            .map_err(|e| refuse(format!("{}: {e}", self.config.display())))
    }
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Validate a station config and survey profile and print the digest.
    Check {
        #[command(flatten)]
        config: ConfigArgs,
    },
    /// Estimate storage for a run and check admission against the data root.
    Estimate {
        #[command(flatten)]
        config: ConfigArgs,
        #[arg(long, value_parser = humantime::parse_duration, default_value = "24h")]
        duration: Duration,
        /// Estimate passive capture instead of the configured mode.
        #[arg(long)]
        passive: bool,
    },
    /// Passive capture: spectrum stream only; optional GET-only CAT readback.
    Capture {
        #[command(flatten)]
        config: ConfigArgs,
        #[arg(long, value_parser = humantime::parse_duration)]
        duration: Duration,
    },
    /// Authorized band survey (retunes VFO A). Requires --allow-retune AND
    /// `collection.allow_retune = true`; use --dry-run first.
    Survey {
        #[command(flatten)]
        config: ConfigArgs,
        #[arg(long, value_parser = humantime::parse_duration)]
        duration: Duration,
        /// Consent to retune the radio (BN/FA/#SPN/#FXT, and AR when
        /// `collection.allow_rx_antenna_switch` is set).
        #[arg(long)]
        allow_retune: bool,
        /// GET-only preflight and plan preview; sends no setters.
        #[arg(long, conflicts_with = "allow_retune")]
        dry_run: bool,
    },
    /// Read every known CAT state value with GET-only queries and print the
    /// raw responses. Sends no setters.
    Probe {
        #[command(flatten)]
        config: ConfigArgs,
    },
    /// Inspect and settle survey recovery journals: what an interrupted
    /// survey changed and how to put it back.
    Recovery {
        #[command(subcommand)]
        action: recovery::RecoveryCmd,
    },
    /// Summarize a data root without the radio (read-only).
    Inspect {
        #[arg(long)]
        data: PathBuf,
    },
    /// Decode a raw :9201 byte capture offline.
    Replay {
        #[arg(long)]
        input: PathBuf,
    },
    /// Run a simulated K4 (CAT + spectrum) on localhost for dry runs.
    Simulate {
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        #[arg(long, default_value_t = 9200)]
        cat_port: u16,
        #[arg(long, default_value_t = 9201)]
        spectrum_port: u16,
        #[arg(long, default_value_t = 15)]
        fps: u32,
        /// Track-mode pan center minus VFO A.
        #[arg(long, default_value_t = 0, allow_hyphen_values = true)]
        pan_offset_hz: i64,
    },
}

#[derive(Debug)]
struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

fn refuse(msg: impl Into<String>) -> anyhow::Error {
    Refused(msg.into()).into()
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("error: cannot start runtime: {e}");
            return ExitCode::from(1);
        }
    };
    match rt.block_on(run(cli)) {
        Ok(code) => code,
        Err(e) if e.is::<Refused>() => {
            eprintln!("refused: {e:#}");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(1)
        }
    }
}

async fn run(cli: Cli) -> Result<ExitCode> {
    let json = cli.json;
    match cli.cmd {
        Cmd::Check { config } => check(&config, json),
        Cmd::Estimate {
            config,
            duration,
            passive,
        } => {
            let cfg = config.load()?;
            let mode = if passive { Mode::Passive } else { cfg.collection.mode };
            let (est, admit) = estimate_for(&cfg, mode, duration)?;
            print_estimate(&cfg, mode, &est, admit.as_ref().err(), json);
            Ok(if admit.is_ok() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            })
        }
        Cmd::Capture { config, duration } => capture(&config.load()?, duration, json).await,
        Cmd::Probe { config } => probe(&config.load()?, json).await,
        Cmd::Survey {
            config,
            duration,
            allow_retune,
            dry_run,
        } => {
            let cfg = config.load()?;
            if dry_run {
                survey_dry_run(&cfg, json).await
            } else {
                survey(&cfg, duration, allow_retune, json).await
            }
        }
        Cmd::Recovery { action } => recovery::run(action, json).await,
        Cmd::Inspect { data } => inspect(&data, json),
        Cmd::Replay { input } => replay(&input, json),
        Cmd::Simulate {
            host,
            cat_port,
            spectrum_port,
            fps,
            pan_offset_hz,
        } => {
            let radio = k4na_sim::SimRadio::new(fps);
            radio.faults(|f| f.pan_center_offset_hz = pan_offset_hz);
            let sim = k4na_sim::TcpSim::start(radio, &host, cat_port, spectrum_port).await?;
            eprintln!(
                "SIMULATED K4 (not a real radio): CAT {} spectrum {}; Ctrl-C to stop",
                sim.cat_addr, sim.pan_addr
            );
            tokio::signal::ctrl_c().await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn check(args: &ConfigArgs, json: bool) -> Result<ExitCode> {
    let cfg = args.load()?;
    let profile = cfg.profile.as_ref();
    if json {
        println!(
            "{}",
            serde_json::json!({
                "ok": true,
                "digest": cfg.digest(),
                "mode": cfg.collection.mode,
                "profile": profile.map(|p| &p.id),
                "antennas": profile.map(|p| &p.antennas),
                "ranges": cfg.ranges().len(),
            })
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!("config OK: {}", args.config.display());
    println!("  digest   {}", cfg.digest());
    println!(
        "  mode     {:?} (allow_retune = {}, allow_rx_antenna_switch = {})",
        cfg.collection.mode, cfg.collection.allow_retune, cfg.collection.allow_rx_antenna_switch
    );
    for a in &cfg.antennas {
        println!("  antenna  {} on {} rev {:?}", a.id, a.port, a.revision);
    }
    match profile {
        Some(p) => {
            println!("  profile  {} ({})", p.id, p.description);
            println!("  surveys  {}", p.antennas.join(", "));
            println!(
                "  cadence  settle {} ms, dwell {} ms, cycle start-to-start >= {} s",
                p.settle_ms,
                p.dwell_ms,
                p.min_cycle_ms / 1000
            );
            println!(
                "  ranges   {}",
                p.ranges
                    .iter()
                    .map(|b| format!(
                        "{} {:.3}-{:.3} MHz",
                        b.id,
                        b.low_hz as f64 / 1e6,
                        b.high_hz as f64 / 1e6
                    ))
                    .collect::<Vec<_>>()
                    .join("; ")
            );
        }
        None => println!("  profile  none"),
    }
    println!("  data     {}", cfg.storage.root.display());
    Ok(ExitCode::SUCCESS)
}

/// Expected cycle shape at the planning sample rate: (visits per cycle,
/// busy time, start-to-start period).
fn cycle_shape(cfg: &Config) -> Option<(usize, Duration, Duration)> {
    let p = cfg.profile.as_ref()?;
    let plan = SurveyPlan::build(
        cfg.ranges(),
        cfg.collection.planning_sample_rate_hz,
        k4na_protocol::BIN_COUNT as u32,
        cfg.collection.usable_fraction,
    )
    .ok()?;
    let visits = plan.cycle.len() * p.antennas.len();
    let busy = Duration::from_millis(visits as u64 * (p.settle_ms + p.dwell_ms + VISIT_OVERHEAD_MS));
    Some((visits, busy, busy.max(Duration::from_millis(p.min_cycle_ms))))
}

fn existing_ancestor(path: &Path) -> PathBuf {
    let mut p = path.to_path_buf();
    while !p.exists() {
        if !p.pop() {
            return PathBuf::from(".");
        }
    }
    p
}

fn estimate_for(cfg: &Config, mode: Mode, _duration: Duration) -> Result<(Estimate, Result<(), String>)> {
    let opts = StoreOptions::from_config(&cfg.storage);
    let c = &cfg.collection;
    let (visit_ms, accepting_ms) = match (mode, cfg.profile.as_ref(), cycle_shape(cfg)) {
        // Idle time between cycles stretches the average visit period.
        (Mode::Survey, Some(p), Some((visits, _, period))) => {
            ((period.as_millis() as u64 / visits.max(1) as u64).max(1), p.dwell_ms)
        }
        (Mode::Survey, _, _) => bail!(refuse("a survey estimate needs a valid survey profile")),
        (Mode::Passive, _, _) => (c.passive_visit_ms, c.passive_visit_ms),
    };
    let est = estimate(&opts, 1, visit_ms, accepting_ms, c.raw_sample_interval_ms);
    let usage = if cfg.storage.root.is_dir() {
        DataSet::open(&cfg.storage.root)
            .and_then(|d| d.inspect())
            .map(|r| r.summary_bytes + r.raw_bytes)
            .unwrap_or(0)
    } else {
        0
    };
    let free = FsSpaceProbe
        .available_bytes(&existing_ancestor(&cfg.storage.root))
        .context("free-space probe")?;
    let admit = admission(&opts, &est, usage, free);
    Ok((est, admit))
}

fn mib(b: u64) -> String {
    format!("{:.1} MiB", b as f64 / 1_048_576.0)
}

fn print_estimate(cfg: &Config, mode: Mode, est: &Estimate, refused: Option<&String>, json: bool) {
    let shape = (mode == Mode::Survey).then(|| cycle_shape(cfg)).flatten();
    if json {
        println!(
            "{}",
            serde_json::json!({
                "estimate": est,
                "admitted": refused.is_none(),
                "reason": refused,
                "cycle": shape.map(|(visits, busy, period)| serde_json::json!({
                    "visits": visits,
                    "busy_s": busy.as_secs_f64(),
                    "period_s": period.as_secs_f64(),
                })),
            })
        );
        return;
    }
    println!("storage estimate ({mode:?} mode, upper bounds):");
    if let Some((visits, busy, period)) = shape {
        println!(
            "  cycle                {visits} visits, ~{} s busy, one every {} s (at {} Hz packets)",
            busy.as_secs(),
            period.as_secs(),
            cfg.collection.planning_sample_rate_hz
        );
    }
    println!("  visits/hour          {:.0}", est.visits_per_hour);
    println!("  raw frames/hour      {:.0}", est.raw_frames_per_hour);
    println!("  summaries/day        {}", mib(est.summary_bytes_per_day));
    println!("  raw/day              {}", mib(est.raw_bytes_per_day));
    println!("  protected summaries  {}", mib(est.protected_summary_bytes));
    println!("  raw retention        {}", mib(est.raw_retention_bytes));
    println!("  summary days at cap  {:.1}", est.summary_days_at_cap);
    match refused {
        None => println!("admission: OK (cap {})", mib(cfg.storage.max_total_bytes)),
        Some(r) => println!("admission: REFUSED: {r}"),
    }
}

fn connectors(cfg: &Config) -> (TcpConnector, TcpConnector) {
    let t = Duration::from_millis(cfg.radio.connect_timeout_ms);
    (
        TcpConnector {
            host: cfg.radio.host.clone(),
            port: cfg.radio.cat_port,
            timeout: t,
        },
        TcpConnector {
            host: cfg.radio.host.clone(),
            port: cfg.radio.spectrum_port,
            timeout: t,
        },
    )
}

fn install_signals(stop: StopHandle) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(s) => s,
                Err(_) => {
                    let _ = tokio::signal::ctrl_c().await;
                    stop.stop("interrupted (SIGINT)");
                    return;
                }
            };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => stop.stop("interrupted (SIGINT)"),
                _ = term.recv() => stop.stop("terminated (SIGTERM)"),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            stop.stop("interrupted");
        }
    });
}

/// Opens the store after admission; returns it with its writer thread.
fn open_store(
    cfg: &Config,
    mode: Mode,
    session_id: &str,
    clock: &Arc<dyn Clock>,
    firmware: Vec<String>,
    snapshot: k4na_core::model::RadioState,
    duration: Duration,
) -> Result<(StoreHandle, std::thread::JoinHandle<()>)> {
    let (_, admit) = estimate_for(cfg, mode, duration)?;
    admit.map_err(|r| refuse(format!("storage admission: {r}")))?;
    let info = k4na_collector::session_info(cfg, mode, session_id, clock.now(), firmware, snapshot);
    let (store, recovery) = Store::open(
        StoreOptions::from_config(&cfg.storage),
        Box::new(FsSpaceProbe),
        info,
        clock.now(),
    )
    .map_err(|e| refuse(format!("cannot open data root {}: {e}", cfg.storage.root.display())))?;
    for p in &recovery.recovered {
        eprintln!("recovered shard {}", p.display());
    }
    for (p, e) in &recovery.unreadable {
        eprintln!("UNREADABLE shard kept (counts toward the cap): {}: {e}", p.display());
    }
    Ok(StoreHandle::spawn(store))
}

async fn capture(cfg: &Config, duration: Duration, json: bool) -> Result<ExitCode> {
    let antenna = cfg.passive_antenna().map_err(refuse)?;
    eprintln!(
        "attributing data to antenna {} on {} (verified per window from AR/AN readback when available)",
        antenna.id, antenna.port
    );
    let clock: Arc<dyn Clock> = Arc::new(TokioClock::real());
    let session_id = k4na_collector::visit::new_id();
    let (cat_conn, pan_conn) = connectors(cfg);
    let (store, writer) = open_store(
        cfg,
        Mode::Passive,
        &session_id,
        &clock,
        vec![],
        Default::default(),
        duration,
    )?;
    let (stop_handle, stop) = stop_pair();
    install_signals(stop_handle.clone());
    let cache = cfg
        .radio
        .passive_cat_readback
        .then(|| Arc::new(Mutex::new(CatStateCache::default())));
    let poller = cache.clone().map(|c| {
        let timeout = Duration::from_millis(cfg.radio.cat_timeout_ms);
        tokio::spawn(poll_cat_state(
            cat_conn,
            timeout,
            Duration::from_secs(10),
            c,
            stop.clone(),
        ))
    });
    let mut spectrum = StreamSpectrum::new(pan_conn, SpectrumOptions::from_config(&cfg.radio), clock.clone());
    eprintln!(
        "passive capture {session_id} for {}; no CAT setters",
        humantime::format_duration(duration)
    );
    let ctx = RunContext {
        cfg,
        store: &store,
        clock: clock.clone(),
        session_id,
        stop,
    };
    let summary = run_passive(ctx, &mut spectrum, cache, duration).await?;
    stop_handle.stop("capture finished");
    if let Some(p) = poller {
        let _ = p.await;
    }
    let status = store.close(clock.now(), &summary.end_reason).await;
    let _ = writer.join();
    print_summary(&summary, None, json);
    let status = status.context("closing store")?;
    if !json {
        println!(
            "storage: {} used, raw dropped {}",
            mib(status.usage_bytes),
            status.raw_dropped
        );
    }
    Ok(if summary.failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

/// GET-only dump of every known query, raw, for checking CAT formats on a
/// real radio. Uses the read-only link, which cannot send setters.
async fn probe(cfg: &Config, json: bool) -> Result<ExitCode> {
    let (cat_conn, _) = connectors(cfg);
    let stream = cat_conn
        .connect()
        .await
        .with_context(|| format!("CAT {}", cat_conn.describe()))?;
    let mut cat = ReadOnlyCat::new(stream, Duration::from_millis(cfg.radio.cat_timeout_ms));
    let mut values = serde_json::Map::new();
    // One query at a time so a slow or unanswered command cannot hide others.
    for q in Query::ALL {
        let rb = cat
            .query(&[q])
            .await
            .with_context(|| format!("querying {}", q.wire()))?;
        let v = if q == Query::Revisions {
            serde_json::json!(rb.revisions)
        } else {
            rb.raw(q).map_or(serde_json::Value::Null, |s| serde_json::json!(s))
        };
        values.insert(q.prefix().to_owned(), v);
    }
    if json {
        println!(
            "{}",
            serde_json::json!({ "values": values, "commands_sent": cat.sent() })
        );
    } else {
        println!("probe: GET-only; no setters were sent");
        for (k, v) in &values {
            println!(
                "  {k:<5} {}",
                if v.is_null() {
                    "(no answer)".to_owned()
                } else {
                    v.to_string()
                }
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn survey_dry_run(cfg: &Config, json: bool) -> Result<ExitCode> {
    let clock: Arc<dyn Clock> = Arc::new(TokioClock::real());
    let (cat_conn, pan_conn) = connectors(cfg);
    let stream = cat_conn
        .connect()
        .await
        .with_context(|| format!("CAT {}", cat_conn.describe()))?;
    let mut cat = ReadOnlyCat::new(stream, Duration::from_millis(cfg.radio.cat_timeout_ms));
    let pre = preflight(&mut cat, cfg).await;
    let mut spectrum = StreamSpectrum::new(pan_conn, SpectrumOptions::from_config(&cfg.radio), clock);
    let pan = cfg.radio.receiver.pan();
    let observed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let SpectrumEvent::Frame(f) = spectrum.next().await
                && f.frame.pan() == pan
            {
                return *f.frame.header();
            }
        }
    })
    .await
    .ok();
    let plan = observed.as_ref().map(|h| {
        SurveyPlan::build(
            cfg.ranges(),
            h.sample_rate_hz,
            k4na_protocol::BIN_COUNT as u32,
            cfg.collection.usable_fraction,
        )
    });
    let blocked = refuse_if_unresolved(cfg).err().map(|e| e.to_string());
    let measurement = pre
        .as_ref()
        .ok()
        .map(|p| measurement_preview(cfg, p))
        .unwrap_or_default();
    if json {
        println!(
            "{}",
            serde_json::json!({
                "recovery_blocked": blocked,
                "cat_meta_mode": pre.as_ref().ok().map(|p| p.meta.as_str()),
                "measurement": measurement,
                "preflight": pre.as_ref().map(|p| &p.snapshot).map_err(|e| e.clone()),
                "observed_center_hz": observed.as_ref().map(|h| h.center_hz),
                "observed_sample_rate_hz": observed.as_ref().map(|h| h.sample_rate_hz),
                "plan": plan.as_ref().and_then(|p| p.as_ref().ok()),
                "commands_sent": cat.sent(),
            })
        );
    } else {
        println!("dry run: GET-only; no setters were sent");
        match &pre {
            Ok(p) => println!(
                "preflight: OK (firmware {}; CAT meta-mode {})",
                p.firmware.join(" "),
                p.meta
            ),
            Err(e) => println!("preflight: {e}"),
        }
        if let Some(b) = &blocked {
            println!("recovery: {b}");
        }
        for (antenna, line) in &measurement {
            println!("measurement on the current band, antenna {antenna}: {line}");
        }
        match &observed {
            Some(h) => println!(
                "observed pan {}: center {} Hz, packet rate {} Hz",
                h.pan, h.center_hz, h.sample_rate_hz
            ),
            None => println!("no spectrum frame on pan {pan} within 10 s"),
        }
        if let (Some(Ok(p)), Some(s)) = (&plan, cfg.profile.as_ref()) {
            let antennas = s.antennas.len();
            println!(
                "plan at the CURRENT packet rate: {} tiles x {antennas} antenna(s), cycle needs ~{} s, \
                 starts at most every {} s",
                p.tiles.len(),
                (p.cycle_duration(s.settle_ms, s.dwell_ms, VISIT_OVERHEAD_MS) * antennas as u32).as_secs(),
                s.min_cycle_ms / 1000
            );
            println!("  (the survey re-plans from the rate observed after applying #SPN)");
            for b in cfg.ranges() {
                println!("  {:>8}: {} tile(s)", b.id, p.tiles_for_band(&b.id).count());
            }
            if let Ok(pre) = &pre {
                let port = k4na_core::config::K4Port::from_readback(
                    pre.readback.raw(k4na_collector::cat::Query::RxAntenna),
                    pre.readback.raw(k4na_collector::cat::Query::TxAntenna),
                );
                println!(
                    "main RX input now: {} (this band only; other bands may recall another)",
                    port.map_or("unknown", |p| p.as_str())
                );
                for a in cfg.survey_antennas() {
                    let note = if Some(a.port) == port {
                        "selected"
                    } else if cfg.collection.allow_rx_antenna_switch {
                        "will be selected with AR"
                    } else {
                        "NOT selected: tiles will be skipped unless chosen on the radio"
                    };
                    println!("  antenna {} on {}: {note}", a.id, a.port);
                }
            }
        }
    }
    Ok(if pre.is_ok() && observed.is_some() && blocked.is_none() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    })
}

async fn survey(cfg: &Config, duration: Duration, allow_retune: bool, json: bool) -> Result<ExitCode> {
    if cfg.collection.mode != Mode::Survey {
        bail!(refuse("config collection.mode is not \"survey\""));
    }
    let consent = RetuneConsent::new(cfg.collection.allow_retune, allow_retune).map_err(refuse)?;
    refuse_if_unresolved(cfg)?;
    let clock: Arc<dyn Clock> = Arc::new(TokioClock::real());
    let session_id = k4na_collector::visit::new_id();
    let (cat_conn, pan_conn) = connectors(cfg);
    let stream = cat_conn
        .connect()
        .await
        .map_err(|e| refuse(format!("CAT {}: {e}", cat_conn.describe())))?;
    let mut cat = ControlCat::new(stream, Duration::from_millis(cfg.radio.cat_timeout_ms), consent);
    let pre = preflight(&mut cat, cfg).await.map_err(refuse)?;
    let (store, writer) = open_store(
        cfg,
        Mode::Survey,
        &session_id,
        &clock,
        pre.firmware.clone(),
        pre.snapshot.clone(),
        duration,
    )?;
    // Durable before any change; refuses if another journal appeared.
    let mut txn = match begin_journal(cfg, &session_id, &pre, clock.clone()) {
        Ok(t) => t,
        Err(e) => {
            let _ = store.close(clock.now(), "recovery journal refused").await;
            let _ = writer.join();
            bail!(refuse(e.to_string()));
        }
    };
    let (stop_handle, stop) = stop_pair();
    install_signals(stop_handle);
    let mut spectrum = StreamSpectrum::new(pan_conn, SpectrumOptions::from_config(&cfg.radio), clock.clone());
    eprintln!(
        "AUTHORIZED SURVEY {session_id} for {}: retunes VFO A and applies measurement settings; stops on \
         transmit or any operator change; journal {}",
        humantime::format_duration(duration),
        txn.path().map_or("?".to_owned(), |p| p.display().to_string())
    );
    let ctx = RunContext {
        cfg,
        store: &store,
        clock: clock.clone(),
        session_id,
        stop,
    };
    let report = run_survey(ctx, &mut spectrum, &mut cat, pre, &mut txn, duration).await?;
    drop(txn);
    let status = store.close(clock.now(), &report.summary.end_reason).await;
    let _ = writer.join();
    print_summary(&report.summary, Some(&report), json);
    status.context("closing store")?;
    Ok(match report.halt.as_str() {
        "stop" => ExitCode::SUCCESS,
        "conflict" => ExitCode::from(3),
        _ => ExitCode::from(1),
    })
}

fn print_summary(s: &RunSummary, survey: Option<&SurveyReport>, json: bool) {
    if json {
        let v = match survey {
            Some(r) => serde_json::to_value(r),
            None => serde_json::to_value(s),
        };
        println!("{}", v.unwrap_or_default());
        return;
    }
    println!("ended: {}", s.end_reason);
    println!(
        "visits {} accepted frames {} gaps {}",
        s.visits, s.accepted_frames, s.gaps
    );
    if !s.ignored_frames.is_empty() {
        println!("ignored other-pan frames: {:?}", s.ignored_frames);
    }
    println!(
        "decoder: {} frames, {} bad CRC, {} bad headers, {} bytes discarded",
        s.decoder.frames_decoded, s.decoder.invalid_checksums, s.decoder.invalid_headers, s.decoder.discarded_bytes
    );
    if let Some(r) = survey {
        println!(
            "survey: {} cycles over {} tiles, {} tile visits without verified coverage, {} skipped for antenna",
            r.cycles, r.tiles_planned, r.coverage_unverified, r.antenna_skips
        );
        if let Some(s) = r.last_cycle_s {
            println!("cadence: last cycle {s:.0} s; {:.0} s idle between cycles", r.idle_s);
        }
        if r.measurement_skips > 0 {
            println!(
                "measurement: {} tile(s) skipped (settings could not be applied)",
                r.measurement_skips
            );
        }
        println!("radio: {}", r.restore);
        if !r.manual.is_empty() {
            println!(
                "RADIO NOT FULLY RESTORED ({}); journal {}",
                r.restore_outcome,
                r.journal.as_deref().unwrap_or("?")
            );
            for m in &r.manual {
                println!("  manual: {m}");
            }
            println!("  see `k4na recovery show/restore/acknowledge`");
        }
    }
}

/// What each survey antenna's measurement settings would change on the
/// current band, judged from the GET-only preflight readback.
fn measurement_preview(cfg: &Config, pre: &k4na_collector::survey::Preflight) -> Vec<(String, String)> {
    let Some(profile) = cfg.profile.as_ref() else {
        return Vec::new();
    };
    let band = pre.readback.u64(Query::Band).and_then(|b| u8::try_from(b).ok());
    profile
        .antennas
        .iter()
        .map(|a| {
            let settings = profile.measurement.resolve(band, a);
            let line = match k4na_collector::measure::plan(pre.meta, &settings, band, &pre.readback) {
                Ok(c) if c.is_empty() => format!("no changes ({settings})"),
                Ok(c) => c.iter().map(|c| c.setter.wire()).collect::<Vec<_>>().join(" "),
                Err(why) => format!("tiles would be skipped: {why}"),
            };
            (a.clone(), line)
        })
        .collect()
}

/// New surveys wait until earlier changes are resolved or acknowledged.
fn refuse_if_unresolved(cfg: &Config) -> Result<()> {
    let dir = cfg.storage.recovery_dir();
    let blocking: Vec<String> = k4na_collector::recovery::list(&dir)?
        .into_iter()
        .filter(|j| j.status.blocks_surveys())
        .map(|j| format!("{} ({})", j.session_id, j.status.as_str()))
        .collect();
    if blocking.is_empty() {
        return Ok(());
    }
    bail!(refuse(format!(
        "unresolved survey recovery journal(s) in {}: {}. Run `k4na recovery show <session>`, then `recovery \
         restore` or `recovery acknowledge`",
        dir.display(),
        blocking.join(", ")
    )))
}

fn inspect(data: &Path, json: bool) -> Result<ExitCode> {
    let ds = DataSet::open(data).with_context(|| format!("open {}", data.display()))?;
    let r = ds.inspect()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&r)?);
        return Ok(ExitCode::SUCCESS);
    }
    println!("data root {}", r.root.display());
    println!(
        "shards: {} summary ({}), {} raw ({}), {} dirty",
        r.summary_shards,
        mib(r.summary_bytes),
        r.raw_shards,
        mib(r.raw_bytes),
        r.dirty_shards
    );
    for (p, e) in &r.unreadable_shards {
        println!("  UNREADABLE {}: {e}", p.display());
    }
    println!("sessions: {}", r.sessions.len());
    for s in &r.sessions {
        println!(
            "  {} {} profile {} antennas {} ended {:?}",
            s.session_id,
            s.mode,
            s.profile_id.as_deref().unwrap_or("-"),
            s.antennas.join(","),
            s.end_reason
        );
    }
    println!("visits: {} {:?}", r.visits_total, r.visits_by_status);
    println!(
        "frames: {} accepted, {} rejected, {} settle-discarded, {} raw kept",
        r.accepted_frames, r.rejected_frames, r.settle_discarded_frames, r.raw_frames
    );
    println!("gaps: {} events, {:.0} s", r.gap_events, r.gap_seconds);
    if let Some(b) = r.bytes_per_day {
        println!("observed growth: {}/day", mib(b as u64));
    }
    for b in &r.bands {
        let pct = if b.target_hz > 0 {
            b.covered_hz as f64 / b.target_hz as f64 * 100.0
        } else {
            0.0
        };
        println!(
            "  {:>8} on {}: {} complete / {} other visits, {} tiles, {:.0}% of planned targets covered",
            b.band_id, b.antenna_id, b.complete_visits, b.other_visits, b.tiles, pct
        );
    }
    println!("events: {:?}", r.events_by_kind);
    for e in &r.recent_problems {
        println!("  {} {}: {}", e.kind, e.utc_ns, e.message);
    }
    Ok(ExitCode::SUCCESS)
}

fn replay(input: &Path, json: bool) -> Result<ExitCode> {
    use std::collections::BTreeMap;
    use std::io::Read;
    let mut f = std::fs::File::open(input).with_context(|| format!("open {}", input.display()))?;
    let mut dec = k4na_protocol::Decoder::new();
    let mut frames = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut groups: BTreeMap<(String, u64, u32), u64> = BTreeMap::new();
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        dec.push(&buf[..n], &mut frames);
        for fr in frames.drain(..) {
            let h = fr.header();
            *groups
                .entry((h.pan.to_string(), h.center_hz, h.sample_rate_hz))
                .or_default() += 1;
        }
    }
    let d = dec.diagnostics();
    if json {
        let g: Vec<_> = groups
            .iter()
            .map(|((pan, c, r), n)| serde_json::json!({"pan": pan, "center_hz": c, "sample_rate_hz": r, "frames": n}))
            .collect();
        println!(
            "{}",
            serde_json::json!({"diagnostics": d, "groups": g, "trailing_bytes": dec.pending_bytes()})
        );
    } else {
        println!(
            "{}: {} frames, {} bytes, {} trailing",
            input.display(),
            d.frames_decoded,
            d.bytes_received,
            dec.pending_bytes()
        );
        println!(
            "rejects: {} bad headers, {} bad CRC, {} unsupported version, {} unsupported pan, {} bytes discarded",
            d.invalid_headers, d.invalid_checksums, d.unsupported_versions, d.unsupported_pans, d.discarded_bytes
        );
        for ((pan, c, r), n) in &groups {
            println!("  pan {pan} center {c} Hz rate {r} Hz: {n} frames");
        }
    }
    Ok(ExitCode::SUCCESS)
}
