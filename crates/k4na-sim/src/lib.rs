//! A **simulated** Elecraft K4 for tests and dry runs: a CAT text server
//! (9200-style) and a LAN spectrum server (9201-style) driven by one shared
//! radio model with fault injection.
//!
//! The simulator is not evidence about real K4 behavior. Its span-to-rate
//! table, band-stack recall and pan-center offset are plausible stand-ins
//! chosen to exercise the collector's verification logic.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use k4na_protocol::PanId;
use k4na_protocol::synth::SyntheticFrame;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Commands the collector must never send. Seeing any of them is a test
/// failure; the simulator records them separately.
pub const FORBIDDEN_PREFIXES: &[&str] = &[
    "TX", "RX", "TU", "TS", "KY", "SW", "DA", "PC", "EC", "EE", "PS", "MN", "AI", "K3", "FB", "MD", "PA", "RA", "AR",
    "AN",
];

#[derive(Debug, Clone)]
pub struct RadioModel {
    pub fa: u64,
    pub fb: u64,
    pub bn: u8,
    pub md: u8,
    pub transmitting: bool,
    pub split: bool,
    pub diversity: bool,
    pub sub_rx: bool,
    pub lock: bool,
    pub rit: bool,
    pub xit: bool,
    pub span: u32,
    pub fixed: bool,
    pub fixed_center: u64,
    pub freeze: bool,
    pub preamp: u8,
    pub attenuator: u8,
    /// Last FA per band, recalled by `BN` (band stack).
    pub band_stack: BTreeMap<u8, u64>,
    /// Main-RX input (`AR`): 1 RX ANT IN2, 2 TX antenna, 4 RX ANT IN1,
    /// 5..=7 ATU ANT1-3.
    pub rx_input: u8,
    /// TX antenna (`AN`) 1..=3.
    pub tx_antenna: u8,
    /// Last `AR` per band, recalled by `BN` (a stand-in for per-band
    /// antenna memory; real K4 behavior is unverified).
    pub band_rx_input: BTreeMap<u8, u8>,
    /// KAT4 installed: without it `AR5..7` are refused.
    pub atu: bool,
    /// Last `#SPN` and `#FXT` per band, recalled by `BN`. A K4D (R39) was
    /// observed recalling a band's span (and so its packet rate) on `BN`.
    pub band_span: BTreeMap<u8, u32>,
    pub band_fixed: BTreeMap<u8, bool>,
}

impl RadioModel {
    /// The port actually feeding the main receiver, in station-config
    /// terms.
    pub fn rx_port(&self) -> Option<&'static str> {
        let ant = |n: u8| match n {
            1 => Some("ant1"),
            2 => Some("ant2"),
            3 => Some("ant3"),
            _ => None,
        };
        match self.rx_input {
            1 => Some("rx-ant-in2"),
            2 => ant(self.tx_antenna),
            4 => Some("rx-ant-in1"),
            5..=7 => ant(self.rx_input - 4),
            _ => None,
        }
    }
}

impl Default for RadioModel {
    fn default() -> Self {
        let band_stack = [
            (0, 1_830_000),
            (1, 3_573_000),
            (2, 5_357_000),
            (3, 7_074_000),
            (4, 10_136_000),
            (5, 14_074_000),
            (6, 18_100_000),
            (7, 21_074_000),
            (8, 24_915_000),
            (9, 28_074_000),
            (10, 50_313_000),
        ]
        .into_iter()
        .collect();
        Self {
            fa: 14_074_000,
            fb: 14_074_000,
            bn: 5,
            md: 6,
            transmitting: false,
            split: false,
            diversity: false,
            sub_rx: false,
            lock: false,
            rit: false,
            xit: false,
            span: 40_000,
            fixed: false,
            fixed_center: 14_074_000,
            freeze: false,
            preamp: 1,
            attenuator: 0,
            band_stack,
            rx_input: 4,
            tx_antenna: 1,
            band_rx_input: BTreeMap::new(),
            band_span: BTreeMap::new(),
            band_fixed: BTreeMap::new(),
            atu: true,
        }
    }
}

