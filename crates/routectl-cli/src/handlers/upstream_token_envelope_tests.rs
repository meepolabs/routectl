//! The non-stream envelope admits an upstream-authored `error.type`,
//! `error.code` and correlation id only when it passes
//! `is_client_safe_token`. A value failing the filter is dropped, so the
//! envelope is the one the same error carries with no upstream value at all.

use super::*;
use axum::body::to_bytes;
use routectl_core::Error;

const BODY: &str = r#"{"error":{"message":"bad request"}}"#;

/// Upstream values that fail the client-token filter, each a named row.
fn unsafe_tokens() -> [(&'static str, String); 3] {
    [
        (
            "twelve-digit run",
            "invalid_request_123456789012".to_string(),
        ),
        ("129 bytes", "a".repeat(129)),
        ("contains a space", "invalid request".to_string()),
    ]
}

fn upstream_400(upstream_type: Option<&str>, upstream_code: Option<&str>) -> Error {
    Error::upstream_full(
        "p",
        400,
        BODY,
        None,
        upstream_type.map(str::to_string),
        upstream_code.map(str::to_string),
    )
}

async fn rendered(shape: ErrorEnvelopeShape, err: Error) -> (HeaderMap, Value) {
    let resp = map_error(shape, err);
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    (headers, serde_json::from_slice(&bytes).unwrap())
}

const SHAPES: [ErrorEnvelopeShape; 2] = [ErrorEnvelopeShape::OpenAi, ErrorEnvelopeShape::Anthropic];

#[tokio::test]
async fn an_unsafe_upstream_type_yields_the_status_mapped_envelope() {
    for shape in SHAPES {
        let (_, baseline) = rendered(shape, upstream_400(None, None)).await;
        for (name, token) in unsafe_tokens() {
            let (_, body) = rendered(shape, upstream_400(Some(&token), None)).await;

            assert_eq!(body, baseline, "{name} {shape:?}");
        }
    }
}

#[tokio::test]
async fn an_unsafe_upstream_code_falls_back_to_the_upstream_type() {
    for (name, token) in unsafe_tokens() {
        let err = upstream_400(Some("invalid_request_error"), Some(&token));

        let (_, body) = rendered(ErrorEnvelopeShape::OpenAi, err).await;

        assert_eq!(body["error"]["type"], "invalid_request_error", "{name}");
        assert_eq!(body["error"]["code"], "invalid_request_error", "{name}");
    }
}

#[tokio::test]
async fn a_safe_upstream_type_and_code_pass_through() {
    let err = upstream_400(Some("invalid_request_error"), Some("rate_limit_exceeded"));

    let (_, body) = rendered(ErrorEnvelopeShape::OpenAi, err).await;

    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert_eq!(body["error"]["code"], "rate_limit_exceeded");
}

#[tokio::test]
async fn the_correlation_id_header_carries_only_a_client_safe_id() {
    let rows = [
        ("anthropic request id", "req_011CQZaSb7nRk3vXyPq8Fh2m", true),
        ("cf-ray", "8f3a1b2c4d5e6f70-SJC", true),
        ("twelve-digit run", "req-123456789012", false),
        ("contains a space", "req abc", false),
    ];
    for (name, id, surfaces) in rows {
        let err = upstream_400(None, None).with_upstream_request_id(Some(id.to_string()));

        let (headers, _) = rendered(ErrorEnvelopeShape::OpenAi, err).await;

        let header = headers
            .get(UPSTREAM_REQUEST_ID_HEADER)
            .map(|v| v.to_str().unwrap().to_string());
        assert_eq!(header, surfaces.then(|| id.to_string()), "{name}");
    }
}
