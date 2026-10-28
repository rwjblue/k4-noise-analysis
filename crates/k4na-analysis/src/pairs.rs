//! Paired main/sub visits: two antennas over the same dwell (docs/paired.md).
//!
//! For each pair whose visits are both complete, carriers seen on both pans
//! are matched and their signal over local background compared, giving the
//! sub-minus-main difference `d` for the pair. With swapped orientations,
//! `d0 = antenna + receiver` and `d1 = -antenna + receiver`, so
//! `antenna = (d0 - d1) / 2` (second antenna minus first) and
//! `receiver = (d0 + d1) / 2` (sub minus main). Floors are compared the
//! same way but are context, not a ranking: a quieter floor can come from
//! less gain as easily as from less noise.

use std::collections::{BTreeMap, BTreeSet};

use k4na_store::reader::VisitRow;
use serde::Serialize;

use crate::background::{Peak, VisitMetrics, median_i16};
use crate::spectrum::VisitSpectrum;
use crate::{Params, db};

/// One complete pair.
#[derive(Debug, Clone, Serialize)]
pub struct PairRow {
    pub pair_id: String,
    pub started_utc_ns: i64,
    pub band_id: String,
    /// `0`: main on the profile's first antenna; `1`: swapped.
    pub orientation: u8,
    pub main_antenna: String,
    pub sub_antenna: String,
    /// Carriers on both pans, and on only one of them.
    pub carriers_matched: u64,
    pub carriers_main_only: u64,
    pub carriers_sub_only: u64,
    /// Median over matched carriers of the sub-minus-main signal over
    /// background, dB; `None` below `pair_min_carriers`.
    pub snr_sub_minus_main_db: Option<f64>,
    pub floor_main_db: f64,
    pub floor_sub_db: f64,
    /// Verified receiver settings that differ between the pans.
    pub settings_differ: Vec<String>,
}

/// Pairs per band and antenna pair, decomposed into antenna and receiver
/// differences when both orientations are present.
#[derive(Debug, Clone, Serialize)]
pub struct PairSummary {
    pub band_id: String,
    /// The profile's first and second antennas (orientation 0: main and sub).
    pub first_antenna: String,
    pub second_antenna: String,
    pub pairs: [u64; 2],
    /// Pairs with enough matched carriers, per orientation.
    pub snr_pairs: [u64; 2],
    /// Median sub-minus-main signal over background per orientation, dB.
    pub snr_d_db: [Option<f64>; 2],
    pub floor_d_db: [Option<f64>; 2],
    /// Second antenna minus first, signal over background, dB. Needs both
    /// orientations.
    pub snr_antenna_db: Option<f64>,
    /// Sub receiver minus main, signal over background, dB.
    pub snr_receiver_db: Option<f64>,
    /// The same decomposition of raw floors; context, not a ranking.
    pub floor_antenna_db: Option<f64>,
    pub floor_receiver_db: Option<f64>,
    pub first_utc_ns: i64,
    pub last_utc_ns: i64,
    /// `one_orientation`: antenna and receiver differences are confounded.
    /// `receiver_difference`: the receivers disagree by more than
    /// `pair_receiver_tolerance`, so they may not be comparable.
    /// `settings_differ`: some pair had unequal verified settings.
    /// `few_carriers`: no orientation had a pair with enough carriers.
    pub flags: Vec<String>,
}

/// The half of a pair seen first, until its peer arrives.
struct Half {
    visit: VisitRow,
    floor: i16,
    peaks: Vec<Peak>,
    bin_hz: f64,
}

#[derive(Default)]
pub(crate) struct Pairs {
    waiting: BTreeMap<String, Half>,
    rows: Vec<PairRow>,
}

impl Pairs {
    pub(crate) fn add(&mut self, s: &VisitSpectrum, m: &VisitMetrics, p: &Params) {
        let Some(pair) = s.visit.pair() else { return };
        let half = Half {
            visit: s.visit.clone(),
            floor: m.floor,
            peaks: m
                .peaks
                .iter()
                .filter(|k| k.width_hz() <= p.max_carrier_width_hz)
                .cloned()
                .collect(),
            bin_hz: s.bin_hz,
        };
        let Some(other) = self.waiting.remove(&pair.pair_id) else {
            self.waiting.insert(pair.pair_id, half);
            return;
        };
        let (main, sub) = if half.visit.pan == "A" {
            (half, other)
        } else {
            (other, half)
        };
        if main.visit.pan != "A" || sub.visit.pan != "B" {
            return;
        }
        self.rows.push(compare(&pair.pair_id, pair.orientation, &main, &sub, p));
    }

