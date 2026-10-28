//! Carriers on a common frequency grid (combs): `f ≈ offset + k * spacing`.
//!
//! Uniformly spaced carriers suggest harmonics of one switching or clock
//! source, but this is a *pattern*, not a source identification.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::Params;
use crate::carriers::Carrier;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Family {
    pub band_id: String,
    pub antenna_id: String,
    /// Least-squares grid fitted to the members.
    pub spacing_hz: f64,
    pub offset_hz: f64,
    pub first_hz: f64,
    pub last_hz: f64,
    /// Member carrier frequencies, ascending.
    pub members: Vec<f64>,
    /// Grid positions from the first to the last member, and the fraction
    /// occupied by a member.
    pub grid_positions: u64,
    pub fill: f64,
    /// RMS distance of members from their grid lines.
    pub residual_rms_hz: f64,
    /// Members expected by chance from the other carriers in the span.
    pub expected_by_chance: f64,
    /// Expected number of families at least this strong arising from
    /// unrelated carriers over all spacings tried (Bonferroni bound).
    pub false_alarm: f64,
    pub median_member_persistence: f64,
}

/// ln P(X >= k) for X ~ Binomial(n, q).
fn ln_binomial_tail(n: u64, q: f64, k: u64) -> f64 {
    if k == 0 {
        return 0.0;
    }
    if k > n || q <= 0.0 {
        return f64::NEG_INFINITY;
    }
    if q >= 1.0 {
        return 0.0;
    }
    let ln_choose = |n: u64, i: u64| -> f64 { (1..=i).map(|j| ((n - i + j) as f64).ln() - (j as f64).ln()).sum() };
    let terms: Vec<f64> = (k..=n)
        .map(|i| ln_choose(n, i) + i as f64 * q.ln() + (n - i) as f64 * (1.0 - q).ln())
        .collect();
    let m = terms.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    m + terms.iter().map(|t| (t - m).exp()).sum::<f64>().ln()
}

#[derive(Debug, Clone, Copy)]
struct Fit {
    spacing: f64,
    offset: f64,
}

impl Fit {
    fn k(&self, f: f64) -> f64 {
        ((f - self.offset) / self.spacing).round()
    }

    fn residual(&self, f: f64) -> f64 {
        f - (self.offset + self.k(f) * self.spacing)
    }
}

/// Least-squares `f = offset + k * spacing` over members with integer k
/// from `guess`.
fn refit(fs: &[f64], guess: Fit) -> Option<Fit> {
    let ks: Vec<f64> = fs.iter().map(|&f| guess.k(f)).collect();
    let n = fs.len() as f64;
    let (mk, mf) = (ks.iter().sum::<f64>() / n, fs.iter().sum::<f64>() / n);
    let sxx: f64 = ks.iter().map(|k| (k - mk).powi(2)).sum();
    if sxx <= 0.0 {
        return None;
    }
    let sxy: f64 = ks.iter().zip(fs).map(|(k, f)| (k - mk) * (f - mf)).sum();
    let spacing = sxy / sxx;
    (spacing > 0.0).then_some(Fit {
        spacing,
        offset: mf - spacing * mk,
    })
}

/// Indices of `fs` within `tol` of the largest phase cluster for `s`.
fn phase_members(fs: &[f64], s: f64, tol: f64) -> Vec<usize> {
    let mut ph: Vec<(f64, usize)> = fs.iter().enumerate().map(|(i, f)| (f.rem_euclid(s), i)).collect();
    ph.sort_by(|a, b| a.0.total_cmp(&b.0));
    let n = ph.len();
    let wrapped: Vec<(f64, usize)> = ph.iter().copied().chain(ph.iter().map(|&(p, i)| (p + s, i))).collect();
    let (mut best, mut best_start, mut j) = (0, 0, 0);
    for i in 0..n {
        j = j.max(i);
        while j < i + n && wrapped[j].0 - wrapped[i].0 <= 2.0 * tol {
            j += 1;
        }
        if j - i > best {
            (best, best_start) = (j - i, i);
        }
    }
    wrapped[best_start..best_start + best].iter().map(|&(_, i)| i).collect()
}

struct Candidate {
    fit: Fit,
    members: Vec<usize>,
    positions: u64,
    fill: f64,
    chance: f64,
    /// ln P(at least this many members | two seed members, unrelated rest).
    ln_p: f64,
}

