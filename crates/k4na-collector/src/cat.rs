//! Receive-only K4 CAT (TCP 9200) access through a reviewed, typed whitelist.
//!
//! Safety layers:
//! 1. Callers can only express commands through [`Query`] (GET forms) and
//!    [`Setter`] (band, VFO, span, track mode, RX input and receiver
//!    measurement settings, for the main receiver and, with `$`, the sub
//!    receiver). There is no string API. `AR`/`AR$` only route an input to
//!    a receiver; the TX antenna (`AN`), split, diversity and band
//!    independence can be read but never set.
//! 2. Every byte sequence is re-validated by [`check_wire`] immediately
//!    before it reaches the socket. Setters are refused on a read-only link.
//! 3. A [`ControlCat`] link can only be created with a [`RetuneConsent`],
//!    which requires both the TOML opt-in and the CLI `--allow-retune` flag.
//!
//! No transmit, PTT, tune, test, keying, audio, switch-emulation or
//! arbitrary command can be encoded. See docs/collection.md.

use std::collections::BTreeMap;
use std::time::Duration;

use k4na_core::model::RadioState;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::net::BoxStream;

/// GET-only state queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Query {
    Identify,
    OptionModules,
    Revisions,
    Transmit,
    VfoA,
    VfoB,
    Band,
    Mode,
    Split,
    Diversity,
    SubRx,
    Lock,
    Rit,
    Xit,
    Span,
    FixedTune,
    FixedTuneMode,
    Freeze,
    RefLevel,
    Scale,
    AutoRef,
    Averaging,
    PanNoiseBlanker,
    Preamp,
    Attenuator,
    Agc,
    RfGain,
    NoiseBlanker,
    NoiseReduction,
    RxAntenna,
    TxAntenna,
    AutoNotch,
    ManualNotch,
    PanNoiseBlankerLevel,
    PeakMode,
    FramesPerSecond,
    Overflow,
    /// `BI`: VFO B may sit on a different band from VFO A.
    BandIndependent,
    // Sub-receiver (`$`) forms. Each is its own query so main and sub
    // readbacks can never be confused.
    SubBand,
    SubSpan,
    SubRefLevel,
    SubPanNoiseBlanker,
    SubPreamp,
    SubAttenuator,
    SubAgc,
    SubRfGain,
    SubNoiseBlanker,
    SubNoiseReduction,
    SubRxAntenna,
    SubOverflow,
}

impl Query {
    pub const ALL: [Query; 50] = [
        Query::Identify,
        Query::OptionModules,
        Query::Revisions,
        Query::Transmit,
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
        Query::FixedTuneMode,
        Query::Freeze,
        Query::RefLevel,
        Query::Scale,
        Query::AutoRef,
        Query::Averaging,
        Query::PanNoiseBlanker,
        Query::Preamp,
        Query::Attenuator,
        Query::Agc,
        Query::RfGain,
        Query::NoiseBlanker,
        Query::NoiseReduction,
        Query::RxAntenna,
        Query::TxAntenna,
        Query::AutoNotch,
        Query::ManualNotch,
        Query::PanNoiseBlankerLevel,
        Query::PeakMode,
        Query::FramesPerSecond,
        Query::Overflow,
        Query::BandIndependent,
        Query::SubBand,
        Query::SubSpan,
        Query::SubRefLevel,
        Query::SubPanNoiseBlanker,
        Query::SubPreamp,
        Query::SubAttenuator,
        Query::SubAgc,
        Query::SubRfGain,
        Query::SubNoiseBlanker,
        Query::SubNoiseReduction,
        Query::SubRxAntenna,
        Query::SubOverflow,
    ];

    /// Sub-receiver state recorded with each visit. On a K4D one
    /// receiver's level can depend on the other's input (see
    /// docs/paired.md), so `AR$` belongs with the main receiver's state.
    pub const SUB_STATE: [Query; 11] = [
        Query::VfoB,
        Query::BandIndependent,
        Query::SubBand,
        Query::SubSpan,
        Query::SubRefLevel,
        Query::SubPanNoiseBlanker,
        Query::SubPreamp,
        Query::SubAttenuator,
        Query::SubAgc,
        Query::SubRfGain,
        Query::SubRxAntenna,
    ];

    /// The sub-receiver form of a main-receiver measurement query, if the
    /// K4 has one. `#AVG`, `#FXT` and `#FPS` are shared by both pans.
    pub fn sub_form(self) -> Option<Query> {
        Some(match self {
            Query::Band => Query::SubBand,
            Query::Span => Query::SubSpan,
            Query::RefLevel => Query::SubRefLevel,
            Query::PanNoiseBlanker => Query::SubPanNoiseBlanker,
            Query::Preamp => Query::SubPreamp,
            Query::Attenuator => Query::SubAttenuator,
            Query::Agc => Query::SubAgc,
            Query::RfGain => Query::SubRfGain,
            Query::NoiseBlanker => Query::SubNoiseBlanker,
            Query::NoiseReduction => Query::SubNoiseReduction,
            Query::RxAntenna => Query::SubRxAntenna,
            Query::Overflow => Query::SubOverflow,
            Query::VfoA => Query::VfoB,
            _ => return None,
        })
    }