    pub(crate) fn finish(mut self, p: &Params) -> (Vec<PairRow>, Vec<PairSummary>) {
        self.rows
            .sort_by(|a, b| (a.started_utc_ns, &a.band_id).cmp(&(b.started_utc_ns, &b.band_id)));
        let summaries = summarize(&self.rows, p);
        (self.rows, summaries)
    }
}

fn compare(pair_id: &str, orientation: u8, main: &Half, sub: &Half, p: &Params) -> PairRow {
    let tol = p.carrier_tolerance_hz.max(1.5 * main.bin_hz.max(sub.bin_hz));
    let mut used = BTreeSet::new();
    let mut diffs = Vec::new();
    for a in &main.peaks {
        let best = sub
            .peaks
            .iter()
            .enumerate()
            .filter(|(i, b)| !used.contains(i) && (a.hz - b.hz).abs() <= tol)
            .min_by(|(_, x), (_, y)| (a.hz - x.hz).abs().total_cmp(&(a.hz - y.hz).abs()));
        if let Some((i, b)) = best {
            used.insert(i);
            diffs.push(b.excess.saturating_sub(a.excess));
        }
    }
    let matched = diffs.len();
    PairRow {
        pair_id: pair_id.to_owned(),
        started_utc_ns: main.visit.started_utc_ns,
        band_id: main.visit.band_id.clone().unwrap_or_default(),
        orientation,
        main_antenna: main.visit.antenna_id.clone(),
        sub_antenna: sub.visit.antenna_id.clone(),
        carriers_matched: matched as u64,
        carriers_main_only: (main.peaks.len() - matched) as u64,
        carriers_sub_only: (sub.peaks.len() - matched) as u64,
        snr_sub_minus_main_db: (matched >= p.pair_min_carriers).then(|| db(median_i16(&mut diffs))),
        floor_main_db: db(main.floor),
        floor_sub_db: db(sub.floor),
        settings_differ: settings_differ(&main.visit, &sub.visit),
    }
}

