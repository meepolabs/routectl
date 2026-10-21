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
//! `Input should be `, the quoted token after `Use `, the type the thinking
//! off-switch template says to send), never from a scan of the whole message: an envelope may also echo the value it refused, and that
//! echo can be a member of the set. A candidate counts only when it equals a
//! member of the row's closed accepted set, and only exactly ONE named member
//! fills it; none or several leave it empty, so a rewrite never picks between
//! candidates.

use std::fmt;

use routectl_core::capability::SignalTier;
use routectl_core::provider_kind::config as kind;
use routectl_core::wire_check::WirePath;
use routectl_core::{ParsedErrorBody, is_safe_token, strip_converse_errors_prefix};

use super::field_repair::{FieldRepairRow, FieldRepairTable, FieldTransform};
use crate::field_verdict_value::canonical_ceiling;

/// Statuses a caller-shaped field rejection arrives on. Everything else says
/// nothing about the request envelope.
pub(super) const REJECTION_STATUSES: [u16; 2] = [400, 422];

/// The Anthropic Messages error type a field rejection carries.
const ANTHROPIC_INVALID_REQUEST: &str = "invalid_request_error";

/// The Converse wrapper object that carries model-specific request fields;
/// a Converse rejection may name a field relative to the request root.
pub(super) const CONVERSE_FIELD_BAG: &str = "additionalModelRequestFields";

/// The location-prefix reasons grounded on captured envelopes are this list
/// lead-in and [`LOCATION_UNSUPPORTED_FIELD_REASON`]. A prefix followed by
/// anything else is still a localization, but only an inferred one.
const GROUNDED_VALUE_LIST_PREFIX: &str = "Input should be ";

const QUOTED_NOT_SUPPORTED: &str = " is not supported";
/// Ends on the opening quote of the replacement it names.
const QUOTED_GROUNDED_CONTINUATION: &str = " for this model. Use \"";

/// The thinking off-switch template,
/// `<lead-in><accepted>"<middle><refused>"<close>`, each type token ended by
/// its closing quote. Prose after the close is free.
const THINKING_SWITCH_LEAD_IN: &str =
    "To turn thinking off on this model, send \"thinking\": {\"type\": \"";
const THINKING_SWITCH_MIDDLE: &str = "} instead of {\"type\": \"";
const THINKING_SWITCH_CLOSE: &str = "}.";

/// A `thinking.type` token the off-switch template may name, and the field
/// path it localizes to.
struct ThinkingType {
    token: &'static str,
    path: &'static str,
}

/// The closed set of thinking types the off-switch template is read for. A
/// token outside it localizes nothing.
static THINKING_TYPES: [ThinkingType; 4] = [
    ThinkingType {
        token: "adaptive",
        path: "thinking.type.adaptive",
    },
    ThinkingType {
        token: "between_tools",
        path: "thinking.type.between_tools",
    },
    ThinkingType {
        token: "disabled",
        path: "thinking.type.disabled",
    },
    ThinkingType {
        token: "enabled",
        path: "thinking.type.enabled",
    },
];

/// Separators between members of an `Input should be` list, longest first so
/// `, or ` is not read as `, ` followed by prose.
const LIST_SEPARATORS: [&str; 3] = [", or ", ", ", " or "];

const OPENAI_UNSUPPORTED_PREFIX: &str = "Unsupported parameter: '";
const OPENAI_UNSUPPORTED_SUFFIX: &str = "' is not supported with this model.";
/// The openai `error.code` that refuses a named parameter.
pub(super) const OPENAI_UNSUPPORTED_CODE: &str = "unsupported_parameter";

/// Bedrock's refusal of the `temperature` field, exactly as captured on
/// Converse. The message names no path, so only this exact text localizes.
const BEDROCK_TEMPERATURE_UNSUPPORTED: &str =
    "This model doesn't support the temperature field. Remove temperature and try again.";
const TEMPERATURE_PATH: &str = "temperature";

