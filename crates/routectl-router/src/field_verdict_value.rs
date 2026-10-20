//! The typed value a field verdict carries, and its persisted token codec.
//!
//! A rewrite verdict is not just "this field is refused": it names WHAT the
//! field is rewritten to. That value rides the ledger in the existing
//! `upstream_token` column as `shape:<token>` or `ceiling:<u32>`, so no schema
//! or vocabulary step is needed to persist it.
//!
//! The column is untrusted on the way back in -- any build, any bug, or a
//! hand-edited ledger may have written it -- so decoding is STRICT against the
//! closed set the code-owned repair row declares ([`ValueSlot`]): an unknown
//! prefix, a shape outside the row's accepted set, a ceiling that is not a
//! canonical in-range integer, an empty or non-ASCII token all decode to
//! `None`, with a WARN and a counter. A value is never derived from the stored
//! text: a decoded shape is the row's own `&'static str` member, so a token can
//! only ever select a value the code already ships.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use routectl_core::sanitize_for_log;

use crate::field_verdict::FieldVerdictKey;

const SHAPE_PREFIX: &str = "shape:";
const CEILING_PREFIX: &str = "ceiling:";

/// Process-lifetime count of persisted value tokens refused by [`VerdictValue::decode`].
static MALFORMED_VALUE_TOTAL: AtomicU64 = AtomicU64::new(0);

/// The value a committed field verdict applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerdictValue {
    /// Rewrite the field to this member of its row's closed accepted set.
    Shape(&'static str),
    /// Clamp a token budget to this ceiling. Zero is not a ceiling: it would
    /// read as "emit no output", so it never decodes.
    Ceiling(u32),
}

/// What the code-owned repair row says a value for it may be: the closed set
/// a token is decoded against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueSlot {
    /// A rewrite row: a shape must be one of these members.
    Shape(&'static [&'static str]),
    /// A ceiling row: a nonzero `u32` in canonical decimal.
    Ceiling,
}

/// Why a persisted token was refused. Logged, never persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Malformed {
    Empty,
    NonAscii,
    UnknownPrefix,
    PrefixNotForRow,
    ShapeNotMember,
    CeilingNotCanonical,
    CeilingOutOfRange,
}

impl Malformed {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::NonAscii => "non_ascii",
            Self::UnknownPrefix => "unknown_prefix",
            Self::PrefixNotForRow => "prefix_not_for_row",
            Self::ShapeNotMember => "shape_not_member",
            Self::CeilingNotCanonical => "ceiling_not_canonical",
            Self::CeilingOutOfRange => "ceiling_out_of_range",
        }
    }
}

impl VerdictValue {
    /// The persisted `upstream_token` spelling of this value.
    pub fn encode(&self) -> String {
        match self {
            Self::Shape(token) => format!("{SHAPE_PREFIX}{token}"),
            Self::Ceiling(ceiling) => format!("{CEILING_PREFIX}{ceiling}"),
        }
    }

    /// Decode a persisted token against `slot`, or `None` -- with a WARN and a
    /// bump of [`Self::malformed_total`] -- when it is not a value that slot
    /// admits.
    pub fn decode(token: &str, slot: ValueSlot) -> Option<Self> {
        decode_counted(token, slot, &MALFORMED_VALUE_TOTAL)
    }

    /// Process-lifetime count of tokens [`Self::decode`] refused.
    pub fn malformed_total() -> u64 {
        MALFORMED_VALUE_TOTAL.load(Ordering::Relaxed)
    }
}

/// [`VerdictValue::decode`] against an explicit counter, so a test can count
/// its own refusals without racing every other decode in the process.
fn decode_counted(token: &str, slot: ValueSlot, malformed: &AtomicU64) -> Option<VerdictValue> {
    match parse(token, slot) {
        Ok(value) => Some(value),
        Err(reason) => {
            malformed.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                event = "field_verdict_value_malformed",
                reason = reason.as_str(),
                token = %sanitize_for_log(token),
                "persisted field-verdict value refused; the verdict carries no value"
            );
            None
        }
    }
}

