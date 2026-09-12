//! Strips the Claude Code billing/attribution block from the outgoing body.

use serde_json::Value;

/// Marker prefix (after trimming leading whitespace) for the Claude Code
/// billing/attribution system block. Same concept as
/// `system_filter::BILLING_PREFIX`, but applied to the JSON body `Value`
/// rather than canonical `SystemContent`.
pub(super) const BILLING_PREFIX: &str = "x-anthropic-billing-header:";

/// Remove the Claude Code billing/attribution block from `body["system"]`.
///
/// routectl strips the billing block here, so the retained
/// `claude_signing::resign_cch_in_place` re-signer no-ops on the
/// transmitted bytes -- it is kept for a future forward-instead-of-strip
/// toggle.
///
/// This strip records NO policy action, unlike the two on the assembly path
/// that withhold the same class. It is defense in depth over a body the
/// assembly already stripped: `system` is a routectl-managed key, so the
/// forward-compat extras merge cannot reintroduce a block past that strip,
/// and every body reaching here has been through it. Verified rather than
/// argued -- an assertion planted in both arms of this function fires only
/// under unit tests that hand-build a body, never once across the suite's
/// assembled-body paths.
///
/// So a counter here would be a key that never moves, which reads to an
/// operator as "this never happens" when it means "this cannot happen". The
/// assembly-path sites own the class; if this one ever becomes reachable,
/// that is a defect in the strip above it rather than a number to report.
pub(super) fn strip_billing_block(body: &mut Value) {
    match body.get_mut("system") {
        Some(Value::Array(blocks)) => {
            blocks.retain(|b| !block_is_billing(b));
        }
        Some(Value::String(s)) if s.trim_start().starts_with(BILLING_PREFIX) => {
            if let Some(obj) = body.as_object_mut() {
                obj.remove("system");
            }
        }
        _ => {}
    }
}

/// True when a system array element's `"text"` (after trimming leading
/// whitespace) starts with the billing prefix.
fn block_is_billing(block: &Value) -> bool {
    block
        .get("text")
        .and_then(Value::as_str)
        .is_some_and(|t| t.trim_start().starts_with(BILLING_PREFIX))
}