/// Bedrock's refusal of an output token limit above the model's, exactly as
/// captured on Converse: `<lead-in>N<middle>N<close>`. The message names no
/// path; the limit is read only from these two positions, and both must
/// spell the same canonical nonzero `u32`.
const BEDROCK_CEILING_LEAD_IN: &str =
    "The maximum tokens you requested exceeds the model limit of ";
const BEDROCK_CEILING_MIDDLE: &str = ". Try again with a maximum tokens value that is lower than ";
const BEDROCK_CEILING_CLOSE: &str = ".";
const MAX_TOKENS_PATH: &str = "max_tokens";

/// The Messages-body refusal of an output token limit above the model's,
/// exactly as captured on anthropic-api and on Bedrock Invoke:
/// `<lead-in>SENT<middle>N<close><model id>`. The model id ends the message
/// and is never read; only a nonempty single token is accepted there.
const MESSAGES_CEILING_LEAD_IN: &str = "max_tokens: ";
const MESSAGES_CEILING_MIDDLE: &str = " > ";
const MESSAGES_CEILING_CLOSE: &str = ", which is the maximum allowed number of output tokens for ";

/// The beta-rejection envelope, `<head><list><tail>`, where the list is
/// backtick-quoted flags joined by the separator.
const BETA_ENVELOPE_HEAD: &str = "Unexpected value(s) ";
const BETA_ENVELOPE_TAIL: &str = " for the `anthropic-beta` header. Please consult our documentation at platform.claude.com/docs or try again without the header.";
const BETA_LIST_SEPARATOR: &str = ", ";
/// The only status a beta rejection arrives on.
const BETA_REJECTION_STATUS: u16 = 400;

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

/// The value a rejection names for its row's field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RejectionValue {
    /// A member of the row's closed accepted set.
    Shape(&'static str),
    /// A token limit the upstream named for the field.
    Ceiling(u32),
}

/// A rejection attributed to one closed-table row.
///
/// Holds no upstream bytes: `row` is the table's own entry, and a `Shape`
/// value is a member of that row's closed accepted set.
#[derive(Clone, Copy, Debug)]
pub(super) struct NamedRejection {
    pub(super) row: &'static FieldRepairRow,
    pub(super) tier: SignalTier,
    pub(super) kind: RejectionKind,
    pub(super) value: Option<RejectionValue>,
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
        value: named_value(row, &localized),
    })
}

/// The value `localized` names for `row`, in the slot `row`'s transform
/// declares: a shape only for a rewrite row, a ceiling only for a ceiling
/// row.
fn named_value(row: &FieldRepairRow, localized: &LocalizedPath<'_>) -> Option<RejectionValue> {
    match row.transform {
        FieldTransform::Ceiling => localized.ceiling.map(RejectionValue::Ceiling),
        FieldTransform::Rewrite { .. } | FieldTransform::Drop => {
            named_accepted(row, &localized.replacements).map(RejectionValue::Shape)
        }
    }
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
    /// Whether the reason refuses something the request carried at the
    /// location: a value outside a listed set, an extra input, an unsupported
    /// parameter. Only a grounded template earns it; any other reason may
    /// name the location because it is missing.
    pub(super) presupposes_presence: bool,
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
        presupposes_presence: localized.presupposes_presence,
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
            kind::ANTHROPIC_API => Some(Self::AnthropicApi),
            kind::BEDROCK => Some(Self::Bedrock),
            kind::OPENAI_COMPAT => Some(Self::OpenAi),
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
    presupposes_presence: bool,
    /// The tokens the envelope's template names as replacements.
    replacements: Vec<&'a str>,
    /// The token limit the envelope's template names.
    ceiling: Option<u32>,
}

