//! Restoration from a [`Ledger`]: shared by the end of a survey and by
//! `k4na recovery restore`.
//!
//! Per field, against the live readback:
//! * equal to the captured original: already restored;
//! * equal to a value the collector set (or was about to set): restored
//!   with the whitelisted setter for the original, then verified;
//! * anything else: someone changed it; left alone (conflict);
//! * original not sendable by the collector: unsupported, left as is.
//!
//! The radio is only moved to another band to restore it when the current
//! band is still where the collector left it; otherwise nothing is sent.

use serde::Serialize;

use super::{Change, Entry, Ledger, Outcome, Purpose, Scope, Transaction, fields, norm_field};
use crate::cat::{CatControl, CatQuery, Query, Readback, Setter};
use crate::measure::{self, MetaMode, expected_readback, label, setter_for};

/// Everything restoration reads: transmit state, meta-mode, band memory
/// fields and receiver settings.
pub fn state_queries() -> Vec<Query> {
    let mut q = vec![
        Query::Transmit,
        Query::Identify,
        Query::Mode,
        Query::TxAntenna,
        Query::Band,
    ];
    q.extend(super::BAND_FIELDS);
    q.extend(measure::KEYS);
    q
}

/// [`state_queries`] plus the sub receiver's, for journals that changed
/// it. (Single-receiver restores do not ask for sub state, so a radio
/// that does not answer it is not slowed down.)
pub fn sub_state_queries() -> Vec<Query> {
    let mut q = state_queries();
    q.extend([Query::SubRx, Query::BandIndependent, Query::SubBand]);
    q.extend(super::SUB_BAND_FIELDS);
    q.extend(measure::SUB_KEYS);
    q
}

fn queries_for(ledger: &Ledger) -> Vec<Query> {
    if ledger.obligations.keys().any(Scope::is_sub) {
        sub_state_queries()
    } else {
        state_queries()
    }
}

/// What to do with one field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Clean { field: String },
    Restore { field: String, setter: Setter },
    Conflict { field: String, current: String },
    Unsupported { field: String },
    Unreadable { field: String },
}

impl Decision {
    pub fn field(&self) -> &str {
        match self {
            Decision::Clean { field }
            | Decision::Restore { field, .. }
            | Decision::Conflict { field, .. }
            | Decision::Unsupported { field }
            | Decision::Unreadable { field } => field,
        }
    }
}

/// Decisions for `scope`'s changed fields (all captured fields with
/// `recheck`, for the final check of the original band).
pub fn decide(ledger: &Ledger, scope: &Scope, rb: &Readback, recheck: bool) -> Vec<Decision> {
    let Some(obligations) = ledger.obligations.get(scope) else {
        return Vec::new();
    };
    let meta = ledger.meta.unwrap_or(MetaMode::Basic);
    let in_flight: Vec<String> = ledger
        .in_flight
        .iter()
        .filter(|c| &c.scope == scope)
        .map(|c| norm_field(&c.field, &c.to))
        .collect();
    let mut out = Vec::new();
    for (field, ob) in obligations {
        if !(ob.dirty() || (recheck && !ob.ours.is_empty())) {
            continue;
        }
        let Some(q) = Query::from_prefix(field) else { continue };
        let field = field.clone();
        let Some(current) = rb.raw(q) else {
            out.push(Decision::Unreadable { field });
            continue;
        };
        let cur = measure::norm(q, current);
        if cur == measure::norm(q, &ob.original) {
            out.push(Decision::Clean { field });
        } else if ob.ours.contains(&cur) || in_flight.contains(&cur) {
            match setter_for(q, meta, &ob.original) {
                Some(setter) => out.push(Decision::Restore { field, setter }),
                None => out.push(Decision::Unsupported { field }),
            }
        } else {
            out.push(Decision::Conflict {
                field,
                current: current.to_owned(),
            });
        }
    }
    out
}

