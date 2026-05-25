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
//! underlying model, so for Claude on Converse we drop `thinking`,
//! `anthropic_beta`, top-level `cache_control`, `output_config`, and any
//! operator-supplied `additional_model_request_fields` here. The
//! request normalizer is intentionally lossy for non-Claude vendors:
//! Mistral / Cohere / Llama on Converse won't honor `thinking` or
//! `anthropic_beta` in the bag, but they also won't 400 on it, so a
//! single code path serves both.
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

use routectl_core::cache_control::{self, Breakpoint, BreakpointPosition};
use routectl_core::{ChatRequest, MessageContent, Result};

use super::super::BedrockConfig;
use super::extras::build_additional_fields;
use super::messages::build_messages;
use super::system::build_system;
use super::tools::{build_tool_config, collect_tool_cache_controls};
use super::types::{ConverseRequest, InferenceConfig};

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
        additional_model_response_field_paths: None,
    })
}

fn build_inference_config(req: &ChatRequest) -> Option<InferenceConfig> {
    let cfg = InferenceConfig {
        max_tokens: req.max_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
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
/// Mirrors `anthropic_api::request::validate_breakpoints` so the
/// 4-breakpoint cap and 1h-after-5m TTL ordering are caught locally
/// before the body reaches AWS. The message-side iteration is
/// canonical-shape (ContentPart.cache_control) -- the cachePoint
/// translation in `messages.rs` happens after this validation.
fn validate_breakpoints(req: &ChatRequest) -> Result<()> {
    let tool_ccs = collect_tool_cache_controls(req);
    let mut bps: Vec<Breakpoint<'_>> = Vec::new();

    // Tools come first in the cache prefix.
    for cc in &tool_ccs {
        bps.push(Breakpoint {
            position: BreakpointPosition::Tools,
            control: cc,
        });
    }

    // Then system blocks (per-block cache_control on
    // `SystemContent::Blocks`).
    if let Some(routectl_core::SystemContent::Blocks(blocks)) = req.system.as_ref() {
        for b in blocks {
            if let Some(cc) = b.cache_control.as_ref() {
                bps.push(Breakpoint {
                    position: BreakpointPosition::System,
                    control: cc,
                });
            }
        }
    }

    // Then messages: each typed ContentPart may carry cache_control.
    for m in &req.messages {
        if let MessageContent::Parts(parts) = &m.content {
            for p in parts {
                if let Some(cc) = p.cache_control() {
                    bps.push(Breakpoint {
                        position: BreakpointPosition::Messages,
                        control: cc,
                    });
                }
            }
        }
    }

    // Top-level auto-cache marker.
    if let Some(cc) = req.cache_control.as_ref() {
        bps.push(Breakpoint {
            position: BreakpointPosition::TopLevel,
            control: cc,
        });
    }

    cache_control::validate(&bps)
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
