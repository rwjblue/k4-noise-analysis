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
    /// Pruning is per shard and open shards are never deleted, so up to one
    /// extra summary shard beyond the protected window and one open raw
    /// shard cannot be reclaimed.
    pub unprunable_shard_bytes: u64,
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
        unprunable_shard_bytes: ((summary_bytes_per_day + raw_bytes_per_day) as f64 * hours(opts.shard_duration_ns)
            / 24.0) as u64,
        summary_days_at_cap: opts.max_total_bytes as f64 * opts.soft_limit_fraction
            / summary_bytes_per_day.max(1) as f64,
    }
}

/// Refuses a run that cannot hold its protected summary window under both
/// the configured cap and the volume's free space (after `min_free_bytes`).
fn usable_bytes(opts: &StoreOptions, usage_bytes: u64, free_bytes: u64) -> u64 {
    let budget = opts
        .max_total_bytes
        .min((usage_bytes + free_bytes).saturating_sub(opts.min_free_bytes));
    // Same basis as the writer's soft limit: a fraction of what may be used
    // before the hard ceiling (budget minus headroom).
    (budget.saturating_sub(opts.headroom_bytes) as f64 * opts.soft_limit_fraction) as u64
}

/// A full-rate raw capture is expendable like all raw data, so storage
/// pressure would prune its own start rather than summaries. Refuse one
/// that cannot fit beside the protected summary window. `packet_bytes`
/// (uncompressed) bounds its disk use.
pub fn admit_raw_capture(
    opts: &StoreOptions,
    est: &Estimate,
    usage_bytes: u64,
    free_bytes: u64,
    packet_bytes: u64,
) -> Result<(), String> {
    let usable = usable_bytes(opts, usage_bytes, free_bytes);
    let need = est.protected_summary_bytes + est.unprunable_shard_bytes + packet_bytes;
    if need > usable {
        return Err(format!(
            "a full-rate raw capture of up to {packet_bytes} packet bytes does not fit beside the protected summary \
             window ({} + one unprunable shard {}) in {usable} usable bytes; storage pressure would prune the capture. \
             Lower --raw-full-rate-max-bytes, free disk space or raise max_total_bytes.",
            est.protected_summary_bytes, est.unprunable_shard_bytes
        ));
    }
    Ok(())
}

pub fn admission(opts: &StoreOptions, est: &Estimate, usage_bytes: u64, free_bytes: u64) -> Result<(), String> {
    let usable = usable_bytes(opts, usage_bytes, free_bytes);
    let need = est.protected_summary_bytes + est.unprunable_shard_bytes;
    if need > usable {
        return Err(format!(
            "storage cannot hold the protected summary window: need ~{} bytes (upper bound: protected summaries {} + \
             one unprunable shard {}) but only {} are usable \
             ((min(max_total_bytes {}, used {} + free {} - min_free_bytes {}) - headroom {}) x soft_limit_fraction {}). \
             Free disk space, raise max_total_bytes, lower min_free_bytes, protected_summary_hours or \
             shard_duration_minutes.",
            need,
            est.protected_summary_bytes,
            est.unprunable_shard_bytes,
            usable,
            opts.max_total_bytes,
            usage_bytes,
            free_bytes,
            opts.min_free_bytes,
            opts.headroom_bytes,
            opts.soft_limit_fraction
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use k4na_core::Config;

    fn opts() -> StoreOptions {
        let c = Config::from_parts(
            include_str!("../../../config/example.toml"),
            Some(include_str!("../../../config/surveys/hf-full.toml")),
        )
        .unwrap();
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
    fn a_raw_capture_must_fit_beside_the_protected_summaries() {
        let o = opts();
        let e = estimate(&o, 1, 4000, 3000, 1000);
        let free = 100 * (1 << 30);
        assert!(admit_raw_capture(&o, &e, 0, free, 1 << 30).is_ok());
        // Half of a 4 GiB cap passes the config check but not beside a day
        // of protected summaries.
        let err = admit_raw_capture(&o, &e, 0, free, o.max_total_bytes / 2 + (1 << 30)).unwrap_err();
        assert!(err.contains("would prune the capture"), "{err}");
        // Little free disk refuses even a small capture.
        assert!(admit_raw_capture(&o, &e, 0, o.min_free_bytes + (900 << 20), 1 << 30).is_err());
    }

    #[test]
    fn refuses_when_free_space_is_too_small() {
        let o = opts();
        let e = estimate(&o, 1, 4000, 3000, 1000);
        // 5.2 GiB free minus 5 GiB minimum leaves ~200 MiB: too small for a day.
        let err = admission(&o, &e, 0, 5_600_000_000).unwrap_err();
        assert!(err.contains("protected summary window"), "{err}");
    }

    #[test]
    fn counts_the_shard_that_cannot_be_pruned() {
        // 64 MiB with a 1 h protected window fits only if shard granularity
        // is ignored: hourly shards keep up to 2 h of summaries plus an open
        // raw shard.
        let mut o = opts();
        o.max_total_bytes = 64 << 20;
        o.protected_summary_ns = 3_600_000_000_000;
        let e = estimate(&o, 1, 3950, 3000, 1000);
        assert!(
            e.protected_summary_bytes + o.headroom_bytes < (o.max_total_bytes as f64 * o.soft_limit_fraction) as u64
        );
        assert!(admission(&o, &e, 0, 1 << 40).unwrap_err().contains("unprunable shard"));
        o.shard_duration_ns = 15 * 60_000_000_000;
        o.protected_summary_ns = 3_600_000_000_000;
        let e = estimate(&o, 1, 3950, 3000, 1000);
        assert!(admission(&o, &e, 0, 1 << 40).is_ok());
    }
}
