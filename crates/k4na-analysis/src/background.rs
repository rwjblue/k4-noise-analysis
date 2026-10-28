//! Robust local background and narrow-peak detection within one visit.

use serde::Serialize;

use crate::Params;
use crate::spectrum::VisitSpectrum;

/// Rolling `percentile` of `values` over a `window_hz` neighbourhood,
/// evaluated at the centre of each `block_hz` block and linearly
/// interpolated between block centres. A low percentile ignores peaks
/// occupying less than `1 - percentile` of the window.
pub fn local_background(values: &[i16], bin_hz: f64, window_hz: f64, block_hz: f64, percentile: f64) -> Vec<i16> {
    let n = values.len();
    if n == 0 {
        return Vec::new();
    }
    let block = ((block_hz / bin_hz).round() as usize).clamp(1, n);
    let half = ((window_hz / 2.0 / bin_hz).round() as usize).max(1);
    let mut scratch = Vec::with_capacity(2 * half + 1);
    let mut anchors: Vec<(f64, f64)> = Vec::new();
    let mut start = 0;
    while start < n {
        let end = (start + block).min(n);
        let centre = (start + end - 1) / 2;
        let lo = centre.saturating_sub(half);
        let hi = (centre + half + 1).min(n);
        scratch.clear();
        scratch.extend_from_slice(&values[lo..hi]);
        let k = ((scratch.len() - 1) as f64 * percentile.clamp(0.0, 1.0)).round() as usize;
        let (_, v, _) = scratch.select_nth_unstable(k);
        anchors.push(((start + end - 1) as f64 / 2.0, f64::from(*v)));
        start = end;
    }
    let mut out = Vec::with_capacity(n);
    let mut j = 0;
    for i in 0..n {
        let x = i as f64;
        while j + 1 < anchors.len() && anchors[j + 1].0 <= x {
            j += 1;
        }
        let (x0, y0) = anchors[j];
        let y = match anchors.get(j + 1) {
            Some(&(x1, y1)) if x > x0 => y0 + (y1 - y0) * (x - x0) / (x1 - x0),
            _ => y0,
        };
        out.push(y.round() as i16);
    }
    out
}

/// A run of adjacent bins whose median level stands above the local
/// background by at least the threshold for the visit's dwell.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Peak {
    /// RF centre of the strongest bin.
    pub hz: f64,
    pub low_hz: f64,
    pub high_hz: f64,
    /// Excess over local background at the strongest bin, native tenths.
    pub excess: i16,
    /// The same bin's p10 and p90 over background: p10 high means the
    /// peak was present nearly the whole dwell.
    pub p10_excess: i16,
    pub p90_excess: i16,
}

impl Peak {
    pub fn width_hz(&self) -> f64 {
        self.high_hz - self.low_hz
    }
}

/// Per-visit derived values; never written back to the source data.
#[derive(Debug, Clone, Serialize)]
pub struct VisitMetrics {
    /// Median of the local background over the visit (native tenths).
    pub floor: i16,
    /// Fraction of bins whose median exceeds background by the threshold.
    pub occupancy: f64,
    /// Median over bins of p90 - p10: within-dwell variability.
    pub variability: i16,
    pub peaks: Vec<Peak>,
}

pub fn median_i16(values: &mut [i16]) -> i16 {
    if values.is_empty() {
        return 0;
    }
    let k = (values.len() - 1) / 2;
    *values.select_nth_unstable(k).1
}

/// Background, occupancy, variability and peaks of one visit.
pub fn analyze_visit(s: &VisitSpectrum, p: &Params) -> (VisitMetrics, Vec<i16>) {
    let bg = local_background(
        &s.p50,
        s.bin_hz,
        p.background_window_hz,
        p.background_block_hz,
        p.background_percentile,
    );
    let excess: Vec<i32> = s
        .p50
        .iter()
        .zip(&bg)
        .map(|(v, b)| i32::from(*v) - i32::from(*b))
        .collect();
    let thr = i32::from(p.peak_threshold);
    let above = excess.iter().filter(|e| **e >= thr).count();
    let mut spread: Vec<i16> = s.p90.iter().zip(&s.p10).map(|(a, b)| a.saturating_sub(*b)).collect();
    let mut peaks = Vec::new();
    let mut i = 0;
    let n = excess.len();
    while i < n {
        if excess[i] < thr {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i;
        // Merge runs separated by a single bin below threshold.
        while end + 1 < n && (excess[end + 1] >= thr || (end + 2 < n && excess[end + 2] >= thr)) {
            end += 1;
        }
        while excess[end] < thr {
            end -= 1;
        }
        let best = (start..=end)
            .max_by_key(|&j| (excess[j], std::cmp::Reverse(j)))
            .unwrap_or(start);
        let half = s.bin_hz / 2.0;
        let sat = |v: i32| v.clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16;
        peaks.push(Peak {
            hz: s.bin_center_hz(best),
            low_hz: s.bin_center_hz(start) - half,
            high_hz: s.bin_center_hz(end) + half,
            excess: sat(excess[best]),
            p10_excess: sat(i32::from(s.p10[best]) - i32::from(bg[best])),
            p90_excess: sat(i32::from(s.p90[best]) - i32::from(bg[best])),
        });
        i = end + 1;
    }
    let mut bg_sorted = bg.clone();
    let m = VisitMetrics {
        floor: median_i16(&mut bg_sorted),
        occupancy: if n == 0 { 0.0 } else { above as f64 / n as f64 },
        variability: median_i16(&mut spread),
        peaks,
    };
    (m, bg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_ignores_narrow_peaks_and_follows_slopes() {
        let mut v: Vec<i16> = (0..1000).map(|i| -1300 + (i / 10) as i16).collect();
        for i in [100, 101, 500, 900] {
            v[i] = -800;
        }
        let bg = local_background(&v, 50.0, 5000.0, 1000.0, 0.2);
        assert_eq!(bg.len(), v.len());
        for i in [100, 101, 500, 900] {
            let want = -1300 + (i / 10) as i16;
            assert!((bg[i] - want).abs() <= 10, "bin {i}: {} vs {want}", bg[i]);
        }
    }

    #[test]
    fn short_inputs_and_huge_windows_do_not_panic() {
        assert!(local_background(&[], 50.0, 5000.0, 1000.0, 0.2).is_empty());
        assert_eq!(local_background(&[-5], 50.0, 5000.0, 1000.0, 0.2), vec![-5]);
        assert_eq!(local_background(&[1, 2, 3], 50.0, 1e9, 1e9, 0.0), vec![1, 1, 1]);
    }
}