/// Injected misbehavior.
#[derive(Debug, Clone, Default)]
pub struct Faults {
    /// In track mode the pan center is `FA + offset` (the K4 need not center
    /// the pan on the VFO).
    pub pan_center_offset_hz: i64,
    /// Ignore `FA` setters (readback then mismatches).
    pub refuse_fa: bool,
    /// Report this sample rate regardless of span.
    pub rate_override: Option<u32>,
    /// Stop sending spectrum bytes while keeping the connection open.
    pub stall_pan: bool,
    /// Every n-th spectrum frame has a corrupted CRC.
    pub corrupt_every: Option<u32>,
    /// Interleave junk bytes before every n-th frame.
    pub garbage_every: Option<u32>,
    /// Do not answer `TQ` (transmit state unknown).
    pub no_tq: bool,
    /// Send the wrong pan id for pan A frames (header says B).
    pub wrong_pan: bool,
    /// Also stream sub (B) and mini-pan (Y) frames.
    pub extra_pans: bool,
    /// Do not answer `RV`.
    pub no_revisions: bool,
}

#[derive(Debug, Default)]
struct Inner {
    model: RadioModel,
    faults: Faults,
    commands: Vec<String>,
    forbidden: Vec<String>,
    pan_epoch: u64,
    cat_epoch: u64,
    frames_sent: u64,
}

/// Shared simulated radio.
#[derive(Debug, Clone, Default)]
pub struct SimRadio {
    inner: Arc<Mutex<Inner>>,
    pub fps: u32,
}

/// Synthetic sample rate for a requested span: smallest of
/// 24/48/96/192/384 kHz at least twice the span (fits the R39 observation
/// that `#SPN40000` produced 96 kHz packets; otherwise a stand-in).
pub fn rate_for_span(span: u32) -> u32 {
    [24_000, 48_000, 96_000, 192_000, 384_000]
        .into_iter()
        .find(|r| *r >= span * 2)
        .unwrap_or(384_000)
}

