//! Frequency coordinates and the provisional bin-to-RF mapping.

use serde::{Deserialize, Serialize};

/// Half-open RF interval `[low_hz, high_hz)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HzRange {
    pub low_hz: u64,
    pub high_hz: u64,
}

impl HzRange {
    pub fn new(low_hz: u64, high_hz: u64) -> Self {
        Self { low_hz, high_hz }
    }

    pub fn width(&self) -> u64 {
        self.high_hz.saturating_sub(self.low_hz)
    }

    pub fn contains(&self, hz: u64) -> bool {
        hz >= self.low_hz && hz < self.high_hz
    }

    pub fn covers(&self, other: &HzRange) -> bool {
        self.low_hz <= other.low_hz && self.high_hz >= other.high_hz
    }

    pub fn midpoint(&self) -> u64 {
        self.low_hz + self.width() / 2
    }
}

impl std::fmt::Display for HzRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:.3}-{:.3} kHz",
            self.low_hz as f64 / 1e3,
            self.high_hz as f64 / 1e3
        )
    }
}

/// Identifies the bin-to-frequency convention stored with every summary.
/// Bins ascend in frequency, already centered (no FFT half swap):
/// `f(i) = center + (i - n/2) * rate / n`. Sub-bin edge convention is not
/// established by hardware evidence; analysis should treat ±1 bin as the
/// mapping uncertainty.
pub const MAPPING_VERSION: &str = "k4-lan-centered-ascending/1";

/// Observed geometry of one frame, taken only from its own header.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BinGeometry {
    pub center_hz: u64,
    pub sample_rate_hz: u32,
    pub bin_count: u32,
}

impl BinGeometry {
    pub fn new(center_hz: u64, sample_rate_hz: u32, bin_count: u32) -> Self {
        Self {
            center_hz,
            sample_rate_hz,
            bin_count,
        }
    }

    pub fn from_header(h: &k4na_protocol::FrameHeader) -> Self {
        Self::new(h.center_hz, h.sample_rate_hz, k4na_protocol::BIN_COUNT as u32)
    }

    pub fn bin_width_hz(&self) -> f64 {
        f64::from(self.sample_rate_hz) / f64::from(self.bin_count)
    }

    /// Nominal center frequency of bin `i`.
    pub fn bin_center_hz(&self, i: u32) -> f64 {
        self.center_hz as f64 + (f64::from(i) - f64::from(self.bin_count) / 2.0) * self.bin_width_hz()
    }

    /// Bins dropped from each edge to keep `usable_fraction` of the packet.
    /// Rounds up so the usable claim is never wider than configured.
    pub fn edge_bins(&self, usable_fraction: f64) -> u32 {
        let drop = f64::from(self.bin_count) * (1.0 - usable_fraction.clamp(0.0, 1.0)) / 2.0;
        (drop.ceil() as u32).min(self.bin_count / 2)
    }

    /// RF span of bins `first..=last` (bin edges at ±half a bin), rounded
    /// inward to whole Hz so coverage claims are conservative.
    pub fn bins_range(&self, first: u32, last: u32) -> HzRange {
        let half = self.bin_width_hz() / 2.0;
        let low = (self.bin_center_hz(first) - half).max(0.0).ceil() as u64;
        let high = (self.bin_center_hz(last) + half).max(0.0).floor() as u64;
        HzRange::new(low, high.max(low))
    }

    /// Entire packet span.
    pub fn full_range(&self) -> HzRange {
        self.bins_range(0, self.bin_count - 1)
    }

    /// Usable interior after excluding `edge_bins` at each side.
    pub fn usable_range(&self, usable_fraction: f64) -> HzRange {
        let e = self.edge_bins(usable_fraction);
        self.bins_range(e, self.bin_count - 1 - e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mapping_matches_documented_formula() {
        let g = BinGeometry::new(14_045_000, 96_000, 2048);
        assert_eq!(g.bin_width_hz(), 46.875);
        assert_eq!(g.bin_center_hz(1024), 14_045_000.0);
        assert_eq!(g.bin_center_hz(0), 14_045_000.0 - 48_000.0);
        let full = g.full_range();
        assert_eq!(full.low_hz, 13_996_977); // 13,996,976.5625 rounded inward
        assert_eq!(full.high_hz, 14_092_976);
        assert_eq!(g.edge_bins(0.8), 205);
        let usable = g.usable_range(0.8);
        assert_eq!(usable.width(), 76_781); // 1638 bins * 46.875 Hz, rounded inward
        assert!(usable.contains(14_045_000));
    }

    #[test]
    fn range_helpers() {
        let r = HzRange::new(100, 200);
        assert!(r.contains(100) && !r.contains(200));
        assert!(r.covers(&HzRange::new(120, 200)));
        assert!(!r.covers(&HzRange::new(99, 150)));
        assert_eq!(r.midpoint(), 150);
    }
}
