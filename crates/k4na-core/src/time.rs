//! Collector-host time. Frames carry arrival time, never a K4 RF clock.

use serde::{Deserialize, Serialize};

/// UTC wall time plus monotonic elapsed time since the session clock began.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Timestamp {
    pub utc_ns: i64,
    pub mono_ns: u64,
}

impl Timestamp {
    pub fn mono_since(&self, earlier: &Timestamp) -> std::time::Duration {
        std::time::Duration::from_nanos(self.mono_ns.saturating_sub(earlier.mono_ns))
    }

    /// Wall-clock minus monotonic drift relative to `earlier`, in ns. A large
    /// magnitude means the UTC clock was stepped.
    pub fn clock_skew_ns(&self, earlier: &Timestamp) -> i64 {
        let utc = self.utc_ns - earlier.utc_ns;
        let mono = i64::try_from(self.mono_ns.saturating_sub(earlier.mono_ns)).unwrap_or(i64::MAX);
        utc - mono
    }
}

pub trait Clock: Send + Sync + std::fmt::Debug {
    fn now(&self) -> Timestamp;
}

/// Real host clock: `SystemTime` for UTC and `Instant` for monotonic.
#[derive(Debug)]
pub struct SystemClock {
    origin: std::time::Instant,
}

impl Default for SystemClock {
    fn default() -> Self {
        Self {
            origin: std::time::Instant::now(),
        }
    }
}

pub fn system_utc_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp {
            utc_ns: system_utc_ns(),
            mono_ns: u64::try_from(self.origin.elapsed().as_nanos()).unwrap_or(u64::MAX),
        }
    }
}

pub const NS_PER_SEC: i64 = 1_000_000_000;
pub const NS_PER_HOUR: i64 = 3600 * NS_PER_SEC;

/// Formats UTC nanoseconds as RFC 3339 with millisecond precision.
pub fn format_utc(utc_ns: i64) -> String {
    let secs = utc_ns.div_euclid(NS_PER_SEC);
    let millis = utc_ns.rem_euclid(NS_PER_SEC) / 1_000_000;
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let sod = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        sod / 3600,
        (sod / 60) % 60,
        sod % 60
    )
}

/// UTC civil date for a count of days since 1970-01-01 (H. Hinnant's algorithm).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_utc() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00.000Z");
        // 2026-10-22T00:43:30.546Z
        assert_eq!(format_utc(1_792_629_810_546_071_000), "2026-10-22T00:43:30.546Z");
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn skew_detects_clock_steps() {
        let a = Timestamp {
            utc_ns: 1_000,
            mono_ns: 0,
        };
        let b = Timestamp {
            utc_ns: 1_000 + 5 * NS_PER_SEC,
            mono_ns: NS_PER_SEC as u64,
        };
        assert_eq!(b.clock_skew_ns(&a), 4 * NS_PER_SEC);
    }
}
