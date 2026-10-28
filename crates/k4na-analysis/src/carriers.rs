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
    /// coverage included its frequency and in which it could have been
    /// detected (detections included).
    pub detections: u64,
    pub exposure: u64,
    /// Covering visits left out of `exposure` because the local background
    /// had risen too far for the carrier, at its median detected level, to
    /// clear the detection threshold by `carrier_mask_margin`: masked, so
    /// neither present nor absent.
    pub masked: u64,
    pub persistence: f64,
    /// Median absolute level of the detections (native scale, dB).
    pub median_level_db: f64,
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
    /// Presence across the visits that covered it (for timing groups).
    #[serde(skip)]
    pub timeline: crate::timing::Timeline,
}

#[derive(Debug, Clone)]
struct Detection {
    hz: f64,
    excess: i16,
    /// Background plus excess: the carrier's own level.
    level: i16,
    steady: bool,
    width_hz: f64,
    visit: usize,
}

#[derive(Debug, Clone)]
struct Seen {
    low_hz: i64,
    high_hz: i64,
    utc_ns: i64,
    end_ns: i64,
    segment: String,
    bin_hz: f64,
    /// The visit's local background every `bg_step_hz` from `bg_first_hz`
    /// (it is smooth over a background block, so a stride loses little).
    bg: Vec<i16>,
    bg_first_hz: f64,
    bg_step_hz: f64,
}

impl Seen {
    fn background_at(&self, hz: f64) -> i16 {
        let i = ((hz - self.bg_first_hz) / self.bg_step_hz).round().max(0.0) as usize;
        self.bg
            .get(i.min(self.bg.len().saturating_sub(1)))
            .copied()
            .unwrap_or(0)
    }
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
    /// `bg` is the visit's local background per bin (from
    /// [`crate::background::analyze_visit`]).
    pub fn add(&mut self, s: &VisitSpectrum, peaks: &[Peak], bg: &[i16], p: &Params) {
        let v = &s.visit;
        let g = self
            .groups
            .entry((
                v.band_id.clone().unwrap_or_else(|| "(no band)".into()),
                v.antenna_id.clone(),
            ))
            .or_default();
        let idx = g.visits.len();
        let stride = ((p.background_block_hz / s.bin_hz / 2.0).round() as usize).max(1);
        let bin_of =
            |hz: f64| (((hz - s.first_bin_hz) / s.bin_hz).round().max(0.0) as usize).min(bg.len().saturating_sub(1));
        g.visits.push(Seen {
            low_hz: s.low_hz,
            high_hz: s.high_hz,
            utc_ns: v.started_utc_ns,
            end_ns: v.ended_utc_ns,
            segment: crate::analysis_segment(v),
            bin_hz: s.bin_hz,
            bg: bg.iter().step_by(stride).copied().collect(),
            bg_first_hz: s.first_bin_hz,
            bg_step_hz: s.bin_hz * stride as f64,
        });
        for pk in peaks.iter().filter(|pk| pk.width_hz() <= p.max_carrier_width_hz) {
            let under = bg.get(bin_of(pk.hz)).copied().unwrap_or(0);
            g.detections.push(Detection {
                hz: pk.hz,
                excess: pk.excess,
                level: under.saturating_add(pk.excess),
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
            let mut start = 0;
            while start < g.detections.len() {
                let mut end = start + 1;
                while end < g.detections.len() && g.detections[end].hz - g.detections[end - 1].hz <= tol {
                    end += 1;
                }
                if let Some(c) = cluster(&band_id, &antenna_id, &g, &g.detections[start..end], tol, p) {
                    out.push(c);
                }
                start = end;
            }
        }
        out
    }
}

fn cluster(band_id: &str, antenna_id: &str, g: &Group, dets: &[Detection], tol: f64, p: &Params) -> Option<Carrier> {
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
    let mut levels: Vec<i16> = per_visit.values().map(|d| d.level).collect();
    levels.sort_unstable();
    let level = levels[(levels.len() - 1) / 2];
    let need = i32::from(p.peak_threshold) + i32::from(p.carrier_mask_margin);
    // Per visit: Some(detected), or None when it covered the carrier but
    // the background hid it; visits not covering it are left out.
    let state = |i: usize, s: &Seen| -> Option<Option<bool>> {
        if per_visit.contains_key(&i) {
            // A detection at a coverage edge still counts as exposure.
            Some(Some(true))
        } else if !covers(s.low_hz, s.high_hz) {
            None
        } else if i32::from(level) - i32::from(s.background_at(median_hz)) < need {
            Some(None)
        } else {
            Some(Some(false))
        }
    };
    let mut hour_exposure = [0u32; 24];
    let mut hour_detections = [0u32; 24];
    let (mut exposure, mut masked) = (0u64, 0u64);
    let mut segments = BTreeSet::new();
    let mut presence: Vec<(i64, i64, Option<bool>)> = Vec::new();
    for (i, s) in g.visits.iter().enumerate() {
        let Some(seen) = state(i, s) else { continue };
        presence.push((s.utc_ns, s.end_ns, seen));
        let h = hour_of_day(s.utc_ns);
        match seen {
            None => masked += 1,
            Some(on) => {
                exposure += 1;
                hour_exposure[h] += 1;
                if on {
                    hour_detections[h] += 1;
                    segments.insert(s.segment.as_str());
                }
            }
        }
    }
    let mut excess: Vec<i16> = per_visit.values().map(|d| d.excess).collect();
    excess.sort_unstable();
    let mut widths: Vec<f64> = per_visit.values().map(|d| d.width_hz).collect();
    widths.sort_by(f64::total_cmp);
    let times = per_visit.keys().map(|&vi| g.visits[vi].utc_ns);
    presence.sort_by_key(|&(s, e, _)| (s, e));
    let timeline = crate::timing::Timeline::from_observations(&presence, p);
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
        masked,
        persistence,
        median_level_db: db(level),
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
        timeline,
    })
}
