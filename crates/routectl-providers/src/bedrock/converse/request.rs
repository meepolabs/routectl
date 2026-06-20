//! Canonical -> Bedrock Converse request body translation.
//!
//! Mirrors `crate::anthropic_api::request::normalize` in shape but emits
//! AWS camelCase + the `additionalModelRequestFields` envelope. Reuses
//! the Anthropic-shape primitives (`translate_system`, `translate_tool`,
//! `lift_legacy_system`, `strip_text_after_tool_use`, `build_thinking`)
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
//! -- only per-block `cachePoint` blocks cache. The request normalizer
//! is intentionally lossy for non-Claude vendors: Mistral / Cohere /
//! Llama on Converse won't honor `thinking` or `anthropic_beta` in the
//! bag, but they also won't 400 on it, so a single code path serves
//! both.
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

use routectl_core::cache_control;
use routectl_core::{ChatRequest, Result};

use super::super::BedrockConfig;
use super::extras::build_additional_fields;
use super::messages::build_messages;
use super::system::build_system;
use super::tools::build_tool_config;
use super::types::{ConverseRequest, InferenceConfig};

/// JSON Pointer paths lifted out of the model-specific response bag
/// onto `additionalModelResponseFields`. AWS silently ignores absent
/// pointers, so sending the same set against every Converse-eligible
/// model is safe even when only Anthropic actually populates
/// `/stop_sequence`. Hoisted as a named const so the wire opt-in is
/// reviewable in one place.
const RESPONSE_FIELD_PATHS: &[&str] = &["/stop_sequence"];

// ---------------------------------------------------------------------------
// Top-level translate
// ---------------------------------------------------------------------------

pub fn translate(cfg: &BedrockConfig, req: &ChatRequest) -> Result<ConverseRequest> {
    let inference_config = build_inference_config(req);
    let system = build_system(req);
    let messages = build_messages(&cfg.id, &req.messages)?;
    let tool_config = build_tool_config(&cfg.id, req)?;
    // Reach into the post-translation toolChoice so build_additional_fields
    // can decide whether to strip thinking. Done here (not inside
    // build_additional_fields) so the extras module stays the single
    // source of truth for bag composition while toolChoice translation
    // stays in tools.rs.
    let tool_choice = tool_config.as_ref().and_then(|tc| tc.tool_choice.as_ref());
    let additional_model_request_fields = build_additional_fields(cfg, req, tool_choice);

    validate_breakpoints(req)?;

    Ok(ConverseRequest {
        messages,
        system,
        inference_config,
        tool_config,
        additional_model_request_fields,
        additional_model_response_field_paths: Some(
            RESPONSE_FIELD_PATHS.iter().map(|s| s.to_string()).collect(),
        ),
    })
}

fn build_inference_config(req: &ChatRequest) -> Option<InferenceConfig> {
    // Claude 4.x rejects requests that carry both `temperature` and
    // `top_p`. Emit `top_p` only when no temperature is set; temperature
    // wins.
    let top_p = if req.temperature.is_some() {
        None
    } else {
        req.top_p
    };
    let cfg = InferenceConfig {
        max_tokens: req.max_tokens,
        temperature: req.temperature,
        top_p,
        stop_sequences: req.stop.clone(),
    };
    let any_set = cfg.max_tokens.is_some()
        || cfg.temperature.is_some()
        || cfg.top_p.is_some()
        || cfg.stop_sequences.is_some();
    if any_set {
        Some(cfg)
    } else {
        None
    }
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
#[path = "request_tests_round2.rs"]
mod tests_round2;
