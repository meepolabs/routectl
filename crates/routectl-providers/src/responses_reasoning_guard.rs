//! Leak-guard for OpenAI-Responses-dialect reasoning sub-keys.
//!
//! The Responses ingress stashes `reasoning.{summary,context,mode,...}`
//! under `provider_extras["reasoning"]` so the Responses egress can
//! re-emit them (they have no canonical `ReasoningConfig` home). Every
//! other egress DROPS that remainder as a routectl-managed key -- correct,
//! since Chat-Completions / Anthropic / Gemini have no equivalent knob.
//! Dropping `context` / `mode` is a real fidelity loss, so a non-Responses
//! egress emits ONE structured WARN per request (no field values) when it
//! cannot represent them.

use routectl_core::ChatRequest;

/// Emit a single WARN when the request carries a Responses-dialect
/// `reasoning.context` or `reasoning.mode` that this (non-Responses)
/// egress will drop. `summary` is intentionally excluded -- its loss is a
/// soft downgrade of summary verbosity, not a semantic gap. Logs no field
/// values (only the provider id).
pub fn warn_dropped_reasoning_dialect(provider_id: &str, req: &ChatRequest) {
    let drops = req
        .provider_extras
        .as_ref()
        .and_then(|v| v.as_object())
        .and_then(|o| o.get("reasoning"))
        .and_then(|v| v.as_object())
        .is_some_and(|m| m.contains_key("context") || m.contains_key("mode"));
    if drops {
        tracing::warn!(
            provider = %provider_id,
            "reasoning context/mode dropped: representable only on the OpenAI Responses egress"
        );
    }
}
