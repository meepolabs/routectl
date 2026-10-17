//! The API-key lane's non-success read, end to end over HTTP: the
//! `Retry-After` hint is kept only on a rate-limit status, and an over-cap
//! error body trips exactly one cap WARN and collapses to the fixed cap
//! message.

use super::*;
use routectl_core::ChatRequest;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const CAP_WARN: &str = "upstream response body exceeded cap; truncated";

async fn provider_answering(template: ResponseTemplate) -> (GeminiProvider, MockServer) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(template)
        .mount(&server)
        .await;
    let mut cfg = GeminiConfig::new("gemini:test", "test-api-key");
    cfg.base_url = server.uri();
    (GeminiProvider::new(cfg), server)
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "gemini-2.5-pro".into(),
        messages: vec![routectl_core::test_utils::user_msg("ping")].into(),
        max_tokens: Some(64),
        ..Default::default()
    }
}

#[tokio::test]
async fn retry_after_is_kept_only_on_a_rate_limit_status() {
    let cases: [(&str, u16, Option<Duration>); 2] = [
        ("429 carries the hint", 429, Some(Duration::from_secs(30))),
        ("400 drops the hint", 400, None),
    ];
    for (name, status, want) in cases {
        let (provider, _server) = provider_answering(
            ResponseTemplate::new(status)
                .insert_header("retry-after", "30")
                .set_body_json(serde_json::json!({
                    "error": {"code": status, "message": "m", "status": "RESOURCE_EXHAUSTED"}
                })),
        )
        .await;

        let err = provider.complete(request()).await.expect_err(name);

        match err {
            Error::Upstream {
                status: got,
                retry_after,
                upstream_type,
                ..
            } => {
                assert_eq!(got, status, "{name}");
                assert_eq!(retry_after, want, "{name}");
                assert_eq!(
                    upstream_type.as_deref(),
                    Some("RESOURCE_EXHAUSTED"),
                    "{name}"
                );
            }
            other => panic!("{name}: expected Upstream, got {other:?}"),
        }
    }
}

#[tokio::test]
async fn over_cap_error_body_warns_once_and_carries_the_fixed_message() {
    let (provider, _server) = provider_answering(ResponseTemplate::new(429).set_body_bytes(vec![
            b'a';
            crate::http_client::MAX_RESPONSE_BODY_BYTES
                + 1
        ]))
    .await;

    let (result, events) = routectl_testkit::with_capture(provider.complete(request())).await;

    match result.expect_err("over-cap 429") {
        Error::Upstream { status, body, .. } => {
            assert_eq!(status, 429);
            assert_eq!(body, crate::http_client::body_cap_exceeded_message());
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
    let cap_warns: Vec<_> = events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN && e.message == CAP_WARN)
        .collect();
    assert_eq!(cap_warns.len(), 1, "exactly one cap WARN");
    assert_eq!(cap_warns[0].field("path"), Some("error_body"));
    assert_eq!(cap_warns[0].field("status"), Some("429"));
    assert_eq!(cap_warns[0].field("body_truncated"), Some("true"));
}
