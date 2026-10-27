//! Receiver measurement settings for survey profiles: defaults plus
//! band, antenna and antenna+band overrides. See docs/measurement.md.
//!
//! Every field is either `as-found` (left as the radio has it, recorded)
//! or a value the survey sets, verifies by readback before accepting
//! data, and restores afterwards.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::band::{k4_band_from_name, k4_band_name};

/// Preamp selection (`PA`). `Pre3` (10 dB preamp + 20 dB LNA) exists on
/// 12-6 m only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preamp {
    Off,
    Pre1,
    Pre2,
    Pre3,
}

impl Preamp {
    pub fn level(self) -> u8 {
        match self {
            Preamp::Off => 0,
            Preamp::Pre1 => 1,
            Preamp::Pre2 => 2,
            Preamp::Pre3 => 3,
        }
    }

    /// Whether this selection exists on K4 band `band` (`BN` number;
    /// `None` = outside the amateur bands).
    pub fn valid_on(self, band: Option<u8>) -> bool {
        self != Preamp::Pre3 || matches!(band, Some(8..=10))
    }
}

/// AGC mode (`GT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Agc {
    Off,
    Slow,
    Fast,
}

/// Panadapter (DDC) noise blanker (`#NB`). `Auto` follows the receiver NB.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PanNoiseBlanker {
    Off,
    On,
    Auto,
}

/// `as-found` or a value to set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub enum Desired<T> {
    #[default]
    AsFound,
    Set(T),
}

impl<T: Copy> Desired<T> {
    pub fn value(self) -> Option<T> {
        match self {
            Desired::AsFound => None,
            Desired::Set(v) => Some(v),
        }
    }
}

const AS_FOUND: &str = "as-found";

impl<T: Serialize> Serialize for Desired<T> {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Desired::AsFound => s.serialize_str(AS_FOUND),
            Desired::Set(v) => v.serialize(s),
        }
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Desired<T> {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let v = toml::Value::deserialize(d)?;
        if v.as_str() == Some(AS_FOUND) {
            return Ok(Desired::AsFound);
        }
        T::deserialize(v).map(Desired::Set).map_err(serde::de::Error::custom)
    }
}

/// One layer of settings: unset fields fall through to the layer below.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsLayer {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preamp: Option<Desired<Preamp>>,
    /// 0 (off) or 3..=21 in 3 dB steps.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attenuator_db: Option<Desired<u8>>,
    /// 0..=60 dB of DSP RF-gain reduction (`RG-nn`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rf_gain_db: Option<Desired<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agc: Option<Desired<Agc>>,
    /// Receiver noise blanker on/off (its level is left as found).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise_blanker: Option<Desired<bool>>,
    /// Receiver noise reduction on/off (its level is left as found).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noise_reduction: Option<Desired<bool>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pan_noise_blanker: Option<Desired<PanNoiseBlanker>>,
    /// Panadapter averaging 1..=20 (`#AVG`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pan_averaging: Option<Desired<u8>>,
}

impl SettingsLayer {
    fn over(&self, base: &mut MeasurementSettings) {
        macro_rules! take {
            ($($f:ident),*) => { $( if let Some(v) = self.$f { base.$f = v; } )* };
        }
        take!(
            preamp,
            attenuator_db,
            rf_gain_db,
            agc,
            noise_blanker,
            noise_reduction,
            pan_noise_blanker,
            pan_averaging
        );
    }

    fn is_empty(&self) -> bool {
        *self == SettingsLayer::default()
    }
}

/// Effective settings for one band and antenna.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
pub struct MeasurementSettings {
    pub preamp: Desired<Preamp>,
    pub attenuator_db: Desired<u8>,
    pub rf_gain_db: Desired<u8>,
    pub agc: Desired<Agc>,
    pub noise_blanker: Desired<bool>,
    pub noise_reduction: Desired<bool>,
    pub pan_noise_blanker: Desired<PanNoiseBlanker>,
    pub pan_averaging: Desired<u8>,
}

