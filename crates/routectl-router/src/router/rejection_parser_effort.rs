//! The value-list reader and the effort refusal templates of the rejection
//! parser: the `Supported levels` refusal, the serde unknown-variant
//! refusal, and the Responses `reasoning.effort` value refusals, each read
//! onto the one canonical effort field with the refused level it echoes and
//! the levels it lists.

use routectl_core::ParsedErrorBody;
use routectl_core::capability::SignalTier;
use routectl_providers::effort::VALID_EFFORT_TOKENS;

use super::{Attribution, RejectionKind, is_name, is_name_byte, localize_openai};

/// The field an effort level travels in on the Messages body.
const EFFORT_PATH: &str = "output_config.effort";

/// The `error.param` the Responses API names an effort level refusal by, and
/// the wire path that level travels in.
const RESPONSES_EFFORT_PARAM: &str = "reasoning.effort";

/// Separators between members of a value list, longest first so `, or ` and
/// `, and ` are not read as `, ` followed by prose.
const LIST_SEPARATORS: [&str; 4] = [", and ", ", or ", ", ", " or "];

/// The effort-level refusal, exactly as captured on anthropic-api and on
/// Bedrock Invoke and Converse: `<lead-in>'<refused>'<middle><levels><close>`,
/// the levels a bare list. The message names no path; only this exact text
/// localizes.
const EFFORT_LEVEL_LEAD_IN: &str = "This model does not support effort level '";
const EFFORT_LEVEL_MIDDLE: &str = "'. Supported levels: ";
const EFFORT_LEVEL_CLOSE: &str = ".";

/// The serde enum rejection Bedrock returns for an unknown effort variant:
/// `<lead-in><refused>`<middle><variants>`, each variant backtick-quoted.
/// Generic enum text: it names no field, so the effort path it implies is
/// only inferred, and only a list of effort levels implies it.
const UNKNOWN_VARIANT_LEAD_IN: &str = "unknown variant ";
const UNKNOWN_VARIANT_MIDDLE: &str = ", expected one of ";
/// The catch-all variant the effort enum's serde list ends with; it is not a
/// level, and the only member allowed outside the effort rank.
const SERDE_CATCH_ALL_VARIANT: &str = "Unhandled";

/// The Responses refusal of a value outside the effort enum, exactly as
/// captured: `<lead-in><refused>'<close><values>.`, the values a quoted list.
const RESPONSES_INVALID_VALUE_LEAD_IN: &str = "Invalid value: '";
const RESPONSES_INVALID_VALUE_CLOSE: &str = "'. ";
/// The Responses refusal of an enum value the model does not take, exactly as
/// captured: `<lead-in><refused><middle><model><close><values>.`. The model
/// id is never read; only a nonempty single token is accepted there.
const RESPONSES_UNSUPPORTED_VALUE_LEAD_IN: &str = "Unsupported value: '";
const RESPONSES_UNSUPPORTED_VALUE_MIDDLE: &str = "' is not supported with the '";
const RESPONSES_UNSUPPORTED_VALUE_CLOSE: &str = "' model. ";
/// The lead-in of the value list both Responses value refusals end with.
const RESPONSES_SUPPORTED_VALUES: &str = "Supported values are: ";
const RESPONSES_SUPPORTED_VALUES_CLOSE: &str = ".";

/// How the members of a value list are spelled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ListMembers {
    /// Each member delimited by `'`, `"` or a backtick.
    Quoted,
    /// Each member a bare run of name bytes (`high, low, medium.`).
    Bare,
}

impl ListMembers {
    fn leading(self, text: &str) -> Option<(&str, &str)> {
        match self {
            Self::Quoted => leading_quoted(text),
            Self::Bare => leading_bare(text),
        }
    }
}

/// The leading quoted list in `text`; see [`list_members`].
///
/// The location-prefix `Input should be` list reads through here, so it
/// takes the effort lists' backtick quoting and `, and ` separator too. That
/// widening is accepted: a listed member only ever fills a rewrite when it
/// equals a member of the row's closed accepted set.
pub(super) fn quoted_list(text: &str) -> Vec<&str> {
    list_members(text, ListMembers::Quoted)
}

