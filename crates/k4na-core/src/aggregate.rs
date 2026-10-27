//! Bounded per-visit, per-bin temporal statistics in native units.

use k4na_protocol::{BIN_COUNT, PanId, SpectrumFrame};
use serde::{Deserialize, Serialize};

use crate::time::Timestamp;
use crate::units::BinGeometry;

/// Statistic definition stored with each summary.
pub const QUANTILE_DEFINITION: &str = "nearest-rank (Hyndman-Fan type 1) over accepted frames; min,p10,p50,p90,max";
/// Order of the statistic arrays in a summary.
pub const STAT_NAMES: [&str; 5] = ["min", "p10", "p50", "p90", "max"];
const QUANTILES: [f64; 5] = [0.0, 0.10, 0.50, 0.90, 1.0];

/// Index of the nearest-rank p-quantile among `n` sorted values.
pub fn nearest_rank_index(p: f64, n: usize) -> usize {
    debug_assert!(n > 0);
    if p <= 0.0 {
        return 0;
    }
    let rank = (p * n as f64).ceil() as usize;
    rank.clamp(1, n) - 1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Accepted,
    /// The frame's pan or geometry differs from the visit; not accepted.
    GeometryChanged,
    /// The visit holds its maximum number of frames; not accepted.
    Full,
}

/// Accumulates every accepted frame of one visit with a fixed observed
/// geometry. Memory is bounded by `max_frames * 2048 * 2` bytes.
#[derive(Debug)]
pub struct VisitAccumulator {
    pan: PanId,
    geometry: BinGeometry,
    max_frames: usize,
    levels: Vec<i16>,
    noise: Vec<i32>,
    first: Option<Timestamp>,
    last: Option<Timestamp>,
    sequence_repeats: u32,
    last_sequence: Option<u8>,
}

impl VisitAccumulator {
    pub fn new(pan: PanId, geometry: BinGeometry, max_frames: usize) -> Self {
        Self {
            pan,
            geometry,
            max_frames,
            levels: Vec::with_capacity(max_frames.min(256) * BIN_COUNT),
            noise: Vec::new(),
            first: None,
            last: None,
            sequence_repeats: 0,
            last_sequence: None,
        }
    }

    pub fn geometry(&self) -> BinGeometry {
        self.geometry
    }

    pub fn pan(&self) -> PanId {
        self.pan
    }

    pub fn frame_count(&self) -> usize {
        self.noise.len()
    }

    pub fn first_frame(&self) -> Option<Timestamp> {
        self.first
    }

    pub fn last_frame(&self) -> Option<Timestamp> {
        self.last
    }

    pub fn is_full(&self) -> bool {
        self.frame_count() >= self.max_frames
    }

    pub fn push(&mut self, frame: &SpectrumFrame, at: Timestamp) -> PushOutcome {
        if frame.pan() != self.pan || BinGeometry::from_header(frame.header()) != self.geometry {
            return PushOutcome::GeometryChanged;
        }
        if self.is_full() {
            return PushOutcome::Full;
        }
        let start = self.levels.len();
        self.levels.resize(start + BIN_COUNT, 0);
        let row: &mut [i16; BIN_COUNT] = (&mut self.levels[start..]).try_into().expect("row is BIN_COUNT long");
        frame.bins_into(row);
        self.noise.push(frame.header().noise_field_tenths);
        let seq = frame.header().sequence;
        if self.last_sequence == Some(seq) {
            self.sequence_repeats += 1;
        }
        self.last_sequence = Some(seq);
        self.first.get_or_insert(at);
        self.last = Some(at);
        PushOutcome::Accepted
    }

    /// Computes exact statistics. Returns `None` if no frame was accepted.
    pub fn finish(self) -> Option<VisitStats> {
        let n = self.frame_count();
        if n == 0 {
            return None;
        }
        let mut stats: [Vec<i16>; 5] = std::array::from_fn(|_| Vec::with_capacity(BIN_COUNT));
        let idx: [usize; 5] = QUANTILES.map(|p| nearest_rank_index(p, n));
        let mut column = vec![0i16; n];
        let mut identical_frames = true;
        for bin in 0..BIN_COUNT {
            for (f, slot) in column.iter_mut().enumerate() {
                *slot = self.levels[f * BIN_COUNT + bin];
            }
            column.sort_unstable();
            if column[0] != column[n - 1] {
                identical_frames = false;
            }
            for (s, i) in stats.iter_mut().zip(idx) {
                s.push(column[i]);
            }
        }
        let mut noise = self.noise.clone();
        noise.sort_unstable();
        Some(VisitStats {
            frame_count: n as u32,
            stats,
            noise_field_min: noise[0],
            noise_field_median: noise[nearest_rank_index(0.5, n)],
            noise_field_max: noise[n - 1],
            sequence_repeats: self.sequence_repeats,
            // Real receiver noise makes bit-identical consecutive sweeps
            // essentially impossible; flag as possibly frozen.
            identical_frames: n > 1 && identical_frames,
        })
    }
}

