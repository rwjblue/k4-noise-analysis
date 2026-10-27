//! Survey tile planning from the *observed* packet sample rate.

use serde::{Deserialize, Serialize};

use crate::band::k4_band_number;
use crate::config::RangeConfig;
use crate::units::{BinGeometry, HzRange};

/// One frequency tile: a target interval that a single pan visit must
/// fully cover with its usable interior.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tile {
    /// Stable index within the plan.
    pub index: usize,
    pub band_id: String,
    pub tile_in_band: u32,
    pub tiles_in_band: u32,
    pub target: HzRange,
    /// Desired pan center (the commanded VFO may be offset from this).
    pub center_hz: u64,
    pub k4_band: Option<u8>,
}

impl Tile {
    pub fn id(&self) -> String {
        format!("{}/{}of{}", self.band_id, self.tile_in_band + 1, self.tiles_in_band)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurveyPlan {
    pub sample_rate_hz: u32,
    pub bin_count: u32,
    pub usable_fraction: f64,
    /// Usable interior width of one packet at this rate.
    pub usable_width_hz: u64,
    /// Maximum target width per tile (usable width minus alignment margin).
    pub max_tile_width_hz: u64,
    pub tiles: Vec<Tile>,
    /// One scheduling cycle, as tile indices. Every band appears at least once
    /// per cycle, so no band can be starved.
    pub cycle: Vec<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("no bands configured")]
    NoBands,
    #[error("observed sample rate {0} Hz leaves no usable tile width")]
    TooNarrow(u32),
}

impl SurveyPlan {
    /// Builds a plan. Each band is split into the fewest equal contiguous
    /// target pieces whose width fits the usable interior with a margin of
    /// four bins on each side, so both band ends are covered.
    pub fn build(
        bands: &[RangeConfig],
        sample_rate_hz: u32,
        bin_count: u32,
        usable_fraction: f64,
    ) -> Result<Self, PlanError> {
        if bands.is_empty() {
            return Err(PlanError::NoBands);
        }
        // Width is translation-invariant; use a center well above 0 Hz so
        // the lower edge is not clamped.
        let geometry = BinGeometry::new(u64::from(sample_rate_hz) * 4, sample_rate_hz, bin_count);
        let usable_width_hz = geometry.usable_range(usable_fraction).width();
        let margin = (4.0 * geometry.bin_width_hz()).ceil() as u64;
        let max_tile_width_hz = usable_width_hz.saturating_sub(2 * margin);
        if max_tile_width_hz < 1000 {
            return Err(PlanError::TooNarrow(sample_rate_hz));
        }
        let mut ordered: Vec<&RangeConfig> = bands.iter().collect();
        ordered.sort_by_key(|b| b.low_hz);

        let mut tiles = Vec::new();
        let mut band_tiles: Vec<(u32, Vec<usize>)> = Vec::new();
        for band in &ordered {
            let width = band.high_hz - band.low_hz;
            let count = width.div_ceil(max_tile_width_hz).max(1);
            let mut indices = Vec::new();
            for j in 0..count {
                let low = band.low_hz + width * j / count;
                let high = band.low_hz + width * (j + 1) / count;
                let target = HzRange::new(low, high);
                let center_hz = target.midpoint();
                indices.push(tiles.len());
                tiles.push(Tile {
                    index: tiles.len(),
                    band_id: band.id.clone(),
                    tile_in_band: j as u32,
                    tiles_in_band: count as u32,
                    target,
                    center_hz,
                    k4_band: k4_band_number(center_hz),
                });
            }
            band_tiles.push((band.weight, indices));
        }

        // Band-at-a-time sweeps minimize band (BPF relay) changes. A band of
        // weight w is swept w times per cycle, in separate rounds.
        let max_weight = band_tiles.iter().map(|(w, _)| *w).max().unwrap_or(1);
        let mut cycle = Vec::new();
        for round in 0..max_weight {
            for (weight, indices) in &band_tiles {
                if *weight > round {
                    cycle.extend(indices);
                }
            }
        }
        Ok(Self {
            sample_rate_hz,
            bin_count,
            usable_fraction,
            usable_width_hz,
            max_tile_width_hz,
            tiles,
            cycle,
        })
    }

