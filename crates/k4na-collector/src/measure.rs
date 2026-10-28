//! CAT codecs for receiver measurement settings (docs/measurement.md).
//!
//! Everything is expressed as readback strings in the radio's current
//! meta-mode: a target is "the readback we want", and a change is only
//! made when both the target and the value found have a whitelisted setter
//! that reads back exactly as that string. A setting whose found value
//! cannot be put back is never changed.

use std::collections::BTreeMap;
use std::fmt;

use k4na_core::measure::{Agc, MeasurementSettings, PanNoiseBlanker, Preamp, band_label};
use serde::{Deserialize, Serialize};

use crate::cat::{Query, Readback, ReceiverSetter, RxInput, Setter};

/// CAT protocol meta-mode read with `K4;`. The collector never changes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetaMode {
    /// `K40`: K3-compatible responses (observed on a K4D).
    Basic,
    /// `K41`: documented K4 responses.
    Advanced,
}

impl MetaMode {
    pub fn from_readback(rb: &Readback) -> Option<Self> {
        match rb.raw(Query::Identify)? {
            "0" => Some(MetaMode::Basic),
            "1" => Some(MetaMode::Advanced),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MetaMode::Basic => "basic",
            MetaMode::Advanced => "advanced",
        }
    }
}

impl fmt::Display for MetaMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Receiver settings a profile may control, in application order.
pub const KEYS: [Query; 8] = [
    Query::Preamp,
    Query::Attenuator,
    Query::RfGain,
    Query::Agc,
    Query::NoiseBlanker,
    Query::NoiseReduction,
    Query::PanNoiseBlanker,
    Query::Averaging,
];

/// Sub-receiver settings a paired profile controls, in application
/// order. `#AVG` is shared by both pans and stays a main-receiver key.
pub const SUB_KEYS: [Query; 7] = [
    Query::SubPreamp,
    Query::SubAttenuator,
    Query::SubRfGain,
    Query::SubAgc,
    Query::SubNoiseBlanker,
    Query::SubNoiseReduction,
    Query::SubPanNoiseBlanker,
];

/// Which receiver a measurement plan targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Receiver {
    Main,
    Sub,
}

impl Receiver {
    pub fn keys(self) -> &'static [Query] {
        match self {
            Receiver::Main => &KEYS,
            Receiver::Sub => &SUB_KEYS,
        }
    }

    /// `q` for this receiver (`PA` -> `PA$` for the sub).
    pub fn form(self, q: Query) -> Query {
        match self {
            Receiver::Main => q,
            Receiver::Sub => q.sub_form().unwrap_or(q),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Receiver::Main => "main",
            Receiver::Sub => "sub",
        }
    }
}

/// The main-receiver form of a sub query (`PA$` -> `PA`), if it is one.
pub fn main_form(q: Query) -> Option<Query> {
    Query::ALL.into_iter().find(|m| m.sub_form() == Some(q))
}

/// Human name of a field for reports and manual instructions.
pub fn label(q: Query) -> &'static str {
    match q {
        Query::SubRx => "sub receiver on/off",
        Query::VfoB => "VFO B frequency",
        Query::SubBand => "sub band",
        Query::SubSpan => "sub pan span",
        Query::SubRxAntenna => "sub RX input",
        Query::SubPreamp => "sub preamp",
        Query::SubAttenuator => "sub attenuator",
        Query::SubRfGain => "sub RF gain",
        Query::SubAgc => "sub AGC",
        Query::SubNoiseBlanker => "sub noise blanker",
        Query::SubNoiseReduction => "sub noise reduction",
        Query::SubPanNoiseBlanker => "sub pan noise blanker",
        Query::Band => "band",
        Query::VfoA => "VFO A frequency",
        Query::Span => "pan span",
        Query::FixedTune => "pan fixed/track mode",
        Query::RxAntenna => "main RX input",
        Query::Preamp => "preamp",
        Query::Attenuator => "attenuator",
        Query::RfGain => "RF gain",
        Query::Agc => "AGC",
        Query::NoiseBlanker => "noise blanker",
        Query::NoiseReduction => "noise reduction",
        Query::PanNoiseBlanker => "pan noise blanker",
        Query::Averaging => "pan averaging",
        _ => "setting",
    }
}

