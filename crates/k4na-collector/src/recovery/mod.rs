//! Crash-safe radio-state preservation for surveys (docs/recovery.md).
//!
//! A survey is a recoverable transaction. Before any CAT change it
//! durably journals what it found (the original state, then each band and
//! RX-input context as it arrives there) and the change it is about to
//! make; after each change it journals the verified readback. Replaying a
//! journal yields a [`Ledger`] of restoration obligations, which the
//! survey's own put-backs and `k4na recovery restore` both settle with the
//! same conflict rule: a field still holding a value the collector set is
//! restored; a field holding anything else was changed by someone else and
//! is left alone.

mod journal;
mod restore;

use std::collections::BTreeMap;

use k4na_core::measure::band_label;
use serde::{Deserialize, Serialize};

pub use journal::{COMPACT_BYTES, JournalError, JournalInfo, JournalSink, JournalStatus, Transaction, list, load};
pub use restore::{
    Decision, FieldReport, RestoreReport, decide, manual_steps, plan, restore, state_queries, wire_value,
};

use crate::cat::Query;
use crate::measure::{self, MetaMode};

/// Where a K4 setting lives, as far as the collector can tell. Scopes are
/// conservative: a setting may really be global, but restoring it per
/// context is still correct.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    /// The operating band (`BN`).
    Session,
    /// A band's own memory: `FA`, `#SPN`, `#FXT` and its RX input `AR`.
    Band(u8),
    /// Receiver settings on a band with a given RX input (`AR` raw value).
    Context(u8, String),
    /// The sub receiver on/off (`SB`).
    SubSession,
    /// The sub receiver's memory on a band: `FB`, `#SPN$` and its RX input
    /// `AR$`. Paired surveys require `BI0`, so VFO B is on the main band.
    SubBand(u8),
    /// Sub-receiver settings on a band with a given sub RX input (`AR$`).
    SubContext(u8, String),
}

impl Scope {
    pub fn band(&self) -> Option<u8> {
        match self {
            Scope::Session | Scope::SubSession => None,
            Scope::Band(b) | Scope::Context(b, _) | Scope::SubBand(b) | Scope::SubContext(b, _) => Some(*b),
        }
    }

    /// Whether this scope holds sub-receiver state.
    pub fn is_sub(&self) -> bool {
        matches!(self, Scope::SubSession | Scope::SubBand(_) | Scope::SubContext(..))
    }

    /// Restored last: the band change and the sub receiver on/off.
    pub fn is_session(&self) -> bool {
        matches!(self, Scope::Session | Scope::SubSession)
    }

    pub fn describe(&self) -> String {
        match self {
            Scope::Session => "the radio".to_owned(),
            Scope::Band(b) => format!("{} (BN{b:02})", band_label(Some(*b))),
            Scope::Context(b, ar) => format!("{} (BN{b:02}) with main RX input AR{ar}", band_label(Some(*b))),
            Scope::SubSession => "the sub receiver".to_owned(),
            Scope::SubBand(b) => format!("{} (BN{b:02}) on the sub receiver", band_label(Some(*b))),
            Scope::SubContext(b, ar) => {
                format!("{} (BN{b:02}) with sub RX input AR${ar}", band_label(Some(*b)))
            }
        }
    }
}

/// Fields captured per scope.
pub const BAND_FIELDS: [Query; 4] = [Query::VfoA, Query::Span, Query::FixedTune, Query::RxAntenna];

/// The sub receiver's band memory fields.
pub const SUB_BAND_FIELDS: [Query; 3] = [Query::VfoB, Query::SubSpan, Query::SubRxAntenna];

pub fn fields(scope: &Scope) -> Vec<Query> {
    match scope {
        Scope::Session => vec![Query::Band],
        Scope::Band(_) => BAND_FIELDS.to_vec(),
        Scope::Context(..) => measure::KEYS.to_vec(),
        Scope::SubSession => vec![Query::SubRx],
        Scope::SubBand(_) => SUB_BAND_FIELDS.to_vec(),
        Scope::SubContext(..) => measure::SUB_KEYS.to_vec(),
    }
}

/// Why a change was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Purpose {
    /// Tuning, pan setup, antenna or measurement settings for a visit.
    Survey,
    /// Putting a context or band back before leaving it.
    PutBack,
    /// End-of-survey or recovery restoration.
    Restore,
}

/// One intended field change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub scope: Scope,
    pub field: String,
    pub from: Option<String>,
    pub to: String,
    pub wire: String,
}

/// Result class of a restoration (survey end or recovery attempt).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Every changed setting is back and verified.
    Complete,
    /// Some settings could not be restored or verified (CAT error,
    /// readback mismatch); a retry may finish the job.
    Partial,
    /// Unsupported: some originals cannot be sent by the collector.
    Unsupported,
    /// Someone changed settings the collector had changed; those were left
    /// alone.
    Conflict,
    /// The survey stopped without attempting restoration (operator activity
    /// or lost CAT).
    NotRestored,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Complete => "complete",
            Outcome::Partial => "partial",
            Outcome::Unsupported => "unsupported",
            Outcome::Conflict => "conflict",
            Outcome::NotRestored => "not_restored",
        }
    }
}

