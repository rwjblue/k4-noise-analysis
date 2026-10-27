//! Reconnecting, receive-only spectrum source for the K4 LAN stream (9201).
//! Nothing is ever written to the spectrum socket.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use k4na_core::time::{Clock, Timestamp};
use k4na_protocol::{Decoder, DecoderDiagnostics, SpectrumFrame};
use tokio::io::AsyncReadExt;

use crate::net::{BoxStream, Connector};

#[derive(Debug, Clone)]
pub struct ReceivedFrame {
    pub frame: SpectrumFrame,
    /// Collector arrival time of the read that completed this frame.
    pub at: Timestamp,
}

#[derive(Debug)]
pub enum SpectrumEvent {
    Connected,
    Frame(ReceivedFrame),
    /// The stream ended, failed, or was silent past the stall timeout.
    Disconnected {
        reason: String,
        stalled: bool,
    },
}

#[allow(async_fn_in_trait)]
pub trait SpectrumSource {
    /// Next event. Reconnects (with backoff) after a disconnect.
    async fn next(&mut self) -> SpectrumEvent;
    fn diagnostics(&self) -> DecoderDiagnostics;
}

#[derive(Debug, Clone)]
pub struct SpectrumOptions {
    pub stall_timeout: Duration,
    pub reconnect_initial: Duration,
    pub reconnect_max: Duration,
}

impl SpectrumOptions {
    pub fn from_config(r: &k4na_core::config::RadioConfig) -> Self {
        Self {
            stall_timeout: Duration::from_millis(r.stall_timeout_ms),
            reconnect_initial: Duration::from_millis(r.reconnect_initial_ms),
            reconnect_max: Duration::from_millis(r.reconnect_max_ms),
        }
    }
}

#[derive(Debug)]
pub struct StreamSpectrum<C: Connector> {
    connector: C,
    opts: SpectrumOptions,
    clock: Arc<dyn Clock>,
    stream: Option<BoxStream>,
    decoder: Decoder,
    pending: VecDeque<ReceivedFrame>,
    backoff: Duration,
    /// Deadline-based (not a relative sleep) so a cancelled `next()` call
    /// cannot restart the backoff indefinitely.
    reconnect_at: Option<tokio::time::Instant>,
    buf: Box<[u8; 16 * 1024]>,
}

impl<C: Connector> StreamSpectrum<C> {
    pub fn new(connector: C, opts: SpectrumOptions, clock: Arc<dyn Clock>) -> Self {
        let backoff = opts.reconnect_initial;
        Self {
            connector,
            opts,
            clock,
            stream: None,
            decoder: Decoder::new(),
            pending: VecDeque::new(),
            backoff,
            reconnect_at: None,
            buf: Box::new([0; 16 * 1024]),
        }
    }

    fn schedule_reconnect(&mut self) {
        self.reconnect_at = Some(tokio::time::Instant::now() + self.backoff);
        self.backoff = (self.backoff * 2).min(self.opts.reconnect_max);
    }

    fn drop_stream(&mut self) {
        self.stream = None;
        self.decoder.discard_partial();
        self.schedule_reconnect();
    }
}

impl<C: Connector> SpectrumSource for StreamSpectrum<C> {
    async fn next(&mut self) -> SpectrumEvent {
        loop {
            if let Some(f) = self.pending.pop_front() {
                return SpectrumEvent::Frame(f);
            }
            let Some(stream) = self.stream.as_mut() else {
                if let Some(at) = self.reconnect_at {
                    tokio::time::sleep_until(at).await;
                }
                match self.connector.connect().await {
                    Ok(s) => {
                        self.reconnect_at = None;
                        self.stream = Some(s);
                        return SpectrumEvent::Connected;
                    }
                    Err(e) => {
                        self.schedule_reconnect();
                        return SpectrumEvent::Disconnected {
                            reason: format!("connect failed: {e}"),
                            stalled: false,
                        };
                    }
                }
            };
            match tokio::time::timeout(self.opts.stall_timeout, stream.read(&mut self.buf[..])).await {
                Err(_) => {
                    self.drop_stream();
                    return SpectrumEvent::Disconnected {
                        reason: format!("no spectrum bytes for {} ms", self.opts.stall_timeout.as_millis()),
                        stalled: true,
                    };
                }
                Ok(Ok(0)) => {
                    self.drop_stream();
                    return SpectrumEvent::Disconnected {
                        reason: "spectrum stream closed by peer".into(),
                        stalled: false,
                    };
                }
                Ok(Err(e)) => {
                    self.drop_stream();
                    return SpectrumEvent::Disconnected {
                        reason: format!("spectrum read error: {e}"),
                        stalled: false,
                    };
                }
                Ok(Ok(n)) => {
                    let at = self.clock.now();
                    let mut frames = Vec::new();
                    self.decoder.push(&self.buf[..n], &mut frames);
                    if !frames.is_empty() {
                        self.backoff = self.opts.reconnect_initial;
                    }
                    self.pending
                        .extend(frames.into_iter().map(|frame| ReceivedFrame { frame, at }));
                }
            }
        }
    }

    fn diagnostics(&self) -> DecoderDiagnostics {
        self.decoder.diagnostics()
    }
}
