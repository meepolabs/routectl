//! Canonical -> Bedrock Converse request body translation.
//!
//! Mirrors `crate::anthropic_api::request::normalize` in shape but emits
//! AWS camelCase + the `additionalModelRequestFields` envelope. Reuses
//! the Anthropic-shape primitives (`translate_system`, `translate_tool`,
//! `lift_legacy_system`, `strip_text_after_tool_use`, `compose_thinking`)
//! where the canonical-side mapping is identical, then adapts to AWS
//! naming.
//!
//! The big "bag" for model-specific extras:
//! `additional_model_request_fields`. AWS forwards this verbatim to the
//! underlying model, so for Claude on Converse we route `thinking`,
//! `anthropic_beta`, `output_config`, and any operator-supplied
//! `additional_model_request_fields` through it here. A top-level
//! `cache_control` marker is also forwarded inert into this bag (with a
//! WARN), since Converse does not honor a top-level marker for caching
//! -- only per-block `cachePoint` blocks cache. Non-Claude models on
//! Converse do not ignore these Anthropic keys: they refuse the request
//! (GPT models with `Unknown parameter: 'anthropic_beta'`, Nova and
//! Llama with `extraneous key [anthropic_beta] is not permitted`, and
//! the same for `output_config`). The router keeps client betas off
//! such a target by withholding every one of them; the operator's beta
//! floor still ships. The bag assembly omits `output_config` for such a
//! target and counts the loss.
//!
//! cache_control breakpoints don't translate to per-block markers on
//! the Converse wire -- AWS uses inline `cachePoint` blocks instead.
//! When a canonical block carries a cache_control marker, the Converse
//! emit sequence is:
//!
//!   - the original block (text / image / tool_use / tool_result / ...)
//!   - immediately followed by `{cachePoint: {type: "default", ttl?}}`
//!
//! mapping the cache_control TTL onto the cachePoint TTL field.
//!
//! The orchestrator runs `cache_control::validate` against the
//! collected breakpoints (tools / system / messages / top-level)
//! before serialization, mirroring the Anthropic egress so a
//! mis-shaped cache prefix surfaces as a clean 400 locally rather than
//! a vague AWS error.

use serde_json::Value;

use routectl_core::cache_control;
use routectl_core::{ChatRequest, Result};

use crate::anthropic_api::request::clamp_sampling_for_thinking;

use super::super::BedrockConfig;
use super::extras::build_additional_fields;
use super::messages::build_messages;
use super::system::{build_system, count_dropped_system_parts};
use super::tools::build_tool_config;
use super::types::{ConverseRequest, InferenceConfig};

/// JSON Pointer paths lifted out of the model-specific response bag
/// onto `additionalModelResponseFields`. AWS silently ignores absent
/// pointers, so sending the same set against every Converse-eligible
/// model is safe even when only Anthropic actually populates
/// `/stop_sequence`. Hoisted as a named const so the wire opt-in is
/// reviewable in one place.
const RESPONSE_FIELD_PATHS: &[&str] = &["/stop_sequence"];

// This lane withholds the fingerprint on three surfaces sharing one tally: the
// top-level `system` field, the Role::System message lift, and the `metadata`
// entry of the forward-compat extras bag. `merge_system_sources` concatenates
// rather than picks, so a caller supplying both system sources trips two sites
// on one request.
//
// Reachability: through the OpenAI and Responses ingresses the Role::System
// lift site sees only a system turn kept in place for its non-text parts; they
// promote every other one into `system` at parse time. It is fully live through
// the Anthropic ingress, which forwards them unlifted, and for a direct library
// caller.
pub(super) use crate::translation_drop_metrics::ClientFingerprintStripTally;

/// Count this request's fingerprint withhold on the converse lane. Production
/// code reaches it only through [`translate`], which owns the per-request
/// lifetime; a test exercising ONE strip site in isolation calls it directly.
pub(super) fn flush_fingerprint_tally(fingerprint: &ClientFingerprintStripTally) {
    if fingerprint.stripped() {
        crate::translation_drop_metrics::record_translation_policy_action(
            super::LANE,
            "client_fingerprint_stripped",
        );
    }
}

// ---------------------------------------------------------------------------
// Top-level translate
// ---------------------------------------------------------------------------

pub fn translate(cfg: &BedrockConfig, req: &ChatRequest) -> Result<ConverseRequest> {
    let mut fingerprint = ClientFingerprintStripTally::default();
    let translated = translate_tallied(cfg, req, &mut fingerprint);
    // Flushed on BOTH arms and outside every fallible step of the translation:
    // a request whose entire system IS the fingerprint block translates to no
    // system at all, and one that withholds a fingerprint and only then fails
    // to translate its messages was still counted by this lane's denominator.
    // Either shape missing from the numerator reads the action rate low for
    // precisely the requests that withhold the most.
    flush_fingerprint_tally(&fingerprint);
    translated
}

