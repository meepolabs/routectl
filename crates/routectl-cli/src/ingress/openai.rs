//! OpenAI Chat Completions ingress (`POST /v1/chat/completions`).
//!
//! The OpenAI dialect IS the canonical wire shape: the existing routectl
//! v0.3 server expected callers to POST a `ChatRequest`-shaped JSON body.
//! v0.4.0 keeps that exactly the same -- this adapter is a thin wrapper
//! that satisfies the `IngressAdapter` trait without changing semantics.
//!
//! Streaming convention: OpenAI emits a sequence of bare `data: <json>`
//! frames followed by `data: [DONE]`. No named events.

use std::any::Any;
use std::collections::BTreeMap;

use axum::http::HeaderMap;
use routectl_core::{
    is_canonical_request_key, ChatChunk, ChatRequest, ChatResponse, Error, MessageContent, Result,
    Role, SystemContent,
};
use serde_json::{Map, Value};

use super::{resolve_alias, IngressAdapter, IngressStreamState, SseEvent};

const DONE_SENTINEL: &str = "[DONE]";

#[derive(Debug, Default)]
pub struct OpenAiIngress {
    /// Map from wire `model` field value to a configured alias. The
    /// `x-routectl-alias` header overrides this. Empty by default
    /// (loopback dev / direct testing).
    pub aliases: BTreeMap<String, String>,
}

impl OpenAiIngress {
    pub fn new(aliases: BTreeMap<String, String>) -> Self {
        Self { aliases }
    }
}

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

    fn parse_request(&self, headers: &HeaderMap, body: Value) -> Result<ChatRequest> {
        // FR-1: trace-level ingress body for triage. Inherits the
        // parent span's `request_id` so a `grep request_id=<id>`
        // shows ingress -> outgoing -> upstream -> egress in one
        // pass. Gated by `tracing::Level::TRACE`; default `info`
        // level pays nothing. Honors ROUTECTL_LOG_REDACT_PROMPTS=1.
        routectl_core::trace_ingress_body("openai", &body);
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
        req.model = resolve_alias(&self.aliases, headers, &req.model);
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
        serde_json::to_value(&resp)
            .map_err(|e| Error::Config(format!("openai ingress: serialize response: {e}")))
    }

    fn new_stream_state(&self) -> Box<dyn IngressStreamState> {
        Box::new(OpenAiStreamState)
    }

    fn render_chunk(
        &self,
        chunk: ChatChunk,
        _state: &mut dyn IngressStreamState,
    ) -> Result<Vec<SseEvent>> {
        let data = serde_json::to_string(&chunk)
            .map_err(|e| Error::Config(format!("openai ingress: serialize chunk: {e}")))?;
        Ok(vec![SseEvent::unnamed(data)])
    }

    fn render_eos(&self, _state: &mut dyn IngressStreamState) -> Vec<SseEvent> {
        vec![SseEvent::unnamed(DONE_SENTINEL)]
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

/// If any `Role::System` messages are in `req.messages`, concatenate
/// their text content with newlines into `req.system` (preserving any
/// existing `req.system` value) and remove them from the messages
/// array. No-op when there are no System messages.
fn lift_system_messages(req: &mut ChatRequest) {
    let mut lifted: Vec<String> = Vec::new();
    req.messages.retain(|m| {
        if !matches!(m.role, Role::System) {
            return true;
        }
        match &m.content {
            MessageContent::Text(t) => lifted.push(t.clone()),
            MessageContent::Parts(parts) => {
                // Collect text parts only -- images/documents in a
                // System message are not meaningful in canonical and
                // would have been dropped by the egress anyway.
                for p in parts {
                    if let routectl_core::ContentPart::Known(
                        routectl_core::KnownContentPart::Text { text, .. },
                    ) = p
                    {
                        lifted.push(text.clone());
                    }
                }
            }
            MessageContent::Null => {}
        }
        false
    });

    if lifted.is_empty() {
        return;
    }
    let lifted_text = lifted.join("\n");
    match req.system.take() {
        Some(SystemContent::Text(existing)) => {
            req.system = Some(SystemContent::Text(format!("{existing}\n{lifted_text}")));
        }
        Some(SystemContent::Blocks(mut blocks)) => {
            blocks.push(routectl_core::SystemBlock {
                kind: "text".into(),
                text: lifted_text,
                cache_control: None,
                citations: None,
            });
            req.system = Some(SystemContent::Blocks(blocks));
        }
        None => {
            req.system = Some(SystemContent::Text(lifted_text));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{
        ChunkChoice, ChunkDelta, MessageContent, ReasoningConfig, ReasoningDetailKind,
    };
    use serde_json::json;

    #[test]
    fn parse_request_accepts_canonical_body() {
        let body = json!({
            "model": "gpt-4o",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        });
        let req = OpenAiIngress::default()
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
        let req = OpenAiIngress::default()
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
        let err = OpenAiIngress::default()
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
            }],
            usage: None,
        };
        let mut state = OpenAiIngress::default().new_stream_state();
        let events = OpenAiIngress::default()
            .render_chunk(chunk, state.as_mut())
            .unwrap();
        assert_eq!(events.len(), 1);
        assert!(events[0].event.is_none());
        assert!(events[0].data.contains("\"content\":\"hello\""));
    }

    #[test]
    fn render_eos_emits_done_sentinel() {
        let mut state = OpenAiIngress::default().new_stream_state();
        let events = OpenAiIngress::default().render_eos(state.as_mut());
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "[DONE]");
        assert!(events[0].event.is_none());
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
        let req = OpenAiIngress::default()
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
        let req = OpenAiIngress::default()
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
        let req = OpenAiIngress::default()
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
        // the original Value preserved verbatim. Round-1 of the
        // dogfood fix had the ingress lift this to `ToolDef::Custom`,
        // which broke the openai-compat egress path: `Custom`
        // serializes flat (no `type:"function"` wrapper) so DeepSeek
        // 400'd. The Anthropic egress's `translate_tool` already
        // converts function-shape `Other` to `AnthropicTool::Custom`,
        // so dropping the ingress lift loses nothing on that path.
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
        let req = OpenAiIngress::default()
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
        let req = OpenAiIngress::default()
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
        // carried. Round-1 of the dogfood fix mistakenly translated
        // here, breaking openai-compat egresses (DeepSeek 400'd on
        // an Anthropic-shape tool_choice). Pin the contract.
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
            let req = OpenAiIngress::default()
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
        };
        let v = OpenAiIngress::default().render_response(resp).unwrap();
        assert_eq!(v["id"], "chatcmpl-1");
        assert_eq!(v["routectl_provider"], "test");
        // Suppress unused-import warnings.
        let _ = MessageContent::Text("".into());
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
        let req = OpenAiIngress::default()
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
        let req = OpenAiIngress::default()
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
        let req = OpenAiIngress::default()
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
        let req = OpenAiIngress::default()
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        assert!(req.messages[0].reasoning.is_none());
    }
}