impl Default for MeasurementSettings {
    /// Built-in defaults: the front end and DSP that alter the spectrum are
    /// put in a known state; preamp, AGC and averaging are left as found
    /// (and recorded) unless a profile chooses them.
    fn default() -> Self {
        Self {
            preamp: Desired::AsFound,
            attenuator_db: Desired::Set(0),
            rf_gain_db: Desired::Set(0),
            agc: Desired::AsFound,
            noise_blanker: Desired::Set(false),
            noise_reduction: Desired::Set(false),
            pan_noise_blanker: Desired::Set(PanNoiseBlanker::Off),
            pan_averaging: Desired::AsFound,
        }
    }
}

impl MeasurementSettings {
    /// Settings that leave the radio entirely as found.
    pub fn as_found() -> Self {
        Self {
            preamp: Desired::AsFound,
            attenuator_db: Desired::AsFound,
            rf_gain_db: Desired::AsFound,
            agc: Desired::AsFound,
            noise_blanker: Desired::AsFound,
            noise_reduction: Desired::AsFound,
            pan_noise_blanker: Desired::AsFound,
            pan_averaging: Desired::AsFound,
        }
    }

    /// Field name to `as-found` or the configured value, as recorded with
    /// each visit.
    pub fn to_map(&self) -> BTreeMap<String, String> {
        fn d<T: Serialize>(v: Desired<T>) -> String {
            match v {
                Desired::AsFound => AS_FOUND.to_owned(),
                Desired::Set(x) => toml::Value::try_from(x).map_or_else(
                    |_| "?".to_owned(),
                    |v| match v {
                        toml::Value::String(s) => s,
                        other => other.to_string(),
                    },
                ),
            }
        }
        [
            ("preamp", d(self.preamp)),
            ("attenuator_db", d(self.attenuator_db)),
            ("rf_gain_db", d(self.rf_gain_db)),
            ("agc", d(self.agc)),
            ("noise_blanker", d(self.noise_blanker)),
            ("noise_reduction", d(self.noise_reduction)),
            ("pan_noise_blanker", d(self.pan_noise_blanker)),
            ("pan_averaging", d(self.pan_averaging)),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v))
        .collect()
    }

    /// Problems with these settings on K4 band `band`.
    pub fn problems(&self, band: Option<u8>) -> Vec<String> {
        let mut p = Vec::new();
        if let Desired::Set(pre) = self.preamp
            && !pre.valid_on(band)
        {
            p.push(format!("preamp pre3 exists on 12-6 m only, not {}", band_label(band)));
        }
        if let Desired::Set(db) = self.attenuator_db
            && (db > 21 || db % 3 != 0)
        {
            p.push(format!("attenuator_db {db} must be 0..=21 in 3 dB steps"));
        }
        if let Desired::Set(db) = self.rf_gain_db
            && db > 60
        {
            p.push(format!("rf_gain_db {db} must be 0..=60"));
        }
        if let Desired::Set(n) = self.pan_averaging
            && !(1..=20).contains(&n)
        {
            p.push(format!("pan_averaging {n} must be 1..=20"));
        }
        p
    }
}

impl fmt::Display for MeasurementSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn d<T: fmt::Debug + Copy>(v: Desired<T>) -> String {
            match v {
                Desired::AsFound => AS_FOUND.to_owned(),
                Desired::Set(x) => format!("{x:?}").to_lowercase(),
            }
        }
        let s = self;
        write!(
            f,
            "preamp {}, attenuator {} dB, rf gain -{} dB, agc {}, nb {}, nr {}, pan nb {}, pan avg {}",
            d(s.preamp),
            d(s.attenuator_db),
            d(s.rf_gain_db),
            d(s.agc),
            d(s.noise_blanker),
            d(s.noise_reduction),
            d(s.pan_noise_blanker),
            d(s.pan_averaging)
        )
    }
}

/// A profile override. `band` is a K4 band name (`"40m"`), not a range id,
/// because the radio stores settings per K4 band.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Override {
    #[serde(default)]
    pub band: Option<String>,
    #[serde(default)]
    pub antenna: Option<String>,
    #[serde(flatten)]
    pub settings: SettingsLayer,
}

