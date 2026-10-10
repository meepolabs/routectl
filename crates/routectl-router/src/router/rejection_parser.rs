//! Per-dialect rejection parser: reads the carried parse of an upstream
//! rejection and names the closed-table field row it rejected, with the
//! signal tier that attribution earns.
//!
//! An upstream names a field path in free text it controls, so the parser
//! never hands that text onward. A localized path is canonicalized (array
//! indices dropped, the Bedrock Converse request-field bag removed) and
//! compared against the closed field-repair table; only a matching row's own
//! `&'static` entry leaves this module. A path with no row resolves to `None`,
//! exactly like an envelope no arm recognizes.
//!
//! Tiers: a structured field (`error.param`) or a message matching a template
//! grounded on a captured envelope is [`SignalTier::SelfIdentifying`]; any
//! other prose attribution is [`SignalTier::Inferred`] and needs independent
//! confirmation before it acts.
//!
//! Kind is orthogonal to tier: it says whether the upstream refused the field
//! itself or only the value sent in it. Dropping a field whose value was
//! refused also strips every valid value from later requests, so only an
//! [`RejectionKind::UnsupportedField`] may justify a learned drop.

use routectl_core::capability::SignalTier;
use routectl_core::{ParsedErrorBody, is_safe_token, strip_converse_errors_prefix};

use super::field_repair::{FieldRepairRow, closed_table};

/// Statuses a caller-shaped field rejection arrives on. Everything else says
/// nothing about the request envelope.
const REJECTION_STATUSES: [u16; 2] = [400, 422];

/// The Anthropic Messages error type a field rejection carries.
const ANTHROPIC_INVALID_REQUEST: &str = "invalid_request_error";

/// The Converse wrapper object that carries model-specific request fields;
/// a Converse rejection may name a field relative to the request root.
const CONVERSE_FIELD_BAG: &str = "additionalModelRequestFields";

/// Location-prefix suffixes grounded on captured envelopes. A prefix followed
/// by anything else is still a localization, but only an inferred one.
const GROUNDED_LOCATION_SUFFIX_PREFIXES: [&str; 1] = ["Input should be "];
const GROUNDED_LOCATION_SUFFIXES: [&str; 1] = [LOCATION_UNSUPPORTED_FIELD_REASON];

const QUOTED_NOT_SUPPORTED: &str = " is not supported";
const QUOTED_GROUNDED_CONTINUATION: &str = " for this model. Use \"";

const OPENAI_UNSUPPORTED_PREFIX: &str = "Unsupported parameter: '";
const OPENAI_UNSUPPORTED_SUFFIX: &str = "' is not supported with this model.";
const OPENAI_UNSUPPORTED_CODE: &str = "unsupported_parameter";

/// The location-prefix reason that refuses the field itself; every other
/// reason refuses the value.
const LOCATION_UNSUPPORTED_FIELD_REASON: &str = "Extra inputs are not permitted";

/// What the upstream refused at the named path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RejectionKind {
    /// The field is not accepted at all on this lane.
    UnsupportedField,
    /// The field is accepted; the value sent in it is not.
    InvalidValue,
}

/// A rejection attributed to one closed-table row.
///
/// Holds no upstream bytes: `row` is the table's own entry and `accepted`,
/// when set, is a member of that row's closed accepted set.
#[derive(Clone, Copy, Debug)]
pub(super) struct NamedRejection {
    pub(super) row: &'static FieldRepairRow,
    pub(super) tier: SignalTier,
    pub(super) kind: RejectionKind,
    pub(super) accepted: Option<&'static str>,
}

/// Name the closed-table row a rejection rejected, or `None` when the kind
/// has no parser, the status cannot carry a field rejection, the body did
/// not parse, no arm localizes a path, or the path has no row. A path ending
/// in an array index names one element, not the field a row describes, so it
/// has no row either.
pub(super) fn parse_rejection(
    provider_kind: &str,
    status: u16,
    parsed: Option<&ParsedErrorBody>,
) -> Option<NamedRejection> {
    let localized = localize(provider_kind, status, parsed?)?;
    if localized.ends_in_index {
        return None;
    }
    Some(NamedRejection {
        row: resolve(&localized.segments)?,
        tier: localized.tier,
        kind: localized.kind,
        accepted: None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Dialect {
    AnthropicApi,
    Bedrock,
    OpenAi,
}

impl Dialect {
    fn of_kind(provider_kind: &str) -> Option<Self> {
        match provider_kind {
            "anthropic-api" => Some(Self::AnthropicApi),
            "bedrock" => Some(Self::Bedrock),
            "openai-compat" => Some(Self::OpenAi),
            _ => None,
        }
    }
}

/// A path an arm lifted from the envelope, already canonicalized. Private:
/// the segments borrow upstream text and never leave this module.
#[derive(Clone, Debug)]
struct LocalizedPath<'a> {
    segments: Vec<&'a str>,
    ends_in_index: bool,
    tier: SignalTier,
    kind: RejectionKind,
}

/// One arm's reading of an envelope: the raw path and what it says about it.
#[derive(Clone, Copy, Debug)]
struct Attribution<'a> {
    path: &'a str,
    tier: SignalTier,
    kind: RejectionKind,
}

fn localize<'a>(
    provider_kind: &str,
    status: u16,
    parsed: &'a ParsedErrorBody,
) -> Option<LocalizedPath<'a>> {
    if !REJECTION_STATUSES.contains(&status) {
        return None;
    }
    let dialect = Dialect::of_kind(provider_kind)?;
    let attribution = match dialect {
        Dialect::AnthropicApi => localize_message(anthropic_message(parsed)?),
        Dialect::Bedrock => {
            localize_message(strip_converse_errors_prefix(bedrock_message(parsed)?))
        }
        Dialect::OpenAi => localize_openai(parsed),
    }?;
    Some(LocalizedPath {
        segments: canonical_segments(dialect, attribution.path)?,
        ends_in_index: ends_in_index(attribution.path),
        tier: attribution.tier,
        kind: attribution.kind,
    })
}

