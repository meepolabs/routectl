//! The lane's non-success read, end to end over HTTP on both API shapes:
//! the `Retry-After` hint is kept only on a rate-limit status, and an
//! over-cap error body trips exactly one cap WARN and collapses to the
//! fixed cap message.

use routectl_core::{ChatRequest, Error, Provider};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth};

const SHAPES: [BedrockApiShape; 2] = [BedrockApiShape::Invoke, BedrockApiShape::Converse];
const CAP_WARN: &str = "upstream response body exceeded cap; truncated";

async fn provider_at(server: &MockServer, shape: BedrockApiShape) -> BedrockProvider {
    let creds = BedrockCreds::BearerKey {
        key: "error-read-test-key".into(),
    };
    let cfg = BedrockConfig {
        id: "bedrock-error-read".into(),
        region: "us-east-1".into(),
        model_id: "us.anthropic.claude-opus-5-5".into(),
        api_shape: shape,
        creds: creds.clone(),
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
        adaptive_thinking: None,
    };
    let resolved = auth::resolve(&creds, "us-east-1").await.expect("resolve");
    let mut provider = BedrockProvider::new(cfg, resolved).expect("canonical region");
    provider.client =
        crate::http_client::build_no_redirect(None, &server.uri()).expect("loopback client");
    provider.runtime_origin = server.uri();
    provider
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "us.anthropic.claude-opus-5-5".into(),
        messages: vec![routectl_core::test_utils::user_msg("hello")].into(),
        max_tokens: Some(16),
        ..Default::default()
    }
}

async fn server_answering(template: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(template)
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn retry_after_is_kept_only_on_a_rate_limit_status() {
    let cases: [(&str, u16, Option<std::time::Duration>); 2] = [
        (
            "429 carries the hint",
            429,
            Some(std::time::Duration::from_secs(30)),
        ),
        ("400 drops the hint", 400, None),
    ];
    for shape in SHAPES {
        for (name, status, want) in cases {
            let server = server_answering(
                ResponseTemplate::new(status)
                    .insert_header("retry-after", "30")
                    .set_body_json(serde_json::json!({"message": "m"})),
            )
            .await;
            let provider = provider_at(&server, shape).await;

            let err = provider.complete(request()).await.expect_err(name);

            match err {
                Error::Upstream {
                    status: got,
                    retry_after,
                    ..
                } => {
                    assert_eq!(got, status, "{shape:?} {name}");
                    assert_eq!(retry_after, want, "{shape:?} {name}");
                }
                other => panic!("{shape:?} {name}: expected Upstream, got {other:?}"),
            }
        }
    }
}

#[tokio::test]
async fn over_cap_error_body_warns_once_and_carries_the_fixed_message() {
    for shape in SHAPES {
        let server = server_answering(ResponseTemplate::new(429).set_body_bytes(vec![
            b'a';
            crate::http_client::MAX_RESPONSE_BODY_BYTES
                + 1
        ]))
        .await;
        let provider = provider_at(&server, shape).await;

        let (result, events) = routectl_testkit::with_capture(provider.complete(request())).await;

        match result.expect_err("over-cap 429") {
            Error::Upstream { status, body, .. } => {
                assert_eq!(status, 429, "{shape:?}");
                assert_eq!(
                    body,
                    crate::http_client::body_cap_exceeded_message(),
                    "{shape:?}"
                );
            }
            other => panic!("{shape:?}: expected Upstream, got {other:?}"),
        }
        let cap_warns: Vec<_> = events
            .iter()
            .filter(|e| e.level == tracing::Level::WARN && e.message == CAP_WARN)
            .collect();
        assert_eq!(cap_warns.len(), 1, "{shape:?}: exactly one cap WARN");
        assert_eq!(cap_warns[0].field("path"), Some("error_body"), "{shape:?}");
        assert_eq!(cap_warns[0].field("status"), Some("429"), "{shape:?}");
        assert_eq!(
            cap_warns[0].field("body_truncated"),
            Some("true"),
            "{shape:?}"
        );
    }
}

/// A header-less flat 400 (no body `__type`, no `x-amzn-errortype`) leaves
/// the capability matcher no type token, so the lane names the missing
/// header once at WARN and bumps its counter.
#[tokio::test]
async fn untyped_request_fault_names_the_unusable_error_type_header() {
    let server = server_answering(
        ResponseTemplate::new(400).set_body_json(serde_json::json!({"message": "bad"})),
    )
    .await;
    let provider = provider_at(&server, BedrockApiShape::Invoke).await;
    let before = crate::aws_error::error_type_header_unusable_count("missing");

    let (result, events) = routectl_testkit::with_capture(provider.complete(request())).await;

    assert!(result.is_err());
    let unusable: Vec<_> = events
        .iter()
        .filter(|e| {
            e.level == tracing::Level::WARN
                && e.message == "bedrock upstream error-type header unusable"
        })
        .collect();
    assert_eq!(unusable.len(), 1);
    assert_eq!(unusable[0].field("reason"), Some("missing"));
    assert!(crate::aws_error::error_type_header_unusable_count("missing") > before);
}

#[tokio::test]
async fn a_recorded_credential_rejection_carries_its_reason_over_http() {
    use crate::upstream_error::reason_tests::{NATIVE_CAPTURE, captured};
    use routectl_core::UpstreamReason::{CredentialExpired, CredentialInvalid};
    let rows = [
        ("expired-invoke", CredentialExpired),
        ("invalid-converse", CredentialInvalid),
    ];
    for shape in SHAPES {
        for (key, want) in rows {
            let row = captured(NATIVE_CAPTURE, key);
            let mut template = ResponseTemplate::new(row.status).set_body_string(row.body);
            for (name, value) in &row.headers {
                template = template.insert_header(name.as_str(), value.to_str().expect("ascii"));
            }
            let server = server_answering(template).await;
            let provider = provider_at(&server, shape).await;

            let err = provider.complete(request()).await.expect_err(key);

            assert_eq!(err.upstream_reason(), Some(want), "{shape:?} {key}");
        }
    }
}
