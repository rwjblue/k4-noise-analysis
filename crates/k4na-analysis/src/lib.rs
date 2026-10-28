//! Offline analysis of k4na data roots. Reads versioned shards read-only
//! through `k4na-store`; never needs the radio and never modifies source
//! measurements. Every output is a *derived* result, labelled with its
//! algorithm version, parameters, inputs and the uncalibrated unit.
//!
//! See docs/analysis.md for the method and its interpretation limits.

pub mod background;
pub mod bands;
pub mod carriers;
pub mod episodes;
pub mod families;
pub mod spectrum;
pub mod timing;

use std::collections::BTreeSet;
use std::fmt::Write as _;

use k4na_store::StoreError;
use k4na_store::reader::DataSet;
use serde::Serialize;

pub use background::{Peak, VisitMetrics, analyze_visit, local_background};
pub use bands::BandHour;
pub use carriers::Carrier;
pub use episodes::{FloorActivity, FloorEpisode};
pub use families::{Family, find_families};
pub use spectrum::VisitSpectrum;
pub use timing::{Feature, TimingGroup};

/// Bumped whenever an algorithm or default changes its output.
/// 2: comparability segments are split by delivered frame rate.
pub const ALGORITHM_VERSION: &str = "k4na-analysis/3";

/// The comparability segment analysis uses for a visit: the stored
/// segment plus the delivered frame rate, as `<segment_id>@<n>fps` (or
/// `@?fps` without two frames). Shards written before the writer keyed
/// segments by frame rate merged visits at different rates; this splits
/// them, and is redundant but harmless for newer shards.
pub fn analysis_segment(v: &k4na_store::reader::VisitRow) -> String {
    match v.frame_rate() {
        Some(f) => format!("{}@{f}fps", v.segment_id),
        None => format!("{}@?fps", v.segment_id),
    }
}

/// A segment label for people: the first 8 hex digits and the rate.
pub fn short_segment(id: &str) -> String {
    let (seg, rate) = id.split_once('@').unwrap_or((id, ""));
    let seg = seg.get(..8).unwrap_or(seg);
    if rate.is_empty() {
        seg.to_owned()
    } else {
        format!("{seg}@{rate}")
    }
}

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
    /// Comb search: grid spacing bounds, minimum members, the minimum
    /// fraction of grid positions occupied, within `carrier_tolerance_hz`.
    pub family_min_spacing_hz: f64,
    pub family_max_spacing_hz: f64,
    pub family_min_members: u64,
    pub family_min_fill: f64,
    /// Largest accepted expected count of chance families (see
    /// `Family::false_alarm`).
    pub family_max_false_alarm: f64,
    /// Floor episodes: a tile's quiet baseline is this quantile of its
    /// floors; it turns on at `max(min_rise, noise_multiple * noise)`
    /// over it and off below half that. Tiles with fewer visits are not
    /// analyzed; a pause over `gap_revisits` revisit intervals ends an
    /// episode unobserved; tile episodes within `merge_revisits` revisit
    /// intervals of each other form one band episode.
    pub episode_baseline_quantile: f64,
    pub episode_noise_multiple: f64,
    pub episode_min_rise: i16,
    pub episode_min_tile_visits: u64,
    pub episode_gap_revisits: f64,
    pub episode_merge_revisits: f64,
    /// Timing groups: two features join when the phi coefficient of their
    /// on/off signals is at least `min_phi` and at least
    /// `min_matched_edges` onsets/ends coincide one-to-one.
    pub timing_min_phi: f64,
    pub timing_min_matched_edges: u64,
    /// Carrier timelines bridge this many consecutive missed detections.
    pub timing_max_missed: u64,
    /// A covering visit without a detection counts as "carrier absent"
    /// only if the carrier, at its median detected level, would have
    /// cleared `peak_threshold` by this much over that visit's local
    /// background (native tenths); otherwise it was masked.
    pub carrier_mask_margin: i16,
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
            family_min_spacing_hz: 2_000.0,
            family_max_spacing_hz: 250_000.0,
            family_min_members: 4,
            family_min_fill: 0.5,
            family_max_false_alarm: 0.01,
            episode_baseline_quantile: 0.2,
            episode_noise_multiple: 5.0,
            episode_min_rise: 30,
            episode_min_tile_visits: 8,
            episode_gap_revisits: 3.0,
            episode_merge_revisits: 1.0,
            timing_min_phi: 0.7,
            timing_min_matched_edges: 3,
            timing_max_missed: 1,
            carrier_mask_margin: 20,
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
    /// Carriers on a common frequency grid, per band and antenna.
    pub families: Vec<Family>,
    /// Floor on/off behaviour and episodes per band, antenna and segment.
    pub floor_activity: Vec<FloorActivity>,
    pub floor_episodes: Vec<FloorEpisode>,
    /// Floors and carriers that switch on and off together.
    pub timing_groups: Vec<TimingGroup>,
}

