//! A visit's stored arrays mapped to RF frequency.

use k4na_core::units::BinGeometry;
use k4na_store::reader::VisitRow;

/// The usable part of one complete visit: the bins whose centres lie in
/// the visit's observed coverage clipped to its planned tile, with their
/// RF coordinates from the frame-header geometry (not the requested CAT
/// frequency or span). Values are native K4 tenths of a dB.
#[derive(Debug, Clone)]
pub struct VisitSpectrum {
    pub visit: VisitRow,
    /// Coverage `[low_hz, high_hz)` the bins represent.
    pub low_hz: i64,
    pub high_hz: i64,
    /// RF centre of bin 0 of the arrays below.
    pub first_bin_hz: f64,
    pub bin_hz: f64,
    pub min: Vec<i16>,
    pub p10: Vec<i16>,
    pub p50: Vec<i16>,
    pub p90: Vec<i16>,
    pub max: Vec<i16>,
}

/// Observed coverage of a visit, clipped to its planned tile when it has
/// one (so overlapping neighbours are not counted twice).
pub fn clipped_range(v: &VisitRow) -> (i64, i64) {
    match (v.target_low_hz, v.target_high_hz) {
        (Some(lo), Some(hi)) => (v.actual_low_hz.max(lo), v.actual_high_hz.min(hi)),
        _ => (v.actual_low_hz, v.actual_high_hz),
    }
}

impl VisitSpectrum {
    /// `None` for incomplete visits, inconsistent arrays or an empty
    /// usable range: such visits are not observations of the spectrum.
    pub fn new(visit: &VisitRow, [min, p10, p50, p90, max]: [Vec<i16>; 5]) -> Option<Self> {
        if visit.status != "complete" {
            return None;
        }
        let bins = u32::try_from(visit.bin_count).ok().filter(|b| *b > 0)?;
        let n = bins as usize;
        if [&min, &p10, &p50, &p90, &max].iter().any(|a| a.len() != n) {
            return None;
        }
        let g = BinGeometry::new(
            u64::try_from(visit.center_hz).ok()?,
            u32::try_from(visit.sample_rate_hz).ok().filter(|r| *r > 0)?,
            bins,
        );
        let (low_hz, high_hz) = clipped_range(visit);
        let w = g.bin_width_hz();
        let c0 = g.bin_center_hz(0);
        // Bins whose centre lies in [low, high).
        let index = |hz: i64| ((hz as f64 - c0) / w).ceil().clamp(0.0, n as f64) as usize;
        let (a, b) = (index(low_hz), index(high_hz));
        if a >= b {
            return None;
        }
        Some(Self {
            visit: visit.clone(),
            low_hz,
            high_hz,
            first_bin_hz: c0 + a as f64 * w,
            bin_hz: w,
            min: min[a..b].to_vec(),
            p10: p10[a..b].to_vec(),
            p50: p50[a..b].to_vec(),
            p90: p90[a..b].to_vec(),
            max: max[a..b].to_vec(),
        })
    }

    pub fn len(&self) -> usize {
        self.p50.len()
    }

    pub fn is_empty(&self) -> bool {
        self.p50.is_empty()
    }

    pub fn bin_center_hz(&self, i: usize) -> f64 {
        self.first_bin_hz + i as f64 * self.bin_hz
    }
}