/// A journal entry. Entries are append-only; replay order is file order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    /// Written before any CAT change: the full original readback.
    Begin {
        session_id: String,
        profile: Option<String>,
        program_version: String,
        meta_mode: MetaMode,
        original: BTreeMap<String, String>,
    },
    /// A scope's state on arrival, before the collector changes it.
    Capture {
        scope: Scope,
        values: BTreeMap<String, String>,
    },
    /// Durable before the setters are sent.
    Intent {
        purpose: Purpose,
        changes: Vec<Change>,
    },
    /// Readback after a change was confirmed.
    Verified {
        state: BTreeMap<String, String>,
    },
    /// These fields are back to their captured originals (verified).
    Restored {
        scope: Scope,
        fields: Vec<String>,
    },
    Note {
        message: String,
    },
    /// End of a survey's or a recovery attempt's restoration.
    End {
        outcome: Outcome,
        message: String,
        manual: Vec<String>,
    },
    /// The operator accepted the radio's state; the journal is resolved.
    Acknowledged {
        note: String,
    },
}

/// A restoration obligation for one field in one scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Obligation {
    /// Raw readback found on arrival.
    pub original: String,
    /// Normalized values the collector intended since then.
    pub ours: Vec<String>,
    pub restored: bool,
}

impl Obligation {
    /// Changed by the collector and not yet put back.
    pub fn dirty(&self) -> bool {
        !self.ours.is_empty() && !self.restored
    }
}

/// How many intended values per field the ledger remembers. The radio can
/// only hold the last value set, or the one before it when a crash fell
/// between journaling an intent and sending it; anything older found in
/// the radio was set by someone else.
pub const OURS_KEPT: usize = 2;

/// Replayed journal state.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Ledger {
    pub session_id: String,
    pub profile: Option<String>,
    pub meta: Option<MetaMode>,
    pub original: BTreeMap<String, String>,
    /// JSON object keys must be strings, so this is written as a list.
    #[serde(serialize_with = "obligations_as_list")]
    pub obligations: BTreeMap<Scope, BTreeMap<String, Obligation>>,
    /// Last verified readback.
    pub last_state: BTreeMap<String, String>,
    /// Changes intended after the last verified readback (possibly sent).
    pub in_flight: Vec<Change>,
    pub outcome: Option<Outcome>,
    pub message: Option<String>,
    pub acknowledged: bool,
}

fn obligations_as_list<S: serde::Serializer>(
    m: &BTreeMap<Scope, BTreeMap<String, Obligation>>,
    s: S,
) -> Result<S::Ok, S::Error> {
    #[derive(Serialize)]
    struct Row<'a> {
        scope: &'a Scope,
        field: &'a str,
        #[serde(flatten)]
        obligation: &'a Obligation,
    }
    s.collect_seq(m.iter().flat_map(|(scope, fields)| {
        fields.iter().map(move |(field, obligation)| Row {
            scope,
            field,
            obligation,
        })
    }))
}

pub(crate) fn norm_field(field: &str, raw: &str) -> String {
    match Query::from_prefix(field) {
        Some(q) => measure::norm(q, raw),
        None => raw.to_owned(),
    }
}

impl Ledger {
    /// Applies one entry, enforcing the journaling invariants.
    pub fn apply(&mut self, e: &Entry) -> Result<(), String> {
        match e {
            Entry::Begin {
                session_id,
                profile,
                meta_mode,
                original,
                ..
            } => {
                if self.meta.is_some() {
                    return Err("second begin entry".into());
                }
                self.session_id = session_id.clone();
                self.profile = profile.clone();
                self.meta = Some(*meta_mode);
                self.original = original.clone();
                self.last_state = original.clone();
                let band = original.get("BN").ok_or("begin entry lacks the original band (BN)")?;
                self.capture(&Scope::Session, &BTreeMap::from([("BN".to_owned(), band.clone())]));
            }
            _ if self.meta.is_none() => return Err("entry before begin".into()),
            Entry::Capture { scope, values } => self.capture(scope, values),
            Entry::Intent { changes, .. } => {
                for c in changes {
                    let ob = self
                        .obligations
                        .get_mut(&c.scope)
                        .and_then(|m| m.get_mut(&c.field))
                        .ok_or_else(|| {
                            format!(
                                "change to {} on {} without a captured original",
                                c.field,
                                c.scope.describe()
                            )
                        })?;
                    let to = norm_field(&c.field, &c.to);
                    if to != norm_field(&c.field, &ob.original) {
                        if !ob.ours.contains(&to) {
                            ob.ours.push(to);
                            if ob.ours.len() > OURS_KEPT {
                                ob.ours.remove(0);
                            }
                        }
                        ob.restored = false;
                    }
                }
                self.in_flight.extend(changes.iter().cloned());
            }
            Entry::Verified { state } => {
                self.last_state = state.clone();
                self.in_flight.clear();
            }
            Entry::Restored { scope, fields } => {
                for f in fields {
                    if let Some(ob) = self.obligations.get_mut(scope).and_then(|m| m.get_mut(f)) {
                        ob.restored = true;
                    }
                }
            }
            Entry::Note { .. } => {}
            Entry::End { outcome, message, .. } => {
                self.outcome = Some(*outcome);
                self.message = Some(message.clone());
            }
            Entry::Acknowledged { .. } => self.acknowledged = true,
        }
        Ok(())
    }

