//! A local refusal reaches the client as exactly the envelope the same refusal
//! produced while it was shaped as an upstream 400: status, `error.type`,
//! `error.code`, `param`, and message, on both dialects and on the stream
//! terminal. The comparison error is built the way that shape was, so the weld
//! breaks if either side drifts.

use super::*;
use axum::body::to_bytes;
use routectl_core::Error;

const DETAIL: &str = "client system content cannot be relocated: the request carries no message \
                      array";

fn local() -> Error {
    Error::LocalRefusal {
        provider: "p-internal".into(),
        detail: DETAIL.into(),
    }
}

fn upstream_shaped() -> Error {
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": format!("routectl refused the request before egress: {DETAIL}"),
        },
    });
    Error::upstream_full(
        "p-internal",
        400,
        body.to_string(),
        None,
        Some("invalid_request_error".into()),
        None,
    )
}

async fn rendered(shape: ErrorEnvelopeShape, err: Error) -> (StatusCode, Value) {
    let resp = map_error(shape, err);
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), 8 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn both_dialect_envelopes_match_the_upstream_shaped_refusal() {
    for shape in [ErrorEnvelopeShape::Anthropic, ErrorEnvelopeShape::OpenAi] {
        let (local_status, local_body) = rendered(shape, local()).await;
        let (upstream_status, upstream_body) = rendered(shape, upstream_shaped()).await;

        assert_eq!(local_status, StatusCode::BAD_REQUEST);
        assert_eq!(local_status, upstream_status);
        assert_eq!(local_body, upstream_body);
        let message = local_body["error"]["message"].as_str().unwrap_or("");
        assert!(message.contains(DETAIL), "{message}");
        assert!(
            !message.contains("p-internal"),
            "names the provider: {message}"
        );
    }
}

#[test]
fn the_stream_terminal_matches_the_upstream_shaped_refusal() {
    let local_class = crate::ingress::StreamErrorClass::from_error(&local());
    let upstream_class = crate::ingress::StreamErrorClass::from_error(&upstream_shaped());

    assert_eq!(
        sanitize_stream_error_for_client(&local()),
        sanitize_stream_error_for_client(&upstream_shaped())
    );
    assert_eq!(local_class.anthropic_type, upstream_class.anthropic_type);
    assert_eq!(local_class.openai_type, upstream_class.openai_type);
    assert_eq!(local_class.openai_code, upstream_class.openai_code);
    assert_eq!(local_class.anthropic_type, "invalid_request_error");
}