    /// State recorded with each visit for comparability segments.
    pub const STATE: [Query; 23] = [
        Query::VfoA,
        Query::Band,
        Query::Mode,
        Query::Split,
        Query::Diversity,
        Query::SubRx,
        Query::Rit,
        Query::Xit,
        Query::Span,
        Query::FixedTune,
        Query::FixedTuneMode,
        Query::Freeze,
        Query::RefLevel,
        Query::Scale,
        Query::AutoRef,
        Query::Averaging,
        Query::PanNoiseBlanker,
        Query::Preamp,
        Query::Attenuator,
        Query::Agc,
        Query::RfGain,
        Query::RxAntenna,
        Query::TxAntenna,
    ];

    /// Response prefix, which is also the GET command text.
    pub fn prefix(self) -> &'static str {
        match self {
            Query::Identify => "K4",
            Query::OptionModules => "OM",
            Query::Revisions => "RV",
            Query::Transmit => "TQ",
            Query::VfoA => "FA",
            Query::VfoB => "FB",
            Query::Band => "BN",
            Query::Mode => "MD",
            Query::Split => "FT",
            Query::Diversity => "DV",
            Query::SubRx => "SB",
            Query::Lock => "LK",
            Query::Rit => "RT",
            Query::Xit => "XT",
            Query::Span => "#SPN",
            Query::FixedTune => "#FXT",
            Query::FixedTuneMode => "#FXA",
            Query::Freeze => "#FRZ",
            Query::RefLevel => "#REF",
            Query::Scale => "#SCL",
            Query::AutoRef => "#AR",
            Query::Averaging => "#AVG",
            Query::PanNoiseBlanker => "#NB",
            Query::Preamp => "PA",
            Query::Attenuator => "RA",
            Query::Agc => "GT",
            Query::RfGain => "RG",
            Query::NoiseBlanker => "NB",
            Query::NoiseReduction => "NR",
            Query::RxAntenna => "AR",
            Query::TxAntenna => "AN",
            Query::AutoNotch => "NA",
            Query::ManualNotch => "NM",
            Query::PanNoiseBlankerLevel => "#NBL",
            Query::PeakMode => "#PKM",
            Query::FramesPerSecond => "#FPS",
            Query::Overflow => "OV",
            Query::BandIndependent => "BI",
            Query::SubBand => "BN$",
            Query::SubSpan => "#SPN$",
            Query::SubRefLevel => "#REF$",
            Query::SubPanNoiseBlanker => "#NB$",
            Query::SubPreamp => "PA$",
            Query::SubAttenuator => "RA$",
            Query::SubAgc => "GT$",
            Query::SubRfGain => "RG$",
            Query::SubNoiseBlanker => "NB$",
            Query::SubNoiseReduction => "NR$",
            Query::SubRxAntenna => "AR$",
            Query::SubOverflow => "OV$",
        }
    }

    pub fn wire(self) -> String {
        format!("{};", self.prefix())
    }

    pub fn from_prefix(p: &str) -> Option<Query> {
        Query::ALL.into_iter().find(|q| q.prefix() == p)
    }
}

/// A main-RX input for `AR`. Only 1 (RX ANT IN2), 2 (RX uses the TX
/// antenna), 4 (RX ANT IN1) and 5..=7 (ATU ANT1-3) can be constructed; never
/// 0 (disconnect every RX input) or 3 (internal transverter).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxInput(u8);

impl RxInput {
    pub fn for_port(port: k4na_core::config::K4Port) -> Self {
        Self(port.ar_value())
    }

    /// A previously read `AR` value, for restoring it.
    pub fn from_readback(raw: &str) -> Option<Self> {
        let n: u8 = raw.trim().parse().ok()?;
        rx_input_ok(n).then_some(Self(n))
    }

    pub fn value(self) -> u8 {
        self.0
    }
}

fn rx_input_ok(n: u8) -> bool {
    matches!(n, 1 | 2 | 4..=7)
}

/// The only radio-state changes the collector can make. Each is reviewed:
/// band select, VFO A/B frequency, pan span, pan track/fixed mode, the RX
/// inputs, sub receiver on/off and receive measurement settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Setter {
    /// `BNnn;` K4 band number 0..=10 (HF/6 m only; no transverter bands).
    Band(u8),
    /// `FAnnnnnnnnnnn;` in Hz (11 digits => Hz interpretation), 100 kHz..54 MHz.
    VfoA(u64),
    /// `#SPNn;` in Hz, 6000..=368000.
    Span(u32),
    /// `#FXT0;` track or `#FXT1;` fixed.
    FixedTune(bool),
    /// `ARn;` main-RX input (receive routing only).
    RxInput(RxInput),
    /// Main-receiver measurement settings (receive path only).
    Receiver(ReceiverSetter),
    /// `SB0;`/`SB1;` sub receiver off/on.
    SubRx(bool),
    /// `FBnnnnnnnnnnn;` VFO B in Hz, same range as VFO A. Only sent with
    /// split off (`FT0`), where VFO B is not the transmit frequency.
    VfoB(u64),
    /// `#SPN$n;` sub pan span in Hz. (The sub band is never set: paired
    /// surveys require `BI0`, where VFO B follows VFO A's band.)
    SubSpan(u32),
    /// `AR$n;` sub-RX input (receive routing only).
    SubRxInput(RxInput),
    /// Sub-receiver measurement settings. `#AVG` has no sub form, so
    /// `SubReceiver(PanAveraging(_))` is never sendable.
    SubReceiver(ReceiverSetter),
}

