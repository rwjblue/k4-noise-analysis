//! `k4na recovery`: inspect and settle survey recovery journals
//! (docs/recovery.md).

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use k4na_collector::cat::{ControlCat, ReadOnlyCat, RetuneConsent};
use k4na_collector::net::Connector;
use k4na_collector::recovery::{
    self, Entry, JournalInfo, JournalStatus, Ledger, Outcome, RestoreReport, Transaction, manual_steps,
};
use k4na_collector::runtime::TokioClock;
use k4na_core::Config;
use k4na_core::time::{Clock, format_utc};

use crate::{ConfigArgs, connectors, refuse};

#[derive(Debug, clap::Subcommand)]
pub enum RecoveryCmd {
    /// List journals and whether they block new surveys.
    List {
        #[command(flatten)]
        config: ConfigArgs,
    },
    /// Show what a session changed and what is not yet back.
    Show {
        /// Session id, or a unique prefix of one.
        session: String,
        #[command(flatten)]
        config: ConfigArgs,
    },
    /// Restore what a session changed, if the radio still holds the
    /// collector's values. Requires --allow-retune AND
    /// `collection.allow_retune = true`, or --dry-run (GET only).
    Restore {
        session: String,
        #[command(flatten)]
        config: ConfigArgs,
        #[arg(long)]
        allow_retune: bool,
        #[arg(long, conflicts_with = "allow_retune")]
        dry_run: bool,
    },
    /// Accept the radio's current state and resolve the journal (after
    /// fixing it by hand, or when the changes no longer matter).
    Acknowledge {
        session: String,
        #[command(flatten)]
        config: ConfigArgs,
        /// Why (kept in the journal).
        #[arg(long, default_value = "acknowledged by operator")]
        note: String,
    },
}

fn find(dir: &Path, session: &str) -> Result<JournalInfo> {
    let all = recovery::list(dir)?;
    let matches: Vec<JournalInfo> = all.into_iter().filter(|j| j.session_id.starts_with(session)).collect();
    match matches.len() {
        1 => Ok(matches.into_iter().next().expect("one match")),
        0 => Err(refuse(format!(
            "no recovery journal for session {session:?} in {}",
            dir.display()
        ))),
        n => Err(refuse(format!(
            "{n} journals match {session:?}; give more of the session id"
        ))),
    }
}

fn when(ns: Option<i64>) -> String {
    ns.map_or_else(|| "?".to_owned(), format_utc)
}

pub async fn run(cmd: RecoveryCmd, json: bool) -> Result<ExitCode> {
    match cmd {
        RecoveryCmd::List { config } => list(&config.load()?.storage.recovery_dir(), json),
        RecoveryCmd::Show { session, config } => show(&find(&config.load()?.storage.recovery_dir(), &session)?, json),
        RecoveryCmd::Restore {
            session,
            config,
            allow_retune,
            dry_run,
        } => {
            let cfg = config.load()?;
            let info = find(&cfg.storage.recovery_dir(), &session)?;
            if dry_run {
                restore_dry_run(&cfg, &info, json).await
            } else {
                restore(&cfg, &info, allow_retune, json).await
            }
        }
        RecoveryCmd::Acknowledge { session, config, note } => {
            acknowledge(&find(&config.load()?.storage.recovery_dir(), &session)?, note, json)
        }
    }
}

