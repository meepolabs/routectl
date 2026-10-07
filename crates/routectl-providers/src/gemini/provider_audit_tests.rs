use crate::gemini::{
    GeminiConfig, GeminiProvider,
    sse::{GeminiStreamState, parse_data_line},
};
use crate::provider_audit_tests::*;
use futures::StreamExt;
use routectl_core::{ChatChunk, ChatRequest};
use routectl_core::{Error, Provider};
use serde_json::{Value, json};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

fn normalized(req: &ChatRequest) -> Value {
    let mut cfg = GeminiConfig::new("gemini:audit", "synthetic-test-key");
    cfg.base_url = "http://127.0.0.1:9".into();
    GeminiProvider::new(cfg).normalize_request(req).unwrap()
}

#[test]
fn public_normalize_image_urls_are_utf8_safe_and_data_schemes_remain_case_insensitive() {
    for url in [
        "",
        "a",
        "abcd",
        "abcde",
        "abcd\u{e9}",
        "abc\u{e9}",
        "\u{1f642}",
        "\u{1f642}a",
        "\u{1f642}\u{1f642}",
        "https://example.test/\u{e9}",
    ] {
        let req = request(
            json!([{"role":"user", "content":[{"type":"image_url", "image_url":{"url":url}}]}]),
        );
        let body = normalized(&req);
        assert_eq!(body["contents"][0]["parts"][0]["text"], url);
    }
    for prefix in ["data:", "DATA:", "DaTa:"] {
        let req = request(
            json!([{"role":"user", "content":[{"type":"image_url", "image_url":{"url":format!("{prefix}image/png;base64,aGVsbG8=")}}]}]),
        );
        let body = normalized(&req);
        assert_eq!(
            body["contents"][0]["parts"][0]["inlineData"]["mimeType"],
            "image/png"
        );
        assert!(body["contents"][0]["parts"][0].get("text").is_none());
        let req = request(
            json!([{"role":"user", "content":[{"type":"image_url", "image_url":{"url":format!("{prefix}broken")}}]}]),
        );
        assert_eq!(normalized(&req)["contents"], json!([]));
    }
}

#[test]
fn gemini_replayed_duplicate_ids_resolve_chronologically_in_both_tool_shapes() {
    let req = request(json!([
        {"role":"tool", "tool_call_id":"same", "content":"before any call"},
        {"role":"assistant", "tool_calls":[{"id":"same", "function":{"name":"alpha", "arguments":"{}"}}]},
        {"role":"tool", "tool_call_id":"same", "content":"first"},
        {"role":"assistant", "content":[{"type":"tool_use", "id":"same", "name":"beta", "input":{}}]},
        {"role":"user", "content":[{"type":"tool_result", "tool_use_id":"same", "content":"second"}]},
        {"role":"tool", "tool_call_id":"same", "name":"beta", "content":"carried"},
        {"role":"tool", "tool_call_id":"orphan", "name":"explicit", "content":"name-only fallback"},
        {"role":"assistant", "tool_calls":[{"id":"same", "function":{"name":"future", "arguments":"{}"}}]}
    ]));
    let body = normalized(&req);
    for (turn, expected) in [
        (0, ""),
        (2, "alpha"),
        (4, "beta"),
        (5, "beta"),
        (6, "explicit"),
    ] {
        assert_eq!(
            body["contents"][turn]["parts"][0]["functionResponse"]["name"],
            expected
        );
    }
}

#[test]
fn gemini_two_streamed_turns_mint_distinct_ids_and_replay_correct_names() {
    let mut messages = Vec::new();
    let mut ids = Vec::new();
    for name in ["alpha", "beta"] {
        let mut state = GeminiStreamState::default();
        let ev = json!({"responseId":"replayed-response-id", "candidates":[{"content":{"parts":[
            {"functionCall":{"name":name, "args":{"x":1}}, "thoughtSignature":"native-signature"}
        ]}, "finishReason":"STOP"}]});
        let mut sdk = SdkAccumulator::default();
        for chunk in state
            .parse_event("p", parse_data_line("p", &ev.to_string()).unwrap())
            .unwrap()
        {
            sdk.push(&chunk);
        }
        state.on_eos("p").unwrap();
        let call = &sdk.0[&(0, 0)];
        ids.push(call.id.clone());
        messages.push(json!({"role":"assistant", "tool_calls":[{"id":call.id,
            "function":{"name":call.name, "arguments":call.arguments}, "thought_signature":"native-signature"}]}));
        messages.push(json!({"role":"tool", "tool_call_id":call.id, "content":"ok"}));
    }
    assert_ne!(ids[0], ids[1]);
    let body = normalized(&request(json!(messages)));
    assert_eq!(
        body["contents"][1]["parts"][0]["functionResponse"]["name"],
        "alpha"
    );
    assert_eq!(
        body["contents"][3]["parts"][0]["functionResponse"]["name"],
        "beta"
    );
}

async fn stream_body(body: String) -> Vec<routectl_core::Result<ChatChunk>> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let mut cfg = GeminiConfig::new("gemini:audit", "synthetic-test-key");
    cfg.base_url = server.uri();
    GeminiProvider::new(cfg)
        .stream(request(json!([{"role":"user", "content":"hi"}])))
        .await
        .unwrap()
        .collect()
        .await
}

