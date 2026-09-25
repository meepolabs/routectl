//! One ledger row's `extra` read into a report category, and a known row's
//! facts: closed-set labels, counts, flags, its field-level data defects and
//! the single reason an anchored row does not pass.
//!
//! Every label kept is one of the producer's own closed-set constants (a
//! `&'static str`); a value outside the set becomes a fixed bucket and never
//! reaches the report as the raw database string.

use serde_json::{Map, Value};

use crate::handlers::opening_diagnostics::{
    OPENING_REASON_LABELS, OPENING_SOURCE_LABELS, TERMINAL_SOURCE_LABELS,
    is_terminal_evidence_label, key, terminal,
};
use crate::ingress::anthropic::context_anchor::OpeningSource;

/// A row passes when its error is at most this percent of the terminal.
pub(super) const PASS_PCT: u128 = 5;

/// An `extra` longer than this is not parsed. The producer writes a few
/// hundred bytes; anything this large is not its output.
pub(super) const MAX_EXTRA_BYTES: usize = 16 * 1024;

/// The report's bucket for a label outside the producer's closed set.
pub(super) const UNRECOGNIZED_LABEL: &str = "<unrecognized>";

/// Why a row cannot be placed in any known category.
pub(super) mod unclassifiable {
    pub const OVERSIZED: &str = "oversized";
    pub const INVALID_JSON: &str = "invalid_json";
    pub const NOT_AN_OBJECT: &str = "not_an_object";
    pub const OPENING_PRESENT_MISSING: &str = "opening_present_missing";
    pub const OPENING_PRESENT_NOT_BOOL: &str = "opening_present_not_bool";
    pub const NO_OPENING_CONTRADICTED: &str = "no_opening_contradicted";
    pub const SOURCE_MISSING: &str = "opening_source_missing";
    pub const SOURCE_UNRECOGNIZED: &str = "opening_source_unrecognized";
}

/// A field-level defect on a row whose category is known.
pub(super) mod defect {
    pub const MARKER_INCONSISTENT: &str = "opening_marker_inconsistent";
    pub const REQUIRED_KEY_MISSING: &str = "required_key_missing";
    pub const REASON_UNRECOGNIZED: &str = "reason_unrecognized";
    pub const TERMINAL_SOURCE_UNRECOGNIZED: &str = "terminal_source_unrecognized";
    pub const COUNT_MALFORMED: &str = "count_malformed";
    pub const FLAG_MALFORMED: &str = "flag_malformed";
}

/// Why a known anchored row does not pass.
pub(super) mod non_pass {
    pub const MARKER_INCONSISTENT: &str = "opening_marker_inconsistent";
    pub const OUTCOME_NOT_OK: &str = "outcome_not_ok";
    pub const OPENING_UNSTATED: &str = "opening_unstated";
    pub const OPENING_MALFORMED: &str = "opening_malformed";
    pub const TERMINAL_MISSING: &str = "terminal_missing";
    pub const TERMINAL_MALFORMED: &str = "terminal_malformed";
    pub const TERMINAL_UNSUPPORTED: &str = "terminal_unsupported";
    pub const TERMINAL_ZERO: &str = "terminal_zero";
    pub const OUTSIDE_5PCT: &str = "outside_5pct";
}

pub(super) enum Class {
    Legacy,
    Unclassifiable(&'static str),
    NoOpening,
    Known(KnownRow),
}

/// The category of a row whose `extra` column holds `extra`.
pub(super) fn classify(extra: Option<&str>) -> Class {
    let Some(text) = extra else {
        return Class::Legacy;
    };
    if text.len() > MAX_EXTRA_BYTES {
        return Class::Unclassifiable(unclassifiable::OVERSIZED);
    }
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return Class::Unclassifiable(unclassifiable::INVALID_JSON);
    };
    let Value::Object(map) = value else {
        return Class::Unclassifiable(unclassifiable::NOT_AN_OBJECT);
    };
    classify_object(&map)
}

