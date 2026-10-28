//! `k4na import-marks`: operator marks from device logs (thermostat
//! runtime, energy-monitor power, smart-plug history) exported as CSV.
//!
//! Two shapes: `--text COL` makes each row a mark (a moment, or an
//! interval with `--end`); `--on COL` turns a numeric column into marks
//! for the intervals where it is above `--above`. Times without a zone
//! need `--tz` (an IANA zone, so daylight-saving changes are right, or a
//! fixed offset). Marks are deduplicated against the file, so importing
//! the same export twice adds nothing.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Args;
use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::{Offset, TimeZone};
use k4na_core::time::format_utc;
use k4na_store::annotations::{self, Annotation};

use crate::refuse;

#[derive(Debug, Args)]
pub struct ImportArgs {
    /// Data root to add marks to.
    #[arg(long)]
    data: PathBuf,
    /// CSV export to read.
    file: PathBuf,
    /// Time column; several (`Date,Time`) are joined with a space.
    #[arg(long, value_delimiter = ',', required = true)]
    time: Vec<String>,
    /// Zone for times without one: IANA (`America/New_York`) or an offset
    /// (`-04:00`).
    #[arg(long)]
    tz: Option<String>,
    /// strftime format of the (joined) time, when it is not ISO 8601 or a
    /// common US form (`%m/%d/%Y %I:%M %p`).
    #[arg(long)]
    format: Option<String>,
    /// Each row is a mark with this column's text (empty rows skipped).
    #[arg(long, conflicts_with = "on", required_unless_present = "on")]
    text: Option<String>,
    /// With --text: end-time column(s), making each row an interval.
    #[arg(long, value_delimiter = ',', requires = "text")]
    end: Vec<String>,
    /// Numeric column; each run of rows above --above becomes one interval
    /// mark. Repeatable.
    #[arg(long)]
    on: Vec<String>,
    /// Threshold for --on (strictly above is on).
    #[arg(long, default_value_t = 0.0, allow_hyphen_values = true)]
    above: f64,
    /// Text for --on marks; default the column name.
    #[arg(long, requires = "on")]
    label: Option<String>,
    /// Join --on intervals separated by at most this.
    #[arg(long, default_value = "0s", value_parser = humantime::parse_duration)]
    merge_gap: Duration,
    /// Drop --on intervals shorter than this.
    #[arg(long, default_value = "0s", value_parser = humantime::parse_duration)]
    min_duration: Duration,
    /// Show the marks without writing them.
    #[arg(long)]
    dry_run: bool,
}

/// Parses a zone argument.
pub fn parse_zone(s: &str) -> Result<TimeZone> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("utc") || t == "Z" {
        return Ok(TimeZone::UTC);
    }
    if let Some(rest) = t.strip_prefix(['+', '-']) {
        let digits: String = rest.chars().filter(|c| *c != ':').collect();
        let (h, m) = match digits.len() {
            2 => (digits.parse::<i32>()?, 0),
            4 => (digits[..2].parse::<i32>()?, digits[2..].parse::<i32>()?),
            _ => bail!("bad offset {s}; use +HH:MM or -HH:MM"),
        };
        anyhow::ensure!(h <= 23 && m <= 59, "bad offset {s}");
        let secs = (h * 3600 + m * 60) * if t.starts_with('-') { -1 } else { 1 };
        return Ok(TimeZone::fixed(Offset::from_seconds(secs)?));
    }
    TimeZone::get(t).with_context(|| format!("unknown time zone {s}"))
}

const COMMON_FORMATS: &[&str] = &[
    "%Y-%m-%d %H:%M:%S",
    "%Y-%m-%d %H:%M",
    "%m/%d/%Y %H:%M:%S",
    "%m/%d/%Y %H:%M",
    "%m/%d/%Y %I:%M:%S %p",
    "%m/%d/%Y %I:%M %p",
    "%Y/%m/%d %H:%M:%S",
    "%Y/%m/%d %H:%M",
];

/// How a parsed time was placed on the UTC line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// It carried its own offset.
    Explicit,
    Zoned,
    /// A wall-clock time that occurs twice (DST fall-back); the earlier was
    /// taken.
    Ambiguous,
    /// A wall-clock time that does not exist (DST spring-forward).
    Skipped,
}