/// Main-receiver settings that change what the receiver and panadapter
/// measure. `Basic` forms are the K3-compatible ones a `K40` radio reads
/// back; `Advanced` forms are the documented K4 ones for `K41`. None
/// affects the transmitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ReceiverSetter {
    /// `PAn;` preamp level 0..=3 (0 = off).
    PreampBasic(u8),
    /// `PAnm;` preamp level 0..=3 and on/off.
    PreampAdvanced(u8, bool),
    /// `RA00;` attenuator off (the only basic value with an unambiguous
    /// readback).
    AttenuatorOffBasic,
    /// `RAnnm;` 0..=21 dB in 3 dB steps and on/off.
    AttenuatorAdvanced(u8, bool),
    /// `RG-nn;` RF gain reduction 0..=60 dB.
    RfGain(u8),
    /// `GT002;` fast or `GT004;` slow (K3 form).
    AgcBasic { slow: bool },
    /// `GTn;` 0 off, 1 slow, 2 fast.
    AgcAdvanced(u8),
    /// `NBm;` receiver noise blanker on/off.
    NoiseBlankerBasic(bool),
    /// `NBnnm;` level 0..=15 and on/off.
    NoiseBlankerAdvanced(u8, bool),
    /// `NRnnm;` level 1..=10 and on/off.
    NoiseReduction(u8, bool),
    /// `#NBn;` 0 off, 1 on, 2 auto.
    PanNoiseBlanker(u8),
    /// `#AVGnn;` 1..=20.
    PanAveraging(u8),
}

impl ReceiverSetter {
    pub fn wire(self) -> String {
        self.wire_for(false)
    }

    /// The command for the main receiver, or with `sub` its `$` form.
    pub fn wire_for(self, sub: bool) -> String {
        let f = u8::from;
        let s = if sub { "$" } else { "" };
        match self {
            ReceiverSetter::PreampBasic(l) => format!("PA{s}{l};"),
            ReceiverSetter::PreampAdvanced(l, on) => format!("PA{s}{l}{};", f(on)),
            ReceiverSetter::AttenuatorOffBasic => format!("RA{s}00;"),
            ReceiverSetter::AttenuatorAdvanced(db, on) => format!("RA{s}{db:02}{};", f(on)),
            ReceiverSetter::RfGain(db) => format!("RG{s}-{db:02};"),
            ReceiverSetter::AgcBasic { slow } => format!("GT{s}{};", if slow { "004" } else { "002" }),
            ReceiverSetter::AgcAdvanced(n) => format!("GT{s}{n};"),
            ReceiverSetter::NoiseBlankerBasic(on) => format!("NB{s}{};", f(on)),
            ReceiverSetter::NoiseBlankerAdvanced(l, on) => format!("NB{s}{l:02}{};", f(on)),
            ReceiverSetter::NoiseReduction(l, on) => format!("NR{s}{l:02}{};", f(on)),
            ReceiverSetter::PanNoiseBlanker(n) => format!("#NB{s}{n};"),
            ReceiverSetter::PanAveraging(n) => format!("#AVG{s}{n:02};"),
        }
    }

    /// The main-receiver query whose readback verifies this setting.
    pub fn query(self) -> Query {
        match self {
            ReceiverSetter::PreampBasic(_) | ReceiverSetter::PreampAdvanced(..) => Query::Preamp,
            ReceiverSetter::AttenuatorOffBasic | ReceiverSetter::AttenuatorAdvanced(..) => Query::Attenuator,
            ReceiverSetter::RfGain(_) => Query::RfGain,
            ReceiverSetter::AgcBasic { .. } | ReceiverSetter::AgcAdvanced(_) => Query::Agc,
            ReceiverSetter::NoiseBlankerBasic(_) | ReceiverSetter::NoiseBlankerAdvanced(..) => Query::NoiseBlanker,
            ReceiverSetter::NoiseReduction(..) => Query::NoiseReduction,
            ReceiverSetter::PanNoiseBlanker(_) => Query::PanNoiseBlanker,
            ReceiverSetter::PanAveraging(_) => Query::Averaging,
        }
    }
}

impl Setter {
    pub fn wire(self) -> String {
        match self {
            Setter::Band(n) => format!("BN{n:02};"),
            Setter::VfoA(hz) => format!("FA{hz:011};"),
            Setter::Span(hz) => format!("#SPN{hz};"),
            Setter::FixedTune(fixed) => format!("#FXT{};", u8::from(fixed)),
            Setter::RxInput(i) => format!("AR{};", i.0),
            Setter::Receiver(r) => r.wire(),
            Setter::SubRx(on) => format!("SB{};", u8::from(on)),
            Setter::VfoB(hz) => format!("FB{hz:011};"),
            Setter::SubSpan(hz) => format!("#SPN${hz};"),
            Setter::SubRxInput(i) => format!("AR${};", i.0),
            Setter::SubReceiver(r) => r.wire_for(true),
        }
    }