/// Verified settings (`verified.*`) that differ between two visits.
fn settings_differ(a: &VisitRow, b: &VisitRow) -> Vec<String> {
    let map = |v: &VisitRow| -> BTreeMap<String, String> {
        v.measurement_json
            .as_deref()
            .and_then(|j| serde_json::from_str(j).ok())
            .unwrap_or_default()
    };
    let (a, b) = (map(a), map(b));
    let keys: BTreeSet<_> = a
        .keys()
        .chain(b.keys())
        .filter(|k| k.starts_with("verified."))
        .collect();
    keys.into_iter()
        .filter(|k| a.get(*k) != b.get(*k))
        .map(|k| k.trim_start_matches("verified.").to_owned())
        .collect()
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

fn summarize(rows: &[PairRow], p: &Params) -> Vec<PairSummary> {
    // Orientation 0 has main on the first antenna.
    let mut groups: BTreeMap<(String, String, String), Vec<&PairRow>> = BTreeMap::new();
    for r in rows {
        let (first, second) = if r.orientation == 0 {
            (&r.main_antenna, &r.sub_antenna)
        } else {
            (&r.sub_antenna, &r.main_antenna)
        };
        groups
            .entry((r.band_id.clone(), first.clone(), second.clone()))
            .or_default()
            .push(r);
    }
    let decompose = |d: [Option<f64>; 2]| match d {
        [Some(d0), Some(d1)] => (Some((d0 - d1) / 2.0), Some((d0 + d1) / 2.0)),
        _ => (None, None),
    };
    groups
        .into_iter()
        .map(|((band_id, first_antenna, second_antenna), rs)| {
            let of = |o: u8| rs.iter().filter(move |r| r.orientation == o);
            let pairs = [of(0).count() as u64, of(1).count() as u64];
            let snr = |o| median(of(o).filter_map(|r| r.snr_sub_minus_main_db).collect());
            let snr_pairs = [0, 1].map(|o| of(o).filter(|r| r.snr_sub_minus_main_db.is_some()).count() as u64);
            let floor = |o| median(of(o).map(|r| r.floor_sub_db - r.floor_main_db).collect());
            let snr_d_db = [snr(0), snr(1)];
            let floor_d_db = [floor(0), floor(1)];
            let (snr_antenna_db, snr_receiver_db) = decompose(snr_d_db);
            let (floor_antenna_db, floor_receiver_db) = decompose(floor_d_db);
            let mut flags = Vec::new();
            if pairs.contains(&0) {
                flags.push("one_orientation".to_owned());
            }
            if snr_receiver_db.is_some_and(|r| r.abs() > db(p.pair_receiver_tolerance)) {
                flags.push("receiver_difference".to_owned());
            }
            if rs.iter().any(|r| !r.settings_differ.is_empty()) {
                flags.push("settings_differ".to_owned());
            }
            if snr_pairs == [0, 0] {
                flags.push("few_carriers".to_owned());
            }
            PairSummary {
                band_id,
                first_antenna,
                second_antenna,
                pairs,
                snr_pairs,
                snr_d_db,
                floor_d_db,
                snr_antenna_db,
                snr_receiver_db,
                floor_antenna_db,
                floor_receiver_db,
                first_utc_ns: rs.iter().map(|r| r.started_utc_ns).min().unwrap_or(0),
                last_utc_ns: rs.iter().map(|r| r.started_utc_ns).max().unwrap_or(0),
                flags,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(o: u8, snr: Option<f64>, main_floor: f64, sub_floor: f64) -> PairRow {
        let (m, s) = if o == 0 { ("rx", "dxc") } else { ("dxc", "rx") };
        PairRow {
            pair_id: format!("p{o}"),
            started_utc_ns: 0,
            band_id: "40m".into(),
            orientation: o,
            main_antenna: m.into(),
            sub_antenna: s.into(),
            carriers_matched: 5,
            carriers_main_only: 0,
            carriers_sub_only: 0,
            snr_sub_minus_main_db: snr,
            floor_main_db: main_floor,
            floor_sub_db: sub_floor,
            settings_differ: vec![],
        }
    }

    #[test]
    fn swapped_orientations_separate_antenna_from_receiver() {
        // dxc is 4 dB worse than rx; the sub reads 1 dB better (SNR) and
        // its floor 3 dB higher; the dxc floor is 20 dB above rx's.
        let rows = [row(0, Some(-3.0), -120.0, -97.0), row(1, Some(5.0), -100.0, -117.0)];
        let s = summarize(&rows, &Params::default());
        assert_eq!(s.len(), 1);
        let s = &s[0];
        assert_eq!((s.first_antenna.as_str(), s.second_antenna.as_str()), ("rx", "dxc"));
        assert_eq!((s.snr_antenna_db, s.snr_receiver_db), (Some(-4.0), Some(1.0)));
        assert_eq!((s.floor_antenna_db, s.floor_receiver_db), (Some(20.0), Some(3.0)));
        assert!(s.flags.is_empty(), "{:?}", s.flags);
    }

    #[test]
    fn one_orientation_or_a_receiver_gap_is_flagged() {
        let s = summarize(&[row(0, Some(-3.0), -120.0, -97.0)], &Params::default());
        assert_eq!(s[0].snr_antenna_db, None);
        assert_eq!(s[0].flags, ["one_orientation"]);
        let s = summarize(
            &[row(0, Some(6.0), 0.0, 0.0), row(1, Some(8.0), 0.0, 0.0)],
            &Params::default(),
        );
        assert_eq!((s[0].snr_antenna_db, s[0].snr_receiver_db), (Some(-1.0), Some(7.0)));
        assert_eq!(s[0].flags, ["receiver_difference"]);
        let s = summarize(&[row(0, None, 0.0, 0.0), row(1, None, 0.0, 0.0)], &Params::default());
        assert_eq!(s[0].flags, ["few_carriers"]);
    }
}
