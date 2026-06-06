//! OpenAI Chat Completions ingress (`POST /v1/chat/completions`).
//!
//! The OpenAI dialect IS the canonical wire shape: the existing routectl
//! v0.3 server expected callers to POST a `ChatRequest`-shaped JSON body.
//! v0.4.0 keeps that exactly the same -- this adapter is a thin wrapper
//! that satisfies the `IngressAdapter` trait without changing semantics.
//!
//! v0.6.0 collapsed the per-dialect alias map into the top-level
//! `[aliases]` table, so this adapter no longer carries any state. The
//! ingress reads the `x-routectl-alias` header (when set) or passes
//! the wire `model` field through verbatim; the router does the
//! alias resolution.
//!
//! Streaming convention: OpenAI emits a sequence of bare `data: <json>`
//! frames followed by `data: [DONE]`. No named events.

use std::any::Any;

use axum::http::HeaderMap;
use routectl_core::{
    is_canonical_request_key, ChatChunk, ChatRequest, ChatResponse, ContentPart, Error,
    KnownContentPart, MessageContent, Result, Role, SystemContent,
};
use serde_json::{Map, Value};

use super::{
    read_alias_header, ErrorEnvelopeShape, IngressAdapter, IngressStreamState, SseEvent,
    STREAM_ERROR_TYPE,
};

const DONE_SENTINEL: &str = "[DONE]";

#[derive(Debug, Default)]
pub struct OpenAiIngress;

#[derive(Debug, Default)]
pub struct OpenAiStreamState;

impl IngressStreamState for OpenAiStreamState {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

impl IngressAdapter for OpenAiIngress {
    fn id(&self) -> &str {
        "openai"
    }

    fn error_envelope_shape(&self) -> ErrorEnvelopeShape {
        ErrorEnvelopeShape::OpenAi
    }

    fn parse_request(&self, headers: &HeaderMap, body: Value) -> Result<ChatRequest> {
        // Trace-level ingress body for triage. Inherits the
        // parent span's `request_id` so a `grep request_id=<id>`
        // shows ingress -> outgoing -> upstream -> egress in one
        // pass. Gated by `tracing::Level::TRACE`; default `info`
        // level pays nothing. Honors ROUTECTL_LOG_REDACT_PROMPTS=1.
        routectl_core::trace_ingress_body("openai", &body);
        // Companion structural summary -- a single TRACE line of
        // stable, prompt-content-free fields the operator's
        // smart-heartbeat validator can grep without fighting the
        // 16 KB body cap. See StructuralSummary on field stability.
        routectl_core::trace_structural_summary("ingress", "ingress", "openai", &body);
        let mut body = body;
        // Coalesce DeepSeek/vLLM-shape `reasoning_content` into
        // canonical `reasoning` on each message BEFORE serde
        // deserialization. Without this, opencode-style clients
        // echoing assistant `reasoning_content` on the wire have
        // their reasoning silently dropped (canonical `Message`
        // doesn't serde-alias to `reasoning_content`; aliasing was
        // explicitly rejected on the schema because NIM emits BOTH
        // keys with one null and serde would dup-fail). The
        // coalescer mirrors `merge_reasoning_keys` in
        // `openai_compat::response.rs` -- same prefer-non-null
        // semantics applied at parse time.
        coalesce_message_reasoning_keys(&mut body);
        // o-series / gpt-5+ clients send `max_completion_tokens`
        // instead of `max_tokens`. `is_canonical_request_key`
        // returns true for it (reserved.rs), so the forward-compat
        // sweep below leaves it in the body -- but `ChatRequest` has
        // no such field and no serde alias, so serde silently drops
        // it. Rename it to `max_tokens` here, before deserialization,
        // so the per-request token cap is never lost.
        normalize_max_completion_tokens(&mut body);
        // o-series / gpt-5+ clients also send `role: "developer"` as
        // the system-voice successor. The canonical `Role` enum does
        // not include that variant so serde would 400. Rewrite it to
        // `role: "system"` here, before deserialization, so it then
        // flows through `lift_system_messages` normally.
        normalize_developer_role(&mut body);

        // Forward-compat sweep: pull every top-level key NOT on
        // `ChatRequest` into `provider_extras` so OpenAI clients
        // sending long-tail knobs (`service_tier`, `parallel_tool_calls`,
        // `prediction`, `audio`, `metadata`, future fields) don't lose
        // them at the ingress boundary. Mirrors the Anthropic ingress
        // sweep so both dialects forward unknown body fields verbatim
        // to the egress (which merges via `merge_provider_extras`).
        let extras = sweep_unknown_top_level_fields(&mut body);

        let mut req: ChatRequest = serde_json::from_value(body).map_err(|e| {
            Error::Validation(format!(
                "openai ingress: invalid /v1/chat/completions body: {e}"
            ))
        })?;
        // Merge swept extras into req.provider_extras (the body may
        // have already carried an explicit `provider_extras` object;
        // sweep keeps both -- the swept ones win on conflict because
        // they were the unknown fields that needed preservation).
        if !extras.is_empty() {
            merge_into_provider_extras(&mut req, extras);
        }
        // v0.6.0: alias resolution lives entirely in the router. The
        // ingress only honors the `x-routectl-alias` header override
        // (otherwise the wire `model` value passes through verbatim).
        if let Some(alias) = read_alias_header(headers) {
            req.model = alias;
        }
        // Honor the canonical contract: `req.system` is the source of
        // truth at egress time. Lift any Role::System messages into
        // `req.system` here at ingress so every egress reads the same
        // shape. Concat with newlines when multiple system messages
        // are present (matching the legacy lift-from-egress behavior).
        lift_system_messages(&mut req);
        // tool_choice and OpenAI function tools are NOT translated at
        // the ingress: different egresses want different shapes
        // (openai-compat passes through verbatim, Anthropic egress
        // translates). The ToolDef deserializer routes
        // `{type:"function",...}` to `ToolDef::Other`, where the
        // Anthropic egress's `translate_tool` already lifts it to
        // `AnthropicTool::Custom`. Translating here once and undoing
        // it at the openai-compat egress would be lossy and
        // double-touched -- leave canonical as the wire form.
        Ok(req)
    }