    /// The query whose readback verifies this setter. `SubReceiver` of a
    /// shared pan setting (`#AVG`) maps to the main query, but is never
    /// sendable.
    pub fn query(self) -> Query {
        match self {
            Setter::Band(_) => Query::Band,
            Setter::VfoA(_) => Query::VfoA,
            Setter::Span(_) => Query::Span,
            Setter::FixedTune(_) => Query::FixedTune,
            Setter::RxInput(_) => Query::RxAntenna,
            Setter::Receiver(r) => r.query(),
            Setter::SubRx(_) => Query::SubRx,
            Setter::VfoB(_) => Query::VfoB,
            Setter::SubSpan(_) => Query::SubSpan,
            Setter::SubRxInput(_) => Query::SubRxAntenna,
            Setter::SubReceiver(r) => r.query().sub_form().unwrap_or(r.query()),
        }
    }

    /// Whether this setter changes the sub receiver or VFO B.
    pub fn is_sub(self) -> bool {
        matches!(
            self,
            Setter::SubRx(_) | Setter::VfoB(_) | Setter::SubSpan(_) | Setter::SubRxInput(_) | Setter::SubReceiver(_)
        )
    }

    /// Whether the whitelist would send this setter. A value the radio
    /// reported can still be outside the reviewed range (a K4D on R39
    /// stored `#SPN5000`, below the documented 6000).
    pub fn is_sendable(self) -> bool {
        check_wire(&self.wire(), true).is_ok()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CatError {
    #[error("refused unsafe or non-whitelisted CAT command {0:?}")]
    Unsafe(String),
    #[error("CAT I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("CAT connection closed")]
    Closed,
    #[error("CAT response buffer overflow")]
    Overflow,
}

/// Splits a response or command into (known prefix, parameters). Longest
/// known prefix wins (`#AR` vs `AR`, `AR$` vs `AR`). A `$` after a prefix
/// with no known sub form is reported as `true` (an unknown sub-receiver
/// item), so main/sub responses can never be confused.
pub fn split_prefix(item: &str) -> Option<(&'static str, bool, &str)> {
    let mut best: Option<&'static str> = None;
    for q in Query::ALL {
        let p = q.prefix();
        if item.starts_with(p) && best.is_none_or(|b| p.len() > b.len()) {
            best = Some(p);
        }
    }
    let p = best?;
    let rest = &item[p.len()..];
    match rest.strip_prefix('$') {
        Some(r) => Some((p, true, r)),
        None => Some((p, false, rest)),
    }
}

/// Validates outgoing bytes against the whitelist. Every `;`-terminated item
/// must be a parameterless main-receiver GET, or (only if `allow_setters`)
/// a well-formed whitelisted setter.
pub fn check_wire(wire: &str, allow_setters: bool) -> Result<(), CatError> {
    let unsafe_err = || CatError::Unsafe(wire.to_owned());
    if wire.is_empty() || !wire.ends_with(';') || !wire.is_ascii() {
        return Err(unsafe_err());
    }
    for item in wire[..wire.len() - 1].split(';') {
        let (prefix, sub, params) = split_prefix(item).ok_or_else(unsafe_err)?;
        if sub {
            return Err(unsafe_err());
        }
        if params.is_empty() {
            continue;
        }
        if !allow_setters || !setter_params_ok(prefix, params) {
            return Err(unsafe_err());
        }
    }
    Ok(())
}

/// The reviewed setter grammar: exact parameter shapes and ranges for the
/// only prefixes the collector may set. Everything else is refused.
fn setter_params_ok(prefix: &str, params: &str) -> bool {
    // Sub-receiver measurement settings share the main grammar. `#AVG`
    // has no sub form and never reaches here (no `#AVG$` prefix).
    let prefix = match prefix {
        "PA$" => "PA",
        "RA$" => "RA",
        "RG$" => "RG",
        "GT$" => "GT",
        "NB$" => "NB",
        "NR$" => "NR",
        "#NB$" => "#NB",
        p => p,
    };
    // RF gain is the one setter with a sign: exactly `-nn`.
    if prefix == "RG" {
        return params
            .strip_prefix('-')
            .filter(|d| d.len() == 2 && d.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|d| d.parse::<u8>().ok())
            .is_some_and(|db| db <= 60);
    }
    if !params.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let Ok(n) = params.parse::<u64>() else {
        return false;
    };
    let digit = |i: usize| u64::from(params.as_bytes()[i] - b'0');
    let len = params.len();
    match prefix {
        "BN" => len == 2 && n <= 10,
        "FA" | "FB" => len == 11 && (100_000..=54_000_000).contains(&n),
        "#SPN" | "#SPN$" => (6_000..=368_000).contains(&n),
        "#FXT" => len == 1 && n <= 1,
        "AR" | "AR$" => len == 1 && rx_input_ok(n as u8),
        "SB" => len == 1 && n <= 1,
        // PAn (basic level) or PAnm (level, on/off).
        "PA" => (len == 1 && n <= 3) || (len == 2 && digit(0) <= 3 && digit(1) <= 1),
        // RA00 (basic off) or RAnnm (dB in 3 dB steps, on/off).
        "RA" => params == "00" || (len == 3 && n / 10 <= 21 && (n / 10) % 3 == 0 && digit(2) <= 1),
        // GT002/GT004 (basic fast/slow) or GTn (0 off, 1 slow, 2 fast).
        "GT" => matches!(params, "002" | "004") || (len == 1 && n <= 2),
        // NBm or NBnnm (level 0..=15).
        "NB" => (len == 1 && n <= 1) || (len == 3 && n / 10 <= 15 && digit(2) <= 1),
        // NRnnm (level 1..=10).
        "NR" => len == 3 && (1..=10).contains(&(n / 10)) && digit(2) <= 1,
        "#NB" => len == 1 && n <= 2,
        "#AVG" => len == 2 && (1..=20).contains(&n),
        _ => false,
    }
}

/// Parsed readbacks keyed by query. Missing entries are unknown.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Readback {
    values: BTreeMap<Query, String>,
    /// All `RV` module responses (one per module).
    pub revisions: Vec<String>,
}

impl Readback {
    pub fn raw(&self, q: Query) -> Option<&str> {
        self.values.get(&q).map(String::as_str)
    }