/// Normalized readback for comparisons, so `00014074000` equals the
/// value `FA14074000` sets and `#AVG2` equals `#AVG02`.
pub fn norm(q: Query, raw: &str) -> String {
    match q {
        Query::VfoA | Query::VfoB | Query::Span | Query::SubSpan | Query::Band | Query::SubBand | Query::Averaging => {
            raw.parse::<u64>().map_or_else(|_| raw.to_owned(), |n| n.to_string())
        }
        _ => raw.to_owned(),
    }
}

/// The readback a setter is expected to produce for its query.
pub fn expected_readback(s: Setter) -> String {
    let wire = s.wire();
    let body = wire.strip_suffix(';').unwrap_or(&wire);
    body.strip_prefix(s.query().prefix()).unwrap_or(body).to_owned()
}

/// The whitelisted setter after which `q` reads back `raw` in `mode`, or
/// `None` when the collector cannot produce that value.
pub fn setter_for(q: Query, mode: MetaMode, raw: &str) -> Option<Setter> {
    if q == Query::SubRx {
        let s = match raw {
            "0" => Setter::SubRx(false),
            "1" => Setter::SubRx(true),
            _ => return None,
        };
        return Some(s);
    }
    if let Some(main) = main_form(q) {
        // The sub receiver shares the main grammar with `$` setters.
        let s = match setter_for(main, mode, raw)? {
            Setter::VfoA(hz) => Setter::VfoB(hz),
            Setter::Span(hz) => Setter::SubSpan(hz),
            Setter::RxInput(i) => Setter::SubRxInput(i),
            Setter::Receiver(r) => Setter::SubReceiver(r),
            _ => return None,
        };
        return (s.is_sendable() && s.query() == q && norm(q, &expected_readback(s)) == norm(q, raw)).then_some(s);
    }
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(raw.trim_start_matches('-')) {
        return None;
    }
    let b = raw.as_bytes();
    let d = |i: usize| b[i] - b'0';
    let num = |s: &str| s.parse::<u8>().ok();
    let r = Setter::Receiver;
    let len = raw.len();
    let s = match (q, mode) {
        (Query::Band, _) => Setter::Band(raw.parse().ok()?),
        (Query::VfoA, _) => Setter::VfoA(raw.parse().ok()?),
        (Query::Span, _) => Setter::Span(raw.parse().ok()?),
        (Query::FixedTune, _) if len == 1 && d(0) <= 1 => Setter::FixedTune(d(0) == 1),
        (Query::RxAntenna, _) => Setter::RxInput(RxInput::from_readback(raw)?),
        (Query::Preamp, MetaMode::Basic) if len == 1 => r(ReceiverSetter::PreampBasic(d(0))),
        (Query::Preamp, MetaMode::Advanced) if len == 2 && d(1) <= 1 => {
            r(ReceiverSetter::PreampAdvanced(d(0), d(1) == 1))
        }
        (Query::Attenuator, MetaMode::Basic) if raw == "00" => r(ReceiverSetter::AttenuatorOffBasic),
        (Query::Attenuator, MetaMode::Advanced) if len == 3 && d(2) <= 1 => {
            r(ReceiverSetter::AttenuatorAdvanced(num(&raw[..2])?, d(2) == 1))
        }
        (Query::RfGain, _) => r(ReceiverSetter::RfGain(num(raw.strip_prefix('-')?)?)),
        (Query::Agc, MetaMode::Basic) => match raw {
            "002" => r(ReceiverSetter::AgcBasic { slow: false }),
            "004" => r(ReceiverSetter::AgcBasic { slow: true }),
            _ => return None,
        },
        (Query::Agc, MetaMode::Advanced) if len == 1 => r(ReceiverSetter::AgcAdvanced(d(0))),
        (Query::NoiseBlanker, MetaMode::Basic) if len == 1 && d(0) <= 1 => {
            r(ReceiverSetter::NoiseBlankerBasic(d(0) == 1))
        }
        (Query::NoiseBlanker, MetaMode::Advanced) if len == 3 && d(2) <= 1 => {
            r(ReceiverSetter::NoiseBlankerAdvanced(num(&raw[..2])?, d(2) == 1))
        }
        (Query::NoiseReduction, _) if len == 3 && d(2) <= 1 => {
            r(ReceiverSetter::NoiseReduction(num(&raw[..2])?, d(2) == 1))
        }
        (Query::PanNoiseBlanker, _) if len == 1 => r(ReceiverSetter::PanNoiseBlanker(d(0))),
        (Query::Averaging, _) => r(ReceiverSetter::PanAveraging(num(raw)?)),
        _ => return None,
    };
    (s.is_sendable() && norm(q, &expected_readback(s)) == norm(q, raw)).then_some(s)
}

