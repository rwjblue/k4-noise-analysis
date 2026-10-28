//! Combs that are harmonics of one fundamental: `f ≈ n * f0`.
//!
//! A [`Family`] is a grid `offset + k * spacing` on one band. A switching
//! or clock source at `f0` radiates at integer multiples of `f0`, so its
//! combs on different bands (or two combs on one band) share `f0`, have
//! spacings that are integer multiples of it, and lie on its harmonics
//! with no free offset. A *harmonic series* is a set of families that all
//! fit one such `f0`.
//!
//! One family alone is no evidence: at harmonic numbers in the hundreds
//! or thousands a tiny change of `f0` shifts every line almost equally,
//! acting as a free offset, so nearly any comb fits *some* `f0` near
//! `spacing / m`. Only the other families' agreement counts toward
//! significance. Like families, a series is a pattern, not a source
//! identification.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::Params;
use crate::carriers::Carrier;
use crate::families::Family;

/// Divisors of a family spacing tried as the fundamental, and so the
/// largest `multiple` a member family may have: a smaller fundamental
/// fits unrelated combs too easily.
const MAX_DIVISOR: u32 = 4;
/// Fundamentals below this are not considered (their harmonics are too
/// dense to be told from chance).
const MIN_FUNDAMENTAL_HZ: f64 = 1_000.0;

/// One family's place in a series.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SeriesFamily {
    pub band_id: String,
    pub antenna_id: String,
    pub family_spacing_hz: f64,
    /// `family_spacing_hz / fundamental_hz`, rounded: 2 means only every
    /// other harmonic appears on this band.
    pub multiple: u64,
    pub first_harmonic: u64,
    pub last_harmonic: u64,
    pub members: u64,
    /// Mean and RMS distance of members from their harmonics.
    pub mean_residual_hz: f64,
    pub residual_rms_hz: f64,
    /// Chance that a comb of this extent, unrelated to the series, would
    /// fit it: offset and spacing both landing within tolerance.
    pub chance: f64,
}

/// A carrier outside the series' families that lies on one of its
/// harmonics. Not used in the fit; some are expected by chance.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SeriesCarrier {
    pub band_id: String,
    pub antenna_id: String,
    pub hz: f64,
    pub harmonic: u64,
    pub residual_hz: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HarmonicSeries {
    /// Least-squares fundamental over all member lines (no offset).
    pub fundamental_hz: f64,
    pub residual_rms_hz: f64,
    pub families: Vec<SeriesFamily>,
    pub bands: Vec<String>,
    /// Expected number of series at least this strong arising from
    /// unrelated families over all candidates tried: the candidate count
    /// times the chance of every family but the least likely one (which
    /// fixed the fit).
    pub false_alarm: f64,
    /// Other carriers within half the carrier tolerance of a harmonic,
    /// and how many of the carriers considered would land so by chance.
    pub on_harmonics: Vec<SeriesCarrier>,
    pub on_harmonics_expected_by_chance: f64,
}

/// Least-squares `f0` for `f = n * f0` with integer `n` from `f0`.
fn refit(hz: &[f64], mut f0: f64) -> f64 {
    for _ in 0..3 {
        let (mut num, mut den) = (0.0, 0.0);
        for &h in hz {
            let n = (h / f0).round();
            num += n * h;
            den += n * n;
        }
        if den > 0.0 {
            f0 = num / den;
        }
    }
    f0
}

fn residuals(hz: &[f64], f0: f64) -> Vec<f64> {
    hz.iter().map(|&h| h - (h / f0).round() * f0).collect()
}

fn rms(r: &[f64]) -> f64 {
    (r.iter().map(|x| x * x).sum::<f64>() / r.len().max(1) as f64).sqrt()
}

fn fits(f: &Family, f0: f64, tol: f64) -> bool {
    let m = (f.spacing_hz / f0).round();
    (1.0..=f64::from(MAX_DIVISOR)).contains(&m) && residuals(&f.members, f0).iter().all(|r| r.abs() <= tol)
}

/// Chance that a comb like `f` fits `f0` by accident: its offset within
/// `tol` of a harmonic (`2 tol / f0`) and its spacing within
/// `tol / positions` of a multiple of `f0` (`2 tol / (positions f0)`).
fn chance(f: &Family, f0: f64, tol: f64) -> f64 {
    let positions = f.grid_positions.max(1) as f64;
    ((2.0 * tol / f0).min(1.0)) * ((2.0 * tol / (positions * f0)).min(1.0))
}

