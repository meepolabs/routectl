//! Canonical -> OpenAI Responses API request body translation.
//!
//! Orchestrator pattern (mirrors `bedrock::converse::request::translate`):
//! call into the per-concern sub-modules in a deterministic order and
//! assemble the final `ResponsesRequest`. Each sub-module owns one
//! field family: `system.rs` -> `instructions`, `messages.rs` ->
//! `input`, `tools.rs` -> `tools`/`tool_choice`, `extras.rs` ->
//! `reasoning` + the 5-key provider_extras allowlist.
//!
//! `store` defaults to `false` here; the `extras` module flips it for
//! non-ChatgptOauth auth_kinds when the operator opts in via
//! `provider_extras["store"]`. `parallel_tool_calls` defaults to
//! `true`, matching codex's `ResponsesApiRequest` default.

use routectl_core::cache_control::{BreakpointPosition, CacheBreakpointSource};
use routectl_core::{ChatRequest, Result};

use super::messages::build_input;
use super::system::translate_system;
use super::tools::{translate_tool_choice, translate_tools};
use super::types::ResponsesRequest;
use super::{OpenAiResponsesConfig, extras};

/// Build a fully-populated `ResponsesRequest` from a routectl
/// `ChatRequest`. The Provider's `complete()` toggles `stream` to
/// false / `stream()` toggles it true; this orchestrator builds with
/// `stream = false` and the caller flips as needed.
pub fn translate(cfg: &OpenAiResponsesConfig, req: &ChatRequest) -> Result<ResponsesRequest> {
    warn_dropped_cache_control(req);

    let instructions = translate_system(req).unwrap_or_default();
    let input = build_input(&cfg.id, cfg.auth_kind, &req.messages)?;
    let tools = translate_tools(req);
    let tool_choice = translate_tool_choice(req.tool_choice.as_ref());

    let mut request = ResponsesRequest {
        model: req.model.clone(),
        instructions,
        input,
        tools,
        tool_choice,
        parallel_tool_calls: true,
        reasoning: None,
        // ChatgptOauth always sends store=false. Other auth_kinds may
        // override via provider_extras["store"] (handled in extras.rs).
        store: false,
        stream: false,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        text: None,
        client_metadata: None,
    };

    extras::apply_reasoning(&mut request, req);
    extras::merge_provider_extras(&mut request, req, cfg.auth_kind);
    // Runs last: the encrypted-reasoning include depends on the final
    // `store` value (which merge_provider_extras may have flipped) and
    // on whether the operator pinned `include` explicitly.
    extras::finalize_reasoning_include(&mut request, req);

    Ok(request)
}

/// Cache-prefix surfaces (other than `system`) that carry a caller
/// `cache_control` marker the Responses egress will drop. The Responses
/// API has no prompt-caching breakpoint surface, so dropping the markers
/// is correct -- this only names which surfaces carried one.
///
/// `system` is excluded on purpose: `system.rs` already logs that drop at
/// DEBUG, so re-reporting it here would double-log the same surface.
/// Pure function of `req` -- no logging, no mutation -- so the detection
/// can be unit-tested directly.
fn dropped_cache_surfaces(req: &ChatRequest) -> Vec<&'static str> {
    let mut surfaces: Vec<&'static str> = Vec::new();
    for bp in req.cache_breakpoints() {
        let name = match bp.position {
            BreakpointPosition::Tools => "tools",
            BreakpointPosition::Messages => "messages",
            BreakpointPosition::TopLevel => "top-level",
            // Already logged at DEBUG in system.rs; skip to avoid a double-log.
            BreakpointPosition::System => continue,
        };
        if !surfaces.contains(&name) {
            surfaces.push(name);
        }
    }
    surfaces
}

/// Emit one WARN naming every cache-prefix surface carrying a caller
/// `cache_control` marker that the Responses egress drops. Matches the
/// openai-compat egress convention (`check_dropped_anthropic_fields`),
/// which WARNs on every dropped cache_control carrier so an operator
/// routing cache-hinted traffic to a Responses target sees the same
/// breadcrumb. Logs only the surface name(s) + a count: no message
/// content, no bodies, no secrets.
fn warn_dropped_cache_control(req: &ChatRequest) {
    let surfaces = dropped_cache_surfaces(req);
    if surfaces.is_empty() {
        return;
    }
    tracing::warn!(
        dropped_surfaces = ?surfaces,
        dropped_count = surfaces.len(),
        "openai-responses egress: cache_control dropped (Responses API has no \
         prompt-cache breakpoint surface)"
    );
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