impl SimRadio {
    pub fn new(fps: u32) -> Self {
        Self {
            inner: Arc::default(),
            fps,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn model(&self) -> RadioModel {
        self.lock().model.clone()
    }

    pub fn update(&self, f: impl FnOnce(&mut RadioModel)) {
        f(&mut self.lock().model);
    }

    pub fn faults(&self, f: impl FnOnce(&mut Faults)) {
        f(&mut self.lock().faults);
    }

    /// Simulates the operator keying the transmitter.
    pub fn set_transmitting(&self, tx: bool) {
        self.lock().model.transmitting = tx;
    }

    /// Simulates a human or second CAT client retuning VFO A.
    pub fn external_retune(&self, hz: u64) {
        self.lock().model.fa = hz;
    }

    /// Every command string received over CAT, in order.
    pub fn commands(&self) -> Vec<String> {
        self.lock().commands.clone()
    }

    /// Received commands matching [`FORBIDDEN_PREFIXES`] (must stay empty).
    pub fn forbidden_commands(&self) -> Vec<String> {
        self.lock().forbidden.clone()
    }

    pub fn setters_received(&self) -> Vec<String> {
        self.commands().into_iter().filter(|c| is_setter(c)).collect()
    }

    pub fn frames_sent(&self) -> u64 {
        self.lock().frames_sent
    }

    /// Drops every connected spectrum client.
    pub fn disconnect_pan(&self) {
        self.lock().pan_epoch += 1;
    }

    /// Drops every connected CAT client.
    pub fn disconnect_cat(&self) {
        self.lock().cat_epoch += 1;
    }

    fn pan_center_and_rate(&self) -> (u64, u32) {
        let g = self.lock();
        let rate = g.faults.rate_override.unwrap_or_else(|| rate_for_span(g.model.span));
        let center = if g.model.fixed {
            g.model.fixed_center
        } else {
            (g.model.fa as i64 + g.faults.pan_center_offset_hz).max(0) as u64
        };
        (center, rate)
    }

    /// Handles one `;`-free CAT command and returns the response, if any.
    pub fn handle_cat(&self, cmd: &str) -> Option<String> {
        let mut g = self.lock();
        g.commands.push(format!("{cmd};"));
        let rx_input_setter = cmd
            .strip_prefix("AR")
            .is_some_and(|v| matches!(v, "1" | "2" | "4" | "5" | "6" | "7"));
        if FORBIDDEN_PREFIXES.iter().any(|p| cmd.starts_with(p))
            && !matches!(cmd, "AR" | "AN" | "PA" | "RA" | "MD" | "FB")
            && !rx_input_setter
        {
            // Parameterless GET forms of these are allowed; anything else
            // in a forbidden family is recorded.
            g.forbidden.push(format!("{cmd};"));
            return Some("?;".into());
        }
        let inner = &mut *g;
        let faults = &inner.faults;
        let m = &mut inner.model;
        let flag = |b: bool| u8::from(b);
        let resp = match cmd {
            "K4" => "K40".to_owned(),
            "OM" => "OM -P-S----4---".to_owned(),
            "TQ" => {
                if faults.no_tq {
                    return None;
                }
                format!("TQ{}", flag(m.transmitting))
            }
            "FA" => format!("FA{:011}", m.fa),
            "FB" => format!("FB{:011}", m.fb),
            "BN" => format!("BN{:02}", m.bn),
            "MD" => format!("MD{}", m.md),
            "FT" => format!("FT{}", flag(m.split)),
            "DV" => format!("DV{}", flag(m.diversity)),
            "SB" => format!("SB{}", flag(m.sub_rx)),
            "LK" => format!("LK{}", flag(m.lock)),
            "RT" => format!("RT{}", flag(m.rit)),
            "XT" => format!("XT{}", flag(m.xit)),
            "#SPN" => format!("#SPN{}", m.span),
            "#FXT" => format!("#FXT{}", flag(m.fixed)),
            "#FXA" => "#FXA2".to_owned(),
            "#FRZ" => format!("#FRZ{}", flag(m.freeze)),
            "#REF" => "#REF-110".to_owned(),
            "#SCL" => "#SCL70".to_owned(),
            "#AR" => "#AR0906+081".to_owned(),
            "#AVG" => "#AVG04".to_owned(),
            "#NB" => "#NB0".to_owned(),
            "#NBL" => "#NBL8".to_owned(),
            "#PKM" => "#PKM0".to_owned(),
            "#FPS" => "#FPS15".to_owned(),
            "NA" => "NA0".to_owned(),
            "NM" => "NM01760".to_owned(),
            "OV" => "OV00".to_owned(),
            "PA" => format!("PA{}", m.preamp),
            "RA" => format!("RA{:02}", m.attenuator),
            "GT" => "GT2".to_owned(),
            "RG" => "RG-00".to_owned(),
            "NB" => "NB0".to_owned(),
            "NR" => "NR0".to_owned(),
            "AR" => format!("AR{}", m.rx_input),
            "AN" => format!("AN{}", m.tx_antenna),
            "RV" => {
                if faults.no_revisions {
                    return None;
                }
                return Some("RV.FP-SIM-R39;RV.DSP-SIM-R39;RV.DDC0-SIM;".into());
            }
            _ => {
                if let Some(v) = cmd.strip_prefix("FA").and_then(|v| v.parse::<u64>().ok()) {
                    if !faults.refuse_fa && !m.lock && !m.transmitting {
                        m.fa = v;
                        let bn = m.bn;
                        m.band_stack.insert(bn, v);
                    }
                    return None;
                }
                if let Some(v) = cmd.strip_prefix("BN").and_then(|v| v.parse::<u8>().ok()) {
                    if v <= 10 && !m.transmitting {
                        let cur = m.bn;
                        let fa = m.fa;
                        m.band_stack.insert(cur, fa);
                        let (input, span, fixed) = (m.rx_input, m.span, m.fixed);
                        m.band_rx_input.insert(cur, input);
                        m.band_span.insert(cur, span);
                        m.band_fixed.insert(cur, fixed);
                        m.bn = v;
                        m.fa = m.band_stack.get(&v).copied().unwrap_or(m.fa);
                        m.rx_input = m.band_rx_input.get(&v).copied().unwrap_or(m.rx_input);
                        m.span = m.band_span.get(&v).copied().unwrap_or(m.span);
                        m.fixed = m.band_fixed.get(&v).copied().unwrap_or(m.fixed);
                        m.fixed_center = m.fa;
                    }
                    return None;
                }
                if rx_input_setter {
                    let v: u8 = cmd[2..].parse().unwrap_or(0);
                    if !m.transmitting && (m.atu || !(5..=7).contains(&v)) {
                        m.rx_input = v;
                    }
                    return None;
                }
                if let Some(v) = cmd.strip_prefix("#SPN").and_then(|v| v.parse::<u32>().ok()) {
                    m.span = v.clamp(6_000, 368_000);
                    return None;
                }
                if let Some(v) = cmd.strip_prefix("#FXT").and_then(|v| v.parse::<u8>().ok()) {
                    m.fixed = v == 1;
                    m.fixed_center = m.fa;
                    return None;
                }
                return Some("?;".into());
            }
        };
        Some(format!("{resp};"))
    }

    /// Builds the next frame for `pan` with deterministic pseudo-noise and
    /// fixed synthetic carriers (a 125 kHz comb plus a few single tones).
    pub fn build_frame(&self, pan: PanId, sequence: u8, frame_index: u64) -> Vec<u8> {
        let (center, rate) = self.pan_center_and_rate();
        let (freeze, wrong_pan) = {
            let g = self.lock();
            (g.model.freeze, g.faults.wrong_pan)
        };
        let center = if pan == PanId::A { center } else { 7_074_000 };
        let seed_index = if freeze { 0 } else { frame_index };
        let port = self.lock().model.rx_port();
        let mut bins = synth_bins(center, rate, seed_index);
        let offset = port_floor_offset(port);
        for b in &mut bins {
            *b = b.saturating_add(offset);
        }
        let header_pan = if wrong_pan && pan == PanId::A { PanId::B } else { pan };
        let frame = SyntheticFrame {
            sequence,
            pan: header_pan,
            sample_rate_hz: rate,
            center_hz: center,
            noise_field_tenths: -1310,
            ..SyntheticFrame::default()
        };
        frame.encode(&bins).to_vec()
    }
}

fn is_setter(cmd: &str) -> bool {
    let body = cmd.trim_end_matches(';');
    let prefixes = [
        // Longer prefixes first (`#NBL` before `#NB`).
        "#NBL", "#PKM", "#FPS", "#SPN", "#FXT", "#FXA", "#FRZ", "#REF", "#SCL", "#AVG", "#AR", "#NB", "FA", "FB", "BN",
        "MD", "FT", "DV", "SB", "LK", "RT", "XT", "PA", "RA", "GT", "RG", "NB", "NR", "NA", "NM", "OV", "AR", "AN",
        "TQ", "OM", "RV", "K4",
    ];
    for p in prefixes {
        if let Some(rest) = body.strip_prefix(p) {
            return !rest.is_empty();
        }
    }
    !body.is_empty()
}

/// Synthetic level offset per RX input, so tests can tell antennas apart:
/// ANT2 reads 6 dB higher, a disconnected input 40 dB lower.
pub fn port_floor_offset(port: Option<&str>) -> i16 {
    match port {
        Some("ant2") => 60,
        Some(_) => 0,
        None => -400,
    }
}

/// Deterministic synthetic spectrum: floor near -130 dB (native tenths)
/// with pseudo-noise, a 125 kHz comb at -95 and a few tones at -80.
pub fn synth_bins(center: u64, rate: u32, frame_index: u64) -> Vec<i16> {
    let n = k4na_protocol::BIN_COUNT;
    let width = f64::from(rate) / n as f64;
    let mut state = frame_index.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ center;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        let f = center as f64 + (i as f64 - n as f64 / 2.0) * width;
        let mut v = -1300 + (state % 60) as i16 - 30;
        let comb = (f / 125_000.0).round() * 125_000.0;
        if (f - comb).abs() < width {
            v = -950 + (state % 10) as i16;
        }
        for tone in [3_520_000.0, 7_040_000.0, 14_100_000.0, 28_200_000.0] {
            if (f - tone).abs() < width {
                v = -800;
            }
        }
        out.push(v);
    }
    out
}

/// Serves CAT on any stream until it closes or [`SimRadio::disconnect_cat`].
pub async fn serve_cat<S: AsyncRead + AsyncWrite + Unpin>(radio: SimRadio, mut stream: S) {
    let epoch = radio.lock().cat_epoch;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let n = match tokio::time::timeout(Duration::from_millis(100), stream.read(&mut chunk)).await {
            Err(_) => {
                if radio.lock().cat_epoch != epoch {
                    return;
                }
                continue;
            }
            Ok(Ok(0)) | Ok(Err(_)) => return,
            Ok(Ok(n)) => n,
        };
        if radio.lock().cat_epoch != epoch {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        let mut out = String::new();
        while let Some(pos) = buf.iter().position(|b| *b == b';') {
            let item: Vec<u8> = buf.drain(..=pos).collect();
            let cmd = String::from_utf8_lossy(&item[..item.len() - 1]).trim().to_owned();
            if let Some(r) = radio.handle_cat(&cmd) {
                out.push_str(&r);
            }
        }
        if !out.is_empty() && stream.write_all(out.as_bytes()).await.is_err() {
            return;
        }
    }
}

/// Streams spectrum frames at `radio.fps` until the client goes away or
/// [`SimRadio::disconnect_pan`] is called.
pub async fn serve_pan<S: AsyncWrite + Unpin>(radio: SimRadio, mut stream: S) {
    let epoch = radio.lock().pan_epoch;
    let period = Duration::from_secs_f64(1.0 / f64::from(radio.fps.max(1)));
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut index: u64 = 0;
    loop {
        tick.tick().await;
        let (stall, corrupt, garbage, extra) = {
            let g = radio.lock();
            if g.pan_epoch != epoch {
                return;
            }
            (
                g.faults.stall_pan,
                g.faults.corrupt_every,
                g.faults.garbage_every,
                g.faults.extra_pans,
            )
        };
        if stall {
            continue;
        }
        index += 1;
        let seq = (index % 256) as u8;
        let mut bytes = Vec::new();
        if garbage.is_some_and(|n| n > 0 && index.is_multiple_of(u64::from(n))) {
            bytes.extend_from_slice(&[0xff, 0xfe, 0x01, 0x00, 0x02, 0x00, b'A', 0x13, 0x37]);
            bytes.extend((0..500u32).map(|i| (i * 31 % 251) as u8));
        }
        let mut a = radio.build_frame(PanId::A, seq, index);
        if corrupt.is_some_and(|n| n > 0 && index.is_multiple_of(u64::from(n))) {
            a[1000] ^= 0x55;
        }
        bytes.extend_from_slice(&a);
        if extra {
            bytes.extend(radio.build_frame(PanId::B, seq, index));
            bytes.extend(radio.build_frame(PanId::Y, seq, index));
        }
        if stream.write_all(&bytes).await.is_err() {
            return;
        }
        radio.lock().frames_sent += 1;
    }
}

/// Listening TCP simulator for manual end-to-end runs (`k4na simulate`).
#[derive(Debug)]
pub struct TcpSim {
    pub cat_addr: std::net::SocketAddr,
    pub pan_addr: std::net::SocketAddr,
}

impl TcpSim {
    pub async fn start(radio: SimRadio, host: &str, cat_port: u16, pan_port: u16) -> std::io::Result<Self> {
        let cat = tokio::net::TcpListener::bind((host, cat_port)).await?;
        let pan = tokio::net::TcpListener::bind((host, pan_port)).await?;
        let sim = Self {
            cat_addr: cat.local_addr()?,
            pan_addr: pan.local_addr()?,
        };
        let r = radio.clone();
        tokio::spawn(async move {
            while let Ok((s, _)) = cat.accept().await {
                tokio::spawn(serve_cat(r.clone(), s));
            }
        });
        tokio::spawn(async move {
            while let Ok((s, _)) = pan.accept().await {
                tokio::spawn(serve_pan(radio.clone(), s));
            }
        });
        Ok(sim)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn band_change_recalls_band_stack_and_logs_commands() {
        let r = SimRadio::new(15);
        assert_eq!(r.handle_cat("BN03"), None);
        assert_eq!(r.model().fa, 7_074_000);
        assert_eq!(r.handle_cat("FA00007100000"), None);
        assert_eq!(r.handle_cat("FA").as_deref(), Some("FA00007100000;"));
        assert_eq!(r.handle_cat("TX").as_deref(), Some("?;"));
        assert_eq!(r.forbidden_commands(), ["TX;"]);
        assert_eq!(r.setters_received(), ["BN03;", "FA00007100000;", "TX;"]);
    }

    #[test]
    fn frames_follow_vfo_in_track_mode_and_decode() {
        let r = SimRadio::new(15);
        r.faults(|f| f.pan_center_offset_hz = -11_240);
        let bytes = r.build_frame(PanId::A, 1, 1);
        let frames = k4na_protocol::Decoder::new().decode(&bytes);
        assert_eq!(frames[0].header().center_hz, 14_074_000 - 11_240);
        assert_eq!(frames[0].header().sample_rate_hz, 96_000);
        r.update(|m| m.fixed = true);
        r.handle_cat("FA00014200000");
        let f2 = k4na_protocol::Decoder::new().decode(&r.build_frame(PanId::A, 2, 2));
        assert_eq!(f2[0].header().center_hz, 14_074_000, "fixed pan does not follow");
    }
}
