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
const GROUNDED_LOCATION_SUFFIXES: [&str; 1] = ["Extra inputs are not permitted"];

const QUOTED_NOT_SUPPORTED: &str = " is not supported";
const QUOTED_GROUNDED_CONTINUATION: &str = " for this model. Use \"";

const OPENAI_UNSUPPORTED_PREFIX: &str = "Unsupported parameter: '";
const OPENAI_UNSUPPORTED_SUFFIX: &str = "' is not supported with this model.";

/// A rejection attributed to one closed-table row.
///
/// Holds no upstream bytes: `row` is the table's own entry and `accepted`,
/// when set, is a member of that row's closed accepted set.
#[derive(Clone, Copy, Debug)]
pub(super) struct NamedRejection {
    pub(super) row: &'static FieldRepairRow,
    pub(super) tier: SignalTier,
    pub(super) accepted: Option<&'static str>,
}

/// Name the closed-table row a rejection rejected, or `None` when the kind
/// has no parser, the status cannot carry a field rejection, the body did
/// not parse, no arm localizes a path, or the path has no row.
pub(super) fn parse_rejection(
    provider_kind: &str,
    status: u16,
    parsed: Option<&ParsedErrorBody>,
) -> Option<NamedRejection> {
    let localized = localize(provider_kind, status, parsed?)?;
    Some(NamedRejection {
        row: resolve(&localized.segments)?,
        tier: localized.tier,
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
            "openai-compat" | "openai-responses" => Some(Self::OpenAi),
            _ => None,
        }
    }
}

/// A path an arm lifted from the envelope, already canonicalized. Private:
/// the segments borrow upstream text and never leave this module.
#[derive(Clone, Debug)]
struct LocalizedPath<'a> {
    segments: Vec<&'a str>,
    tier: SignalTier,
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
    let (path, tier) = match dialect {
        Dialect::AnthropicApi => localize_message(anthropic_message(parsed)?),
        Dialect::Bedrock => {
            localize_message(strip_converse_errors_prefix(bedrock_message(parsed)?))
        }
        Dialect::OpenAi => localize_openai(parsed),
    }?;
    Some(LocalizedPath {
        segments: canonical_segments(dialect, path)?,
        tier,
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

fn localize_message(message: &str) -> Option<(&str, SignalTier)> {
    quoted_not_supported(message).or_else(|| location_prefix(message))
}

/// `"a.b" is not supported for this model. Use "x.y" ...`
fn quoted_not_supported(message: &str) -> Option<(&str, SignalTier)> {
    let (path, tail) = message.strip_prefix('"')?.split_once('"')?;
    let continuation = tail.strip_prefix(QUOTED_NOT_SUPPORTED)?;
    let tier = if continuation.starts_with(QUOTED_GROUNDED_CONTINUATION) {
        SignalTier::SelfIdentifying
    } else {
        SignalTier::Inferred
    };
    Some((path, tier))
}

/// `<dotted.path>: <reason>`, the request-validation location prefix.
fn location_prefix(message: &str) -> Option<(&str, SignalTier)> {
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
    Some((path, tier))
}

fn localize_openai(parsed: &ParsedErrorBody) -> Option<(&str, SignalTier)> {
    if let Some(param) = parsed.error_param() {
        return Some((param, SignalTier::SelfIdentifying));
    }
    let param = parsed
        .error_message()?
        .strip_prefix(OPENAI_UNSUPPORTED_PREFIX)?
        .strip_suffix(OPENAI_UNSUPPORTED_SUFFIX)?;
    Some((param, SignalTier::SelfIdentifying))
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