/// Target readback for `q` under `settings`, given the value found.
/// `Ok(None)`: leave as found. `Err`: the setting cannot be expressed in
/// this meta-mode or on this band.
pub fn target(
    q: Query,
    mode: MetaMode,
    s: &MeasurementSettings,
    band: Option<u8>,
    found: Option<&str>,
) -> Result<Option<String>, String> {
    let basic = mode == MetaMode::Basic;
    // Level digits kept from the found readback (advanced forms).
    let kept = |n: usize| {
        found
            .and_then(|f| f.get(..n))
            .filter(|l| l.bytes().all(|b| b.is_ascii_digit()))
            .map(str::to_owned)
            .ok_or_else(|| format!("{} readback {found:?} has no level to keep", q.prefix()))
    };
    let unsupported = |what: &str| Err(format!("{what} cannot be set or verified in {mode} CAT mode (K40)"));
    let on = |b: bool| u8::from(b).to_string();
    let t = match q {
        Query::Preamp => match s.preamp.value() {
            None => None,
            Some(p) if !p.valid_on(band) => return Err(format!("preamp {p:?} does not exist on {}", band_label(band))),
            Some(p) if basic => Some(p.level().to_string()),
            Some(Preamp::Off) => Some(format!("{}0", kept(1)?)),
            Some(p) => Some(format!("{}1", p.level())),
        },
        Query::Attenuator => match s.attenuator_db.value() {
            None => None,
            Some(0) if basic => Some("00".to_owned()),
            Some(_) if basic => return unsupported("attenuator_db above 0"),
            Some(0) => Some(format!("{}0", kept(2)?)),
            Some(db) => Some(format!("{db:02}1")),
        },
        Query::RfGain => s.rf_gain_db.value().map(|g| format!("-{g:02}")),
        Query::Agc => match s.agc.value() {
            None => None,
            Some(Agc::Off) if basic => return unsupported("agc = \"off\""),
            Some(Agc::Slow) if basic => Some("004".to_owned()),
            Some(Agc::Fast) if basic => Some("002".to_owned()),
            Some(a) => Some(
                match a {
                    Agc::Off => "0",
                    Agc::Slow => "1",
                    Agc::Fast => "2",
                }
                .to_owned(),
            ),
        },
        Query::NoiseBlanker => match s.noise_blanker.value() {
            None => None,
            Some(b) if basic => Some(on(b)),
            Some(b) => Some(format!("{}{}", kept(2)?, on(b))),
        },
        Query::NoiseReduction => match s.noise_reduction.value() {
            None => None,
            Some(b) => Some(format!("{}{}", kept(2)?, on(b))),
        },
        Query::PanNoiseBlanker => s.pan_noise_blanker.value().map(|n| {
            match n {
                PanNoiseBlanker::Off => "0",
                PanNoiseBlanker::On => "1",
                PanNoiseBlanker::Auto => "2",
            }
            .to_owned()
        }),
        Query::Averaging => s.pan_averaging.value().map(|n| n.to_string()),
        _ => None,
    };
    Ok(t)
}

/// One planned measurement change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change {
    pub query: Query,
    pub found: String,
    pub target: String,
    pub setter: Setter,
}

/// The changes that bring a context to `settings`. `Err` names why the
/// context cannot be measured as configured (nothing is changed then).
pub fn plan(
    mode: MetaMode,
    settings: &MeasurementSettings,
    band: Option<u8>,
    rb: &Readback,
) -> Result<Vec<Change>, String> {
    plan_for(Receiver::Main, mode, settings, band, rb)
}