fn classify_object(map: &Map<String, Value>) -> Class {
    let present = map.get(key::OPENING_PRESENT);
    let source = map.get(key::OPENING_SOURCE);
    // An exact anchor source is a known anchor-used turn whatever its marker
    // says; the inconsistency is its non-pass, not its exclusion.
    if source.and_then(Value::as_str) == Some(OpeningSource::Anchor.as_str()) {
        let marker_ok = matches!(present, Some(Value::Bool(true)));
        return Class::Known(KnownRow::read(
            map,
            OpeningSource::Anchor.as_str(),
            marker_ok,
        ));
    }
    if !key::ALL.iter().any(|k| map.contains_key(*k)) {
        return Class::Legacy;
    }
    match present {
        None => Class::Unclassifiable(unclassifiable::OPENING_PRESENT_MISSING),
        Some(Value::Bool(false)) => {
            let only_marker = key::ALL
                .iter()
                .all(|k| *k == key::OPENING_PRESENT || !map.contains_key(*k));
            if only_marker {
                Class::NoOpening
            } else {
                Class::Unclassifiable(unclassifiable::NO_OPENING_CONTRADICTED)
            }
        }
        Some(Value::Bool(true)) => match source {
            None => Class::Unclassifiable(unclassifiable::SOURCE_MISSING),
            Some(value) => match closed(OPENING_SOURCE_LABELS, value) {
                Some(label) => Class::Known(KnownRow::read(map, label, true)),
                None => Class::Unclassifiable(unclassifiable::SOURCE_UNRECOGNIZED),
            },
        },
        Some(_) => Class::Unclassifiable(unclassifiable::OPENING_PRESENT_NOT_BOOL),
    }
}