/// Per-field result.
#[derive(Debug, Clone, Serialize)]
pub struct FieldReport {
    pub scope: Scope,
    pub field: String,
    pub label: &'static str,
    pub original: String,
    pub current: Option<String>,
    /// `already_original`, `restored`, `would_restore`, `conflict`,
    /// `unsupported`, `unreadable` or `failed`.
    pub result: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestoreReport {
    pub outcome: Outcome,
    pub message: String,
    pub dry_run: bool,
    pub sent: Vec<String>,
    pub fields: Vec<FieldReport>,
    /// Steps the operator must take by hand (empty when complete).
    pub manual: Vec<String>,
}

fn field_label(field: &str) -> &'static str {
    Query::from_prefix(field).map_or("setting", label)
}

/// A journaled (normalized) value as the radio would read it back, for
/// people: `BN03`, not `BN3`. Falls back to the field and raw value.
pub fn wire_value(field: &str, meta: MetaMode, value: &str) -> String {
    Query::from_prefix(field)
        .and_then(|q| setter_for(q, meta, value))
        .map_or_else(
            || format!("{field}{value}"),
            |s| s.wire().trim_end_matches(';').to_owned(),
        )
}

/// Manual instructions for every changed field not known to be back.
pub fn manual_steps(ledger: &Ledger) -> Vec<String> {
    let mut out = Vec::new();
    if !ledger.is_dirty() {
        return out;
    }
    let meta = ledger.meta.unwrap_or(MetaMode::Basic);
    // The sub receiver's on/off comes after its band settings.
    let mut dirty = ledger.dirty();
    dirty.sort_by_key(|(s, _, _)| *s == Scope::SubSession);
    for (scope, field, ob) in dirty {
        if scope == Scope::Session {
            continue;
        }
        out.push(format!(
            "On {}: set the {} back to {field}{} (the survey may have left {})",
            scope.describe(),
            field_label(&field),
            ob.original,
            ob.ours
                .iter()
                .map(|v| wire_value(&field, meta, v))
                .collect::<Vec<_>>()
                .join(" or ")
        ));
    }
    if let Some(bn) = ledger.original.get("BN") {
        let band = bn.parse::<u8>().ok();
        let fa = ledger
            .original
            .get("FA")
            .map_or(String::new(), |f| format!(", VFO A FA{f}"));
        out.push(format!(
            "Finally return to {} (BN{bn}){fa}, the state before the survey",
            k4na_core::measure::band_label(band)
        ));
    }
    out
}

struct Run<'a, C> {
    txn: &'a mut Transaction,
    cat: &'a mut C,
    rb: Readback,
    sent: Vec<String>,
    fields: Vec<FieldReport>,
    queries: Vec<Query>,
}

type Fatal = (Outcome, String);

/// One receiver's scopes on a band, for settling inputs generically.
#[derive(Clone, Copy)]
enum Rx {
    Main,
    Sub,
}

impl Rx {
    fn input(self) -> Query {
        match self {
            Rx::Main => Query::RxAntenna,
            Rx::Sub => Query::SubRxAntenna,
        }
    }

    fn band_scope(self, band: u8) -> Scope {
        match self {
            Rx::Main => Scope::Band(band),
            Rx::Sub => Scope::SubBand(band),
        }
    }

    fn context(self, band: u8, input: String) -> Scope {
        match self {
            Rx::Main => Scope::Context(band, input),
            Rx::Sub => Scope::SubContext(band, input),
        }
    }

    fn context_input(self, s: &Scope, band: u8) -> Option<String> {
        match (self, s) {
            (Rx::Main, Scope::Context(b, ar)) | (Rx::Sub, Scope::SubContext(b, ar)) if *b == band => Some(ar.clone()),
            _ => None,
        }
    }
}