fn translate_tallied(
    cfg: &BedrockConfig,
    req: &ChatRequest,
    fingerprint: &mut ClientFingerprintStripTally,
) -> Result<ConverseRequest> {
    // The canonical sampling knobs have no `inferenceConfig` home and are
    // gated out of the additionalModelRequestFields merge as canonical keys;
    // log once at DEBUG (an expected transform).
    crate::sampling_drop_guard::log_dropped_sampling_fields(&cfg.id, req, &[]);
    count_dropped_system_parts(req);
    let system = build_system(req, fingerprint);
    let messages = build_messages(&cfg.id, &req.messages)?;
    let tool_config = build_tool_config(&cfg.id, req, &messages)?;
    // Reach into the post-translation toolChoice so build_additional_fields
    // can decide whether to strip thinking. Done here (not inside
    // build_additional_fields) so the extras module stays the single
    // source of truth for bag composition while toolChoice translation
    // stays in tools.rs.
    let tool_choice = tool_config.as_ref().and_then(|tc| tc.tool_choice.as_ref());
    let mut additional_model_request_fields =
        build_additional_fields(cfg, req, tool_choice, fingerprint);
    repair_output_schema(cfg, additional_model_request_fields.as_mut())?;

    // The sampling clamp reads the thinking type the final bag carries:
    // build_additional_fields strips active thinking when toolChoice forces
    // tool use and keeps an inactive one, and the type it ships may be one
    // the canonical reasoning cannot express.
    let thinking_type = additional_model_request_fields
        .as_ref()
        .and_then(|bag| bag.pointer("/thinking/type"))
        .and_then(Value::as_str);
    let inference_config = build_inference_config(req, thinking_type);

    validate_breakpoints(req)?;

    Ok(ConverseRequest {
        messages,
        system,
        inference_config,
        tool_config,
        additional_model_request_fields,
        additional_model_response_field_paths: Some(
            RESPONSE_FIELD_PATHS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        ),
    })
}

/// Supply the `additionalProperties: false` Anthropic requires on every object
/// of `output_config.format.schema`; Bedrock forwards the bag verbatim and does
/// not fill it in. Runs on the FINAL bag so it sees every `output_config`
/// writer. A schema past the walk's bounds fails the request here, before any send.
fn repair_output_schema(cfg: &BedrockConfig, bag: Option<&mut Value>) -> Result<()> {
    let Some(bag) = bag.and_then(Value::as_object_mut) else {
        return Ok(());
    };
    crate::anthropic_api::output_schema::inject_additional_properties_false(&cfg.id, bag)?
        .warn(&cfg.id);
    Ok(())
}

fn build_inference_config(
    req: &ChatRequest,
    thinking_type: Option<&str>,
) -> Option<InferenceConfig> {
    // Clamp sampling for thinking mode via the shared Anthropic-API helper:
    // Claude accepts only temperature=1.0 (and no top_p) while thinking, and
    // rejects a temperature+top_p pair otherwise. The Anthropic-API and
    // Bedrock-Invoke seams apply the identical clamp; Converse builds its
    // inferenceConfig independently, so it must call the same helper or the
    // clamp drifts.
    //
    // `thinking_type` is the type the final bag ships, `None` when no thinking
    // survived; a stripped or inactive type restores the caller's sampling,
    // matching the direct Anthropic path, which calls the same helper.
    let (temperature, top_p) =
        clamp_sampling_for_thinking(thinking_type, req.temperature, req.top_p);
    let cfg_inference = InferenceConfig {
        max_tokens: req.max_tokens,
        temperature,
        top_p,
        stop_sequences: req.stop.clone(),
    };
    let any_set = cfg_inference.max_tokens.is_some()
        || cfg_inference.temperature.is_some()
        || cfg_inference.top_p.is_some()
        || cfg_inference.stop_sequences.is_some();
    if any_set { Some(cfg_inference) } else { None }
}

// ---------------------------------------------------------------------------
// cache_control validation
// ---------------------------------------------------------------------------

/// Walk every position of the canonical request and run
/// `cache_control::validate` over the collected breakpoint sequence.
/// Delegates to the shared `CacheBreakpointSource` walk on
/// `ChatRequest` so the 4-breakpoint cap and 1h-after-5m TTL ordering
/// are caught locally before the body reaches AWS. The cachePoint
/// translation in `messages.rs` / `tools.rs` happens after this.
fn validate_breakpoints(req: &ChatRequest) -> Result<()> {
    cache_control::validate_source(req)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "request_tests_field_translation.rs"]
mod tests_field_translation;

#[cfg(test)]
#[path = "request_tests_parity.rs"]
mod tests_parity;

#[cfg(test)]
#[path = "request_tests_output_schema.rs"]
mod tests_output_schema;

#[cfg(test)]
#[path = "request_thinking_carrier_tests.rs"]
mod tests_thinking_carrier;

#[cfg(test)]
#[path = "request_client_output_format_tests.rs"]
mod tests_client_output_format;
