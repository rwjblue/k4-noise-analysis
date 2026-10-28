//! Strict TOML configuration. Unknown keys are errors, and safety-relevant
//! values that this release does not implement are rejected rather than
//! silently ignored.
//!
//! Two files make up a configuration:
//! * the **station** file: radio, antenna registry, collection safety and
//!   storage. It changes rarely.
//! * a **survey profile**: which antennas and frequency ranges to survey
//!   and how often. Keep several (whole bands, CW segments only, ...) and
//!   pick one with `collection.survey_profile` or `--profile`.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::band::k4_band_number;
use crate::measure::MeasurementConfig;
use crate::units::HzRange;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub radio: RadioConfig,
    /// Every antenna that can be connected, with the K4 port that proves it
    /// is selected.
    pub antennas: Vec<AntennaConfig>,
    pub collection: CollectionConfig,
    pub storage: StorageConfig,
    /// The loaded survey profile. Never read from the station file; it is
    /// serialized so the digest and stored session config cover it.
    #[serde(default, skip_deserializing)]
    pub profile: Option<SurveyProfile>,
}

/// A main-receiver RF input on the K4 (HF-6 m).
///
/// The K4 Programmer's Reference (checked against the public rev. C15 and
/// D5 PDFs from ftp.elecraft.com) defines `AR` (main RX antenna)
/// as 1 = RX ANT IN2, 2 = RX uses the TX antenna, 4 = RX ANT IN1,
/// 5/6/7 = ATU (KAT4) ANT1/2/3, and `AN` as the TX antenna 1..=3 (also the
/// RX input when `AR2`). ANT2/ANT3 exist only with a KAT4.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum K4Port {
    RxAntIn1,
    RxAntIn2,
    Ant1,
    Ant2,
    Ant3,
}

impl K4Port {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RxAntIn1 => "rx-ant-in1",
            Self::RxAntIn2 => "rx-ant-in2",
            Self::Ant1 => "ant1",
            Self::Ant2 => "ant2",
            Self::Ant3 => "ant3",
        }
    }

    /// The `AR` value that routes this port to the main receiver without
    /// touching the TX antenna (`AN` is never set).
    pub fn ar_value(self) -> u8 {
        match self {
            Self::RxAntIn1 => 4,
            Self::RxAntIn2 => 1,
            Self::Ant1 => 5,
            Self::Ant2 => 6,
            Self::Ant3 => 7,
        }
    }

    /// Resolves the main-RX input from raw `AR` and `AN` readbacks. `None`
    /// for disconnected/transverter inputs or unreadable values.
    pub fn from_readback(ar: Option<&str>, an: Option<&str>) -> Option<Self> {
        let tx = || match an?.trim().parse::<u8>().ok()? {
            1 => Some(Self::Ant1),
            2 => Some(Self::Ant2),
            3 => Some(Self::Ant3),
            _ => None,
        };
        match ar?.trim().parse::<u8>().ok()? {
            1 => Some(Self::RxAntIn2),
            2 => tx(),
            4 => Some(Self::RxAntIn1),
            5 => Some(Self::Ant1),
            6 => Some(Self::Ant2),
            7 => Some(Self::Ant3),
            _ => None,
        }
    }
}

impl fmt::Display for K4Port {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
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
    /// Where it is connected; visits are only attributed to this antenna
    /// when `AR`/`AN` readback resolves to this port.
    pub port: K4Port,
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

/// Keep every accepted frame from the start of the run until `duration`
/// has passed or `max_packet_bytes` of original packets are kept, then
/// fall back to `raw_sample_interval_ms`. Packet bytes bound disk use:
/// stored frames are compressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawCapture {
    pub duration: std::time::Duration,
    pub max_packet_bytes: u64,
}