/// Parses one time; `tz` places times without an offset.
pub fn parse_device_time(s: &str, format: Option<&str>, tz: Option<&TimeZone>) -> Result<(i64, Placement)> {
    let s = s.trim();
    let civil = |dt: DateTime| -> Result<(i64, Placement)> {
        let Some(tz) = tz else {
            bail!("{s} has no time zone; pass --tz (the device's zone, such as America/New_York)");
        };
        let amb = tz.to_ambiguous_zoned(dt);
        let placement = if amb.is_ambiguous() {
            match amb.offset() {
                jiff::tz::AmbiguousOffset::Gap { .. } => Placement::Skipped,
                _ => Placement::Ambiguous,
            }
        } else {
            Placement::Zoned
        };
        let z = amb.compatible()?;
        Ok((i64::try_from(z.timestamp().as_nanosecond())?, placement))
    };
    if let Some(f) = format {
        let tm = jiff::fmt::strtime::parse(f, s).with_context(|| format!("{s} does not match {f}"))?;
        if tm.offset().is_some() {
            return Ok((i64::try_from(tm.to_timestamp()?.as_nanosecond())?, Placement::Explicit));
        }
        return civil(tm.to_datetime()?);
    }
    if let Ok(t) = s.parse::<Timestamp>() {
        return Ok((i64::try_from(t.as_nanosecond())?, Placement::Explicit));
    }
    if let Ok(dt) = s.parse::<DateTime>() {
        return civil(dt);
    }
    for f in COMMON_FORMATS {
        if let Ok(dt) = jiff::fmt::strtime::parse(f, s).and_then(|tm| tm.to_datetime()) {
            return civil(dt);
        }
    }
    bail!("cannot read {s:?} as a time; pass --format")
}

/// Rows with a time and the selected cells, after the header row (the
/// first one naming every wanted column, so preambles are skipped).
struct Table {
    rows: Vec<Vec<String>>,
    index: std::collections::HashMap<String, usize>,
}

fn read_table(path: &Path, wanted: &[&str]) -> Result<Table> {
    let mut r = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_path(path)
        .with_context(|| format!("open {}", path.display()))?;
    let mut header: Option<std::collections::HashMap<String, usize>> = None;
    let mut rows = Vec::new();
    for rec in r.records() {
        let rec = rec.with_context(|| format!("read {}", path.display()))?;
        let cells: Vec<String> = rec
            .iter()
            .map(|c| c.trim().trim_start_matches('\u{feff}').to_owned())
            .collect();
        match &header {
            None => {
                if wanted.iter().all(|w| cells.iter().any(|c| c == w)) {
                    header = Some(cells.iter().enumerate().rev().map(|(i, c)| (c.clone(), i)).collect());
                }
            }
            Some(_) => rows.push(cells),
        }
    }
    let Some(index) = header else {
        bail!(
            "no header row in {} names all of: {}",
            path.display(),
            wanted.join(", ")
        );
    };
    Ok(Table { rows, index })
}

impl Table {
    fn cell<'a>(&self, row: &'a [String], col: &str) -> &'a str {
        self.index.get(col).and_then(|&i| row.get(i)).map_or("", String::as_str)
    }

    fn joined(&self, row: &[String], cols: &[String]) -> String {
        cols.iter().map(|c| self.cell(row, c)).collect::<Vec<_>>().join(" ")
    }
}

#[derive(Debug, Default)]
struct Counts {
    unreadable: Vec<String>,
    ambiguous: u64,
    skipped_dst: u64,
}

fn place(s: &str, a: &ImportArgs, tz: Option<&TimeZone>, n: &mut Counts) -> Result<Option<i64>> {
    if s.trim().is_empty() {
        return Ok(None);
    }
    match parse_device_time(s, a.format.as_deref(), tz) {
        Ok((t, p)) => {
            match p {
                Placement::Ambiguous => n.ambiguous += 1,
                Placement::Skipped => n.skipped_dst += 1,
                _ => {}
            }
            Ok(Some(t))
        }
        Err(e) if e.to_string().contains("no time zone") => Err(refuse(e.to_string())),
        Err(e) => {
            n.unreadable.push(e.to_string());
            Ok(None)
        }
    }
}