fn list(dir: &Path, json: bool) -> Result<ExitCode> {
    let all = recovery::list(dir)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&all)?);
        return Ok(ExitCode::SUCCESS);
    }
    println!("recovery journals in {}", dir.display());
    if all.is_empty() {
        println!("  none");
    }
    for j in &all {
        println!(
            "  {} {} {:<12} {} changed setting(s) pending{}{}",
            j.session_id,
            when(j.started_utc_ns),
            j.status.as_str(),
            j.dirty,
            j.outcome
                .map_or(String::new(), |o| format!("; last outcome {}", o.as_str())),
            if j.status.blocks_surveys() {
                "; BLOCKS new surveys"
            } else {
                ""
            }
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn load(info: &JournalInfo) -> Result<Ledger> {
    if info.status == JournalStatus::Corrupt {
        bail!(refuse(format!(
            "{} is corrupt ({}); restore the radio by hand, then remove or acknowledge it",
            info.path.display(),
            info.message.as_deref().unwrap_or("?")
        )));
    }
    recovery::load(&info.path).with_context(|| format!("reading {}", info.path.display()))
}

fn show(info: &JournalInfo, json: bool) -> Result<ExitCode> {
    let ledger = load(info)?;
    let manual = manual_steps(&ledger);
    if json {
        println!(
            "{}",
            serde_json::json!({ "journal": info, "ledger": ledger, "manual": manual })
        );
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "session {} (profile {})",
        info.session_id,
        info.profile.as_deref().unwrap_or("?")
    );
    println!("journal {}", info.path.display());
    println!("started {}; status {}", when(info.started_utc_ns), info.status.as_str());
    if let Some(m) = &info.message {
        println!("last outcome: {} ({m})", info.outcome.map_or("?", Outcome::as_str));
    }
    if info.torn_tail {
        println!("note: the last journal line was torn by a crash; its change was never sent");
    }
    let dirty = ledger.dirty();
    if dirty.is_empty() {
        println!("nothing the collector changed is pending");
    }
    let meta = ledger.meta.unwrap_or(k4na_collector::measure::MetaMode::Basic);
    for (scope, field, ob) in dirty {
        let ours: Vec<String> = ob.ours.iter().map(|v| recovery::wire_value(&field, meta, v)).collect();
        println!(
            "  pending: {} {field}: found {field}{}, collector set {}",
            scope.describe(),
            ob.original,
            ours.join(" or ")
        );
    }
    for m in &manual {
        println!("  manual: {m}");
    }
    Ok(ExitCode::SUCCESS)
}

fn print_report(r: &RestoreReport, json: bool) {
    if json {
        println!("{}", serde_json::to_string(r).unwrap_or_default());
        return;
    }
    if r.dry_run {
        println!("dry run (GET only): {}", r.message);
    } else {
        println!("restore {}: {}", r.outcome.as_str(), r.message);
    }
    for f in &r.fields {
        if f.result == "already_original" {
            continue;
        }
        println!(
            "  {:<16} {} {} (found {}{}, now {})",
            f.result,
            f.scope.describe(),
            f.label,
            f.field,
            f.original,
            f.current
                .as_deref()
                .map_or("unknown".to_owned(), |c| format!("{}{c}", f.field))
        );
    }
    for m in &r.manual {
        println!("  manual: {m}");
    }
    if !r.dry_run && r.outcome != Outcome::Complete {
        println!("  when the radio is right, run `k4na recovery acknowledge <session>`; a retry is safe");
    }
}

fn exit_for(o: Outcome) -> ExitCode {
    match o {
        Outcome::Complete => ExitCode::SUCCESS,
        Outcome::Conflict => ExitCode::from(3),
        _ => ExitCode::from(1),
    }
}

async fn restore_dry_run(cfg: &Config, info: &JournalInfo, json: bool) -> Result<ExitCode> {
    let ledger = load(info)?;
    let (cat_conn, _) = connectors(cfg);
    let stream = cat_conn
        .connect()
        .await
        .with_context(|| format!("CAT {}", cat_conn.describe()))?;
    let mut cat = ReadOnlyCat::new(stream, Duration::from_millis(cfg.radio.cat_timeout_ms));
    let r = recovery::plan(&ledger, &mut cat).await.map_err(refuse)?;
    print_report(&r, json);
    Ok(ExitCode::SUCCESS)
}

async fn restore(cfg: &Config, info: &JournalInfo, allow_retune: bool, json: bool) -> Result<ExitCode> {
    let consent = RetuneConsent::new(cfg.collection.allow_retune, allow_retune).map_err(refuse)?;
    load(info)?;
    if matches!(info.status, JournalStatus::Resolved | JournalStatus::Acknowledged) {
        bail!(refuse(format!(
            "session {} is already {}",
            info.session_id,
            info.status.as_str()
        )));
    }
    let clock: Arc<dyn Clock> = Arc::new(TokioClock::real());
    let mut txn = Transaction::resume(&info.path, clock).map_err(|e| refuse(e.to_string()))?;
    let (cat_conn, _) = connectors(cfg);
    let stream = cat_conn
        .connect()
        .await
        .map_err(|e| refuse(format!("CAT {}: {e}", cat_conn.describe())))?;
    let mut cat = ControlCat::new(stream, Duration::from_millis(cfg.radio.cat_timeout_ms), consent);
    eprintln!("RESTORING radio state changed by survey {}", info.session_id);
    let r = recovery::restore(&mut txn, &mut cat).await;
    print_report(&r, json);
    Ok(exit_for(r.outcome))
}

fn acknowledge(info: &JournalInfo, note: String, json: bool) -> Result<ExitCode> {
    if info.status == JournalStatus::Corrupt {
        // Unreadable, so nothing can be appended; move it aside.
        let to: PathBuf = info.path.with_extension("jsonl.acknowledged-corrupt");
        std::fs::rename(&info.path, &to).with_context(|| format!("renaming {}", info.path.display()))?;
        println!("moved corrupt journal to {}", to.display());
        return Ok(ExitCode::SUCCESS);
    }
    let clock: Arc<dyn Clock> = Arc::new(TokioClock::real());
    let mut txn = Transaction::resume(&info.path, clock).map_err(|e| refuse(e.to_string()))?;
    let pending = manual_steps(txn.ledger());
    txn.record(Entry::Acknowledged { note: note.clone() })?;
    if json {
        println!(
            "{}",
            serde_json::json!({ "session": info.session_id, "acknowledged": true, "note": note, "was_pending": pending })
        );
    } else {
        println!(
            "session {} acknowledged; new surveys are no longer blocked by it",
            info.session_id
        );
        for m in pending {
            println!("  (was pending) {m}");
        }
    }
    Ok(ExitCode::SUCCESS)
}