impl<C: CatControl> Run<'_, C> {
    async fn read(&mut self) -> Result<Readback, Fatal> {
        let rb = self.cat.query(&self.queries).await.map_err(|e| {
            (
                Outcome::Partial,
                format!("CAT readback failed ({e}); radio state is unknown"),
            )
        })?;
        match rb.flag(Query::Transmit) {
            Some(false) => Ok(rb),
            Some(true) => Err((
                Outcome::Conflict,
                "the radio is transmitting; nothing more was sent".into(),
            )),
            None => Err((
                Outcome::Conflict,
                "transmit state (TQ) is unreadable; nothing more was sent".into(),
            )),
        }
    }

    /// Journals, sends and verifies; returns which changes read back.
    async fn send(&mut self, purpose: Purpose, changes: Vec<(Scope, Setter)>) -> Result<Vec<bool>, Fatal> {
        let entries: Vec<Change> = changes
            .iter()
            .map(|(scope, s)| Change {
                scope: scope.clone(),
                field: s.query().prefix().to_owned(),
                from: self.rb.raw(s.query()).map(str::to_owned),
                to: expected_readback(*s),
                wire: s.wire(),
            })
            .collect();
        self.txn
            .record(Entry::Intent {
                purpose,
                changes: entries,
            })
            .map_err(|e| (Outcome::Partial, format!("{e}; nothing more was sent")))?;
        for (_, s) in &changes {
            self.sent.push(s.wire());
            self.cat.set(*s).await.map_err(|e| {
                (
                    Outcome::Partial,
                    format!("CAT failed sending {} ({e}); radio state is unknown", s.wire()),
                )
            })?;
        }
        self.rb = self.read().await?;
        let ok: Vec<bool> = changes
            .iter()
            .map(|(_, s)| {
                let q = s.query();
                self.rb.raw(q).map(|r| measure::norm(q, r)) == Some(measure::norm(q, &expected_readback(*s)))
            })
            .collect();
        let entry = if ok.iter().all(|b| *b) {
            Entry::Verified {
                state: self.rb.to_map(),
            }
        } else {
            Entry::Note {
                message: format!("readback after restore disagrees: {:?}", self.rb.to_map()),
            }
        };
        self.txn
            .record(entry)
            .map_err(|e| (Outcome::Partial, format!("{e}; nothing more was sent")))?;
        Ok(ok)
    }

    fn report(&mut self, scope: &Scope, field: &str, result: &'static str) {
        let original = self
            .txn
            .ledger()
            .obligation(scope, field)
            .map(|o| o.original.clone())
            .unwrap_or_default();
        let current = Query::from_prefix(field).and_then(|q| self.rb.raw(q).map(str::to_owned));
        self.fields.push(FieldReport {
            scope: scope.clone(),
            field: field.to_owned(),
            label: field_label(field),
            original,
            current,
            result,
        });
    }

    /// Settles one scope; journals the fields that are back.
    async fn settle(&mut self, scope: &Scope, recheck: bool) -> Result<(), Fatal> {
        let decisions = decide(self.txn.ledger(), scope, &self.rb, recheck);
        let mut back = Vec::new();
        let mut restores = Vec::new();
        for d in &decisions {
            match d {
                Decision::Clean { field } => {
                    back.push(field.clone());
                    self.report(scope, field, "already_original");
                }
                Decision::Restore { setter, .. } => restores.push((scope.clone(), *setter)),
                Decision::Conflict { field, .. } => self.report(scope, field, "conflict"),
                Decision::Unsupported { field } => self.report(scope, field, "unsupported"),
                Decision::Unreadable { field } => self.report(scope, field, "unreadable"),
            }
        }
        if !restores.is_empty() {
            let ok = self.send(Purpose::Restore, restores.clone()).await?;
            for ((_, s), ok) in restores.iter().zip(ok) {
                let f = s.query().prefix();
                if ok {
                    back.push(f.to_owned());
                }
                self.report(scope, f, if ok { "restored" } else { "failed" });
            }
        }
        if !back.is_empty() {
            self.txn
                .record(Entry::Restored {
                    scope: scope.clone(),
                    fields: back,
                })
                .map_err(|e| (Outcome::Partial, e.to_string()))?;
        }
        Ok(())
    }

    fn band_now(&self) -> Option<u8> {
        self.rb.u64(Query::Band).and_then(|b| u8::try_from(b).ok())
    }

    async fn select_band(&mut self, band: u8) -> Result<(), Fatal> {
        if self.band_now() == Some(band) {
            return Ok(());
        }
        let ok = self
            .send(Purpose::Restore, vec![(Scope::Session, Setter::Band(band))])
            .await?;
        if ok != [true] {
            return Err((Outcome::Partial, format!("BN{band:02} was not confirmed by readback")));
        }
        Ok(())
    }

    /// Restores one band: each receiver's RX-input contexts (current input
    /// first), then its own memory, including the input it was found on.
    /// The main receiver goes first; the sub follows the main band (`BI0`).
    async fn settle_band(&mut self, band: u8) -> Result<(), Fatal> {
        self.select_band(band).await?;
        self.settle_inputs(band, Rx::Main).await?;
        self.settle(&Scope::Band(band), false).await?;
        self.settle_inputs(band, Rx::Sub).await?;
        self.settle(&Scope::SubBand(band), false).await
    }

    async fn settle_inputs(&mut self, band: u8, rx: Rx) -> Result<(), Fatal> {
        let meta = self.txn.ledger().meta.unwrap_or(MetaMode::Basic);
        let mut contexts: Vec<String> = self
            .txn
            .ledger()
            .dirty()
            .into_iter()
            .filter_map(|(s, _, _)| rx.context_input(&s, band))
            .collect();
        contexts.dedup();
        let current = self.rb.raw(rx.input()).map(str::to_owned);
        contexts.sort_by_key(|ar| Some(ar) != current.as_ref());
        for ar in contexts {
            let scope = rx.context(band, ar.clone());
            if self.rb.raw(rx.input()) != Some(ar.as_str()) {
                // Switching inputs is only safe if the input is ours.
                let band_scope = rx.band_scope(band);
                let input_field = rx.input().prefix();
                let input_ok = decide(self.txn.ledger(), &band_scope, &self.rb, true).iter().any(|d| {
                    d.field() == input_field && matches!(d, Decision::Restore { .. } | Decision::Clean { .. })
                });
                match setter_for(rx.input(), meta, &ar) {
                    Some(s) if input_ok => {
                        let ok = self.send(Purpose::Restore, vec![(band_scope, s)]).await?;
                        if ok != [true] {
                            for f in fields(&scope) {
                                self.report(&scope, f.prefix(), "failed");
                            }
                            continue;
                        }
                    }
                    _ => {
                        for (s, f, _) in self.txn.ledger().dirty() {
                            if s == scope {
                                self.report(&scope, &f, if input_ok { "unsupported" } else { "conflict" });
                            }
                        }
                        continue;
                    }
                }
            }
            self.settle(&scope, false).await?;
        }
        Ok(())
    }

    async fn run(&mut self) -> Result<(), Fatal> {
        self.rb = self.read().await?;
        let ledger = self.txn.ledger().clone();
        let meta = ledger.meta.unwrap_or(MetaMode::Basic);
        if MetaMode::from_readback(&self.rb) != Some(meta) {
            return Err((
                Outcome::Unsupported,
                format!(
                    "the CAT meta-mode is now {:?} but the journal was recorded in {meta} mode; values cannot be \
                     compared, so nothing was sent",
                    self.rb.raw(Query::Identify)
                ),
            ));
        }
        let original_band = ledger
            .original
            .get("BN")
            .and_then(|b| b.parse::<u8>().ok())
            .ok_or((Outcome::Unsupported, "the journal lacks the original band".to_owned()))?;
        let mut expected: Vec<String> = ledger.last_state.get("BN").into_iter().cloned().collect();
        expected.extend(
            ledger
                .in_flight
                .iter()
                .filter(|c| c.field == "BN")
                .map(|c| c.to.clone()),
        );
        expected.push(original_band.to_string());
        let expected: Vec<String> = expected.iter().map(|b| norm_field("BN", b)).collect();
        let now = self.rb.raw(Query::Band).map(|b| norm_field("BN", b));
        if !now.as_ref().is_some_and(|b| expected.contains(b)) {
            return Err((
                Outcome::Conflict,
                format!(
                    "the radio is on BN{} but the collector left it on BN{}; someone changed bands since, so \
                     nothing was sent",
                    now.as_deref().unwrap_or("?"),
                    ledger.last_state.get("BN").map_or("?", String::as_str)
                ),
            ));
        }
        let mut bands: Vec<u8> = ledger.dirty().iter().filter_map(|(s, _, _)| s.band()).collect();
        bands.sort_unstable();
        bands.dedup();
        let current = self.band_now();
        bands.sort_by_key(|b| Some(*b) != current);
        for band in bands {
            self.settle_band(band).await?;
        }
        self.select_band(original_band).await?;
        // The original band's recalled memory must be its original too.
        self.settle(&Scope::Band(original_band), true).await?;
        if let Some(ar) = self.rb.raw(Query::RxAntenna).map(str::to_owned) {
            self.settle(&Scope::Context(original_band, ar), true).await?;
        }
        self.settle(&Scope::SubBand(original_band), true).await?;
        if let Some(ar) = self.rb.raw(Query::SubRxAntenna).map(str::to_owned) {
            self.settle(&Scope::SubContext(original_band, ar), true).await?;
        }
        self.settle(&Scope::Session, true).await?;
        self.settle(&Scope::SubSession, true).await
    }
}

