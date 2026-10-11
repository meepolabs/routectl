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
//!
//! For a row that rewrites rather than drops, the parser also reports the
//! accepted value the envelope names. Candidates come ONLY from the template
//! position a grounded arm knows to list replacements (the quoted list after
//! `Input should be `, the quoted token after `Use `), never from a scan of the
//! whole message: an envelope may also echo the value it refused, and that
//! echo can be a member of the set. A candidate counts only when it equals a
//! member of the row's closed accepted set, and only exactly ONE named member
//! fills it; none or several leave it empty, so a rewrite never picks between
//! candidates.

use std::fmt;

use routectl_core::capability::SignalTier;
use routectl_core::wire_check::WirePath;
use routectl_core::{ParsedErrorBody, is_safe_token, strip_converse_errors_prefix};

use super::field_repair::{FieldRepairRow, FieldRepairTable, FieldTransform};

/// Statuses a caller-shaped field rejection arrives on. Everything else says
/// nothing about the request envelope.
pub(super) const REJECTION_STATUSES: [u16; 2] = [400, 422];

/// The Anthropic Messages error type a field rejection carries.
const ANTHROPIC_INVALID_REQUEST: &str = "invalid_request_error";

/// The Converse wrapper object that carries model-specific request fields;
/// a Converse rejection may name a field relative to the request root.
const CONVERSE_FIELD_BAG: &str = "additionalModelRequestFields";

/// The location-prefix reasons grounded on captured envelopes are this list
/// lead-in and [`LOCATION_UNSUPPORTED_FIELD_REASON`]. A prefix followed by
/// anything else is still a localization, but only an inferred one.
const GROUNDED_VALUE_LIST_PREFIX: &str = "Input should be ";

const QUOTED_NOT_SUPPORTED: &str = " is not supported";
/// Ends on the opening quote of the replacement it names.
const QUOTED_GROUNDED_CONTINUATION: &str = " for this model. Use \"";

/// Separators between members of an `Input should be` list, longest first so
/// `, or ` is not read as `, ` followed by prose.
const LIST_SEPARATORS: [&str; 3] = [", or ", ", ", " or "];

const OPENAI_UNSUPPORTED_PREFIX: &str = "Unsupported parameter: '";
const OPENAI_UNSUPPORTED_SUFFIX: &str = "' is not supported with this model.";
/// The openai `error.code` that refuses a named parameter.
pub(super) const OPENAI_UNSUPPORTED_CODE: &str = "unsupported_parameter";

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
    table: FieldRepairTable,
    provider_kind: &str,
    status: u16,
    parsed: Option<&ParsedErrorBody>,
) -> Option<NamedRejection> {
    let localized = localize(provider_kind, status, parsed?)?;
    if localized.ends_in_index {
        return None;
    }
    let row = resolve(table, &localized.segments)?;
    Some(NamedRejection {
        row,
        tier: localized.tier,
        kind: localized.kind,
        accepted: named_accepted(row, &localized.replacements),
    })
}

/// Where a rejection says the refused field sits.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct RejectedWirePath {
    /// The location in the request body as the upstream named it, array
    /// indices kept, sanitized for logging.
    pub(super) wire: WirePath,
    /// The same location unmasked, one segment per key or index, to look up
    /// in a wire body.
    pub(super) locator: RawPath,
    /// The canonical field path, unmasked: indices dropped and, on Bedrock,
    /// the Converse field bag removed -- the shape a closed-table row path
    /// has, so a repair record can be matched against it.
    pub(super) field: RawPath,
}

/// Path segments exactly as a rejection named them, for lookups and joins
/// only. A key may be client data, so `Debug` prints the segment count and
/// nothing else, and the segments never reach a rendered surface.
#[derive(Clone, PartialEq, Eq)]
pub(super) struct RawPath(Vec<String>);

impl RawPath {
    fn new<'s>(segments: impl IntoIterator<Item = &'s str>) -> Self {
        Self(segments.into_iter().map(str::to_owned).collect())
    }

    pub(super) fn as_slice(&self) -> &[String] {
        &self.0
    }

    pub(super) fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.iter().map(String::as_str)
    }
}

impl fmt::Debug for RawPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RawPath({} segments)", self.0.len())
    }
}

/// The path a rejection names, whether or not the closed table has a row for
/// it. Reads only; `None` exactly when [`parse_rejection`] could not localize
/// the rejection either.
pub(super) fn rejected_wire_path(
    provider_kind: &str,
    status: u16,
    parsed: Option<&ParsedErrorBody>,
) -> Option<RejectedWirePath> {
    let localized = localize(provider_kind, status, parsed?)?;
    let raw_segments: Vec<&str> = localized.raw.split('.').flat_map(split_brackets).collect();
    Some(RejectedWirePath {
        wire: raw_segments
            .iter()
            .fold(WirePath::root(), |path, name| push_raw_name(path, name)),
        locator: RawPath::new(raw_segments.iter().copied()),
        field: RawPath::new(localized.segments.iter().copied()),
    })
}

