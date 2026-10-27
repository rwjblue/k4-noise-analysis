//! Clock and stop signalling for collection runs.

use std::sync::Arc;

use k4na_core::Config;
use k4na_core::time::{Clock, Timestamp, system_utc_ns};

use crate::store_task::StoreHandle;

/// Everything a collection run needs besides its radio connections.
#[derive(Debug, Clone)]
pub struct RunContext<'a> {
    pub cfg: &'a Config,
    pub store: &'a StoreHandle,
    pub clock: Arc<dyn Clock>,
    pub session_id: String,
    pub stop: Stop,
}

/// Monotonic time from Tokio (so paused-time simulations are deterministic)
/// and UTC from the host clock, or derived from a fixed base when simulating.
#[derive(Debug)]
pub struct TokioClock {
    origin: tokio::time::Instant,
    simulated_utc_base: Option<i64>,
}

impl TokioClock {
    pub fn real() -> Self {
        Self {
            origin: tokio::time::Instant::now(),
            simulated_utc_base: None,
        }
    }

    /// UTC = `utc_base_ns` + elapsed Tokio time. For accelerated simulations.
    pub fn simulated(utc_base_ns: i64) -> Self {
        Self {
            origin: tokio::time::Instant::now(),
            simulated_utc_base: Some(utc_base_ns),
        }
    }
}

impl Clock for TokioClock {
    fn now(&self) -> Timestamp {
        let mono = tokio::time::Instant::now().duration_since(self.origin);
        let mono_ns = u64::try_from(mono.as_nanos()).unwrap_or(u64::MAX);
        let utc_ns = match self.simulated_utc_base {
            Some(base) => base + mono_ns as i64,
            None => system_utc_ns(),
        };
        Timestamp { utc_ns, mono_ns }
    }
}

/// Cooperative stop request (signal handler, test, or operator).
#[derive(Debug, Clone)]
pub struct Stop {
    rx: tokio::sync::watch::Receiver<Option<String>>,
}

#[derive(Debug, Clone)]
pub struct StopHandle {
    tx: tokio::sync::watch::Sender<Option<String>>,
}

pub fn stop_pair() -> (StopHandle, Stop) {
    let (tx, rx) = tokio::sync::watch::channel(None);
    (StopHandle { tx }, Stop { rx })
}

impl StopHandle {
    pub fn stop(&self, reason: impl Into<String>) {
        self.tx.send_if_modified(|r| {
            if r.is_none() {
                *r = Some(reason.into());
                true
            } else {
                false
            }
        });
    }
}

impl Stop {
    pub fn reason(&self) -> Option<String> {
        self.rx.borrow().clone()
    }

    /// Resolves when a stop has been requested.
    pub async fn requested(&mut self) -> String {
        loop {
            if let Some(r) = self.rx.borrow_and_update().clone() {
                return r;
            }
            if self.rx.changed().await.is_err() {
                // Sender dropped: never resolves.
                std::future::pending::<()>().await;
            }
        }
    }
}
