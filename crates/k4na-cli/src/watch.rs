//! `k4na watch`: live, receive-only line/floor monitor for source hunting.
//!
//! Connects to the spectrum stream only. It never opens the CAT port, so
//! it cannot change the radio; the operator tunes the K4 by hand. With
//! `--mark`, confirmed changes and typed notes become operator marks in
//! the data root (see `k4na mark`).

use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Args;
use k4na_collector::runtime::{TokioClock, stop_pair};
use k4na_collector::spectrum::{SpectrumEvent, SpectrumOptions, SpectrumSource, StreamSpectrum};
use k4na_collector::watch::{
    FloorState, LineReading, LineState, Subject, Targets, WatchEvent, WatchOutput, WatchParams, WatchReport, Watcher,
};
use k4na_core::Config;
use k4na_core::time::{Clock, format_utc};
use k4na_protocol::PanId;
use k4na_store::annotations::{self, Annotation};
use tokio::io::AsyncBufReadExt;

use crate::{ConfigArgs, connectors, install_signals, refuse};

/// Parses a frequency in Hz, or with a k/M suffix (`10123.547k`).
pub fn parse_freq(s: &str) -> Result<f64, String> {
    let t = s.trim();
    let (num, mult) = match t.chars().last() {
        Some('k' | 'K') => (&t[..t.len() - 1], 1e3),
        Some('M' | 'm') => (&t[..t.len() - 1], 1e6),
        _ => (t.strip_suffix("Hz").or_else(|| t.strip_suffix("hz")).unwrap_or(t), 1.0),
    };
    let v: f64 = num.trim().parse().map_err(|_| format!("not a frequency: {s}"))?;
    if v.is_finite() && v > 0.0 {
        Ok(v * mult)
    } else {
        Err(format!("not a positive frequency: {s}"))
    }
}

#[derive(Debug, Args)]
pub struct WatchArgs {
    #[command(flatten)]
    config: ConfigArgs,
    /// Line to track, in Hz or with k/M (10123.547k). Repeatable.
    #[arg(long, value_parser = parse_freq, required_unless_present = "f0")]
    hz: Vec<f64>,
    /// Track every harmonic of this fundamental in view (15967.69).
    #[arg(long, value_parser = parse_freq, conflicts_with = "hz")]
    f0: Option<f64>,
    /// Pan to read (A, B, Y, Z); default: the configured receiver's.
    #[arg(long)]
    pan: Option<PanId>,
    /// Report interval; frames are averaged over it.
    #[arg(long, default_value = "1s", value_parser = humantime::parse_duration)]
    every: Duration,
    /// A line is present at this many dB over its local background.
    #[arg(long, default_value_t = 6.0)]
    threshold: f64,
    /// The floor is raised at this many dB over its 15-minute baseline.
    #[arg(long, default_value_t = 4.0)]
    floor_rise: f64,
    /// A change must last this long before it is reported.
    #[arg(long, default_value = "3s", value_parser = humantime::parse_duration)]
    hold: Duration,
    /// Search a line's peak within ±this many Hz.
    #[arg(long, default_value_t = 60.0)]
    tolerance: f64,
    /// Stop after this long (default: until Ctrl-C).
    #[arg(long, value_parser = humantime::parse_duration)]
    duration: Option<Duration>,
    /// Save confirmed changes and typed notes as marks in the data root.
    #[arg(long)]
    mark: bool,
    /// Append one CSV row per line per interval to this file.
    #[arg(long)]
    log: Option<PathBuf>,
    /// Print only changes and notes, not every interval.
    #[arg(long)]
    events_only: bool,
}

struct Out {
    json: bool,
    events_only: bool,
    log: Option<std::fs::File>,
    mark_root: Option<PathBuf>,
    clock: Arc<dyn Clock>,
    f0: Option<f64>,
    shown_view: Option<(u64, u32)>,
}

fn line_label(target_hz: f64, harmonic: Option<u64>) -> String {
    match harmonic {
        Some(k) => format!("h{k} {:.3} kHz", target_hz / 1e3),
        None => format!("{:.3} kHz", target_hz / 1e3),
    }
}

fn hhmmss(ns: i64) -> String {
    let s = format_utc(ns);
    // 2026-10-28T19:23:05.123Z -> 19:23:05Z
    s.get(11..19).map_or(s.clone(), |t| format!("{t}Z"))
}

impl Out {
    fn mark(&self, start_utc_ns: i64, text: String) {
        let Some(root) = &self.mark_root else { return };
        let a = Annotation {
            start_utc_ns,
            end_utc_ns: None,
            text,
            created_utc_ns: self.clock.now().utc_ns,
            source: Some("watch".into()),
        };
        if let Err(e) = annotations::append(root, &a) {
            eprintln!("warning: could not save mark: {e}");
        }
    }

