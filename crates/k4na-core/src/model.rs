//! Records passed from acquisition to storage. Field meanings are part of
//! the on-disk contract documented in docs/storage.md.

use std::collections::BTreeMap;

use k4na_protocol::PanId;
use serde::{Deserialize, Serialize};

use crate::aggregate::VisitStats;
use crate::config::Mode;
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
    pub antenna_id: String,
    pub antenna_revision: String,
    pub antenna_description: String,
    pub antenna_notes: String,
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
    pub radio_state: RadioState,
    pub stats: Option<VisitStats>,
}

impl VisitRecord {
    pub fn accepted_frames(&self) -> u32 {
        self.stats.as_ref().map_or(0, |s| s.frame_count)
    }

    /// Comparability segment key: antenna, pan, mapping and the readback
    /// subset that affects spectra. Equal keys may be compared directly.
    pub fn segment_key(&self, session: &SessionInfo) -> String {
        let mut parts = vec![
            format!("antenna={}", session.antenna_id),
            format!("antenna_revision={}", session.antenna_revision),
            format!("pan={}", self.pan),
            format!("mapping={}", session.mapping_version),
            format!("decoder={}", session.decoder_version),
            format!("rate={}", self.geometry.sample_rate_hz),
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
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
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
