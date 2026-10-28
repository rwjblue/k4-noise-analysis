//! Opt-in survey: retunes VFO A across configured bands and dwells on
//! tiles whose coverage is verified from spectrum frame headers.
//!
//! Safety contract (docs/collection.md, docs/recovery.md):
//! * Requires a [`ControlCat`](crate::cat::ControlCat), which only exists
//!   with both config and CLI consent. Only `BN`, `FA`, `#SPN`, `#FXT`,
//!   (with `collection.allow_rx_antenna_switch`) `AR`, and the receiver
//!   measurement settings of docs/measurement.md can be sent. The TX
//!   antenna and the CAT meta-mode are never changed.
//! * Every change is a journaled transaction step: the scope's state as
//!   found and the intended change are durable before a setter is sent;
//!   the verified readback follows. A setting whose found value the
//!   collector could not put back is never changed.
//! * Profile measurement settings are applied and verified per band and
//!   antenna before a visit; a context that cannot be set as configured is
//!   skipped unchanged.
//! * Every visit is attributed to a profile antenna whose port matches the
//!   `AR`/`AN` readback. The input is re-read after every band change
//!   (the K4 may recall a per-band antenna); a band on the wrong input is
//!   selected with `AR` if allowed, else skipped. The input found on a
//!   band is put back before leaving it.
//! * Preflight refuses to start unless transmit state is readable and off,
//!   lock/split/sub/diversity/RIT/XIT are readable and off, and the original
//!   FA/BN are known (so restoration is possible).
//! * After every own change the full interlock state is read back and
//!   becomes the baseline. Any later difference, any transmit, or an
//!   unreadable transmit state is operator activity: the survey stops and
//!   sends no further setters. Resuming requires a new run.
//! * Lost CAT is treated as unknown radio state: no retune, no restore;
//!   the journal stays pending for `k4na recovery`.
//! * On a clean end the shared restore engine puts every changed field
//!   back if it still holds the collector's value, and leaves others.
//! * Cycles start at most every `min_cycle_ms`; between cycles the survey
//!   holds the last tile without retuning and keeps polling the interlock.

use std::sync::Arc;
use std::time::Duration;

use k4na_core::Config;
use k4na_core::aggregate::PushOutcome;
use k4na_core::band::k4_band_number;
use k4na_core::config::{AntennaConfig, K4Port, SurveyProfile};
use k4na_core::measure::{MeasurementSettings, band_label};
use k4na_core::model::{Event, EventKind, RadioState, TileRef, VisitAntenna, VisitStatus};
use k4na_core::plan::{SurveyPlan, Tile};
use k4na_core::time::{Clock, Timestamp};
use k4na_core::units::{BinGeometry, HzRange};
use k4na_protocol::{BIN_COUNT, DecoderDiagnostics, PanId};

use crate::cat::{CatControl, CatError, CatQuery, Query, Readback, RxInput, Setter};
use crate::measure::{self, MetaMode, expected_readback, setter_for};
use crate::passive::{RunError, RunSummary};
use crate::recovery::{self, Change, Decision, Entry, JournalError, Outcome, Purpose, Scope, Transaction};
use crate::runtime::{RunContext, Stop};
use crate::spectrum::{ReceivedFrame, SpectrumEvent, SpectrumSource};
use crate::store_task::StoreHandle;
use crate::visit::{RawSampler, VisitBuilder};

/// State fields that must not change except through the collector's own
/// setters, including the receiver measurement settings and the CAT
/// meta-mode (whose change would alter readback formats).
pub const WATCH: [Query; 24] = [
    Query::Identify,
    Query::Preamp,
    Query::Attenuator,
    Query::RfGain,
    Query::Agc,
    Query::NoiseBlanker,
    Query::NoiseReduction,
    Query::PanNoiseBlanker,
    Query::Averaging,
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
    Query::Freeze,
    Query::RxAntenna,
    Query::TxAntenna,
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
fn poll_interval(cfg: &Config) -> Duration {
    Duration::from_millis(cfg.collection.cat_poll_ms)
}

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
    pub meta: MetaMode,
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
    // #FRZ may be unsupported; refuse only if it is known to be on.
    if rb.flag(Query::FixedTune) == Some(true) && !cfg.collection.set_pan_track_mode {
        problems.push(
            "the pan is fixed (#FXT1) and would not follow FA; set collection.set_pan_track_mode = true to let \
             the survey switch it to track mode (restored afterwards)"
                .to_owned(),
        );
    }
    if rb.flag(Query::Freeze) == Some(true) {
        problems.push("the pan display is frozen (#FRZ1), so frames would not be live".to_owned());
    }
    if rb.u64(Query::RxAntenna).is_none() {
        problems.push("the main RX input (AR) is unreadable, so the antenna cannot be verified".to_owned());
    }
    if rb.u64(Query::VfoA).is_none() || rb.u64(Query::Band).is_none() {
        problems.push("original FA/BN are unreadable, so restoration would be impossible".to_owned());
    }
    if cfg.radio.require_firmware_readback && rb.revisions.is_empty() {
        problems.push("firmware revisions (RV) were not read back".to_owned());
    }
    let meta = MetaMode::from_readback(&rb);
    if meta.is_none() {
        problems.push(format!(
            "the CAT meta-mode (K4) reads {:?}, so readback formats are unknown",
            rb.raw(Query::Identify)
        ));
    }
    if !problems.is_empty() {
        return Err(format!("survey refused before any change: {}", problems.join("; ")));
    }
    Ok(Preflight {
        snapshot: rb.to_state(&queries),
        firmware: rb.revisions.clone(),
        readback: rb,
        meta: meta.unwrap_or(MetaMode::Basic),
    })
}