    /// Records a scope's arrival state. A field the collector changed and
    /// has not put back keeps its first original.
    fn capture(&mut self, scope: &Scope, values: &BTreeMap<String, String>) {
        let m = self.obligations.entry(scope.clone()).or_default();
        for (f, v) in values {
            if m.get(f).is_some_and(Obligation::dirty) {
                continue;
            }
            m.insert(
                f.clone(),
                Obligation {
                    original: v.clone(),
                    ours: Vec::new(),
                    restored: false,
                },
            );
        }
    }

    pub fn has_capture(&self, scope: &Scope, field: &str) -> bool {
        self.obligations.get(scope).is_some_and(|m| m.contains_key(field))
    }

    pub fn obligation(&self, scope: &Scope, field: &str) -> Option<&Obligation> {
        self.obligations.get(scope)?.get(field)
    }

    /// Changed fields not yet put back, by scope.
    pub fn dirty(&self) -> Vec<(Scope, String, Obligation)> {
        let mut out = Vec::new();
        for (s, m) in &self.obligations {
            for (f, ob) in m {
                if ob.dirty() {
                    out.push((s.clone(), f.clone(), ob.clone()));
                }
            }
        }
        out
    }

    pub fn is_dirty(&self) -> bool {
        self.obligations.values().any(|m| m.values().any(Obligation::dirty))
    }

    /// Whether this journal still needs attention.
    pub fn resolved(&self) -> bool {
        self.acknowledged || (self.outcome == Some(Outcome::Complete) && !self.is_dirty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn begin() -> Entry {
        Entry::Begin {
            session_id: "s".into(),
            profile: None,
            program_version: "t".into(),
            meta_mode: MetaMode::Basic,
            original: BTreeMap::from([("BN".into(), "03".into()), ("FA".into(), "00007032000".into())]),
        }
    }

    fn change(scope: Scope, field: &str, to: &str) -> Change {
        Change {
            scope,
            field: field.into(),
            from: None,
            to: to.into(),
            wire: String::new(),
        }
    }

    #[test]
    fn intent_requires_a_capture_and_marks_dirty() {
        let mut l = Ledger::default();
        assert!(l.apply(&Entry::Note { message: "x".into() }).is_err());
        l.apply(&begin()).unwrap();
        let fa = change(Scope::Band(3), "FA", "00007040000");
        let e = Entry::Intent {
            purpose: Purpose::Survey,
            changes: vec![fa.clone()],
        };
        assert!(l.apply(&e).unwrap_err().contains("without a captured original"));
        l.apply(&Entry::Capture {
            scope: Scope::Band(3),
            values: BTreeMap::from([("FA".into(), "00007032000".into())]),
        })
        .unwrap();
        l.apply(&e).unwrap();
        assert!(l.is_dirty());
        assert_eq!(l.in_flight.len(), 1);
        assert_eq!(l.obligation(&Scope::Band(3), "FA").unwrap().ours, ["7040000"]);
        // A re-capture while dirty keeps the first original.
        l.apply(&Entry::Capture {
            scope: Scope::Band(3),
            values: BTreeMap::from([("FA".into(), "00007040000".into())]),
        })
        .unwrap();
        assert_eq!(l.obligation(&Scope::Band(3), "FA").unwrap().original, "00007032000");
        l.apply(&Entry::Verified { state: BTreeMap::new() }).unwrap();
        assert!(l.in_flight.is_empty());
        l.apply(&Entry::Restored {
            scope: Scope::Band(3),
            fields: vec!["FA".into()],
        })
        .unwrap();
        assert!(!l.is_dirty());
        assert!(!l.resolved(), "no end entry yet");
        l.apply(&Entry::End {
            outcome: Outcome::Complete,
            message: String::new(),
            manual: vec![],
        })
        .unwrap();
        assert!(l.resolved());
    }

    #[test]
    fn putting_back_to_the_original_is_not_ours() {
        let mut l = Ledger::default();
        l.apply(&begin()).unwrap();
        l.apply(&Entry::Intent {
            purpose: Purpose::Restore,
            changes: vec![change(Scope::Session, "BN", "3")],
        })
        .unwrap();
        assert!(!l.is_dirty());
        l.apply(&Entry::Intent {
            purpose: Purpose::Survey,
            changes: vec![change(Scope::Session, "BN", "05")],
        })
        .unwrap();
        assert!(l.is_dirty());
        l.apply(&Entry::Acknowledged { note: String::new() }).unwrap();
        assert!(l.resolved());
    }
}
