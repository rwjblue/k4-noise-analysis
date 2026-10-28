//! Live line/floor monitor for source hunting (`k4na watch`).
//!
//! Consumes spectrum frames only: nothing here (or in the command that
//! drives it) opens CAT, so the radio cannot be changed. The operator
//! tunes the K4; coverage comes from each frame header, never from an
//! assumed span or VFO. Frames of one pan are averaged per interval, then
//! each target line is measured as its peak over a local background and
//! the pan floor as the median of the usable bins. State changes need a
//! hold time and are timestamped at onset (the start of the first
//! interval in the new state). Levels are native K4 tenths of a dB on the
//! uncalibrated LAN scale.

use std::collections::{BTreeMap, VecDeque};
use std::time::Duration;

use k4na_core::time::Timestamp;
use k4na_core::units::BinGeometry;
use k4na_protocol::{BIN_COUNT, PanId};
use serde::Serialize;

use crate::spectrum::ReceivedFrame;

/// What to measure.
#[derive(Debug, Clone, PartialEq)]
pub enum Targets {
    /// Specific RF frequencies.
    Hz(Vec<f64>),
    /// Every harmonic `n * f0_hz` in view.
    Harmonics { f0_hz: f64 },
}

#[derive(Debug, Clone)]
pub struct WatchParams {
    pub targets: Targets,
    pub pan: PanId,
    /// Fraction of the packet's bins used (edges dropped), as for capture.
    pub usable_fraction: f64,
    /// A line's peak is searched within ±this of its target.
    pub tolerance_hz: f64,
    /// Local background: median of bins within ±this of the target,
    /// excluding ±2 × tolerance.
    pub background_hz: f64,
    /// Frames are averaged over this long per report.
    pub interval: Duration,
    /// A line is present at this many dB over its background, absent
    /// below `line_threshold_db - hysteresis_db`.
    pub line_threshold_db: f64,
    pub hysteresis_db: f64,
    /// The floor is raised at this many dB over its baseline, normal again
    /// below half of it.
    pub floor_rise_db: f64,
    /// Baseline floor: 10th percentile of interval floors over this long.
    pub baseline_window: Duration,
    /// A new state must last this long before it is reported.
    pub hold: Duration,
}

impl WatchParams {
    pub fn new(targets: Targets, pan: PanId) -> Self {
        Self {
            targets,
            pan,
            usable_fraction: 0.9,
            tolerance_hz: 60.0,
            background_hz: 3_000.0,
            interval: Duration::from_secs(1),
            line_threshold_db: 6.0,
            hysteresis_db: 3.0,
            floor_rise_db: 4.0,
            baseline_window: Duration::from_secs(15 * 60),
            hold: Duration::from_secs(3),
        }
    }