    pub fn tiles_for_band<'a>(&'a self, band_id: &'a str) -> impl Iterator<Item = &'a Tile> + 'a {
        self.tiles.iter().filter(move |t| t.band_id == band_id)
    }

    /// Upper bound on one cycle's duration given per-visit timings.
    pub fn cycle_duration(&self, settle_ms: u64, dwell_ms: u64, overhead_ms: u64) -> std::time::Duration {
        std::time::Duration::from_millis(self.cycle.len() as u64 * (settle_ms + dwell_ms + overhead_ms))
    }

    /// The tile of `band_id` whose center is nearest `center_hz` (used to
    /// resume a cycle after a plan rebuild).
    pub fn tile_matching(&self, band_id: &str, center_hz: u64) -> Option<&Tile> {
        self.tiles
            .iter()
            .filter(|t| t.band_id == band_id)
            .min_by_key(|t| t.center_hz.abs_diff(center_hz))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn bands() -> Vec<RangeConfig> {
        Config::from_parts(
            include_str!("../../../config/example.toml"),
            Some(include_str!("../../../config/surveys/hf-full.toml")),
        )
        .expect("config")
        .ranges()
        .to_vec()
    }

    #[test]
    fn default_bands_are_fully_covered_by_usable_interiors() {
        let plan = SurveyPlan::build(&bands(), 96_000, 2048, 0.8).expect("plan");
        assert_eq!(plan.usable_width_hz, 76_781);
        for band in bands() {
            let tiles: Vec<_> = plan.tiles_for_band(&band.id).collect();
            assert_eq!(tiles.first().map(|t| t.target.low_hz), Some(band.low_hz));
            assert_eq!(tiles.last().map(|t| t.target.high_hz), Some(band.high_hz));
            for w in tiles.windows(2) {
                assert_eq!(w[0].target.high_hz, w[1].target.low_hz, "contiguous");
            }
            for t in &tiles {
                // A packet centered on the tile covers its target with margin.
                let g = BinGeometry::new(t.center_hz, 96_000, 2048);
                let usable = g.usable_range(0.8);
                assert!(usable.covers(&t.target), "{} {usable} vs {}", t.id(), t.target);
                assert!(usable.low_hz + 180 <= t.target.low_hz);
                assert!(t.k4_band.is_some());
            }
            // Fewest tiles: one fewer would not fit.
            let n = tiles.len() as u64;
            assert!((band.high_hz - band.low_hz) > (n - 1) * plan.max_tile_width_hz);
        }
        let counts: Vec<_> = bands().iter().map(|b| plan.tiles_for_band(&b.id).count()).collect();
        assert_eq!(counts, [7, 4, 1, 5, 2, 6, 2, 23]);
        assert_eq!(plan.cycle.len(), 50);
    }

    #[test]
    fn weights_repeat_bands_without_starving_others() {
        let mut b = bands();
        b[2].weight = 3; // 30m
        let plan = SurveyPlan::build(&b, 96_000, 2048, 0.8).expect("plan");
        let thirty: Vec<_> = plan.tiles_for_band("30m").map(|t| t.index).collect();
        assert_eq!(plan.cycle.iter().filter(|i| thirty.contains(i)).count(), 3);
        for band in &b {
            assert!(
                plan.tiles_for_band(&band.id).all(|t| plan.cycle.contains(&t.index)),
                "{} starved",
                band.id
            );
        }
    }

    #[test]
    fn narrow_rates_need_more_tiles() {
        let wide = SurveyPlan::build(&bands(), 96_000, 2048, 0.8).expect("wide");
        let narrow = SurveyPlan::build(&bands(), 24_000, 2048, 0.8).expect("narrow");
        assert!(narrow.tiles.len() > 3 * wide.tiles.len());
        assert!(SurveyPlan::build(&bands(), 1_000, 2048, 0.8).is_err());
    }
}