/// Starts the survey's recovery journal with the preflight readback as the
/// original state. Refuses while an earlier journal is unresolved.
pub fn begin_journal(
    cfg: &Config,
    session_id: &str,
    preflight: &Preflight,
    clock: Arc<dyn Clock>,
) -> Result<Transaction, JournalError> {
    Transaction::begin(
        &cfg.storage.recovery_dir(),
        Entry::Begin {
            session_id: session_id.to_owned(),
            profile: cfg.profile.as_ref().map(|p| p.id.clone()),
            program_version: env!("CARGO_PKG_VERSION").to_owned(),
            meta_mode: preflight.meta,
            original: preflight.readback.to_map(),
        },
        clock,
    )
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
    /// The recovery journal could not be written: no further change may be
    /// made, so no restore either.
    Journal(String),
}

impl Halt {
    fn reason(&self) -> &str {
        match self {
            Halt::Stop(r)
            | Halt::Conflict(r)
            | Halt::CatLost(r)
            | Halt::Storage(r)
            | Halt::NoSpectrum(r)
            | Halt::Journal(r) => r,
        }
    }

    fn may_restore(&self) -> bool {
        matches!(self, Halt::Stop(_) | Halt::Storage(_) | Halt::NoSpectrum(_))
    }
}

impl From<CatError> for Halt {
    fn from(e: CatError) -> Self {
        match e {
            // Refused locally: nothing was sent and the state is known, but
            // the collector cannot continue as planned.
            CatError::Unsafe(_) => Halt::Conflict(format!("collector {e}; nothing was sent")),
            _ => Halt::CatLost(format!("CAT failed ({e}); radio state is unknown")),
        }
    }
}

impl From<JournalError> for Halt {
    fn from(e: JournalError) -> Self {
        Halt::Journal(format!("{e}; no further change was sent"))
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
    /// Duration of the last complete cycle (start to end, before idling).
    pub last_cycle_s: Option<f64>,
    /// Total time spent idling between cycles to honor `min_cycle_ms`.
    pub idle_s: f64,
    /// Tiles skipped because the RX input was not the wanted antenna.
    pub antenna_skips: u64,
    /// Tiles skipped because measurement settings could not be applied.
    pub measurement_skips: u64,
    /// `complete`, `partial`, `unsupported`, `conflict` or `not_restored`.
    pub restore_outcome: String,
    /// Manual steps when restoration was not complete.
    pub manual: Vec<String>,
    pub journal: Option<String>,
}

struct Survey<'a, S, C> {
    cfg: &'a Config,
    profile: &'a SurveyProfile,
    /// Profile antennas in survey order.
    antennas: Vec<AntennaConfig>,
    /// The recovery journal: what was found and changed, per scope.
    txn: &'a mut Transaction,
    meta: MetaMode,
    /// (antenna index, tile index) in visiting order.
    schedule: Vec<(usize, usize)>,
    cycle_started: tokio::time::Instant,
    store: &'a StoreHandle,
    clock: Arc<dyn Clock>,
    session_id: String,
    stop: Stop,
    deadline: tokio::time::Instant,
    spectrum: &'a mut S,
    cat: &'a mut C,
    pan: PanId,
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

    fn band_now(&self) -> Step<u8> {
        self.baseline
            .u64(Query::Band)
            .and_then(|b| u8::try_from(b).ok())
            .ok_or_else(|| Halt::Conflict("band (BN) unreadable".into()))
    }

    /// The scope a setter changes, from the current baseline.
    fn scope_of(&self, s: Setter) -> Step<Scope> {
        Ok(match s {
            Setter::Band(_) => Scope::Session,
            Setter::VfoA(_) | Setter::Span(_) | Setter::FixedTune(_) | Setter::RxInput(_) => {
                Scope::Band(self.band_now()?)
            }
            Setter::Receiver(_) => Scope::Context(self.band_now()?, self.input_now()?),
        })
    }