/// [`plan`] for either receiver; the sub uses its `$` forms and leaves
/// the shared pan averaging to the main plan.
pub fn plan_for(
    rx: Receiver,
    mode: MetaMode,
    settings: &MeasurementSettings,
    band: Option<u8>,
    rb: &Readback,
) -> Result<Vec<Change>, String> {
    let mut out = Vec::new();
    let mut problems = Vec::new();
    for &q in rx.keys() {
        let found = rb.raw(q);
        let main = main_form(q).unwrap_or(q);
        let t = match target(main, mode, settings, band, found) {
            Ok(Some(t)) => t,
            Ok(None) => continue,
            Err(e) => {
                problems.push(e);
                continue;
            }
        };
        let Some(found) = found else {
            problems.push(format!("{} ({}) is unreadable", label(q), q.prefix()));
            continue;
        };
        if norm(q, found) == norm(q, &t) {
            continue;
        }
        let Some(setter) = setter_for(q, mode, &t) else {
            problems.push(format!(
                "{} target {}{t} is not a whitelisted setter",
                label(q),
                q.prefix()
            ));
            continue;
        };
        if setter_for(q, mode, found).is_none() {
            problems.push(format!(
                "{} is {}{found}, which the collector cannot put back in {mode} CAT mode",
                label(q),
                q.prefix()
            ));
            continue;
        }
        out.push(Change {
            query: q,
            found: found.to_owned(),
            target: t,
            setter,
        });
    }
    if problems.is_empty() {
        Ok(out)
    } else {
        Err(problems.join("; "))
    }
}

/// The profile settings recorded with each visit (`as-found` or the
/// configured value), the CAT meta-mode the readbacks are in, and the
/// effective values as verified by readback (`verified.<prefix>`, raw
/// readback text in that mode).
pub fn visit_settings(s: &MeasurementSettings, mode: MetaMode, verified: &Readback) -> BTreeMap<String, String> {
    visit_settings_for(Receiver::Main, s, mode, verified)
}