/// Intervals where `values` (time-sorted `(utc_ns, value)`; `None` =
/// unreadable) are above `above`. Each row covers the time to the next
/// row, or the median spacing at the end and across data gaps (spacing
/// over 3 x median).
pub fn on_intervals(values: &[(i64, Option<f64>)], above: f64, merge_gap_ns: i64, min_ns: i64) -> Vec<(i64, i64)> {
    if values.is_empty() {
        return Vec::new();
    }
    let mut steps: Vec<i64> = values.windows(2).map(|w| w[1].0 - w[0].0).filter(|d| *d > 0).collect();
    steps.sort_unstable();
    let median = steps.get(steps.len() / 2).copied().unwrap_or(0);
    let mut out: Vec<(i64, i64)> = Vec::new();
    for (i, &(t, v)) in values.iter().enumerate() {
        if !v.is_some_and(|v| v > above) {
            continue;
        }
        let next = values.get(i + 1).map(|n| n.0 - t);
        let cover = match next {
            Some(d) if d > 0 && (median == 0 || d <= 3 * median) => d,
            _ => median,
        };
        let end = t + cover;
        match out.last_mut() {
            Some(last) if t - last.1 <= merge_gap_ns => last.1 = last.1.max(end),
            _ => out.push((t, end)),
        }
    }
    out.retain(|(a, b)| b - a >= min_ns);
    out
}