    fn render_response(&self, resp: ChatResponse) -> Result<Value> {
        // OpenAI-side mirror of the Anthropic ingress's tool_use dedup
        // (see `strip_tool_use_parts_when_tool_calls_present`). Strip
        // duplicate `tool_use` content blocks before the bare serde so
        // OpenAI Chat-Completions clients see the tool call only on the
        // `tool_calls` channel they understand.
        let resp = strip_tool_use_parts_when_tool_calls_present(resp);
        // Strip `matched_stop_sequence` from every choice: it is an
        // Anthropic-internal field set by the egress to round-trip the
        // matched stop sequence back through the canonical layer to the
        // Anthropic ingress. OpenAI Chat-Completions clients do not
        // expect it and some SDKs error or forward it unexpectedly.
        let resp = strip_matched_stop_sequence_from_response(resp);
        serde_json::to_value(&resp)
            .map_err(|e| Error::Internal(format!("openai ingress: serialize response: {e}")))
    }

    fn new_stream_state(&self) -> Box<dyn IngressStreamState> {
        Box::new(OpenAiStreamState)
    }

    fn render_chunk(
        &self,
        chunk: ChatChunk,
        _state: &mut dyn IngressStreamState,
    ) -> Result<Vec<SseEvent>> {
        // No tool_use dedup needed on the streaming path: the canonical
        // `ChunkDelta` has no `MessageContent::Parts` slot (`content` is
        // `Option<String>`, tool calls ride `tool_calls`), so a streamed
        // chunk can never carry a `tool_use` content block. The
        // double-emission the non-streaming `render_response` guards
        // against is structurally impossible here.
        //
        // Strip `matched_stop_sequence` before serializing: it is an
        // Anthropic-internal field (see `Choice.matched_stop_sequence`
        // docs) and OpenAI Chat-Completions clients do not expect it on
        // streaming chunks.
        let chunk = strip_matched_stop_sequence_from_chunk(chunk);
        let data = serde_json::to_string(&chunk)
            .map_err(|e| Error::Internal(format!("openai ingress: serialize chunk: {e}")))?;
        Ok(vec![SseEvent::unnamed(data)])
    }

    fn render_eos(&self, _state: &mut dyn IngressStreamState) -> Vec<SseEvent> {
        vec![SseEvent::unnamed(DONE_SENTINEL)]
    }

    fn render_error_eos(
        &self,
        _state: &mut dyn IngressStreamState,
        error: &dyn std::fmt::Display,
    ) -> Vec<SseEvent> {
        // The caller in `handlers::ingress_handle` already strips
        // provider names, upstream bodies, and tokens before passing
        // `error`, but a second `sanitize_for_log` pass here filters
        // control chars (CRLF, ANSI escapes, NULs) that would
        // otherwise break SSE wire framing or forge log lines on
        // downstream text-format subscribers. Defense in depth.
        let msg = routectl_core::sanitize_for_log(&error.to_string());
        let payload = serde_json::json!({
            "error": {
                "type": STREAM_ERROR_TYPE,
                "message": msg,
            }
        });
        // OpenAI streaming clients consume `data: [DONE]` as the
        // universal stream terminator (success OR failure). The
        // preceding `error` chunk lets the SDK distinguish a clean
        // failure from a clean completion, so it stops the
        // suspected-truncation retry loop.
        vec![
            SseEvent::unnamed(
                serde_json::to_string(&payload)
                    .expect("Value serialization is infallible for BTreeMap-backed literals"),
            ),
            SseEvent::unnamed(DONE_SENTINEL),
        ]
    }
}

/// Pull every top-level body key NOT recognized as a canonical
/// `ChatRequest` field into a separate `Map` so the caller can stash
/// it in `provider_extras`. Mirrors `sweep_anthropic_extras` so the
/// two ingresses share the forward-compat property: a new OpenAI
/// top-level field (e.g. `service_tier`, `parallel_tool_calls`,
/// `prediction`, `audio`, future additions) reaches the egress
/// without a code edit and is forwarded verbatim by
/// `merge_provider_extras`.
fn sweep_unknown_top_level_fields(body: &mut Value) -> Map<String, Value> {
    let Some(obj) = body.as_object_mut() else {
        return Map::new();
    };
    let unknown_keys: Vec<String> = obj
        .keys()
        .filter(|k| !is_canonical_request_key(k))
        .cloned()
        .collect();
    let mut extras = Map::new();
    for k in unknown_keys {
        if let Some(v) = obj.remove(&k) {
            extras.insert(k, v);
        }
    }
    extras
}

/// Merge swept extras into `req.provider_extras`. Preserves any
/// existing `provider_extras` object the caller sent explicitly;
/// swept-unknown fields take precedence on key conflict because
/// they're the ones a future serde update would otherwise drop.
fn merge_into_provider_extras(req: &mut ChatRequest, swept: Map<String, Value>) {
    if swept.is_empty() {
        return;
    }
    let mut combined = match req.provider_extras.take() {
        Some(Value::Object(existing)) => existing,
        _ => Map::new(),
    };
    for (k, v) in swept {
        combined.insert(k, v);
    }
    req.provider_extras = Some(Value::Object(combined));
}

/// Coalesce `reasoning_content` into `reasoning` on each message
/// before serde deserialization. Mirrors the response-side
/// `merge_reasoning_keys`: DeepSeek-style upstreams carry the text
/// under `reasoning_content`, but canonical `Message.reasoning` is
/// the only string slot.
///
/// Prefer-non-null: keep non-null `reasoning`; else promote non-null
/// `reasoning_content`; else drop both. Coalescing here (vs. a serde
/// alias) handles NIM's both-keys-one-null shape that would
/// deserialize-fail with "duplicate field reasoning".
fn coalesce_message_reasoning_keys(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(|v| v.as_array_mut()) else {
        return;
    };
    for msg in messages.iter_mut() {
        let Some(obj) = msg.as_object_mut() else {
            continue;
        };
        let rc = obj.remove("reasoning_content");
        let r_is_null = obj.get("reasoning").map_or(true, |v| v.is_null());
        if r_is_null {
            match rc {
                Some(v) if !v.is_null() => {
                    obj.insert("reasoning".into(), v);
                }
                _ => {
                    obj.remove("reasoning");
                }
            }
        }
    }
}

/// Normalize the o-series / gpt-5+ `max_completion_tokens` field to
/// the canonical `max_tokens` before serde deserialization.
///
/// Rules:
/// - Only `max_completion_tokens` present: rename it to `max_tokens`.
/// - Both present: keep `max_tokens` (wins), remove
///   `max_completion_tokens` so it neither overwrites nor lingers as
///   an unknown canonical key that the forward-compat sweep skips.
/// - Only `max_tokens`: no-op.
fn normalize_max_completion_tokens(body: &mut Value) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let has_mct = obj.contains_key("max_completion_tokens");
    if !has_mct {
        return;
    }
    let mct_val = obj.remove("max_completion_tokens");
    if !obj.contains_key("max_tokens") {
        if let Some(v) = mct_val {
            obj.insert("max_tokens".into(), v);
        }
    }
    // If `max_tokens` was already present we just removed
    // `max_completion_tokens` above; `max_tokens` stays unchanged.
}

