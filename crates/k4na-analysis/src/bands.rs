//! Band/hour background summaries.

use std::collections::BTreeMap;

use k4na_core::time::NS_PER_HOUR;
use serde::Serialize;

use crate::background::{VisitMetrics, median_i16};
use crate::spectrum::VisitSpectrum;
use crate::{db, quantile_i16};

/// One UTC hour of one band on one antenna in one comparability segment.
/// Levels are dB on the K4's uncalibrated pan scale, never dBm.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct BandHour {
    pub band_id: String,
    pub antenna_id: String,
    pub segment_id: String,
    pub hour_start_utc_ns: i64,
    pub visits: u64,
    /// Observed spectrum in this hour (sum over visits of clipped coverage).
    pub observed_hz: u64,
    /// Median over visits of each visit's median local background.
    pub floor_db: f64,
    /// 10th/90th percentiles of the per-visit backgrounds.
    pub floor_p10_db: f64,
    pub floor_p90_db: f64,
    pub floor_min_db: f64,
    pub floor_max_db: f64,
    /// Median fraction of bins standing above background by the threshold.
    pub occupancy: f64,
    /// Median within-dwell p90 - p10 spread.
    pub variability_db: f64,
    /// Receiver settings of the segment's first visit (`measurement_json`).
    pub measurement: Option<String>,
}

#[derive(Debug, Default)]
struct Acc {
    visits: u64,
    observed_hz: u64,
    floors: Vec<i16>,
    occupancy: Vec<f64>,
    variability: Vec<i16>,
    measurement: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct BandHours {
    groups: BTreeMap<(String, String, String, i64), Acc>,
}

impl BandHours {
    pub fn add(&mut self, s: &VisitSpectrum, m: &VisitMetrics) {
        let v = &s.visit;
        let hour = v.started_utc_ns.div_euclid(NS_PER_HOUR) * NS_PER_HOUR;
        let key = (
            v.band_id.clone().unwrap_or_else(|| "(no band)".into()),
            v.antenna_id.clone(),
            v.segment_id.clone(),
            hour,
        );
        let a = self.groups.entry(key).or_default();
        a.visits += 1;
        a.observed_hz += u64::try_from(s.high_hz - s.low_hz).unwrap_or(0);
        a.floors.push(m.floor);
        a.occupancy.push(m.occupancy);
        a.variability.push(m.variability);
        if a.measurement.is_none() {
            a.measurement.clone_from(&v.measurement_json);
        }
    }

    pub fn finish(self) -> Vec<BandHour> {
        self.groups
            .into_iter()
            .map(|((band_id, antenna_id, segment_id, hour), mut a)| {
                a.floors.sort_unstable();
                a.occupancy.sort_by(f64::total_cmp);
                BandHour {
                    band_id,
                    antenna_id,
                    segment_id,
                    hour_start_utc_ns: hour,
                    visits: a.visits,
                    observed_hz: a.observed_hz,
                    floor_db: db(quantile_i16(&a.floors, 0.5)),
                    floor_p10_db: db(quantile_i16(&a.floors, 0.1)),
                    floor_p90_db: db(quantile_i16(&a.floors, 0.9)),
                    floor_min_db: db(a.floors[0]),
                    floor_max_db: db(a.floors[a.floors.len() - 1]),
                    occupancy: a.occupancy[(a.occupancy.len() - 1) / 2],
                    variability_db: db(median_i16(&mut a.variability)),
                    measurement: a.measurement,
                }
            })
            .collect()
    }
}
