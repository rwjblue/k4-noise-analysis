//! On/off episodes of the noise floor.
//!
//! A series is one tile's per-visit floor within one band, antenna and
//! comparability segment. Each series gets its own quiet baseline (a low
//! quantile of its floors) and noise estimate (from successive
//! differences, which a source switching on and off barely affects), and
//! is thresholded with hysteresis. Tile episodes on one band that overlap
//! in time merge into a band episode. Timing is bounded by the visits that
//! observed it: an episode began after `start_after_utc_ns` (the last
//! quiet visit) and by `start_utc_ns` (the first elevated one).
//!
//! An episode says the floor was raised, not why: a local device switching,
//! a propagation change and a nearby wideband transmission can all raise
//! it. Abrupt edges and repetition favour a switched local source; slow
//! edges favour propagation.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::background::VisitMetrics;
use crate::spectrum::VisitSpectrum;
use crate::{Params, db, quantile_i16};

/// A band-level interval during which one or more tiles' floors were
/// raised over their quiet baseline.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FloorEpisode {
    pub band_id: String,
    pub antenna_id: String,
    pub segment_id: String,
    /// Start of the first and end of the last elevated visit.
    pub start_utc_ns: i64,
    pub end_utc_ns: i64,
    /// End of the last quiet visit before, and start of the first quiet
    /// visit after (`None` at the edge of the data or of a gap).
    pub start_after_utc_ns: Option<i64>,
    pub end_before_utc_ns: Option<i64>,
    /// Elevated visits, the tiles they were on, and the tiles analyzed.
    pub visits: u64,
    pub tiles: u64,
    pub tiles_analyzed: u64,
    /// Rise over each visit's tile baseline.
    pub median_rise_db: f64,
    pub max_rise_db: f64,
}

/// The floor behaviour of one band, antenna and segment.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FloorActivity {
    pub band_id: String,
    pub antenna_id: String,
    pub segment_id: String,
    /// Tiles with enough visits to analyze, and those skipped.
    pub tiles: u64,
    pub tiles_skipped: u64,
    pub visits: u64,
    pub first_utc_ns: i64,
    pub last_utc_ns: i64,
    /// Medians over tiles: time between visits to one tile, the floor's
    /// visit-to-visit noise, and the rise that turns a tile on.
    pub revisit_s: f64,
    pub noise_db: f64,
    pub threshold_db: f64,
    pub episodes: u64,
    /// Fraction of analyzed visits that were elevated.
    pub elevated_fraction: f64,
    /// Medians over episodes (`None` with too few): duration, quiet time
    /// between episodes, and onset-to-onset interval.
    pub median_duration_s: Option<f64>,
    pub median_gap_s: Option<f64>,
    pub median_period_s: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
struct Obs {
    start: i64,
    end: i64,
    floor: i16,
}

type SeriesKey = (String, String, String);

#[derive(Debug, Default)]
pub(crate) struct Floors {
    series: BTreeMap<SeriesKey, BTreeMap<String, Vec<Obs>>>,
}

/// One tile's elevated run, as indices into its observations.
#[derive(Debug, Clone)]
struct TileEpisode {
    tile: usize,
    start: i64,
    end: i64,
    start_after: Option<i64>,
    end_before: Option<i64>,
    rises: Vec<i16>,
}

#[derive(Debug)]
struct TileResult {
    revisit_ns: f64,
    noise: f64,
    threshold: i16,
    visits: u64,
    elevated: u64,
    episodes: Vec<TileEpisode>,
}

fn median_f64(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    Some(v[(v.len() - 1) / 2])
}

/// Robust visit-to-visit noise: `1.4826 * median|d| / sqrt(2)` over
/// successive differences (native tenths).
fn successive_noise(floors: &[i16]) -> f64 {
    let mut d: Vec<f64> = floors.windows(2).map(|w| f64::from((w[1] - w[0]).abs())).collect();
    median_f64(&mut d).unwrap_or(0.0) * 1.4826 / std::f64::consts::SQRT_2
}

