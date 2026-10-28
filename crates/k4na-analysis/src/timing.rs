//! Grouping features that switch on and off together.
//!
//! A feature is anything with an on/off timeline: a band's raised floor
//! (from [`crate::episodes`]) or a carrier's presence across the visits
//! that covered it. Features whose timelines agree, by a chance-corrected
//! correlation and several coincident edges, are grouped: a *timing
//! group* is a hypothesis that one switched source (or one propagation
//! change) drives them all, not a source identification.

use serde::Serialize;

use crate::Params;

/// An on/off timeline estimated from observations.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Timeline {
    /// Observed span.
    pub first_utc_ns: i64,
    pub last_utc_ns: i64,
    /// Median time between observations.
    pub revisit_ns: f64,
    /// Estimated on intervals: each episode extended halfway to the
    /// observations bounding it.
    pub on: Vec<(i64, i64)>,
    /// Estimated times of observed transitions (onsets and ends whose
    /// bounding quiet observation exists), sorted.
    pub edges: Vec<i64>,
    /// Estimated onsets only.
    pub onsets: Vec<i64>,
}

/// An episode with its bounds: `(start_after, start, end, end_before)`.
pub type Bounded = (Option<i64>, i64, i64, Option<i64>);

impl Timeline {
    pub fn from_bounded(first: i64, last: i64, revisit_ns: f64, eps: impl IntoIterator<Item = Bounded>) -> Self {
        let mut t = Self {
            first_utc_ns: first,
            last_utc_ns: last,
            revisit_ns,
            ..Self::default()
        };
        for (after, start, end, before) in eps {
            let s = after.map_or(start, |a| a + (start - a) / 2);
            let e = before.map_or(end, |b| end + (b - end) / 2);
            t.on.push((s, e.max(s)));
            if after.is_some() {
                t.edges.push(s);
                t.onsets.push(s);
            }
            if before.is_some() {
                t.edges.push(e);
            }
        }
        t.on.sort_unstable();
        t.edges.sort_unstable();
        t.onsets.sort_unstable();
        t
    }

    /// From `(start, end, on)` observations in time order: runs of `on`
    /// are episodes, bridging up to `timing_max_missed` consecutive off
    /// observations (a detector flickering near its threshold); a pause
    /// over `episode_gap_revisits` revisits leaves the bounds on either
    /// side unobserved.
    pub fn from_observations(obs: &[(i64, i64, bool)], p: &Params) -> Self {
        let Some((&(first, _, _), &(_, last, _))) = obs.first().zip(obs.last()) else {
            return Self::default();
        };
        let mut steps: Vec<i64> = obs.windows(2).map(|w| w[1].0 - w[0].0).collect();
        steps.sort_unstable();
        let revisit = steps.get(steps.len().saturating_sub(1) / 2).copied().unwrap_or(0) as f64;
        let gap = |i: usize| i > 0 && (obs[i].0 - obs[i - 1].1) as f64 > p.episode_gap_revisits * revisit;
        let mut eps: Vec<Bounded> = Vec::new();
        let mut open: Option<Bounded> = None;
        // Start of the first off observation after the last on one, and
        // how many off observations have followed it.
        let (mut pending, mut misses) = (None::<i64>, 0u64);
        for (i, &(s, e, on)) in obs.iter().enumerate() {
            if gap(i)
                && let Some(mut x) = open.take()
            {
                x.3 = pending;
                eps.push(x);
            }
            match (&mut open, on) {
                (Some(x), true) => x.2 = e,
                (Some(_), false) => {
                    pending = pending.or(Some(s));
                    misses += 1;
                    if misses > p.timing_max_missed
                        && let Some(mut x) = open.take()
                    {
                        x.3 = pending;
                        eps.push(x);
                    }
                }
                (None, true) => open = Some(((i > 0 && !gap(i)).then(|| obs[i - 1].1), s, e, None)),
                (None, false) => {}
            }
            if on || open.is_none() {
                (pending, misses) = (None, 0);
            }
        }
        if let Some(mut x) = open {
            x.3 = pending;
            eps.push(x);
        }
        Self::from_bounded(first, last, revisit, eps)
    }

    /// Fraction of the observed span estimated on.
    pub fn on_fraction(&self) -> f64 {
        let span = (self.last_utc_ns - self.first_utc_ns).max(1) as f64;
        self.on.iter().map(|(s, e)| (e - s) as f64).sum::<f64>() / span
    }
}

/// Total length of `on` within `[lo, hi)`, and of the intersection of
/// two such sets.
fn clipped(on: &[(i64, i64)], lo: i64, hi: i64) -> Vec<(i64, i64)> {
    on.iter()
        .map(|&(s, e)| (s.max(lo), e.min(hi)))
        .filter(|(s, e)| e > s)
        .collect()
}

fn total(on: &[(i64, i64)]) -> i64 {
    on.iter().map(|(s, e)| e - s).sum()
}

fn intersection(a: &[(i64, i64)], b: &[(i64, i64)]) -> i64 {
    let (mut i, mut j, mut t) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        let (s, e) = (a[i].0.max(b[j].0), a[i].1.min(b[j].1));
        if e > s {
            t += e - s;
        }
        if a[i].1 < b[j].1 { i += 1 } else { j += 1 }
    }
    t
}