    fn input_now(&self) -> Step<String> {
        self.baseline
            .raw(Query::RxAntenna)
            .map(str::to_owned)
            .ok_or_else(|| Halt::Conflict("main RX input (AR) unreadable".into()))
    }

    fn dirty(&self, scope: &Scope, field: &str) -> bool {
        self.txn
            .ledger()
            .obligation(scope, field)
            .is_some_and(recovery::Obligation::dirty)
    }

    /// Makes durable, before anything is sent: each touched scope's state
    /// as found (on the first change of a field there) and the intended
    /// changes. Refuses a change whose found value could not be put back.
    fn journal_intent(&mut self, purpose: Purpose, setters: &[Setter]) -> Step<()> {
        let mut changes = Vec::new();
        for s in setters {
            let scope = self.scope_of(*s)?;
            let q = s.query();
            let field = q.prefix();
            if !self.dirty(&scope, field) {
                if purpose == Purpose::Survey
                    && let Some(found) = self.baseline.raw(q)
                    && setter_for(q, self.meta, found).is_none()
                {
                    return Err(Halt::Conflict(format!(
                        "refusing {}: the found {field}{found} could not be put back; nothing was sent",
                        s.wire()
                    )));
                }
                let values = recovery::fields(&scope)
                    .into_iter()
                    .filter_map(|f| self.baseline.raw(f).map(|v| (f.prefix().to_owned(), v.to_owned())))
                    .collect();
                self.txn.record(Entry::Capture {
                    scope: scope.clone(),
                    values,
                })?;
            }
            changes.push(Change {
                scope,
                field: field.to_owned(),
                from: self.baseline.raw(q).map(str::to_owned),
                to: expected_readback(*s),
                wire: s.wire(),
            });
        }
        self.txn.record(Entry::Intent { purpose, changes })?;
        Ok(())
    }

    /// Checks the interlock, journals, sends setters, reads back and
    /// verifies them, and adopts the readback as the new baseline.
    async fn apply(&mut self, setters: &[Setter], purpose: Purpose) -> Step<()> {
        self.check().await?;
        self.journal_intent(purpose, setters)?;
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
                    // A band change recalls that band's memory: mode,
                    // antennas and pan span/track mode (observed on a K4D)
                    // and possibly receiver settings (unverified).
                    may_change.extend([
                        Query::Band,
                        Query::Mode,
                        Query::VfoB,
                        Query::RxAntenna,
                        Query::TxAntenna,
                        Query::Span,
                        Query::FixedTune,
                    ]);
                    may_change.extend(measure::KEYS);
                    rb.u64(Query::Band) == Some(u64::from(n))
                }
                Setter::RxInput(i) => {
                    // Receiver settings may be stored per RX input.
                    may_change.push(Query::RxAntenna);
                    may_change.extend(measure::KEYS);
                    rb.u64(Query::RxAntenna) == Some(u64::from(i.value()))
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
                Setter::Receiver(_) => {
                    let q = s.query();
                    may_change.push(q);
                    rb.raw(q).map(|r| measure::norm(q, r)) == Some(measure::norm(q, &expected_readback(*s)))
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
            // The intent stays in flight, so recovery accepts either value.
            let _ = self.txn.record(Entry::Note {
                message: format!("{msg}; readback {:?}", rb.to_map()),
            });
            self.record(self.event(EventKind::ReadbackMismatch, &msg)).await?;
            return Err(Halt::Conflict(msg));
        }
        self.txn.record(Entry::Verified { state: rb.to_map() })?;
        self.baseline = rb;
        Ok(())
    }

