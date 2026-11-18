//! Reducers from a vendor's usage-endpoint body onto one complete account
//! observation.
//!
//! Pure functions over bytes the caller already fetched and stamped; nothing
//! here performs I/O. A body reduces to `Complete(windows)` or `NoWindows`
//! only after the WHOLE body validates. Every other outcome is a typed
//! [`BodyError`], because the store reads `NoWindows` as "this account has no
//! cap": a missing member, a mistyped value or an unknown top-level shape must
//! never be spelled that way.
//!
//! # Why a duplicated JSON key refuses the whole body
//!
//! A JSON object with one member twice has no single meaning: one parser keeps
//! the first, another the last. Reading a quota from such a body would be a
//! guess about which one the vendor meant, so the parse refuses it at any
//! depth instead of inheriting `serde_json`'s last-wins behavior.
//!
//! # Why a percent above 100 is refused here
//!
//! The header reducers saturate an over-scale value to exhausted. These bodies
//! report a rounded percent of the account's own limit, so a value past 100 is
//! not "slightly over"; it is a unit or shape this code does not know, and the
//! window model would silently saturate it if it got that far.

use std::cell::Cell;
use std::fmt;
use std::time::{Duration, SystemTime};

use serde::de::{self, DeserializeSeed, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use super::freshness::ObservationStamp;
use super::reading::{
    AccountObservation, MAX_WINDOWS_PER_OBSERVATION, ObservationReport, ReadingSource, Scope,
    Started, WindowKey, WindowReading, WindowSet,
};
use super::reduce::{PERCENT_SCALE, RejectionReason};

const FIVE_HOURS_SECS: u64 = 5 * 3_600;
const SEVEN_DAYS_SECS: u64 = 7 * 86_400;

/// Anthropic members that must be present (each may be `null`) for a body to
/// count as the usage shape at all.
const ANTHROPIC_REQUIRED_MEMBERS: [&str; 2] = ["five_hour", "seven_day"];

/// Prefix of Anthropic's scoped seven-day family; the rest of the member name
/// is the window's scope.
const ANTHROPIC_SCOPED_WEEKLY_PREFIX: &str = "seven_day_";

const CODEX_WINDOW_MEMBERS: [&str; 2] = ["primary_window", "secondary_window"];

/// The longest usage body either reducer reads. A parsed JSON tree can take
/// many times its text's size, so a longer body is refused before parsing;
/// a fetcher cuts its read at this same bound.
pub const MAX_USAGE_BODY_BYTES: usize = 64 * 1024;

/// Why a usage body did not reduce to an observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyError {
    /// The body was empty or whitespace only.
    Empty,
    /// The body was longer than [`MAX_USAGE_BODY_BYTES`].
    TooLarge,
    /// The body was not one complete JSON value.
    NotJson,
    /// An object in the body carried one member name twice.
    DuplicateKey,
    /// A member the shape requires was absent.
    MissingMember,
    /// A value had the wrong JSON type, or a number that should be whole
    /// non-negative seconds was not.
    WrongType,
    /// A reset string was not an RFC 3339 timestamp.
    InvalidTimestamp,
    /// A percent was negative or above 100.
    InvalidPercent,
    /// A scoped window's name did not yield a canonical scope tag.
    NonCanonicalScope,
    /// The body reported more windows than one observation may carry.
    TooManyWindows,
    /// Two reported windows had the same length and scope.
    DuplicateWindow,
    /// The window model refused a reported window or the observation.
    Window(RejectionReason),
}

impl From<RejectionReason> for BodyError {
    fn from(reason: RejectionReason) -> Self {
        Self::Window(reason)
    }
}

/// Which vendor's usage-endpoint body a poll fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageVendor {
    /// The Anthropic OAuth usage endpoint.
    Anthropic,
    /// The Codex usage endpoint.
    Codex,
}