fn evaluate(fs: &[f64], s: f64, tol: f64, p: &Params) -> Option<Candidate> {
    let seed = phase_members(fs, s, tol);
    // Refinement rarely adds more than one member: skip hopeless grids.
    if seed.len() < 2 || (seed.len() as u64) + 1 < p.family_min_members {
        return None;
    }
    let first = seed.iter().map(|&i| fs[i]).fold(f64::INFINITY, f64::min);
    let mut fit = Fit {
        spacing: s,
        offset: first,
    };
    let mut members = seed;
    // Refine the grid twice, re-selecting members against it each time.
    for _ in 0..2 {
        let m: Vec<f64> = members.iter().map(|&i| fs[i]).collect();
        fit = refit(&m, fit).unwrap_or(fit);
        members = (0..fs.len()).filter(|&i| fit.residual(fs[i]).abs() <= tol).collect();
        if members.len() < 2 {
            return None;
        }
    }
    if fit.spacing < p.family_min_spacing_hz || fit.spacing > p.family_max_spacing_hz {
        return None;
    }
    let ks: Vec<i64> = members.iter().map(|&i| fit.k(fs[i]) as i64).collect();
    let (kmin, kmax) = (*ks.iter().min()?, *ks.iter().max()?);
    let mut distinct = ks.clone();
    distinct.sort_unstable();
    distinct.dedup();
    if distinct.len() != members.len() {
        // Two carriers on one grid line: keep this spacing out.
        return None;
    }
    let positions = (kmax - kmin + 1) as u64;
    let (lo, hi) = (
        fit.offset + kmin as f64 * fit.spacing,
        fit.offset + kmax as f64 * fit.spacing,
    );
    let in_span = fs.iter().filter(|&&f| f >= lo - tol && f <= hi + tol).count() as u64;
    let q = (2.0 * tol / fit.spacing).min(1.0);
    let m = members.len() as u64;
    Some(Candidate {
        fit,
        fill: m as f64 / positions as f64,
        chance: in_span as f64 * q,
        // Two members define a candidate grid; the rest must land on it.
        ln_p: ln_binomial_tail(in_span.saturating_sub(2), q, m.saturating_sub(2)),
        members,
        positions,
    })
}

fn accept(c: &Candidate, p: &Params, ln_trials: f64) -> bool {
    c.members.len() as u64 >= p.family_min_members
        && c.fill >= p.family_min_fill
        && c.ln_p + ln_trials <= p.family_max_false_alarm.ln()
}

