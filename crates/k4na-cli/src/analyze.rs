//! `k4na analyze`: derived, read-only views of a data root.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::{Subcommand, ValueEnum};
use k4na_analysis::{Analysis, Filter, LEVEL_UNITS, Params, band_hours_csv, carriers_csv, utc};
use k4na_store::reader::DataSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Text,
    Csv,
    Json,
}

#[derive(Debug, clap::Args)]
pub struct Select {
    #[command(flatten)]
    pub filter: FilterArgs,
    #[arg(long, value_enum, default_value_t = Format::Text)]
    pub format: Format,
    /// Write to this file instead of stdout.
    #[arg(long)]
    pub out: Option<PathBuf>,
}

#[derive(Debug, clap::Args)]
pub struct FilterArgs {
    /// Data root to read (never modified).
    #[arg(long)]
    pub data: PathBuf,
    /// Only this survey range id (for example `40m`).
    #[arg(long)]
    pub band: Option<String>,
    /// Only this antenna id.
    #[arg(long)]
    pub antenna: Option<String>,
    /// Only sessions whose id starts with this prefix.
    #[arg(long)]
    pub session: Option<String>,
    /// Visits starting at or after this UTC time (RFC 3339).
    #[arg(long)]
    pub since: Option<String>,
    /// Visits starting before this UTC time (RFC 3339).
    #[arg(long)]
    pub until: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum AnalyzeCmd {
    /// Local background per band, antenna, receiver-settings segment and
    /// UTC hour.
    Bands {
        #[command(flatten)]
        select: Select,
    },
    /// Narrow peaks recurring at nearly fixed frequencies.
    Carriers {
        #[command(flatten)]
        select: Select,
        /// Only this class: stationary, recurring, occasional or
        /// insufficient_exposure.
        #[arg(long)]
        class: Option<String>,
        /// Text output: show at most this many carriers.
        #[arg(long, default_value_t = 40)]
        limit: usize,
    },
    /// Carriers on a common frequency grid (comb patterns).
    Families {
        #[command(flatten)]
        select: Select,
    },
}

fn parse_time(s: &str) -> Result<i64> {
    let t = humantime::parse_rfc3339_weak(s).with_context(|| format!("not an RFC 3339 time: {s}"))?;
    let d = t.duration_since(std::time::UNIX_EPOCH).context("time before 1970")?;
    i64::try_from(d.as_nanos()).context("time out of range")
}

impl FilterArgs {
    pub fn filter(&self) -> Result<Filter> {
        Ok(Filter {
            band: self.band.clone(),
            antenna: self.antenna.clone(),
            session: self.session.clone(),
            since_utc_ns: self.since.as_deref().map(parse_time).transpose()?,
            until_utc_ns: self.until.as_deref().map(parse_time).transpose()?,
        })
    }

    pub fn open(&self) -> Result<DataSet> {
        DataSet::open(&self.data).with_context(|| format!("open {}", self.data.display()))
    }
}

impl Select {
    pub fn analyze(&self) -> Result<Analysis> {
        let ds = self.filter.open()?;
        Ok(k4na_analysis::run(
            &ds,
            &self.filter.filter()?,
            &Params::default(),
            |_, _, _| {},
        )?)
    }

