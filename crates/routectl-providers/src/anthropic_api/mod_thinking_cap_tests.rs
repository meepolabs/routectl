//! The thinking-cache per-entry cap is a fixed 1 MiB on both provider write
//! paths: an entry whose serialized JSON is exactly 1 MiB is cached, one byte
//! more is not.
//!
//! The cap is spelled as a literal here, not read from the provider's
//! constant, so a change to the constant moves the boundary and fails a row.
//!
//! Declared on `mod.rs` via `#[cfg(test)] #[path = ...]`.

use super::*;
use routectl_core::{ChatRequest, Message, MessageContent, Role};
use serde_json::json;
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ONE_MIB: usize = 1024 * 1024;
const PROVIDER_ID: &str = "thinking-cap-test";
const TOOL_USE_ID: &str = "toolu_cap_01";
const SIGNATURE: &str = "sig-cap";

#[derive(Clone, Copy, Debug)]
enum WritePath {
    Complete,
    Stream,
}

/// The `thinking` text length whose cached entry serializes to `entry_bytes`.
/// Both write paths build the entry with `make_thinking_detail` from a v4 UUID
/// (always 36 chars) at index 0, so the envelope around the text is fixed.
fn text_len_for_entry_bytes(entry_bytes: usize) -> usize {
    let envelope = vec![context_management::make_thinking_detail(
        "0".repeat(36),
        0,
        String::new(),
        SIGNATURE.to_string(),
    )];
    let overhead = serde_json::to_vec(&envelope).expect("serialize").len();
    entry_bytes - overhead
}

fn client_req() -> ChatRequest {
    ChatRequest {
        model: "claude-3-opus".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extras: serde_json::Map::new(),
        }]
        .into(),
        max_tokens: Some(2048),
        ..Default::default()
    }
}

fn complete_body(thinking: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "msg_cap",
        "type": "message",
        "role": "assistant",
        "model": "claude-3-opus",
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 5, "output_tokens": 3},
        "content": [
            {"type": "thinking", "thinking": thinking, "signature": SIGNATURE},
            {"type": "tool_use", "id": TOOL_USE_ID, "name": "search", "input": {}}
        ]
    }))
}

fn sse_event(name: &str, data: &serde_json::Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

fn stream_body(thinking: &str) -> ResponseTemplate {
    let body = [
        sse_event(
            "message_start",
            &json!({"type": "message_start", "message": {"id": "msg_cap_sse",
                "model": "claude-3-opus", "usage": {"input_tokens": 5, "output_tokens": 0}}}),
        ),
        sse_event(
            "content_block_start",
            &json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "thinking", "thinking": ""}}),
        ),
        sse_event(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "thinking_delta", "thinking": thinking}}),
        ),
        sse_event(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": 0,
                "delta": {"type": "signature_delta", "signature": SIGNATURE}}),
        ),
        sse_event(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": 0}),
        ),
        sse_event(
            "content_block_start",
            &json!({"type": "content_block_start", "index": 1,
                "content_block": {"type": "tool_use", "id": TOOL_USE_ID, "name": "search"}}),
        ),
        sse_event(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": 1}),
        ),
        sse_event("message_stop", &json!({"type": "message_stop"})),
    ]
    .concat();
    ResponseTemplate::new(200)
        .set_body_string(body)
        .append_header("content-type", "text/event-stream")
}

async fn provider_after_response(path: WritePath, thinking: &str) -> AnthropicApiProvider {
    let server = MockServer::start().await;
    let template = match path {
        WritePath::Complete => complete_body(thinking),
        WritePath::Stream => stream_body(thinking),
    };
    Mock::given(method("POST"))
        .and(wm_path("/v1/messages"))
        .respond_with(template)
        .mount(&server)
        .await;
    let mut cfg = AnthropicApiConfig::new(PROVIDER_ID, "test-key");
    cfg.base_url = server.uri();
    cfg.context_management = true;
    let provider = AnthropicApiProvider::new(cfg);
    match path {
        WritePath::Complete => {
            provider.complete(client_req()).await.expect("complete");
        }
        WritePath::Stream => {
            use futures::StreamExt;
            let mut req = client_req();
            req.stream = Some(true);
            let mut stream = provider.stream(req).await.expect("stream opens");
            while let Some(chunk) = stream.next().await {
                chunk.expect("chunk parses");
            }
        }
    }
    provider
}

#[tokio::test(flavor = "current_thread")]
async fn a_thinking_entry_at_one_mib_is_cached_and_one_byte_more_is_not() {
    let rows = [
        (WritePath::Complete, ONE_MIB, true),
        (WritePath::Complete, ONE_MIB + 1, false),
        (WritePath::Stream, ONE_MIB, true),
        (WritePath::Stream, ONE_MIB + 1, false),
    ];
    for (path, entry_bytes, cached) in rows {
        let thinking = "a".repeat(text_len_for_entry_bytes(entry_bytes));

        let provider = provider_after_response(path, &thinking).await;

        let stored =
            context_management::lookup_thinking(&provider.thinking_cache, PROVIDER_ID, TOOL_USE_ID);
        assert_eq!(
            stored.is_some(),
            cached,
            "row {path:?} entry_bytes={entry_bytes}: cached must be {cached}"
        );
        if let Some(stored) = stored {
            let measured = serde_json::to_vec(&stored).expect("serialize").len();
            assert_eq!(
                measured, entry_bytes,
                "row {path:?}: the stored entry must measure exactly the planned size"
            );
        }
    }
}
