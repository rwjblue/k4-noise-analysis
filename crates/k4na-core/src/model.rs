//! Records passed from acquisition to storage. Field meanings are part of
//! the on-disk contract documented in docs/storage.md.

use std::collections::BTreeMap;

use k4na_protocol::PanId;
use serde::{Deserialize, Serialize};

use crate::aggregate::VisitStats;
use crate::config::{AntennaConfig, K4Port, Mode};
use crate::time::Timestamp;
use crate::units::{BinGeometry, HzRange};

/// Declared measurement units for every stored level.
pub const UNITS: &str = "k4-pan-tenth-db";
/// Calibration status for every stored level.
pub const CALIBRATION: &str = "uncalibrated: native K4 LAN pan units, not dBm";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VisitStatus {
    /// Planned dwell or window completed with stable geometry and state.
    Complete,
    /// Ended early or with too few frames; usable with caution.
    Partial,
    /// Invalidated (conflict, coverage failure); stored for gap accounting.
    Aborted,
}

impl VisitStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Aborted => "aborted",
        }
    }
}

/// CAT readbacks captured for a visit. Values are raw response parameters;
/// a missing key or `"unknown"` means the state could not be read.
pub type RadioState = BTreeMap<String, String>;

/// Readback keys that do not affect comparability of spectra (they change
/// on every retune by design).
pub const NON_COMPARABILITY_KEYS: &[&str] = &["FA", "FB", "BN", "TQ"];

/// Delivered panadapter frame rate of one visit, rounded to whole frames
/// per second: `(accepted - 1) / (last - first)` on the monotonic clock.
///
/// The K4 may deliver fewer frames than `#FPS` requests (observed: 15/s
/// while a second client was connected, 30/s otherwise, with `#FPS;`
/// lagging the change), and the rate changes how many frames each visit's
/// percentiles summarize, so it is measured rather than read back. One
/// dropped frame in a 5 s dwell moves the estimate by under 1%.
/// `None` with fewer than two frames.
pub fn delivered_frame_rate(accepted: u64, first_mono_ns: Option<u64>, last_mono_ns: Option<u64>) -> Option<u32> {
    let (first, last) = (first_mono_ns?, last_mono_ns?);
    if accepted < 2 || last <= first {
        return None;
    }
    let fps = (accepted - 1) as f64 * 1e9 / (last - first) as f64;
    Some(fps.round() as u32)
}

/// Identity of the session that produced a record.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub mode: Mode,
    pub started: Timestamp,
    pub program_version: String,
    pub decoder_version: String,
    pub mapping_version: String,
    pub config_digest: String,
    /// Redacted configuration TOML (no host).
    pub config_toml: String,
    /// Survey profile id, if one was loaded.
    pub profile_id: Option<String>,
    /// Registry entries this session may attribute visits to. Each visit
    /// records its own antenna.
    pub antennas: Vec<AntennaConfig>,
    pub receiver: String,
    /// Firmware revisions from `RV`, if read.
    pub firmware: Vec<String>,
    /// Pre-survey snapshot of collector-modifiable state (for operator
    /// recovery after an abnormal exit; never auto-restored).
    pub snapshot: RadioState,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TileRef {
    pub index: usize,
    pub id: String,
    pub target: HzRange,
}

/// One finalized visit (dwell or passive window) on one pan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VisitRecord {
    pub visit_id: String,
    pub session_id: String,
    pub pan: PanId,
    pub band_id: Option<String>,
    pub tile: Option<TileRef>,
    pub requested_vfo_hz: Option<u64>,
    pub requested_span_hz: Option<u32>,
    pub geometry: BinGeometry,
    pub usable_fraction: f64,
    pub full: HzRange,
    pub usable: HzRange,
    /// Visit start, including retune/settle time.
    pub started: Timestamp,
    pub first_frame: Option<Timestamp>,
    pub last_frame: Option<Timestamp>,
    pub ended: Timestamp,
    pub settle_ms: u64,
    pub settle_discarded_frames: u32,
    pub rejected_frames: u32,
    pub status: VisitStatus,
    pub flags: Vec<String>,
    pub end_reason: String,
    pub antenna: VisitAntenna,
    /// Verified readbacks during the visit (raw K4 values by prefix).
    pub radio_state: RadioState,
    /// Profile measurement settings in effect: each field `as-found` or the
    /// configured value, plus `cat_meta_mode`. Empty for passive visits.
    /// The verified values themselves are in `radio_state`.
    #[serde(default)]
    pub measurement: BTreeMap<String, String>,
    pub stats: Option<VisitStats>,
}

