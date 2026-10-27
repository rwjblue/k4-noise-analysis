//! Runs the synchronous [`Store`] on a dedicated thread. Summary and event
//! writes are acknowledged (the collector waits for durability before the
//! next visit); sampled raw frames use a small bounded queue and are dropped
//! and counted when it is full, never blocking acquisition.
//!
//! [`StoreHandle::inline`] runs the store on the calling task instead, for
//! deterministic paused-time simulations.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use k4na_core::model::{Event, RawFrameRecord, VisitRecord};
use k4na_core::time::Timestamp;
use k4na_store::{Store, StoreError, StoreStatus};
use tokio::sync::oneshot;

pub const RAW_QUEUE: usize = 32;

enum Cmd {
    Visit(Box<VisitRecord>, Vec<Event>, oneshot::Sender<Result<(), StoreError>>),
    Events(Vec<Event>, Timestamp, oneshot::Sender<Result<(), StoreError>>),
    Tick(Timestamp, oneshot::Sender<Result<(), StoreError>>),
    Close(Timestamp, String, oneshot::Sender<Result<StoreStatus, StoreError>>),
}

#[derive(Debug, Clone)]
enum Backend {
    Thread {
        cmd: mpsc::Sender<Cmd>,
        raw: mpsc::SyncSender<RawFrameRecord>,
    },
    Inline(Arc<Mutex<Option<Store>>>),
}

#[derive(Debug, Clone)]
pub struct StoreHandle {
    backend: Backend,
    raw_queue_drops: Arc<AtomicU64>,
    status: Arc<Mutex<StoreStatus>>,
}

impl std::fmt::Debug for Cmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Cmd::Visit(..) => "Visit",
            Cmd::Events(..) => "Events",
            Cmd::Tick(..) => "Tick",
            Cmd::Close(..) => "Close",
        })
    }
}

fn closed() -> StoreError {
    StoreError::Io(std::io::Error::other("store writer has stopped"))
}

impl StoreHandle {
    pub fn spawn(store: Store) -> (Self, std::thread::JoinHandle<()>) {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (raw_tx, raw_rx) = mpsc::sync_channel::<RawFrameRecord>(RAW_QUEUE);
        let status = Arc::new(Mutex::new(store.status().clone()));
        let shared = status.clone();
        let join = std::thread::Builder::new()
            .name("k4na-store".into())
            .spawn(move || writer_loop(store, cmd_rx, raw_rx, shared))
            .expect("spawn store writer thread");
        let backend = Backend::Thread {
            cmd: cmd_tx,
            raw: raw_tx,
        };
        (
            Self {
                backend,
                raw_queue_drops: Arc::new(AtomicU64::new(0)),
                status,
            },
            join,
        )
    }

    pub fn inline(store: Store) -> Self {
        let status = Arc::new(Mutex::new(store.status().clone()));
        Self {
            backend: Backend::Inline(Arc::new(Mutex::new(Some(store)))),
            raw_queue_drops: Arc::new(AtomicU64::new(0)),
            status,
        }
    }

    fn with_inline<T>(
        &self,
        store: &Arc<Mutex<Option<Store>>>,
        f: impl FnOnce(&mut Store) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        let mut g = store.lock().map_err(|_| closed())?;
        let s = g.as_mut().ok_or_else(closed)?;
        let r = f(s);
        if let Ok(mut st) = self.status.lock() {
            *st = s.status().clone();
        }
        r
    }

    pub fn status(&self) -> StoreStatus {
        let mut s = self.status.lock().map(|s| s.clone()).unwrap_or_default();
        s.raw_dropped += self.raw_queue_drops.load(Ordering::Relaxed);
        s
    }

    pub fn raw_paused(&self) -> bool {
        self.status
            .lock()
            .map(|s| s.raw_paused || s.suspended.is_some())
            .unwrap_or(true)
    }

    /// Persists a visit and its events; resolves once committed.
    pub async fn write_visit(&self, visit: VisitRecord, events: Vec<Event>) -> Result<(), StoreError> {
        match &self.backend {
            Backend::Thread { cmd, .. } => {
                let (tx, rx) = oneshot::channel();
                cmd.send(Cmd::Visit(Box::new(visit), events, tx))
                    .map_err(|_| closed())?;
                rx.await.map_err(|_| closed())?
            }
            Backend::Inline(s) => self.with_inline(s, |s| s.write_visit(&visit, &events)),
        }
    }