pub fn find_series(families: &[Family], carriers: &[Carrier], p: &Params) -> Vec<HarmonicSeries> {
    let tol = p.carrier_tolerance_hz;
    // Candidate fundamentals: each family's spacing over 1..=MAX_DIVISOR,
    // refitted to that family's own lines.
    let mut candidates: Vec<f64> = Vec::new();
    for f in families {
        for m in 1..=MAX_DIVISOR {
            let f0 = refit(&f.members, f.spacing_hz / f64::from(m));
            if f0 >= MIN_FUNDAMENTAL_HZ && fits(f, f0, tol) {
                candidates.push(f0);
            }
        }
    }
    let tried = candidates.len().max(1) as f64;
    // For each candidate, the families it explains, refitted over all of
    // their lines until the set is stable.
    let mut found: Vec<(BTreeSet<usize>, f64)> = Vec::new();
    for c in candidates {
        let mut f0 = c;
        let mut set = BTreeSet::new();
        for _ in 0..3 {
            set = (0..families.len()).filter(|&i| fits(&families[i], f0, tol)).collect();
            if set.len() < 2 {
                break;
            }
            let lines: Vec<f64> = set.iter().flat_map(|&i| families[i].members.iter().copied()).collect();
            f0 = refit(&lines, f0);
        }
        set.retain(|&i| fits(&families[i], f0, tol));
        if set.len() < 2 || f0 < MIN_FUNDAMENTAL_HZ {
            continue;
        }
        // Any f0 / k also fits; keep the largest fundamental per set.
        match found.iter_mut().find(|(s, _)| *s == set) {
            Some(e) if e.1 >= f0 => {}
            Some(e) => e.1 = f0,
            None => found.push((set, f0)),
        }
    }
    let mut series: Vec<(BTreeSet<usize>, HarmonicSeries)> = found
        .into_iter()
        .map(|(set, f0)| {
            let mut fams: Vec<SeriesFamily> = set
                .iter()
                .map(|&i| {
                    let f = &families[i];
                    let r = residuals(&f.members, f0);
                    let n = |h: f64| (h / f0).round() as u64;
                    SeriesFamily {
                        band_id: f.band_id.clone(),
                        antenna_id: f.antenna_id.clone(),
                        family_spacing_hz: f.spacing_hz,
                        multiple: (f.spacing_hz / f0).round() as u64,
                        first_harmonic: n(f.first_hz),
                        last_harmonic: n(f.last_hz),
                        members: f.members.len() as u64,
                        mean_residual_hz: r.iter().sum::<f64>() / r.len().max(1) as f64,
                        residual_rms_hz: rms(&r),
                        chance: chance(f, f0, tol),
                    }
                })
                .collect();
            fams.sort_by_key(|f| f.first_harmonic);
            let mut chances: Vec<f64> = fams.iter().map(|f| f.chance).collect();
            chances.sort_by(f64::total_cmp);
            let false_alarm = tried * chances.iter().skip(1).product::<f64>();
            let lines: Vec<f64> = set.iter().flat_map(|&i| families[i].members.iter().copied()).collect();
            let in_series = |c: &Carrier| {
                set.iter().any(|&i| {
                    let f = &families[i];
                    f.band_id == c.band_id
                        && f.antenna_id == c.antenna_id
                        && f.members.iter().any(|&m| (m - c.hz).abs() < 0.5)
                })
            };
            let near = tol / 2.0;
            let others: Vec<&Carrier> = carriers.iter().filter(|c| !in_series(c)).collect();
            let on_harmonics = others
                .iter()
                .filter_map(|c| {
                    let n = (c.hz / f0).round();
                    let r = c.hz - n * f0;
                    (r.abs() <= near).then(|| SeriesCarrier {
                        band_id: c.band_id.clone(),
                        antenna_id: c.antenna_id.clone(),
                        hz: c.hz,
                        harmonic: n as u64,
                        residual_hz: r,
                    })
                })
                .collect();
            let mut bands: Vec<String> = fams.iter().map(|f| f.band_id.clone()).collect();
            bands.sort();
            bands.dedup();
            let s = HarmonicSeries {
                fundamental_hz: f0,
                residual_rms_hz: rms(&residuals(&lines, f0)),
                families: fams,
                bands,
                false_alarm,
                on_harmonics,
                on_harmonics_expected_by_chance: others.len() as f64 * (2.0 * near / f0).min(1.0),
            };
            (set, s)
        })
        .filter(|(_, s)| s.false_alarm <= p.family_max_false_alarm)
        .collect();
    // Strongest first; each family joins at most one series.
    series.sort_by(|a, b| {
        a.1.false_alarm
            .total_cmp(&b.1.false_alarm)
            .then(b.1.fundamental_hz.total_cmp(&a.1.fundamental_hz))
    });
    let mut used = BTreeSet::new();
    let mut out = Vec::new();
    for (set, s) in series {
        if set.is_disjoint(&used) {
            used.extend(set);
            out.push(s);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn family(band: &str, f0: f64, multiple: f64, first: u64, count: u64, wobble: f64) -> Family {
        let spacing = f0 * multiple;
        let members: Vec<f64> = (0..count)
            .map(|k| (first as f64 + k as f64 * multiple) * f0 + if k % 2 == 0 { wobble } else { -wobble })
            .collect();
        Family {
            band_id: band.into(),
            antenna_id: "rx1".into(),
            spacing_hz: spacing,
            offset_hz: members[0].rem_euclid(spacing),
            first_hz: members[0],
            last_hz: members[members.len() - 1],
            grid_positions: count,
            fill: 1.0,
            residual_rms_hz: wobble,
            expected_by_chance: 0.0,
            false_alarm: 0.0,
            members,
            median_member_persistence: 1.0,
        }
    }

    fn carrier(band: &str, hz: f64) -> Carrier {
        let f = crate::families::tests::carrier(hz);
        Carrier {
            band_id: band.into(),
            ..f
        }
    }

    #[test]
    fn combs_on_two_bands_share_a_fundamental() {
        let p = Params::default();
        let f0 = 15_967.7;
        // 40 m: every harmonic from 439; 20 m: every other one from 878.
        let fams = vec![
            family("40m", f0, 1.0, 439, 12, 20.0),
            family("20m", f0, 2.0, 878, 11, 15.0),
            // A 10 m comb unrelated to it.
            family("10m", 35_087.0, 1.0, 813, 30, 10.0),
        ];
        let carriers = vec![carrier("30m", 633.0 * f0 + 14.0), carrier("30m", 10_115_000.0)];
        let s = find_series(&fams, &carriers, &p);
        assert_eq!(s.len(), 1, "{s:#?}");
        let s = &s[0];
        assert!((s.fundamental_hz - f0).abs() < 0.5, "{}", s.fundamental_hz);
        assert_eq!(s.bands, ["20m", "40m"]);
        assert_eq!(
            s.families
                .iter()
                .map(|f| (f.multiple, f.first_harmonic))
                .collect::<Vec<_>>(),
            [(1, 439), (2, 878)]
        );
        assert!(s.false_alarm < 1e-3, "{}", s.false_alarm);
        assert_eq!(s.on_harmonics.len(), 1);
        assert_eq!(s.on_harmonics[0].harmonic, 633);
        assert!(s.on_harmonics_expected_by_chance < 0.02);
    }

    #[test]
    fn one_comb_or_an_offset_comb_is_not_a_series() {
        let p = Params::default();
        let f0 = 15_967.7;
        let alone = vec![family("40m", f0, 1.0, 439, 12, 20.0)];
        assert!(find_series(&alone, &[], &p).is_empty());
        // Same spacing on 20 m but offset by 0.37 of it: no fundamental
        // of at least a quarter of the spacing fits both.
        let mut shifted = family("20m", f0, 1.0, 878, 11, 15.0);
        for m in &mut shifted.members {
            *m += 0.37 * f0;
        }
        let pair = vec![family("40m", f0, 1.0, 439, 12, 20.0), shifted.clone()];
        assert!(find_series(&pair, &[], &p).is_empty());
        // Offset by exactly half, the two are odd and even harmonics of
        // f0 / 2: a legitimate series.
        for m in &mut shifted.members {
            *m += (0.5 - 0.37) * f0;
        }
        let pair = vec![family("40m", f0, 1.0, 439, 12, 20.0), shifted];
        let s = find_series(&pair, &[], &p);
        assert_eq!(s.len(), 1);
        assert!((s[0].fundamental_hz - f0 / 2.0).abs() < 0.5);
        assert!(s[0].families.iter().all(|f| f.multiple == 2));
    }
}