/// One raw dotted segment split into its name and trailing bracket indices
/// (`tools[0]` -> `tools`, `0`), empty names dropped. Only called on a path
/// [`canonical_segments`] accepted, so every bracket holds an index.
fn split_brackets(segment: &str) -> impl Iterator<Item = &str> {
    let (name, brackets) = segment.split_once('[').unwrap_or((segment, ""));
    std::iter::once(name)
        .chain(
            brackets
                .split('[')
                .filter_map(|bracket| bracket.strip_suffix(']')),
        )
        .filter(|name| !name.is_empty())
}

fn push_raw_name(path: WirePath, name: &str) -> WirePath {
    match name.parse::<usize>() {
        Ok(index) if is_index(name) => path.index(index),
        _ => path.child(name),
    }
}

/// The one member of `row`'s closed accepted set among `replacements`, or
/// `None` for a row that does not rewrite, replacements naming no member, and
/// replacements naming several.
fn named_accepted(row: &FieldRepairRow, replacements: &[&str]) -> Option<&'static str> {
    let FieldTransform::Rewrite { accepted } = row.transform else {
        return None;
    };
    let mut named = replacements
        .iter()
        .filter_map(|token| accepted.iter().copied().find(|member| member == token));
    let first = named.next()?;
    named.all(|other| other == first).then_some(first)
}

/// The leading run of quoted tokens in `text` joined by a list separator
/// (`'a', 'b' or 'c'`). Text after the last token that does not start with a
/// separator ends the run, so trailing prose -- an echo of the refused input
/// included -- never contributes. A separator not followed by a cleanly quoted
/// member (unterminated, or carrying a `\`) invalidates the whole list: it
/// names nothing.
fn quoted_list(text: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut rest = text;
    loop {
        let Some((token, after)) = leading_quoted(rest) else {
            // A separator promised another member that never came (a
            // truncated `'a' or `), or a member was not cleanly quoted: the
            // list is not a list this parser can trust, so it names nothing.
            return if tokens.is_empty() {
                tokens
            } else {
                Vec::new()
            };
        };
        tokens.push(token);
        match LIST_SEPARATORS
            .iter()
            .find_map(|separator| after.strip_prefix(separator))
        {
            Some(next) => rest = next,
            None => return tokens,
        }
    }
}

/// The `'...'` or `"..."` span `text` starts with, and the text after it. A
/// span carrying a backslash is refused: an escaped delimiter would move the
/// token boundary, and no grounded envelope escapes a closed-set value.
fn leading_quoted(text: &str) -> Option<(&str, &str)> {
    let quote = text.chars().next().filter(|c| matches!(c, '\'' | '"'))?;
    let (token, after) = text[1..].split_once(quote)?;
    (!token.contains('\\')).then_some((token, after))
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
    /// The path as the envelope spelled it, already accepted by
    /// [`canonical_segments`].
    raw: &'a str,
    segments: Vec<&'a str>,
    ends_in_index: bool,
    tier: SignalTier,
    kind: RejectionKind,
    /// The tokens the envelope's template names as replacements.
    replacements: Vec<&'a str>,
}

/// One arm's reading of an envelope: the raw path and what it says about it.
#[derive(Clone, Debug)]
struct Attribution<'a> {
    path: &'a str,
    tier: SignalTier,
    kind: RejectionKind,
    replacements: Vec<&'a str>,
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
        raw: attribution.path,
        segments: canonical_segments(dialect, attribution.path)?,
        ends_in_index: ends_in_index(attribution.path),
        tier: attribution.tier,
        kind: attribution.kind,
        replacements: attribution.replacements,
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
    let replacement = continuation
        .strip_prefix(QUOTED_GROUNDED_CONTINUATION)
        .map(|after| after.split_once('"').map(|(token, _)| token));
    let tier = if replacement.is_some() {
        SignalTier::SelfIdentifying
    } else {
        SignalTier::Inferred
    };
    Some(Attribution {
        path,
        tier,
        kind: RejectionKind::UnsupportedField,
        replacements: replacement.flatten().into_iter().collect(),
    })
}

/// `<dotted.path>: <reason>`, the request-validation location prefix.
fn location_prefix(message: &str) -> Option<Attribution<'_>> {
    let (path, reason) = message.split_once(": ")?;
    let listed = reason.strip_prefix(GROUNDED_VALUE_LIST_PREFIX);
    let grounded = listed.is_some() || reason == LOCATION_UNSUPPORTED_FIELD_REASON;
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
    Some(Attribution {
        path,
        tier,
        kind,
        replacements: listed.map(quoted_list).unwrap_or_default(),
    })
}

/// `error.param` alone names a field without saying the field was refused;
/// only the `unsupported_parameter` code or the exact template naming the
/// same parameter does.
fn localize_openai(parsed: &ParsedErrorBody) -> Option<Attribution<'_>> {
    let message = parsed.error_message();
    let templated = message.and_then(unsupported_parameter);
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
        replacements: Vec::new(),
    })
}

/// `Unsupported parameter: '<x>' is not supported with this model.`
fn unsupported_parameter(message: &str) -> Option<&str> {
    message
        .strip_prefix(OPENAI_UNSUPPORTED_PREFIX)?
        .strip_suffix(OPENAI_UNSUPPORTED_SUFFIX)
}

fn resolve(table: FieldRepairTable, segments: &[&str]) -> Option<&'static FieldRepairRow> {
    table
        .rows()
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