/// Rewrite `role: "developer"` to `role: "system"` on each message
/// before serde deserialization. The o-series / gpt-5+ system-voice
/// successor role is not a canonical `Role` variant; renaming it here
/// means the message then flows through `lift_system_messages` normally.
/// No other role values are touched.
fn normalize_developer_role(body: &mut Value) {
    let Some(messages) = body.get_mut("messages").and_then(|v| v.as_array_mut()) else {
        return;
    };
    for msg in messages.iter_mut() {
        let Some(obj) = msg.as_object_mut() else {
            continue;
        };
        if obj.get("role").and_then(|v| v.as_str()) == Some("developer") {
            obj.insert("role".into(), Value::String("system".into()));
        }
    }
}

/// If any `Role::System` messages are in `req.messages`, lift their text
/// content into `req.system` and remove them from the messages array.
/// No-op when there are no System messages.
///
/// Lifting strategy:
/// - `MessageContent::Text` messages contribute a `SystemBlock` with no
///   `cache_control` (same as before v0.7.0).
/// - `MessageContent::Parts` messages contribute one `SystemBlock` per text
///   part, preserving `cache_control` and `citations` from each part.
///
/// Output shape:
/// - When no lifted block carries `cache_control` or `citations`, the result
///   is concatenated into `SystemContent::Text` (backward-compatible).
/// - When any block carries `cache_control` or `citations`, the result is
///   `SystemContent::Blocks` so the per-block cache breakpoints survive the
///   ingress boundary and reach the Anthropic / Bedrock egress intact.
fn lift_system_messages(req: &mut ChatRequest) {
    let mut lifted_blocks: Vec<routectl_core::SystemBlock> = Vec::new();
    req.messages.retain(|m| {
        if !matches!(m.role, Role::System) {
            return true;
        }
        match &m.content {
            MessageContent::Text(t) => {
                lifted_blocks.push(routectl_core::SystemBlock {
                    kind: "text".into(),
                    text: t.clone(),
                    cache_control: None,
                    citations: None,
                });
            }
            MessageContent::Parts(parts) => {
                // Preserve cache_control and citations from each text part
                // so prompt-cache breakpoints survive the ingress boundary.
                // Non-text parts (images, documents) in a System message are
                // not meaningful in canonical and would be dropped by egresses
                // that do not support them; skip them here as before.
                for p in parts {
                    if let ContentPart::Known(KnownContentPart::Text {
                        text,
                        cache_control,
                    }) = p
                    {
                        lifted_blocks.push(routectl_core::SystemBlock {
                            kind: "text".into(),
                            text: text.clone(),
                            cache_control: cache_control.clone(),
                            citations: None,
                        });
                    }
                }
            }
            MessageContent::Null => {}
        }
        false
    });

    if lifted_blocks.is_empty() {
        return;
    }

    let needs_blocks = lifted_blocks
        .iter()
        .any(|b| b.cache_control.is_some() || b.citations.is_some());

    match req.system.take() {
        Some(SystemContent::Text(existing)) if !needs_blocks => {
            let lifted_text = lifted_blocks
                .iter()
                .map(|b| b.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            req.system = Some(SystemContent::Text(format!("{existing}\n{lifted_text}")));
        }
        Some(SystemContent::Text(existing)) => {
            // Upgrade the plain existing text to a Blocks vec so that
            // cache_control on the lifted parts is not silently dropped.
            let mut blocks = vec![routectl_core::SystemBlock {
                kind: "text".into(),
                text: existing,
                cache_control: None,
                citations: None,
            }];
            blocks.extend(lifted_blocks);
            req.system = Some(SystemContent::Blocks(blocks));
        }
        Some(SystemContent::Blocks(mut blocks)) => {
            blocks.extend(lifted_blocks);
            req.system = Some(SystemContent::Blocks(blocks));
        }
        None if !needs_blocks => {
            let lifted_text = lifted_blocks
                .iter()
                .map(|b| b.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            req.system = Some(SystemContent::Text(lifted_text));
        }
        None => {
            req.system = Some(SystemContent::Blocks(lifted_blocks));
        }
    }
}

/// Strip `matched_stop_sequence` from every `Choice` in a `ChatResponse`.
/// This is an Anthropic-internal field set by the egress to thread the
/// matched stop sequence back to the Anthropic ingress via the canonical
/// layer. OpenAI Chat-Completions clients do not expect it and some SDKs
/// error or forward it unexpectedly to callers.
fn strip_matched_stop_sequence_from_response(mut resp: ChatResponse) -> ChatResponse {
    for choice in resp.choices.iter_mut() {
        choice.matched_stop_sequence = None;
    }
    resp
}

/// Strip `matched_stop_sequence` from every `ChunkChoice` in a `ChatChunk`.
/// Same rationale as `strip_matched_stop_sequence_from_response`: OpenAI
/// streaming clients do not expect this Anthropic-internal field.
fn strip_matched_stop_sequence_from_chunk(mut chunk: ChatChunk) -> ChatChunk {
    for choice in chunk.choices.iter_mut() {
        choice.matched_stop_sequence = None;
    }
    chunk
}

/// Strip duplicate `tool_use` content blocks from any assistant choice
/// that also carries a non-empty `tool_calls`.
///
/// The Anthropic-shape egresses (anthropic-api, bedrock-converse,
/// openai-responses) populate BOTH the canonical `tool_calls` field AND
/// a `ContentPart::ToolUse` part for the same upstream tool call, so a
/// bare serde of the canonical response would emit the call twice: once
/// in `tool_calls` (the channel OpenAI clients read) and once as a
/// `tool_use` content block. OpenAI Chat-Completions clients do not
/// understand `tool_use` blocks on assistant messages -- many SDKs choke
/// or silently drop the sibling text when they see one.
///
/// This is the OpenAI-side mirror of the Anthropic ingress's
/// `parts_tool_use_ids` dedup (render.rs ~183), run in reverse: there,
/// the `parts` ToolUse wins because it carries `cache_control`; here,
/// `tool_calls` wins because it is the OpenAI-native channel and the
/// `tool_use` part is the unrenderable duplicate. We strip the ToolUse
/// parts and leave `tool_calls` untouched.
///
/// After the strip, the surviving content collapses the same way
/// `select_message_content` (openai_responses/response.rs) builds it:
/// all-text -> a `content` string; nothing left -> `content: null`
/// (OpenAI's shape for an assistant turn that is purely tool calls); any
/// non-text part remaining -> keep the parts array verbatim.
fn strip_tool_use_parts_when_tool_calls_present(mut resp: ChatResponse) -> ChatResponse {
    for choice in resp.choices.iter_mut() {
        let has_tool_calls = choice
            .message
            .tool_calls
            .as_ref()
            .is_some_and(|tcs| !tcs.is_empty());
        if !has_tool_calls {
            continue;
        }
        let MessageContent::Parts(parts) = &choice.message.content else {
            continue;
        };
        if !parts.iter().any(is_tool_use_part) {
            continue;
        }
        let remaining: Vec<ContentPart> = parts
            .iter()
            .filter(|p| !is_tool_use_part(p))
            .cloned()
            .collect();
        choice.message.content = collapse_after_tool_use_strip(remaining);
    }
    resp
}

/// True for a `tool_use` content block in either shape: the typed
/// `KnownContentPart::ToolUse`, or the forward-compat `ContentPart::Other`
/// whose `type` discriminant is `"tool_use"` (the variant a future
/// tool_use sub-field would deserialize into). Mirrors the dual-shape
/// scan in the Anthropic ingress dedup.
fn is_tool_use_part(part: &ContentPart) -> bool {
    match part {
        ContentPart::Known(KnownContentPart::ToolUse { .. }) => true,
        ContentPart::Other { type_tag, .. } => type_tag == "tool_use",
        _ => false,
    }
}

/// Collapse the content parts left after stripping tool_use blocks into
/// the shape an OpenAI client expects: `null` when nothing meaningful
/// remains, a plain text string when only text parts survive, otherwise
/// the parts array verbatim. Matches `select_message_content`'s
/// all-text-collapses-to-Text convention.
fn collapse_after_tool_use_strip(parts: Vec<ContentPart>) -> MessageContent {
    if parts.is_empty() {
        return MessageContent::Null;
    }
    let only_text = parts
        .iter()
        .all(|p| matches!(p, ContentPart::Known(KnownContentPart::Text { .. })));
    if !only_text {
        return MessageContent::Parts(parts);
    }
    let text = parts
        .iter()
        .filter_map(|p| match p {
            ContentPart::Known(KnownContentPart::Text { text, .. }) => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    MessageContent::Text(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{
        Choice, ChunkChoice, ChunkDelta, Message, MessageContent, ReasoningConfig,
        ReasoningDetailKind,
    };
    use serde_json::json;

    #[test]
    fn parse_request_accepts_canonical_body() {
        let body = json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        });
        let req = OpenAiIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        assert_eq!(req.model, "gpt-4o");
        assert_eq!(req.stream, Some(true));
    }

    #[test]
    fn parse_request_with_reasoning_config_round_trips() {
        let body = json!({
            "model": "openai/o3",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning": {"effort": "high"}
        });
        let req = OpenAiIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        assert_eq!(req.reasoning.unwrap().effort.as_deref(), Some("high"));
        // The unused-import lint catches accidental dead deps.
        let _ = ReasoningConfig::default();
        let _ = ReasoningDetailKind::Text;
    }

    #[test]
    fn parse_request_rejects_malformed_body() {
        let body = json!({"this": "is not a chat request"});
        let err = OpenAiIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap_err();
        assert!(matches!(err, Error::Validation(_)));
    }

    #[test]
    fn render_chunk_emits_single_unnamed_data_frame() {
        let chunk = ChatChunk {
            id: "chatcmpl-1".into(),
            model: "gpt-4o".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    content: Some("hello".into()),
                    ..Default::default()
                },
                finish_reason: None,
                matched_stop_sequence: None,
            }],
            usage: None,
            opaque_events: Vec::new(),
        };
        let mut state = OpenAiIngress.new_stream_state();
        let events = OpenAiIngress.render_chunk(chunk, state.as_mut()).unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].event.is_none());
        assert!(events[0].data.contains("\"content\":\"hello\""));
    }

    #[test]
    fn render_eos_emits_done_sentinel() {
        let mut state = OpenAiIngress.new_stream_state();
        let events = OpenAiIngress.render_eos(state.as_mut());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "[DONE]");
        assert!(events[0].event.is_none());
    }

    /// Mid-stream upstream failure on the OpenAI ingress: the adapter
    /// must emit a clean FAILURE terminator pair (one `data:
    /// {"error":...}` chunk followed by one `data: [DONE]` chunk) so
    /// SDK consumers (Claude Code SDK, OpenAI SDK) see a clean failure
    /// rather than network truncation. `[DONE]` is the OpenAI
    /// universal stream terminator; without it, SDKs treat the close
    /// as a truncation and retry.
    #[test]
    fn render_error_eos_returns_openai_error_chunk_then_done() {
        // Arrange
        let mut state = OpenAiIngress.new_stream_state();
        let error_msg = "upstream stream error (HTTP 529)";

        // Act
        let events = OpenAiIngress.render_error_eos(state.as_mut(), &error_msg);

        // Assert
        // Two events: error chunk + [DONE].
        assert_eq!(events.len(), 2, "expected error chunk + [DONE]");
        // First: bare `data: <json>` chunk with the error envelope.
        assert!(
            events[0].event.is_none(),
            "OpenAI emits unnamed (bare data:) frames"
        );
        let payload: Value = serde_json::from_str(&events[0].data).unwrap();
        assert_eq!(payload["error"]["type"], STREAM_ERROR_TYPE);
        assert!(payload["error"]["message"]
            .as_str()
            .unwrap()
            .contains("upstream stream error"));
        // Second: the universal `[DONE]` terminator.
        assert!(events[1].event.is_none());
        assert_eq!(events[1].data, "[DONE]");
    }

    /// Belt-and-suspenders sanitization. Even though the caller in
    /// `handlers::ingress_handle` strips provider names and tokens
    /// before passing the error here, control characters in a
    /// future caller's message must still be filtered out so the
    /// emitted SSE bytes never break framing or forge log lines on
    /// downstream text-format subscribers.
    #[test]
    fn render_error_eos_filters_control_chars_via_sanitize_for_log() {
        // Arrange: a message containing CR, LF, and an ANSI escape.
        let mut state = OpenAiIngress.new_stream_state();
        let dirty = "upstream stream error\r\n\x1b[31mexploit\x1b[0m";

        // Act
        let events = OpenAiIngress.render_error_eos(state.as_mut(), &dirty);

        // Assert: the emitted message has no raw \r, \n, or ESC bytes.
        let payload: Value = serde_json::from_str(&events[0].data).unwrap();
        let msg = payload["error"]["message"].as_str().unwrap();
        assert!(!msg.contains('\r'), "raw CR must be filtered: {msg:?}");
        assert!(!msg.contains('\n'), "raw LF must be filtered: {msg:?}");
        assert!(
            !msg.contains('\x1b'),
            "raw ESC byte must be filtered: {msg:?}"
        );
        // The placeholder `?` from sanitize_for_log appears in place
        // of each filtered byte.
        assert!(
            msg.contains('?'),
            "sanitization placeholder present: {msg:?}"
        );
    }

    #[test]
    fn openai_ingress_lifts_role_system_to_canonical_system() {
        let body = json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "you are helpful"},
                {"role": "user", "content": "hi"}
            ]
        });
        let req = OpenAiIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        match req.system {
            Some(SystemContent::Text(s)) => assert_eq!(s, "you are helpful"),
            other => panic!("expected SystemContent::Text, got {other:?}"),
        }
        // System message removed from the messages array.
        assert_eq!(req.messages.len(), 1);
        assert!(matches!(req.messages[0].role, Role::User));
    }

    #[test]
    fn openai_ingress_concatenates_multiple_role_system_messages() {
        let body = json!({
            "model": "gpt-4o",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "system", "content": "be polite"},
                {"role": "user", "content": "hi"}
            ]
        });
        let req = OpenAiIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        match req.system {
            Some(SystemContent::Text(s)) => assert_eq!(s, "be brief\nbe polite"),
            other => panic!("expected concat, got {other:?}"),
        }
        assert_eq!(req.messages.len(), 1);
    }

    #[test]
    fn openai_ingress_appends_lifted_system_to_existing_text_system() {
        // Edge case: caller already set req.system explicitly AND has
        // Role::System messages. Lift appends to existing.
        let body = json!({
            "model": "gpt-4o",
            "system": "primary",
            "messages": [
                {"role": "system", "content": "secondary"},
                {"role": "user", "content": "hi"}
            ]
        });
        let req = OpenAiIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        match req.system {
            Some(SystemContent::Text(s)) => assert_eq!(s, "primary\nsecondary"),
            other => panic!("expected concat, got {other:?}"),
        }
    }

    #[test]
    fn openai_ingress_passes_function_tools_through_as_other_verbatim() {
        // OpenAI function tool wire shape `{type: "function", function:
        // {...}}` must pass through canonical as `ToolDef::Other` with
        // the original Value preserved verbatim. Lifting to
        // `ToolDef::Custom` breaks the openai-compat egress path:
        // `Custom` serializes flat (no `type:"function"` wrapper) so
        // DeepSeek 400's. The Anthropic egress's `translate_tool`
        // already converts function-shape `Other` to
        // `AnthropicTool::Custom`, so the lift would lose nothing on
        // that path.
        let body = json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get current weather",
                    "parameters": {
                        "type": "object",
                        "properties": {"city": {"type": "string"}},
                        "required": ["city"]
                    },
                    "strict": true
                }
            }]
        });
        let req = OpenAiIngress
            .parse_request(&HeaderMap::new(), body.clone())
            .unwrap();
        let tools = req.tools.expect("tools present");
        assert_eq!(tools.len(), 1);
        match &tools[0] {
            routectl_core::ToolDef::Other(v) => {
                // Verbatim preservation: the wire JSON survives the
                // round-trip through canonical.
                assert_eq!(v, &body["tools"][0]);
            }
            other => panic!("expected ToolDef::Other (function-shape passthrough), got {other:?}"),
        }
    }

    #[test]
    fn openai_ingress_passes_unknown_tool_shapes_through_as_other() {
        // Builtin / unknown tool shapes (Anthropic builtins, server-side
        // tools, future formats) must NOT be coerced to Custom; they
        // pass through as ToolDef::Other so the appropriate egress can
        // forward them verbatim.
        let body = json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "type": "bash_20250124",
                "name": "bash"
            }]
        });
        let req = OpenAiIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        let tools = req.tools.expect("tools present");
        assert!(matches!(&tools[0], routectl_core::ToolDef::Other(_)));
    }

    #[test]
    fn tool_choice_passes_through_canonical_unchanged() {
        // tool_choice translation belongs in the egress (different
        // upstreams want different shapes -- openai-compat wants the
        // OpenAI shape unchanged, Anthropic wants {"type":"auto"}, etc).
        // The ingress is shape-agnostic and passes whatever the wire
        // carried. Translating here breaks openai-compat egresses
        // (DeepSeek 400's on an Anthropic-shape tool_choice). Pin the
        // contract.
        for tc in [
            json!("auto"),
            json!("required"),
            json!("none"),
            json!({"type":"function","function":{"name":"X"}}),
            json!({"type":"auto"}),
            json!({"type":"tool","name":"X"}),
        ] {
            let body = json!({
                "model": "gpt-4o",
                "messages": [{"role":"user","content":"hi"}],
                "tool_choice": tc.clone(),
            });
            let req = OpenAiIngress
                .parse_request(&HeaderMap::new(), body)
                .unwrap();
            assert_eq!(
                req.tool_choice,
                Some(tc.clone()),
                "ingress must pass tool_choice through verbatim: {tc:?}"
            );
        }
    }

    #[test]
    fn render_response_serializes_canonical_to_wire() {
        let resp = ChatResponse {
            id: "chatcmpl-1".into(),
            model: "gpt-4o".into(),
            created: 1700000000,
            choices: vec![],
            usage: None,
            routectl_provider: Some("test".into()),
            extras: Default::default(),
        };
        let v = OpenAiIngress.render_response(resp).unwrap();
        assert_eq!(v["id"], "chatcmpl-1");
        assert_eq!(v["routectl_provider"], "test");
        // Suppress unused-import warnings.
        let _ = MessageContent::Text("".into());
    }

    /// Build a canonical assistant message shaped like an Anthropic-shape
    /// egress output: a `tool_calls` entry AND a `ContentPart::ToolUse`
    /// part carrying the same id, optionally preceded by a text part.
    fn anthropic_shape_choice(text: Option<&str>, tool_id: &str) -> Choice {
        let mut parts = Vec::new();
        if let Some(t) = text {
            parts.push(ContentPart::Known(KnownContentPart::Text {
                text: t.into(),
                cache_control: None,
            }));
        }
        parts.push(ContentPart::Known(KnownContentPart::ToolUse {
            id: tool_id.into(),
            name: "get_weather".into(),
            input: json!({ "city": "Paris" }),
            cache_control: None,
        }));
        Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: MessageContent::Parts(parts),
                reasoning: None,
                reasoning_details: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": tool_id,
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "arguments": "{\"city\":\"Paris\"}"
                    }
                })]),
            },
            finish_reason: Some("tool_calls".into()),
            matched_stop_sequence: None,
        }
    }

    fn response_with_choices(choices: Vec<Choice>) -> ChatResponse {
        ChatResponse {
            id: "chatcmpl-1".into(),
            model: "claude".into(),
            created: 1700000000,
            choices,
            usage: None,
            routectl_provider: Some("anthropic-api".into()),
            extras: Default::default(),
        }
    }

    /// Anthropic-shape egresses emit a tool call in BOTH `tool_calls` and
    /// a `ContentPart::ToolUse` part. The OpenAI ingress must strip the
    /// duplicate `tool_use` content block: OpenAI Chat-Completions
    /// clients do not understand it and many SDKs choke or drop the
    /// sibling text. This test FAILS before the dedup (the bare serde
    /// emits the tool_use block) and PASSES after.
    #[test]
    fn render_response_strips_tool_use_block_when_tool_calls_present() {
        // Arrange
        let tool_id = "toolu_01";
        let resp = response_with_choices(vec![anthropic_shape_choice(
            Some("Checking weather."),
            tool_id,
        )]);

        // Act
        let v = OpenAiIngress.render_response(resp).unwrap();
        let msg = &v["choices"][0]["message"];

        // Assert: tool_calls survives untouched -- the OpenAI channel.
        assert_eq!(msg["tool_calls"][0]["id"], tool_id);
        assert_eq!(msg["tool_calls"][0]["function"]["name"], "get_weather");
        // Assert: no tool_use block survives anywhere in the message.
        assert!(
            !msg.to_string().contains("tool_use"),
            "message must carry no tool_use content block: {msg}"
        );
        // The lone text part collapses to a plain content string.
        assert_eq!(msg["content"], "Checking weather.");
    }

    /// When the assistant turn is purely a tool call (a ToolUse part and
    /// nothing else alongside `tool_calls`), stripping leaves no content,
    /// so the ingress emits `content: null` -- OpenAI's shape for a
    /// tool-call-only assistant message.
    #[test]
    fn render_response_emits_null_content_when_only_tool_use_remains() {
        // Arrange
        let tool_id = "toolu_02";
        let resp = response_with_choices(vec![anthropic_shape_choice(None, tool_id)]);

        // Act
        let v = OpenAiIngress.render_response(resp).unwrap();
        let msg = &v["choices"][0]["message"];

        // Assert
        assert_eq!(msg["tool_calls"][0]["id"], tool_id);
        assert!(
            msg["content"].is_null(),
            "tool-call-only message must emit content: null, got {}",
            msg["content"]
        );
    }

    /// Forward-compat shape: a future tool_use sub-field deserializes the
    /// block into `ContentPart::Other { type_tag: "tool_use", .. }`
    /// instead of the typed `ToolUse`. The strip must cover that shape
    /// too (mirrors the Anthropic ingress's dual-shape scan), or the
    /// duplicate block leaks back onto the OpenAI wire.
    #[test]
    fn render_response_strips_forward_compat_other_tool_use_block() {
        // Arrange
        let tool_id = "toolu_03";
        let mut other_extras = serde_json::Map::new();
        other_extras.insert("id".into(), json!(tool_id));
        other_extras.insert("name".into(), json!("get_weather"));
        other_extras.insert("input".into(), json!({ "city": "Paris" }));
        other_extras.insert("future_field".into(), json!("v2"));
        let choice = Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::Text {
                        text: "Checking.".into(),
                        cache_control: None,
                    }),
                    ContentPart::Other {
                        type_tag: "tool_use".into(),
                        cache_control: None,
                        extras: other_extras,
                    },
                ]),
                reasoning: None,
                reasoning_details: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({ "id": tool_id, "type": "function" })]),
            },
            finish_reason: Some("tool_calls".into()),
            matched_stop_sequence: None,
        };
        let resp = response_with_choices(vec![choice]);

        // Act
        let v = OpenAiIngress.render_response(resp).unwrap();
        let msg = &v["choices"][0]["message"];

        // Assert
        assert!(
            !msg.to_string().contains("tool_use"),
            "forward-compat tool_use block must be stripped: {msg}"
        );
        assert_eq!(msg["content"], "Checking.");
    }

    /// Control: an assistant message with text content and NO tool_calls
    /// renders its text unchanged -- the strip must never touch a message
    /// that has no tool_calls.
    #[test]
    fn render_response_leaves_text_content_untouched_without_tool_calls() {
        // Arrange
        let resp = response_with_choices(vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: MessageContent::Text("hello world".into()),
                reasoning: None,
                reasoning_details: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }]);

        // Act
        let v = OpenAiIngress.render_response(resp).unwrap();

        // Assert
        assert_eq!(v["choices"][0]["message"]["content"], "hello world");
        assert!(v["choices"][0]["message"]["tool_calls"].is_null());
    }

    /// Streaming analog. The canonical `ChunkDelta` cannot represent a
    /// `tool_use` content block (its `content` is `Option<String>`, tool
    /// calls ride `tool_calls`), so a tool-call chunk renders the
    /// tool_calls channel only -- no tool_use block can ever appear on
    /// the OpenAI streaming wire. Pins that structural invariant.
    #[test]
    fn render_chunk_with_tool_calls_emits_no_tool_use_block() {
        // Arrange
        let chunk = ChatChunk {
            id: "chatcmpl-1".into(),
            model: "claude".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    tool_calls: Some(vec![json!({
                        "index": 0,
                        "id": "toolu_04",
                        "type": "function",
                        "function": {"name": "get_weather", "arguments": ""}
                    })]),
                    ..Default::default()
                },
                finish_reason: None,
                matched_stop_sequence: None,
            }],
            usage: None,
            opaque_events: Vec::new(),
        };
        let mut state = OpenAiIngress.new_stream_state();

        // Act
        let events = OpenAiIngress.render_chunk(chunk, state.as_mut()).unwrap();

        // Assert
        assert_eq!(events.len(), 1);
        assert!(events[0].data.contains("\"tool_calls\""));
        assert!(
            !events[0].data.contains("tool_use"),
            "streaming wire must carry no tool_use block: {}",
            events[0].data
        );
    }

    /// DeepSeek-style upstreams (and clients echoing them, like
    /// opencode) carry assistant reasoning under `reasoning_content`
    /// on the wire. The OpenAI ingress must coalesce that into
    /// canonical `reasoning` BEFORE serde deserialization, otherwise
    /// the field drops on the floor and the egress has nothing to
    /// echo back. Pin the contract.
    #[test]
    fn ingress_coalesces_reasoning_content_into_reasoning() {
        let body = json!({
            "model": "gpt-4o",
            "messages": [
                {"role":"user","content":"hi"},
                {"role":"assistant","content":"answer","reasoning_content":"my hidden chain"}
            ]
        });
        let req = OpenAiIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        let assistant = &req.messages[1];
        assert_eq!(assistant.reasoning.as_deref(), Some("my hidden chain"));
    }

    /// Counterpart: when both `reasoning` and `reasoning_content` are
    /// present (NIM does this with one set to null), the coalescer
    /// must prefer the non-null value rather than serde-dup-failing.
    #[test]
    fn ingress_coalesces_reasoning_keys_with_null_reasoning_field() {
        let body = json!({
            "model": "gpt-4o",
            "messages": [
                {"role":"assistant","content":"x","reasoning":null,"reasoning_content":"the real one"}
            ]
        });
        let req = OpenAiIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        assert_eq!(req.messages[0].reasoning.as_deref(), Some("the real one"));
    }

    #[test]
    fn ingress_prefers_existing_non_null_reasoning_over_reasoning_content() {
        // If both fields are non-null, `reasoning` wins (it's the
        // canonical field name). Drop `reasoning_content` afterward.
        let body = json!({
            "model": "gpt-4o",
            "messages": [
                {"role":"assistant","content":"x","reasoning":"primary","reasoning_content":"secondary"}
            ]
        });
        let req = OpenAiIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        assert_eq!(req.messages[0].reasoning.as_deref(), Some("primary"));
    }

    #[test]
    fn ingress_coalesce_no_op_when_neither_field_present() {
        // No reasoning fields at all -- canonical message has
        // reasoning = None.
        let body = json!({
            "model": "gpt-4o",
            "messages": [{"role":"user","content":"hi"}]
        });
        let req = OpenAiIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        assert!(req.messages[0].reasoning.is_none());
    }

    /// o-series / gpt-5+ clients send `max_completion_tokens` instead
    /// of `max_tokens`. The ingress must rename it before serde so the
    /// per-request token cap is not silently dropped.
    #[test]
    fn ingress_normalizes_max_completion_tokens_only() {
        let body = json!({
            "model": "o3",
            "messages": [{"role":"user","content":"hi"}],
            "max_completion_tokens": 8000
        });
        let req = OpenAiIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        assert_eq!(req.max_tokens, Some(8000));
    }

    /// When a client sends both keys, `max_tokens` wins and the request
    /// still parses cleanly (no unknown-key error, no overwrite).
    #[test]
    fn ingress_normalizes_max_completion_tokens_max_tokens_wins_on_conflict() {
        let body = json!({
            "model": "o3",
            "messages": [{"role":"user","content":"hi"}],
            "max_tokens": 100,
            "max_completion_tokens": 8000
        });
        let req = OpenAiIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        assert_eq!(req.max_tokens, Some(100));
    }

    /// Control: only `max_tokens` present -- no normalization, value
    /// passes through unchanged.
    #[test]
    fn ingress_normalizes_max_completion_tokens_noop_when_only_max_tokens() {
        let body = json!({
            "model": "gpt-4o",
            "messages": [{"role":"user","content":"hi"}],
            "max_tokens": 512
        });
        let req = OpenAiIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        assert_eq!(req.max_tokens, Some(512));
    }

    /// o-series / gpt-5+ clients may send `role: "developer"` as the
    /// system-voice successor role. The canonical `Role` enum has no
    /// such variant, so a bare serde would 400. The ingress rewrites
    /// it to `role: "system"` before deserialization, then
    /// `lift_system_messages` promotes it into `req.system` normally.
    #[test]
    fn ingress_treats_developer_role_as_system() {
        // Arrange: one developer-role message + one user message.
        let body = json!({
            "model": "o3",
            "messages": [
                {"role": "developer", "content": "respond in JSON only"},
                {"role": "user", "content": "list colors"}
            ]
        });

        // Act
        let req = OpenAiIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();

        // Assert: developer message lifted into req.system.
        match req.system {
            Some(SystemContent::Text(s)) => assert_eq!(s, "respond in JSON only"),
            other => panic!("expected SystemContent::Text, got {other:?}"),
        }
        // Developer message removed from messages array.
        assert_eq!(req.messages.len(), 1);
        assert!(matches!(req.messages[0].role, Role::User));
    }

    /// A Parts-form system message whose text block carries a
    /// `cache_control` marker must survive the `lift_system_messages`
    /// pass as `SystemContent::Blocks` with the per-block `cache_control`
    /// intact. Prior to the fix, the cache_control was silently dropped
    /// (the old code concatenated only the text string).
    #[test]
    fn lift_system_message_parts_preserves_cache_control() {
        // Arrange: system message with a Parts body carrying cache_control.
        let body = json!({
            "model": "claude-sonnet-4",
            "messages": [
                {
                    "role": "system",
                    "content": [
                        {
                            "type": "text",
                            "text": "be precise",
                            "cache_control": {"type": "ephemeral", "ttl": "5m"}
                        }
                    ]
                },
                {"role": "user", "content": "hi"}
            ]
        });

        // Act
        let req = OpenAiIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();

        // Assert: system is Blocks and cache_control survived.
        match &req.system {
            Some(SystemContent::Blocks(blocks)) => {
                assert_eq!(blocks.len(), 1);
                assert_eq!(blocks[0].text, "be precise");
                let cc = blocks[0]
                    .cache_control
                    .as_ref()
                    .expect("cache_control must survive the lift");
                assert_eq!(cc.effective_ttl(), "5m");
            }
            other => panic!("expected SystemContent::Blocks, got {other:?}"),
        }
        // System message removed from messages array.
        assert_eq!(req.messages.len(), 1);
    }

    /// `matched_stop_sequence` is an Anthropic-internal field threaded
    /// through the canonical layer so the Anthropic ingress can emit
    /// the correct `stop_reason` / `stop_sequence` pair. OpenAI
    /// Chat-Completions clients do not expect it. The OpenAI ingress
    /// must strip it from every `choices[]` entry before serializing.
    /// This test covers both the non-streaming (`render_response`) and
    /// the streaming (`render_chunk`) paths.
    #[test]
    fn render_strips_matched_stop_sequence_from_choices() {
        // --- non-streaming path ---
        let resp = ChatResponse {
            id: "chatcmpl-1".into(),
            model: "claude-sonnet-4".into(),
            created: 1700000000,
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: MessageContent::Text("done".into()),
                    reasoning: None,
                    reasoning_details: Vec::new(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
                finish_reason: Some("stop".into()),
                matched_stop_sequence: Some("</answer>".into()),
            }],
            usage: None,
            routectl_provider: None,
            extras: Default::default(),
        };
        let v = OpenAiIngress.render_response(resp).unwrap();
        let choice = &v["choices"][0];
        assert!(
            choice.get("matched_stop_sequence").is_none()
                || choice["matched_stop_sequence"].is_null(),
            "render_response must not emit matched_stop_sequence: {choice}"
        );

        // --- streaming path ---
        let chunk = ChatChunk {
            id: "chatcmpl-2".into(),
            model: "claude-sonnet-4".into(),
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    content: None,
                    ..Default::default()
                },
                finish_reason: Some("stop_sequence".into()),
                matched_stop_sequence: Some("</answer>".into()),
            }],
            usage: None,
            opaque_events: Vec::new(),
        };
        let mut state = OpenAiIngress.new_stream_state();
        let events = OpenAiIngress.render_chunk(chunk, state.as_mut()).unwrap();
        assert_eq!(events.len(), 1);
        assert!(
            !events[0].data.contains("matched_stop_sequence"),
            "render_chunk must not emit matched_stop_sequence: {}",
            events[0].data
        );
    }
}
