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
use routectl_core::{ChatRequest, ResponsesPassthroughItem, Result};

use super::messages::build_input;
use super::system::translate_system;
use super::tools::{translate_tool_choice, translate_tools};
use super::types::{ResponseInput, ResponsesRequest};
use super::{AuthKind, OpenAiResponsesConfig, extras};
use crate::translation_drop_metrics::{record_translation_drop, record_translation_lane_seen};

/// Build a fully-populated `ResponsesRequest` from a routectl
/// `ChatRequest`. The Provider's `complete()` toggles `stream` to
/// false / `stream()` toggles it true; this orchestrator builds with
/// `stream = false` and the caller flips as needed.
pub fn translate(cfg: &OpenAiResponsesConfig, req: &ChatRequest) -> Result<ResponsesRequest> {
    // The ONE per-lane denominator site for `openai-responses`. Placed at the
    // top of the orchestrator rather than inside a sub-module: this is the
    // only function every request on this lane passes through exactly once,
    // and it runs before the first `?` so a request that FAILS translation
    // still counts toward the lane's volume -- a drop rate whose denominator
    // omitted the failures would read low for exactly the requests that went
    // worst.
    record_translation_lane_seen("openai-responses");
    warn_dropped_cache_control(req);
    // The canonical sampling knobs have no Responses-API home and are gated
    // out of the provider_extras merge as canonical keys; WARN once so the
    // loss isn't silent.
    crate::sampling_drop_guard::warn_dropped_sampling_fields(&cfg.id, req, &[]);

    let instructions = translate_system(req).unwrap_or_default();
    let input = build_input_with_passthrough(&cfg.id, cfg.auth_kind, req)?;
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
        // Codex's `ResponsesApiRequest` has no `max_output_tokens` member,
        // and chatgpt.com's codex backend rejects fields the client never
        // sends. Other auth_kinds (ApiKey, BedrockMantle) accept it as a
        // documented top-level field, so forward the caller's ceiling there.
        max_output_tokens: match cfg.auth_kind {
            AuthKind::ChatgptOauth => None,
            _ => req.max_tokens,
        },
    };

    extras::apply_reasoning(&mut request, req);
    extras::merge_provider_extras(&mut request, req, cfg.auth_kind);
    // Runs after merge_provider_extras so the stamp lands on the merged
    // object; the resolved id wins over any request-carried value.
    extras::apply_installation_id(&mut request, cfg.auth_kind, cfg.installation_id.as_deref());
    // Honor the canonical structured-output directive onto `text.format`.
    // Runs after merge_provider_extras so a `verbosity` sibling lifted into
    // provider_extras["text"] survives and the format merges alongside it.
    extras::apply_response_format(&mut request, req);
    // Runs last: the encrypted-reasoning include depends on the final
    // `store` value (which merge_provider_extras may have flipped) and
    // on whether the operator pinned `include` explicitly.
    extras::finalize_reasoning_include(&mut request, req);

    Ok(request)
}

/// Build the `input[]` array: the modeled items translated from the
/// canonical `messages[]`, with any Responses item kinds this hub does
/// not model spliced back into their original inbound positions from
/// `req.routectl_internal.responses_input_passthrough`.
///
/// The passthrough items are the Responses ingress's capture of
/// codex-only kinds (`local_shell_call`, `custom_tool_call(_output)`,
/// `tool_search_call`, `agent_message`, ...) that no canonical field
/// models; replaying them keeps a codex multi-turn conversation whole
/// instead of dropping them. Each carries a `modeled_prefix` (the count
/// of modeled INPUT items that preceded it inbound), so it is re-emitted
/// after that many modeled EGRESS items -- preserving original source
/// order rather than appending everything to the tail. Preserve-and-
/// passthrough only: the raw JSON is forwarded unchanged, no cross-
/// dialect translation. Empty for every request that did not enter
/// through the Responses ingress.
fn build_input_with_passthrough(
    id: &str,
    auth_kind: AuthKind,
    req: &ChatRequest,
) -> Result<Vec<ResponseInput>> {
    let items = build_input(id, auth_kind, &req.messages)?;
    let modeled: Vec<ResponseInput> = items.into_iter().map(ResponseInput::Item).collect();
    Ok(merge_passthrough(
        modeled,
        &req.routectl_internal.responses_input_passthrough,
    ))
}

/// Splice preserved passthrough items back into the modeled egress
/// `input[]` so each sits at its original inbound position instead of
/// being appended after every modeled item. Each passthrough carries
/// `modeled_prefix` = the count of modeled INPUT items that preceded it
/// inbound; it is emitted after that many modeled EGRESS items.
/// Passthroughs keep their mutual inbound order.
///
/// Residual limitation: the prefix counts INBOUND modeled items, while
/// the splice indexes EGRESS modeled items. These match 1:1 for plain
/// messages (the regression-tested case), but a single modeled inbound
/// item can expand to a different egress-item count -- an assistant turn
/// with tool_calls emits a message item PLUS one `function_call` item per
/// call, and a `function_call` input item attaches to a prior turn rather
/// than emitting its own message. When the counts diverge a passthrough
/// lands at the best stable position (prefix-count splice, clamped to the
/// tail) rather than its exact original slot -- still order-stable, never
/// dropped.
fn merge_passthrough(
    modeled: Vec<ResponseInput>,
    passthrough: &[ResponsesPassthroughItem],
) -> Vec<ResponseInput> {
    if passthrough.is_empty() {
        return modeled;
    }
    let mut out: Vec<ResponseInput> = Vec::with_capacity(modeled.len() + passthrough.len());
    let mut next = passthrough.iter().peekable();
    for (i, item) in modeled.into_iter().enumerate() {
        while let Some(p) = next.peek() {
            if p.modeled_prefix > i {
                break;
            }
            out.push(ResponseInput::Passthrough(
                next.next().expect("peeked").item.clone(),
            ));
        }
        out.push(item);
    }
    // Trailing passthroughs whose prefix exceeds the modeled egress count
    // (or that follow the last modeled item) land at the tail, in order.
    for p in next {
        out.push(ResponseInput::Passthrough(p.item.clone()));
    }
    out
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
            // This `continue` skips only the REPORTING of a system-surface
            // marker, never the marker's translation: `system.rs` owns both
            // the drop and its record for that surface. Nothing is lost here
            // that is not accounted for there.
            // TRANSLATION-DROP: structural -- de-duplicates the system surface's diagnostic; system.rs owns that drop and its record
            BreakpointPosition::System => continue,
        };
        if !surfaces.contains(&name) {
            surfaces.push(name);
        }
    }
    surfaces
}