/// How two timelines agree over the span both observed.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Agreement {
    /// Phi coefficient of the two on/off signals (1 = identical, 0 =
    /// independent); `None` when either is constant over the span.
    pub phi: Option<f64>,
    /// Edges of one matched one-to-one to edges of the other within the
    /// combined revisit uncertainty.
    pub matched_edges: u64,
}

pub fn agreement(a: &Timeline, b: &Timeline) -> Agreement {
    let (lo, hi) = (a.first_utc_ns.max(b.first_utc_ns), a.last_utc_ns.min(b.last_utc_ns));
    let tol = ((a.revisit_ns + b.revisit_ns) / 2.0) as i64;
    let in_span = |t: &&i64| (lo - tol..=hi + tol).contains(*t);
    let (ea, eb): (Vec<i64>, Vec<i64>) = (
        a.edges.iter().filter(in_span).copied().collect(),
        b.edges.iter().filter(in_span).copied().collect(),
    );
    let (mut i, mut j, mut matched) = (0, 0, 0);
    while i < ea.len() && j < eb.len() {
        if (ea[i] - eb[j]).abs() <= tol {
            matched += 1;
            i += 1;
            j += 1;
        } else if ea[i] < eb[j] {
            i += 1;
        } else {
            j += 1;
        }
    }
    let span = hi - lo;
    let phi = (span > 0)
        .then(|| {
            let (ca, cb) = (clipped(&a.on, lo, hi), clipped(&b.on, lo, hi));
            let t = span as f64;
            let (pa, pb) = (total(&ca) as f64 / t, total(&cb) as f64 / t);
            let p11 = intersection(&ca, &cb) as f64 / t;
            let d = (pa * (1.0 - pa) * pb * (1.0 - pb)).sqrt();
            (d > 1e-9).then(|| (p11 - pa * pb) / d)
        })
        .flatten();
    Agreement {
        phi,
        matched_edges: matched,
    }
}

/// A feature that took part in grouping.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Feature {
    /// `floor` or `carrier`.
    pub kind: &'static str,
    pub band_id: String,
    pub antenna_id: String,
    /// Floors: the comparability segment.
    pub segment_id: Option<String>,
    /// Carriers: median frequency, and the spacing of the family it
    /// belongs to, if any.
    pub hz: Option<f64>,
    pub family_spacing_hz: Option<f64>,
    pub episodes: u64,
    pub on_fraction: f64,
    #[serde(skip)]
    pub timeline: Timeline,
}

/// Features whose on/off timing agrees.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TimingGroup {
    pub members: Vec<Feature>,
    /// Weakest and strongest agreement among the pairs that joined it.
    pub min_phi: f64,
    pub max_phi: f64,
    /// Onsets of the member with the most observed edges, and the
    /// median onset-to-onset interval (`None` with fewer than 2).
    pub onsets_utc_ns: Vec<i64>,
    pub median_period_s: Option<f64>,
    pub bands: Vec<String>,
}

fn find(parent: &mut [usize], i: usize) -> usize {
    let mut r = i;
    while parent[r] != r {
        r = parent[r];
    }
    let mut i = i;
    while parent[i] != r {
        let n = parent[i];
        parent[i] = r;
        i = n;
    }
    r
}

