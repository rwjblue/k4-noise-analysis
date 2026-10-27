//! Receive-only K4 CAT (TCP 9200) access through a reviewed, typed whitelist.
//!
//! Safety layers:
//! 1. Callers can only express commands through [`Query`] (GET forms) and
//!    [`Setter`] (`BN`, `FA`, `#SPN`, `#FXT`, `AR`). There is no string API.
//!    `AR` only routes an input to the main receiver; the TX antenna (`AN`)
//!    can be read but never set.
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
}

impl Query {
    pub const ALL: [Query; 31] = [
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
    ];

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
        }
    }

    pub fn wire(self) -> String {
        format!("{};", self.prefix())
    }

    fn from_prefix(p: &str) -> Option<Query> {
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
/// band select, VFO A frequency, pan span, pan track/fixed mode and the
/// main-RX input.
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
}

impl Setter {
    pub fn wire(self) -> String {
        match self {
            Setter::Band(n) => format!("BN{n:02};"),
            Setter::VfoA(hz) => format!("FA{hz:011};"),
            Setter::Span(hz) => format!("#SPN{hz};"),
            Setter::FixedTune(fixed) => format!("#FXT{};", u8::from(fixed)),
            Setter::RxInput(i) => format!("AR{};", i.0),
        }
    }
}

const SETTER_PREFIXES: [&str; 5] = ["BN", "FA", "#SPN", "#FXT", "AR"];

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
/// known prefix wins (`#AR` vs `AR`); `$` (sub receiver) is kept with the
/// prefix so main/sub responses can never be confused.
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
        if !allow_setters || !SETTER_PREFIXES.contains(&prefix) || !params.bytes().all(|b| b.is_ascii_digit()) {
            return Err(unsafe_err());
        }
        let n: u64 = params.parse().map_err(|_| unsafe_err())?;
        let ok = match prefix {
            "BN" => params.len() == 2 && n <= 10,
            "FA" => params.len() == 11 && (100_000..=54_000_000).contains(&n),
            "#SPN" => (6_000..=368_000).contains(&n),
            "#FXT" => params.len() == 1 && n <= 1,
            "AR" => params.len() == 1 && rx_input_ok(n as u8),
            _ => false,
        };
        if !ok {
            return Err(unsafe_err());
        }
    }
    Ok(())
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
            "FB00014000000;",
            "FA$00014000000;",
            "BN$05;",
            "MD3;",
            "PA1;",
            "RA1;",
            "AR0;",
            "AR3;",
            "AR8;",
            "AR12;",
            "AR/;",
            "AR$4;",
            "AN1;",
            "AN2;",
            "AN/;",
            "#SPN$40000;",
            "#FXA1;",
            "#REF-110;",
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
        assert_eq!(split_prefix("PA$1"), Some(("PA", true, "1")));
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
            assert_eq!(&buf[..n], b"TQ;FA;#AR;AR;");
            server
                .write_all(b"FA$00007000000;TQ0;?;FA00014045000;AR2;#AR0906+081;")
                .await
                .unwrap();
            server
        });
        let rb = cat
            .query(&[Query::Transmit, Query::VfoA, Query::AutoRef, Query::RxAntenna])
            .await
            .unwrap();
        assert_eq!(rb.flag(Query::Transmit), Some(false));
        assert_eq!(rb.u64(Query::VfoA), Some(14_045_000));
        assert_eq!(rb.raw(Query::AutoRef), Some("0906+081"));
        assert_eq!(rb.raw(Query::RxAntenna), Some("2"));
        assert_eq!(cat.sent(), ["TQ;FA;#AR;AR;"]);
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
