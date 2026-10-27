//! Passive capture: listen to :9201 only and never change the radio.
//!
//! Visits are fixed windows (`collection.passive_visit_ms`) of stable
//! observed geometry. Whatever spectrum the operator has selected is
//! recorded as observed; passive capture never claims band coverage.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use k4na_core::Config;
use k4na_core::aggregate::PushOutcome;
use k4na_core::model::{Event, EventKind, RadioState, VisitStatus};
use k4na_core::time::{Clock, NS_PER_SEC, Timestamp};
use k4na_protocol::{DecoderDiagnostics, PanId};
use k4na_store::StoreError;

use crate::cat::{CatQuery, Query, ReadOnlyCat};
use crate::net::Connector;
use crate::runtime::{RunContext, Stop};
use crate::spectrum::{ReceivedFrame, SpectrumEvent, SpectrumSource};
use crate::store_task::StoreHandle;
use crate::visit::{RawSampler, VisitBuilder};

/// Latest GET-only CAT readback shared with the capture loop.
#[derive(Debug, Default, Clone)]
pub struct CatStateCache {
    pub state: RadioState,
    pub available: bool,
    pub last_error: Option<String>,
}

pub type SharedCatState = Arc<Mutex<CatStateCache>>;

/// Periodically reads GET-only state on a [`ReadOnlyCat`]. It is not
/// possible to send a setter through this task.
pub async fn poll_cat_state<C: Connector>(
    connector: C,
    timeout: Duration,
    every: Duration,
    cache: SharedCatState,
    mut stop: Stop,
) {
    let mut queries: Vec<Query> = Query::STATE.to_vec();
    queries.push(Query::Transmit);
    loop {
        let result = async {
            let stream = connector.connect().await?;
            let mut cat = ReadOnlyCat::new(stream, timeout);
            loop {
                let rb = cat
                    .query(&queries)
                    .await
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                if let Ok(mut c) = cache.lock() {
                    c.state = rb.to_state(&queries);
                    c.available = true;
                    c.last_error = None;
                }
                tokio::time::sleep(every).await;
            }
            #[allow(unreachable_code)]
            Ok::<(), std::io::Error>(())
        };
        tokio::select! {
            _ = stop.requested() => return,
            r = result => {
                if let (Err(e), Ok(mut c)) = (r, cache.lock()) {
                    c.available = false;
                    c.last_error = Some(e.to_string());
                    c.state.values_mut().for_each(|v| *v = "unknown".into());
                }
            }
        }
        tokio::select! {
            _ = stop.requested() => return,
            _ = tokio::time::sleep(every.max(Duration::from_secs(5))) => {}
        }
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct RunSummary {
    pub end_reason: String,
    pub failed: bool,
    pub visits: u64,
    pub accepted_frames: u64,
    pub ignored_frames: BTreeMap<String, u64>,
    pub gaps: u64,
    pub decoder: DecoderDiagnostics,
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("storage: {0}")]
    Store(#[from] StoreError),
}

struct Passive<'a> {
    cfg: &'a Config,
    store: &'a StoreHandle,
    clock: Arc<dyn Clock>,
    session_id: String,
    pan: PanId,
    window: Duration,
    cat: Option<SharedCatState>,
    sampler: RawSampler,
    policy: String,
    visit: Option<VisitBuilder>,
    last_frame: Option<Timestamp>,
    disconnected_since: Option<Timestamp>,
    diag_at_last_visit: DecoderDiagnostics,
    summary: RunSummary,
    cat_unavailable_reported: bool,
}

impl Passive<'_> {
    fn state(&mut self) -> RadioState {
        let Some(cache) = &self.cat else {
            return RadioState::new();
        };
        let c = cache.lock().map(|c| c.clone()).unwrap_or_default();
        let mut s = c.state;
        s.insert(
            "source".into(),
            if c.available {
                "cat-get-async".into()
            } else {
                "unavailable".into()
            },
        );
        s
    }

    async fn finish_visit(
        &mut self,
        end: Timestamp,
        status_if_window: VisitStatus,
        reason: &str,
        diag: DecoderDiagnostics,
    ) -> Result<(), RunError> {
        let Some(mut v) = self.visit.take() else { return Ok(()) };
        let mut status = status_if_window;
        if v.frames() < self.cfg.collection.min_frames_per_visit {
            status = VisitStatus::Partial;
            v.flag("too_few_frames");
        }
        if let Some(g) = v.geometry() {
            v.band_id = self.cfg.band_containing(g.center_hz).map(|b| b.id.clone());
        }
        let mut events = Vec::new();
        let delta = diag.since(&self.diag_at_last_visit);
        if delta.rejected_candidates() > 0 || delta.discarded_bytes > 0 {
            events.push(
                Event::new(
                    &self.session_id,
                    end,
                    EventKind::DecoderRejects,
                    format!("decoder rejected candidates during visit: {delta:?}"),
                )
                .count(delta.rejected_candidates()),
            );
        }
        self.diag_at_last_visit = diag;
        if let Some(cache) = &self.cat {
            let unavailable = cache
                .lock()
                .map(|c| (!c.available).then(|| c.last_error.clone().unwrap_or_default()))
                .unwrap_or(None);
            if let (Some(err), false) = (&unavailable, self.cat_unavailable_reported) {
                events.push(Event::new(
                    &self.session_id,
                    end,
                    EventKind::CatUnavailable,
                    format!("GET-only CAT readback unavailable (capture continues): {err}"),
                ));
                self.cat_unavailable_reported = true;
            } else if unavailable.is_none() {
                self.cat_unavailable_reported = false;
            }
        }
        let state = self.state();
        let frames = v.frames() as u64;
        if let Some(record) = v.finish(end, status, reason, state) {
            self.summary.visits += 1;
            self.summary.accepted_frames += frames;
            self.store.write_visit(record, events).await?;
        } else if !events.is_empty() {
            self.store.write_events(events, end).await?;
        }
        Ok(())
    }

    fn new_visit(&self, at: Timestamp) -> VisitBuilder {
        VisitBuilder::new(
            &self.session_id,
            self.pan,
            at,
            self.cfg.collection.usable_fraction,
            self.cfg.collection.max_frames_per_visit,
        )
    }

    async fn on_frame(&mut self, rf: ReceivedFrame, diag: DecoderDiagnostics) -> Result<(), RunError> {
        if rf.frame.pan() != self.pan {
            *self
                .summary
                .ignored_frames
                .entry(rf.frame.pan().to_string())
                .or_default() += 1;
            return Ok(());
        }
        if let Some(since) = self.disconnected_since.take() {
            self.summary.gaps += 1;
            let gap = Event::new(
                &self.session_id,
                rf.at,
                EventKind::Gap,
                "no spectrum while disconnected",
            )
            .interval(since.utc_ns, rf.at.utc_ns);
            self.store.event(gap).await?;
        }
        if let Some(prev) = self.last_frame {
            let skew = rf.at.clock_skew_ns(&prev);
            if skew.abs() > 2 * NS_PER_SEC {
                self.store
                    .event(Event::new(
                        &self.session_id,
                        rf.at,
                        EventKind::ClockJump,
                        format!("UTC moved {} ms relative to monotonic time", skew / 1_000_000),
                    ))
                    .await?;
                if let Some(v) = self.visit.as_mut() {
                    v.flag("clock_jump");
                }
                self.finish_visit(prev, VisitStatus::Partial, "host clock jump", diag)
                    .await?;
            }
        }
        self.last_frame = Some(rf.at);

        // Close the window, then start a new visit with this frame.
        if let Some(v) = &self.visit
            && let Some(first) = v.first_frame()
            && rf.at.mono_since(&first) >= self.window
        {
            let end = v.last_frame().unwrap_or(rf.at);
            self.finish_visit(end, VisitStatus::Complete, "window complete", diag)
                .await?;
        }
        let visit = self.visit.get_or_insert_with(|| {
            VisitBuilder::new(
                &self.session_id,
                self.pan,
                rf.at,
                self.cfg.collection.usable_fraction,
                self.cfg.collection.max_frames_per_visit,
            )
        });
        match visit.accept(&rf) {
            PushOutcome::Accepted => {}
            outcome => {
                let (status, reason, flag) = match outcome {
                    PushOutcome::Full => (VisitStatus::Complete, "frame cap reached", "frame_cap"),
                    _ => (
                        VisitStatus::Partial,
                        "pan geometry changed (operator retune or span change)",
                        "geometry_changed",
                    ),
                };
                visit.rejected = visit.rejected.saturating_sub(1);
                visit.flag(flag);
                let end = visit.last_frame().unwrap_or(rf.at);
                self.finish_visit(end, status, reason, diag).await?;
                let mut v = self.new_visit(rf.at);
                v.accept(&rf);
                self.visit = Some(v);
            }
        }
        if self.sampler.due(rf.at)
            && !self.store.raw_paused()
            && let Some(v) = &self.visit
        {
            self.store.offer_raw(v.raw_record(&rf, &self.policy));
        }
        Ok(())
    }
}