/// Longest a single every-frame capture may run.
pub const RAW_CAPTURE_MAX_DURATION: std::time::Duration = std::time::Duration::from_secs(24 * 3600);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct CollectionConfig {
    pub mode: Mode,
    /// TOML half of the two-part retune consent (CLI `--allow-retune` is the other).
    #[serde(default)]
    pub allow_retune: bool,
    /// Survey profile file, relative to this station file.
    #[serde(default)]
    pub survey_profile: Option<PathBuf>,
    /// Permit the survey to select the profile's antennas with `AR` (main
    /// RX input only; the TX antenna is never changed). Off by default: a
    /// band whose RX input is not the wanted antenna is then skipped.
    #[serde(default)]
    pub allow_rx_antenna_switch: bool,
    /// Permit a paired survey to use the sub receiver: turn it on (`SB1`),
    /// set VFO B and the sub pan span, route its RX input with `AR$` and
    /// apply the profile's receiver settings to it, all journaled and put
    /// back. Needs a KRX4 (docs/paired.md). Off by default.
    #[serde(default)]
    pub allow_sub_receiver: bool,
    /// Permit the survey to set `#FXT0` (track) if the pan is fixed. Off by
    /// default: a fixed pan makes the survey refuse to start instead.
    #[serde(default)]
    pub set_pan_track_mode: bool,
    #[serde(default = "default_usable_fraction")]
    pub usable_fraction: f64,
    #[serde(default = "yes")]
    pub pause_on_transmit: bool,
    #[serde(default = "yes")]
    pub pause_on_external_change: bool,
    #[serde(default = "yes")]
    pub manual_resume_on_conflict: bool,
    /// At most one original packet per interval per pan; 0 disables raw sampling.
    #[serde(default = "default_raw_interval_ms")]
    pub raw_sample_interval_ms: u64,
    /// Temporary every-frame raw capture. Command line only
    /// (`--raw-full-rate`); a config file cannot set it.
    #[serde(skip)]
    pub raw_capture: Option<RawCapture>,
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
    /// Survey recovery journals (docs/recovery.md). Default
    /// `<root>/recovery`. Never pruned and not counted against
    /// `max_total_bytes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery_dir: Option<PathBuf>,
}

impl StorageConfig {
    pub fn recovery_dir(&self) -> PathBuf {
        self.recovery_dir.clone().unwrap_or_else(|| self.root.join("recovery"))
    }
}

/// What to survey and how often. Lives in its own file so alternative
/// surveys can be kept side by side.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SurveyProfile {
    pub id: String,
    #[serde(default)]
    pub description: String,
    /// Antenna ids from the station registry. With more than one, each band
    /// is surveyed on every antenna in turn (needs
    /// `collection.allow_rx_antenna_switch`).
    pub antennas: Vec<String>,
    #[serde(default = "default_settle_ms")]
    pub settle_ms: u64,
    #[serde(default = "default_dwell_ms")]
    pub dwell_ms: u64,
    /// Target start-to-start period of a full cycle. A faster cycle waits;
    /// a slower one starts the next immediately. 0 = back-to-back.
    #[serde(default = "default_min_cycle_ms")]
    pub min_cycle_ms: u64,
    /// `#SPN` request; it is NOT the sampled packet width.
    #[serde(default)]
    pub requested_pan_span_hz: Option<u32>,
    pub ranges: Vec<RangeConfig>,
    /// Receiver settings applied and verified before measuring, with band,
    /// antenna and antenna+band overrides. See docs/measurement.md.
    #[serde(default)]
    pub measurement: MeasurementConfig,
    /// Measure exactly two antennas at once: the main receiver (pan A) on
    /// one and the sub receiver (pan B) on the other, on the same tile.
    /// Needs `collection.allow_rx_antenna_switch` and
    /// `collection.allow_sub_receiver`. See docs/paired.md.
    #[serde(default)]
    pub paired: bool,
    /// With `paired`, exchange the receivers' antennas every other cycle so
    /// the main/sub difference can be separated from the antenna
    /// difference.
    #[serde(default = "yes")]
    pub swap: bool,
}

impl SurveyProfile {
    /// K4 bands (`None` = general coverage) the ranges touch.
    pub fn k4_bands(&self) -> Vec<Option<u8>> {
        let mut out = std::collections::BTreeSet::new();
        for r in &self.ranges {
            out.insert(k4_band_number(r.low_hz));
            out.insert(k4_band_number(r.high_hz.saturating_sub(1)));
        }
        out.into_iter().collect()
    }
}

/// A named frequency range to survey: a whole band or part of one. The id
/// is stored with each visit as its `band_id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RangeConfig {
    pub id: String,
    pub low_hz: u64,
    pub high_hz: u64,
    #[serde(default = "default_weight")]
    pub weight: u32,
}