/// `value` as the matching member of `set`, if it is one.
fn closed(set: &[&'static str], value: &Value) -> Option<&'static str> {
    let text = value.as_str()?;
    set.iter().copied().find(|label| *label == text)
}

/// A count field: absent, a non-negative integer, or anything else.
#[derive(Clone, Copy)]
enum Count {
    Absent,
    Value(u64),
    Malformed,
}

impl Count {
    fn read(map: &Map<String, Value>, field: &str) -> Self {
        map.get(field).map_or(Self::Absent, |v| {
            v.as_u64().map_or(Self::Malformed, Self::Value)
        })
    }
}

/// `|opening - terminal|` against a positive `terminal`, compared in
/// integers so no rounding can move a row across a bucket edge.
#[derive(Clone, Copy)]
pub(super) struct ErrorPct {
    diff: u128,
    terminal: u128,
}

impl ErrorPct {
    pub(super) const fn within(self, pct: u128) -> bool {
        self.diff * 100 <= pct * self.terminal
    }
}

/// What a known row states, read once.
pub(super) struct KnownRow {
    pub(super) source: &'static str,
    pub(super) reason: &'static str,
    /// `None` when the label is absent or outside the closed set.
    terminal_label: Option<&'static str>,
    pub(super) vendor_verified: bool,
    pub(super) provisional: bool,
    pub(super) lane_switched: bool,
    marker_ok: bool,
    opening: Count,
    terminal: Count,
    /// Field-level data defects, each counted once per row.
    pub(super) defects: Vec<&'static str>,
}

impl KnownRow {
    fn read(map: &Map<String, Value>, source: &'static str, marker_ok: bool) -> Self {
        let mut defects = Vec::new();
        if !marker_ok {
            defects.push(defect::MARKER_INCONSISTENT);
        }
        let required = [
            key::OPENING_REASON,
            key::OPENING_PROVISIONAL,
            key::TERMINAL_SOURCE,
            key::TERMINAL_VENDOR_VERIFIED,
        ];
        if marker_ok && required.iter().any(|k| !map.contains_key(*k)) {
            defects.push(defect::REQUIRED_KEY_MISSING);
        }
        let reason = map
            .get(key::OPENING_REASON)
            .and_then(|v| closed(OPENING_REASON_LABELS, v));
        if map.contains_key(key::OPENING_REASON) && reason.is_none() {
            defects.push(defect::REASON_UNRECOGNIZED);
        }
        let terminal_label = map
            .get(key::TERMINAL_SOURCE)
            .and_then(|v| closed(TERMINAL_SOURCE_LABELS, v));
        if map.contains_key(key::TERMINAL_SOURCE) && terminal_label.is_none() {
            defects.push(defect::TERMINAL_SOURCE_UNRECOGNIZED);
        }
        let opening = Count::read(map, key::OPENING_INPUT);
        let terminal = Count::read(map, key::TERMINAL_INPUT);
        if matches!(opening, Count::Malformed) || matches!(terminal, Count::Malformed) {
            defects.push(defect::COUNT_MALFORMED);
        }
        let flags = [
            key::OPENING_PROVISIONAL,
            key::OPENING_LANE_SWITCHED,
            key::TERMINAL_VENDOR_VERIFIED,
        ];
        if flags
            .iter()
            .any(|k| map.get(*k).is_some_and(|v| !v.is_boolean()))
        {
            defects.push(defect::FLAG_MALFORMED);
        }
        let flag = |k: &str| matches!(map.get(k), Some(Value::Bool(true)));
        Self {
            source,
            reason: reason.unwrap_or(UNRECOGNIZED_LABEL),
            terminal_label,
            vendor_verified: flag(key::TERMINAL_VENDOR_VERIFIED),
            provisional: flag(key::OPENING_PROVISIONAL),
            lane_switched: flag(key::OPENING_LANE_SWITCHED),
            marker_ok,
            opening,
            terminal,
            defects,
        }
    }

    /// The terminal label for display: a closed-set member or the fixed
    /// unrecognized bucket.
    pub(super) fn terminal_display(&self) -> &'static str {
        self.terminal_label.unwrap_or(UNRECOGNIZED_LABEL)
    }

    /// Whether the row carries a terminal input report that is not
    /// established as the vendor's own, whatever the turn's outcome.
    pub(super) const fn is_unverified_report(&self) -> bool {
        !matches!(self.terminal, Count::Absent) && !self.vendor_verified
    }

    fn is_evidence(&self) -> bool {
        self.terminal_label.is_some_and(is_terminal_evidence_label)
    }

    /// The error against an evidence terminal, when both counts are stated
    /// and the terminal is positive.
    pub(super) fn error(&self) -> Option<ErrorPct> {
        let (Count::Value(opening), Count::Value(terminal)) = (self.opening, self.terminal) else {
            return None;
        };
        (self.is_evidence() && terminal > 0).then(|| ErrorPct {
            diff: u128::from(opening.abs_diff(terminal)),
            terminal: u128::from(terminal),
        })
    }

    /// Why an anchored row does not pass, the first reason in this order;
    /// `None` is a pass.
    pub(super) fn non_pass(&self, outcome: &str) -> Option<&'static str> {
        if !self.marker_ok {
            return Some(non_pass::MARKER_INCONSISTENT);
        }
        if outcome != routectl_usage::Outcome::Ok.as_str() {
            return Some(non_pass::OUTCOME_NOT_OK);
        }
        match self.opening {
            Count::Absent => return Some(non_pass::OPENING_UNSTATED),
            Count::Malformed => return Some(non_pass::OPENING_MALFORMED),
            Count::Value(_) => {}
        }
        match self.terminal_label {
            None => return Some(non_pass::TERMINAL_MALFORMED),
            Some(label) if label == terminal::MISSING => return Some(non_pass::TERMINAL_MISSING),
            Some(_) if !self.is_evidence() => return Some(non_pass::TERMINAL_UNSUPPORTED),
            Some(_) => {}
        }
        match self.terminal {
            Count::Absent | Count::Malformed => Some(non_pass::TERMINAL_MALFORMED),
            Count::Value(0) => Some(non_pass::TERMINAL_ZERO),
            Count::Value(_) => match self.error() {
                Some(e) if e.within(PASS_PCT) => None,
                _ => Some(non_pass::OUTSIDE_5PCT),
            },
        }
    }
}