fn analyze_tile(tile: usize, obs: &[Obs], p: &Params) -> TileResult {
    let mut sorted: Vec<i16> = obs.iter().map(|o| o.floor).collect();
    let noise = successive_noise(&sorted);
    sorted.sort_unstable();
    let baseline = quantile_i16(&sorted, p.episode_baseline_quantile);
    let threshold = ((p.episode_noise_multiple * noise).round() as i16).max(p.episode_min_rise);
    let release = threshold / 2;
    let mut steps: Vec<f64> = obs.windows(2).map(|w| (w[1].start - w[0].start) as f64).collect();
    let revisit_ns = median_f64(&mut steps).unwrap_or(0.0);
    let gap_ns = p.episode_gap_revisits * revisit_ns;

    let mut episodes = Vec::new();
    let mut open: Option<TileEpisode> = None;
    let mut elevated = 0;
    for (i, o) in obs.iter().enumerate() {
        let gap = i > 0 && (o.start - obs[i - 1].end) as f64 > gap_ns;
        if gap && let Some(e) = open.take() {
            episodes.push(e); // ends unobserved: end_before stays None
        }
        let rise = o.floor - baseline;
        match &mut open {
            Some(e) if rise >= release => {
                e.end = o.end;
                e.rises.push(rise);
            }
            Some(_) => {
                let mut e = open.take().unwrap_or_else(|| unreachable!());
                e.end_before = Some(o.start);
                episodes.push(e);
            }
            None if rise >= threshold => {
                open = Some(TileEpisode {
                    tile,
                    start: o.start,
                    end: o.end,
                    start_after: (i > 0 && !gap).then(|| obs[i - 1].end),
                    end_before: None,
                    rises: vec![rise],
                });
            }
            None => {}
        }
        if open.is_some() {
            elevated += 1;
        }
    }
    episodes.extend(open);
    TileResult {
        revisit_ns,
        noise,
        threshold,
        visits: obs.len() as u64,
        elevated,
        episodes,
    }
}

impl Floors {
    pub fn add(&mut self, s: &VisitSpectrum, m: &VisitMetrics) {
        let v = &s.visit;
        let key = (
            v.band_id.clone().unwrap_or_else(|| "(no band)".into()),
            v.antenna_id.clone(),
            crate::analysis_segment(v),
        );
        let tile = v
            .tile_id
            .clone()
            .unwrap_or_else(|| format!("{}-{}", s.low_hz, s.high_hz));
        self.series.entry(key).or_default().entry(tile).or_default().push(Obs {
            start: v.started_utc_ns,
            end: v.ended_utc_ns,
            floor: m.floor,
        });
    }

    pub fn finish(self, p: &Params) -> (Vec<FloorActivity>, Vec<FloorEpisode>) {
        let (mut activity, mut out) = (Vec::new(), Vec::new());
        for ((band_id, antenna_id, segment_id), tiles) in self.series {
            let mut results = Vec::new();
            let mut skipped = 0;
            let (mut first, mut last) = (i64::MAX, i64::MIN);
            for (ti, (_, mut obs)) in tiles.into_iter().enumerate() {
                obs.sort_by_key(|o| o.start);
                if (obs.len() as u64) < p.episode_min_tile_visits {
                    skipped += 1;
                    continue;
                }
                first = first.min(obs[0].start);
                last = last.max(obs[obs.len() - 1].end);
                results.push(analyze_tile(ti, &obs, p));
            }
            if results.is_empty() {
                continue;
            }
            let revisit = median_f64(&mut results.iter().map(|r| r.revisit_ns).collect::<Vec<_>>()).unwrap_or(0.0);
            let eps = merge(
                results.iter().flat_map(|r| r.episodes.iter().cloned()).collect(),
                revisit * p.episode_merge_revisits,
            );
            let band: Vec<FloorEpisode> = eps
                .into_iter()
                .map(|m| {
                    let mut rises: Vec<i16> = m.iter().flat_map(|e| e.rises.iter().copied()).collect();
                    rises.sort_unstable();
                    let earliest = m.iter().min_by_key(|e| e.start).unwrap_or_else(|| unreachable!());
                    let latest = m.iter().max_by_key(|e| e.end).unwrap_or_else(|| unreachable!());
                    FloorEpisode {
                        band_id: band_id.clone(),
                        antenna_id: antenna_id.clone(),
                        segment_id: segment_id.clone(),
                        start_utc_ns: earliest.start,
                        end_utc_ns: latest.end,
                        start_after_utc_ns: earliest.start_after,
                        end_before_utc_ns: latest.end_before,
                        visits: rises.len() as u64,
                        tiles: m.iter().map(|e| e.tile).collect::<BTreeSet<_>>().len() as u64,
                        tiles_analyzed: results.len() as u64,
                        median_rise_db: db(rises[(rises.len() - 1) / 2]),
                        max_rise_db: db(rises[rises.len() - 1]),
                    }
                })
                .collect();
            let secs = |ns: i64| ns as f64 / 1e9;
            let mut dur: Vec<f64> = band.iter().map(|e| secs(e.end_utc_ns - e.start_utc_ns)).collect();
            let mut gaps: Vec<f64> = band
                .windows(2)
                .map(|w| secs(w[1].start_utc_ns - w[0].end_utc_ns))
                .collect();
            let mut period: Vec<f64> = band
                .windows(2)
                .map(|w| secs(w[1].start_utc_ns - w[0].start_utc_ns))
                .collect();
            let visits: u64 = results.iter().map(|r| r.visits).sum();
            activity.push(FloorActivity {
                band_id,
                antenna_id,
                segment_id,
                tiles: results.len() as u64,
                tiles_skipped: skipped,
                visits,
                first_utc_ns: first,
                last_utc_ns: last,
                revisit_s: revisit / 1e9,
                noise_db: median_f64(&mut results.iter().map(|r| r.noise / 10.0).collect::<Vec<_>>()).unwrap_or(0.0),
                threshold_db: median_f64(&mut results.iter().map(|r| db(r.threshold)).collect::<Vec<_>>())
                    .unwrap_or(0.0),
                episodes: band.len() as u64,
                elevated_fraction: results.iter().map(|r| r.elevated).sum::<u64>() as f64 / visits.max(1) as f64,
                median_duration_s: median_f64(&mut dur),
                median_gap_s: median_f64(&mut gaps),
                median_period_s: median_f64(&mut period),
            });
            out.extend(band);
        }
        (activity, out)
    }
}