/// Groups features with `phi >= timing_min_phi` and at least
/// `timing_min_matched_edges` matched edges. Only features with at least
/// that many observed edges take part; groups need two members.
pub fn group(features: Vec<Feature>, p: &Params) -> Vec<TimingGroup> {
    let features: Vec<Feature> = features
        .into_iter()
        .filter(|f| f.timeline.edges.len() as u64 >= p.timing_min_matched_edges)
        .collect();
    let n = features.len();
    let mut parent: Vec<usize> = (0..n).collect();
    let mut links: Vec<(usize, f64)> = Vec::new();
    for i in 0..n {
        for j in i + 1..n {
            let ag = agreement(&features[i].timeline, &features[j].timeline);
            if let Some(phi) = ag.phi
                && phi >= p.timing_min_phi
                && ag.matched_edges >= p.timing_min_matched_edges
            {
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                parent[ri.max(rj)] = ri.min(rj);
                links.push((i, phi));
            }
        }
    }
    let roots: Vec<usize> = (0..n).map(|i| find(&mut parent, i)).collect();
    let mut by_root: std::collections::BTreeMap<usize, Vec<usize>> = Default::default();
    for (i, &r) in roots.iter().enumerate() {
        by_root.entry(r).or_default().push(i);
    }
    let mut out = Vec::new();
    for (root, idx) in by_root {
        if idx.len() < 2 {
            continue;
        }
        let phis: Vec<f64> = links
            .iter()
            .filter(|(i, _)| roots[*i] == root)
            .map(|&(_, phi)| phi)
            .collect();
        let members: Vec<Feature> = idx.iter().map(|&i| features[i].clone()).collect();
        // Floors are measured over many bins and flicker least.
        let anchor = members
            .iter()
            .max_by_key(|m| (m.kind == "floor", m.timeline.edges.len()))
            .unwrap_or_else(|| unreachable!());
        let onsets = anchor.timeline.onsets.clone();
        let mut periods: Vec<f64> = onsets.windows(2).map(|w| (w[1] - w[0]) as f64 / 1e9).collect();
        periods.sort_by(f64::total_cmp);
        let mut bands: Vec<String> = members.iter().map(|m| m.band_id.clone()).collect();
        bands.sort();
        bands.dedup();
        out.push(TimingGroup {
            min_phi: phis.iter().copied().fold(f64::INFINITY, f64::min),
            max_phi: phis.iter().copied().fold(f64::NEG_INFINITY, f64::max),
            onsets_utc_ns: onsets,
            median_period_s: (!periods.is_empty()).then(|| periods[(periods.len() - 1) / 2]),
            bands,
            members,
        });
    }
    out.sort_by(|a, b| {
        b.members
            .len()
            .cmp(&a.members.len())
            .then(a.onsets_utc_ns.cmp(&b.onsets_utc_ns))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000_000_000;

    /// Observations every `step` minutes for `n` steps, on when `on(t)`.
    fn obs(n: i64, step: i64, offset: i64, on: impl Fn(i64) -> bool) -> Vec<(i64, i64, bool)> {
        (0..n)
            .map(|i| {
                let t = offset + i * step * MIN;
                (t, t + 5_000_000_000, on(t))
            })
            .collect()
    }

    fn cycling(t: i64) -> bool {
        (t / MIN) % 18 < 8
    }

    fn feature(kind: &'static str, band: &str, t: Timeline) -> Feature {
        Feature {
            kind,
            band_id: band.into(),
            antenna_id: "rx1".into(),
            segment_id: None,
            hz: None,
            family_spacing_hz: None,
            episodes: t.on.len() as u64,
            on_fraction: t.on_fraction(),
            timeline: t,
        }
    }

    #[test]
    fn observations_become_bounded_episodes_with_midpoint_edges() {
        let p = Params::default();
        let o = obs(10, 4, 0, |t| (8 * MIN..20 * MIN).contains(&t));
        let t = Timeline::from_observations(&o, &p);
        // On at 8, 12, 16; quiet at 4 and 20.
        assert_eq!(t.on, [(6 * MIN + 2_500_000_000, 18 * MIN + 2_500_000_000)]);
        assert_eq!(t.edges.len(), 2);
        assert_eq!(t.revisit_ns, (4 * MIN) as f64);
        let edge = Timeline::from_observations(&obs(3, 4, 0, |_| true), &p);
        assert!(edge.edges.is_empty(), "always on: no observed transitions");
        // One missed detection is bridged; two end the episode, bounded by
        // the first miss.
        let o = obs(12, 4, 0, |t| ![0, 16 * MIN, 28 * MIN, 32 * MIN].contains(&t));
        let t = Timeline::from_observations(&o, &p);
        assert_eq!(t.on.len(), 2, "{:?}", t.on);
        assert_eq!(t.edges.len(), 3, "onset, end, onset");
        let (last_on_end, first_miss) = (24 * MIN + 5_000_000_000, 28 * MIN);
        assert_eq!(t.on[0].1, last_on_end + (first_miss - last_on_end) / 2);
    }

    #[test]
    fn features_switching_together_group_and_others_do_not() {
        let p = Params::default();
        let a = Timeline::from_observations(&obs(120, 4, 0, cycling), &p);
        // Another band, sampled 40 s later, same source.
        let b = Timeline::from_observations(&obs(120, 4, 40_000_000_000, cycling), &p);
        // Same duty, different phase: not related.
        let c = Timeline::from_observations(&obs(120, 4, 0, |t| cycling(t + 9 * MIN)), &p);
        // A long single episode overlapping both.
        let d = Timeline::from_observations(&obs(120, 4, 0, |t| t > 100 * MIN && t < 300 * MIN), &p);
        let ab = agreement(&a, &b);
        assert!(ab.phi.unwrap() > 0.8 && ab.matched_edges >= 20, "{ab:?}");
        let ac = agreement(&a, &c);
        assert!(ac.phi.unwrap() < 0.0, "{ac:?}");
        assert!(agreement(&a, &d).matched_edges < p.timing_min_matched_edges);
        let g = group(
            vec![
                feature("floor", "40m", a),
                feature("carrier", "20m", b),
                feature("floor", "30m", c),
                feature("floor", "80m", d),
            ],
            &p,
        );
        assert_eq!(g.len(), 1, "{g:#?}");
        assert_eq!(g[0].bands, ["20m", "40m"]);
        assert_eq!(
            g[0].members[0].kind, "floor",
            "the anchor and ordering are deterministic"
        );
        // 4-minute sampling quantizes an 18-minute cycle's onsets to 16 or
        // 20 minutes apart: the period is known to within one revisit.
        let period = g[0].median_period_s.unwrap();
        assert!((period - 18.0 * 60.0).abs() <= 4.0 * 60.0, "{period}");
    }
}