    fn emit(&self, text: &str) -> Result<()> {
        match &self.out {
            Some(p) => std::fs::write(p, text).with_context(|| format!("write {}", p.display())),
            None => {
                print!("{text}");
                Ok(())
            }
        }
    }
}

pub fn run(cmd: AnalyzeCmd, json: bool) -> Result<ExitCode> {
    let format = |s: &Select| if json { Format::Json } else { s.format };
    match cmd {
        AnalyzeCmd::Bands { select } => {
            let a = select.analyze()?;
            let out = match format(&select) {
                Format::Json => json_of(&a.provenance, "band_hours", serde_json::to_value(&a.band_hours)?)?,
                Format::Csv => band_hours_csv(&a.band_hours),
                Format::Text => bands_text(&a),
            };
            select.emit(&out)?;
        }
        AnalyzeCmd::Carriers { select, class, limit } => {
            let mut a = select.analyze()?;
            if let Some(c) = &class {
                a.carriers.retain(|x| x.class == c);
            }
            let out = match format(&select) {
                Format::Json => json_of(&a.provenance, "carriers", serde_json::to_value(&a.carriers)?)?,
                Format::Csv => carriers_csv(&a.carriers),
                Format::Text => carriers_text(&a, limit),
            };
            select.emit(&out)?;
        }
        AnalyzeCmd::Families { select } => {
            let a = select.analyze()?;
            let out = match format(&select) {
                Format::Json => json_of(&a.provenance, "families", serde_json::to_value(&a.families)?)?,
                Format::Csv => families_csv(&a.families),
                Format::Text => families_text(&a),
            };
            select.emit(&out)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `k4na report`: one self-contained HTML file.
pub fn report(f: &FilterArgs, out: &std::path::Path, json: bool) -> Result<ExitCode> {
    let ds = f.open()?;
    let now = k4na_core::time::system_utc_ns();
    let html = k4na_report::render(&ds, &f.filter()?, &Params::default(), now)?;
    std::fs::write(out, &html).with_context(|| format!("write {}", out.display()))?;
    if json {
        println!("{}", serde_json::json!({ "out": out, "bytes": html.len() }));
    } else {
        println!(
            "wrote {} ({} KiB); open it in a browser (no network needed)",
            out.display(),
            html.len() / 1024
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn json_of(p: &k4na_analysis::Provenance, key: &str, rows: serde_json::Value) -> Result<String> {
    let mut v = serde_json::json!({ "provenance": p });
    v[key] = rows;
    Ok(serde_json::to_string_pretty(&v)? + "\n")
}

fn header(a: &Analysis) -> String {
    let p = &a.provenance;
    let span = match (p.first_utc_ns, p.last_utc_ns) {
        (Some(f), Some(l)) => format!("{} .. {}", utc(f), utc(l)),
        _ => "no data".into(),
    };
    format!(
        "{} visits analyzed ({} not complete, skipped) from {} summary shard(s), {} session(s); {span}\n{}\n",
        p.visits_analyzed,
        p.visits_skipped,
        p.shards.len(),
        p.sessions.len(),
        p.algorithm,
    )
}

fn short(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn bands_text(a: &Analysis) -> String {
    let mut s = header(a);
    s += "band  antenna     segment   hour (UTC)         visits  floor  p10     p90     occ%   var\n";
    for r in &a.band_hours {
        s += &format!(
            "{:<5} {:<11} {:<9} {:<18} {:>6}  {:>6.1} {:>6.1} {:>6.1}  {:>5.1}  {:>4.1}\n",
            r.band_id,
            r.antenna_id,
            short(&r.segment_id),
            &utc(r.hour_start_utc_ns)[..16],
            r.visits,
            r.floor_db,
            r.floor_p10_db,
            r.floor_p90_db,
            r.occupancy * 100.0,
            r.variability_db,
        );
    }
    // Quietest and noisiest hour per band, antenna and segment: only
    // comparable within the same receiver settings.
    let mut by: BTreeMap<(&str, &str, &str), Vec<&k4na_analysis::BandHour>> = BTreeMap::new();
    for r in &a.band_hours {
        by.entry((&r.band_id, &r.antenna_id, &r.segment_id))
            .or_default()
            .push(r);
    }
    if by.values().any(|v| v.len() > 1) {
        s += "\nquietest / noisiest / most variable hour (same band, antenna and settings):\n";
        for ((band, ant, seg), rows) in by.into_iter().filter(|(_, v)| v.len() > 1) {
            let pick = |f: fn(&k4na_analysis::BandHour) -> f64, max: bool| {
                rows.iter()
                    .copied()
                    .reduce(|a, b| if (f(b) > f(a)) == max && f(b) != f(a) { b } else { a })
                    .map(|r| format!("{} ({:.1})", &utc(r.hour_start_utc_ns)[11..16], f(r)))
                    .unwrap_or_default()
            };
            s += &format!(
                "  {band:<5} {ant:<11} {:<9} {} / {} / {}\n",
                short(seg),
                pick(|r| r.floor_db, false),
                pick(|r| r.floor_db, true),
                pick(|r| r.variability_db, true),
            );
        }
    }
    s += &format!(
        "\nfloor = median local background; occ = bins >= 8 dB above it; var = median p90-p10.\nlevels: {LEVEL_UNITS}\n"
    );
    s
}

fn families_csv(rows: &[k4na_analysis::Family]) -> String {
    let mut s = String::from(
        "band_id,antenna_id,spacing_hz,offset_hz,first_hz,last_hz,members,grid_positions,fill,residual_rms_hz,false_alarm,median_member_persistence\n",
    );
    for f in rows {
        s += &format!(
            "{},{},{:.1},{:.1},{:.0},{:.0},{},{},{:.3},{:.1},{:.2e},{:.3}\n",
            f.band_id,
            f.antenna_id,
            f.spacing_hz,
            f.offset_hz,
            f.first_hz,
            f.last_hz,
            f.members.len(),
            f.grid_positions,
            f.fill,
            f.residual_rms_hz,
            f.false_alarm,
            f.median_member_persistence,
        );
    }
    s
}

fn families_text(a: &Analysis) -> String {
    let mut s = header(a);
    s += &format!(
        "{} famil(ies) among {} carrier(s)\n",
        a.families.len(),
        a.carriers.len()
    );
    for f in &a.families {
        s += &format!(
            "{} {}: spacing {:.3} kHz, {} members on {} grid lines ({:.0}% filled), {:.3}-{:.3} MHz, rms {:.0} Hz, chance {:.1e}, median persistence {:.0}%\n",
            f.band_id,
            f.antenna_id,
            f.spacing_hz / 1000.0,
            f.members.len(),
            f.grid_positions,
            f.fill * 100.0,
            f.first_hz / 1e6,
            f.last_hz / 1e6,
            f.residual_rms_hz,
            f.false_alarm,
            f.median_member_persistence * 100.0,
        );
    }
    s += "\nA family is a frequency pattern (harmonics of one switching or clock source are a common cause),\nnot a source identification. chance = expected families this strong from unrelated carriers.\n";
    s
}

fn carriers_text(a: &Analysis, limit: usize) -> String {
    let mut s = header(a);
    let mut rows: Vec<_> = a.carriers.iter().collect();
    rows.sort_by(|x, y| {
        y.persistence
            .total_cmp(&x.persistence)
            .then(y.median_excess_db.total_cmp(&x.median_excess_db))
    });
    s += &format!("{} carrier(s); strongest persistence first\n", rows.len());
    s += "band  antenna     freq (kHz)    class                  seen/covered  pers%  steady%  excess  width\n";
    for c in rows.iter().take(limit) {
        s += &format!(
            "{:<5} {:<11} {:>12.3}  {:<21}  {:>5}/{:<6}  {:>5.1}  {:>6.1}  {:>5.1}  {:>4.0}\n",
            c.band_id,
            c.antenna_id,
            c.hz / 1000.0,
            c.class,
            c.detections,
            c.exposure,
            c.persistence * 100.0,
            c.steady_fraction * 100.0,
            c.median_excess_db,
            c.median_width_hz,
        );
    }
    if rows.len() > limit {
        s += &format!(
            "... {} more (use --limit, --format csv or --json)\n",
            rows.len() - limit
        );
    }
    s += "\nclass describes timing only; a stationary carrier is a candidate birdie, NOT a confirmed\nlocal source (broadcast, beacons and other stations can be stationary too).\n";
    s += &format!("excess = median dB above local background; levels: {LEVEL_UNITS}\n");
    s
}