impl UsageVendor {
    /// Reduce `body`, read at `observed`, with this vendor's reducer.
    pub fn reduce(
        self,
        body: &[u8],
        observed: ObservationStamp,
    ) -> Result<AccountObservation, BodyError> {
        match self {
            Self::Anthropic => reduce_anthropic_body(body, observed),
            Self::Codex => reduce_codex_body(body, observed),
        }
    }
}

impl BodyError {
    /// The rejection reason this error is counted under.
    pub const fn reason(self) -> RejectionReason {
        match self {
            Self::Window(reason) => reason,
            _ => RejectionReason::MalformedBody,
        }
    }
}

/// Reduce an Anthropic usage body read at `observed`.
///
/// Windows come only from `five_hour`, `seven_day` and the `seven_day_<scope>`
/// family; every other member is ignored. A known window member is either
/// `null` (no such window) or `{utilization, resets_at}` with a 0-100 percent
/// and an RFC 3339 reset, where a `null` reset marks a window that has not
/// started.
pub fn reduce_anthropic_body(
    body: &[u8],
    observed: ObservationStamp,
) -> Result<AccountObservation, BodyError> {
    let members = parse_object(body)?;
    for name in ANTHROPIC_REQUIRED_MEMBERS {
        member(&members, name)?;
    }
    let mut windows = Vec::new();
    for (name, value) in &members {
        let Some(key) = anthropic_window_key(name)? else {
            continue;
        };
        let Some(window) = object_or_null(value)? else {
            continue;
        };
        push_bounded(&mut windows, anthropic_window(key, window, &observed)?)?;
    }
    complete(windows, observed)
}

/// Reduce a Codex usage body read at `observed`.
///
/// Windows come only from `rate_limit.primary_window` and
/// `rate_limit.secondary_window`, each `null` or `{used_percent,
/// limit_window_seconds, reset_after_seconds, reset_at}`, and from each
/// `additional_rate_limits[]` entry's `rate_limit` windows. A window whose
/// `reset_after_seconds` is at least its length has not started.
///
/// An additional limit's windows are scoped by its `metered_feature`, the
/// vendor's limit id (`limit_name` is a display name). A `metered_feature`
/// that is not a canonical scope refuses the whole body: skipping that entry
/// would let a complete report drop a window the account still has.
pub fn reduce_codex_body(
    body: &[u8],
    observed: ObservationStamp,
) -> Result<AccountObservation, BodyError> {
    let members = parse_object(body)?;
    let rate_limit = member(&members, "rate_limit")?
        .as_object()
        .ok_or(BodyError::WrongType)?;
    let mut windows = Vec::new();
    for name in CODEX_WINDOW_MEMBERS {
        if let Some(window) = object_or_null(member(rate_limit, name)?)? {
            push_bounded(&mut windows, codex_window(window, None, &observed)?)?;
        }
    }
    for entry in codex_additional_limits(&members)? {
        let scope = Scope::new(
            member(entry, "metered_feature")?
                .as_str()
                .ok_or(BodyError::WrongType)?,
        )
        .map_err(|_| BodyError::NonCanonicalScope)?;
        let Some(rate_limit) = object_or_null(member(entry, "rate_limit")?)? else {
            continue;
        };
        for name in CODEX_WINDOW_MEMBERS {
            let Some(window) = rate_limit
                .get(name)
                .map(object_or_null)
                .transpose()?
                .flatten()
            else {
                continue;
            };
            push_bounded(
                &mut windows,
                codex_window(window, Some(scope.clone()), &observed)?,
            )?;
        }
    }
    complete(windows, observed)
}

/// The `additional_rate_limits` entries; an absent or `null` member is none.
fn codex_additional_limits(
    members: &Map<String, Value>,
) -> Result<Vec<&Map<String, Value>>, BodyError> {
    match members.get("additional_rate_limits") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(entries)) => entries
            .iter()
            .map(|entry| entry.as_object().ok_or(BodyError::WrongType))
            .collect(),
        Some(_) => Err(BodyError::WrongType),
    }
}