/// Groups tile episodes whose intervals overlap or are within `slack_ns`.
fn merge(mut eps: Vec<TileEpisode>, slack_ns: f64) -> Vec<Vec<TileEpisode>> {
    eps.sort_by_key(|e| (e.start, e.tile));
    let mut out: Vec<Vec<TileEpisode>> = Vec::new();
    let mut end = i64::MIN;
    for e in eps {
        match out.last_mut() {
            Some(g) if (e.start - end) as f64 <= slack_ns => {
                end = end.max(e.end);
                g.push(e);
            }
            _ => {
                end = e.end;
                out.push(vec![e]);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000_000_000;

    fn series(floors: &[i16]) -> Vec<Obs> {
        floors
            .iter()
            .enumerate()
            .map(|(i, &f)| Obs {
                start: i as i64 * 4 * MIN,
                end: i as i64 * 4 * MIN + 5_000_000_000,
                floor: f,
            })
            .collect()
    }

    #[test]
    fn successive_noise_ignores_steps() {
        // +-5 tenths ripple with two 190-tenth steps.
        let mut f: Vec<i16> = (0..40).map(|i| -1270 + if i % 2 == 0 { 5 } else { -5 }).collect();
        for x in &mut f[10..20] {
            *x += 190;
        }
        let n = successive_noise(&f);
        assert!((n - 10.0 * 1.4826 / std::f64::consts::SQRT_2).abs() < 0.01, "{n}");
    }

    #[test]
    fn hysteresis_finds_bounded_episodes_and_breaks_at_gaps() {
        let mut f = vec![-1270i16; 30];
        for x in &mut f[5..8] {
            *x = -1080; // +19 dB
        }
        f[8] = -1250; // +2 dB: still above the 1.5 dB release, so still on
        for x in &mut f[20..23] {
            *x = -1240;
        }
        let mut obs = series(&f);
        // A long outage between visits 21 and 22.
        for o in &mut obs[22..] {
            o.start += 60 * MIN;
            o.end += 60 * MIN;
        }
        let p = Params::default();
        let r = analyze_tile(0, &obs, &p);
        assert_eq!(r.threshold, 30);
        assert_eq!(r.episodes.len(), 3, "{:#?}", r.episodes);
        let e = &r.episodes[0];
        assert_eq!((e.start, e.start_after), (obs[5].start, Some(obs[4].end)));
        assert_eq!((e.end, e.end_before), (obs[8].end, Some(obs[9].start)));
        assert_eq!(e.rises, [190, 190, 190, 20]);
        // The gap closes the second episode unobserved and opens a third
        // with an unknown start.
        assert_eq!((r.episodes[1].start, r.episodes[1].end_before), (obs[20].start, None));
        assert_eq!((r.episodes[2].start, r.episodes[2].start_after), (obs[22].start, None));
        assert_eq!(r.elevated, 4 + 2 + 1);
    }

    #[test]
    fn overlapping_tiles_merge_into_one_band_episode() {
        let ep = |tile, start, end| TileEpisode {
            tile,
            start,
            end,
            start_after: Some(start - MIN),
            end_before: Some(end + MIN),
            rises: vec![100],
        };
        let m = merge(
            vec![ep(1, 2 * MIN, 9 * MIN), ep(0, MIN, 8 * MIN), ep(0, 30 * MIN, 31 * MIN)],
            0.0,
        );
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].iter().map(|e| e.tile).collect::<Vec<_>>(), [0, 1]);
    }
}