/// The leading run of members in `text` joined by a list separator
/// (`'a', 'b' or 'c'`, `a, b, and c`). Text after the last member that does
/// not start with a separator ends the run, so trailing prose (an echo of
/// the refused input, a closing period, a parser position tail) never
/// contributes. A separator not followed by a cleanly spelled member
/// (unterminated, or carrying a `\`) invalidates the whole list: it names
/// nothing.
pub(super) fn list_members(text: &str, members: ListMembers) -> Vec<&str> {
    list_and_rest(text, members).map_or_else(Vec::new, |(tokens, _)| tokens)
}

/// The members [`list_members`] reads and the text after the last one, or
/// `None` when the list names nothing.
fn list_and_rest(text: &str, members: ListMembers) -> Option<(Vec<&str>, &str)> {
    let mut tokens = Vec::new();
    let mut rest = text;
    loop {
        // No member here: `text` starts with none, a separator promised
        // another member that never came (a truncated `'a' or `), or a
        // member was not cleanly spelled. Either way the list names nothing.
        let (token, after) = members.leading(rest)?;
        tokens.push(token);
        match LIST_SEPARATORS
            .iter()
            .find_map(|separator| after.strip_prefix(separator))
        {
            Some(next) => rest = next,
            None => return Some((tokens, after)),
        }
    }
}

/// The `'...'`, `"..."` or `` `...` `` span `text` starts with, and the text
/// after it. A span carrying a backslash is refused: an escaped delimiter
/// would move the token boundary, and no grounded envelope escapes a
/// closed-set value.
fn leading_quoted(text: &str) -> Option<(&str, &str)> {
    let quote = text
        .chars()
        .next()
        .filter(|c| matches!(c, '\'' | '"' | '`'))?;
    let (token, after) = text[1..].split_once(quote)?;
    (!token.contains('\\')).then_some((token, after))
}

/// The nonempty run of name bytes `text` starts with, and the text after it.
fn leading_bare(text: &str) -> Option<(&str, &str)> {
    let end = text
        .bytes()
        .position(|b| !is_name_byte(b))
        .unwrap_or(text.len());
    (end > 0).then(|| text.split_at(end))
}

/// The members of `tokens` in the effort rank as a bitmask: bit `i` set
/// when `tokens` names member `i` of [`VALID_EFFORT_TOKENS`]. Tokens outside
/// the rank (`none`, a serde catch-all variant, an unknown level) set
/// nothing, so a list of only such tokens is `0`.
pub(super) fn levels_mask(tokens: &[&str]) -> u8 {
    tokens
        .iter()
        .filter_map(|token| VALID_EFFORT_TOKENS.iter().position(|rank| rank == token))
        .fold(0, |mask, index| mask | (1 << index))
}

const _: () = assert!(VALID_EFFORT_TOKENS.len() <= u8::BITS as usize);

/// The [`levels_mask`] of `replacements` when `segments` is the canonical
/// effort field; `0` for any other field, whatever its list names.
pub(super) fn effort_field_levels(segments: &[&str], replacements: &[&str]) -> u8 {
    if segments.iter().copied().eq(EFFORT_PATH.split('.')) {
        levels_mask(replacements)
    } else {
        0
    }
}

/// The Responses spelling of the effort field read as the Messages one, so
/// every dialect names the effort field one way; any other path unchanged.
pub(super) fn responses_effort_field(segments: Vec<&str>) -> Vec<&str> {
    if segments
        .iter()
        .copied()
        .eq(RESPONSES_EFFORT_PARAM.split('.'))
    {
        EFFORT_PATH.split('.').collect()
    } else {
        segments
    }
}

/// `This model does not support effort level '<x>'. Supported levels: a, b,
/// c.`, verbatim: the levels a nonempty bare list ended by the closing period
/// and nothing after it.
pub(super) fn unsupported_effort_level(message: &str) -> Option<Attribution<'_>> {
    let rest = message.strip_prefix(EFFORT_LEVEL_LEAD_IN)?;
    let (echoed, rest) = rest.split_once(EFFORT_LEVEL_MIDDLE)?;
    let (levels, close) = list_and_rest(rest, ListMembers::Bare)?;
    (is_name(echoed) && close == EFFORT_LEVEL_CLOSE).then_some(Attribution {
        path: EFFORT_PATH,
        tier: SignalTier::SelfIdentifying,
        kind: RejectionKind::InvalidValue,
        presupposes_presence: true,
        replacements: levels,
        ceiling: None,
        echoed: Some(echoed),
    })
}