/// One arm's reading of an envelope: the raw path and what it says about it.
#[derive(Clone, Debug)]
struct Attribution<'a> {
    path: &'a str,
    tier: SignalTier,
    kind: RejectionKind,
    /// The reason refuses something sent at `path`, so cannot be a report
    /// that `path` is missing.
    presupposes_presence: bool,
    replacements: Vec<&'a str>,
    ceiling: Option<u32>,
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
            localize_bedrock(strip_converse_errors_prefix(bedrock_message(parsed)?))
        }
        Dialect::OpenAi => localize_openai(parsed),
    }?;
    Some(LocalizedPath {
        raw: attribution.path,
        segments: canonical_segments(dialect, attribution.path)?,
        ends_in_index: ends_in_index(attribution.path),
        tier: attribution.tier,
        kind: attribution.kind,
        presupposes_presence: attribution.presupposes_presence,
        replacements: attribution.replacements,
        ceiling: attribution.ceiling,
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

/// The Bedrock-only templates first, then the shared Anthropic arms.
fn localize_bedrock(message: &str) -> Option<Attribution<'_>> {
    unsupported_temperature(message)
        .or_else(|| output_ceiling(message))
        .or_else(|| localize_message(message))
}

/// `This model doesn't support the temperature field. ...`, verbatim.
fn unsupported_temperature(message: &str) -> Option<Attribution<'static>> {
    (message == BEDROCK_TEMPERATURE_UNSUPPORTED).then(|| Attribution {
        path: TEMPERATURE_PATH,
        tier: SignalTier::SelfIdentifying,
        kind: RejectionKind::UnsupportedField,
        presupposes_presence: true,
        replacements: Vec::new(),
        ceiling: None,
    })
}

/// `The maximum tokens you requested exceeds the model limit of N. Try again
/// with a maximum tokens value that is lower than N.`, verbatim, both copies
/// of N agreeing. Whether N is below the value sent is the repair's check:
/// the parser never sees the request.
fn output_ceiling(message: &str) -> Option<Attribution<'static>> {
    let rest = message.strip_prefix(BEDROCK_CEILING_LEAD_IN)?;
    let (first, rest) = rest.split_once(BEDROCK_CEILING_MIDDLE)?;
    let second = rest.strip_suffix(BEDROCK_CEILING_CLOSE)?;
    let ceiling = canonical_ceiling(first)?;
    (canonical_ceiling(second)? == ceiling).then(|| Attribution {
        path: MAX_TOKENS_PATH,
        tier: SignalTier::SelfIdentifying,
        kind: RejectionKind::InvalidValue,
        presupposes_presence: true,
        replacements: Vec::new(),
        ceiling: Some(ceiling),
    })
}

/// `max_tokens: SENT > N, which is the maximum allowed number of output
/// tokens for <model>`, verbatim, both numbers canonical and N below SENT.
/// Unlike the Converse template this one echoes the value sent, so the
/// template itself must not contradict the refusal it states.
fn messages_output_ceiling(message: &str) -> Option<Attribution<'static>> {
    let rest = message.strip_prefix(MESSAGES_CEILING_LEAD_IN)?;
    let (sent, rest) = rest.split_once(MESSAGES_CEILING_MIDDLE)?;
    let (limit, model) = rest.split_once(MESSAGES_CEILING_CLOSE)?;
    let single_token = !model.is_empty() && !model.contains(char::is_whitespace);
    let ceiling = canonical_ceiling(limit)?;
    (single_token && ceiling < canonical_ceiling(sent)?).then(|| Attribution {
        path: MAX_TOKENS_PATH,
        tier: SignalTier::SelfIdentifying,
        kind: RejectionKind::InvalidValue,
        presupposes_presence: true,
        replacements: Vec::new(),
        ceiling: Some(ceiling),
    })
}

fn localize_message(message: &str) -> Option<Attribution<'_>> {
    quoted_not_supported(message)
        .or_else(|| thinking_off_switch(message))
        .or_else(|| messages_output_ceiling(message))
        .or_else(|| location_prefix(message))
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
        presupposes_presence: replacement.is_some(),
        replacements: replacement.flatten().into_iter().collect(),
        ceiling: None,
    })
}