/// [`visit_settings`] for the receiver that produced a pan. Keys are the
/// main-form prefixes (`verified.PA`) so both pans read alike; the sub's
/// values come from its `$` readbacks, and shared `#AVG` from the main.
pub fn visit_settings_for(
    rx: Receiver,
    s: &MeasurementSettings,
    mode: MetaMode,
    verified: &Readback,
) -> BTreeMap<String, String> {
    let mut m = s.to_map();
    m.insert("cat_meta_mode".into(), mode.as_str().into());
    // Only the sub is marked, so main-receiver segment identities are
    // unchanged from single-receiver surveys.
    if rx == Receiver::Sub {
        m.insert("receiver".into(), "sub".into());
    }
    for q in KEYS {
        let own = if q == Query::Averaging { q } else { rx.form(q) };
        if let Some(raw) = verified.raw(own) {
            m.insert(format!("verified.{}", q.prefix()), raw.to_owned());
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use k4na_core::measure::Desired;

    use super::*;

    fn rb(pairs: &[(Query, &str)]) -> Readback {
        let mut r = Readback::default();
        for (q, v) in pairs {
            r.insert(*q, *v);
        }
        r
    }

    /// The K4D's observed basic-mode readbacks (`k4na probe`, R39).
    fn k4d() -> Readback {
        rb(&[
            (Query::Identify, "0"),
            (Query::Preamp, "1"),
            (Query::Attenuator, "00"),
            (Query::RfGain, "-00"),
            (Query::Agc, "002"),
            (Query::NoiseBlanker, "0"),
            (Query::NoiseReduction, "010"),
            (Query::PanNoiseBlanker, "0"),
            (Query::Averaging, "2"),
        ])
    }

    #[test]
    fn observed_k4d_values_are_all_restorable_in_basic_mode() {
        let r = k4d();
        assert_eq!(MetaMode::from_readback(&r), Some(MetaMode::Basic));
        for q in KEYS {
            let raw = r.raw(q).unwrap();
            let s = setter_for(q, MetaMode::Basic, raw).unwrap_or_else(|| panic!("{q:?} {raw}"));
            assert!(s.is_sendable());
            assert_eq!(norm(q, &expected_readback(s)), norm(q, raw), "{q:?}");
        }
    }

    #[test]
    fn built_in_defaults_on_the_k4d_change_nothing() {
        let changes = plan(MetaMode::Basic, &MeasurementSettings::default(), Some(3), &k4d()).unwrap();
        assert!(changes.is_empty(), "{changes:?}");
    }

    #[test]
    fn basic_mode_plan_uses_k3_forms() {
        let s = MeasurementSettings {
            preamp: Desired::Set(Preamp::Pre2),
            agc: Desired::Set(Agc::Slow),
            noise_reduction: Desired::Set(true),
            pan_averaging: Desired::Set(4),
            ..MeasurementSettings::default()
        };
        let wires: Vec<String> = plan(MetaMode::Basic, &s, Some(3), &k4d())
            .unwrap()
            .into_iter()
            .map(|c| c.setter.wire())
            .collect();
        assert_eq!(wires, ["PA2;", "GT004;", "NR011;", "#AVG04;"]);
    }

    #[test]
    fn advanced_mode_keeps_levels_when_switching_off() {
        let r = rb(&[
            (Query::Preamp, "21"),
            (Query::Attenuator, "091"),
            (Query::NoiseBlanker, "051"),
            (Query::NoiseReduction, "031"),
        ]);
        let s = MeasurementSettings {
            preamp: Desired::Set(Preamp::Off),
            rf_gain_db: Desired::AsFound,
            pan_noise_blanker: Desired::AsFound,
            ..MeasurementSettings::default()
        };
        let wires: Vec<String> = plan(MetaMode::Advanced, &s, Some(3), &r)
            .unwrap()
            .into_iter()
            .map(|c| c.setter.wire())
            .collect();
        assert_eq!(wires, ["PA20;", "RA090;", "NB050;", "NR030;"]);
    }

    #[test]
    fn unsupported_or_unrestorable_changes_are_refused() {
        let att = MeasurementSettings {
            attenuator_db: Desired::Set(6),
            ..MeasurementSettings::as_found()
        };
        let e = plan(MetaMode::Basic, &att, Some(3), &k4d()).unwrap_err();
        assert!(e.contains("attenuator_db above 0 cannot be set"), "{e}");

        // Basic RA01 (on, level unknown) cannot be put back exactly.
        let mut r = k4d();
        r.insert(Query::Attenuator, "01");
        let e = plan(MetaMode::Basic, &MeasurementSettings::default(), Some(3), &r).unwrap_err();
        assert!(e.contains("RA01, which the collector cannot put back"), "{e}");

        let pre3 = MeasurementSettings {
            preamp: Desired::Set(Preamp::Pre3),
            ..MeasurementSettings::as_found()
        };
        assert!(plan(MetaMode::Basic, &pre3, Some(3), &k4d()).is_err());
        assert!(plan(MetaMode::Basic, &pre3, Some(9), &k4d()).is_ok());

        let mut r = k4d();
        r.remove(Query::NoiseBlanker);
        let e = plan(MetaMode::Basic, &MeasurementSettings::default(), Some(3), &r).unwrap_err();
        assert!(e.contains("noise blanker (NB) is unreadable"), "{e}");
    }

    #[test]
    fn setter_for_rejects_values_outside_the_whitelist() {
        assert_eq!(setter_for(Query::Span, MetaMode::Basic, "5000"), None);
        assert_eq!(setter_for(Query::RxAntenna, MetaMode::Basic, "3"), None);
        assert_eq!(setter_for(Query::Agc, MetaMode::Basic, "000"), None);
        assert_eq!(setter_for(Query::Preamp, MetaMode::Advanced, "22"), None);
        assert_eq!(setter_for(Query::Attenuator, MetaMode::Advanced, "041"), None);
        assert_eq!(setter_for(Query::Averaging, MetaMode::Basic, "0"), None);
        assert_eq!(
            setter_for(Query::VfoA, MetaMode::Basic, "00007032000").map(Setter::wire),
            Some("FA00007032000;".to_owned())
        );
    }
}