    pub fn check(&self) -> Result<(), String> {
        let mut p = Vec::new();
        match &self.targets {
            Targets::Hz(v) if v.is_empty() => p.push("give at least one target frequency".to_owned()),
            Targets::Hz(v) if v.iter().any(|h| !h.is_finite() || *h <= 0.0) => {
                p.push("target frequencies must be positive".into())
            }
            Targets::Harmonics { f0_hz } if !f0_hz.is_finite() || *f0_hz < 100.0 => {
                p.push("the fundamental must be at least 100 Hz".into())
            }
            _ => {}
        }
        if !(1.0..=2_000.0).contains(&self.tolerance_hz) {
            p.push("tolerance must be within 1..=2000 Hz".into());
        }
        if self.background_hz < 4.0 * self.tolerance_hz {
            p.push("background window must be at least 4 x the tolerance".into());
        }
        if self.interval < Duration::from_millis(100) {
            p.push("interval must be at least 100 ms".into());
        }
        if !(0.1..=1.0).contains(&self.usable_fraction) {
            p.push("usable fraction must be within 0.1..=1.0".into());
        }
        if self.line_threshold_db <= self.hysteresis_db || self.hysteresis_db < 0.0 {
            p.push("line threshold must exceed the hysteresis".into());
        }
        if self.floor_rise_db <= 0.0 {
            p.push("floor rise must be positive".into());
        }
        if p.is_empty() { Ok(()) } else { Err(p.join("; ")) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LineState {
    Unknown,
    Present,
    Absent,
    /// Below threshold, but the floor rose so far that the line's last
    /// present level would not stand out either: not evidence it stopped.
    Masked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FloorState {
    /// Less than a minute of history: no baseline yet.
    Unknown,
    Normal,
    Raised,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LineReading {
    pub target_hz: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harmonic: Option<u64>,
    /// Centre of the strongest bin within the tolerance.
    pub peak_hz: f64,
    pub level_db: f64,
    pub background_db: f64,
    pub excess_db: f64,
    pub state: LineState,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WatchReport {
    pub start_utc_ns: i64,
    pub end_utc_ns: i64,
    pub frames: u32,
    pub center_hz: u64,
    pub sample_rate_hz: u32,
    pub bin_hz: f64,
    /// Centres of the first and last usable bins.
    pub low_hz: f64,
    pub high_hz: f64,
    /// Median of the usable bins.
    pub floor_db: f64,
    /// Floor over its baseline, once there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub floor_rise_db: Option<f64>,
    pub floor_state: FloorState,
    pub lines: Vec<LineReading>,
    /// Targets outside the usable coverage (only for `Targets::Hz`).
    pub out_of_view_hz: Vec<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Subject {
    Line {
        target_hz: f64,
        #[serde(skip_serializing_if = "Option::is_none")]
        harmonic: Option<u64>,
    },
    Floor,
    /// The frame geometry changed (the operator retuned or changed span):
    /// states restart.
    View {
        center_hz: u64,
        sample_rate_hz: u32,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WatchEvent {
    /// Start of the first interval in the new state.
    pub onset_utc_ns: i64,
    /// End of the interval that confirmed it.
    pub detected_utc_ns: i64,
    pub subject: Subject,
    pub from: String,
    pub to: String,
    /// Excess (line) or rise (floor) when confirmed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value_db: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WatchOutput {
    Report(WatchReport),
    Event(WatchEvent),
}

#[derive(Debug, Clone)]
struct Pending<S> {
    state: S,
    onset: Timestamp,
}

/// Debounced state: a change must persist for `hold`.
#[derive(Debug, Clone)]
struct Debounced<S> {
    state: S,
    pending: Option<Pending<S>>,
}

impl<S: Copy + PartialEq> Debounced<S> {
    fn new(state: S) -> Self {
        Self { state, pending: None }
    }

    /// Offers this interval's class (`None` = in the hysteresis band, keep
    /// the current state); returns the committed transition, if any.
    fn offer(
        &mut self,
        class: Option<S>,
        start: Timestamp,
        end: Timestamp,
        hold: Duration,
    ) -> Option<(S, S, Timestamp)> {
        let Some(class) = class.filter(|c| *c != self.state) else {
            self.pending = None;
            return None;
        };
        let p = match &self.pending {
            Some(p) if p.state == class => p.clone(),
            _ => Pending {
                state: class,
                onset: start,
            },
        };
        if Duration::from_nanos(end.mono_ns.saturating_sub(p.onset.mono_ns)) >= hold {
            let from = self.state;
            self.state = class;
            self.pending = None;
            Some((from, class, p.onset))
        } else {
            self.pending = Some(p);
            None
        }
    }
}

#[derive(Debug, Clone)]
struct LineTracker {
    state: Debounced<LineState>,
    /// Peak level and background of the last interval measuring present.
    present: Option<(f64, f64)>,
}

#[derive(Debug)]
pub struct Watcher {
    p: WatchParams,
    geometry: Option<BinGeometry>,
    sums: Vec<f64>,
    frames: u32,
    start: Option<Timestamp>,
    last: Option<Timestamp>,
    floors: VecDeque<(u64, f64)>,
    floor: Debounced<FloorState>,
    lines: BTreeMap<i64, LineTracker>,
}

fn median(v: &mut [f64]) -> f64 {
    let k = v.len() / 2;
    let (_, m, _) = v.select_nth_unstable_by(k, f64::total_cmp);
    *m
}

fn state_name<S: Serialize>(s: S) -> String {
    serde_json::to_value(s)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

impl Watcher {
    pub fn new(p: WatchParams) -> Result<Self, String> {
        p.check()?;
        Ok(Self {
            p,
            geometry: None,
            sums: vec![0.0; BIN_COUNT],
            frames: 0,
            start: None,
            last: None,
            floors: VecDeque::new(),
            floor: Debounced::new(FloorState::Unknown),
            lines: BTreeMap::new(),
        })
    }

    pub fn params(&self) -> &WatchParams {
        &self.p
    }

    /// Feeds one frame; returns any finished report and events.
    pub fn push(&mut self, f: &ReceivedFrame) -> Vec<WatchOutput> {
        if f.frame.pan() != self.p.pan {
            return Vec::new();
        }
        let mut out = Vec::new();
        let g = BinGeometry::from_header(f.frame.header());
        if self.geometry.is_some_and(|old| old != g) {
            out.extend(self.flush());
            self.lines.clear();
            self.floors.clear();
            self.floor = Debounced::new(FloorState::Unknown);
            out.push(WatchOutput::Event(WatchEvent {
                onset_utc_ns: f.at.utc_ns,
                detected_utc_ns: f.at.utc_ns,
                subject: Subject::View {
                    center_hz: g.center_hz,
                    sample_rate_hz: g.sample_rate_hz,
                },
                from: String::new(),
                to: String::new(),
                value_db: None,
            }));
        } else if let Some(s) = self.start
            && Duration::from_nanos(f.at.mono_ns.saturating_sub(s.mono_ns)) >= self.p.interval
        {
            out.extend(self.flush());
        }
        self.geometry = Some(g);
        if self.start.is_none() {
            self.start = Some(f.at);
        }
        for (i, s) in self.sums.iter_mut().enumerate() {
            *s += f64::from(f.frame.bin(i));
        }
        self.frames += 1;
        self.last = Some(f.at);
        out
    }

    /// Ends the current interval (on a timer boundary, retune or stop).
    pub fn flush(&mut self) -> Vec<WatchOutput> {
        let (Some(g), Some(start), Some(end)) = (self.geometry, self.start.take(), self.last) else {
            return Vec::new();
        };
        if self.frames == 0 {
            return Vec::new();
        }
        let n = f64::from(self.frames);
        let avg: Vec<f64> = self.sums.iter().map(|s| s / n / 10.0).collect();
        self.sums.iter_mut().for_each(|s| *s = 0.0);
        let frames = std::mem::take(&mut self.frames);
        let mut out = Vec::new();

        let edge = g.edge_bins(self.p.usable_fraction) as usize;
        let (a, b) = (edge, BIN_COUNT - edge);
        let hz = |i: usize| g.bin_center_hz(i as u32);
        let (low_hz, high_hz) = (hz(a), hz(b - 1));
        let floor_db = median(&mut avg[a..b].to_vec());

        // Floor baseline over the window, then the floor's state.
        let window = u64::try_from(self.p.baseline_window.as_nanos()).unwrap_or(u64::MAX);
        self.floors.push_back((end.mono_ns, floor_db));
        while self
            .floors
            .front()
            .is_some_and(|(t, _)| end.mono_ns.saturating_sub(*t) > window)
        {
            self.floors.pop_front();
        }
        let span = end
            .mono_ns
            .saturating_sub(self.floors.front().map_or(end.mono_ns, |f| f.0));
        let floor_rise_db = (span >= 60_000_000_000).then(|| {
            let mut v: Vec<f64> = self.floors.iter().map(|f| f.1).collect();
            v.sort_by(f64::total_cmp);
            floor_db - v[((v.len() - 1) as f64 * 0.1).round() as usize]
        });
        let class = floor_rise_db.and_then(|r| {
            if r >= self.p.floor_rise_db {
                Some(FloorState::Raised)
            } else if r < self.p.floor_rise_db / 2.0 {
                Some(FloorState::Normal)
            } else {
                None
            }
        });
        if let Some((from, to, onset)) = self.floor.offer(class, start, end, self.p.hold) {
            out.push(WatchOutput::Event(WatchEvent {
                onset_utc_ns: onset.utc_ns,
                detected_utc_ns: end.utc_ns,
                subject: Subject::Floor,
                from: state_name(from),
                to: state_name(to),
                value_db: floor_rise_db,
            }));
        }

        // Lines.
        let (tol, bgw) = (self.p.tolerance_hz, self.p.background_hz);
        let fits = |t: f64| t - tol >= low_hz && t + tol <= high_hz;
        let mut out_of_view_hz = Vec::new();
        let targets: Vec<(f64, Option<u64>)> = match &self.p.targets {
            Targets::Hz(v) => v
                .iter()
                .filter_map(|&t| {
                    if fits(t) {
                        Some((t, None))
                    } else {
                        out_of_view_hz.push(t);
                        None
                    }
                })
                .collect(),
            Targets::Harmonics { f0_hz } => {
                let first = ((low_hz + tol) / f0_hz).ceil() as u64;
                let last = ((high_hz - tol) / f0_hz).floor() as u64;
                (first..=last).map(|k| (k as f64 * f0_hz, Some(k))).collect()
            }
        };
        let bin = g.bin_width_hz();
        let index = |f: f64| ((f - hz(0)) / bin).round().clamp(0.0, (BIN_COUNT - 1) as f64) as usize;
        let mut lines = Vec::new();
        for (t, harmonic) in targets {
            let (pa, pb) = (index(t - tol).max(a), index(t + tol).min(b - 1));
            let Some((pi, level_db)) = (pa..=pb).map(|i| (i, avg[i])).max_by(|x, y| x.1.total_cmp(&y.1)) else {
                continue;
            };
            let mut bg: Vec<f64> = (index(t - bgw).max(a)..=index(t + bgw).min(b - 1))
                .filter(|&i| (hz(i) - t).abs() > 2.0 * tol)
                .map(|i| avg[i])
                .collect();
            if bg.len() < 8 {
                out_of_view_hz.push(t);
                continue;
            }
            let background_db = median(&mut bg);
            let excess_db = level_db - background_db;
            let key = t.round() as i64;
            let tr = self.lines.entry(key).or_insert_with(|| LineTracker {
                state: Debounced::new(LineState::Unknown),
                present: None,
            });
            let class = if excess_db >= self.p.line_threshold_db {
                Some(LineState::Present)
            } else if excess_db < self.p.line_threshold_db - self.p.hysteresis_db {
                // Masked when the background rose since the line was last
                // present, so far that its level then would not stand out
                // now; otherwise gone.
                match tr.present {
                    Some((level, bg))
                        if background_db - bg >= self.p.hysteresis_db
                            && level - background_db < self.p.line_threshold_db =>
                    {
                        Some(LineState::Masked)
                    }
                    _ => Some(LineState::Absent),
                }
            } else {
                None
            };
            if let Some((from, to, onset)) = tr.state.offer(class, start, end, self.p.hold) {
                out.push(WatchOutput::Event(WatchEvent {
                    onset_utc_ns: onset.utc_ns,
                    detected_utc_ns: end.utc_ns,
                    subject: Subject::Line { target_hz: t, harmonic },
                    from: state_name(from),
                    to: state_name(to),
                    value_db: Some(excess_db),
                }));
            }
            // Only an interval that itself measures present updates the
            // level (not one held in Present while a change is pending).
            if class == Some(LineState::Present) {
                tr.present = Some((level_db, background_db));
            }
            lines.push(LineReading {
                target_hz: t,
                harmonic,
                peak_hz: hz(pi),
                level_db,
                background_db,
                excess_db,
                state: tr.state.state,
            });
        }
        // Changes first, then the interval that shows them.
        out.push(WatchOutput::Report(WatchReport {
            start_utc_ns: start.utc_ns,
            end_utc_ns: end.utc_ns,
            frames,
            center_hz: g.center_hz,
            sample_rate_hz: g.sample_rate_hz,
            bin_hz: bin,
            low_hz,
            high_hz,
            floor_db,
            floor_rise_db,
            floor_state: self.floor.state,
            lines,
            out_of_view_hz,
        }));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k4na_protocol::SpectrumFrame;
    use k4na_protocol::synth::SyntheticFrame;

    const CENTER: u64 = 10_125_000;
    const RATE: u32 = 96_000;

    /// A frame at `t_ms` with a flat floor and optional lines.
    fn frame(t_ms: u64, floor: i16, lines: &[(f64, i16)], center: u64) -> ReceivedFrame {
        let g = BinGeometry::new(center, RATE, BIN_COUNT as u32);
        let mut bins = vec![floor; BIN_COUNT];
        // Deterministic ±1 dB ripple, so medians and maxima are not ties.
        for (i, b) in bins.iter_mut().enumerate() {
            *b += [0, 7, -5, 3, -9, 4, -2, 8][i % 8];
        }
        for &(hz, level) in lines {
            let i = ((hz - g.bin_center_hz(0)) / g.bin_width_hz()).round() as usize;
            bins[i] = level;
        }
        let bytes = SyntheticFrame {
            center_hz: center,
            sample_rate_hz: RATE,
            ..Default::default()
        }
        .encode(&bins);
        ReceivedFrame {
            frame: SpectrumFrame::from_bytes(&bytes).unwrap(),
            at: Timestamp {
                utc_ns: 1_800_000_000_000_000_000 + t_ms as i64 * 1_000_000,
                mono_ns: t_ms * 1_000_000,
            },
        }
    }

    fn run(w: &mut Watcher, frames: impl IntoIterator<Item = ReceivedFrame>) -> (Vec<WatchReport>, Vec<WatchEvent>) {
        let (mut r, mut e) = (Vec::new(), Vec::new());
        for f in frames {
            for o in w.push(&f) {
                match o {
                    WatchOutput::Report(x) => r.push(x),
                    WatchOutput::Event(x) => e.push(x),
                }
            }
        }
        (r, e)
    }

    const LINE: f64 = 10_123_547.0;

    #[test]
    fn line_appears_and_goes_with_onset_times_and_hold() {
        let mut w = Watcher::new(WatchParams::new(Targets::Hz(vec![LINE, 14_000_000.0]), PanId::A)).unwrap();
        // 10 fps: line on 0-9.9 s, off 10-19.9 s, a 1 s blip 20-20.9 s, off after.
        let frames = (0..300).map(|k| {
            let t = k * 100;
            let on = t < 10_000 || (20_000..21_000).contains(&t);
            frame(t, -1200, if on { &[(LINE, -1080)] } else { &[] }, CENTER)
        });
        let (reports, events) = run(&mut w, frames);
        assert_eq!(reports.len(), 29);
        let r = &reports[0];
        assert_eq!((r.frames, r.lines.len()), (10, 1));
        assert_eq!(
            r.out_of_view_hz,
            vec![14_000_000.0],
            "not claimed outside the header coverage"
        );
        assert!((r.lines[0].excess_db - 12.0).abs() < 1.5, "{r:?}");
        assert!((r.lines[0].peak_hz - LINE).abs() <= r.bin_hz);
        let lines: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.subject, Subject::Line { .. }))
            .map(|e| {
                (
                    e.from.as_str(),
                    e.to.as_str(),
                    (e.onset_utc_ns - 1_800_000_000_000_000_000) / 1_000_000,
                )
            })
            .collect();
        // Present from the first interval; gone at 10 s; the blip is shorter than the hold.
        assert_eq!(lines, vec![("unknown", "present", 0), ("present", "absent", 10_000)]);
        let gone = events.iter().find(|e| e.to == "absent").unwrap();
        // Confirmed at the end of the first interval that completes the 3 s hold.
        assert_eq!(gone.detected_utc_ns - gone.onset_utc_ns, 3_900_000_000);
    }

    #[test]
    fn floor_rise_masks_a_line_instead_of_ending_it() {
        let mut p = WatchParams::new(Targets::Harmonics { f0_hz: 15_967.69 }, PanId::A);
        p.hold = Duration::from_secs(2);
        let mut w = Watcher::new(p).unwrap();
        let harmonics: Vec<(f64, i16)> = (632..=636).map(|k| (k as f64 * 15_967.69, -1100)).collect();
        // 2 minutes quiet with lines, then the floor rises 15 dB for a minute, then falls.
        let frames = (0..2400).map(|k| {
            let t = k * 100;
            let floor = if (120_000..180_000).contains(&t) { -1050 } else { -1200 };
            frame(t, floor, &harmonics, CENTER)
        });
        let (reports, events) = run(&mut w, frames);
        let in_view: Vec<u64> = reports[0].lines.iter().filter_map(|l| l.harmonic).collect();
        assert_eq!(
            in_view,
            vec![632, 633, 634, 635, 636],
            "harmonics within the usable coverage"
        );
        let floor: Vec<_> = events
            .iter()
            .filter(|e| e.subject == Subject::Floor)
            .map(|e| (e.to.as_str(), (e.onset_utc_ns - 1_800_000_000_000_000_000) / 1_000_000))
            .collect();
        // No baseline until a minute of history.
        assert_eq!(
            floor,
            vec![("normal", 60_000), ("raised", 120_000), ("normal", 180_000)]
        );
        let line_634: Vec<_> = events
            .iter()
            .filter(|e| {
                matches!(
                    e.subject,
                    Subject::Line {
                        harmonic: Some(634),
                        ..
                    }
                )
            })
            .map(|e| e.to.as_str())
            .collect();
        assert_eq!(line_634, vec!["present", "masked", "present"], "never reported gone");
    }

    #[test]
    fn a_retune_restarts_states_and_other_pans_are_ignored() {
        let mut w = Watcher::new(WatchParams::new(Targets::Hz(vec![LINE]), PanId::A)).unwrap();
        let mut frames: Vec<ReceivedFrame> = (0..50)
            .map(|k| frame(k * 100, -1200, &[(LINE, -1080)], CENTER))
            .collect();
        let mut b = frame(5_000, -1200, &[], CENTER);
        let bytes = SyntheticFrame {
            pan: PanId::B,
            center_hz: CENTER,
            sample_rate_hz: RATE,
            ..Default::default()
        }
        .encode(&[-1200; BIN_COUNT]);
        b.frame = SpectrumFrame::from_bytes(&bytes).unwrap();
        frames.push(b);
        frames.extend((51..120).map(|k| frame(k * 100, -1200, &[(LINE, -1080)], CENTER + 20_000)));
        let (reports, events) = run(&mut w, frames);
        assert!(reports.iter().all(|r| r.frames <= 10));
        let view: Vec<_> = events
            .iter()
            .filter(|e| matches!(e.subject, Subject::View { .. }))
            .collect();
        assert_eq!(view.len(), 1);
        let presents = events.iter().filter(|e| e.to == "present").count();
        assert_eq!(presents, 2, "state restarted after the retune");
    }

    #[test]
    fn bad_parameters_are_refused() {
        assert!(Watcher::new(WatchParams::new(Targets::Hz(vec![]), PanId::A)).is_err());
        assert!(Watcher::new(WatchParams::new(Targets::Harmonics { f0_hz: 10.0 }, PanId::A)).is_err());
        let mut p = WatchParams::new(Targets::Hz(vec![LINE]), PanId::A);
        p.background_hz = 100.0;
        assert!(Watcher::new(p).is_err());
    }
}
