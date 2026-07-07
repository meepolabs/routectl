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
    ChatChunk, ChatRequest, ChatResponse, ContentPart, Error, KnownContentPart, MessageContent,
    Result, is_canonical_request_key,
};
use serde_json::{Map, Value};

use super::{
    ErrorEnvelopeShape, IngressAdapter, IngressStreamState, SseEvent, StreamErrorClass,
    StreamRequestContext, read_alias_header,
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
    fn id(&self) -> &'static str {
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
        // Vanilla OpenAI clients set thinking via a top-level
        // `reasoning_effort` string, not the canonical nested
        // `reasoning.effort`. `ChatRequest` has no such field and no
        // serde alias, so without this the key is swept into
        // `provider_extras`, merged verbatim into the egress body
        // (leaking a stray `reasoning_effort` that strict Anthropic-shape
        // upstreams reject), while `req.reasoning` stays None and thinking
        // is never composed. Promote it into `reasoning.effort` here, then
        // drop the top-level key so it never survives serde or the sweep.
        normalize_reasoning_effort(&mut body);

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
        super::lift_system_messages(&mut req);
        // tool_choice and OpenAI function tools are NOT translated at
        // the ingress: different egresses want different shapes
        // (openai-compat passes through verbatim, Anthropic egress
        // translates). The ToolDef deserializer routes
        // `{type:"function",...}` to `ToolDef::Other`, where the
        // Anthropic egress's `translate_tool` already lifts it to
        // `AnthropicTool::Custom`. Translating here once and undoing
        // it at the openai-compat egress would be lossy and
        // double-touched -- leave canonical as the wire form.

        // Stamp ingress provenance so downstream observability can
        // attribute the request to the OpenAI Chat Completions dialect.
        req.routectl_internal.provenance = routectl_core::RequestProvenance::OpenaiIngress;
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
        let mut value = serde_json::to_value(&resp)
            .map_err(|e| Error::Internal(format!("openai ingress: serialize response: {e}")))?;
        // Surface Anthropic/Bedrock cache-read counts under the standard
        // OpenAI `usage.prompt_tokens_details.cached_tokens` field so an
        // OpenAI-shape cost tracker sees them. Canonical emits them only
        // under the Anthropic-vocabulary `cache_read_input_tokens`, which
        // OpenAI clients ignore. Additive: `cache_read_input_tokens`
        // stays present, and a response with no cache tokens gains no
        // `prompt_tokens_details` key. Lives in the OpenAI dialect, not
        // the canonical type, so the Anthropic ingress keeps its own
        // vocabulary.
        surface_cached_tokens_in_usage(&mut value);
        Ok(value)
    }

    fn new_stream_state(&self, _ctx: &StreamRequestContext) -> Box<dyn IngressStreamState> {
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
        class: &StreamErrorClass,
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
                "type": class.openai_type,
                "code": class.openai_code,
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
        let r_is_null = obj.get("reasoning").is_none_or(serde_json::Value::is_null);
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
    if !obj.contains_key("max_tokens")
        && let Some(v) = mct_val
    {
        obj.insert("max_tokens".into(), v);
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

/// Promote the top-level OpenAI `reasoning_effort` string into the
/// canonical `reasoning.effort` field before serde deserialization,
/// then remove the top-level key.
///
/// Rules (mirror `normalize_max_completion_tokens`, where the explicit
/// canonical key wins):
/// - No `reasoning_effort`, or it is not a string: no-op.
/// - `reasoning` absent: create `{"effort": <reasoning_effort>}`.
/// - `reasoning` present without `effort`: fill its `effort`, leaving
///   sibling fields (`max_tokens`, `exclude`, `enabled`) untouched.
/// - `reasoning.effort` already set: the explicit nested value wins;
///   just drop `reasoning_effort`.
fn normalize_reasoning_effort(body: &mut Value) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    if !obj.get("reasoning_effort").is_some_and(Value::is_string) {
        // Leave a non-string `reasoning_effort` in place so the sweep
        // forwards it verbatim and serde surfaces any shape error.
        return;
    }
    let effort = obj.remove("reasoning_effort");
    if let Some(Value::Object(reasoning)) = obj.get_mut("reasoning") {
        reasoning.entry("effort").or_insert_with(|| {
            effort.expect("reasoning_effort confirmed present as a string above")
        });
    } else {
        let mut reasoning = Map::new();
        reasoning.insert(
            "effort".into(),
            effort.expect("reasoning_effort confirmed present as a string above"),
        );
        obj.insert("reasoning".into(), Value::Object(reasoning));
    }
}

/// Strip `matched_stop_sequence` from every `Choice` in a `ChatResponse`.
/// This is an Anthropic-internal field set by the egress to thread the
/// matched stop sequence back to the Anthropic ingress via the canonical
/// layer. OpenAI Chat-Completions clients do not expect it and some SDKs
/// error or forward it unexpectedly to callers.
fn strip_matched_stop_sequence_from_response(mut resp: ChatResponse) -> ChatResponse {
    for choice in &mut resp.choices {
        choice.matched_stop_sequence = None;
    }
    resp
}

/// Strip `matched_stop_sequence` from every `ChunkChoice` in a `ChatChunk`.
/// Same rationale as `strip_matched_stop_sequence_from_response`: OpenAI
/// streaming clients do not expect this Anthropic-internal field.
fn strip_matched_stop_sequence_from_chunk(mut chunk: ChatChunk) -> ChatChunk {
    for choice in &mut chunk.choices {
        choice.matched_stop_sequence = None;
    }
    chunk
}

/// Mirror the canonical Anthropic-vocabulary `usage.cache_read_input_tokens`
/// into the standard OpenAI `usage.prompt_tokens_details.cached_tokens` on
/// the serialized response body. Runs on the already-serialized `Value`
/// (rather than the canonical type) so this stays an OpenAI-dialect
/// concern: the Anthropic ingress must keep the Anthropic names.
///
/// Only acts when `cache_read_input_tokens` is present and non-zero; a
/// zero or absent value leaves the usage object untouched (no empty
/// `prompt_tokens_details`). If an upstream already supplied
/// `prompt_tokens_details`, only the `cached_tokens` sub-key is set,
/// preserving any sibling fields. `cache_read_input_tokens` is left in
/// place (additive).
fn surface_cached_tokens_in_usage(value: &mut Value) {
    let Some(usage) = value.get_mut("usage").and_then(|u| u.as_object_mut()) else {
        return;
    };
    let cached = usage
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if cached == 0 {
        return;
    }
    let details = usage
        .entry("prompt_tokens_details")
        .or_insert_with(|| Value::Object(Map::new()));
    if let Some(obj) = details.as_object_mut() {
        obj.insert("cached_tokens".into(), Value::from(cached));
    }
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
    for choice in &mut resp.choices {
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
    use crate::ingress::STREAM_ERROR_TYPE;
    use routectl_core::{
        Choice, ChunkChoice, ChunkDelta, Message, MessageContent, ReasoningConfig,
        ReasoningDetailKind, Role, SystemContent, Usage,
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
    fn parse_request_stamps_openai_provenance() {
        let body = json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let req = OpenAiIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        assert_eq!(
            req.routectl_internal.provenance,
            routectl_core::RequestProvenance::OpenaiIngress,
        );
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
            upstream_meta: None,
        };
        let mut state = OpenAiIngress.new_stream_state(&StreamRequestContext::default());
        let events = OpenAiIngress.render_chunk(chunk, state.as_mut()).unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].event.is_none());
        assert!(events[0].data.contains("\"content\":\"hello\""));
    }

    /// Ingress-layer serialization pin: the OpenAI ingress renders
    /// `chunk.model` verbatim into each streaming frame. The relabel
    /// itself happens upstream in the router (which rewrites `chunk.model`
    /// to the client-visible label -- requested alias by default, or a
    /// per-model `reported_model` override). This test proves only that
    /// whatever label the chunk carries is passed through unchanged into
    /// the serialized body; router-integration coverage lives in
    /// tests/router.rs and src/router.rs.
    #[test]
    fn render_chunk_surfaces_rewritten_chunk_model_label() {
        // Arrange: a chunk stamped with a client-visible label.
        let chunk = ChatChunk {
            id: "chatcmpl-1".into(),
            model: "public-label".into(),
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
            upstream_meta: None,
        };
        let mut state = OpenAiIngress.new_stream_state(&StreamRequestContext::default());

        // Act
        let events = OpenAiIngress.render_chunk(chunk, state.as_mut()).unwrap();

        // Assert: the serialized frame carries the canonical chunk.model.
        assert_eq!(events.len(), 1);
        let payload: Value = serde_json::from_str(&events[0].data).unwrap();
        assert_eq!(payload["model"], "public-label");
    }

    #[test]
    fn render_eos_emits_done_sentinel() {
        let mut state = OpenAiIngress.new_stream_state(&StreamRequestContext::default());
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
        let mut state = OpenAiIngress.new_stream_state(&StreamRequestContext::default());
        let error_msg = "upstream stream error (HTTP 529)";
        let class =
            StreamErrorClass::from_error(&routectl_core::Error::Streaming("render failure".into()));

        // Act
        let events = OpenAiIngress.render_error_eos(state.as_mut(), &error_msg, &class);

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
        assert!(
            payload["error"]["message"]
                .as_str()
                .unwrap()
                .contains("upstream stream error")
        );
        // Second: the universal `[DONE]` terminator.
        assert!(events[1].event.is_none());
        assert_eq!(events[1].data, "[DONE]");
    }

    /// Layer D: a 503/529 upstream stream error must carry
    /// `overloaded_error` on the OpenAI terminal error chunk so stream
    /// and non-stream classification agree; a present upstream type is
    /// preferred over the status-derived guess.
    #[test]
    fn render_error_eos_openai_emits_overloaded_for_529() {
        // Arrange
        let mut state = OpenAiIngress.new_stream_state(&StreamRequestContext::default());
        let err = routectl_core::Error::upstream("p", 529, "overloaded");
        let class = StreamErrorClass::from_error(&err);

        // Act
        let events = OpenAiIngress.render_error_eos(state.as_mut(), &"boom", &class);

        // Assert
        let payload: Value = serde_json::from_str(&events[0].data).unwrap();
        assert_eq!(payload["error"]["type"], "overloaded_error");
    }

    /// Layer D: when the upstream supplied its own classifier, the
    /// OpenAI terminal error chunk surfaces it verbatim on type/code.
    #[test]
    fn render_error_eos_openai_prefers_upstream_type_and_code() {
        // Arrange
        let mut state = OpenAiIngress.new_stream_state(&StreamRequestContext::default());
        let err = routectl_core::Error::upstream_full(
            "p",
            429,
            "rate limited",
            None,
            Some("rate_limit_exceeded".into()),
            Some("rate_limited".into()),
        );
        let class = StreamErrorClass::from_error(&err);

        // Act
        let events = OpenAiIngress.render_error_eos(state.as_mut(), &"boom", &class);

        // Assert
        let payload: Value = serde_json::from_str(&events[0].data).unwrap();
        assert_eq!(payload["error"]["type"], "rate_limit_exceeded");
        assert_eq!(payload["error"]["code"], "rate_limited");
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
        let mut state = OpenAiIngress.new_stream_state(&StreamRequestContext::default());
        let dirty = "upstream stream error\r\n\x1b[31mexploit\x1b[0m";
        let class =
            StreamErrorClass::from_error(&routectl_core::Error::Streaming("render failure".into()));

        // Act
        let events = OpenAiIngress.render_error_eos(state.as_mut(), &dirty, &class);

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
            upstream_meta: None,
        };
        let v = OpenAiIngress.render_response(resp).unwrap();
        assert_eq!(v["id"], "chatcmpl-1");
        assert_eq!(v["routectl_provider"], "test");
        // Suppress unused-import warnings.
        let _ = MessageContent::Text(String::new());
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
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
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
            upstream_meta: None,
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

    /// The OpenAI ingress renders `resp.model` verbatim. Since the
    /// router rewrites `resp.model` to the client-visible label
    /// (requested alias by default, or the per-model `reported_model`
    /// override), the rendered body surfaces that label rather than the
    /// upstream wire id. Pin that the ingress passes it through.
    #[test]
    fn render_response_surfaces_router_model_label_verbatim() {
        // Arrange: a response stamped with a client-visible label.
        let mut resp = response_with_choices(vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: routectl_core::Role::Assistant,
                content: MessageContent::Text("ok".into()),
                reasoning: None,
                reasoning_details: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }]);
        resp.model = "public-label".into();

        // Act
        let v = OpenAiIngress.render_response(resp).unwrap();

        // Assert
        assert_eq!(v["model"], "public-label");
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
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
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
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
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
            upstream_meta: None,
        };
        let mut state = OpenAiIngress.new_stream_state(&StreamRequestContext::default());

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

    /// Vanilla OpenAI clients set thinking via a top-level
    /// `reasoning_effort` string. The ingress must promote it into
    /// canonical `reasoning.effort` and drop the top-level key, so the
    /// effort survives serde and never leaks into the egress body.
    #[test]
    fn ingress_promotes_top_level_reasoning_effort_to_reasoning() {
        // Arrange
        let body = json!({
            "model": "claude-sonnet-4",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "high"
        });

        // Act
        let req = OpenAiIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();

        // Assert: canonical effort set.
        assert_eq!(req.reasoning.unwrap().effort.as_deref(), Some("high"));
        // No leftover top-level key swept into provider_extras.
        let leaked = req
            .provider_extras
            .as_ref()
            .and_then(|v| v.get("reasoning_effort"));
        assert!(
            leaked.is_none(),
            "reasoning_effort must not leak: {leaked:?}"
        );
    }

    /// Precedence: when both the top-level `reasoning_effort` and a
    /// nested `reasoning.effort` are present, the explicit nested value
    /// wins and the top-level key is dropped.
    #[test]
    fn ingress_reasoning_effort_explicit_object_wins() {
        // Arrange
        let body = json!({
            "model": "claude-sonnet-4",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "low",
            "reasoning": {"effort": "high"}
        });

        // Act
        let req = OpenAiIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();

        // Assert
        assert_eq!(req.reasoning.unwrap().effort.as_deref(), Some("high"));
    }

    /// Promotion must not clobber sibling `reasoning` fields: a body with
    /// `reasoning_effort` AND `reasoning.max_tokens` keeps both.
    #[test]
    fn ingress_reasoning_effort_preserves_reasoning_siblings() {
        // Arrange
        let body = json!({
            "model": "claude-sonnet-4",
            "messages": [{"role": "user", "content": "hi"}],
            "reasoning_effort": "medium",
            "reasoning": {"max_tokens": 2048}
        });

        // Act
        let req = OpenAiIngress
            .parse_request(&HeaderMap::new(), body)
            .unwrap();

        // Assert
        let reasoning = req.reasoning.unwrap();
        assert_eq!(reasoning.effort.as_deref(), Some("medium"));
        assert_eq!(reasoning.max_tokens, Some(2048));
    }

    /// o-series / gpt-5+ clients may send `role: "developer"` as the
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
                logprobs: None,
                index: 0,
                message: Message {
                    refusal: None,
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
            upstream_meta: None,
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
            upstream_meta: None,
        };
        let mut state = OpenAiIngress.new_stream_state(&StreamRequestContext::default());
        let events = OpenAiIngress.render_chunk(chunk, state.as_mut()).unwrap();
        assert_eq!(events.len(), 1);
        assert!(
            !events[0].data.contains("matched_stop_sequence"),
            "render_chunk must not emit matched_stop_sequence: {}",
            events[0].data
        );
    }

    /// An openai-compat upstream may return a safety refusal on
    /// `message.refusal` (with `content: null`) and a per-choice
    /// `logprobs` object. The OpenAI ingress re-serialize must emit both
    /// so the client still sees `message.refusal` and `choices[].logprobs`.
    #[test]
    fn render_response_preserves_refusal_and_logprobs() {
        // Arrange: canonical response as the openai-compat egress would
        // deserialize it from the upstream wire.
        let resp = ChatResponse {
            id: "chatcmpl-1".into(),
            model: "gpt-4o".into(),
            created: 1700000000,
            choices: vec![Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: MessageContent::Null,
                    reasoning: None,
                    reasoning_details: Vec::new(),
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    refusal: Some("I can't help with that.".into()),
                },
                finish_reason: Some("stop".into()),
                matched_stop_sequence: None,
                logprobs: Some(json!({"content": [{"token": "x", "logprob": -0.2}]})),
            }],
            usage: None,
            routectl_provider: None,
            extras: Default::default(),
            upstream_meta: None,
        };

        // Act
        let v = OpenAiIngress.render_response(resp).unwrap();

        // Assert
        let choice = &v["choices"][0];
        assert_eq!(choice["message"]["refusal"], "I can't help with that.");
        assert!(choice["message"]["content"].is_null());
        assert_eq!(choice["logprobs"]["content"][0]["token"], "x");
    }

    /// Anthropic/Bedrock cache-read counts arrive on canonical under the
    /// Anthropic-vocabulary `usage.cache_read_input_tokens`. The OpenAI
    /// ingress must ALSO surface them under the standard OpenAI
    /// `usage.prompt_tokens_details.cached_tokens` so an OpenAI cost
    /// tracker sees the cache hit. `cache_read_input_tokens` stays present.
    #[test]
    fn render_response_surfaces_cached_tokens_in_usage() {
        // Arrange
        let resp = ChatResponse {
            id: "chatcmpl-1".into(),
            model: "claude".into(),
            created: 1700000000,
            choices: vec![],
            usage: Some(Usage {
                prompt_tokens: 10000,
                completion_tokens: 50,
                total_tokens: 10050,
                cache_read_input_tokens: Some(8192),
                ..Default::default()
            }),
            routectl_provider: Some("anthropic-api".into()),
            extras: Default::default(),
            upstream_meta: None,
        };

        // Act
        let v = OpenAiIngress.render_response(resp).unwrap();

        // Assert: standard OpenAI field present and equal.
        assert_eq!(
            v["usage"]["prompt_tokens_details"]["cached_tokens"], 8192,
            "cached_tokens must mirror cache_read_input_tokens: {}",
            v["usage"]
        );
        // Anthropic-vocabulary field stays present (additive).
        assert_eq!(v["usage"]["cache_read_input_tokens"], 8192);
    }

    /// No cache tokens -> no `prompt_tokens_details` key at all (no empty
    /// object emitted).
    #[test]
    fn render_response_omits_prompt_tokens_details_without_cache() {
        // Arrange
        let resp = ChatResponse {
            id: "chatcmpl-1".into(),
            model: "gpt-4o".into(),
            created: 1700000000,
            choices: vec![],
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                ..Default::default()
            }),
            routectl_provider: None,
            extras: Default::default(),
            upstream_meta: None,
        };

        // Act
        let v = OpenAiIngress.render_response(resp).unwrap();

        // Assert
        assert!(
            v["usage"].get("prompt_tokens_details").is_none(),
            "no cache tokens must not emit prompt_tokens_details: {}",
            v["usage"]
        );
    }
}
