//! Strict TOML configuration. Unknown keys are errors, and safety-relevant
//! values that this release does not implement are rejected rather than
//! silently ignored.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::units::HzRange;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub radio: RadioConfig,
    pub antenna: AntennaConfig,
    pub collection: CollectionConfig,
    pub storage: StorageConfig,
    #[serde(default)]
    pub bands: Vec<BandConfig>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Receiver {
    /// Main receiver, pan `A`.
    Main,
    /// Sub receiver, pan `B` (passive only in this release).
    Sub,
}

impl Receiver {
    pub fn pan(self) -> k4na_protocol::PanId {
        match self {
            Self::Main => k4na_protocol::PanId::A,
            Self::Sub => k4na_protocol::PanId::B,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Sub => "sub",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RadioConfig {
    pub host: String,
    #[serde(default = "default_cat_port")]
    pub cat_port: u16,
    #[serde(default = "default_spectrum_port")]
    pub spectrum_port: u16,
    #[serde(default = "default_receiver")]
    pub receiver: Receiver,
    /// Survey refuses to start without a firmware (`RV`) readback.
    #[serde(default = "yes")]
    pub require_firmware_readback: bool,
    /// Passive capture may read CAT GET-only metadata. Failure never blocks
    /// spectrum capture. No CAT setter is ever sent in passive mode.
    #[serde(default = "yes")]
    pub passive_cat_readback: bool,
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    /// A spectrum stream silent for this long is treated as stalled.
    #[serde(default = "default_stall_timeout_ms")]
    pub stall_timeout_ms: u64,
    #[serde(default = "default_reconnect_initial_ms")]
    pub reconnect_initial_ms: u64,
    #[serde(default = "default_reconnect_max_ms")]
    pub reconnect_max_ms: u64,
    #[serde(default = "default_cat_timeout_ms")]
    pub cat_timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AntennaConfig {
    pub id: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub revision: String,
    #[serde(default)]
    pub notes: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Passive,
    Survey,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Passive => "passive",
            Self::Survey => "survey",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CollectionConfig {
    pub mode: Mode,
    /// TOML half of the two-part retune consent (CLI `--allow-retune` is the other).
    #[serde(default)]
    pub allow_retune: bool,
    /// `#SPN` request; it is NOT the sampled packet width.
    #[serde(default)]
    pub requested_pan_span_hz: Option<u32>,
    /// Permit the survey to set `#FXT0` (track) if the pan is fixed. Off by
    /// default: a fixed pan makes the survey refuse to start instead.
    #[serde(default)]
    pub set_pan_track_mode: bool,
    #[serde(default = "default_usable_fraction")]
    pub usable_fraction: f64,
    #[serde(default = "default_settle_ms")]
    pub settle_ms: u64,
    #[serde(default = "default_dwell_ms")]
    pub dwell_ms: u64,
    #[serde(default = "yes")]
    pub pause_on_transmit: bool,
    #[serde(default = "yes")]
    pub pause_on_external_change: bool,
    #[serde(default = "yes")]
    pub manual_resume_on_conflict: bool,
    /// At most one original packet per interval per pan; 0 disables raw sampling.
    #[serde(default = "default_raw_interval_ms")]
    pub raw_sample_interval_ms: u64,
    /// Passive capture visit window (statistics are summarized per window).
    #[serde(default = "default_passive_visit_ms")]
    pub passive_visit_ms: u64,
    /// CAT poll interval for TX/VFO interlocks during a survey dwell.
    #[serde(default = "default_cat_poll_ms")]
    pub cat_poll_ms: u64,
    /// Bound on frames held for one visit's exact quantiles.
    #[serde(default = "default_max_frames_per_visit")]
    pub max_frames_per_visit: usize,
    /// Visits with fewer accepted frames are stored as partial.
    #[serde(default = "default_min_frames_per_visit")]
    pub min_frames_per_visit: usize,
    /// Closed-loop VFO corrections per tile when the observed pan center is
    /// offset from the requested frequency.
    #[serde(default = "default_max_tile_corrections")]
    pub max_tile_corrections: u32,
    /// Sample rate assumed by `estimate` and offline dry-runs (the live
    /// survey always plans from observed frame headers).
    #[serde(default = "default_planning_rate")]
    pub planning_sample_rate_hz: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Compression {
    Zstd,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StorageConfig {
    pub root: PathBuf,
    pub max_total_bytes: u64,
    pub min_free_bytes: u64,
    pub raw_retention_hours: u64,
    pub summary_retention_days: u64,
    pub protected_summary_hours: u64,
    #[serde(default = "default_shard_minutes")]
    pub shard_duration_minutes: u64,
    #[serde(default = "default_compression")]
    pub compression: Compression,
    #[serde(default = "default_zstd_level")]
    pub zstd_level: i32,
    /// Fraction of the effective byte budget at which raw sampling stops and
    /// pruning begins.
    #[serde(default = "default_soft_fraction")]
    pub soft_limit_fraction: f64,
    /// Fraction below which raw sampling resumes after pressure.
    #[serde(default = "default_resume_fraction")]
    pub raw_resume_fraction: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct BandConfig {
    pub id: String,
    pub low_hz: u64,
    pub high_hz: u64,
    #[serde(default = "default_weight")]
    pub weight: u32,
}

impl BandConfig {
    pub fn range(&self) -> HzRange {
        HzRange::new(self.low_hz, self.high_hz)
    }
}

fn yes() -> bool {
    true
}
fn default_cat_port() -> u16 {
    9200
}
fn default_spectrum_port() -> u16 {
    9201
}
fn default_receiver() -> Receiver {
    Receiver::Main
}
fn default_connect_timeout_ms() -> u64 {
    5000
}
fn default_stall_timeout_ms() -> u64 {
    5000
}
fn default_reconnect_initial_ms() -> u64 {
    1000
}
fn default_reconnect_max_ms() -> u64 {
    30_000
}
fn default_cat_timeout_ms() -> u64 {
    2000
}
fn default_usable_fraction() -> f64 {
    0.80
}
fn default_settle_ms() -> u64 {
    700
}
fn default_dwell_ms() -> u64 {
    3000
}
fn default_raw_interval_ms() -> u64 {
    1000
}
fn default_passive_visit_ms() -> u64 {
    10_000
}
fn default_cat_poll_ms() -> u64 {
    500
}
fn default_max_frames_per_visit() -> usize {
    1024
}
fn default_min_frames_per_visit() -> usize {
    5
}
fn default_max_tile_corrections() -> u32 {
    2
}
fn default_planning_rate() -> u32 {
    96_000
}
fn default_shard_minutes() -> u64 {
    60
}
fn default_compression() -> Compression {
    Compression::Zstd
}
fn default_zstd_level() -> i32 {
    3
}
fn default_soft_fraction() -> f64 {
    0.85
}
fn default_resume_fraction() -> f64 {
    0.75
}
fn default_weight() -> u32 {
    1
}

/// K4 `FA` accepts 100 kHz to 54 MHz.
pub const K4_MIN_HZ: u64 = 100_000;
pub const K4_MAX_HZ: u64 = 54_000_000;
/// K4 `#SPN` range.
pub const K4_SPAN_RANGE: std::ops::RangeInclusive<u32> = 6_000..=368_000;

#[derive(Debug)]
pub struct ConfigError {
    pub problems: Vec<String>,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "invalid configuration:")?;
        for p in &self.problems {
            writeln!(f, "  - {p}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        let config: Config = toml::from_str(text).map_err(|e| ConfigError {
            problems: vec![e.to_string()],
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Loads, parses and validates a config file. Relative storage roots are
    /// resolved against the current directory, not the config file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError {
            problems: vec![format!("cannot read {}: {e}", path.display())],
        })?;
        Self::from_toml(&text)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut p = Vec::new();
        let r = &self.radio;
        if r.host.trim().is_empty() {
            p.push("radio.host must not be empty".into());
        }
        if r.cat_port == 0 || r.spectrum_port == 0 {
            p.push("radio ports must be non-zero".into());
        }
        if r.stall_timeout_ms < 500 {
            p.push("radio.stall_timeout_ms must be >= 500".into());
        }
        if r.reconnect_initial_ms == 0 || r.reconnect_max_ms < r.reconnect_initial_ms {
            p.push("radio.reconnect_initial_ms must be > 0 and <= reconnect_max_ms".into());
        }
        if r.cat_timeout_ms < 100 {
            p.push("radio.cat_timeout_ms must be >= 100".into());
        }
        if self.antenna.id.trim().is_empty() {
            p.push("antenna.id must not be empty".into());
        }

        let c = &self.collection;
        if !(0.1..=1.0).contains(&c.usable_fraction) {
            p.push("collection.usable_fraction must be within 0.1..=1.0".into());
        }
        if let Some(span) = c.requested_pan_span_hz
            && !K4_SPAN_RANGE.contains(&span)
        {
            p.push(format!(
                "collection.requested_pan_span_hz {span} outside K4 #SPN range 6000..=368000"
            ));
        }
        if c.dwell_ms < 200 {
            p.push("collection.dwell_ms must be >= 200".into());
        }
        if c.settle_ms < 100 {
            p.push("collection.settle_ms must be >= 100".into());
        }
        if c.passive_visit_ms < 500 {
            p.push("collection.passive_visit_ms must be >= 500".into());
        }
        if c.cat_poll_ms < 50 || c.cat_poll_ms > 2000 {
            p.push("collection.cat_poll_ms must be within 50..=2000".into());
        }
        if c.max_frames_per_visit < 2 || c.max_frames_per_visit > 16_384 {
            p.push("collection.max_frames_per_visit must be within 2..=16384".into());
        }
        if c.min_frames_per_visit == 0 || c.min_frames_per_visit > c.max_frames_per_visit {
            p.push("collection.min_frames_per_visit must be within 1..=max_frames_per_visit".into());
        }
        if c.raw_sample_interval_ms != 0 && c.raw_sample_interval_ms < 100 {
            p.push("collection.raw_sample_interval_ms must be 0 (off) or >= 100".into());
        }
        if c.max_tile_corrections > 5 {
            p.push("collection.max_tile_corrections must be <= 5".into());
        }
        if c.planning_sample_rate_hz == 0 {
            p.push("collection.planning_sample_rate_hz must be > 0".into());
        }
        // These interlocks are mandatory; disabling them is not supported.
        if !c.pause_on_transmit {
            p.push("collection.pause_on_transmit=false is not supported: transmit always stops the survey".into());
        }
        if !c.pause_on_external_change {
            p.push(
                "collection.pause_on_external_change=false is not supported: external changes always stop the survey"
                    .into(),
            );
        }
        if !c.manual_resume_on_conflict {
            p.push(
                "collection.manual_resume_on_conflict=false is not supported: automatic resume is not implemented"
                    .into(),
            );
        }
        if c.mode == Mode::Survey {
            if self.radio.receiver != Receiver::Main {
                p.push("survey mode supports only radio.receiver = \"main\" in this release".into());
            }
            if self.bands.is_empty() {
                p.push("survey mode requires at least one [[bands]] entry".into());
            }
        }

        let s = &self.storage;
        if s.root.as_os_str().is_empty() {
            p.push("storage.root must not be empty".into());
        }
        if s.max_total_bytes < 64 * 1024 * 1024 {
            p.push("storage.max_total_bytes must be at least 64 MiB".into());
        }
        if s.shard_duration_minutes == 0 || 1440 % s.shard_duration_minutes != 0 {
            p.push("storage.shard_duration_minutes must evenly divide 1440 (a UTC day)".into());
        }
        if s.raw_retention_hours == 0 {
            p.push("storage.raw_retention_hours must be > 0".into());
        }
        if s.summary_retention_days == 0 {
            p.push("storage.summary_retention_days must be > 0".into());
        }
        if s.protected_summary_hours > s.summary_retention_days * 24 {
            p.push("storage.protected_summary_hours must not exceed summary retention".into());
        }
        if !(1..=19).contains(&s.zstd_level) {
            p.push("storage.zstd_level must be within 1..=19".into());
        }
        if !(0.5..0.99).contains(&s.soft_limit_fraction) {
            p.push("storage.soft_limit_fraction must be within 0.5..0.99".into());
        }
        if !(0.1..s.soft_limit_fraction).contains(&s.raw_resume_fraction) {
            p.push("storage.raw_resume_fraction must be within 0.1..soft_limit_fraction".into());
        }

        let mut ids = std::collections::BTreeSet::new();
        for b in &self.bands {
            if !ids.insert(b.id.as_str()) {
                p.push(format!("duplicate band id {:?}", b.id));
            }
            if b.id.trim().is_empty() || b.id.len() > 32 {
                p.push("band ids must be 1..=32 characters".into());
            }
            if b.low_hz >= b.high_hz {
                p.push(format!("band {}: low_hz must be < high_hz", b.id));
            }
            if b.low_hz < K4_MIN_HZ || b.high_hz > K4_MAX_HZ {
                p.push(format!("band {}: interval must lie within 100 kHz..54 MHz", b.id));
            }
            if b.weight == 0 || b.weight > 10 {
                p.push(format!("band {}: weight must be within 1..=10", b.id));
            }
        }
        let mut sorted: Vec<_> = self.bands.iter().collect();
        sorted.sort_by_key(|b| b.low_hz);
        for w in sorted.windows(2) {
            if w[0].high_hz > w[1].low_hz {
                p.push(format!("bands {} and {} overlap", w[0].id, w[1].id));
            }
        }

        if p.is_empty() {
            Ok(())
        } else {
            Err(ConfigError { problems: p })
        }
    }

    /// SHA-256 of the canonical serialized configuration, recorded with
    /// every session. The radio host is excluded so the digest is not a
    /// network identifier and survives DHCP changes.
    pub fn digest(&self) -> String {
        let redacted = self.redacted();
        let text = toml::to_string(&redacted).unwrap_or_default();
        hex::encode(Sha256::digest(text.as_bytes()))
    }

    /// A copy suitable for storing in measurement shards: no host/IP.
    pub fn redacted(&self) -> Self {
        let mut c = self.clone();
        c.radio.host = "<redacted>".into();
        c
    }

    pub fn band(&self, id: &str) -> Option<&BandConfig> {
        self.bands.iter().find(|b| b.id == id)
    }

    /// The configured band containing `hz`, if any.
    pub fn band_containing(&self, hz: u64) -> Option<&BandConfig> {
        self.bands.iter().find(|b| b.range().contains(hz))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const EXAMPLE: &str = include_str!("../../../config/example.toml");

    #[test]
    fn example_config_is_valid() {
        let c = Config::from_toml(EXAMPLE).expect("example config");
        assert_eq!(c.bands.len(), 8);
        assert_eq!(c.collection.mode, Mode::Survey);
        assert_eq!(c.storage.max_total_bytes, 4 * 1024 * 1024 * 1024);
        assert_eq!(c.radio.receiver, Receiver::Main);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let text = EXAMPLE.replace("[radio]", "[radio]\ncat_script = \"TX;\"");
        let err = Config::from_toml(&text).unwrap_err();
        assert!(err.to_string().contains("cat_script"), "{err}");
    }

    #[test]
    fn disabling_interlocks_is_rejected() {
        let text = EXAMPLE.replace("pause_on_transmit = true", "pause_on_transmit = false");
        let err = Config::from_toml(&text).unwrap_err();
        assert!(err.to_string().contains("pause_on_transmit"));
    }

    #[test]
    fn overlapping_bands_are_rejected() {
        let text = EXAMPLE.replace("high_hz = 7300000", "high_hz = 10120000");
        let err = Config::from_toml(&text).unwrap_err();
        assert!(err.to_string().contains("overlap"), "{err}");
    }

    #[test]
    fn digest_ignores_host() {
        let a = Config::from_toml(EXAMPLE).expect("a");
        let b = Config::from_toml(&EXAMPLE.replace("k4.local", "192.0.2.10")).expect("b");
        assert_eq!(a.digest(), b.digest());
        assert_eq!(b.redacted().radio.host, "<redacted>");
    }
}
