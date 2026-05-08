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

use axum::http::HeaderMap;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Result};
use serde_json::Value;

use super::{IngressAdapter, IngressStreamState, SseEvent};

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

    fn parse_request(&self, _headers: &HeaderMap, body: Value) -> Result<ChatRequest> {
        serde_json::from_value::<ChatRequest>(body).map_err(|e| {
            Error::Validation(format!(
                "openai ingress: invalid /v1/chat/completions body: {e}"
            ))
        })
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
            }],
            usage: None,
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
        let v = OpenAiIngress.render_response(resp).unwrap();
        assert_eq!(v["id"], "chatcmpl-1");
        assert_eq!(v["routectl_provider"], "test");
        // Suppress unused-import warnings.
        let _ = MessageContent::Text("".into());
    }
}
