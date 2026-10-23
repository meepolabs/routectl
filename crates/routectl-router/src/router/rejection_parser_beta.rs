//! The `anthropic-beta` rejection reader of the rejection parser: the flags
//! one exact beta-rejection envelope lists.

use routectl_core::{ParsedErrorBody, is_safe_token, strip_converse_errors_prefix};

use super::{Dialect, anthropic_message, bedrock_message};

/// The beta-rejection envelope, `<head><list><tail>`, where the list is
/// backtick-quoted flags joined by the separator.
const BETA_ENVELOPE_HEAD: &str = "Unexpected value(s) ";
const BETA_ENVELOPE_TAIL: &str = " for the `anthropic-beta` header. Please consult our documentation at platform.claude.com/docs or try again without the header.";
const BETA_LIST_SEPARATOR: &str = ", ";
/// The only status a beta rejection arrives on.
const BETA_REJECTION_STATUS: u16 = 400;

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
pub(in crate::router) fn rejected_beta_flags(
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
        Dialect::OpenAi | Dialect::OpenAiResponses => return None,
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