/// The antenna a visit measured and how that was established.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VisitAntenna {
    /// Registry id, or `unregistered` when readback showed a port no
    /// registry entry claims.
    pub id: String,
    pub revision: String,
    /// The port the registry places this antenna on.
    pub port: K4Port,
    /// The main-RX input resolved from this visit's `AR`/`AN` readback;
    /// `None` when it was not read (the attribution is then unverified).
    pub observed_port: Option<K4Port>,
}

impl VisitAntenna {
    pub fn configured(a: &AntennaConfig, observed_port: Option<K4Port>) -> Self {
        Self {
            id: a.id.clone(),
            revision: a.revision.clone(),
            port: a.port,
            observed_port,
        }
    }

    pub fn verified(&self) -> bool {
        self.observed_port == Some(self.port)
    }
}

impl VisitRecord {
    pub fn accepted_frames(&self) -> u32 {
        self.stats.as_ref().map_or(0, |s| s.frame_count)
    }

    /// Comparability segment key: band, antenna, pan, mapping, delivered
    /// frame rate and the verified readbacks (including receiver
    /// measurement settings) that affect spectra. Equal keys may be
    /// compared directly.
    pub fn segment_key(&self, session: &SessionInfo) -> String {
        let mut parts = vec![
            format!("band={}", self.band_id.as_deref().unwrap_or("none")),
            format!("antenna={}", self.antenna.id),
            format!("antenna_revision={}", self.antenna.revision),
            format!(
                "antenna_port={}",
                self.antenna.observed_port.map_or("unverified", K4Port::as_str)
            ),
            format!("pan={}", self.pan),
            format!("mapping={}", session.mapping_version),
            format!("decoder={}", session.decoder_version),
            format!("rate={}", self.geometry.sample_rate_hz),
            format!(
                "frame_rate={}",
                delivered_frame_rate(
                    self.accepted_frames().into(),
                    self.first_frame.map(|t| t.mono_ns),
                    self.last_frame.map(|t| t.mono_ns),
                )
                .map_or_else(|| "unknown".to_owned(), |f| f.to_string())
            ),
        ];
        for (k, v) in &self.radio_state {
            if !NON_COMPARABILITY_KEYS.contains(&k.as_str()) {
                parts.push(format!("{k}={v}"));
            }
        }
        parts.join(";")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SessionStart,
    SessionEnd,
    SpectrumConnected,
    SpectrumDisconnected,
    SpectrumStalled,
    CatConnected,
    CatUnavailable,
    /// Interval without accepted coverage; `start`/`end` bound it.
    Gap,
    ClockJump,
    DecoderRejects,
    RawSamplingPaused,
    RawSamplingResumed,
    RawFramesDropped,
    Pruned,
    StorageSuspended,
    TransmitDetected,
    ExternalChange,
    ReadbackMismatch,
    CoverageUnverified,
    PlanBuilt,
    PlanRebuilt,
    StateChanged,
    RetuneCorrection,
    RestoreDone,
    RestoreSkipped,
    ShardRecovered,
    Warning,
    /// The survey selected (or put back) a main-RX input with `AR`.
    AntennaSelected,
    /// The main-RX input is not the wanted antenna and was not changed.
    AntennaMismatch,
    /// A full survey cycle finished; `count` is the cycle number.
    CycleComplete,
    /// Profile measurement settings were applied and verified.
    MeasurementApplied,
    /// A context could not be measured as configured; its tiles were
    /// skipped without changing anything.
    MeasurementSkipped,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AntennaSelected => "antenna_selected",
            Self::AntennaMismatch => "antenna_mismatch",
            Self::CycleComplete => "cycle_complete",
            Self::MeasurementApplied => "measurement_applied",
            Self::MeasurementSkipped => "measurement_skipped",
            Self::SessionStart => "session_start",
            Self::SessionEnd => "session_end",
            Self::SpectrumConnected => "spectrum_connected",
            Self::SpectrumDisconnected => "spectrum_disconnected",
            Self::SpectrumStalled => "spectrum_stalled",
            Self::CatConnected => "cat_connected",
            Self::CatUnavailable => "cat_unavailable",
            Self::Gap => "gap",
            Self::ClockJump => "clock_jump",
            Self::DecoderRejects => "decoder_rejects",
            Self::RawSamplingPaused => "raw_sampling_paused",
            Self::RawSamplingResumed => "raw_sampling_resumed",
            Self::RawFramesDropped => "raw_frames_dropped",
            Self::Pruned => "pruned",
            Self::StorageSuspended => "storage_suspended",
            Self::TransmitDetected => "transmit_detected",
            Self::ExternalChange => "external_change",
            Self::ReadbackMismatch => "readback_mismatch",
            Self::CoverageUnverified => "coverage_unverified",
            Self::PlanBuilt => "plan_built",
            Self::PlanRebuilt => "plan_rebuilt",
            Self::StateChanged => "state_changed",
            Self::RetuneCorrection => "retune_correction",
            Self::RestoreDone => "restore_done",
            Self::RestoreSkipped => "restore_skipped",
            Self::ShardRecovered => "shard_recovered",
            Self::Warning => "warning",
        }
    }
}

