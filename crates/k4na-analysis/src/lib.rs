//! Offline analysis of k4na data roots. Reads versioned shards read-only
//! through `k4na-store`; never needs the radio and never modifies source
//! measurements. Every output is a *derived* result, labelled with its
//! algorithm version, parameters, inputs and the uncalibrated unit.
//!
//! See docs/analysis.md for the method and its interpretation limits.

pub mod background;
pub mod bands;
pub mod carriers;
pub mod spectrum;

use std::collections::BTreeSet;
use std::fmt::Write as _;

use k4na_store::StoreError;
use k4na_store::reader::DataSet;
use serde::Serialize;

pub use background::{Peak, VisitMetrics, analyze_visit, local_background};
pub use bands::BandHour;
pub use carriers::Carrier;
pub use spectrum::VisitSpectrum;

/// Bumped whenever an algorithm or default changes its output.
pub const ALGORITHM_VERSION: &str = "k4na-analysis/1";

/// What derived levels mean. Native K4 tenths of a dB divided by ten.
pub const LEVEL_UNITS: &str = "dB on the K4's uncalibrated LAN panadapter scale (native tenths / 10); not dBm";

/// Tunables. Thresholds are exploratory hypotheses, not standards.
#[derive(Debug, Clone, Serialize)]
pub struct Params {
    /// Neighbourhood and percentile of the rolling local background
    /// over each visit's per-bin median.
    pub background_window_hz: f64,
    pub background_block_hz: f64,
    pub background_percentile: f64,
    /// Minimum median excess over background for a peak (native tenths).
    pub peak_threshold: i16,
    /// Peaks wider than this are not considered carriers.
    pub max_carrier_width_hz: f64,
    /// Peaks within this distance (or 1.5 bins, if wider) are one carrier.
    pub carrier_tolerance_hz: f64,
    pub min_carrier_detections: u64,
    /// Below this many covering visits a carrier is not classified.
    pub min_carrier_exposure: u64,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            background_window_hz: 5_000.0,
            background_block_hz: 1_000.0,
            background_percentile: 0.2,
            peak_threshold: 80,
            max_carrier_width_hz: 300.0,
            carrier_tolerance_hz: 100.0,
            min_carrier_detections: 3,
            min_carrier_exposure: 6,
        }
    }
}

/// Which visits to analyze. Unset fields select everything.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Filter {
    pub band: Option<String>,
    pub antenna: Option<String>,
    pub session: Option<String>,
    /// Visit start time bounds, `[since, until)`.
    pub since_utc_ns: Option<i64>,
    pub until_utc_ns: Option<i64>,
}

impl Filter {
    pub fn accepts(&self, v: &k4na_store::reader::VisitRow) -> bool {
        self.band.as_ref().is_none_or(|b| v.band_id.as_ref() == Some(b))
            && self.antenna.as_ref().is_none_or(|a| &v.antenna_id == a)
            && self
                .session
                .as_ref()
                .is_none_or(|s| v.session_id.starts_with(s.as_str()))
            && self.since_utc_ns.is_none_or(|t| v.started_utc_ns >= t)
            && self.until_utc_ns.is_none_or(|t| v.started_utc_ns < t)
    }
}