    fn report(&mut self, r: &WatchReport) -> Result<()> {
        if let Some(f) = &mut self.log {
            for l in r.lines.iter().map(Some).chain(r.lines.is_empty().then_some(None)) {
                let state = l.map_or(String::new(), |l| {
                    serde_json::to_value(l.state)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default()
                });
                writeln!(
                    f,
                    "{},{},{},{},{:.1},{},{},{},{},{},{},{},{},{}",
                    format_utc(r.start_utc_ns),
                    r.frames,
                    r.center_hz,
                    r.sample_rate_hz,
                    r.floor_db,
                    r.floor_rise_db.map_or(String::new(), |v| format!("{v:.1}")),
                    serde_json::to_value(r.floor_state)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned))
                        .unwrap_or_default(),
                    l.map_or(String::new(), |l| format!("{:.1}", l.target_hz)),
                    l.and_then(|l| l.harmonic).map_or(String::new(), |k| k.to_string()),
                    l.map_or(String::new(), |l| format!("{:.1}", l.peak_hz)),
                    l.map_or(String::new(), |l| format!("{:.1}", l.level_db)),
                    l.map_or(String::new(), |l| format!("{:.1}", l.background_db)),
                    l.map_or(String::new(), |l| format!("{:.1}", l.excess_db)),
                    state,
                )?;
            }
        }
        if self.json {
            println!("{}", serde_json::to_string(&WatchOutput::Report(r.clone()))?);
            return Ok(());
        }
        if self.shown_view != Some((r.center_hz, r.sample_rate_hz)) {
            self.shown_view = Some((r.center_hz, r.sample_rate_hz));
            let lines: Vec<String> = r.lines.iter().map(|l| line_label(l.target_hz, l.harmonic)).collect();
            println!(
                "pan view {:.4}-{:.4} MHz (frame headers), {:.1} Hz/bin; tracking {}{}",
                r.low_hz / 1e6,
                r.high_hz / 1e6,
                r.bin_hz,
                if lines.is_empty() {
                    "no lines in view".to_owned()
                } else {
                    lines.join(", ")
                },
                match self.f0 {
                    Some(f0) => format!(" (harmonics of {f0:.2} Hz)"),
                    None => String::new(),
                }
            );
            if !r.out_of_view_hz.is_empty() {
                let v: Vec<String> = r.out_of_view_hz.iter().map(|h| format!("{:.3} kHz", h / 1e3)).collect();
                println!("  not in view: {}", v.join(", "));
            }
        }
        if self.events_only {
            return Ok(());
        }
        let floor = match (r.floor_rise_db, r.floor_state) {
            (Some(rise), FloorState::Raised) => format!("{:7.1} dB {rise:+5.1} RAISED", r.floor_db),
            (Some(rise), _) => format!("{:7.1} dB {rise:+5.1}       ", r.floor_db),
            (None, _) => format!("{:7.1} dB (baseline after 1 min)", r.floor_db),
        };
        let lines: Vec<String> = r.lines.iter().map(line_cell).collect();
        println!(
            "{}  {:>3} fr  floor {floor}  {}",
            hhmmss(r.start_utc_ns),
            r.frames,
            lines.join("  ")
        );
        Ok(())
    }

    fn event(&mut self, e: &WatchEvent) -> Result<()> {
        if self.json {
            println!("{}", serde_json::to_string(&WatchOutput::Event(e.clone()))?);
        }
        let (text, markable) = match &e.subject {
            Subject::Line { target_hz, harmonic } => (
                format!(
                    "line {}{}: {} -> {} ({:+.1} dB over background)",
                    line_label(*target_hz, *harmonic),
                    self.f0.map_or(String::new(), |f0| format!(" of {f0:.2} Hz")),
                    e.from,
                    e.to,
                    e.value_db.unwrap_or(f64::NAN)
                ),
                // Masked and first readings are not evidence of a change.
                e.from != "unknown" && e.to != "masked" && e.from != "masked",
            ),
            Subject::Floor => (
                format!(
                    "floor: {} -> {} ({:+.1} dB over baseline)",
                    e.from,
                    e.to,
                    e.value_db.unwrap_or(f64::NAN)
                ),
                e.from != "unknown",
            ),
            Subject::View {
                center_hz,
                sample_rate_hz,
            } => (
                format!(
                    "view changed (center {:.4} MHz, {sample_rate_hz} Hz packet rate): states restart",
                    *center_hz as f64 / 1e6
                ),
                false,
            ),
        };
        if !self.json {
            println!(
                ">>> {} {text}; onset {}",
                hhmmss(e.detected_utc_ns),
                hhmmss(e.onset_utc_ns)
            );
        }
        if markable {
            self.mark(e.onset_utc_ns, format!("watch: {text}"));
        }
        Ok(())
    }

    fn note(&mut self, text: &str) {
        let now = self.clock.now().utc_ns;
        if self.json {
            println!(
                "{}",
                serde_json::json!({ "type": "note", "utc_ns": now, "text": text, "saved": self.mark_root.is_some() })
            );
            self.mark(now, text.to_owned());
            return;
        }
        if self.mark_root.is_some() {
            self.mark(now, text.to_owned());
            println!("--- {} note saved: {text}", hhmmss(now));
        } else {
            println!("--- {} note (not saved; run with --mark): {text}", hhmmss(now));
        }
    }

    fn handle(&mut self, outs: Vec<WatchOutput>) -> Result<()> {
        for o in outs {
            match o {
                WatchOutput::Report(r) => self.report(&r)?,
                WatchOutput::Event(e) => self.event(&e)?,
            }
        }
        Ok(())
    }
}

