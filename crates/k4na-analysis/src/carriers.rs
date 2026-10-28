//! Narrow peaks recurring at nearly the same frequency across visits.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::background::Peak;
use crate::spectrum::VisitSpectrum;
use crate::{Params, db};

/// A cluster of narrow peaks at nearly one frequency on one band and
/// antenna. The class describes only temporal behaviour: a stationary
/// carrier is a *candidate* birdie, but broadcast, beacon and other real
/// transmissions can be stationary too. No source is implied.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Carrier {
    pub band_id: String,
    pub antenna_id: String,
    /// Median peak frequency and the spread of detected frequencies.
    pub hz: f64,
    pub min_hz: f64,
    pub max_hz: f64,
    /// Visits in which the peak was detected, and visits whose usable
    /// coverage included its frequency (detections included).
    pub detections: u64,
    pub exposure: u64,
    pub persistence: f64,
    pub median_excess_db: f64,
    pub max_excess_db: f64,
    /// Fraction of detections where p10 also exceeded the threshold: the
    /// carrier was present nearly the entire dwell.
    pub steady_fraction: f64,
    pub median_width_hz: f64,
    pub first_utc_ns: i64,
    pub last_utc_ns: i64,
    /// Detections and exposure per UTC hour of day.
    pub hour_detections: [u32; 24],
    pub hour_exposure: [u32; 24],
    pub segments: u64,
    pub class: &'static str,
}

#[derive(Debug, Clone)]
struct Detection {
    hz: f64,
    excess: i16,
    steady: bool,
    width_hz: f64,
    visit: usize,
}

#[derive(Debug, Clone)]
struct Seen {
    low_hz: i64,
    high_hz: i64,
    utc_ns: i64,
    segment: String,
    bin_hz: f64,
}

#[derive(Debug, Default)]
struct Group {
    visits: Vec<Seen>,
    detections: Vec<Detection>,
}

#[derive(Debug, Default)]
pub(crate) struct Carriers {
    groups: BTreeMap<(String, String), Group>,
}

fn hour_of_day(utc_ns: i64) -> usize {
    (utc_ns.div_euclid(3_600_000_000_000).rem_euclid(24)) as usize
}

impl Carriers {
    pub fn add(&mut self, s: &VisitSpectrum, peaks: &[Peak], p: &Params) {
        let v = &s.visit;
        let g = self
            .groups
            .entry((
                v.band_id.clone().unwrap_or_else(|| "(no band)".into()),
                v.antenna_id.clone(),
            ))
            .or_default();
        let idx = g.visits.len();
        g.visits.push(Seen {
            low_hz: s.low_hz,
            high_hz: s.high_hz,
            utc_ns: v.started_utc_ns,
            segment: crate::analysis_segment(v),
            bin_hz: s.bin_hz,
        });
        for pk in peaks.iter().filter(|pk| pk.width_hz() <= p.max_carrier_width_hz) {
            g.detections.push(Detection {
                hz: pk.hz,
                excess: pk.excess,
                steady: pk.p10_excess >= p.peak_threshold,
                width_hz: pk.width_hz(),
                visit: idx,
            });
        }
    }

    pub fn finish(self, p: &Params) -> Vec<Carrier> {
        let mut out = Vec::new();
        for ((band_id, antenna_id), mut g) in self.groups {
            let widest_bin = g.visits.iter().map(|s| s.bin_hz).fold(0.0, f64::max);
            let tol = p.carrier_tolerance_hz.max(1.5 * widest_bin);
            g.detections
                .sort_by(|a, b| a.hz.total_cmp(&b.hz).then(a.visit.cmp(&b.visit)));
            // Coverage by distinct visited range, with per-hour counts.
            let mut ranges: BTreeMap<(i64, i64), [u32; 24]> = BTreeMap::new();
            for s in &g.visits {
                ranges.entry((s.low_hz, s.high_hz)).or_default()[hour_of_day(s.utc_ns)] += 1;
            }
            let mut start = 0;
            while start < g.detections.len() {
                let mut end = start + 1;
                while end < g.detections.len() && g.detections[end].hz - g.detections[end - 1].hz <= tol {
                    end += 1;
                }
                if let Some(c) = cluster(&band_id, &antenna_id, &g, &g.detections[start..end], &ranges, tol, p) {
                    out.push(c);
                }
                start = end;
            }
        }
        out
    }
}

fn cluster(
    band_id: &str,
    antenna_id: &str,
    g: &Group,
    dets: &[Detection],
    ranges: &BTreeMap<(i64, i64), [u32; 24]>,
    tol: f64,
    p: &Params,
) -> Option<Carrier> {
    // One detection per visit: keep the strongest.
    let mut per_visit: BTreeMap<usize, &Detection> = BTreeMap::new();
    for d in dets {
        per_visit
            .entry(d.visit)
            .and_modify(|e| {
                if d.excess > e.excess {
                    *e = d;
                }
            })
            .or_insert(d);
    }
    let detections = per_visit.len() as u64;
    if detections < p.min_carrier_detections {
        return None;
    }
    let mut hz: Vec<f64> = per_visit.values().map(|d| d.hz).collect();
    hz.sort_by(f64::total_cmp);
    let median_hz = hz[(hz.len() - 1) / 2];
    let covers = |lo: i64, hi: i64| (lo as f64) <= median_hz && median_hz < hi as f64;
    let mut hour_exposure = [0u32; 24];
    let mut exposure = 0u64;
    for (&(lo, hi), hours) in ranges {
        if covers(lo, hi) {
            for (h, n) in hours.iter().enumerate() {
                hour_exposure[h] += n;
                exposure += u64::from(*n);
            }
        }
    }
    let mut hour_detections = [0u32; 24];
    let mut segments = BTreeSet::new();
    for &vi in per_visit.keys() {
        let s = &g.visits[vi];
        let h = hour_of_day(s.utc_ns);
        hour_detections[h] += 1;
        segments.insert(s.segment.as_str());
        // A detection at a coverage edge still counts as exposure.
        if !covers(s.low_hz, s.high_hz) {
            hour_exposure[h] += 1;
            exposure += 1;
        }
    }
    let mut excess: Vec<i16> = per_visit.values().map(|d| d.excess).collect();
    excess.sort_unstable();
    let mut widths: Vec<f64> = per_visit.values().map(|d| d.width_hz).collect();
    widths.sort_by(f64::total_cmp);
    let times = per_visit.keys().map(|&vi| g.visits[vi].utc_ns);
    let persistence = detections as f64 / exposure.max(1) as f64;
    let (min_hz, max_hz) = (hz[0], hz[hz.len() - 1]);
    let class = if exposure < p.min_carrier_exposure {
        "insufficient_exposure"
    } else if persistence >= 0.9 && max_hz - min_hz <= 2.0 * tol {
        "stationary"
    } else if persistence >= 0.25 {
        "recurring"
    } else {
        "occasional"
    };
    Some(Carrier {
        band_id: band_id.to_owned(),
        antenna_id: antenna_id.to_owned(),
        hz: median_hz,
        min_hz,
        max_hz,
        detections,
        exposure,
        persistence,
        median_excess_db: db(excess[(excess.len() - 1) / 2]),
        max_excess_db: db(excess[excess.len() - 1]),
        steady_fraction: per_visit.values().filter(|d| d.steady).count() as f64 / detections as f64,
        median_width_hz: widths[(widths.len() - 1) / 2],
        first_utc_ns: times.clone().min().unwrap_or_default(),
        last_utc_ns: times.max().unwrap_or_default(),
        hour_detections,
        hour_exposure,
        segments: segments.len() as u64,
        class,
    })
}