    /// Puts `scope`'s changed fields back to what was found, verified.
    /// Fields that cannot be put back are reported and stay pending in the
    /// journal (the survey's end restore and `k4na recovery` see them).
    async fn put_back(&mut self, scope: &Scope) -> Step<String> {
        let decisions = recovery::decide(self.txn.ledger(), scope, &self.baseline, false);
        let mut back = Vec::new();
        let mut setters = Vec::new();
        for d in decisions {
            match d {
                Decision::Clean { field } => back.push(field),
                Decision::Restore { field, setter } => {
                    back.push(field);
                    setters.push(setter);
                }
                Decision::Unsupported { field } => {
                    let msg = format!(
                        "{}'s found {field} cannot be sent by the collector; left as the survey set it",
                        scope.describe()
                    );
                    let _ = self.record(self.event(EventKind::Warning, msg)).await;
                }
                Decision::Conflict { field, current } => {
                    let msg = format!(
                        "{}'s {field} reads {current}, which the collector did not set; not put back",
                        scope.describe()
                    );
                    let _ = self.record(self.event(EventKind::Warning, msg)).await;
                }
                Decision::Unreadable { field } => {
                    let msg = format!("{}'s {field} is unreadable; not put back", scope.describe());
                    let _ = self.record(self.event(EventKind::Warning, msg)).await;
                }
            }
        }
        let wire: String = setters.iter().map(|s| s.wire()).collect();
        if !setters.is_empty() {
            self.apply(&setters, Purpose::PutBack).await?;
            let msg = format!("put back {}'s own settings: {wire}", scope.describe());
            // Best effort: a storage stop must not prevent restoring.
            let _ = self.record(self.event(EventKind::StateChanged, msg)).await;
        }
        if !back.is_empty() {
            self.txn.record(Entry::Restored {
                scope: scope.clone(),
                fields: back,
            })?;
        }
        Ok(wire)
    }

    /// Track mode and the profile's span for the current band, from the
    /// current readback. A band that recalls a fixed pan without
    /// `set_pan_track_mode` cannot be surveyed.
    fn pan_setup(&self) -> Step<Vec<Setter>> {
        let mut setup = Vec::new();
        match self.baseline.flag(Query::FixedTune) {
            Some(false) => {}
            Some(true) if self.cfg.collection.set_pan_track_mode => setup.push(Setter::FixedTune(false)),
            Some(true) => {
                return Err(Halt::NoSpectrum(format!(
                    "band {} recalled a fixed pan (#FXT1); set collection.set_pan_track_mode = true to survey it",
                    self.baseline.raw(Query::Band).unwrap_or("?")
                )));
            }
            None => return Err(Halt::Conflict("#FXT unreadable; pan tracking unknown".into())),
        }
        if let Some(span) = self.profile.requested_pan_span_hz
            && self.baseline.u64(Query::Span).is_some()
            && self.baseline.u64(Query::Span) != Some(u64::from(span))
            && self.span_restorable()?
        {
            setup.push(Setter::Span(span));
        }
        Ok(setup)
    }

    /// Whether the band's span may be changed: it already was (and so was
    /// found restorable), or the found span has a whitelisted setter.
    fn span_restorable(&self) -> Step<bool> {
        Ok(self.dirty(&Scope::Band(self.band_now()?), "#SPN")
            || self
                .baseline
                .raw(Query::Span)
                .is_some_and(|f| setter_for(Query::Span, self.meta, f).is_some()))
    }