/// Operational event, including explicit gaps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub session_id: String,
    pub at: Timestamp,
    pub kind: EventKind,
    /// For intervals (gaps, disconnects): start and end UTC ns.
    pub start_utc_ns: Option<i64>,
    pub end_utc_ns: Option<i64>,
    pub band_id: Option<String>,
    pub tile_id: Option<String>,
    pub count: Option<u64>,
    pub message: String,
}

impl Event {
    pub fn new(session_id: &str, at: Timestamp, kind: EventKind, message: impl Into<String>) -> Self {
        Self {
            session_id: session_id.to_owned(),
            at,
            kind,
            start_utc_ns: None,
            end_utc_ns: None,
            band_id: None,
            tile_id: None,
            count: None,
            message: message.into(),
        }
    }

    pub fn interval(mut self, start_utc_ns: i64, end_utc_ns: i64) -> Self {
        self.start_utc_ns = Some(start_utc_ns);
        self.end_utc_ns = Some(end_utc_ns);
        self
    }

    pub fn count(mut self, n: u64) -> Self {
        self.count = Some(n);
        self
    }

    pub fn band(mut self, band: Option<&str>, tile: Option<&str>) -> Self {
        self.band_id = band.map(str::to_owned);
        self.tile_id = tile.map(str::to_owned);
        self
    }
}

/// One sampled, checksum-valid original wire frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFrameRecord {
    pub session_id: String,
    pub visit_id: Option<String>,
    pub received: Timestamp,
    pub pan: PanId,
    pub sequence: u8,
    pub center_hz: u64,
    pub sample_rate_hz: u32,
    pub sampling_policy: String,
    pub bytes: Vec<u8>,
}

#[cfg(test)]
mod tests {
    use super::delivered_frame_rate;

    const S: u64 = 1_000_000_000;

    #[test]
    fn delivered_frame_rate_rounds_and_tolerates_a_dropped_frame() {
        // As observed on a K4D: 151 frames over ~4.93 s, 75 over ~4.9 s.
        assert_eq!(delivered_frame_rate(151, Some(0), Some(4_934_000_000)), Some(30));
        assert_eq!(delivered_frame_rate(75, Some(0), Some(4_900_000_000)), Some(15));
        assert_eq!(delivered_frame_rate(150, Some(0), Some(4_934_000_000)), Some(30));
        assert_eq!(delivered_frame_rate(1, Some(0), Some(S)), None);
        assert_eq!(delivered_frame_rate(10, Some(S), Some(S)), None);
        assert_eq!(delivered_frame_rate(10, None, Some(S)), None);
    }
}
