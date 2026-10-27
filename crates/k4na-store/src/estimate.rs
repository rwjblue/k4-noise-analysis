//! Planning estimates and admission control. Estimates are conservative
//! **upper bounds** (no compression credit); actual usage is measured live.

use serde::Serialize;

use crate::StoreOptions;

/// Five 2048-bin i16 arrays plus visit/summary row, index and page overhead.
pub const SUMMARY_VISIT_UPPER_BYTES: u64 = 5 * 2048 * 2 + 3 * 1024;
/// One 4,162-byte wire frame plus row and index overhead.
pub const RAW_FRAME_UPPER_BYTES: u64 = 4162 + 256;

#[derive(Debug, Clone, Serialize)]
pub struct Estimate {
    pub visits_per_hour: f64,
    pub raw_frames_per_hour: f64,
    pub summary_bytes_per_day: u64,
    pub raw_bytes_per_day: u64,
    /// Summary bytes needed to hold the protected window.
    pub protected_summary_bytes: u64,
    /// Raw bytes needed for the nominal raw retention.
    pub raw_retention_bytes: u64,
    /// Days of summaries the byte cap can hold with raw sampling paused.
    pub summary_days_at_cap: f64,
}

/// `visit_ms` is the wall time of one visit (survey: settle + dwell +
/// overhead; passive: the visit window). Raw frames are sampled at most
/// once per `raw_interval_ms` per pan while frames are accepted
/// (`accepting_ms` of each visit).
pub fn estimate(opts: &StoreOptions, pans: u32, visit_ms: u64, accepting_ms: u64, raw_interval_ms: u64) -> Estimate {
    let visits_per_hour = 3_600_000.0 / visit_ms.max(1) as f64 * f64::from(pans);
    let raw_per_visit = accepting_ms
        .checked_div(raw_interval_ms)
        .map_or(0.0, |n| (n + 1) as f64);
    let raw_frames_per_hour = visits_per_hour * raw_per_visit;
    let summary_bytes_per_day = (visits_per_hour * 24.0 * SUMMARY_VISIT_UPPER_BYTES as f64) as u64;
    let raw_bytes_per_day = (raw_frames_per_hour * 24.0 * RAW_FRAME_UPPER_BYTES as f64) as u64;
    let hours = |ns: i64| ns as f64 / 3.6e12;
    Estimate {
        visits_per_hour,
        raw_frames_per_hour,
        summary_bytes_per_day,
        raw_bytes_per_day,
        protected_summary_bytes: (summary_bytes_per_day as f64 * hours(opts.protected_summary_ns) / 24.0) as u64,
        raw_retention_bytes: (raw_bytes_per_day as f64 * hours(opts.raw_retention_ns) / 24.0) as u64,
        summary_days_at_cap: opts.max_total_bytes as f64 * opts.soft_limit_fraction
            / summary_bytes_per_day.max(1) as f64,
    }
}

/// Refuses a run that cannot hold its protected summary window under both
/// the configured cap and the volume's free space (after `min_free_bytes`).
pub fn admission(opts: &StoreOptions, est: &Estimate, usage_bytes: u64, free_bytes: u64) -> Result<(), String> {
    let budget = opts
        .max_total_bytes
        .min((usage_bytes + free_bytes).saturating_sub(opts.min_free_bytes));
    let usable = (budget as f64 * opts.soft_limit_fraction) as u64;
    let need = est.protected_summary_bytes + opts.headroom_bytes;
    if need > usable {
        return Err(format!(
            "storage cannot hold the protected summary window: need ~{} bytes (upper bound) but only {} are usable \
             (min(max_total_bytes {}, used {} + free {} - min_free_bytes {}) x soft_limit_fraction {}). \
             Free disk space, raise max_total_bytes, lower min_free_bytes or protected_summary_hours.",
            need, usable, opts.max_total_bytes, usage_bytes, free_bytes, opts.min_free_bytes, opts.soft_limit_fraction
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use k4na_core::Config;

    fn opts() -> StoreOptions {
        let c = Config::from_toml(include_str!("../../../config/example.toml")).unwrap();
        StoreOptions::from_config(&c.storage)
    }

    #[test]
    fn example_survey_fits_in_4_gib() {
        let o = opts();
        // 700 ms settle + 3000 ms dwell + ~300 ms CAT overhead.
        let e = estimate(&o, 1, 4000, 3000, 1000);
        assert_eq!(e.visits_per_hour, 900.0);
        assert!(e.summary_bytes_per_day < 600_000_000, "{e:?}");
        assert!(e.protected_summary_bytes < (o.max_total_bytes as f64 * 0.85) as u64);
        assert!(admission(&o, &e, 0, 100 * (1 << 30)).is_ok());
    }

    #[test]
    fn refuses_when_free_space_is_too_small() {
        let o = opts();
        let e = estimate(&o, 1, 4000, 3000, 1000);
        // 5.2 GiB free minus 5 GiB minimum leaves ~200 MiB: too small for a day.
        let err = admission(&o, &e, 0, 5_600_000_000).unwrap_err();
        assert!(err.contains("protected summary window"), "{err}");
    }
}