/// ``unknown variant `<x>`, expected one of `a`, `b` ...``. The text after the
/// variant list (a parser position) is never read. Only a list of effort
/// levels, the catch-all variant aside, is the effort enum's; a list with any
/// other member, or with no level at all, is another enum's and localizes
/// nothing.
pub(super) fn unknown_effort_variant(message: &str) -> Option<Attribution<'_>> {
    let rest = message.strip_prefix(UNKNOWN_VARIANT_LEAD_IN)?;
    let (echoed, rest) = rest.strip_prefix('`')?.split_once('`')?;
    let rest = rest.strip_prefix(UNKNOWN_VARIANT_MIDDLE)?;
    let variants = list_members(rest, ListMembers::Quoted);
    let only_levels = variants.iter().all(|variant| {
        *variant == SERDE_CATCH_ALL_VARIANT || VALID_EFFORT_TOKENS.contains(variant)
    });
    (is_name(echoed) && only_levels && levels_mask(&variants) != 0).then_some(Attribution {
        path: EFFORT_PATH,
        tier: SignalTier::Inferred,
        kind: RejectionKind::InvalidValue,
        presupposes_presence: false,
        replacements: variants,
        ceiling: None,
        echoed: Some(echoed),
    })
}

/// The OpenAI envelope reading of a refusal whose `error.param` names the
/// effort field, plus the refused level and the listed levels when the
/// message is one of the captured value refusals. Any other `error.param`,
/// or none, localizes nothing on this kind: a parameter routectl itself put
/// on the wire (an operator payload extra) must not read as a refusal the
/// upstream attributed to the client.
pub(super) fn localize_responses(parsed: &ParsedErrorBody) -> Option<Attribution<'_>> {
    if parsed.error_param()? != RESPONSES_EFFORT_PARAM {
        return None;
    }
    let attribution = localize_openai(parsed)?;
    let Some((echoed, levels)) = parsed.error_message().and_then(responses_value_refusal) else {
        return Some(attribution);
    };
    Some(Attribution {
        presupposes_presence: true,
        replacements: levels,
        echoed: Some(echoed),
        ..attribution
    })
}

/// `Invalid value: '<x>'. Supported values are: ...` or `Unsupported value:
/// '<x>' is not supported with the '<model>' model. Supported values are:
/// ...`, verbatim: the refused value a name and the values a nonempty quoted
/// list ended by the closing period and nothing after it.
fn responses_value_refusal(message: &str) -> Option<(&str, Vec<&str>)> {
    let (echoed, rest) = invalid_value_head(message).or_else(|| unsupported_value_head(message))?;
    let rest = rest.strip_prefix(RESPONSES_SUPPORTED_VALUES)?;
    let (values, close) = list_and_rest(rest, ListMembers::Quoted)?;
    (is_name(echoed) && close == RESPONSES_SUPPORTED_VALUES_CLOSE).then_some((echoed, values))
}

fn invalid_value_head(message: &str) -> Option<(&str, &str)> {
    message
        .strip_prefix(RESPONSES_INVALID_VALUE_LEAD_IN)?
        .split_once(RESPONSES_INVALID_VALUE_CLOSE)
}

fn unsupported_value_head(message: &str) -> Option<(&str, &str)> {
    let rest = message.strip_prefix(RESPONSES_UNSUPPORTED_VALUE_LEAD_IN)?;
    let (echoed, rest) = rest.split_once(RESPONSES_UNSUPPORTED_VALUE_MIDDLE)?;
    let (model, rest) = rest.split_once(RESPONSES_UNSUPPORTED_VALUE_CLOSE)?;
    let single_token =
        !model.is_empty() && !model.contains(|c: char| c.is_whitespace() || c == '\'');
    single_token.then_some((echoed, rest))
}
