//! Contract tests for the native Gemini egress.
//!
//! Mirrors the `anthropic_api` / `openai_compat` contract suite for the
//! `gemini` provider across the three egress directions:
//!
//!   - canonical `ChatRequest` -> Gemini wire body (`normalize_request`),
//!   - Gemini wire response -> canonical `ChatResponse`
//!     (`normalize_response`),
//!   - Gemini `streamGenerateContent` SSE sequence -> canonical
//!     `ChatChunk` stream (`stream()` driven over a `wiremock` server).
//!
//! Snapshots live under `tests/snapshots/gemini/`. Two determinism notes:
//!
//!   - `ChatResponse.created` is a wall-clock timestamp (`Utc::now()`);
//!     it is pinned to `0` before snapshotting. The workspace `insta`
//!     build has no `redactions` feature, so field pinning is done by
//!     hand rather than via a redaction selector.
//!   - Gemini stream chunks carry deterministic ids (`resp-*` from the
//!     fixture, `call_<index>` for tool calls), so the SSE chunk
//!     sequence snapshots as-is.

#![cfg(feature = "gemini")]

mod common;

use futures::StreamExt;
use routectl_core::{
    ChatChunk, ChatRequest, Message, MessageContent, Provider, ReasoningDetail,
    ReasoningDetailKind, Role,
};
use routectl_providers::gemini::{GeminiConfig, GeminiProvider};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::scenarios;

// ---------------------------------------------------------------------
// Provider builders
// ---------------------------------------------------------------------

fn gemini_provider() -> GeminiProvider {
    GeminiProvider::new(GeminiConfig::new("gemini-test", "test-key"))
}

fn gemini_provider_at(base_url: &str) -> GeminiProvider {
    let mut cfg = GeminiConfig::new("gemini-test", "test-key");
    cfg.base_url = base_url.to_string();
    GeminiProvider::new(cfg)
}

// =====================================================================
// Request side: canonical ChatRequest -> Gemini wire body
// =====================================================================

// Scenario 1: a top-level system prompt lifts into `systemInstruction`
// (no role), not a chat turn -- the shape Gemini expects.
mod scenario_1_system_handling {
    use super::*;