/// `[measurement]` in a survey profile.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct MeasurementConfig {
    #[serde(flatten)]
    pub defaults: SettingsLayer,
    #[serde(rename = "override", skip_serializing_if = "Vec::is_empty")]
    pub overrides: Vec<Override>,
}

// serde's `flatten` ignores `deny_unknown_fields`, so split the table by
// hand and let `SettingsLayer` reject anything unknown.
fn take<'de, D: Deserializer<'de>, T: serde::de::DeserializeOwned>(
    table: &mut toml::Table,
    key: &str,
) -> Result<Option<T>, D::Error> {
    table
        .remove(key)
        .map(|v| {
            v.try_into()
                .map_err(|e: toml::de::Error| serde::de::Error::custom(format!("{key}: {e}")))
        })
        .transpose()
}

fn layer<'de, D: Deserializer<'de>>(table: toml::Table) -> Result<SettingsLayer, D::Error> {
    toml::Value::Table(table).try_into().map_err(serde::de::Error::custom)
}

impl<'de> Deserialize<'de> for Override {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut t = toml::Table::deserialize(d)?;
        Ok(Self {
            band: take::<D, _>(&mut t, "band")?,
            antenna: take::<D, _>(&mut t, "antenna")?,
            settings: layer::<D>(t)?,
        })
    }
}

impl<'de> Deserialize<'de> for MeasurementConfig {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut t = toml::Table::deserialize(d)?;
        Ok(Self {
            overrides: take::<D, _>(&mut t, "override")?.unwrap_or_default(),
            defaults: layer::<D>(t)?,
        })
    }
}

impl MeasurementConfig {
    /// Effective settings: built-in defaults, then `[measurement]`, then
    /// band, antenna and antenna+band overrides (most specific wins, per
    /// field).
    pub fn resolve(&self, band: Option<u8>, antenna: &str) -> MeasurementSettings {
        let mut s = MeasurementSettings::default();
        self.defaults.over(&mut s);
        let name = band.and_then(k4_band_name);
        let matches = |o: &&Override, want_band: bool, want_antenna: bool| {
            o.band.is_some() == want_band
                && o.antenna.is_some() == want_antenna
                && o.band.as_deref().is_none_or(|b| name == Some(b))
                && o.antenna.as_deref().is_none_or(|a| a == antenna)
        };
        for (want_band, want_antenna) in [(true, false), (false, true), (true, true)] {
            for o in self.overrides.iter().filter(|o| matches(o, want_band, want_antenna)) {
                o.settings.over(&mut s);
            }
        }
        s
    }

    /// Validation against the profile's antennas and K4 bands.
    pub fn problems(&self, antennas: &[String], bands: &[Option<u8>]) -> Vec<String> {
        let mut p = Vec::new();
        let mut seen = BTreeMap::new();
        for (i, o) in self.overrides.iter().enumerate() {
            let at = format!("measurement.override[{i}]");
            if o.band.is_none() && o.antenna.is_none() {
                p.push(format!("{at} needs a band, an antenna or both"));
            }
            if let Some(b) = &o.band
                && k4_band_from_name(b).is_none()
            {
                p.push(format!("{at} band {b:?} is not a K4 band name (160m..6m)"));
            }
            if let Some(a) = &o.antenna
                && !antennas.contains(a)
            {
                p.push(format!("{at} antenna {a:?} is not one of the profile's antennas"));
            }
            if o.settings.is_empty() {
                p.push(format!("{at} sets nothing"));
            }
            if let Some(prev) = seen.insert((o.band.clone(), o.antenna.clone()), i) {
                p.push(format!("{at} repeats the selector of measurement.override[{prev}]"));
            }
        }
        let mut reported = std::collections::BTreeSet::new();
        for b in bands {
            for a in antennas {
                for problem in self.resolve(*b, a).problems(*b) {
                    if reported.insert(problem.clone()) {
                        p.push(format!("measurement for {a} on {}: {problem}", band_label(*b)));
                    }
                }
            }
        }
        p
    }
}