/// Inputs and method behind a derived result.
#[derive(Debug, Clone, Serialize)]
pub struct Provenance {
    pub algorithm: &'static str,
    pub level_units: &'static str,
    pub source_units: Vec<String>,
    pub calibration: Vec<String>,
    pub params: Params,
    pub filter: Filter,
    /// Summary shard file names read, and any that were unreadable.
    pub shards: Vec<String>,
    pub unreadable_shards: Vec<String>,
    pub sessions: Vec<String>,
    pub profiles: Vec<String>,
    pub visits_analyzed: u64,
    /// Visits matching the filter that were not complete observations.
    pub visits_skipped: u64,
    pub first_utc_ns: Option<i64>,
    pub last_utc_ns: Option<i64>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Analysis {
    pub provenance: Provenance,
    pub band_hours: Vec<BandHour>,
    pub carriers: Vec<Carrier>,
}

/// Native tenths to (uncalibrated) dB.
pub fn db(tenths: i16) -> f64 {
    f64::from(tenths) / 10.0
}

/// Nearest-rank quantile of sorted values.
pub fn quantile_i16(sorted: &[i16], q: f64) -> i16 {
    if sorted.is_empty() {
        return 0;
    }
    sorted[((sorted.len() - 1) as f64 * q.clamp(0.0, 1.0)).round() as usize]
}

/// Analyzes every matching visit. `observe` sees each visit's spectrum,
/// metrics and local background (for renderers such as heatmaps).
pub fn run(
    ds: &DataSet,
    filter: &Filter,
    params: &Params,
    mut observe: impl FnMut(&VisitSpectrum, &VisitMetrics, &[i16]),
) -> Result<Analysis, StoreError> {
    let mut hours = bands::BandHours::default();
    let mut carriers = carriers::Carriers::default();
    let (mut analyzed, mut skipped) = (0u64, 0u64);
    let (mut first, mut last) = (None::<i64>, None::<i64>);
    let mut session_ids = BTreeSet::new();
    ds.for_each_summary(|v, stats| {
        if !filter.accepts(v) {
            return Ok(());
        }
        let Some(s) = VisitSpectrum::new(v, stats) else {
            skipped += 1;
            return Ok(());
        };
        let (m, bg) = analyze_visit(&s, params);
        hours.add(&s, &m);
        carriers.add(&s, &m.peaks, params);
        observe(&s, &m, &bg);
        analyzed += 1;
        session_ids.insert(v.session_id.clone());
        first = Some(first.map_or(v.started_utc_ns, |t| t.min(v.started_utc_ns)));
        last = Some(last.map_or(v.ended_utc_ns, |t| t.max(v.ended_utc_ns)));
        Ok(())
    })?;
    let sessions: Vec<_> = ds
        .sessions()?
        .into_iter()
        .filter(|s| session_ids.contains(&s.session_id))
        .collect();
    let mut carriers = carriers.finish(params);
    carriers.sort_by(|a, b| {
        (&a.band_id, &a.antenna_id)
            .cmp(&(&b.band_id, &b.antenna_id))
            .then(a.hz.total_cmp(&b.hz))
    });
    let name = |p: &std::path::Path| {
        p.file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    };
    Ok(Analysis {
        provenance: Provenance {
            algorithm: ALGORITHM_VERSION,
            level_units: LEVEL_UNITS,
            source_units: distinct(sessions.iter().map(|s| s.units.clone())),
            calibration: distinct(sessions.iter().map(|s| s.calibration.clone())),
            params: params.clone(),
            filter: filter.clone(),
            shards: ds
                .shards()
                .filter(|s| s.kind == "summary" && s.error.is_none())
                .map(|s| name(&s.path))
                .collect(),
            unreadable_shards: ds
                .shards()
                .filter(|s| s.error.is_some())
                .map(|s| name(&s.path))
                .collect(),
            sessions: sessions.iter().map(|s| s.session_id.clone()).collect(),
            profiles: distinct(sessions.iter().filter_map(|s| s.profile_id.clone())),
            visits_analyzed: analyzed,
            visits_skipped: skipped,
            first_utc_ns: first,
            last_utc_ns: last,
        },
        band_hours: hours.finish(),
        carriers,
    })
}

fn distinct(it: impl Iterator<Item = String>) -> Vec<String> {
    it.collect::<BTreeSet<_>>().into_iter().collect()
}

fn csv_field(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_owned()
    }
}

/// RFC 3339 UTC, millisecond precision.
pub fn utc(ns: i64) -> String {
    k4na_core::time::format_utc(ns)
}

/// Band/hour rows as CSV, one header line.
pub fn band_hours_csv(rows: &[BandHour]) -> String {
    let mut s = String::from(
        "band_id,antenna_id,segment_id,hour_start_utc,visits,observed_hz,floor_db,floor_p10_db,floor_p90_db,floor_min_db,floor_max_db,occupancy,variability_db,measurement\n",
    );
    for r in rows {
        let _ = writeln!(
            s,
            "{},{},{},{},{},{},{:.1},{:.1},{:.1},{:.1},{:.1},{:.4},{:.1},{}",
            csv_field(&r.band_id),
            csv_field(&r.antenna_id),
            r.segment_id,
            utc(r.hour_start_utc_ns),
            r.visits,
            r.observed_hz,
            r.floor_db,
            r.floor_p10_db,
            r.floor_p90_db,
            r.floor_min_db,
            r.floor_max_db,
            r.occupancy,
            r.variability_db,
            csv_field(r.measurement.as_deref().unwrap_or("")),
        );
    }
    s
}

/// Carrier rows as CSV, one header line.
pub fn carriers_csv(rows: &[Carrier]) -> String {
    let mut s = String::from(
        "band_id,antenna_id,hz,min_hz,max_hz,class,detections,exposure,persistence,steady_fraction,median_excess_db,max_excess_db,median_width_hz,segments,first_utc,last_utc\n",
    );
    for c in rows {
        let _ = writeln!(
            s,
            "{},{},{:.0},{:.0},{:.0},{},{},{},{:.3},{:.3},{:.1},{:.1},{:.0},{},{},{}",
            csv_field(&c.band_id),
            csv_field(&c.antenna_id),
            c.hz,
            c.min_hz,
            c.max_hz,
            c.class,
            c.detections,
            c.exposure,
            c.persistence,
            c.steady_fraction,
            c.median_excess_db,
            c.max_excess_db,
            c.median_width_hz,
            c.segments,
            utc(c.first_utc_ns),
            utc(c.last_utc_ns),
        );
    }
    s
}