pub fn run(a: ImportArgs, json: bool) -> Result<std::process::ExitCode> {
    if !a.data.join(k4na_store::writer::ROOT_MARKER).is_file() {
        return Err(refuse(format!(
            "{} is not a k4na data root; marks are only written beside measurements",
            a.data.display()
        )));
    }
    let tz =
        a.tz.as_deref()
            .map(parse_zone)
            .transpose()
            .map_err(|e| refuse(e.to_string()))?;
    let mut wanted: Vec<&str> = a.time.iter().map(String::as_str).collect();
    wanted.extend(a.end.iter().map(String::as_str));
    wanted.extend(a.text.as_deref());
    wanted.extend(a.on.iter().map(String::as_str));
    let table = read_table(&a.file, &wanted).map_err(|e| refuse(format!("{e:#}")))?;
    let file_name = a
        .file
        .file_name()
        .map_or_else(|| a.file.display().to_string(), |n| n.to_string_lossy().into_owned());
    let source = format!("import:{file_name}");
    let mut n = Counts::default();
    let mut marks: Vec<(i64, Option<i64>, String)> = Vec::new();

    if let Some(text_col) = &a.text {
        for row in &table.rows {
            let text = table.cell(row, text_col).trim();
            if text.is_empty() {
                continue;
            }
            let Some(start) = place(&table.joined(row, &a.time), &a, tz.as_ref(), &mut n)? else {
                continue;
            };
            let end = if a.end.is_empty() {
                None
            } else {
                place(&table.joined(row, &a.end), &a, tz.as_ref(), &mut n)?.filter(|e| *e >= start)
            };
            marks.push((start, end, text.chars().take(annotations::MAX_TEXT_CHARS).collect()));
        }
    } else {
        let mut times = Vec::new();
        for row in &table.rows {
            times.push(place(&table.joined(row, &a.time), &a, tz.as_ref(), &mut n)?);
        }
        for col in &a.on {
            let mut values: Vec<(i64, Option<f64>)> = table
                .rows
                .iter()
                .zip(&times)
                .filter_map(|(row, t)| {
                    let v = table.cell(row, col).replace(',', "");
                    t.map(|t| (t, v.trim().parse::<f64>().ok()))
                })
                .collect();
            values.sort_by_key(|v| v.0);
            let ns = |d: Duration| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX);
            let label = a.label.clone().unwrap_or_else(|| col.clone());
            for (s, e) in on_intervals(&values, a.above, ns(a.merge_gap), ns(a.min_duration)) {
                marks.push((s, Some(e), format!("{label} on ({col} > {})", a.above)));
            }
        }
    }

    let (existing, _) = annotations::read(&a.data)?;
    let mut seen: HashSet<(i64, Option<i64>, String)> = existing
        .into_iter()
        .map(|m| (m.start_utc_ns, m.end_utc_ns, m.text))
        .collect();
    let now = k4na_core::time::system_utc_ns();
    let mut added = Vec::new();
    let mut duplicates = 0u64;
    for (start, end, text) in marks {
        if !seen.insert((start, end, text.clone())) {
            duplicates += 1;
            continue;
        }
        let m = Annotation {
            start_utc_ns: start,
            end_utc_ns: end,
            text,
            created_utc_ns: now,
            source: Some(source.clone()),
        };
        if !a.dry_run {
            annotations::append(&a.data, &m).map_err(|e| refuse(e.to_string()))?;
        }
        added.push(m);
    }
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "dry_run": a.dry_run,
                "added": added,
                "duplicates": duplicates,
                "unreadable_times": n.unreadable.len(),
                "ambiguous_times": n.ambiguous,
                "nonexistent_times": n.skipped_dst,
            }))?
        );
        return Ok(std::process::ExitCode::SUCCESS);
    }
    for m in &added {
        let when = match m.end_utc_ns {
            Some(e) => format!("{} .. {}", format_utc(m.start_utc_ns), format_utc(e)),
            None => format_utc(m.start_utc_ns),
        };
        println!("{when}  {}", m.text);
    }
    println!(
        "{} {} mark(s) from {} ({} duplicate(s) skipped)",
        if a.dry_run { "would add" } else { "added" },
        added.len(),
        a.file.display(),
        duplicates
    );
    if !n.unreadable.is_empty() {
        eprintln!(
            "warning: {} unreadable time(s), rows skipped; first: {}",
            n.unreadable.len(),
            n.unreadable[0]
        );
    }
    if n.ambiguous > 0 {
        eprintln!(
            "warning: {} wall-clock time(s) occur twice (daylight-saving fall-back); the earlier was used",
            n.ambiguous
        );
    }
    if n.skipped_dst > 0 {
        eprintln!(
            "warning: {} wall-clock time(s) do not exist (daylight-saving spring-forward); moved forward",
            n.skipped_dst
        );
    }
    Ok(std::process::ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: i64 = 1_000_000_000;

    #[test]
    fn device_times_with_zones_and_dst() {
        let ny = parse_zone("America/New_York").unwrap();
        // EDT (-4) before 2026-11-01 02:00 local, EST (-5) after.
        let (t, p) = parse_device_time("2026-10-28 02:15:00", None, Some(&ny)).unwrap();
        assert_eq!((t, p), (1_793_168_100 * S, Placement::Zoned));
        let (t2, _) = parse_device_time("10/28/2026 2:15 AM", None, Some(&ny)).unwrap();
        assert_eq!(t2, t);
        let (t3, p3) = parse_device_time("2026-10-28T06:15:00Z", None, None).unwrap();
        assert_eq!((t3, p3), (t, Placement::Explicit));
        let (t4, _) = parse_device_time("2026-11-02 02:15", None, Some(&ny)).unwrap();
        assert_eq!(t4 - t, (5 * 24 * 3600 + 3600) * S, "EST after the change");
        let (_, amb) = parse_device_time("2026-11-01 01:30", None, Some(&ny)).unwrap();
        assert_eq!(amb, Placement::Ambiguous);
        let (_, gap) = parse_device_time("2027-03-14 02:30", None, Some(&ny)).unwrap();
        assert_eq!(gap, Placement::Skipped);
        let fixed = parse_zone("-04:00").unwrap();
        assert_eq!(parse_device_time("2026-10-28 02:15", None, Some(&fixed)).unwrap().0, t);
        let (t5, _) = parse_device_time("28.10.2026 02h15", Some("%d.%m.%Y %Hh%M"), Some(&ny)).unwrap();
        assert_eq!(t5, t);
        assert!(
            parse_device_time("2026-10-28 02:15", None, None)
                .unwrap_err()
                .to_string()
                .contains("no time zone")
        );
        assert!(parse_device_time("soon", None, Some(&ny)).is_err());
        assert!(parse_zone("Mars/Olympus").is_err());
    }

    #[test]
    fn runs_above_threshold_become_intervals() {
        let m = 60 * S;
        let v = |k: i64, x: Option<f64>| (k * 5 * m, x);
        // 5-minute rows: on at 0-1, 3, 5 (unreadable 4), then a data gap after 7.
        let rows = [
            v(0, Some(300.0)),
            v(1, Some(120.0)),
            v(2, Some(0.0)),
            v(3, Some(30.0)),
            v(4, None),
            v(5, Some(10.0)),
            v(6, Some(0.0)),
            v(7, Some(50.0)),
            v(20, Some(0.0)),
        ];
        assert_eq!(
            on_intervals(&rows, 0.0, 0, 0),
            vec![(0, 10 * m), (15 * m, 20 * m), (25 * m, 30 * m), (35 * m, 40 * m)]
        );
        // Gaps of up to 5 min merge (all of them here); the 5 min ones are
        // shorter than 6 min.
        assert_eq!(on_intervals(&rows, 0.0, 5 * m, 0), vec![(0, 40 * m)]);
        assert_eq!(on_intervals(&rows, 0.0, 4 * m, 6 * m), vec![(0, 10 * m)]);
        assert_eq!(on_intervals(&rows, 100.0, 0, 0), vec![(0, 10 * m)]);
        assert!(on_intervals(&[], 0.0, 0, 0).is_empty());
    }
}