/// `"40m"`, or `"general coverage"` outside the amateur bands.
pub fn band_label(band: Option<u8>) -> &'static str {
    band.and_then(k4_band_name).unwrap_or("general coverage")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> MeasurementConfig {
        #[derive(Deserialize)]
        struct W {
            measurement: MeasurementConfig,
        }
        toml::from_str::<W>(text).unwrap().measurement
    }

    #[test]
    fn layers_resolve_most_specific_last() {
        let m = parse(
            r#"
[measurement]
preamp = "pre1"
agc = "fast"

[[measurement.override]]
band = "10m"
preamp = "pre2"
attenuator_db = 6

[[measurement.override]]
antenna = "rx1"
preamp = "pre2"
rf_gain_db = 3

[[measurement.override]]
band = "10m"
antenna = "rx1"
preamp = "off"
"#,
        );
        let forty_dxc = m.resolve(Some(3), "dxc");
        assert_eq!(forty_dxc.preamp, Desired::Set(Preamp::Pre1));
        assert_eq!(forty_dxc.agc, Desired::Set(Agc::Fast));
        assert_eq!(forty_dxc.attenuator_db, Desired::Set(0), "built-in default");
        assert_eq!(forty_dxc.pan_averaging, Desired::AsFound);
        let ten_dxc = m.resolve(Some(9), "dxc");
        assert_eq!(
            (ten_dxc.preamp, ten_dxc.attenuator_db),
            (Desired::Set(Preamp::Pre2), Desired::Set(6))
        );
        let forty_rx1 = m.resolve(Some(3), "rx1");
        assert_eq!(
            (forty_rx1.preamp, forty_rx1.rf_gain_db),
            (Desired::Set(Preamp::Pre2), Desired::Set(3))
        );
        let ten_rx1 = m.resolve(Some(9), "rx1");
        // Antenna+band beats antenna beats band; other fields fall through.
        assert_eq!(ten_rx1.preamp, Desired::Set(Preamp::Off));
        assert_eq!(ten_rx1.attenuator_db, Desired::Set(6));
        assert_eq!(ten_rx1.rf_gain_db, Desired::Set(3));
    }

    #[test]
    fn as_found_is_explicit_and_absent_section_uses_built_in_defaults() {
        let m = parse(
            r#"
[measurement]
attenuator_db = "as-found"
noise_reduction = "as-found"
"#,
        );
        let s = m.resolve(Some(5), "rx1");
        assert_eq!(s.attenuator_db, Desired::AsFound);
        assert_eq!(s.noise_reduction, Desired::AsFound);
        assert_eq!(s.noise_blanker, Desired::Set(false));
        assert_eq!(
            MeasurementConfig::default().resolve(None, "x"),
            MeasurementSettings::default()
        );
        let map = s.to_map();
        assert_eq!(map["attenuator_db"], "as-found");
        assert_eq!(map["rf_gain_db"], "0");
        assert_eq!(map["noise_blanker"], "false");
        assert_eq!(map["pan_noise_blanker"], "off");
    }

    #[test]
    fn problems_are_specific() {
        let m = parse(
            r#"
[measurement]
preamp = "pre3"
attenuator_db = 4

[[measurement.override]]
band = "40m-cw"
agc = "slow"

[[measurement.override]]
antenna = "nope"
agc = "slow"

[[measurement.override]]
band = "10m"
"#,
        );
        let p = m.problems(&["rx1".into()], &[Some(3), Some(9), None]).join("\n");
        assert!(p.contains("pre3 exists on 12-6 m only, not 40m"), "{p}");
        assert!(!p.contains("not 10m"), "{p}");
        assert!(p.contains("attenuator_db 4"), "{p}");
        assert!(p.contains("\"40m-cw\" is not a K4 band name"), "{p}");
        assert!(p.contains("\"nope\" is not one of"), "{p}");
        assert!(p.contains("override[2] sets nothing"), "{p}");
    }

    #[test]
    fn unknown_keys_and_values_are_rejected() {
        #[derive(Debug, Deserialize)]
        #[allow(dead_code)]
        struct W {
            measurement: MeasurementConfig,
        }
        assert!(toml::from_str::<W>("[measurement]\npreamp = \"pre9\"\n").is_err());
        assert!(toml::from_str::<W>("[measurement]\ngain = 1\n").is_err());
    }
}