    #[test]
    fn gemini_egress() {
        let req = scenarios::scenario_1_system_handling();
        let body = gemini_provider()
            .normalize_request(&req)
            .expect("gemini normalize");

        let si = body
            .get("systemInstruction")
            .expect("systemInstruction present");
        assert!(
            si.get("role").is_none(),
            "systemInstruction must carry no role; got: {si}"
        );

        insta::with_settings!({snapshot_path => "snapshots/gemini"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

// Scenario 2: one custom tool + `tool_choice: "auto"` -> native
// `tools[].functionDeclarations[]` + `toolConfig`.
mod scenario_2_tool_choice_auto {
    use super::*;

    #[test]
    fn gemini_egress() {
        let req = scenarios::scenario_2_tool_choice_auto();
        let body = gemini_provider()
            .normalize_request(&req)
            .expect("gemini normalize");

        insta::with_settings!({snapshot_path => "snapshots/gemini"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

// Scenario 3: a tool-round-trip history. The tool result lands as a
// user-turn `functionResponse` part -- Gemini has no dedicated tool role.
mod scenario_3_multi_turn_with_tool_result {
    use super::*;

    #[test]
    fn gemini_egress() {
        let req = scenarios::scenario_3_multi_turn_with_tool_result();
        let body = gemini_provider()
            .normalize_request(&req)
            .expect("gemini normalize");

        let contents = body["contents"].as_array().expect("contents array");
        let has_fn_response_user = contents.iter().any(|c| {
            c["role"] == "user"
                && c["parts"]
                    .as_array()
                    .is_some_and(|ps| ps.iter().any(|p| p.get("functionResponse").is_some()))
        });
        assert!(
            has_fn_response_user,
            "tool result must map to a user-turn functionResponse; body: {body}"
        );

        insta::with_settings!({snapshot_path => "snapshots/gemini"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

// thoughtSignature replay: a Gemini-origin assistant reasoning_detail is
// replayed as a `thought` part (carrying the opaque signature) ahead of
// the visible answer, so multi-turn chain-of-thought continues.
mod thought_signature_replay {
    use super::*;

    // Must match `gemini::GEMINI_FORMAT` (crate-private, so hardcoded
    // here). A mismatch drops the replay and changes the snapshot, so a
    // rename is caught by this test failing rather than passing silently.
    const GEMINI_FORMAT: &str = "gemini-v1";

    #[test]
    fn gemini_egress() {
        let assistant = Message {
            refusal: None,
            role: Role::Assistant,
            content: MessageContent::Text("The answer is 42.".into()),
            reasoning: None,
            reasoning_details: vec![ReasoningDetail {
                kind: ReasoningDetailKind::Text,
                id: None,
                format: Some(GEMINI_FORMAT.into()),
                index: Some(0),
                payload: json!({"text": "6 * 7 = 42", "thought_signature": "sig-abc"}),
            }],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };
        let req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![
                common::user_msg("What is 6 times 7?"),
                assistant,
                common::user_msg("And 6 times 8?"),
            ],
            max_tokens: Some(256),
            ..Default::default()
        };

        let body = gemini_provider()
            .normalize_request(&req)
            .expect("gemini normalize");

        // Sanity-pin the replayed thought part before the snapshot so a
        // regression that drops the signature fails clearly.
        let model_turn = body["contents"]
            .as_array()
            .expect("contents array")
            .iter()
            .find(|c| c["role"] == "model")
            .expect("model turn present");
        let thought = model_turn["parts"]
            .as_array()
            .expect("parts array")
            .iter()
            .find(|p| p.get("thought") == Some(&json!(true)))
            .expect("model turn must carry a replayed thought part");
        assert_eq!(
            thought["thoughtSignature"].as_str(),
            Some("sig-abc"),
            "replayed thought part must carry the thoughtSignature verbatim"
        );

        insta::with_settings!({snapshot_path => "snapshots/gemini"}, {
            insta::assert_json_snapshot!("request_body", body);
        });
    }
}

// =====================================================================
// Response side: Gemini wire response -> canonical ChatResponse
// =====================================================================

// A text response with a full usageMetadata block: cachedContentTokenCount
// lifts to `cache_read_input_tokens`, thoughtsTokenCount to
// `reasoning_tokens`.
mod response_text_and_usage {
    use super::*;

    #[test]
    fn gemini_egress() {
        let raw = json!({
            "candidates": [{
                "content": {"parts": [{"text": "Hello!"}], "role": "model"},
                "finishReason": "STOP",
                "index": 0
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "totalTokenCount": 15,
                "cachedContentTokenCount": 4,
                "thoughtsTokenCount": 3
            },
            "modelVersion": "gemini-2.5-pro-001",
            "responseId": "resp-contract-1"
        });

        let mut resp = gemini_provider()
            .normalize_response(raw)
            .expect("gemini normalize_response");
        // Utc::now() timestamp -- pin for a deterministic snapshot.
        resp.created = 0;

        insta::with_settings!({snapshot_path => "snapshots/gemini"}, {
            insta::assert_json_snapshot!("response", resp);
        });
    }
}

// A response carrying a `thought` part (with thoughtSignature) plus visible
// text: the thought becomes a `reasoning_details[]` entry (format tag
// `gemini-v1`), the visible text becomes the message content.
mod response_thought_replay {
    use super::*;

    #[test]
    fn gemini_egress() {
        let raw = json!({
            "candidates": [{
                "content": {"parts": [
                    {"text": "let me think", "thought": true, "thoughtSignature": "sig-42"},
                    {"text": "the answer"}
                ], "role": "model"},
                "finishReason": "STOP",
                "index": 0
            }],
            "modelVersion": "gemini-2.5-pro-001",
            "responseId": "resp-contract-2"
        });

        let mut resp = gemini_provider()
            .normalize_response(raw)
            .expect("gemini normalize_response");
        resp.created = 0;

        insta::with_settings!({snapshot_path => "snapshots/gemini"}, {
            insta::assert_json_snapshot!("response", resp);
        });
    }
}

// =====================================================================
// Stream side: Gemini SSE sequence -> canonical ChatChunk stream
// =====================================================================

// A three-event SSE stream: a thought delta (with signature), a visible
// text delta, then a terminal partial carrying finishReason + usage.
mod stream_sequence {
    use super::*;

    const fn gemini_sse_body() -> &'static str {
        concat!(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"let me think\",\"thought\":true,\"thoughtSignature\":\"sig-42\"}],\"role\":\"model\"},\"index\":0}],\"responseId\":\"resp-stream-1\",\"modelVersion\":\"gemini-2.5-pro-001\"}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"Hello\"}],\"role\":\"model\"},\"index\":0}]}\n\n",
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\" world\"}],\"role\":\"model\"},\"finishReason\":\"STOP\",\"index\":0}],\"usageMetadata\":{\"promptTokenCount\":5,\"candidatesTokenCount\":2,\"totalTokenCount\":7,\"thoughtsTokenCount\":1}}\n\n",
        )
    }

    #[tokio::test]
    async fn gemini_egress() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/models/gemini-2.5-pro:streamGenerateContent"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(gemini_sse_body()),
            )
            .mount(&server)
            .await;

        let provider = gemini_provider_at(&server.uri());
        let req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![common::user_msg("hi")],
            max_tokens: Some(64),
            stream: Some(true),
            ..Default::default()
        };

        let mut stream = provider.stream(req).await.expect("stream open");
        let mut chunks: Vec<ChatChunk> = Vec::new();
        while let Some(item) = stream.next().await {
            chunks.push(item.expect("chunk decoded without error"));
        }

        insta::with_settings!({snapshot_path => "snapshots/gemini"}, {
            insta::assert_json_snapshot!("stream_chunks", chunks);
        });
    }
}