    /// Tunes so the pan is expected to center on `center_hz`. A band
    /// change puts back the old band's memory, selects the new band alone,
    /// records what it recalled, then applies the pan setup with `FA`.
    async fn tune(&mut self, center_hz: u64) -> Step<u64> {
        let fa = round_to_step(center_hz as i64 - self.offset_hz);
        if let Some(n) = k4_band_number(fa)
            && self.baseline.u64(Query::Band) != Some(u64::from(n))
        {
            self.leave_band().await?;
            self.apply(&[Setter::Band(n)], Purpose::Survey).await?;
            if self.profile.requested_pan_span_hz.is_some() && !self.span_restorable()? {
                let msg = format!(
                    "band {} recalled #SPN{}, which the collector cannot put back; surveying at that span",
                    self.baseline.raw(Query::Band).unwrap_or("?"),
                    self.baseline.raw(Query::Span).unwrap_or("?")
                );
                self.record(self.event(EventKind::Warning, msg)).await?;
            }
        }
        let mut setters = self.pan_setup()?;
        setters.push(Setter::VfoA(fa));
        self.apply(&setters, Purpose::Survey).await?;
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
        let settle_end = tokio::time::Instant::now() + Duration::from_millis(self.profile.settle_ms);
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
        let dwell_end = tokio::time::Instant::now() + Duration::from_millis(self.profile.dwell_ms);
        let poll = poll_interval(self.cfg);
        let mut next_poll = tokio::time::Instant::now() + poll;
        loop {
            let now = tokio::time::Instant::now();
            if now >= dwell_end {
                return (v, None);
            }
            if now >= next_poll {
                next_poll = now + poll;
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
            self.cfg.ranges(),
            sample_rate_hz,
            BIN_COUNT as u32,
            self.cfg.collection.usable_fraction,
        )
        .map_err(|e| Halt::NoSpectrum(format!("cannot plan with observed sample rate: {e}")))?;
        let rebuilt = self.plan.is_some();
        self.report.tiles_planned = plan.tiles.len();
        self.schedule = schedule(&plan, self.antennas.len());
        self.plan = Some(plan);
        Ok(rebuilt)
    }

    async fn plan_event(&self, rebuilt: bool) -> Step<()> {
        let Some(p) = &self.plan else { return Ok(()) };
        let s = self.profile;
        let busy = p.cycle_duration(s.settle_ms, s.dwell_ms, 250) * self.antennas.len() as u32;
        let target = Duration::from_millis(s.min_cycle_ms);
        let msg = format!(
            "{} tiles x {} antenna(s) = {} visits/cycle, observed packet rate {} Hz, usable {} Hz; \
             cycle needs about {} s, target start-to-start {} s{}",
            p.tiles.len(),
            self.antennas.len(),
            self.schedule.len(),
            p.sample_rate_hz,
            p.usable_width_hz,
            busy.as_secs(),
            target.as_secs(),
            if busy > target {
                " (cycles will run longer than the target)"
            } else {
                ""
            }
        );
        let kind = if rebuilt {
            EventKind::PlanRebuilt
        } else {
            EventKind::PlanBuilt
        };
        self.record(self.event(kind, msg).count(p.tiles.len() as u64)).await
    }

    /// The main-RX input resolved from the current baseline readback.
    fn observed_port(&self) -> Option<K4Port> {
        K4Port::from_readback(self.baseline.raw(Query::RxAntenna), self.baseline.raw(Query::TxAntenna))
    }

    /// Ensures the main-RX input is `a`. Returns `false`, having changed
    /// nothing, when it is not and switching is not allowed.
    async fn select_antenna(&mut self, a: &AntennaConfig, band: Option<&str>, tile: Option<&str>) -> Step<bool> {
        if self.observed_port() == Some(a.port) {
            return Ok(true);
        }
        let found = format!(
            "AR{} AN{}",
            self.baseline.raw(Query::RxAntenna).unwrap_or("?"),
            self.baseline.raw(Query::TxAntenna).unwrap_or("?")
        );
        if !self.cfg.collection.allow_rx_antenna_switch {
            let msg = format!(
                "main RX input is {} ({found}), not antenna {} on {}; tile skipped \
                 (select it on this band, or set collection.allow_rx_antenna_switch)",
                self.observed_port().map_or("unknown", K4Port::as_str),
                a.id,
                a.port
            );
            self.report.antenna_skips += 1;
            self.record(self.event(EventKind::AntennaMismatch, msg).band(band, tile))
                .await?;
            return Ok(false);
        }
        // The band's found input must be one the collector can put back.
        let band_scope = Scope::Band(self.band_now()?);
        let current = self.input_now()?;
        if !self.dirty(&band_scope, "AR") && setter_for(Query::RxAntenna, self.meta, &current).is_none() {
            let msg = format!(
                "main RX input is AR{current}, which the collector cannot select again afterwards; antenna {} \
                 tile skipped",
                a.id
            );
            self.report.antenna_skips += 1;
            self.record(self.event(EventKind::AntennaMismatch, msg).band(band, tile))
                .await?;
            return Ok(false);
        }
        // Receiver settings may be stored per input: put this input's back
        // before leaving it.
        self.put_back(&Scope::Context(self.band_now()?, current)).await?;
        let setter = Setter::RxInput(RxInput::for_port(a.port));
        self.apply(&[setter], Purpose::Survey).await?;
        if self.observed_port() != Some(a.port) {
            let msg = format!("{} did not select port {}", setter.wire(), a.port);
            self.record(self.event(EventKind::ReadbackMismatch, &msg)).await?;
            return Err(Halt::Conflict(msg));
        }
        let msg = format!(
            "selected antenna {} ({}) with {} (was {found})",
            a.id,
            a.port,
            setter.wire()
        );
        self.record(self.event(EventKind::AntennaSelected, msg).band(band, tile))
            .await?;
        Ok(true)
    }

    /// Puts back the current RX input's receiver settings, then the band's
    /// own `FA`/`#SPN`/`#FXT`/`AR` as found, so the radio's memories are
    /// unchanged. Called before leaving a band and before idling.
    async fn leave_band(&mut self) -> Step<String> {
        let band = self.band_now()?;
        let mut wire = self.put_back(&Scope::Context(band, self.input_now()?)).await?;
        wire.push_str(&self.put_back(&Scope::Band(band)).await?);
        Ok(wire)
    }

    /// Applies and verifies the profile's measurement settings for antenna
    /// `a` on the current band. `None` (nothing changed) when this context
    /// cannot be measured as configured.
    async fn apply_measurement(
        &mut self,
        a: &AntennaConfig,
        band: Option<&str>,
        tile: Option<&str>,
    ) -> Step<Option<MeasurementSettings>> {
        let k4_band = self.baseline.u64(Query::Band).and_then(|b| u8::try_from(b).ok());
        let settings = self.profile.measurement.resolve(k4_band, &a.id);
        match measure::plan(self.meta, &settings, k4_band, &self.baseline) {
            Ok(changes) if changes.is_empty() => Ok(Some(settings)),
            Ok(changes) => {
                let setters: Vec<Setter> = changes.iter().map(|c| c.setter).collect();
                self.apply(&setters, Purpose::Survey).await?;
                let msg = format!(
                    "measurement settings for {} on {}: {}",
                    a.id,
                    band_label(k4_band),
                    changes
                        .iter()
                        .map(|c| format!("{}{} -> {}", c.query.prefix(), c.found, c.target))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                self.record(self.event(EventKind::MeasurementApplied, msg).band(band, tile))
                    .await?;
                Ok(Some(settings))
            }
            Err(why) => {
                self.report.measurement_skips += 1;
                let msg = format!(
                    "antenna {} on {} cannot be measured as configured ({settings}): {why}; tile skipped, nothing \
                     changed",
                    a.id,
                    band_label(k4_band)
                );
                self.record(self.event(EventKind::MeasurementSkipped, msg).band(band, tile))
                    .await?;
                Ok(None)
            }
        }
    }

    /// Tunes for a center, selects the antenna and settles; up to
    /// `max_tile_corrections` learned-offset retunes if the observed usable
    /// span misses `target`. `None` if the antenna could not be selected.
    async fn tune_and_settle(
        &mut self,
        center_hz: u64,
        target: Option<&HzRange>,
        antenna: Option<&AntennaConfig>,
        band: Option<&str>,
        tile: Option<&str>,
    ) -> Step<Option<(u64, Settled, MeasurementSettings)>> {
        let mut fa = self.tune(center_hz).await?;
        let Some(settings) = self.prepare_context(antenna, band, tile).await? else {
            return Ok(None);
        };
        let mut settled = self.settle().await?;
        let mut corrections = 0;
        while let Some(g) = settled.geometry {
            self.offset_hz = g.center_hz as i64 - fa as i64;
            let Some(target) = target else { break };
            if g.usable_range(self.cfg.collection.usable_fraction).covers(target)
                || corrections >= self.cfg.collection.max_tile_corrections
            {
                break;
            }
            corrections += 1;
            let msg = format!(
                "pan center {} Hz with FA {} Hz misses target {}..{} Hz; retuning with learned offset {} Hz",
                g.center_hz, fa, target.low_hz, target.high_hz, self.offset_hz
            );
            self.record(self.event(EventKind::RetuneCorrection, msg).band(band, tile))
                .await?;
            fa = self.tune(center_hz).await?;
            if self.prepare_context(antenna, band, tile).await?.is_none() {
                return Ok(None);
            }
            let again = self.settle().await?;
            settled = Settled {
                geometry: again.geometry,
                discarded: settled.discarded + again.discarded,
            };
        }
        Ok(Some((fa, settled, settings)))
    }

    /// Selects the antenna and applies its measurement settings. `None`
    /// when the tile must be skipped. Without an antenna (the geometry
    /// probe, which is not stored) the radio is left as found.
    async fn prepare_context(
        &mut self,
        antenna: Option<&AntennaConfig>,
        band: Option<&str>,
        tile: Option<&str>,
    ) -> Step<Option<MeasurementSettings>> {
        let Some(a) = antenna else {
            return Ok(Some(MeasurementSettings::as_found()));
        };
        if !self.select_antenna(a, band, tile).await? {
            return Ok(None);
        }
        self.apply_measurement(a, band, tile).await
    }

    /// Holds the current tile without retuning until `until`, still
    /// polling the interlock and flushing storage. Frames are not stored.
    async fn idle(&mut self, until: tokio::time::Instant) -> Step<()> {
        let poll = poll_interval(self.cfg).max(Duration::from_secs(2));
        let mut next_check = tokio::time::Instant::now() + poll;
        loop {
            let now = tokio::time::Instant::now();
            if now >= until {
                return Ok(());
            }
            if now >= next_check {
                next_check = now + poll;
                self.check().await?;
                self.maybe_tick().await?;
            }
            match self.next_event(until.min(next_check)).await? {
                None => {}
                Some(SpectrumEvent::Frame(rf)) => {
                    if !self.note_ignored(&rf) {
                        self.on_reconnect(rf.at).await?;
                    }
                }
                Some(SpectrumEvent::Connected) => {
                    self.record(self.event(EventKind::SpectrumConnected, "spectrum stream connected"))
                        .await?;
                }
                Some(SpectrumEvent::Disconnected { reason, stalled }) => self.on_disconnect(reason, stalled).await?,
            }
        }
    }

    /// Records a finished cycle and waits out the rest of `min_cycle_ms`.
    async fn end_cycle(&mut self) -> Step<()> {
        let elapsed = self.cycle_started.elapsed();
        let target = Duration::from_millis(self.profile.min_cycle_ms);
        self.report.cycles += 1;
        self.report.last_cycle_s = Some(elapsed.as_secs_f64());
        let wait = target.saturating_sub(elapsed);
        let msg = format!(
            "cycle {} took {:.1} s (target {} s); {}",
            self.report.cycles,
            elapsed.as_secs_f64(),
            target.as_secs(),
            if wait.is_zero() {
                "next cycle starts now".to_owned()
            } else {
                format!("holding {:.1} s before the next", wait.as_secs_f64())
            }
        );
        self.record(self.event(EventKind::CycleComplete, msg).count(self.report.cycles))
            .await?;
        if !wait.is_zero() {
            self.leave_band().await?;
            let start = tokio::time::Instant::now();
            self.idle(start + wait).await?;
            self.report.idle_s += start.elapsed().as_secs_f64();
        }
        self.cycle_started = tokio::time::Instant::now();
        Ok(())
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

    async fn finish_visit(
        &mut self,
        mut v: VisitBuilder,
        tile: &Tile,
        antenna: VisitAntenna,
        halt: Option<&Halt>,
    ) -> Step<()> {
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
        if !antenna.verified() {
            // The interlock watches AR/AN, so this means the dwell's state
            // was inconsistent; never attribute it as verified.
            v.flag("antenna_unverified");
            status = VisitStatus::Aborted;
        }
        if let Some(record) = v.finish(end, status, &reason, antenna, state) {
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

    async fn visit_tile(&mut self, tile: &Tile, antenna: &AntennaConfig) -> Step<()> {
        let tile_id = tile.id();
        let started = self.now();
        let Some((fa, settled, settings)) = self
            .tune_and_settle(
                tile.center_hz,
                Some(&tile.target),
                Some(antenna),
                Some(&tile.band_id),
                Some(&tile_id),
            )
            .await?
        else {
            self.report.coverage_unverified += 1;
            return Ok(());
        };
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
        v.settle_ms = self.profile.settle_ms;
        v.settle_discarded = settled.discarded;
        v.measurement = measure::visit_settings(&settings, self.meta);
        let visit_antenna = VisitAntenna::configured(antenna, self.observed_port());
        let (v, halt) = self.dwell(v).await;
        // Confirm state at the end of the dwell before calling it complete.
        let halt = match halt {
            Some(h) => Some(h),
            None => self.check().await.err(),
        };
        self.finish_visit(v, tile, visit_antenna, halt.as_ref()).await?;
        match halt {
            Some(h) => Err(h),
            None => Ok(()),
        }
    }

    async fn run(&mut self) -> Step<()> {
        // Each scope's found state is journaled on its first change.
        // Preflight refused a fixed pan unless set_pan_track_mode allows it.
        // Probe the observed packet geometry at the first band's center.
        let first = self
            .cfg
            .ranges()
            .iter()
            .min_by_key(|b| b.low_hz)
            .ok_or_else(|| Halt::NoSpectrum("no ranges".into()))?;
        let center = first.range().midpoint();
        // The geometry probe is not stored, so any antenna will do.
        let settled = self
            .tune_and_settle(center, None, None, Some(&first.id), None)
            .await?
            .map(|(_, s, _)| s);
        let Some(g) = settled.and_then(|s| s.geometry) else {
            return Err(Halt::NoSpectrum(
                "no spectrum frames after tuning; coverage cannot be verified".into(),
            ));
        };
        self.ensure_plan(g.sample_rate_hz)?;
        self.plan_event(false).await?;

        let mut pos = 0usize;
        self.cycle_started = tokio::time::Instant::now();
        loop {
            self.time_left()?;
            if self.spectrum_down_since.is_some() {
                self.wait_for_spectrum().await?;
            }
            if pos >= self.schedule.len() {
                pos = 0;
                self.end_cycle().await?;
            }
            let plan = self.plan.as_ref().ok_or_else(|| Halt::NoSpectrum("no plan".into()))?;
            let (ant, tile_idx) = self.schedule[pos];
            let tile = plan.tiles[tile_idx].clone();
            let rate = plan.sample_rate_hz;
            let antenna = self.antennas[ant].clone();
            self.visit_tile(&tile, &antenna).await?;
            self.maybe_tick().await?;
            let plan = self.plan.as_ref().ok_or_else(|| Halt::NoSpectrum("no plan".into()))?;
            if plan.sample_rate_hz != rate {
                // Resume on the nearest tile of the same band and antenna.
                let idx = plan.tile_matching(&tile.band_id, tile.center_hz).map_or(0, |t| t.index);
                pos = self.schedule.iter().position(|e| *e == (ant, idx)).unwrap_or(0);
            } else {
                pos += 1;
            }
        }
    }

    /// Ends the transaction: restores through the shared engine when the
    /// halt allows it, else journals why not and what is left to do.
    async fn restore(&mut self, halt: &Halt) -> String {
        if !halt.may_restore() {
            let ledger = self.txn.ledger();
            let (outcome, msg) = if ledger.is_dirty() {
                (Outcome::NotRestored, format!("not restoring: {}", halt.reason()))
            } else {
                (
                    Outcome::Complete,
                    format!(
                        "not restoring: {}; nothing the collector changed remains",
                        halt.reason()
                    ),
                )
            };
            let manual = recovery::manual_steps(ledger);
            let _ = self.txn.record(Entry::End {
                outcome,
                message: msg.clone(),
                manual: manual.clone(),
            });
            self.report.restore_outcome = outcome.as_str().to_owned();
            self.report.manual = manual;
            let _ = self.record(self.event(EventKind::RestoreSkipped, &msg)).await;
            return msg;
        }
        let r = recovery::restore(self.txn, self.cat).await;
        self.report.restore_outcome = r.outcome.as_str().to_owned();
        self.report.manual = r.manual.clone();
        let msg = match r.outcome {
            Outcome::Complete => r.message,
            o => format!("restore {}: {}", o.as_str(), r.message),
        };
        let kind = if r.outcome == Outcome::Complete {
            EventKind::RestoreDone
        } else {
            EventKind::RestoreSkipped
        };
        let _ = self.record(self.event(kind, &msg)).await;
        msg
    }
}

/// Visiting order: each band's run of tiles once per antenna (all tiles on
/// the first antenna, then all on the next), so antennas are compared
/// minutes apart and the RX input changes at most once per antenna per
/// band.
fn schedule(plan: &SurveyPlan, antennas: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < plan.cycle.len() {
        let band = &plan.tiles[plan.cycle[i]].band_id;
        let mut j = i;
        while j < plan.cycle.len() && &plan.tiles[plan.cycle[j]].band_id == band {
            j += 1;
        }
        for a in 0..antennas {
            out.extend(plan.cycle[i..j].iter().map(|t| (a, *t)));
        }
        i = j;
    }
    out
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
/// must hold a [`ControlCat`](crate::cat::ControlCat), which proves consent,
/// and the journal from [`begin_journal`] for this session.
pub async fn run_survey<S: SpectrumSource, C: CatControl>(
    ctx: RunContext<'_>,
    spectrum: &mut S,
    cat: &mut C,
    preflight: Preflight,
    txn: &mut Transaction,
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
    let profile = cfg
        .profile
        .as_ref()
        .ok_or_else(|| RunError::Config("a survey needs a survey profile".into()))?;
    let antennas: Vec<AntennaConfig> = cfg.survey_antennas().into_iter().cloned().collect();
    if antennas.is_empty() {
        return Err(RunError::Config(
            "the survey profile lists no registered antenna".into(),
        ));
    }
    if txn.ledger().session_id != session_id || txn.ledger().meta != Some(preflight.meta) {
        return Err(RunError::Config(
            "the recovery journal does not belong to this session's preflight".into(),
        ));
    }
    let journal = txn.path().map(|p| p.display().to_string());
    let mut s = Survey {
        cfg,
        profile,
        antennas,
        txn,
        meta: preflight.meta,
        schedule: Vec::new(),
        cycle_started: tokio::time::Instant::now(),
        store,
        clock: clock.clone(),
        session_id,
        stop,
        deadline: tokio::time::Instant::now() + duration,
        spectrum,
        cat,
        pan: cfg.radio.receiver.pan(),
        baseline: preflight.readback,
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
            last_cycle_s: None,
            idle_s: 0.0,
            antenna_skips: 0,
            measurement_skips: 0,
            restore_outcome: String::new(),
            manual: Vec::new(),
            journal,
        },
    };
    store
        .event(s.event(
            EventKind::SessionStart,
            format!(
                "authorized survey for {} s with profile {:?} on antenna(s) {}; setters limited to \
                 BN/FA/#SPN/#FXT{} and receiver measurement settings; CAT meta-mode {}; journal {}",
                duration.as_secs(),
                profile.id,
                profile.antennas.join(", "),
                if cfg.collection.allow_rx_antenna_switch {
                    "/AR"
                } else {
                    ""
                },
                preflight.meta,
                s.report.journal.as_deref().unwrap_or("in memory")
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
        Halt::Journal(_) => "journal",
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