fn parse(token: &str, slot: ValueSlot) -> Result<VerdictValue, Malformed> {
    if token.is_empty() {
        return Err(Malformed::Empty);
    }
    if !token.is_ascii() {
        return Err(Malformed::NonAscii);
    }
    if let Some(shape) = token.strip_prefix(SHAPE_PREFIX) {
        let ValueSlot::Shape(accepted) = slot else {
            return Err(Malformed::PrefixNotForRow);
        };
        return accepted
            .iter()
            .copied()
            .find(|member| *member == shape)
            .map(VerdictValue::Shape)
            .ok_or(Malformed::ShapeNotMember);
    }
    if let Some(digits) = token.strip_prefix(CEILING_PREFIX) {
        if slot != ValueSlot::Ceiling {
            return Err(Malformed::PrefixNotForRow);
        }
        return parse_ceiling(digits).map(VerdictValue::Ceiling);
    }
    Err(Malformed::UnknownPrefix)
}

/// `digits` as a ceiling under the persisted codec's own rule (canonical
/// decimal, nonzero, in `u32`), or `None`.
pub fn canonical_ceiling(digits: &str) -> Option<u32> {
    parse_ceiling(digits).ok()
}

/// Canonical decimal only (no sign, no leading zero, no whitespace), so every
/// accepted token is exactly what [`VerdictValue::encode`] writes.
fn parse_ceiling(digits: &str) -> Result<u32, Malformed> {
    let canonical = !digits.is_empty()
        && digits.bytes().all(|b| b.is_ascii_digit())
        && !(digits.len() > 1 && digits.starts_with('0'));
    if !canonical {
        return Err(Malformed::CeilingNotCanonical);
    }
    match digits.parse::<u32>() {
        Ok(0) | Err(_) => Err(Malformed::CeilingOutOfRange),
        Ok(ceiling) => Ok(ceiling),
    }
}

// ---------------------------------------------------------------------------
// The resident value of each field verdict
// ---------------------------------------------------------------------------
//
// The learned-capability registry stores a verdict's lifecycle (tier, decay,
// observations) but has no notion of a value, so a rewrite verdict's value
// lives here, beside it. Every mutation of the learned row mints a new
// incarnation, and a value is answered only for the exact incarnation it was
// recorded against: a lookup naming any other lifecycle -- a cleared, purged,
// or relearned verdict -- answers nothing.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StoredValue {
    incarnation: u64,
    value: VerdictValue,
}

/// Per-identity value of the resident field verdict. One entry per identity:
/// recording a new incarnation replaces the previous one.
#[derive(Debug, Default)]
pub struct FieldValueStore {
    values: Mutex<HashMap<FieldVerdictKey, StoredValue>>,
}

impl FieldValueStore {
    /// The value recorded for `key` at exactly `incarnation`, or `None`.
    #[must_use]
    pub fn value_at(&self, key: &FieldVerdictKey, incarnation: u64) -> Option<VerdictValue> {
        self.values
            .lock()
            .get(key)
            .filter(|stored| stored.incarnation == incarnation)
            .map(|stored| stored.value)
    }

    /// The most recently recorded value for `key`, whatever incarnation it was
    /// recorded at: what a new observation is compared against to decide
    /// whether it continues the same lifecycle or starts a new one.
    #[must_use]
    pub fn latest(&self, key: &FieldVerdictKey) -> Option<VerdictValue> {
        self.values.lock().get(key).map(|stored| stored.value)
    }

    /// Record `value` as `key`'s value at `incarnation`; `None` forgets any
    /// value, because a verdict minted without one must not inherit its
    /// predecessor's.
    pub fn record(&self, key: &FieldVerdictKey, incarnation: u64, value: Option<VerdictValue>) {
        let mut values = self.values.lock();
        match value {
            Some(value) => {
                values.insert(key.clone(), StoredValue { incarnation, value });
            }
            None => {
                values.remove(key);
            }
        }
    }

    /// Drop any value for `key`.
    pub fn forget(&self, key: &FieldVerdictKey) {
        self.values.lock().remove(key);
    }
}

#[cfg(test)]
#[path = "field_verdict_value_tests.rs"]
mod tests;
