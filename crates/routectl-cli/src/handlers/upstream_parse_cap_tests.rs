//! The client-facing upstream detail and `param` come only from the body
//! parse the error carries. A JSON error body over the core parse cap carries
//! no parse, so neither the non-streaming envelope nor the stream terminal
//! surfaces anything from it; the same envelope within the cap still does.

use super::*;
use axum::body::to_bytes;
use routectl_core::{Error, MAX_ERROR_BODY_BYTES};

const MESSAGE: &str = "max_tokens is too large";
const PARAM: &str = "max_tokens";

fn envelope_body(padding: usize) -> String {
    json!({
        "error": {
            "type": "invalid_request_error",
            "message": MESSAGE,
            "param": PARAM,
        },
        "padding": "x".repeat(padding),
    })
    .to_string()
}

fn in_cap_error() -> Error {
    let err = Error::upstream_full("p-internal", 400, envelope_body(0), None, None, None);
    assert!(
        err.parsed_body().is_some(),
        "in-cap fixture must carry a parse"
    );
    err
}

fn over_cap_error() -> Error {
    let body = envelope_body(MAX_ERROR_BODY_BYTES);
    assert!(body.len() > MAX_ERROR_BODY_BYTES);
    assert!(
        serde_json::from_str::<Value>(&body).is_ok(),
        "over-cap fixture must be valid JSON so only the cap withholds it"
    );
    let err = Error::upstream_full("p-internal", 400, body, None, None, None);
    assert!(
        err.parsed_body().is_none(),
        "over-cap body carries no parse"
    );
    err
}

async fn openai_error_object(err: Error) -> Value {
    let resp = map_error(ErrorEnvelopeShape::OpenAi, err);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    body["error"].clone()
}

#[tokio::test]
async fn map_error_surfaces_detail_and_param_from_an_in_cap_body() {
    let error = openai_error_object(in_cap_error()).await;

    assert_eq!(
        error["message"],
        format!("upstream error (HTTP 400): {MESSAGE}")
    );
    assert_eq!(error["param"], PARAM);
}

#[tokio::test]
async fn map_error_surfaces_no_detail_or_param_from_an_over_cap_body() {
    let error = openai_error_object(over_cap_error()).await;

    assert_eq!(error["message"], "upstream error (HTTP 400)");
    assert!(error["param"].is_null(), "param must be null: {error}");
}

#[test]
fn stream_error_surfaces_detail_from_an_in_cap_body() {
    let safe = sanitize_stream_error_for_client(&in_cap_error());

    assert_eq!(safe, format!("upstream stream error (HTTP 400): {MESSAGE}"));
}

#[test]
fn stream_error_surfaces_no_detail_from_an_over_cap_body() {
    let safe = sanitize_stream_error_for_client(&over_cap_error());

    assert_eq!(safe, "upstream stream error (HTTP 400)");
}