impl RangeConfig {
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
    5000
}
fn default_min_cycle_ms() -> u64 {
    300_000
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

fn read(path: &Path) -> Result<String, ConfigError> {
    std::fs::read_to_string(path).map_err(|e| ConfigError {
        problems: vec![format!("cannot read {}: {e}", path.display())],
    })
}

impl SurveyProfile {
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|e| ConfigError {
            problems: vec![format!("survey profile: {e}")],
        })
    }
}

impl Config {
    /// Checks [`CollectionConfig::raw_capture`], which the command line sets
    /// after the file is validated. A capture must end within a day, before
    /// raw retention could prune its start, and in at most half the store.
    pub fn check_raw_capture(&self) -> Result<(), String> {
        let Some(c) = self.collection.raw_capture else {
            return Ok(());
        };
        if c.duration.is_zero() || c.duration > RAW_CAPTURE_MAX_DURATION {
            return Err("a full-rate raw capture must last more than 0 and at most 24 hours".into());
        }
        if c.duration.as_secs() > self.storage.raw_retention_hours.saturating_mul(3600) {
            return Err(format!(
                "a {} s full-rate raw capture outlasts storage.raw_retention_hours = {}; retention would prune its start",
                c.duration.as_secs(),
                self.storage.raw_retention_hours
            ));
        }
        let half = self.storage.max_total_bytes / 2;
        if c.max_packet_bytes == 0 || c.max_packet_bytes > half {
            return Err(format!(
                "a full-rate raw capture may keep 1 to {half} packet bytes (half of storage.max_total_bytes), not {}",
                c.max_packet_bytes
            ));
        }
        Ok(())
    }

    /// Parses a station file only; `collection.survey_profile` is NOT loaded
    /// (use [`Config::load`] or [`Config::from_parts`]).
    pub fn from_toml(text: &str) -> Result<Self, ConfigError> {
        Self::from_parts(text, None)
    }

    /// Parses a station file and an optional survey profile, then validates
    /// them together.
    pub fn from_parts(station: &str, profile: Option<&str>) -> Result<Self, ConfigError> {
        let mut config: Config = toml::from_str(station).map_err(|e| ConfigError {
            problems: vec![e.to_string()],
        })?;
        config.profile = profile.map(SurveyProfile::from_toml).transpose()?;
        config.validate()?;
        Ok(config)
    }