fn classify(fields: &[FieldReport], ledger: &Ledger) -> Outcome {
    let has = |r: &str| fields.iter().any(|f| f.result == r);
    if has("conflict") {
        Outcome::Conflict
    } else if has("unsupported") {
        Outcome::Unsupported
    } else if has("failed") || has("unreadable") || ledger.is_dirty() {
        Outcome::Partial
    } else {
        Outcome::Complete
    }
}

/// Restores what the ledger says the collector changed, journaling every
/// step, and ends the journal with the outcome.
pub async fn restore<C: CatControl>(txn: &mut Transaction, cat: &mut C) -> RestoreReport {
    let queries = queries_for(txn.ledger());
    let mut run = Run {
        txn,
        cat,
        rb: Readback::default(),
        sent: Vec::new(),
        fields: Vec::new(),
        queries,
    };
    let fatal = run.run().await.err();
    let ledger = run.txn.ledger().clone();
    let (outcome, message) = match fatal {
        Some((o, m)) => (o, m),
        None => {
            let o = classify(&run.fields, &ledger);
            let restored = run.fields.iter().filter(|f| f.result == "restored").count();
            let m = match o {
                Outcome::Complete if run.sent.is_empty() => "radio already in its original state".to_owned(),
                Outcome::Complete => format!("restored {restored} setting(s): {}", run.sent.concat()),
                _ => format!(
                    "restored {restored} setting(s); {} need attention",
                    run.fields
                        .iter()
                        .filter(|f| !matches!(f.result, "restored" | "already_original"))
                        .count()
                ),
            };
            (o, m)
        }
    };
    let manual = if outcome == Outcome::Complete {
        Vec::new()
    } else {
        manual_steps(&ledger)
    };
    let _ = run.txn.record(Entry::End {
        outcome,
        message: message.clone(),
        manual: manual.clone(),
    });
    RestoreReport {
        outcome,
        message,
        dry_run: false,
        sent: run.sent,
        fields: run.fields,
        manual,
    }
}