/// Emit one WARN naming every cache-prefix surface carrying a caller
/// `cache_control` marker that the Responses egress drops, and count the
/// drop once for the request. Matches the openai-compat egress convention
/// (`check_dropped_anthropic_fields`), which WARNs on every dropped
/// cache_control carrier so an operator routing cache-hinted traffic to a
/// Responses target sees the same breadcrumb. Logs only the surface
/// name(s) + a count: no message content, no bodies, no secrets.
///
/// The WARN and the COUNTER have deliberately different surface sets. The
/// WARN excludes `system` because `system.rs` already logs that surface at
/// DEBUG and a second record would double-report it. The counter includes
/// it, because a request whose ONLY marker sat on a system block did have a
/// marker dropped, and a counter that missed it would understate the lane's
/// drop rate. Both fire at most once per request either way.
///
/// Cross-dialect only. The Responses wire has no prompt-cache breakpoint
/// field, so its ingress mints no `cache_control` anywhere -- every content
/// part and system block it builds sets the marker to `None`. A marker
/// reaching this egress therefore came from an Anthropic-shape or
/// OpenAI-shape client. Seed per foundations sec 14, deletion-blocked
/// pending per-lane wire evidence.
/// TRANSLATION-DROP: lane=openai-responses class=cache_control_unsupported test=cache_control_marker_drops_from_the_wire_and_counts
fn warn_dropped_cache_control(req: &ChatRequest) {
    if !req.cache_breakpoints().is_empty() {
        record_translation_drop("openai-responses", "cache_control_unsupported");
    }
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

// response_format round-trip: the Responses ingress parses inbound
// text.format into req.response_format; the egress must re-emit it onto
// text.format (same-protocol regression).
#[cfg(test)]
mod response_format_tests {
    use super::translate;
    use crate::openai_responses::{AuthKind, OpenAiResponsesConfig};
    use routectl_core::{ChatRequest, Message, MessageContent, Role};
    use serde_json::json;

    fn cfg() -> OpenAiResponsesConfig {
        let mut c = OpenAiResponsesConfig::new("openai-responses:test", "literal:test");
        c.auth_kind = AuthKind::ChatgptOauth;
        c
    }

    fn req_with(response_format: Option<serde_json::Value>) -> ChatRequest {
        ChatRequest {
            model: "gpt-5".into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }]
            .into(),
            response_format,
            ..Default::default()
        }
    }

    #[test]
    fn json_schema_response_format_round_trips_to_text_format() {
        let req = req_with(Some(json!({
            "type": "json_schema",
            "json_schema": {
                "name": "widget",
                "schema": {"type": "object", "required": ["x"]},
                "strict": true
            }
        })));
        let request = translate(&cfg(), &req).unwrap();
        let body = serde_json::to_value(&request).unwrap();
        let fmt = &body["text"]["format"];
        assert_eq!(fmt["type"], "json_schema", "got: {body}");
        assert_eq!(fmt["name"], "widget", "got: {body}");
        assert_eq!(fmt["schema"]["required"][0], "x", "got: {body}");
        assert_eq!(fmt["strict"], true, "got: {body}");
    }

    #[test]
    fn json_object_response_format_round_trips_to_text_format() {
        let req = req_with(Some(json!({"type": "json_object"})));
        let request = translate(&cfg(), &req).unwrap();
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(body["text"]["format"]["type"], "json_object", "got: {body}");
    }

    #[test]
    fn response_format_merges_with_verbosity_from_provider_extras() {
        // The ingress lifts the non-format remainder of `text` (e.g.
        // verbosity) into provider_extras["text"]; the egress must keep it
        // and merge the format alongside.
        let mut req = req_with(Some(json!({"type": "json_object"})));
        req.provider_extras = Some(json!({"text": {"verbosity": "low"}}));
        let request = translate(&cfg(), &req).unwrap();
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(body["text"]["verbosity"], "low", "got: {body}");
        assert_eq!(body["text"]["format"]["type"], "json_object", "got: {body}");
    }

    #[test]
    fn caller_text_format_wins_over_response_format() {
        // An operator-supplied text.format is left untouched.
        let mut req = req_with(Some(json!({"type": "json_object"})));
        req.provider_extras = Some(json!({
            "text": {"format": {"type": "json_schema", "name": "op", "schema": {}}}
        }));
        let request = translate(&cfg(), &req).unwrap();
        let body = serde_json::to_value(&request).unwrap();
        assert_eq!(
            body["text"]["format"]["type"], "json_schema",
            "caller text.format must win: {body}"
        );
    }

    #[test]
    fn no_response_format_leaves_text_absent() {
        let req = req_with(None);
        let request = translate(&cfg(), &req).unwrap();
        let body = serde_json::to_value(&request).unwrap();
        assert!(body.get("text").is_none(), "got: {body}");
    }
}