/// `To turn thinking off on this model, send "thinking": {"type": "<A>"}
/// instead of {"type": "<B>"}.` refuses type B and names type A. Both must be
/// in [`THINKING_TYPES`]; the attribution carries only that table's entries.
fn thinking_off_switch(message: &str) -> Option<Attribution<'static>> {
    let rest = message.strip_prefix(THINKING_SWITCH_LEAD_IN)?;
    let (accepted, rest) = rest.split_once('"')?;
    let rest = rest.strip_prefix(THINKING_SWITCH_MIDDLE)?;
    let (refused, rest) = rest.split_once('"')?;
    rest.strip_prefix(THINKING_SWITCH_CLOSE)?;
    Some(Attribution {
        path: thinking_type(refused)?.path,
        tier: SignalTier::SelfIdentifying,
        kind: RejectionKind::UnsupportedField,
        presupposes_presence: true,
        replacements: vec![thinking_type(accepted)?.path],
        ceiling: None,
    })
}

fn thinking_type(token: &str) -> Option<&'static ThinkingType> {
    THINKING_TYPES.iter().find(|known| known.token == token)
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
        presupposes_presence: grounded,
        replacements: listed.map(quoted_list).unwrap_or_default(),
        ceiling: None,
    })
}

/// `error.param` alone names a field without saying the field was refused;
/// only the `unsupported_parameter` code or the exact template naming the
/// same parameter does. A bare `error.param` may also name a parameter the
/// request is missing, so only a refusal presupposes it was sent.
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
        presupposes_presence: unsupported,
        replacements: Vec::new(),
        ceiling: None,
    })
}

/// `Unsupported parameter: '<x>' is not supported with this model.`
fn unsupported_parameter(message: &str) -> Option<&str> {
    message
        .strip_prefix(OPENAI_UNSUPPORTED_PREFIX)?
        .strip_suffix(OPENAI_UNSUPPORTED_SUFFIX)
}

/// The `anthropic-beta` flags a beta rejection names, deduplicated with
/// first-seen order kept, or `None` when the error is not one.
///
/// The envelope lists exactly the rejected flags, backtick-quoted, in request
/// order. Bedrock sends it as the flat `message` of a 400 whose lifted type is
/// `ValidationException`, bare on Invoke and inside one Converse errors
/// wrapper on Converse; anthropic-api (Mantle included) sends it as the
/// `error.message` of a 400 `invalid_request_error`. A never-issued flag
/// draws the same envelope, so the caller must still check each flag against
/// what the request actually sent.
pub(super) fn rejected_beta_flags(
    provider_kind: &str,
    status: u16,
    upstream_type: Option<&str>,
    parsed: Option<&ParsedErrorBody>,
) -> Option<Vec<String>> {
    if status != BETA_REJECTION_STATUS {
        return None;
    }
    let parsed = parsed?;
    let message = match Dialect::of_kind(provider_kind)? {
        Dialect::AnthropicApi => anthropic_message(parsed)?,
        Dialect::Bedrock => {
            let validation = upstream_type.is_some_and(|t| {
                routectl_providers::aws_exception_type_is(
                    t,
                    routectl_providers::VALIDATION_EXCEPTION_TYPE,
                )
            });
            if !validation {
                return None;
            }
            strip_converse_errors_prefix(bedrock_message(parsed)?)
        }
        Dialect::OpenAi => return None,
    };
    let mut flags: Vec<String> = Vec::new();
    for flag in beta_envelope_flags(message)? {
        if !flags.iter().any(|seen| seen == flag) {
            flags.push(flag.to_string());
        }
    }
    Some(flags)
}

/// The tokens an exact beta-rejection envelope lists, in order. `None` when
/// the message deviates from the envelope in any byte, or any listed token
/// is not token-shaped or carries a backtick.
fn beta_envelope_flags(message: &str) -> Option<Vec<&str>> {
    message
        .strip_prefix(BETA_ENVELOPE_HEAD)?
        .strip_suffix(BETA_ENVELOPE_TAIL)?
        .split(BETA_LIST_SEPARATOR)
        .map(|quoted| {
            let token = quoted.strip_prefix('`')?.strip_suffix('`')?;
            (!token.contains('`') && is_safe_token(token)).then_some(token)
        })
        .collect()
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

#[cfg(test)]
#[path = "rejection_parser_beta_tests.rs"]
mod beta_tests;