/// What [`restore`] would do on the current band, from GET queries only.
pub async fn plan<C: CatQuery>(ledger: &Ledger, cat: &mut C) -> Result<RestoreReport, String> {
    let rb = cat
        .query(&queries_for(ledger))
        .await
        .map_err(|e| format!("CAT readback failed: {e}"))?;
    let mut fields = Vec::new();
    let band = rb.u64(Query::Band).and_then(|b| u8::try_from(b).ok());
    let mut scopes: Vec<Scope> = ledger.dirty().into_iter().map(|(s, _, _)| s).collect();
    scopes.dedup();
    // In restore order: the band's own settings first, the band change and
    // the sub receiver's on/off last.
    scopes.sort_by_key(|s| (s.is_session(), *s == Scope::SubSession));
    let mut sent = Vec::new();
    for scope in scopes {
        let here = match &scope {
            Scope::Session | Scope::SubSession => true,
            Scope::Band(b) | Scope::SubBand(b) => Some(*b) == band,
            Scope::Context(b, ar) => Some(*b) == band && rb.raw(Query::RxAntenna) == Some(ar.as_str()),
            Scope::SubContext(b, ar) => Some(*b) == band && rb.raw(Query::SubRxAntenna) == Some(ar.as_str()),
        };
        for d in decide(ledger, &scope, &rb, false) {
            let field = d.field().to_owned();
            let result = match (&d, here) {
                (_, false) => "unvisited",
                (Decision::Clean { .. }, _) => "already_original",
                (Decision::Restore { setter, .. }, _) => {
                    sent.push(setter.wire());
                    "would_restore"
                }
                (Decision::Conflict { .. }, _) => "conflict",
                (Decision::Unsupported { .. }, _) => "unsupported",
                (Decision::Unreadable { .. }, _) => "unreadable",
            };
            fields.push(FieldReport {
                original: ledger
                    .obligation(&scope, &field)
                    .map(|o| o.original.clone())
                    .unwrap_or_default(),
                current: Query::from_prefix(&field).and_then(|q| rb.raw(q).map(str::to_owned)),
                label: field_label(&field),
                scope: scope.clone(),
                field,
                result,
            });
        }
    }
    let msg = format!(
        "would send {} for the current band (other bands are settled on arrival); nothing was sent",
        if sent.is_empty() {
            "nothing".to_owned()
        } else {
            sent.concat()
        }
    );
    Ok(RestoreReport {
        outcome: classify(&fields, &Ledger::default()),
        message: msg,
        dry_run: true,
        sent: Vec::new(),
        fields,
        manual: manual_steps(ledger),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn ledger() -> Ledger {
        let mut l = Ledger::default();
        let m = |pairs: &[(&str, &str)]| -> BTreeMap<String, String> {
            pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
        };
        l.apply(&Entry::Begin {
            session_id: "s".into(),
            profile: None,
            program_version: "t".into(),
            meta_mode: MetaMode::Basic,
            original: m(&[("BN", "04")]),
        })
        .unwrap();
        let scope = Scope::Band(4);
        l.apply(&Entry::Capture {
            scope: scope.clone(),
            values: m(&[("FA", "00010120000"), ("#SPN", "5000"), ("#FXT", "1"), ("AR", "4")]),
        })
        .unwrap();
        let ch = |field: &str, to: &str| Change {
            scope: scope.clone(),
            field: field.into(),
            from: None,
            to: to.into(),
            wire: String::new(),
        };
        l.apply(&Entry::Intent {
            purpose: Purpose::Survey,
            changes: vec![
                ch("FA", "00010125000"),
                ch("#SPN", "40000"),
                ch("#FXT", "0"),
                ch("AR", "5"),
            ],
        })
        .unwrap();
        l
    }

    #[test]
    fn journaled_values_are_shown_as_the_radio_reads_them() {
        let b = MetaMode::Basic;
        assert_eq!(wire_value("BN", b, "3"), "BN03");
        assert_eq!(wire_value("FA", b, "7030000"), "FA00007030000");
        assert_eq!(wire_value("#AVG", b, "4"), "#AVG04");
        assert_eq!(wire_value("PA", b, "0"), "PA0");
        assert_eq!(wire_value("XX", b, "7"), "XX7");
    }

    #[test]
    fn decisions_follow_the_conflict_rule() {
        let l = ledger();
        let mut rb = Readback::default();
        rb.insert(Query::VfoA, "00010125000"); // ours -> restore
        rb.insert(Query::Span, "40000"); // ours, but 5000 is not sendable
        rb.insert(Query::FixedTune, "1"); // already original
        rb.insert(Query::RxAntenna, "1"); // someone else's
        let d = decide(&l, &Scope::Band(4), &rb, false);
        assert_eq!(
            d,
            [
                Decision::Clean { field: "#FXT".into() },
                Decision::Unsupported { field: "#SPN".into() },
                Decision::Conflict {
                    field: "AR".into(),
                    current: "1".into()
                },
                Decision::Restore {
                    field: "FA".into(),
                    setter: Setter::VfoA(10_120_000)
                },
            ]
        );
        let steps = manual_steps(&l).join("\n");
        assert!(
            steps.contains("On 30m (BN04): set the pan span back to #SPN5000"),
            "{steps}"
        );
        assert!(steps.contains("Finally return to 30m (BN04)"), "{steps}");
    }
}