    pub fn insert(&mut self, q: Query, v: impl Into<String>) {
        self.values.insert(q, v.into());
    }

    pub fn remove(&mut self, q: Query) {
        self.values.remove(&q);
    }

    /// Raw values keyed by prefix (the journal's representation).
    pub fn to_map(&self) -> BTreeMap<String, String> {
        self.values
            .iter()
            .map(|(q, v)| (q.prefix().to_owned(), v.clone()))
            .collect()
    }

    /// Inverse of [`Readback::to_map`]; unknown prefixes are ignored.
    pub fn from_map(m: &BTreeMap<String, String>) -> Self {
        let mut rb = Readback::default();
        for (k, v) in m {
            if let Some(q) = Query::from_prefix(k) {
                rb.values.insert(q, v.clone());
            }
        }
        rb
    }

    pub fn u64(&self, q: Query) -> Option<u64> {
        self.raw(q)?.trim().parse().ok()
    }

    /// `Some(true)` for `1`, `Some(false)` for `0`, else unknown.
    pub fn flag(&self, q: Query) -> Option<bool> {
        match self.raw(q)?.trim() {
            "1" => Some(true),
            "0" => Some(false),
            _ => None,
        }
    }

    /// Merges into a stored radio-state map; missing values become "unknown".
    pub fn to_state(&self, queries: &[Query]) -> RadioState {
        queries
            .iter()
            .map(|q| {
                (
                    q.prefix().to_owned(),
                    self.raw(*q).map_or_else(|| "unknown".to_owned(), str::to_owned),
                )
            })
            .collect()
    }
}

/// GET-only access. Implemented by both link kinds.
#[allow(async_fn_in_trait)]
pub trait CatQuery {
    async fn query(&mut self, queries: &[Query]) -> Result<Readback, CatError>;
}

/// Setter access, only available with retune consent.
#[allow(async_fn_in_trait)]
pub trait CatControl: CatQuery {
    async fn set(&mut self, setter: Setter) -> Result<(), CatError>;
}

/// Proof that both the TOML `allow_retune` and CLI `--allow-retune` were given.
#[derive(Debug, Clone, Copy)]
pub struct RetuneConsent(());

impl RetuneConsent {
    pub fn new(config_allows: bool, cli_allows: bool) -> Result<Self, &'static str> {
        match (config_allows, cli_allows) {
            (true, true) => Ok(Self(())),
            (false, _) => Err("collection.allow_retune is false in the config"),
            (true, false) => Err("the --allow-retune command-line flag was not given"),
        }
    }
}

const MAX_RESPONSE_BUFFER: usize = 64 * 1024;

#[derive(Debug)]
struct Link {
    stream: BoxStream,
    buf: Vec<u8>,
    timeout: Duration,
    sent: Vec<String>,
}

impl Link {
    async fn write_checked(&mut self, wire: &str, allow_setters: bool) -> Result<(), CatError> {
        check_wire(wire, allow_setters)?;
        self.stream.write_all(wire.as_bytes()).await?;
        self.stream.flush().await?;
        if self.sent.len() < 4096 {
            self.sent.push(wire.to_owned());
        }
        Ok(())
    }

    /// Reads `;`-terminated items until `done` returns true or the timeout.
    async fn read_until(
        &mut self,
        mut on_item: impl FnMut(&str) -> bool,
        deadline: tokio::time::Instant,
    ) -> Result<(), CatError> {
        loop {
            while let Some(pos) = self.buf.iter().position(|b| *b == b';') {
                let item: Vec<u8> = self.buf.drain(..=pos).collect();
                let text = String::from_utf8_lossy(&item[..item.len() - 1]).trim().to_owned();
                if !text.is_empty() && on_item(&text) {
                    return Ok(());
                }
            }
            let mut chunk = [0u8; 1024];
            let n = match tokio::time::timeout_at(deadline, self.stream.read(&mut chunk)).await {
                Err(_) => return Ok(()),
                Ok(r) => r?,
            };
            if n == 0 {
                return Err(CatError::Closed);
            }
            self.buf.extend_from_slice(&chunk[..n]);
            if self.buf.len() > MAX_RESPONSE_BUFFER {
                self.buf.clear();
                return Err(CatError::Overflow);
            }
        }
    }