fn anthropic_window_key(name: &str) -> Result<Option<WindowKey>, BodyError> {
    let (length_secs, scope) = match name {
        "five_hour" => (FIVE_HOURS_SECS, None),
        "seven_day" => (SEVEN_DAYS_SECS, None),
        _ => {
            let Some(raw_scope) = name.strip_prefix(ANTHROPIC_SCOPED_WEEKLY_PREFIX) else {
                return Ok(None);
            };
            let scope = Scope::new(raw_scope).map_err(|_| BodyError::NonCanonicalScope)?;
            (SEVEN_DAYS_SECS, Some(scope))
        }
    };
    Ok(Some(WindowKey::new(length_secs, scope)?))
}

fn anthropic_window(
    key: WindowKey,
    window: &Map<String, Value>,
    observed: &ObservationStamp,
) -> Result<WindowReading, BodyError> {
    let used = percent_as_fraction(member(window, "utilization")?)?;
    let reset_at = match member(window, "resets_at")? {
        Value::Null => None,
        Value::String(raw) => Some(rfc3339(raw)?),
        _ => return Err(BodyError::WrongType),
    };
    let started = if reset_at.is_some() {
        Started::Yes
    } else {
        Started::No
    };
    Ok(WindowReading::new(key, used, reset_at, started, observed)?)
}

fn codex_window(
    window: &Map<String, Value>,
    scope: Option<Scope>,
    observed: &ObservationStamp,
) -> Result<WindowReading, BodyError> {
    let used = percent_as_fraction(member(window, "used_percent")?)?;
    let length_secs = whole_seconds(member(window, "limit_window_seconds")?)?;
    let reset_after_secs = whole_seconds(member(window, "reset_after_seconds")?)?;
    let reset_at = epoch_seconds(whole_seconds(member(window, "reset_at")?)?)?;
    let started = if reset_after_secs >= length_secs {
        Started::No
    } else {
        Started::Yes
    };
    let key = WindowKey::new(length_secs, scope)?;
    Ok(WindowReading::new(
        key,
        used,
        Some(reset_at),
        started,
        observed,
    )?)
}

/// Append one window, refusing the body once it exceeds the per-observation
/// bound rather than collecting the excess first.
fn push_bounded(windows: &mut Vec<WindowReading>, window: WindowReading) -> Result<(), BodyError> {
    if windows.len() >= MAX_WINDOWS_PER_OBSERVATION {
        return Err(BodyError::TooManyWindows);
    }
    windows.push(window);
    Ok(())
}

fn complete(
    windows: Vec<WindowReading>,
    observed: ObservationStamp,
) -> Result<AccountObservation, BodyError> {
    let report = if windows.is_empty() {
        ObservationReport::NoWindows
    } else {
        let has_duplicate = windows.iter().enumerate().any(|(i, window)| {
            windows[..i]
                .iter()
                .any(|earlier| earlier.key() == window.key())
        });
        if has_duplicate {
            return Err(BodyError::DuplicateWindow);
        }
        ObservationReport::Complete(WindowSet::new(windows)?)
    };
    Ok(AccountObservation::new(
        ReadingSource::Poll,
        observed,
        report,
    )?)
}

fn member<'a>(object: &'a Map<String, Value>, name: &str) -> Result<&'a Value, BodyError> {
    object.get(name).ok_or(BodyError::MissingMember)
}

const fn object_or_null(value: &Value) -> Result<Option<&Map<String, Value>>, BodyError> {
    match value {
        Value::Null => Ok(None),
        Value::Object(object) => Ok(Some(object)),
        _ => Err(BodyError::WrongType),
    }
}