fn anthropic_message(parsed: &ParsedErrorBody) -> Option<&str> {
    (parsed.error_type()? == ANTHROPIC_INVALID_REQUEST)
        .then(|| parsed.error_message())
        .flatten()
}

/// bedrock-runtime returns the flat `{"message": ...}` shape, not the
/// `{"error": {...}}` envelope.
fn bedrock_message(parsed: &ParsedErrorBody) -> Option<&str> {
    parsed.value().get("message")?.as_str()
}

fn localize_message(message: &str) -> Option<Attribution<'_>> {
    quoted_not_supported(message).or_else(|| location_prefix(message))
}

/// `"a.b" is not supported for this model. Use "x.y" ...`
fn quoted_not_supported(message: &str) -> Option<Attribution<'_>> {
    let (path, tail) = message.strip_prefix('"')?.split_once('"')?;
    let continuation = tail.strip_prefix(QUOTED_NOT_SUPPORTED)?;
    let tier = if continuation.starts_with(QUOTED_GROUNDED_CONTINUATION) {
        SignalTier::SelfIdentifying
    } else {
        SignalTier::Inferred
    };
    Some(Attribution {
        path,
        tier,
        kind: RejectionKind::UnsupportedField,
    })
}

/// `<dotted.path>: <reason>`, the request-validation location prefix.
fn location_prefix(message: &str) -> Option<Attribution<'_>> {
    let (path, reason) = message.split_once(": ")?;
    let grounded = GROUNDED_LOCATION_SUFFIXES.contains(&reason)
        || GROUNDED_LOCATION_SUFFIX_PREFIXES
            .iter()
            .any(|prefix| reason.starts_with(prefix));
    let tier = if grounded {
        SignalTier::SelfIdentifying
    } else {
        SignalTier::Inferred
    };
    let kind = if reason == LOCATION_UNSUPPORTED_FIELD_REASON {
        RejectionKind::UnsupportedField
    } else {
        RejectionKind::InvalidValue
    };
    Some(Attribution { path, tier, kind })
}

/// `error.param` alone names a field without saying the field was refused;
/// only the `unsupported_parameter` code or the exact template naming the
/// same parameter does.
fn localize_openai(parsed: &ParsedErrorBody) -> Option<Attribution<'_>> {
    let templated = parsed.error_message().and_then(unsupported_parameter);
    let (path, unsupported) = match parsed.error_param() {
        Some(param) => (
            param,
            parsed.error_code().as_deref() == Some(OPENAI_UNSUPPORTED_CODE)
                || templated == Some(param),
        ),
        None => (templated?, true),
    };
    let kind = if unsupported {
        RejectionKind::UnsupportedField
    } else {
        RejectionKind::InvalidValue
    };
    Some(Attribution {
        path,
        tier: SignalTier::SelfIdentifying,
        kind,
    })
}

/// `Unsupported parameter: '<x>' is not supported with this model.`
fn unsupported_parameter(message: &str) -> Option<&str> {
    message
        .strip_prefix(OPENAI_UNSUPPORTED_PREFIX)?
        .strip_suffix(OPENAI_UNSUPPORTED_SUFFIX)
}

fn resolve(segments: &[&str]) -> Option<&'static FieldRepairRow> {
    closed_table()
        .iter()
        .find(|row| row.path.split('.').eq(segments.iter().copied()))
}

/// The canonical segments of `raw`: indices (`.0` or `[0]`) dropped and, on
/// Bedrock, a leading Converse field bag removed. `None` when `raw` is not a
/// bounded ASCII dotted path.
fn canonical_segments(dialect: Dialect, raw: &str) -> Option<Vec<&str>> {
    if !is_safe_token(raw) {
        return None;
    }
    let mut segments = Vec::new();
    for segment in raw.split('.') {
        let name = strip_bracket_indices(segment)?;
        if is_index(name) {
            continue;
        }
        if name.is_empty() || !name.bytes().all(is_name_byte) {
            return None;
        }
        segments.push(name);
    }
    if dialect == Dialect::Bedrock && segments.first() == Some(&CONVERSE_FIELD_BAG) {
        segments.remove(0);
    }
    (!segments.is_empty()).then_some(segments)
}

/// True when the last segment of `raw` is an index (`tools.0`, `tools[0]`).
/// Only meaningful once [`canonical_segments`] accepted `raw`, which proves
/// every bracket holds an index.
fn ends_in_index(raw: &str) -> bool {
    raw.rsplit('.')
        .next()
        .is_some_and(|last| is_index(last) || last.ends_with(']'))
}

fn strip_bracket_indices(segment: &str) -> Option<&str> {
    let mut name = segment;
    while let Some(open) = name.strip_suffix(']') {
        let (head, index) = open.rsplit_once('[')?;
        if !is_index(index) {
            return None;
        }
        name = head;
    }
    Some(name)
}

fn is_index(segment: &str) -> bool {
    !segment.is_empty() && segment.bytes().all(|b| b.is_ascii_digit())
}

const fn is_name_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

#[cfg(test)]
#[path = "rejection_parser_tests.rs"]
mod tests;