/// Timing features: each band floor with its episodes, and each carrier
/// with its presence (and family, if any).
fn timing_features(
    activity: &[FloorActivity],
    episodes: &[FloorEpisode],
    carriers: &[Carrier],
    families: &[Family],
) -> Vec<Feature> {
    let mut out = Vec::new();
    for a in activity {
        let eps = episodes
            .iter()
            .filter(|e| (&e.band_id, &e.antenna_id, &e.segment_id) == (&a.band_id, &a.antenna_id, &a.segment_id))
            .map(|e| (e.start_after_utc_ns, e.start_utc_ns, e.end_utc_ns, e.end_before_utc_ns));
        let t = timing::Timeline::from_bounded(a.first_utc_ns, a.last_utc_ns, a.revisit_s * 1e9, eps);
        out.push(Feature {
            kind: "floor",
            band_id: a.band_id.clone(),
            antenna_id: a.antenna_id.clone(),
            segment_id: Some(a.segment_id.clone()),
            hz: None,
            family_spacing_hz: None,
            episodes: a.episodes,
            on_fraction: t.on_fraction(),
            timeline: t,
        });
    }
    for c in carriers {
        let family = families.iter().find(|f| {
            f.band_id == c.band_id && f.antenna_id == c.antenna_id && f.members.iter().any(|&m| (m - c.hz).abs() < 0.5)
        });
        out.push(Feature {
            kind: "carrier",
            band_id: c.band_id.clone(),
            antenna_id: c.antenna_id.clone(),
            segment_id: None,
            hz: Some(c.hz),
            family_spacing_hz: family.map(|f| f.spacing_hz),
            episodes: c.timeline.on.len() as u64,
            on_fraction: c.timeline.on_fraction(),
            timeline: c.timeline.clone(),
        });
    }
    out
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
    let mut floors = episodes::Floors::default();
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
        carriers.add(&s, &m.peaks, &bg, params);
        floors.add(&s, &m);
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
    let (floor_activity, mut floor_episodes) = floors.finish(params);
    floor_episodes
        .sort_by(|a, b| (a.start_utc_ns, &a.band_id, &a.antenna_id).cmp(&(b.start_utc_ns, &b.band_id, &b.antenna_id)));
    carriers.sort_by(|a, b| {
        (&a.band_id, &a.antenna_id)
            .cmp(&(&b.band_id, &b.antenna_id))
            .then(a.hz.total_cmp(&b.hz))
    });
    let families = find_families(&carriers, params);
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
        timing_groups: timing::group(
            timing_features(&floor_activity, &floor_episodes, &carriers, &families),
            params,
        ),
        families,
        carriers,
        floor_activity,
        floor_episodes,
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

fn opt_utc(ns: Option<i64>) -> String {
    ns.map(utc).unwrap_or_default()
}

fn opt_f64(v: Option<f64>) -> String {
    v.map(|v| format!("{v:.0}")).unwrap_or_default()
}

/// Floor episodes as CSV, one header line. Empty bounds are unobserved.
pub fn floor_episodes_csv(rows: &[FloorEpisode]) -> String {
    let mut s = String::from(
        "band_id,antenna_id,segment_id,start_after_utc,start_utc,end_utc,end_before_utc,visits,tiles,tiles_analyzed,median_rise_db,max_rise_db\n",
    );
    for e in rows {
        let _ = writeln!(
            s,
            "{},{},{},{},{},{},{},{},{},{},{:.1},{:.1}",
            csv_field(&e.band_id),
            csv_field(&e.antenna_id),
            e.segment_id,
            opt_utc(e.start_after_utc_ns),
            utc(e.start_utc_ns),
            utc(e.end_utc_ns),
            opt_utc(e.end_before_utc_ns),
            e.visits,
            e.tiles,
            e.tiles_analyzed,
            e.median_rise_db,
            e.max_rise_db,
        );
    }
    s
}

/// Floor activity per band, antenna and segment as CSV.
pub fn floor_activity_csv(rows: &[FloorActivity]) -> String {
    let mut s = String::from(
        "band_id,antenna_id,segment_id,tiles,tiles_skipped,visits,first_utc,last_utc,revisit_s,noise_db,threshold_db,episodes,elevated_fraction,median_duration_s,median_gap_s,median_period_s\n",
    );
    for a in rows {
        let _ = writeln!(
            s,
            "{},{},{},{},{},{},{},{},{:.0},{:.2},{:.1},{},{:.3},{},{},{}",
            csv_field(&a.band_id),
            csv_field(&a.antenna_id),
            a.segment_id,
            a.tiles,
            a.tiles_skipped,
            a.visits,
            utc(a.first_utc_ns),
            utc(a.last_utc_ns),
            a.revisit_s,
            a.noise_db,
            a.threshold_db,
            a.episodes,
            a.elevated_fraction,
            opt_f64(a.median_duration_s),
            opt_f64(a.median_gap_s),
            opt_f64(a.median_period_s),
        );
    }
    s
}

/// Timing groups as CSV: one row per member, numbered by group.
pub fn timing_groups_csv(rows: &[TimingGroup]) -> String {
    let mut s = String::from(
        "group,group_members,min_phi,max_phi,median_period_s,kind,band_id,antenna_id,segment_id,hz,family_spacing_hz,episodes,on_fraction\n",
    );
    for (i, g) in rows.iter().enumerate() {
        for m in &g.members {
            let _ = writeln!(
                s,
                "{},{},{:.3},{:.3},{},{},{},{},{},{},{},{},{:.3}",
                i + 1,
                g.members.len(),
                g.min_phi,
                g.max_phi,
                opt_f64(g.median_period_s),
                m.kind,
                csv_field(&m.band_id),
                csv_field(&m.antenna_id),
                m.segment_id.as_deref().unwrap_or(""),
                opt_f64(m.hz),
                m.family_spacing_hz.map(|f| format!("{f:.1}")).unwrap_or_default(),
                m.episodes,
                m.on_fraction,
            );
        }
    }
    s
}

/// Carrier rows as CSV, one header line.
pub fn carriers_csv(rows: &[Carrier]) -> String {
    let mut s = String::from(
        "band_id,antenna_id,hz,min_hz,max_hz,class,detections,exposure,masked,persistence,steady_fraction,median_level_db,median_excess_db,max_excess_db,median_width_hz,segments,first_utc,last_utc\n",
    );
    for c in rows {
        let _ = writeln!(
            s,
            "{},{},{:.0},{:.0},{:.0},{},{},{},{},{:.3},{:.3},{:.1},{:.1},{:.1},{:.0},{},{},{}",
            csv_field(&c.band_id),
            csv_field(&c.antenna_id),
            c.hz,
            c.min_hz,
            c.max_hz,
            c.class,
            c.detections,
            c.exposure,
            c.masked,
            c.persistence,
            c.steady_fraction,
            c.median_level_db,
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
