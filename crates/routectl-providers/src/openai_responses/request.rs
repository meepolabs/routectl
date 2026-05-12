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

use routectl_core::{ChatRequest, Result};

use super::messages::build_input;
use super::system::translate_system;
use super::tools::{translate_tool_choice, translate_tools};
use super::types::ResponsesRequest;
use super::{extras, OpenAiResponsesConfig};

/// Build a fully-populated `ResponsesRequest` from a routectl
/// `ChatRequest`. The Provider's `complete()` toggles `stream` to
/// false / `stream()` toggles it true; this orchestrator builds with
/// `stream = false` and the caller flips as needed.
pub(crate) fn translate(
    cfg: &OpenAiResponsesConfig,
    req: &ChatRequest,
) -> Result<ResponsesRequest> {
    let instructions = translate_system(req).unwrap_or_default();
    let input = build_input(&cfg.id, &req.messages)?;
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

    Ok(request)
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