    /// Loads a station file and its survey profile: `profile` if given
    /// (relative to the current directory), else `collection.survey_profile`
    /// (relative to the station file). Relative storage roots are resolved
    /// against the current directory.
    pub fn load(path: &Path, profile: Option<&Path>) -> Result<Self, ConfigError> {
        let station = read(path)?;
        let parsed: Config = toml::from_str(&station).map_err(|e| ConfigError {
            problems: vec![e.to_string()],
        })?;
        let profile_path = match profile {
            Some(p) => Some(p.to_path_buf()),
            None => parsed
                .collection
                .survey_profile
                .as_ref()
                .map(|p| path.parent().unwrap_or(Path::new(".")).join(p)),
        };
        let profile_text = profile_path.as_deref().map(read).transpose()?;
        Self::from_parts(&station, profile_text.as_deref()).map_err(|mut e| {
            if let Some(p) = &profile_path {
                e.problems.push(format!("(survey profile: {})", p.display()));
            }
            e
        })
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
        if self.antennas.is_empty() {
            p.push("at least one [[antennas]] entry is required".into());
        }
        let mut antenna_ids = std::collections::BTreeSet::new();
        for a in &self.antennas {
            if a.id.trim().is_empty() || a.id.len() > 32 {
                p.push("antenna ids must be 1..=32 characters".into());
            }
            if !antenna_ids.insert(a.id.as_str()) {
                p.push(format!("duplicate antenna id {:?}", a.id));
            }
        }

        let c = &self.collection;
        if !(0.1..=1.0).contains(&c.usable_fraction) {
            p.push("collection.usable_fraction must be within 0.1..=1.0".into());
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
            if self.profile.is_none() {
                p.push("survey mode needs a survey profile (collection.survey_profile or --profile)".into());
            }
        }
        if let Some(profile) = &self.profile {
            self.validate_profile(profile, &mut p);
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

        if p.is_empty() {
            Ok(())
        } else {
            Err(ConfigError { problems: p })
        }
    }

    fn validate_profile(&self, s: &SurveyProfile, p: &mut Vec<String>) {
        if s.id.trim().is_empty() || s.id.len() > 64 {
            p.push("profile id must be 1..=64 characters".into());
        }
        if s.antennas.is_empty() {
            p.push("profile antennas must list at least one antenna id".into());
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut ports = std::collections::BTreeMap::new();
        for id in &s.antennas {
            if !seen.insert(id.as_str()) {
                p.push(format!("profile lists antenna {id:?} twice"));
            }
            match self.antenna(id) {
                None => p.push(format!(
                    "profile antenna {id:?} is not in the station [[antennas]] registry"
                )),
                Some(a) => {
                    if let Some(other) = ports.insert(a.port, id.as_str()) {
                        p.push(format!(
                            "profile antennas {other:?} and {id:?} share port {} and cannot be told apart",
                            a.port
                        ));
                    }
                }
            }
        }
        if s.antennas.len() > 1 && !self.collection.allow_rx_antenna_switch {
            p.push(format!(
                "profile surveys {} antennas, which needs collection.allow_rx_antenna_switch = true",
                s.antennas.len()
            ));
        }
        if s.paired {
            if s.antennas.len() != 2 {
                p.push(format!(
                    "a paired profile measures exactly two antennas at once; it lists {}",
                    s.antennas.len()
                ));
            }
            if !self.collection.allow_sub_receiver {
                p.push(
                    "a paired profile uses the sub receiver, which needs collection.allow_sub_receiver = true".into(),
                );
            }
        }
        if let Some(span) = s.requested_pan_span_hz
            && !K4_SPAN_RANGE.contains(&span)
        {
            p.push(format!(
                "profile requested_pan_span_hz {span} outside K4 #SPN range 6000..=368000"
            ));
        }
        if s.dwell_ms < 200 {
            p.push("profile dwell_ms must be >= 200".into());
        }
        if s.settle_ms < 100 {
            p.push("profile settle_ms must be >= 100".into());
        }
        if s.min_cycle_ms > 86_400_000 {
            p.push("profile min_cycle_ms must be <= 86400000 (one day)".into());
        }
        if s.ranges.is_empty() {
            p.push("profile needs at least one [[ranges]] entry".into());
        }
        let mut ids = std::collections::BTreeSet::new();
        for b in &s.ranges {
            if !ids.insert(b.id.as_str()) {
                p.push(format!("duplicate range id {:?}", b.id));
            }
            if b.id.trim().is_empty() || b.id.len() > 32 {
                p.push("range ids must be 1..=32 characters".into());
            }
            if b.low_hz >= b.high_hz {
                p.push(format!("range {}: low_hz must be < high_hz", b.id));
            }
            if b.low_hz < K4_MIN_HZ || b.high_hz > K4_MAX_HZ {
                p.push(format!("range {}: interval must lie within 100 kHz..54 MHz", b.id));
            }
            if b.weight == 0 || b.weight > 10 {
                p.push(format!("range {}: weight must be within 1..=10", b.id));
            }
        }
        p.extend(s.measurement.problems(&s.antennas, &s.k4_bands()));
        let mut sorted: Vec<_> = s.ranges.iter().collect();
        sorted.sort_by_key(|b| b.low_hz);
        for w in sorted.windows(2) {
            if w[0].high_hz > w[1].low_hz {
                p.push(format!("ranges {} and {} overlap", w[0].id, w[1].id));
            }
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

    /// Survey ranges from the loaded profile (empty without one).
    pub fn ranges(&self) -> &[RangeConfig] {
        self.profile.as_ref().map_or(&[], |p| p.ranges.as_slice())
    }

    /// The profile range containing `hz`, if any.
    pub fn range_containing(&self, hz: u64) -> Option<&RangeConfig> {
        self.ranges().iter().find(|b| b.range().contains(hz))
    }

    pub fn antenna(&self, id: &str) -> Option<&AntennaConfig> {
        self.antennas.iter().find(|a| a.id == id)
    }

    /// The profile's antennas in survey order.
    pub fn survey_antennas(&self) -> Vec<&AntennaConfig> {
        self.profile
            .iter()
            .flat_map(|p| p.antennas.iter())
            .filter_map(|id| self.antenna(id))
            .collect()
    }

    /// The antenna passive capture attributes data to: the profile's only
    /// antenna, or the registry's only antenna without a profile.
    pub fn passive_antenna(&self) -> Result<&AntennaConfig, String> {
        let candidates = match &self.profile {
            Some(_) => self.survey_antennas(),
            None => self.antennas.iter().collect(),
        };
        match candidates.as_slice() {
            [one] => Ok(one),
            _ => Err(format!(
                "passive capture needs exactly one antenna, found {}: use a profile listing one antenna",
                candidates.len()
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) const EXAMPLE: &str = include_str!("../../../config/example.toml");
    pub(crate) const HF_FULL: &str = include_str!("../../../config/surveys/hf-full.toml");
    const HF_CW: &str = include_str!("../../../config/surveys/hf-cw-us.toml");
    const RX_VS_DXC: &str = include_str!("../../../config/surveys/rx-vs-dxc.toml");
    const RX_VS_DXC_PAIRED: &str = include_str!("../../../config/surveys/rx-vs-dxc-paired.toml");

    fn example() -> Config {
        Config::from_parts(EXAMPLE, Some(HF_FULL)).expect("example config")
    }

    #[test]
    fn example_config_is_valid() {
        let c = example();
        assert_eq!(c.ranges().len(), 8);
        assert_eq!(c.collection.mode, Mode::Survey);
        assert_eq!(c.storage.max_total_bytes, 4 * 1024 * 1024 * 1024);
        assert_eq!(c.radio.receiver, Receiver::Main);
        let p = c.profile.as_ref().unwrap();
        assert_eq!((p.dwell_ms, p.min_cycle_ms), (5000, 300_000));
        assert_eq!(c.passive_antenna().unwrap().port, K4Port::RxAntIn1);
    }

    #[test]
    fn example_profiles_are_valid() {
        let cw = Config::from_parts(EXAMPLE, Some(HF_CW)).expect("cw profile");
        assert_eq!(cw.range_containing(7_050_000).map(|r| r.id.as_str()), Some("40m-cw"));
        assert!(cw.range_containing(7_200_000).is_none());
        // Two antennas need the switching opt-in.
        let err = Config::from_parts(EXAMPLE, Some(RX_VS_DXC)).unwrap_err();
        assert!(err.to_string().contains("allow_rx_antenna_switch"), "{err}");
        let station = EXAMPLE.replace("allow_rx_antenna_switch = false", "allow_rx_antenna_switch = true");
        let both = Config::from_parts(&station, Some(RX_VS_DXC)).expect("two antennas");
        let ports: Vec<_> = both.survey_antennas().iter().map(|a| a.port).collect();
        assert_eq!(ports, [K4Port::RxAntIn1, K4Port::Ant2]);
        assert!(both.passive_antenna().is_err());
        // A paired profile also needs the sub-receiver opt-in.
        let err = Config::from_parts(&station, Some(RX_VS_DXC_PAIRED)).unwrap_err();
        assert!(err.to_string().contains("allow_sub_receiver"), "{err}");
        let station = station.replace("allow_sub_receiver = false", "allow_sub_receiver = true");
        let paired = Config::from_parts(&station, Some(RX_VS_DXC_PAIRED)).expect("paired");
        let p = paired.profile.as_ref().unwrap();
        assert!(p.paired && p.swap);
        assert_eq!(p.antennas, ["log-near-house", "dxc"]);
    }

    #[test]
    fn raw_captures_are_bounded_by_a_day_retention_and_half_the_store() {
        let mut c = example();
        assert!(c.check_raw_capture().is_ok());
        let hour = std::time::Duration::from_secs(3600);
        let ok = RawCapture {
            duration: 2 * hour,
            max_packet_bytes: 1 << 30,
        };
        c.collection.raw_capture = Some(ok);
        c.check_raw_capture().unwrap();
        for bad in [
            RawCapture {
                duration: std::time::Duration::ZERO,
                ..ok
            },
            RawCapture {
                duration: 25 * hour,
                ..ok
            },
            RawCapture {
                max_packet_bytes: 0,
                ..ok
            },
            RawCapture {
                max_packet_bytes: c.storage.max_total_bytes / 2 + 1,
                ..ok
            },
        ] {
            c.collection.raw_capture = Some(bad);
            assert!(c.check_raw_capture().is_err(), "{bad:?}");
        }
        c.collection.raw_capture = Some(ok);
        c.storage.raw_retention_hours = 1;
        assert!(c.check_raw_capture().unwrap_err().contains("raw_retention_hours"));
        // A config file cannot ask for one.
        let text = format!("{}\n", include_str!("../../../config/example.toml"))
            .replace("[collection]\n", "[collection]\nraw_capture = true\n");
        assert!(Config::from_toml(&text).is_err());
    }

    #[test]
    fn survey_mode_needs_a_profile() {
        let err = Config::from_toml(EXAMPLE).unwrap_err();
        assert!(err.to_string().contains("survey profile"), "{err}");
        let passive = EXAMPLE.replace("mode = \"survey\"", "mode = \"passive\"");
        assert!(Config::from_toml(&passive).is_ok());
    }

    #[test]
    fn profile_antennas_must_be_registered() {
        let bad = HF_FULL.replace("antennas = [\"log-near-house\"]", "antennas = [\"nope\"]");
        let err = Config::from_parts(EXAMPLE, Some(&bad)).unwrap_err();
        assert!(err.to_string().contains("registry"), "{err}");
    }

    #[test]
    fn load_resolves_the_profile_relative_to_the_station_file() {
        let dir = std::env::temp_dir().join(format!("k4na-config-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("surveys")).unwrap();
        std::fs::write(dir.join("station.toml"), EXAMPLE).unwrap();
        std::fs::write(dir.join("surveys/hf-full.toml"), HF_FULL).unwrap();
        std::fs::write(dir.join("cw.toml"), HF_CW).unwrap();
        let c = Config::load(&dir.join("station.toml"), None).unwrap();
        assert_eq!(c.profile.as_ref().unwrap().id, "hf-full");
        let c = Config::load(&dir.join("station.toml"), Some(&dir.join("cw.toml"))).unwrap();
        assert_eq!(c.profile.as_ref().unwrap().id, "hf-cw-us");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ports_resolve_from_ar_and_an() {
        use K4Port::*;
        assert_eq!(K4Port::from_readback(Some("4"), Some("1")), Some(RxAntIn1));
        assert_eq!(K4Port::from_readback(Some("1"), None), Some(RxAntIn2));
        assert_eq!(K4Port::from_readback(Some("6"), Some("1")), Some(Ant2));
        assert_eq!(K4Port::from_readback(Some("2"), Some("2")), Some(Ant2));
        assert_eq!(K4Port::from_readback(Some("2"), None), None);
        assert_eq!(K4Port::from_readback(Some("0"), Some("1")), None);
        assert_eq!(K4Port::from_readback(Some("3"), Some("1")), None);
        for port in [RxAntIn1, RxAntIn2, Ant1, Ant2, Ant3] {
            let ar = port.ar_value().to_string();
            assert_eq!(K4Port::from_readback(Some(&ar), Some("1")), Some(port));
        }
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let text = EXAMPLE.replace("[radio]", "[radio]\ncat_script = \"TX;\"");
        let err = Config::from_parts(&text, Some(HF_FULL)).unwrap_err();
        assert!(err.to_string().contains("cat_script"), "{err}");
        let err = Config::from_parts(EXAMPLE, Some(&format!("cat_script = 1\n{HF_FULL}"))).unwrap_err();
        assert!(err.to_string().contains("cat_script"), "{err}");
    }

    #[test]
    fn disabling_interlocks_is_rejected() {
        let text = EXAMPLE.replace("pause_on_transmit = true", "pause_on_transmit = false");
        let err = Config::from_parts(&text, Some(HF_FULL)).unwrap_err();
        assert!(err.to_string().contains("pause_on_transmit"));
    }

    #[test]
    fn overlapping_ranges_are_rejected() {
        let text = HF_FULL.replace("high_hz = 7300000", "high_hz = 10120000");
        let err = Config::from_parts(EXAMPLE, Some(&text)).unwrap_err();
        assert!(err.to_string().contains("overlap"), "{err}");
    }

    #[test]
    fn digest_ignores_host_and_covers_the_profile() {
        let a = example();
        let b = Config::from_parts(&EXAMPLE.replace("k4.local", "192.0.2.10"), Some(HF_FULL)).expect("b");
        assert_eq!(a.digest(), b.digest());
        assert_eq!(b.redacted().radio.host, "<redacted>");
        let c = Config::from_parts(EXAMPLE, Some(&HF_FULL.replace("dwell_ms = 5000", "dwell_ms = 4000"))).unwrap();
        assert_ne!(a.digest(), c.digest());
    }
}