    async fn query(&mut self, queries: &[Query]) -> Result<Readback, CatError> {
        let mut rb = Readback::default();
        if queries.is_empty() {
            return Ok(rb);
        }
        let wire: String = queries.iter().map(|q| q.wire()).collect();
        self.write_checked(&wire, false).await?;
        let mut wanted: std::collections::BTreeSet<Query> = queries.iter().copied().collect();
        let deadline = tokio::time::Instant::now() + self.timeout;
        let wants_rv = wanted.contains(&Query::Revisions);
        self.read_until(
            |item| {
                if let Some((prefix, false, params)) = split_prefix(item)
                    && let Some(q) = Query::from_prefix(prefix)
                {
                    if q == Query::Revisions {
                        rb.revisions.push(params.to_owned());
                        rb.values.insert(q, params.to_owned());
                        // RV returns one response per module; keep reading.
                        wanted.remove(&q);
                        return false;
                    }
                    // A "?" item is an error for an unknown position; ignore.
                    if wanted.remove(&q) {
                        rb.values.insert(q, params.to_owned());
                    }
                }
                wanted.is_empty() && !wants_rv
            },
            deadline,
        )
        .await?;
        if wants_rv && !rb.revisions.is_empty() {
            // Modules may still be arriving; collect briefly.
            let more = tokio::time::Instant::now() + Duration::from_millis(250);
            self.read_until(
                |item| {
                    if let Some(("RV", false, params)) = split_prefix(item) {
                        rb.revisions.push(params.to_owned());
                    }
                    false
                },
                more.min(deadline),
            )
            .await?;
        }
        Ok(rb)
    }
}

/// GET-only CAT link: cannot send setters, by type and by wire check.
#[derive(Debug)]
pub struct ReadOnlyCat {
    link: Link,
}

impl ReadOnlyCat {
    pub fn new(stream: BoxStream, timeout: Duration) -> Self {
        Self {
            link: Link {
                stream,
                buf: Vec::new(),
                timeout,
                sent: Vec::new(),
            },
        }
    }

    /// Commands written so far (diagnostics and tests).
    pub fn sent(&self) -> &[String] {
        &self.link.sent
    }
}

impl CatQuery for ReadOnlyCat {
    async fn query(&mut self, queries: &[Query]) -> Result<Readback, CatError> {
        self.link.query(queries).await
    }
}

/// Link allowed to send whitelisted setters.
#[derive(Debug)]
pub struct ControlCat {
    link: Link,
}

impl ControlCat {
    pub fn new(stream: BoxStream, timeout: Duration, _consent: RetuneConsent) -> Self {
        Self {
            link: Link {
                stream,
                buf: Vec::new(),
                timeout,
                sent: Vec::new(),
            },
        }
    }

    pub fn sent(&self) -> &[String] {
        &self.link.sent
    }
}

impl CatQuery for ControlCat {
    async fn query(&mut self, queries: &[Query]) -> Result<Readback, CatError> {
        self.link.query(queries).await
    }
}

