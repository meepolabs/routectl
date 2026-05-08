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
    ChatChunk, ChatRequest, ChatResponse, Error, MessageContent, Result, Role, SystemContent,
};
use serde_json::Value;

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
        let mut req: ChatRequest = serde_json::from_value(body).map_err(|e| {
            Error::Validation(format!(
                "openai ingress: invalid /v1/chat/completions body: {e}"
            ))
        })?;
        req.model = resolve_alias(&self.aliases, headers, &req.model);
        // Honor the canonical contract: `req.system` is the source of
        // truth at egress time. Lift any Role::System messages into
        // `req.system` here at ingress so every egress reads the same
        // shape. Concat with newlines when multiple system messages
        // are present (matching the legacy lift-from-egress behavior).
        lift_system_messages(&mut req);
        // Translate OpenAI function tools (`{type: "function", function:
        // {...}}`) into canonical `ToolDef::Custom` so all egresses see
        // the canonical tool representation. Builtin / unknown tool
        // shapes pass through as `ToolDef::Other`.
        lift_openai_function_tools(&mut req);
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

/// Walk `req.tools` and translate `ToolDef::Other` entries that match
/// the OpenAI function-tool shape (`{type: "function", function:
/// {name, description?, parameters?, strict?}}`) into `ToolDef::Custom`.
/// Other `ToolDef::Other` shapes (Anthropic builtins, server-side tools,
/// forward-compat) pass through verbatim.
fn lift_openai_function_tools(req: &mut ChatRequest) {
    let Some(tools) = req.tools.as_mut() else {
        return;
    };
    for tool in tools.iter_mut() {
        if let routectl_core::ToolDef::Other(v) = tool {
            if let Some(custom) = routectl_core::CustomTool::from_openai_function(v) {
                *tool = routectl_core::ToolDef::Custom(custom);
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
    fn openai_ingress_lifts_function_tools_into_custom() {
        // OpenAI tool wire shape `{type: "function", function: {...}}`
        // must arrive in canonical as `ToolDef::Custom`. Without the
        // ingress translation it would land in `ToolDef::Other` (since
        // the type discriminator is "function", not "custom") and miss
        // the canonical typed surface.
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
            .parse_request(&HeaderMap::new(), body)
            .unwrap();
        let tools = req.tools.expect("tools present");
        assert_eq!(tools.len(), 1);
        match &tools[0] {
            routectl_core::ToolDef::Custom(c) => {
                assert_eq!(c.name, "get_weather");
                assert_eq!(c.description.as_deref(), Some("Get current weather"));
                assert_eq!(c.strict, Some(true));
                assert!(c.input_schema.is_object());
            }
            other => panic!("expected ToolDef::Custom, got {other:?}"),
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
}