#[tokio::test]
async fn gemini_unspecified_finish_does_not_terminate_or_drop_continued_content() {
    for reason in ["", "FINISH_REASON_UNSPECIFIED"] {
        let first =
            json!({"candidates":[{"content":{"parts":[{"text":"first"}]}, "finishReason":reason}]});
        let chunks = stream_body(format!("data: {first}\n\n")).await;
        assert!(matches!(
            chunks.last(),
            Some(Err(Error::Upstream { status: 0, .. }))
        ));
        let last = json!({"candidates":[{"content":{"parts":[{"text":"second"}]}, "finishReason":"STOP"}]});
        let chunks = stream_body(format!("data: {first}\n\ndata: {last}\n\n")).await;
        assert!(chunks.iter().all(|c| c.is_ok()));
        let text: String = chunks
            .iter()
            .filter_map(|c| c.as_ref().ok())
            .flat_map(|c| &c.choices)
            .filter_map(|c| c.delta.content.as_deref())
            .collect();
        assert_eq!(text, "firstsecond");
    }
}

#[tokio::test]
async fn gemini_clean_eof_without_semantic_terminal_errors_even_after_content_or_usage() {
    for event in [
        None,
        Some(json!({"candidates":[{"content":{"parts":[{"text":"visible"}]}}]})),
        Some(
            json!({"candidates":[{"content":{"parts":[{"functionCall":{"name":"alpha", "args":{"x":1}}}]}}]}),
        ),
        Some(json!({"usageMetadata":{"totalTokenCount":7}})),
        Some(json!({"promptFeedback":{"blockReason":"BLOCK_REASON_UNSPECIFIED"}})),
    ] {
        let wire = event.map(|e| format!("data: {e}\n\n")).unwrap_or_default();
        let items = stream_body(wire).await;
        assert_eq!(items.iter().filter(|i| i.is_err()).count(), 1);
        assert!(matches!(
            items.last(),
            Some(Err(Error::Upstream { status: 0, .. }))
        ));
    }
}

#[tokio::test]
async fn gemini_semantic_finish_and_prompt_block_are_successful_without_usage() {
    for event in [
        json!({"candidates":[{"finishReason":"STOP"}]}),
        json!({"promptFeedback":{"blockReason":"SAFETY"}}),
    ] {
        let items = stream_body(format!("data: {event}\n\n")).await;
        assert!(items.iter().all(|i| i.is_ok()));
        assert_eq!(
            items
                .iter()
                .filter_map(|i| i.as_ref().ok())
                .filter(|c| c.choices[0].finish_reason.is_some())
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn gemini_in_band_failure_is_not_followed_by_a_second_eof_error() {
    let items = stream_body(
        "data: {\"error\":{\"code\":503,\"message\":\"synthetic failure\"}}\n\n".into(),
    )
    .await;
    assert_eq!(items.len(), 1);
    assert!(matches!(items[0], Err(Error::Upstream { status: 503, .. })));
}

#[tokio::test]
async fn gemini_clean_eof_mid_function_call_json_is_not_success() {
    let items = stream_body("data: {\"candidates\":[{\"content\":{\"parts\":[{\"functionCall\":{\"name\":\"alpha\",\"args\":{\"x\":".into()).await;
    assert_eq!(items.iter().filter(|i| i.is_err()).count(), 1);
    assert!(items.last().unwrap().is_err());
}

async fn truncated_http(body: String) -> (String, tokio::task::JoinHandle<()>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        loop {
            let mut buf = [0; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&buf[..n]);
            if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..end]).to_ascii_lowercase();
                let len: usize = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                if request.len() >= end + 4 + len {
                    break;
                }
            }
        }
        let headers = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len() + 64
        );
        socket.write_all(headers.as_bytes()).await.unwrap();
        socket.write_all(body.as_bytes()).await.unwrap();
        socket.shutdown().await.unwrap();
    });
    (url, task)
}

#[tokio::test]
async fn gemini_transport_failure_after_visible_content_errors_once_even_after_finish() {
    for terminal in [false, true] {
        let mut candidate = json!({"content":{"parts":[{"text":"visible"}]}});
        if terminal {
            candidate["finishReason"] = json!("STOP");
        }
        let body = format!("data: {}\n\n", json!({"candidates":[candidate]}));
        let (url, server) = truncated_http(body).await;
        let mut cfg = GeminiConfig::new("gemini:audit", "synthetic-test-key");
        cfg.base_url = url;
        let items = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            GeminiProvider::new(cfg)
                .stream(request(json!([{"role":"user", "content":"hi"}])))
                .await
                .unwrap()
                .collect::<Vec<_>>()
                .await
        })
        .await
        .unwrap();
        server.await.unwrap();
        assert!(
            items
                .iter()
                .filter_map(|i| i.as_ref().ok())
                .any(|c| c.choices[0].delta.content.as_deref() == Some("visible"))
        );
        assert_eq!(items.iter().filter(|i| i.is_err()).count(), 1);
        assert!(matches!(items.last(), Some(Err(Error::Streaming(_)))));
    }
}

#[test]
fn gemini_multiple_complete_function_parts_are_sdk_concatenable() {
    let mut state = GeminiStreamState::default();
    let mut sdk = SdkAccumulator::default();
    for name in ["alpha", "beta"] {
        let ev = json!({"candidates":[{"content":{"parts":[
            {"text":"between tools"}, {"functionCall":{"name":name,"args":{"x":1}}}
        ]}}]});
        for chunk in state
            .parse_event("p", parse_data_line("p", &ev.to_string()).unwrap())
            .unwrap()
        {
            sdk.push(&chunk);
        }
    }
    let a = &sdk.0[&(0, 0)];
    let b = &sdk.0[&(0, 1)];
    assert_ne!(a.id, b.id);
    sdk.assert_call(0, 0, &a.id, "alpha", "{\"x\":1}");
    sdk.assert_call(0, 1, &b.id, "beta", "{\"x\":1}");
}