/// The one place a vendor's 0-100 percent becomes a 0-1 fraction.
fn percent_as_fraction(value: &Value) -> Result<f64, BodyError> {
    let percent = value.as_f64().ok_or(BodyError::WrongType)?;
    if !percent.is_finite() || !(0.0..=PERCENT_SCALE).contains(&percent) {
        return Err(BodyError::InvalidPercent);
    }
    Ok(percent / PERCENT_SCALE)
}

fn whole_seconds(value: &Value) -> Result<u64, BodyError> {
    value.as_u64().ok_or(BodyError::WrongType)
}

fn epoch_seconds(secs: u64) -> Result<SystemTime, BodyError> {
    SystemTime::UNIX_EPOCH
        .checked_add(Duration::from_secs(secs))
        .ok_or(BodyError::Window(RejectionReason::Overflow))
}

/// Parse an RFC 3339 instant without `time`'s panicking `SystemTime`
/// conversion.
fn rfc3339(raw: &str) -> Result<SystemTime, BodyError> {
    let parsed = OffsetDateTime::parse(raw, &Rfc3339).map_err(|_| BodyError::InvalidTimestamp)?;
    let secs = parsed.unix_timestamp();
    let whole = Duration::from_secs(secs.unsigned_abs());
    let at_second = if secs >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(whole)
    } else {
        SystemTime::UNIX_EPOCH.checked_sub(whole)
    };
    at_second
        .and_then(|at| at.checked_add(Duration::from_nanos(u64::from(parsed.nanosecond()))))
        .ok_or(BodyError::Window(RejectionReason::Overflow))
}

/// Parse a body whose top level must be a JSON object, refusing a body longer
/// than [`MAX_USAGE_BODY_BYTES`] and duplicate keys at every depth.
fn parse_object(body: &[u8]) -> Result<Map<String, Value>, BodyError> {
    if body.len() > MAX_USAGE_BODY_BYTES {
        return Err(BodyError::TooLarge);
    }
    if body.trim_ascii().is_empty() {
        return Err(BodyError::Empty);
    }
    let duplicate = Cell::new(false);
    let mut deserializer = serde_json::Deserializer::from_slice(body);
    let parsed = StrictJson {
        duplicate: &duplicate,
    }
    .deserialize(&mut deserializer)
    .and_then(|value| deserializer.end().map(|()| value));
    match parsed {
        Ok(Value::Object(members)) => Ok(members),
        Ok(_) => Err(BodyError::WrongType),
        Err(_) if duplicate.get() => Err(BodyError::DuplicateKey),
        Err(_) => Err(BodyError::NotJson),
    }
}

/// Builds a `serde_json::Value` like its own `Deserialize`, except that an
/// object member seen twice stops the parse and sets `duplicate`.
#[derive(Clone, Copy)]
struct StrictJson<'a> {
    duplicate: &'a Cell<bool>,
}

impl<'de> DeserializeSeed<'de> for StrictJson<'_> {
    type Value = Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}

impl<'de> Visitor<'de> for StrictJson<'_> {
    type Value = Value;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<Value, E> {
        Ok(Value::Bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<Value, E> {
        Ok(Value::from(v))
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<Value, E> {
        Ok(Value::from(v))
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<Value, E> {
        Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("non-finite number"))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<Value, E> {
        Ok(Value::String(v.to_owned()))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<Value, E> {
        Ok(Value::String(v))
    }

    fn visit_unit<E: de::Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
        let mut items = Vec::new();
        while let Some(item) = seq.next_element_seed(self)? {
            items.push(item);
        }
        Ok(Value::Array(items))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
        let mut members = Map::new();
        while let Some(key) = map.next_key::<String>()? {
            if members.contains_key(&key) {
                self.duplicate.set(true);
                return Err(de::Error::custom("duplicate object key"));
            }
            let value = map.next_value_seed(self)?;
            members.insert(key, value);
        }
        Ok(Value::Object(members))
    }
}

#[cfg(test)]
#[path = "usage_body_tests.rs"]
mod usage_body_tests;