fn line_cell(l: &LineReading) -> String {
    let flag = match l.state {
        LineState::Present => "*",
        LineState::Absent => " ",
        LineState::Masked => "~",
        LineState::Unknown => "?",
    };
    let label = match l.harmonic {
        Some(k) => format!("h{k}"),
        None => format!("{:.3}k", l.target_hz / 1e3),
    };
    format!("{label} {:+5.1}{flag}", l.excess_db)
}

pub async fn run(args: WatchArgs, json: bool) -> Result<std::process::ExitCode> {
    let cfg: Config = args.config.load()?;
    let targets = match args.f0 {
        Some(f0_hz) => Targets::Harmonics { f0_hz },
        None => Targets::Hz(args.hz.clone()),
    };
    let mut p = WatchParams::new(targets, args.pan.unwrap_or_else(|| cfg.radio.receiver.pan()));
    p.usable_fraction = cfg.collection.usable_fraction;
    p.interval = args.every;
    p.line_threshold_db = args.threshold;
    p.floor_rise_db = args.floor_rise;
    p.hold = args.hold;
    p.tolerance_hz = args.tolerance;
    p.background_hz = p.background_hz.max(4.0 * args.tolerance);
    let mut w = Watcher::new(p).map_err(refuse)?;
    let mark_root = if args.mark {
        let root = cfg.storage.root.clone();
        if !root.join(k4na_store::writer::ROOT_MARKER).is_file() {
            return Err(refuse(format!(
                "--mark needs an existing data root; {} has none (collect once, or omit --mark)",
                root.display()
            )));
        }
        Some(root)
    } else {
        None
    };
    let log = match &args.log {
        Some(path) => {
            let fresh = !path.exists();
            let mut f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .with_context(|| format!("open {}", path.display()))?;
            if fresh {
                writeln!(
                    f,
                    "start_utc,frames,center_hz,sample_rate_hz,floor_db,floor_rise_db,floor_state,target_hz,harmonic,peak_hz,level_db,background_db,excess_db,state"
                )?;
            }
            Some(f)
        }
        None => None,
    };
    let clock: Arc<dyn Clock> = Arc::new(TokioClock::real());
    let mut out = Out {
        json,
        events_only: args.events_only,
        log,
        mark_root,
        clock: clock.clone(),
        f0: args.f0,
        shown_view: None,
    };

    // Only the spectrum connector is used; the CAT one is never opened.
    let (_, pan_conn) = connectors(&cfg);
    let mut spectrum = StreamSpectrum::new(pan_conn, SpectrumOptions::from_config(&cfg.radio), clock);
    let (stop_handle, mut stop) = stop_pair();
    install_signals(stop_handle.clone());
    if let Some(d) = args.duration {
        let h = stop_handle.clone();
        tokio::spawn(async move {
            tokio::time::sleep(d).await;
            h.stop("duration reached");
        });
    }
    let (note_tx, mut notes) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            if note_tx.send(l).is_err() {
                break;
            }
        }
    });
    if !json {
        eprintln!(
            "watching pan {} (receive-only: spectrum stream only, no CAT); type a note and Enter to record it, Ctrl-C to stop",
            w.params().pan.as_str()
        );
        eprintln!(
            "excess = dB over local background (* present, ~ masked by a raised floor, ? not yet known); levels are the K4's uncalibrated scale, not dBm"
        );
    }
    let reason = loop {
        tokio::select! {
            r = stop.requested() => break r,
            Some(n) = notes.recv() => {
                let n = n.trim();
                if !n.is_empty() {
                    out.note(n);
                }
            }
            ev = spectrum.next() => match ev {
                SpectrumEvent::Frame(f) => out.handle(w.push(&f))?,
                SpectrumEvent::Connected => eprintln!("spectrum connected"),
                SpectrumEvent::Disconnected { reason, .. } => {
                    out.handle(w.flush())?;
                    eprintln!("spectrum: {reason}; reconnecting");
                }
            },
        }
    };
    out.handle(w.flush())?;
    if let Some(f) = &mut out.log {
        f.sync_all()?;
    }
    if !json {
        println!("ended: {reason}");
    }
    Ok(std::process::ExitCode::SUCCESS)
}
