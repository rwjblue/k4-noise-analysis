//! Building one visit: accepted frames, raw sampling and final record.

use std::time::Duration;

use k4na_core::aggregate::{PushOutcome, VisitAccumulator};
use k4na_core::config::RawCapture;
use k4na_core::model::{RadioState, RawFrameRecord, TileRef, VisitAntenna, VisitRecord, VisitStatus};
use k4na_core::time::Timestamp;
use k4na_core::units::BinGeometry;
use k4na_protocol::PanId;

use crate::spectrum::ReceivedFrame;

pub fn new_id() -> String {
    uuid::Uuid::now_v7().to_string()
}

/// Deterministic raw sampling: the first accepted frame on a pan, then the
/// first accepted frame at least `interval` after the previous sample.
/// An optional [`RawCapture`] first keeps every accepted frame until its
/// duration (from the first accepted frame) or packet-byte cap is reached.
#[derive(Debug, Clone)]
pub struct RawSampler {
    interval: Option<Duration>,
    last_mono_ns: Option<u64>,
    capture: Option<Capture>,
    interval_policy: String,
    notice: Option<String>,
}

#[derive(Debug, Clone)]
struct Capture {
    limits: RawCapture,
    until_mono_ns: Option<u64>,
    kept_bytes: u64,
    policy: String,
}

impl RawSampler {
    pub fn new(interval_ms: u64) -> Self {
        Self::with_capture(interval_ms, None)
    }

    pub fn with_capture(interval_ms: u64, capture: Option<RawCapture>) -> Self {
        let interval = (interval_ms > 0).then(|| Duration::from_millis(interval_ms));
        let interval_policy = match interval {
            Some(i) => format!(
                "first accepted frame then >= {} ms apart per pan; deterministic; expendable under storage pressure",
                i.as_millis()
            ),
            None => "disabled".into(),
        };
        let capture = capture.map(|limits| Capture {
            policy: format!(
                "every accepted frame (full-rate capture for {} s or {} packet bytes); expendable under storage pressure",
                limits.duration.as_secs(),
                limits.max_packet_bytes
            ),
            limits,
            until_mono_ns: None,
            kept_bytes: 0,
        });
        Self {
            interval,
            last_mono_ns: None,
            capture,
            interval_policy,
            notice: None,
        }
    }

    /// The policy that kept the most recent sample (stored with each frame).
    pub fn policy(&self) -> &str {
        self.capture.as_ref().map_or(&self.interval_policy, |c| &c.policy)
    }

    /// A capture start or end message, once, for the session's event log.
    pub fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    pub fn due(&mut self, at: Timestamp, packet_bytes: usize) -> bool {
        if let Some(c) = self.capture.as_mut() {
            let until = *c.until_mono_ns.get_or_insert_with(|| {
                self.notice = Some(format!("raw full-rate capture started: {}", c.policy));
                at.mono_ns.saturating_add(c.limits.duration.as_nanos() as u64)
            });
            let bytes = c.kept_bytes.saturating_add(packet_bytes as u64);
            if at.mono_ns < until && bytes <= c.limits.max_packet_bytes {
                c.kept_bytes = bytes;
                self.last_mono_ns = Some(at.mono_ns);
                return true;
            }
            let why = if at.mono_ns >= until {
                "duration reached"
            } else {
                "packet-byte cap reached"
            };
            self.notice = Some(format!(
                "raw full-rate capture ended ({why}) after {} packet bytes; now {}",
                c.kept_bytes, self.interval_policy
            ));
            self.capture = None;
        }
        let Some(interval) = self.interval else { return false };
        let due = self
            .last_mono_ns
            .is_none_or(|last| at.mono_ns.saturating_sub(last) >= interval.as_nanos() as u64);
        if due {
            self.last_mono_ns = Some(at.mono_ns);
        }
        due
    }
}

#[derive(Debug)]
pub struct VisitBuilder {
    pub visit_id: String,
    pub session_id: String,
    pub pan: PanId,
    pub started: Timestamp,
    pub band_id: Option<String>,
    /// Profile measurement settings for the visit (see `VisitRecord`).
    pub measurement: std::collections::BTreeMap<String, String>,
    pub tile: Option<TileRef>,
    pub requested_vfo_hz: Option<u64>,
    pub requested_span_hz: Option<u32>,
    pub usable_fraction: f64,
    pub settle_ms: u64,
    pub settle_discarded: u32,
    pub rejected: u32,
    pub flags: Vec<String>,
    max_frames: usize,
    acc: Option<VisitAccumulator>,
}

impl VisitBuilder {
    pub fn new(session_id: &str, pan: PanId, started: Timestamp, usable_fraction: f64, max_frames: usize) -> Self {
        Self {
            visit_id: new_id(),
            session_id: session_id.to_owned(),
            pan,
            started,
            band_id: None,
            measurement: std::collections::BTreeMap::new(),
            tile: None,
            requested_vfo_hz: None,
            requested_span_hz: None,
            usable_fraction,
            settle_ms: 0,
            settle_discarded: 0,
            rejected: 0,
            flags: Vec::new(),
            max_frames,
            acc: None,
        }
    }

    pub fn geometry(&self) -> Option<BinGeometry> {
        self.acc.as_ref().map(VisitAccumulator::geometry)
    }

    pub fn frames(&self) -> usize {
        self.acc.as_ref().map_or(0, VisitAccumulator::frame_count)
    }

    pub fn first_frame(&self) -> Option<Timestamp> {
        self.acc.as_ref().and_then(VisitAccumulator::first_frame)
    }

