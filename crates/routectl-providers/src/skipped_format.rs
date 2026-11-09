//! How an egress names the format tag of a reasoning detail it skipped.
//!
//! Every egress that drops foreign-dialect reasoning details reports the
//! distinct tags it saw in one aggregated log field. A recognized tag is
//! closed protocol vocabulary; an unrecognized one is caller-chosen free
//! text. This module is the one place that decides which is which and how
//! each renders under the prompt-redaction knob, so the lanes cannot drift.
//!
//! The Anthropic tag itself is also what the openai-compat and Bedrock
//! lanes stamp on the reasoning details they lift, so the constant is
//! compiled under every lane that reads it while the renderer stays with
//! the two egresses that log skipped tags.

#[cfg(any(feature = "anthropic-api", feature = "openai-responses"))]
use routectl_core::sanitize_for_log;

/// Anthropic wire-format tag for reasoning details.
pub const ANTHROPIC_FORMAT: &str = "anthropic-claude-v1";

/// Stand-in for a format tag outside the recognized vocabulary, rendered
/// when the operator has opted into prompt redaction. A literal, so it
/// carries no caller bytes.
#[cfg(any(feature = "anthropic-api", feature = "openai-responses"))]
const UNRECOGNIZED_FORMAT_PLACEHOLDER: &str = "<unrecognized>";

/// Whether a format tag belongs to the vocabulary routectl knows: the
/// Responses family plus Anthropic's own. Anything else is caller-chosen
/// free text as far as this process is concerned.
#[cfg(any(feature = "anthropic-api", feature = "openai-responses"))]
fn is_known_format_tag(format: Option<&str>) -> bool {
    routectl_core::is_responses_family(format) || format == Some(ANTHROPIC_FORMAT)
}

/// Render one skipped detail's format tag for the aggregated log field.
///
/// A recognized tag is closed-vocabulary and always echoes. An
/// unrecognized one is a caller-chosen free-text string, so it echoes only
/// while prompt redaction is off: under the knob it collapses to
/// [`UNRECOGNIZED_FORMAT_PLACEHOLDER`], because an operator who opted into
/// redaction asked for caller content to stay out of the logs and this
/// field is the one channel that would otherwise carry it. Flipping the
/// knob off restores the literal for forward-compat discovery.
#[cfg(any(feature = "anthropic-api", feature = "openai-responses"))]
pub fn render_skipped_format(format: Option<&str>, redact: bool) -> String {
    match format {
        None => sanitize_for_log("<none>"),
        Some(tag) if redact && !is_known_format_tag(Some(tag)) => {
            UNRECOGNIZED_FORMAT_PLACEHOLDER.to_string()
        }
        Some(tag) => sanitize_for_log(tag),
    }
}
