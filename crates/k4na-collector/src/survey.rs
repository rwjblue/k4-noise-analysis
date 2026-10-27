//! Opt-in survey: retunes VFO A across configured bands and dwells on
//! tiles whose coverage is verified from spectrum frame headers.
//!
//! Safety contract (docs/collection.md):
//! * Requires a [`ControlCat`](crate::cat::ControlCat), which only exists
//!   with both config and CLI consent. Only `BN`, `FA`, `#SPN` and `#FXT`
//!   can be sent.
//! * Preflight refuses to start unless transmit state is readable and off,
//!   lock/split/sub/diversity/RIT/XIT are readable and off, and the original
//!   FA/BN are known (so restoration is possible).
//! * After every own change the full interlock state is read back and
//!   becomes the baseline. Any later difference, any transmit, or an
//!   unreadable transmit state is operator activity: the survey stops and
//!   sends no further setters. Resuming requires a new run.
//! * Lost CAT is treated as unknown radio state: no retune, no restore.
//! * On a clean end the original state is restored only if the radio still
//!   matches what the collector last set.

use std::sync::Arc;
use std::time::Duration;

use k4na_core::Config;
use k4na_core::aggregate::PushOutcome;
use k4na_core::band::k4_band_number;
use k4na_core::model::{Event, EventKind, RadioState, TileRef, VisitStatus};
use k4na_core::plan::{SurveyPlan, Tile};
use k4na_core::time::{Clock, Timestamp};
use k4na_core::units::{BinGeometry, HzRange};
use k4na_protocol::{BIN_COUNT, DecoderDiagnostics, PanId};

use crate::cat::{CatControl, CatError, CatQuery, Query, Readback, Setter};
use crate::passive::{RunError, RunSummary};
use crate::runtime::{RunContext, Stop};
use crate::spectrum::{ReceivedFrame, SpectrumEvent, SpectrumSource};
use crate::store_task::StoreHandle;
use crate::visit::{RawSampler, VisitBuilder};

/// State fields that must not change except through the collector's own
/// setters.
pub const WATCH: [Query; 12] = [
    Query::VfoA,
    Query::VfoB,
    Query::Band,
    Query::Mode,
    Query::Split,
    Query::Diversity,
    Query::SubRx,
    Query::Lock,
    Query::Rit,
    Query::Xit,
    Query::Span,
    Query::FixedTune,
];

/// Must be readable and off before and during a survey.
pub const MUST_BE_OFF: [Query; 6] = [
    Query::Lock,
    Query::Split,
    Query::Diversity,
    Query::SubRx,
    Query::Rit,
    Query::Xit,
];

/// Commanded VFO frequencies are rounded to this step; readback must match
/// exactly. The rounding error is well inside the per-tile bin margin.
pub const FA_STEP_HZ: u64 = 100;

/// How often the interlock is polled during a dwell.
pub const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// How long to wait for spectrum after tuning before giving up on a tile.
const FRAME_WAIT: Duration = Duration::from_secs(5);

fn interlock_queries() -> Vec<Query> {
    let mut q = vec![Query::Transmit];
    q.extend(WATCH);
    q
}

/// Result of the read-only preflight.
#[derive(Debug, Clone)]
pub struct Preflight {
    pub readback: Readback,
    pub snapshot: RadioState,
    pub firmware: Vec<String>,
}