impl CatControl for ControlCat {
    async fn set(&mut self, setter: Setter) -> Result<(), CatError> {
        self.link.write_checked(&setter.wire(), true).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_query_is_a_parameterless_get() {
        for q in Query::ALL {
            check_wire(&q.wire(), false).unwrap();
            assert!(q.wire().ends_with(';'));
        }
        let batch: String = Query::ALL.iter().map(|q| q.wire()).collect();
        check_wire(&batch, false).unwrap();
    }

    #[test]
    fn setters_need_a_control_link_and_valid_ranges() {
        let ok = [
            Setter::Band(5),
            Setter::VfoA(14_045_000),
            Setter::Span(40_000),
            Setter::FixedTune(false),
            Setter::RxInput(RxInput::for_port(k4na_core::config::K4Port::RxAntIn1)),
            Setter::RxInput(RxInput::for_port(k4na_core::config::K4Port::Ant2)),
            Setter::RxInput(RxInput::from_readback("2").unwrap()),
            Setter::Receiver(ReceiverSetter::PreampBasic(0)),
            Setter::Receiver(ReceiverSetter::PreampBasic(3)),
            Setter::Receiver(ReceiverSetter::PreampAdvanced(2, true)),
            Setter::Receiver(ReceiverSetter::AttenuatorOffBasic),
            Setter::Receiver(ReceiverSetter::AttenuatorAdvanced(21, true)),
            Setter::Receiver(ReceiverSetter::AttenuatorAdvanced(0, false)),
            Setter::Receiver(ReceiverSetter::RfGain(0)),
            Setter::Receiver(ReceiverSetter::RfGain(60)),
            Setter::Receiver(ReceiverSetter::AgcBasic { slow: true }),
            Setter::Receiver(ReceiverSetter::AgcBasic { slow: false }),
            Setter::Receiver(ReceiverSetter::AgcAdvanced(0)),
            Setter::Receiver(ReceiverSetter::NoiseBlankerBasic(true)),
            Setter::Receiver(ReceiverSetter::NoiseBlankerAdvanced(15, false)),
            Setter::Receiver(ReceiverSetter::NoiseReduction(10, true)),
            Setter::Receiver(ReceiverSetter::PanNoiseBlanker(2)),
            Setter::Receiver(ReceiverSetter::PanAveraging(1)),
            Setter::Receiver(ReceiverSetter::PanAveraging(20)),
            Setter::SubRx(true),
            Setter::SubRx(false),
            Setter::VfoB(14_045_000),
            Setter::SubSpan(40_000),
            Setter::SubRxInput(RxInput::for_port(k4na_core::config::K4Port::RxAntIn1)),
            Setter::SubRxInput(RxInput::for_port(k4na_core::config::K4Port::Ant2)),
            Setter::SubReceiver(ReceiverSetter::PreampBasic(2)),
            Setter::SubReceiver(ReceiverSetter::PreampAdvanced(2, true)),
            Setter::SubReceiver(ReceiverSetter::AttenuatorOffBasic),
            Setter::SubReceiver(ReceiverSetter::AttenuatorAdvanced(6, true)),
            Setter::SubReceiver(ReceiverSetter::RfGain(10)),
            Setter::SubReceiver(ReceiverSetter::AgcBasic { slow: true }),
            Setter::SubReceiver(ReceiverSetter::AgcAdvanced(1)),
            Setter::SubReceiver(ReceiverSetter::NoiseBlankerBasic(false)),
            Setter::SubReceiver(ReceiverSetter::NoiseBlankerAdvanced(3, true)),
            Setter::SubReceiver(ReceiverSetter::NoiseReduction(5, false)),
            Setter::SubReceiver(ReceiverSetter::PanNoiseBlanker(0)),
        ];
        for s in ok {
            check_wire(&s.wire(), true).unwrap();
            assert!(check_wire(&s.wire(), false).is_err(), "{s:?} allowed on read-only link");
        }
        assert_eq!(Setter::VfoA(7_000_000).wire(), "FA00007000000;");
        assert_eq!(
            Setter::RxInput(RxInput::for_port(k4na_core::config::K4Port::Ant2)).wire(),
            "AR6;"
        );
        assert_eq!(
            Setter::SubRxInput(RxInput::for_port(k4na_core::config::K4Port::Ant2)).wire(),
            "AR$6;"
        );
        assert_eq!(Setter::VfoB(7_000_000).wire(), "FB00007000000;");
        assert_eq!(Setter::SubSpan(40_000).wire(), "#SPN$40000;");
        assert_eq!(Setter::SubReceiver(ReceiverSetter::RfGain(6)).wire(), "RG$-06;");
        assert_eq!(
            Setter::SubReceiver(ReceiverSetter::AgcBasic { slow: false }).wire(),
            "GT$002;"
        );
        assert_eq!(
            Setter::SubReceiver(ReceiverSetter::PreampBasic(1)).query(),
            Query::SubPreamp
        );
        assert_eq!(
            Setter::SubRxInput(RxInput::from_readback("4").unwrap()).query(),
            Query::SubRxAntenna
        );
        // Pan averaging is shared by both pans: no sub form exists.
        assert!(!Setter::SubReceiver(ReceiverSetter::PanAveraging(4)).is_sendable());
        assert!(Setter::VfoB(14_000_000).is_sub() && !Setter::VfoA(14_000_000).is_sub());
        // Disconnect-all and the internal transverter input are not inputs
        // the collector can select or restore.
        assert_eq!(RxInput::from_readback("0"), None);
        assert_eq!(RxInput::from_readback("3"), None);
        assert_eq!(RxInput::from_readback("8"), None);
        for bad in [
            Setter::Band(11),
            Setter::VfoA(99_999),
            Setter::VfoA(54_000_001),
            Setter::Span(5_999),
            Setter::Receiver(ReceiverSetter::PreampBasic(4)),
            Setter::Receiver(ReceiverSetter::AttenuatorAdvanced(4, true)),
            Setter::Receiver(ReceiverSetter::AttenuatorAdvanced(24, true)),
            Setter::Receiver(ReceiverSetter::RfGain(61)),
            Setter::Receiver(ReceiverSetter::AgcAdvanced(3)),
            Setter::Receiver(ReceiverSetter::NoiseBlankerAdvanced(16, true)),
            Setter::Receiver(ReceiverSetter::NoiseReduction(0, true)),
            Setter::Receiver(ReceiverSetter::NoiseReduction(11, true)),
            Setter::Receiver(ReceiverSetter::PanNoiseBlanker(3)),
            Setter::Receiver(ReceiverSetter::PanAveraging(0)),
            Setter::Receiver(ReceiverSetter::PanAveraging(21)),
            Setter::VfoB(99_999),
            Setter::SubSpan(368_001),
            Setter::SubReceiver(ReceiverSetter::PreampBasic(4)),
            Setter::SubReceiver(ReceiverSetter::RfGain(61)),
            Setter::SubReceiver(ReceiverSetter::PanAveraging(4)),
        ] {
            assert!(check_wire(&bad.wire(), true).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn transmit_and_arbitrary_commands_can_never_pass() {
        let forbidden = [
            "TX;",
            "TX1;",
            "RX;",
            "TU1;",
            "TU;",
            "TS1;",
            "KY TEST;",
            "KYW;",
            "SW40;",
            "SWT11;",
            "DA1;",
            "PC050;",
            "EC BN05;",
            "EE0;",
            "PS0;",
            "MN;",
            "AI2;",
            "K31;",
            "K40;",
            "FA$00014000000;",
            "FB$00014000000;",
            "FB0001400000;",
            "BN$05;",
            "BN$$05;",
            "BI0;",
            "MD3;",
            "MD$3;",
            "PA4;",
            "PA12;",
            "PA/;",
            "PA$4;",
            "PA$$1;",
            "SB2;",
            "SB$1;",
            "DV1;",
            "DV0;",
            "BI1;",
            "FT1;",
            "FT0;",
            "RA1;",
            "RA01;",
            "RA011;",
            "RA241;",
            "RG00;",
            "RG-61;",
            "RG+10;",
            "RG-1;",
            "GT3;",
            "GT001;",
            "NB2;",
            "NB161;",
            "NB$161;",
            "NR001;",
            "NR111;",
            "#NB3;",
            "#NBL5;",
            "#AVG00;",
            "#AVG21;",
            "#AVG5;",
            "NA1;",
            "NM01760;",
            "PC050;PA1;",
            "AR0;",
            "AR3;",
            "AR8;",
            "AR12;",
            "AR/;",
            "AR$0;",
            "AR$3;",
            "AR$8;",
            "AN1;",
            "AN2;",
            "AN$1;",
            "AN/;",
            "#SPN$5999;",
            "#AVG$04;",
            "#FXT$1;",
            "#FXA1;",
            "#REF-110;",
            "#REF$-110;",
            "OV$1;",
            "FA14045000;",
            "FA0001404500;",
            "FA;TX;",
            "FA;\nTX;",
            "fa;",
            "FA",
            "",
            ";",
            "ZZ;",
            "FA00014045000;TX;",
        ];
        for f in forbidden {
            assert!(check_wire(f, true).is_err(), "{f:?} passed the whitelist");
        }
    }

    #[test]
    fn prefixes_disambiguate_main_sub_and_hash_forms() {
        assert_eq!(split_prefix("#AR0906+081"), Some(("#AR", false, "0906+081")));
        assert_eq!(split_prefix("AR1"), Some(("AR", false, "1")));
        assert_eq!(split_prefix("PA$1"), Some(("PA$", false, "1")));
        assert_eq!(split_prefix("AR$6"), Some(("AR$", false, "6")));
        assert_eq!(split_prefix("RG$-00"), Some(("RG$", false, "-00")));
        // A `$` form with no known sub query stays marked as sub.
        assert_eq!(split_prefix("MD$3"), Some(("MD", true, "3")));
        assert_eq!(split_prefix("#AVG$04"), Some(("#AVG", true, "04")));
        assert_eq!(split_prefix("K41"), Some(("K4", false, "1")));
        assert_eq!(split_prefix("RV.FP-03.57"), Some(("RV", false, ".FP-03.57")));
        assert_eq!(split_prefix("#SPN40000"), Some(("#SPN", false, "40000")));
        assert_eq!(split_prefix("TX"), None);
    }

    #[test]
    fn consent_requires_both_halves() {
        assert!(RetuneConsent::new(true, true).is_ok());
        assert!(RetuneConsent::new(true, false).is_err());
        assert!(RetuneConsent::new(false, true).is_err());
    }

    #[tokio::test]
    async fn query_parses_batched_responses_and_ignores_others() {
        let (client, mut server) = tokio::io::duplex(4096);
        let mut cat = ReadOnlyCat::new(Box::new(client), Duration::from_millis(500));
        let srv = tokio::spawn(async move {
            let mut buf = [0u8; 256];
            let n = server.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"TQ;FA;#AR;AR;AR$;");
            server
                .write_all(b"FA$00007000000;AR$6;TQ0;?;FA00014045000;AR2;#AR0906+081;")
                .await
                .unwrap();
            server
        });
        let rb = cat
            .query(&[
                Query::Transmit,
                Query::VfoA,
                Query::AutoRef,
                Query::RxAntenna,
                Query::SubRxAntenna,
            ])
            .await
            .unwrap();
        assert_eq!(rb.flag(Query::Transmit), Some(false));
        assert_eq!(rb.u64(Query::VfoA), Some(14_045_000));
        assert_eq!(rb.raw(Query::AutoRef), Some("0906+081"));
        assert_eq!(rb.raw(Query::RxAntenna), Some("2"));
        assert_eq!(rb.raw(Query::SubRxAntenna), Some("6"));
        assert_eq!(cat.sent(), ["TQ;FA;#AR;AR;AR$;"]);
        drop(srv.await.unwrap());
    }

    #[tokio::test(start_paused = true)]
    async fn missing_responses_time_out_as_unknown() {
        let (client, _server) = tokio::io::duplex(4096);
        let mut cat = ReadOnlyCat::new(Box::new(client), Duration::from_millis(300));
        let rb = cat.query(&[Query::Transmit]).await.unwrap();
        assert_eq!(rb.flag(Query::Transmit), None);
        assert_eq!(rb.to_state(&[Query::Transmit])["TQ"], "unknown");
    }
}