/// Finds families per band and antenna, strongest first, each carrier in
/// at most one family.
pub fn find_families(carriers: &[Carrier], p: &Params) -> Vec<Family> {
    let mut groups: BTreeMap<(&str, &str), Vec<&Carrier>> = BTreeMap::new();
    for c in carriers {
        groups.entry((&c.band_id, &c.antenna_id)).or_default().push(c);
    }
    let mut out = Vec::new();
    for ((band, antenna), mut cs) in groups {
        cs.sort_by(|a, b| a.hz.total_cmp(&b.hz));
        let tol = p.carrier_tolerance_hz;
        loop {
            let fs: Vec<f64> = cs.iter().map(|c| c.hz).collect();
            let mut spacings = Vec::new();
            for i in 0..fs.len() {
                for j in i + 1..fs.len() {
                    let d = fs[j] - fs[i];
                    if d > p.family_max_spacing_hz * 4.0 {
                        break;
                    }
                    for k in 1..=4 {
                        let s = d / f64::from(k);
                        if (p.family_min_spacing_hz..=p.family_max_spacing_hz).contains(&s) {
                            spacings.push(s);
                        }
                    }
                }
            }
            spacings.sort_by(f64::total_cmp);
            spacings.dedup_by(|a, b| (*a - *b).abs() < 1.0);
            let ln_trials = (spacings.len().max(1) as f64).ln();
            let best = spacings
                .iter()
                .filter_map(|&s| evaluate(&fs, s, tol, p))
                .filter(|c| accept(c, p, ln_trials))
                .max_by(|a, b| {
                    a.members
                        .len()
                        .cmp(&b.members.len())
                        .then(a.fill.total_cmp(&b.fill))
                        .then(a.fit.spacing.total_cmp(&b.fit.spacing))
                });
            let Some(c) = best else { break };
            let mut members: Vec<f64> = c.members.iter().map(|&i| fs[i]).collect();
            members.sort_by(f64::total_cmp);
            let rms =
                (c.members.iter().map(|&i| c.fit.residual(fs[i]).powi(2)).sum::<f64>() / c.members.len() as f64).sqrt();
            let mut pers: Vec<f64> = c.members.iter().map(|&i| cs[i].persistence).collect();
            pers.sort_by(f64::total_cmp);
            out.push(Family {
                band_id: band.to_owned(),
                antenna_id: antenna.to_owned(),
                spacing_hz: c.fit.spacing,
                offset_hz: c.fit.offset.rem_euclid(c.fit.spacing),
                first_hz: members[0],
                last_hz: members[members.len() - 1],
                members,
                grid_positions: c.positions,
                fill: c.fill,
                residual_rms_hz: rms,
                expected_by_chance: c.chance,
                false_alarm: (c.ln_p + ln_trials).exp(),
                median_member_persistence: pers[(pers.len() - 1) / 2],
            });
            let taken: std::collections::BTreeSet<usize> = c.members.into_iter().collect();
            cs = cs
                .into_iter()
                .enumerate()
                .filter(|(i, _)| !taken.contains(i))
                .map(|(_, c)| c)
                .collect();
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn carrier(hz: f64) -> Carrier {
        Carrier {
            band_id: "10m".into(),
            antenna_id: "rx1".into(),
            hz,
            min_hz: hz,
            max_hz: hz,
            detections: 10,
            exposure: 10,
            masked: 0,
            persistence: 1.0,
            median_level_db: -100.0,
            median_excess_db: 12.0,
            max_excess_db: 12.0,
            steady_fraction: 1.0,
            median_width_hz: 47.0,
            first_utc_ns: 0,
            last_utc_ns: 0,
            hour_detections: [0; 24],
            hour_exposure: [0; 24],
            segments: 1,
            class: "stationary",
            timeline: Default::default(),
        }
    }

    /// Deterministic pseudo-random frequencies in [lo, hi).
    fn scatter(n: usize, lo: f64, hi: f64, seed: u64) -> Vec<f64> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x = x
                    .wrapping_mul(6_364_136_223_846_793_005)
                    .wrapping_add(1_442_695_040_888_963_407);
                lo + (x >> 11) as f64 / (1u64 << 53) as f64 * (hi - lo)
            })
            .collect()
    }

    #[test]
    fn binomial_tail_matches_direct_sums() {
        let p = |n, q, k| ln_binomial_tail(n, q, k).exp();
        assert!((p(10, 0.5, 0) - 1.0).abs() < 1e-12);
        assert!((p(10, 0.5, 10) - 0.5f64.powi(10)).abs() < 1e-12);
        assert!((p(4, 0.1, 2) - (6.0 * 0.01 * 0.81 + 4.0 * 0.001 * 0.9 + 0.0001)).abs() < 1e-12);
        assert_eq!(p(3, 0.1, 4), 0.0);
    }

    #[test]
    fn finds_a_comb_among_unrelated_carriers() {
        let spacing = 35_087.3;
        let mut cs: Vec<Carrier> = (0..40)
            .filter(|k| k % 5 != 3) // some harmonics missing
            .map(|k| {
                let jitter = ((k * 37) % 61) as f64 - 30.0;
                carrier(28_001_234.0 + f64::from(k) * spacing + jitter)
            })
            .collect();
        cs.extend(scatter(30, 28_000_000.0, 29_700_000.0, 7).into_iter().map(carrier));
        let fams = find_families(&cs, &Params::default());
        assert_eq!(fams.len(), 1, "{fams:#?}");
        let f = &fams[0];
        assert!((f.spacing_hz - spacing).abs() < 2.0, "{}", f.spacing_hz);
        assert!(f.members.len() >= 32, "{}", f.members.len());
        assert!(f.fill >= 0.75 && f.residual_rms_hz < 40.0, "{f:#?}");
    }

    #[test]
    fn unrelated_carriers_form_no_family() {
        for seed in 1..=6 {
            for n in [20, 60, 120] {
                let cs: Vec<Carrier> = scatter(n, 28_000_000.0, 29_700_000.0, seed)
                    .into_iter()
                    .map(carrier)
                    .collect();
                let f = find_families(&cs, &Params::default());
                assert!(f.is_empty(), "seed {seed}, {n} carriers: {f:#?}");
            }
        }
        let few: Vec<Carrier> = [14_000_000.0, 14_010_000.0, 14_020_000.0].map(carrier).to_vec();
        assert!(
            find_families(&few, &Params::default()).is_empty(),
            "three is not a family"
        );
    }

    #[test]
    fn reports_the_fundamental_not_a_sub_multiple() {
        let cs: Vec<Carrier> = (0..12)
            .map(|k| carrier(7_000_500.0 + f64::from(k) * 10_000.0))
            .collect();
        let fams = find_families(&cs, &Params::default());
        assert_eq!(fams.len(), 1);
        assert!((fams[0].spacing_hz - 10_000.0).abs() < 1.0, "{}", fams[0].spacing_hz);
        assert_eq!(fams[0].fill, 1.0);
    }
}