/// Exact per-bin statistics for one visit, in native tenths of a dB.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisitStats {
    pub frame_count: u32,
    /// `[min, p10, p50, p90, max]`, each `BIN_COUNT` long, wire bin order.
    pub stats: [Vec<i16>; 5],
    pub noise_field_min: i32,
    pub noise_field_median: i32,
    pub noise_field_max: i32,
    pub sequence_repeats: u32,
    pub identical_frames: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use k4na_protocol::synth::SyntheticFrame;

    fn frame(seq: u8, value_for_bin: impl Fn(usize) -> i16) -> SpectrumFrame {
        let bins: Vec<i16> = (0..BIN_COUNT).map(value_for_bin).collect();
        let s = SyntheticFrame {
            sequence: seq,
            ..SyntheticFrame::default()
        };
        SpectrumFrame::from_bytes(&s.encode(&bins)).expect("valid")
    }

    fn ts(i: u64) -> Timestamp {
        Timestamp {
            utc_ns: i as i64 * 66_000_000,
            mono_ns: i * 66_000_000,
        }
    }

    #[test]
    fn nearest_rank_definition() {
        assert_eq!(nearest_rank_index(0.0, 10), 0);
        assert_eq!(nearest_rank_index(0.1, 10), 0);
        assert_eq!(nearest_rank_index(0.5, 10), 4);
        assert_eq!(nearest_rank_index(0.9, 10), 8);
        assert_eq!(nearest_rank_index(1.0, 10), 9);
        assert_eq!(nearest_rank_index(0.5, 1), 0);
        assert_eq!(nearest_rank_index(0.1, 45), 4);
    }

    #[test]
    fn exact_quantiles_per_bin() {
        let g = BinGeometry::new(14_045_000, 96_000, 2048);
        let mut acc = VisitAccumulator::new(PanId::A, g, 100);
        // Frame k has value k*10 + bin%3 in every bin, k = 1..=10 pushed out of order.
        for (i, k) in [5, 1, 9, 3, 7, 2, 10, 4, 8, 6].into_iter().enumerate() {
            assert_eq!(
                acc.push(&frame(i as u8, |b| k * 10 + (b % 3) as i16), ts(i as u64)),
                PushOutcome::Accepted
            );
        }
        let s = acc.finish().expect("stats");
        assert_eq!(s.frame_count, 10);
        assert_eq!(s.stats[0][0], 10);
        assert_eq!(s.stats[1][0], 10);
        assert_eq!(s.stats[2][0], 50);
        assert_eq!(s.stats[3][1], 91);
        assert_eq!(s.stats[4][2], 102);
        assert!(!s.identical_frames);
        assert_eq!(s.noise_field_min, -1300);
    }

    #[test]
    fn rejects_geometry_change_and_bounds_memory() {
        let g = BinGeometry::new(14_045_000, 96_000, 2048);
        let mut acc = VisitAccumulator::new(PanId::A, g, 2);
        assert_eq!(acc.push(&frame(0, |_| 1), ts(0)), PushOutcome::Accepted);
        let other = SyntheticFrame {
            center_hz: 7_000_000,
            ..SyntheticFrame::default()
        };
        let other = SpectrumFrame::from_bytes(&other.encode(&[])).expect("valid");
        assert_eq!(acc.push(&other, ts(1)), PushOutcome::GeometryChanged);
        let sub = SyntheticFrame {
            pan: PanId::B,
            ..SyntheticFrame::default()
        };
        let sub = SpectrumFrame::from_bytes(&sub.encode(&[])).expect("valid");
        assert_eq!(acc.push(&sub, ts(1)), PushOutcome::GeometryChanged);
        assert_eq!(acc.push(&frame(1, |_| 1), ts(2)), PushOutcome::Accepted);
        assert_eq!(acc.push(&frame(2, |_| 1), ts(3)), PushOutcome::Full);
        assert_eq!(acc.frame_count(), 2);
        assert_eq!(acc.first_frame(), Some(ts(0)));
        assert_eq!(acc.last_frame(), Some(ts(2)));
        let s = acc.finish().expect("stats");
        assert!(s.identical_frames);
    }

    #[test]
    fn empty_visit_has_no_stats() {
        let g = BinGeometry::new(1, 1, 2048);
        assert!(VisitAccumulator::new(PanId::A, g, 4).finish().is_none());
    }
}