    pub async fn write_events(&self, events: Vec<Event>, now: Timestamp) -> Result<(), StoreError> {
        match &self.backend {
            Backend::Thread { cmd, .. } => {
                let (tx, rx) = oneshot::channel();
                cmd.send(Cmd::Events(events, now, tx)).map_err(|_| closed())?;
                rx.await.map_err(|_| closed())?
            }
            Backend::Inline(s) => self.with_inline(s, |s| s.write_events(&events, now)),
        }
    }

    pub async fn event(&self, event: Event) -> Result<(), StoreError> {
        let at = event.at;
        self.write_events(vec![event], at).await
    }

    /// Rotation and quota housekeeping at collector time `now`.
    pub async fn tick(&self, now: Timestamp) -> Result<(), StoreError> {
        match &self.backend {
            Backend::Thread { cmd, .. } => {
                let (tx, rx) = oneshot::channel();
                cmd.send(Cmd::Tick(now, tx)).map_err(|_| closed())?;
                rx.await.map_err(|_| closed())?
            }
            Backend::Inline(s) => self.with_inline(s, |s| s.maintain(now)),
        }
    }

    /// Offers a sampled raw frame; returns false if it was dropped.
    pub fn offer_raw(&self, raw: RawFrameRecord) -> bool {
        match &self.backend {
            Backend::Thread { raw: tx, .. } => match tx.try_send(raw) {
                Ok(()) => true,
                Err(_) => {
                    self.raw_queue_drops.fetch_add(1, Ordering::Relaxed);
                    false
                }
            },
            Backend::Inline(s) => matches!(
                self.with_inline(s, |s| s.write_raw(&raw)),
                Ok(k4na_store::RawOutcome::Written)
            ),
        }
    }

    pub async fn close(&self, end: Timestamp, reason: &str) -> Result<StoreStatus, StoreError> {
        let mut status = match &self.backend {
            Backend::Thread { cmd, .. } => {
                let (tx, rx) = oneshot::channel();
                cmd.send(Cmd::Close(end, reason.to_owned(), tx)).map_err(|_| closed())?;
                rx.await.map_err(|_| closed())??
            }
            Backend::Inline(s) => {
                let store = s.lock().map_err(|_| closed())?.take().ok_or_else(closed)?;
                store.close(end, reason)?
            }
        };
        status.raw_dropped += self.raw_queue_drops.load(Ordering::Relaxed);
        Ok(status)
    }
}

fn writer_loop(
    store: Store,
    cmd_rx: mpsc::Receiver<Cmd>,
    raw_rx: mpsc::Receiver<RawFrameRecord>,
    status: Arc<Mutex<StoreStatus>>,
) {
    let mut store = Some(store);
    loop {
        let cmd = cmd_rx.recv_timeout(Duration::from_millis(100));
        let Some(s) = store.as_mut() else { break };
        while let Ok(raw) = raw_rx.try_recv() {
            if let Err(e) = s.write_raw(&raw) {
                tracing::warn!("raw frame not stored: {e}");
            }
        }
        match cmd {
            Ok(Cmd::Visit(v, events, ack)) => {
                let _ = ack.send(s.write_visit(&v, &events));
            }
            Ok(Cmd::Events(events, now, ack)) => {
                let _ = ack.send(s.write_events(&events, now));
            }
            Ok(Cmd::Tick(now, ack)) => {
                let _ = ack.send(s.maintain(now));
            }
            Ok(Cmd::Close(end, reason, ack)) => {
                if let Some(s) = store.take() {
                    let _ = ack.send(s.close(end, &reason));
                }
                break;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Collector dropped without closing: finalize best-effort.
                if let Some(s) = store.take() {
                    let now = Timestamp {
                        utc_ns: k4na_core::time::system_utc_ns(),
                        mono_ns: 0,
                    };
                    let _ = s.close(now, "collector exited without closing the store");
                }
                break;
            }
        }
        if let (Some(s), Ok(mut g)) = (store.as_ref(), status.lock()) {
            *g = s.status().clone();
        }
    }
}