/// Runs passive capture until `duration`, a stop request or a storage
/// suspension. Never opens a CAT write path.
pub async fn run_passive<S: SpectrumSource>(
    ctx: RunContext<'_>,
    spectrum: &mut S,
    cat: Option<SharedCatState>,
    duration: Duration,
) -> Result<RunSummary, RunError> {
    let RunContext {
        cfg,
        store,
        clock,
        session_id,
        mut stop,
    } = ctx;
    let session_id = session_id.as_str();
    let sampler = RawSampler::new(cfg.collection.raw_sample_interval_ms);
    let policy = sampler.policy();
    let mut p = Passive {
        cfg,
        store,
        clock: clock.clone(),
        session_id: session_id.to_owned(),
        pan: cfg.radio.receiver.pan(),
        window: Duration::from_millis(cfg.collection.passive_visit_ms),
        cat,
        sampler,
        policy,
        visit: None,
        last_frame: None,
        disconnected_since: Some(clock.now()),
        diag_at_last_visit: DecoderDiagnostics::default(),
        summary: RunSummary::default(),
        cat_unavailable_reported: false,
    };
    let start = clock.now();
    store
        .event(Event::new(
            session_id,
            start,
            EventKind::SessionStart,
            format!(
                "passive capture of pan {} for {} s; no CAT setters",
                p.pan,
                duration.as_secs()
            ),
        ))
        .await?;
    let deadline = tokio::time::Instant::now() + duration;
    let mut last_tick = tokio::time::Instant::now();
    let reason = loop {
        let ev = tokio::select! {
            r = stop.requested() => break r,
            _ = tokio::time::sleep_until(deadline) => break "duration reached".to_owned(),
            ev = spectrum.next() => ev,
        };
        let diag = spectrum.diagnostics();
        let result = match ev {
            SpectrumEvent::Connected => store
                .event(Event::new(
                    session_id,
                    p.clock.now(),
                    EventKind::SpectrumConnected,
                    "spectrum stream connected",
                ))
                .await
                .map_err(RunError::from),
            SpectrumEvent::Disconnected { reason, stalled } => {
                let now = p.clock.now();
                let end = p.visit.as_ref().and_then(VisitBuilder::last_frame).unwrap_or(now);
                if let Some(v) = p.visit.as_mut() {
                    v.flag("spectrum_lost");
                }
                let r = p
                    .finish_visit(end, VisitStatus::Partial, "spectrum stream lost", diag)
                    .await;
                if p.disconnected_since.is_none() {
                    p.disconnected_since = Some(end);
                }
                let kind = if stalled {
                    EventKind::SpectrumStalled
                } else {
                    EventKind::SpectrumDisconnected
                };
                match r {
                    Ok(()) => store
                        .event(Event::new(session_id, now, kind, reason))
                        .await
                        .map_err(RunError::from),
                    Err(e) => Err(e),
                }
            }
            SpectrumEvent::Frame(rf) => p.on_frame(rf, diag).await,
        };
        if let Err(RunError::Store(e)) = result {
            p.summary.failed = true;
            break format!("storage stopped collection: {e}");
        }
        if last_tick.elapsed() >= Duration::from_secs(1) {
            last_tick = tokio::time::Instant::now();
            if let Err(e) = store.tick(p.clock.now()).await {
                p.summary.failed = true;
                break format!("storage stopped collection: {e}");
            }
        }
        if let Some(s) = store.status().suspended {
            p.summary.failed = true;
            break format!("storage stopped collection: {s}");
        }
    };
    let end = p.clock.now();
    let diag = spectrum.diagnostics();
    if !p.summary.failed {
        let last = p.visit.as_ref().and_then(VisitBuilder::last_frame).unwrap_or(end);
        p.finish_visit(last, VisitStatus::Partial, &format!("stopped: {reason}"), diag)
            .await
            .ok();
        if let Some(since) = p.disconnected_since {
            p.summary.gaps += 1;
            store
                .event(
                    Event::new(session_id, end, EventKind::Gap, "no spectrum until session end")
                        .interval(since.utc_ns, end.utc_ns),
                )
                .await
                .ok();
        }
        if !p.summary.ignored_frames.is_empty() {
            let msg = format!("frames from other pans were not stored: {:?}", p.summary.ignored_frames);
            store
                .event(Event::new(session_id, end, EventKind::Warning, msg))
                .await
                .ok();
        }
    }
    p.summary.end_reason = reason;
    p.summary.decoder = diag;
    Ok(p.summary)
}