/// Reads state with GET queries only and decides whether a survey may
/// start. Never sends a setter.
pub async fn preflight<C: CatQuery>(cat: &mut C, cfg: &Config) -> Result<Preflight, String> {
    let mut queries = interlock_queries();
    queries.extend(Query::STATE);
    queries.extend([Query::Identify, Query::Revisions]);
    queries.sort();
    queries.dedup();
    let rb = cat
        .query(&queries)
        .await
        .map_err(|e| format!("CAT readback failed: {e}"))?;
    let mut problems = Vec::new();
    match rb.flag(Query::Transmit) {
        Some(false) => {}
        Some(true) => problems.push("the radio is transmitting".to_owned()),
        None => problems.push("transmit state (TQ) is unreadable".to_owned()),
    }
    for q in MUST_BE_OFF {
        match rb.flag(q) {
            Some(false) => {}
            Some(true) => problems.push(format!("{} is on (survey needs a simple main-RX setup)", q.prefix())),
            None => problems.push(format!("{} is unreadable", q.prefix())),
        }
    }
    if rb.u64(Query::VfoA).is_none() || rb.u64(Query::Band).is_none() {
        problems.push("original FA/BN are unreadable, so restoration would be impossible".to_owned());
    }
    if cfg.radio.require_firmware_readback && rb.revisions.is_empty() {
        problems.push("firmware revisions (RV) were not read back".to_owned());
    }
    if !problems.is_empty() {
        return Err(format!("survey refused before any change: {}", problems.join("; ")));
    }
    Ok(Preflight {
        snapshot: rb.to_state(&queries),
        firmware: rb.revisions.clone(),
        readback: rb,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Halt {
    /// Stop request or duration reached: restore if safe.
    Stop(String),
    /// Operator activity or readback disagreement: never touch the radio.
    Conflict(String),
    /// CAT unusable: state unknown, never touch the radio.
    CatLost(String),
    /// Storage refused writes: restore if safe.
    Storage(String),
    /// Spectrum unusable for verification: restore if safe.
    NoSpectrum(String),
}

impl Halt {
    fn reason(&self) -> &str {
        match self {
            Halt::Stop(r) | Halt::Conflict(r) | Halt::CatLost(r) | Halt::Storage(r) | Halt::NoSpectrum(r) => r,
        }
    }

    fn may_restore(&self) -> bool {
        matches!(self, Halt::Stop(_) | Halt::Storage(_) | Halt::NoSpectrum(_))
    }
}

impl From<CatError> for Halt {
    fn from(e: CatError) -> Self {
        Halt::CatLost(format!("CAT failed ({e}); radio state is unknown"))
    }
}

impl From<RunError> for Halt {
    fn from(e: RunError) -> Self {
        Halt::Storage(format!("storage stopped collection: {e}"))
    }
}

impl From<k4na_store::StoreError> for Halt {
    fn from(e: k4na_store::StoreError) -> Self {
        Halt::Storage(format!("storage stopped collection: {e}"))
    }
}

type Step<T> = Result<T, Halt>;

#[derive(Debug, Clone, serde::Serialize)]
pub struct SurveyReport {
    pub summary: RunSummary,
    /// `stop`, `conflict`, `cat_lost`, `storage` or `no_spectrum`.
    pub halt: String,
    pub restore: String,
    pub cycles: u64,
    pub tiles_planned: usize,
    pub coverage_unverified: u64,
}

struct Survey<'a, S, C> {
    cfg: &'a Config,
    store: &'a StoreHandle,
    clock: Arc<dyn Clock>,
    session_id: String,
    stop: Stop,
    deadline: tokio::time::Instant,
    spectrum: &'a mut S,
    cat: &'a mut C,
    pan: PanId,
    original: Readback,
    baseline: Readback,
    offset_hz: i64,
    plan: Option<SurveyPlan>,
    sampler: RawSampler,
    policy: String,
    diag_at_last_visit: DecoderDiagnostics,
    spectrum_down_since: Option<Timestamp>,
    last_tick: tokio::time::Instant,
    report: SurveyReport,
}

/// Outcome of settling on a tuned frequency.
struct Settled {
    geometry: Option<BinGeometry>,
    discarded: u32,
}

impl<S: SpectrumSource, C: CatControl> Survey<'_, S, C> {
    fn now(&self) -> Timestamp {
        self.clock.now()
    }

    fn event(&self, kind: EventKind, msg: impl Into<String>) -> Event {
        Event::new(&self.session_id, self.now(), kind, msg)
    }

    async fn record(&self, e: Event) -> Step<()> {
        self.store.event(e).await.map_err(Halt::from)
    }

    async fn read(&mut self) -> Step<Readback> {
        Ok(self.cat.query(&interlock_queries()).await?)
    }

    /// Reads the interlock and compares it with the baseline.
    async fn check(&mut self) -> Step<Readback> {
        let rb = self.read().await?;
        match rb.flag(Query::Transmit) {
            Some(false) => {}
            Some(true) => {
                self.record(self.event(
                    EventKind::TransmitDetected,
                    "radio is transmitting; survey stops without further changes",
                ))
                .await?;
                return Err(Halt::Conflict("transmit detected".into()));
            }
            None => {
                self.record(self.event(
                    EventKind::TransmitDetected,
                    "transmit state unreadable; treated as unsafe",
                ))
                .await?;
                return Err(Halt::Conflict("transmit state unreadable".into()));
            }
        }
        let diffs = diff(&self.baseline, &rb, &WATCH);
        if !diffs.is_empty() {
            let msg = format!("radio changed outside the collector: {}", diffs.join(", "));
            self.record(self.event(EventKind::ExternalChange, &msg)).await?;
            return Err(Halt::Conflict(msg));
        }
        Ok(rb)
    }

    /// Checks the interlock, sends setters, reads back and verifies them,
    /// and adopts the readback as the new baseline.
    async fn apply(&mut self, setters: &[Setter]) -> Step<()> {
        self.check().await?;
        for s in setters {
            self.cat.set(*s).await?;
        }
        let rb = self.read().await?;
        if rb.flag(Query::Transmit) != Some(false) {
            self.record(self.event(
                EventKind::TransmitDetected,
                "transmit active or unreadable right after retune",
            ))
            .await?;
            return Err(Halt::Conflict("transmit detected after retune".into()));
        }
        let mut problems = Vec::new();
        let mut may_change = vec![Query::VfoA];
        for s in setters {
            let ok = match *s {
                Setter::Band(n) => {
                    may_change.extend([Query::Band, Query::Mode, Query::VfoB]);
                    rb.u64(Query::Band) == Some(u64::from(n))
                }
                Setter::VfoA(hz) => rb.u64(Query::VfoA) == Some(hz),
                Setter::Span(hz) => {
                    may_change.push(Query::Span);
                    rb.u64(Query::Span) == Some(u64::from(hz))
                }
                Setter::FixedTune(f) => {
                    may_change.push(Query::FixedTune);
                    rb.flag(Query::FixedTune) == Some(f)
                }
            };
            if !ok {
                problems.push(format!("{} not confirmed by readback", s.wire()));
            }
        }
        let others: Vec<Query> = WATCH.into_iter().filter(|q| !may_change.contains(q)).collect();
        problems.extend(diff(&self.baseline, &rb, &others));
        if !problems.is_empty() {
            let msg = format!("readback disagrees after retune: {}", problems.join(", "));
            self.record(self.event(EventKind::ReadbackMismatch, &msg)).await?;
            return Err(Halt::Conflict(msg));
        }
        self.baseline = rb;
        Ok(())
    }

    /// Tunes so the pan is expected to center on `center_hz`.
    async fn tune(&mut self, center_hz: u64) -> Step<u64> {
        let fa = round_to_step(center_hz as i64 - self.offset_hz);
        let mut setters = Vec::new();
        if let Some(n) = k4_band_number(fa)
            && self.baseline.u64(Query::Band) != Some(u64::from(n))
        {
            setters.push(Setter::Band(n));
        }
        setters.push(Setter::VfoA(fa));
        self.apply(&setters).await?;
        Ok(fa)
    }

    fn time_left(&self) -> Step<()> {
        if let Some(r) = self.stop.reason() {
            return Err(Halt::Stop(r));
        }
        if tokio::time::Instant::now() >= self.deadline {
            return Err(Halt::Stop("duration reached".into()));
        }
        Ok(())
    }

    /// Next spectrum event, or a halt on stop/deadline.
    async fn next_event(&mut self, until: tokio::time::Instant) -> Step<Option<SpectrumEvent>> {
        let deadline = self.deadline;
        tokio::select! {
            r = self.stop.requested() => Err(Halt::Stop(r)),
            _ = tokio::time::sleep_until(deadline) => Err(Halt::Stop("duration reached".into())),
            _ = tokio::time::sleep_until(until) => Ok(None),
            ev = self.spectrum.next() => Ok(Some(ev)),
        }
    }

    fn note_ignored(&mut self, rf: &ReceivedFrame) -> bool {
        if rf.frame.pan() == self.pan {
            return false;
        }
        *self
            .report
            .summary
            .ignored_frames
            .entry(rf.frame.pan().to_string())
            .or_default() += 1;
        true
    }

    async fn on_disconnect(&mut self, reason: String, stalled: bool) -> Step<()> {
        let kind = if stalled {
            EventKind::SpectrumStalled
        } else {
            EventKind::SpectrumDisconnected
        };
        if self.spectrum_down_since.is_none() {
            self.spectrum_down_since = Some(self.now());
        }
        self.record(self.event(kind, reason)).await
    }

    async fn on_reconnect(&mut self, at: Timestamp) -> Step<()> {
        if let Some(since) = self.spectrum_down_since.take() {
            self.report.summary.gaps += 1;
            self.record(
                self.event(EventKind::Gap, "no spectrum while disconnected")
                    .interval(since.utc_ns, at.utc_ns),
            )
            .await?;
        }
        Ok(())
    }

    /// Waits (without retuning) for spectrum to return, still polling the
    /// interlock so operator activity is noticed.
    async fn wait_for_spectrum(&mut self) -> Step<()> {
        let mut since_check = tokio::time::Instant::now();
        while self.spectrum_down_since.is_some() {
            let until = tokio::time::Instant::now() + Duration::from_secs(2);
            match self.next_event(until).await? {
                Some(SpectrumEvent::Connected) => {
                    self.record(self.event(EventKind::SpectrumConnected, "spectrum stream connected"))
                        .await?;
                }
                Some(SpectrumEvent::Frame(rf)) => {
                    if !self.note_ignored(&rf) {
                        self.on_reconnect(rf.at).await?;
                    }
                }
                Some(SpectrumEvent::Disconnected { reason, stalled }) => self.on_disconnect(reason, stalled).await?,
                None => {}
            }
            if since_check.elapsed() >= Duration::from_secs(2) {
                since_check = tokio::time::Instant::now();
                self.check().await?;
            }
        }
        Ok(())
    }

    /// Discards frames until `settle_ms` after the confirmed retune, and
    /// returns the last observed pan geometry.
    async fn settle(&mut self) -> Step<Settled> {
        let settle_end = tokio::time::Instant::now() + Duration::from_millis(self.cfg.collection.settle_ms);
        let wait_end = settle_end + FRAME_WAIT;
        let mut s = Settled {
            geometry: None,
            discarded: 0,
        };
        loop {
            let now = tokio::time::Instant::now();
            if now >= settle_end && s.geometry.is_some() {
                return Ok(s);
            }
            let until = if now < settle_end { settle_end } else { wait_end };
            match self.next_event(until).await? {
                None if tokio::time::Instant::now() >= wait_end => return Ok(s),
                None => {}
                Some(SpectrumEvent::Frame(rf)) => {
                    if self.note_ignored(&rf) {
                        continue;
                    }
                    self.on_reconnect(rf.at).await?;
                    // After the settle time, the first frame defines the
                    // geometry the dwell must keep.
                    let g = BinGeometry::from_header(rf.frame.header());
                    s.geometry = Some(g);
                    if tokio::time::Instant::now() < settle_end {
                        s.discarded += 1;
                    }
                }
                Some(SpectrumEvent::Connected) => {
                    self.record(self.event(EventKind::SpectrumConnected, "spectrum stream connected"))
                        .await?;
                }
                Some(SpectrumEvent::Disconnected { reason, stalled }) => {
                    self.on_disconnect(reason, stalled).await?;
                    return Ok(Settled {
                        geometry: None,
                        discarded: s.discarded,
                    });
                }
            }
        }
    }

    /// Accepts frames for `dwell_ms`, polling the interlock. Returns the
    /// visit and the halt that ended it early, if any.
    async fn dwell(&mut self, mut v: VisitBuilder) -> (VisitBuilder, Option<Halt>) {
        let dwell_end = tokio::time::Instant::now() + Duration::from_millis(self.cfg.collection.dwell_ms);
        let mut next_poll = tokio::time::Instant::now() + POLL_INTERVAL;
        loop {
            let now = tokio::time::Instant::now();
            if now >= dwell_end {
                return (v, None);
            }
            if now >= next_poll {
                next_poll = now + POLL_INTERVAL;
                if let Err(h) = self.check().await {
                    v.flag("interlock");
                    return (v, Some(h));
                }
            }
            let ev = match self.next_event(dwell_end.min(next_poll)).await {
                Ok(ev) => ev,
                Err(h) => return (v, Some(h)),
            };
            match ev {
                None => {}
                Some(SpectrumEvent::Frame(rf)) => {
                    if self.note_ignored(&rf) {
                        continue;
                    }
                    match v.accept(&rf) {
                        PushOutcome::Accepted => {
                            if self.sampler.due(rf.at) && !self.store.raw_paused() {
                                self.store.offer_raw(v.raw_record(&rf, &self.policy));
                            }
                        }
                        PushOutcome::Full => {
                            v.rejected = v.rejected.saturating_sub(1);
                            v.flag("frame_cap");
                            return (v, None);
                        }
                        _ => {
                            // Geometry moved without a collector setter.
                            v.flag("geometry_changed");
                            let h = match self.check().await {
                                Err(h) => h,
                                Ok(_) => Halt::Conflict(
                                    "pan geometry changed during dwell without a CAT state change".into(),
                                ),
                            };
                            return (v, Some(h));
                        }
                    }
                }
                Some(SpectrumEvent::Connected) => {}
                Some(SpectrumEvent::Disconnected { reason, stalled }) => {
                    v.flag("spectrum_lost");
                    if let Err(h) = self.on_disconnect(reason, stalled).await {
                        return (v, Some(h));
                    }
                    return (v, None);
                }
            }
        }
    }

    fn ensure_plan(&mut self, sample_rate_hz: u32) -> Step<bool> {
        if self.plan.as_ref().is_some_and(|p| p.sample_rate_hz == sample_rate_hz) {
            return Ok(false);
        }
        let plan = SurveyPlan::build(
            &self.cfg.bands,
            sample_rate_hz,
            BIN_COUNT as u32,
            self.cfg.collection.usable_fraction,
        )
        .map_err(|e| Halt::NoSpectrum(format!("cannot plan with observed sample rate: {e}")))?;
        let rebuilt = self.plan.is_some();
        self.report.tiles_planned = plan.tiles.len();
        self.plan = Some(plan);
        Ok(rebuilt)
    }

    async fn plan_event(&self, rebuilt: bool) -> Step<()> {
        let Some(p) = &self.plan else { return Ok(()) };
        let c = &self.cfg.collection;
        let msg = format!(
            "{} tiles, {} visits/cycle, observed packet rate {} Hz, usable {} Hz, cycle <= {} s",
            p.tiles.len(),
            p.cycle.len(),
            p.sample_rate_hz,
            p.usable_width_hz,
            p.cycle_duration(c.settle_ms, c.dwell_ms, 250).as_secs()
        );
        let kind = if rebuilt {
            EventKind::PlanRebuilt
        } else {
            EventKind::PlanBuilt
        };
        self.record(self.event(kind, msg).count(p.tiles.len() as u64)).await
    }

    /// Tunes for a center and settles; one learned-offset correction if the
    /// observed usable span misses `target`.
    async fn tune_and_settle(
        &mut self,
        center_hz: u64,
        target: Option<&HzRange>,
        band: Option<&str>,
        tile: Option<&str>,
    ) -> Step<(u64, Settled)> {
        let mut fa = self.tune(center_hz).await?;
        let mut settled = self.settle().await?;
        let Some(g) = settled.geometry else {
            return Ok((fa, settled));
        };
        self.offset_hz = g.center_hz as i64 - fa as i64;
        if let Some(target) = target
            && !g.usable_range(self.cfg.collection.usable_fraction).covers(target)
        {
            let msg = format!(
                "pan center {} Hz with FA {} Hz misses target {}..{} Hz; retuning with learned offset {} Hz",
                g.center_hz, fa, target.low_hz, target.high_hz, self.offset_hz
            );
            self.record(self.event(EventKind::RetuneCorrection, msg).band(band, tile))
                .await?;
            fa = self.tune(center_hz).await?;
            let again = self.settle().await?;
            if let Some(g2) = again.geometry {
                self.offset_hz = g2.center_hz as i64 - fa as i64;
            }
            settled = Settled {
                geometry: again.geometry,
                discarded: settled.discarded + again.discarded,
            };
        }
        Ok((fa, settled))
    }

    async fn maybe_tick(&mut self) -> Step<()> {
        if self.last_tick.elapsed() >= Duration::from_secs(1) {
            self.last_tick = tokio::time::Instant::now();
            self.store.tick(self.now()).await?;
        }
        if let Some(s) = self.store.status().suspended {
            return Err(Halt::Storage(format!("storage stopped collection: {s}")));
        }
        Ok(())
    }

    async fn finish_visit(&mut self, mut v: VisitBuilder, tile: &Tile, halt: Option<&Halt>) -> Step<()> {
        let end = v.last_frame().unwrap_or_else(|| self.now());
        let frames = v.frames();
        let covered = v
            .geometry()
            .is_some_and(|g| g.usable_range(self.cfg.collection.usable_fraction).covers(&tile.target));
        let mut status = VisitStatus::Complete;
        let mut reason = "dwell complete".to_owned();
        if frames < self.cfg.collection.min_frames_per_visit {
            v.flag("too_few_frames");
            status = VisitStatus::Partial;
            reason = "too few frames".into();
        }
        if v.flags.iter().any(|f| f == "spectrum_lost" || f == "geometry_changed") {
            status = VisitStatus::Aborted;
            reason = "spectrum lost or geometry changed".into();
        }
        if !covered && frames > 0 {
            v.flag("coverage_unverified");
            status = VisitStatus::Aborted;
            reason = "observed usable span does not cover the tile".into();
        }
        if let Some(h) = halt {
            status = match h {
                Halt::Stop(_) | Halt::Storage(_) if status == VisitStatus::Complete => VisitStatus::Partial,
                Halt::Stop(_) | Halt::Storage(_) => status,
                _ => VisitStatus::Aborted,
            };
            reason = h.reason().to_owned();
        }
        let mut events = Vec::new();
        let diag = self.spectrum.diagnostics();
        let delta = diag.since(&self.diag_at_last_visit);
        self.diag_at_last_visit = diag;
        if delta.rejected_candidates() > 0 || delta.discarded_bytes > 0 {
            events.push(
                self.event(
                    EventKind::DecoderRejects,
                    format!("decoder rejected candidates during visit: {delta:?}"),
                )
                .band(Some(&tile.band_id), Some(&tile.id()))
                .count(delta.rejected_candidates()),
            );
        }
        let state = self.baseline.to_state(&interlock_queries());
        if let Some(record) = v.finish(end, status, &reason, state) {
            self.report.summary.visits += 1;
            self.report.summary.accepted_frames += frames as u64;
            self.store.write_visit(record, events).await?;
        } else {
            self.report.coverage_unverified += 1;
            events.push(
                self.event(EventKind::CoverageUnverified, format!("no frames for tile: {reason}"))
                    .band(Some(&tile.band_id), Some(&tile.id())),
            );
            self.store.write_events(events, end).await?;
        }
        Ok(())
    }

    async fn visit_tile(&mut self, tile: &Tile) -> Step<()> {
        let tile_id = tile.id();
        let started = self.now();
        let (fa, settled) = self
            .tune_and_settle(tile.center_hz, Some(&tile.target), Some(&tile.band_id), Some(&tile_id))
            .await?;
        let Some(g) = settled.geometry else {
            self.report.coverage_unverified += 1;
            let e = self
                .event(EventKind::CoverageUnverified, "no spectrum frames after tuning")
                .band(Some(&tile.band_id), Some(&tile_id));
            return self.record(e.interval(started.utc_ns, self.now().utc_ns)).await;
        };
        if self.ensure_plan(g.sample_rate_hz)? {
            self.plan_event(true).await?;
            // The tile set changed; the caller resumes from the new plan.
            return Ok(());
        }
        if !g.usable_range(self.cfg.collection.usable_fraction).covers(&tile.target) {
            self.report.coverage_unverified += 1;
            let msg = format!(
                "observed pan {}..{} Hz still misses the tile after correction",
                g.full_range().low_hz,
                g.full_range().high_hz
            );
            let e = self
                .event(EventKind::CoverageUnverified, msg)
                .band(Some(&tile.band_id), Some(&tile_id));
            return self.record(e.interval(started.utc_ns, self.now().utc_ns)).await;
        }
        let c = &self.cfg.collection;
        let mut v = VisitBuilder::new(
            &self.session_id,
            self.pan,
            started,
            c.usable_fraction,
            c.max_frames_per_visit,
        );
        v.band_id = Some(tile.band_id.clone());
        v.tile = Some(TileRef {
            index: tile.index,
            id: tile_id,
            target: tile.target,
        });
        v.requested_vfo_hz = Some(fa);
        v.requested_span_hz = self.baseline.u64(Query::Span).and_then(|s| u32::try_from(s).ok());
        v.settle_ms = c.settle_ms;
        v.settle_discarded = settled.discarded;
        let (v, halt) = self.dwell(v).await;
        // Confirm state at the end of the dwell before calling it complete.
        let halt = match halt {
            Some(h) => Some(h),
            None => self.check().await.err(),
        };
        self.finish_visit(v, tile, halt.as_ref()).await?;
        match halt {
            Some(h) => Err(h),
            None => Ok(()),
        }
    }

    async fn run(&mut self) -> Step<()> {
        // Track mode and requested span, only if their originals are known
        // (so they can be restored).
        let mut setup = Vec::new();
        if self.original.flag(Query::FixedTune) == Some(true) {
            setup.push(Setter::FixedTune(false));
        }
        if let (Some(span), Some(_)) = (
            self.cfg.collection.requested_pan_span_hz,
            self.original.u64(Query::Span),
        ) && self.original.u64(Query::Span) != Some(u64::from(span))
        {
            setup.push(Setter::Span(span));
        }
        if !setup.is_empty() {
            self.apply(&setup).await?;
        }
        // Probe the observed packet geometry at the first band's center.
        let first = self
            .cfg
            .bands
            .iter()
            .min_by_key(|b| b.low_hz)
            .ok_or_else(|| Halt::NoSpectrum("no bands".into()))?;
        let center = first.range().midpoint();
        let (_, settled) = self.tune_and_settle(center, None, Some(&first.id), None).await?;
        let Some(g) = settled.geometry else {
            return Err(Halt::NoSpectrum(
                "no spectrum frames after tuning; coverage cannot be verified".into(),
            ));
        };
        self.ensure_plan(g.sample_rate_hz)?;
        self.plan_event(false).await?;

        let mut pos = 0usize;
        loop {
            self.time_left()?;
            if self.spectrum_down_since.is_some() {
                self.wait_for_spectrum().await?;
            }
            let plan = self.plan.as_ref().ok_or_else(|| Halt::NoSpectrum("no plan".into()))?;
            if pos >= plan.cycle.len() {
                pos = 0;
                self.report.cycles += 1;
            }
            let tile = plan.tiles[plan.cycle[pos]].clone();
            let rate = plan.sample_rate_hz;
            self.visit_tile(&tile).await?;
            self.maybe_tick().await?;
            let plan = self.plan.as_ref().ok_or_else(|| Halt::NoSpectrum("no plan".into()))?;
            if plan.sample_rate_hz != rate {
                // Resume on the nearest tile of the same band in the new plan.
                let idx = plan.tile_matching(&tile.band_id, tile.center_hz).map_or(0, |t| t.index);
                pos = plan.cycle.iter().position(|i| *i == idx).unwrap_or(0);
            } else {
                pos += 1;
            }
        }
    }

    /// Restores the original BN/FA/#SPN/#FXT if the radio still matches the
    /// collector's last state. Returns a human-readable result.
    async fn restore(&mut self, halt: &Halt) -> String {
        if !halt.may_restore() {
            let msg = format!("not restoring: {}", halt.reason());
            let _ = self.record(self.event(EventKind::RestoreSkipped, &msg)).await;
            return msg;
        }
        let mut setters = Vec::new();
        let orig_band = self.original.u64(Query::Band).and_then(|b| u8::try_from(b).ok());
        if let Some(b) = orig_band
            && self.baseline.u64(Query::Band) != Some(u64::from(b))
        {
            setters.push(Setter::Band(b));
        }
        if let Some(fa) = self.original.u64(Query::VfoA)
            && self.baseline.u64(Query::VfoA) != Some(fa)
        {
            setters.push(Setter::VfoA(fa));
        }
        if let Some(span) = self.original.u64(Query::Span).and_then(|s| u32::try_from(s).ok())
            && self.baseline.u64(Query::Span) != Some(u64::from(span))
        {
            setters.push(Setter::Span(span));
        }
        if let Some(fixed) = self.original.flag(Query::FixedTune)
            && self.baseline.flag(Query::FixedTune) != Some(fixed)
        {
            setters.push(Setter::FixedTune(fixed));
        }
        if setters.is_empty() {
            let msg = "radio already in its original state".to_owned();
            let _ = self.record(self.event(EventKind::RestoreDone, &msg)).await;
            return msg;
        }
        match self.apply(&setters).await {
            Ok(()) => {
                let mut msg = format!("restored: {}", setters.iter().map(|s| s.wire()).collect::<String>());
                if self.baseline.raw(Query::Mode) != self.original.raw(Query::Mode) {
                    msg.push_str(&format!(
                        "; mode is {:?} (was {:?}) and the collector cannot set mode",
                        self.baseline.raw(Query::Mode),
                        self.original.raw(Query::Mode)
                    ));
                }
                let _ = self.record(self.event(EventKind::RestoreDone, &msg)).await;
                msg
            }
            Err(h) => {
                let msg = format!("not restoring: {}", h.reason());
                let _ = self.record(self.event(EventKind::RestoreSkipped, &msg)).await;
                msg
            }
        }
    }
}

fn diff(before: &Readback, after: &Readback, fields: &[Query]) -> Vec<String> {
    fields
        .iter()
        .filter(|q| before.raw(**q) != after.raw(**q))
        .map(|q| format!("{} {:?} -> {:?}", q.prefix(), before.raw(*q), after.raw(*q)))
        .collect()
}

fn round_to_step(hz: i64) -> u64 {
    let step = FA_STEP_HZ as i64;
    (((hz + step / 2).div_euclid(step)) * step).max(0) as u64
}

/// Runs an authorized survey after a successful [`preflight`]. The caller
/// must hold a [`ControlCat`](crate::cat::ControlCat), which proves consent.
pub async fn run_survey<S: SpectrumSource, C: CatControl>(
    ctx: RunContext<'_>,
    spectrum: &mut S,
    cat: &mut C,
    preflight: Preflight,
    duration: Duration,
) -> Result<SurveyReport, RunError> {
    let RunContext {
        cfg,
        store,
        clock,
        session_id,
        stop,
    } = ctx;
    let sampler = RawSampler::new(cfg.collection.raw_sample_interval_ms);
    let policy = sampler.policy();
    let mut s = Survey {
        cfg,
        store,
        clock: clock.clone(),
        session_id,
        stop,
        deadline: tokio::time::Instant::now() + duration,
        spectrum,
        cat,
        pan: cfg.radio.receiver.pan(),
        baseline: preflight.readback.clone(),
        original: preflight.readback,
        offset_hz: 0,
        plan: None,
        sampler,
        policy,
        diag_at_last_visit: DecoderDiagnostics::default(),
        spectrum_down_since: Some(clock.now()),
        last_tick: tokio::time::Instant::now(),
        report: SurveyReport {
            summary: RunSummary::default(),
            halt: String::new(),
            restore: String::new(),
            cycles: 0,
            tiles_planned: 0,
            coverage_unverified: 0,
        },
    };
    store
        .event(s.event(
            EventKind::SessionStart,
            format!(
                "authorized survey for {} s; setters limited to BN/FA/#SPN/#FXT",
                duration.as_secs()
            ),
        ))
        .await?;
    let halt = s.run().await.err().unwrap_or_else(|| Halt::Stop("finished".into()));
    let restore = s.restore(&halt).await;
    if let Some(since) = s.spectrum_down_since {
        let end = s.now();
        s.report.summary.gaps += 1;
        let _ = s
            .record(
                s.event(EventKind::Gap, "no spectrum until session end")
                    .interval(since.utc_ns, end.utc_ns),
            )
            .await;
    }
    if !s.report.summary.ignored_frames.is_empty() {
        let msg = format!(
            "frames from other pans were not stored: {:?}",
            s.report.summary.ignored_frames
        );
        let _ = s.record(s.event(EventKind::Warning, msg)).await;
    }
    s.report.halt = match &halt {
        Halt::Stop(_) => "stop",
        Halt::Conflict(_) => "conflict",
        Halt::CatLost(_) => "cat_lost",
        Halt::Storage(_) => "storage",
        Halt::NoSpectrum(_) => "no_spectrum",
    }
    .to_owned();
    s.report.summary.failed = !matches!(halt, Halt::Stop(_));
    s.report.summary.end_reason = halt.reason().to_owned();
    s.report.summary.decoder = s.spectrum.diagnostics();
    s.report.restore = restore;
    Ok(s.report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fa_rounding() {
        assert_eq!(round_to_step(14_074_049), 14_074_000);
        assert_eq!(round_to_step(14_074_050), 14_074_100);
        assert_eq!(round_to_step(-5), 0);
    }
}