    pub fn last_frame(&self) -> Option<Timestamp> {
        self.acc.as_ref().and_then(VisitAccumulator::last_frame)
    }

    /// Accepts a frame. The first frame fixes the visit's observed geometry.
    pub fn accept(&mut self, rf: &ReceivedFrame) -> PushOutcome {
        let acc = self.acc.get_or_insert_with(|| {
            VisitAccumulator::new(self.pan, BinGeometry::from_header(rf.frame.header()), self.max_frames)
        });
        let outcome = acc.push(&rf.frame, rf.at);
        if outcome != PushOutcome::Accepted {
            self.rejected += 1;
        }
        outcome
    }

    pub fn flag(&mut self, f: &str) {
        if !self.flags.iter().any(|x| x == f) {
            self.flags.push(f.to_owned());
        }
    }

    pub fn raw_record(&self, rf: &ReceivedFrame, policy: &str) -> RawFrameRecord {
        let h = rf.frame.header();
        RawFrameRecord {
            session_id: self.session_id.clone(),
            visit_id: Some(self.visit_id.clone()),
            received: rf.at,
            pan: h.pan,
            sequence: h.sequence,
            center_hz: h.center_hz,
            sample_rate_hz: h.sample_rate_hz,
            sampling_policy: policy.to_owned(),
            bytes: rf.frame.raw().to_vec(),
        }
    }

    /// Finalizes into a record, or `None` if no frame was ever accepted
    /// (callers record a gap event instead: no data is not a low reading).
    pub fn finish(
        mut self,
        ended: Timestamp,
        status: VisitStatus,
        end_reason: &str,
        antenna: VisitAntenna,
        radio_state: RadioState,
    ) -> Option<VisitRecord> {
        let acc = self.acc.take()?;
        let geometry = acc.geometry();
        let first = acc.first_frame();
        let last = acc.last_frame();
        let stats = acc.finish();
        // Bin-identical frames are not a live measurement (frozen pan or
        // stuck stream): stored, but never as complete coverage.
        let mut status = status;
        let mut end_reason = end_reason.to_owned();
        if stats.as_ref().is_some_and(|s| s.identical_frames) {
            self.flag("identical_frames_possible_freeze");
            status = VisitStatus::Aborted;
            end_reason = format!("identical frames (pan frozen or stream stuck); {end_reason}");
        }
        if stats.as_ref().is_some_and(|s| s.sequence_repeats > 0) {
            self.flag("repeated_sequence_bytes");
        }
        Some(VisitRecord {
            visit_id: self.visit_id,
            session_id: self.session_id,
            pan: self.pan,
            band_id: self.band_id,
            tile: self.tile,
            requested_vfo_hz: self.requested_vfo_hz,
            requested_span_hz: self.requested_span_hz,
            geometry,
            usable_fraction: self.usable_fraction,
            full: geometry.full_range(),
            usable: geometry.usable_range(self.usable_fraction),
            started: self.started,
            first_frame: first,
            last_frame: last,
            ended,
            settle_ms: self.settle_ms,
            settle_discarded_frames: self.settle_discarded,
            rejected_frames: self.rejected,
            status,
            flags: self.flags,
            end_reason,
            antenna,
            radio_state,
            measurement: self.measurement,
            stats,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Timestamp {
        Timestamp {
            utc_ns: ms as i64 * 1_000_000,
            mono_ns: ms * 1_000_000,
        }
    }

    #[test]
    fn interval_sampling_keeps_the_first_frame_then_one_per_interval() {
        let mut s = RawSampler::new(1000);
        let kept: Vec<u64> = (0..90).map(|i| i * 33).filter(|&ms| s.due(at(ms), 4164)).collect();
        assert_eq!(kept, [0, 1023, 2046]);
        assert!(s.policy().starts_with("first accepted frame then >= 1000 ms"));
        assert!(s.take_notice().is_none());
        assert!(!RawSampler::new(0).due(at(0), 4164));
    }

    #[test]
    fn capture_keeps_every_frame_until_its_duration_then_samples() {
        let cap = RawCapture {
            duration: Duration::from_secs(1),
            max_packet_bytes: u64::MAX,
        };
        let mut s = RawSampler::with_capture(1000, Some(cap));
        assert!(s.due(at(500), 4164));
        assert!(s.policy().starts_with("every accepted frame"));
        assert!(s.take_notice().unwrap().contains("started"));
        // Every frame inside the window, measured from the first frame.
        assert!((1..30).all(|i| s.due(at(500 + i * 33), 4164)));
        assert!(s.take_notice().is_none());
        // The first frame past the window falls back to the interval.
        assert!(!s.due(at(1500), 4164));
        assert!(s.take_notice().unwrap().contains("duration reached"));
        assert!(s.policy().starts_with("first accepted frame"));
        assert!(s.due(at(2457), 4164));
    }

    #[test]
    fn capture_ends_at_its_packet_byte_cap() {
        let cap = RawCapture {
            duration: Duration::from_secs(3600),
            max_packet_bytes: 3 * 4164,
        };
        let mut s = RawSampler::with_capture(0, Some(cap));
        assert!((0..3).all(|i| s.due(at(i * 33), 4164)));
        assert!(!s.due(at(99), 4164));
        let note = s.take_notice().unwrap();
        assert!(
            note.contains("(packet-byte cap reached) after 12492 packet bytes"),
            "{note}"
        );
        // Raw sampling is otherwise off, so nothing more is kept.
        assert!(!s.due(at(5000), 4164));
    }
}
