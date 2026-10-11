//! The wire tap on the gemini egress: every send (API key and Cloud Code,
//! complete and stream) deposits the exact bytes the upstream received.
//! Gemini translates every dialect, so it declares no edits.

use std::sync::Arc;

use routectl_core::wire_check::{Leg, WireTap};
use routectl_core::{
    ChatRequest, CloudProjectCache, InMemoryProjectCache, Message, MessageContent, Provider, Role,
    StaticToken, TokenSource,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::cloudcode;
use super::{GeminiConfig, GeminiProvider};

const MODEL: &str = "gemini-2.5-pro";

fn api_key_provider(base_url: &str) -> GeminiProvider {
    let mut cfg = GeminiConfig::new("gemini:tap", "test-api-key");
    cfg.base_url = base_url.to_string();
    GeminiProvider::new(cfg)
}

fn cloud_code_provider(base_url: &str) -> GeminiProvider {
    let auth: Arc<dyn TokenSource> = Arc::new(StaticToken::new("ya29.tap"));
    let cache: Arc<dyn CloudProjectCache> = Arc::new(InMemoryProjectCache::with("proj-tap"));
    let mut cfg = GeminiConfig::new_cloud_code("gemini:tap", auth, cache);
    cfg.base_url = base_url.to_string();
    GeminiProvider::new(cfg)
}

fn request() -> ChatRequest {
    ChatRequest {
        model: MODEL.into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("ping".into()),
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(64),
        temperature: Some(0.25),
        ..Default::default()
    }
}

fn ok_body() -> Value {
    json!({
        "candidates": [{"content": {"parts": [{"text": "pong"}], "role": "model"}, "finishReason": "STOP", "index": 0}],
        "responseId": "r1",
        "modelVersion": "gemini-2.5-pro-001"
    })
}

fn sse_body(wrapped: bool) -> String {
    let event = if wrapped {
        json!({"response": ok_body()})
    } else {
        ok_body()
    };
    format!("data: {event}\n\n")
}

/// A mock for one send site, answering the shape that site parses.
fn mock_for(site: Site) -> Mock {
    let (route, template) = match site {
        Site::ApiKeyComplete => (
            format!("/models/{MODEL}:generateContent"),
            ResponseTemplate::new(200).set_body_json(ok_body()),
        ),
        Site::ApiKeyStream => (
            format!("/models/{MODEL}:streamGenerateContent"),
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body(false)),
        ),
        Site::CloudCodeComplete => (
            cloudcode::GENERATE_PATH.to_string(),
            ResponseTemplate::new(200).set_body_json(json!({"response": ok_body()})),
        ),
        Site::CloudCodeStream => (
            "/v1internal:streamGenerateContent".to_string(),
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(sse_body(true)),
        ),
    };
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(template)
}

#[derive(Debug, Clone, Copy)]
enum Site {
    ApiKeyComplete,
    ApiKeyStream,
    CloudCodeComplete,
    CloudCodeStream,
}

impl Site {
    const ALL: [Self; 4] = [
        Self::ApiKeyComplete,
        Self::ApiKeyStream,
        Self::CloudCodeComplete,
        Self::CloudCodeStream,
    ];

    const fn streams(self) -> bool {
        matches!(self, Self::ApiKeyStream | Self::CloudCodeStream)
    }

    fn provider(self, base_url: &str) -> GeminiProvider {
        match self {
            Self::ApiKeyComplete | Self::ApiKeyStream => api_key_provider(base_url),
            Self::CloudCodeComplete | Self::CloudCodeStream => cloud_code_provider(base_url),
        }
    }
}

/// Drive one send site against wiremock and return what the upstream
/// received (body, content-type) alongside the tap's contents.
async fn send(site: Site, wire_tap: Option<WireTap>) -> (Vec<u8>, Option<String>) {
    let server = MockServer::start().await;
    mock_for(site).mount(&server).await;
    let provider = site.provider(&server.uri());
    let mut req = request();
    req.routectl_internal.wire_tap = wire_tap;
    if site.streams() {
        let _stream = provider.stream(req).await.expect("stream open");
    } else {
        provider.complete(req).await.expect("complete");
    }
    let requests = server.received_requests().await.expect("recording on");
    let sent = requests.last().expect("one request");
    let content_type = sent
        .headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (sent.body.clone(), content_type)
}

#[tokio::test]
async fn every_send_site_deposits_the_exact_bytes_the_upstream_received() {
    for site in Site::ALL {
        let wire_tap = WireTap::default();

        let (body, _) = send(site, Some(wire_tap.clone())).await;

        let contents = wire_tap.take_request();
        assert_eq!(
            contents.request().map(|b| b.as_ref()),
            Some(body.as_slice()),
            "{site:?}"
        );
        assert!(contents.edits(Leg::Request).is_empty(), "{site:?}");
    }
}

/// The bytes and content-type the old `.header(content-type).json(&body)`
/// builder produced, so the explicit body is pinned against the builder it
/// replaced rather than against itself.
fn json_builder_reference(body: &Value) -> (Vec<u8>, Option<String>) {
    let request = reqwest::Client::new()
        .post("http://127.0.0.1:9/x")
        .header("content-type", "application/json")
        .json(body)
        .build()
        .expect("reference request");
    let bytes = request
        .body()
        .and_then(reqwest::Body::as_bytes)
        .expect("buffered body")
        .to_vec();
    let content_type = request
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (bytes, content_type)
}

#[tokio::test]
async fn body_and_content_type_match_the_json_builder_on_every_send_site() {
    let expected_inner = api_key_provider("http://127.0.0.1:9")
        .normalize_request(&request())
        .expect("normalize");
    for site in Site::ALL {
        let expected = match site {
            Site::ApiKeyComplete | Site::ApiKeyStream => expected_inner.clone(),
            Site::CloudCodeComplete | Site::CloudCodeStream => {
                cloudcode::wrap_envelope(expected_inner.clone(), "proj-tap", MODEL)
            }
        };

        let (body, content_type) = send(site, None).await;

        let (reference_body, reference_type) = json_builder_reference(&expected);
        assert_eq!(body, reference_body, "{site:?}");
        assert_eq!(content_type, reference_type, "{site:?}");
        assert_eq!(
            content_type.as_deref(),
            Some("application/json"),
            "{site:?}"
        );
    }
}
