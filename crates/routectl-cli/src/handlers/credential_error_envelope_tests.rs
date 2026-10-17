//! A local credential error (a terminal OAuth refresh, or every reachable seat
//! parked on a credential rejection) reaches the client as a 503 `auth_error`
//! with a fixed message: the reason token and, when seats are parked, the
//! remaining wait, but never the provider id, the OAuth detail, or a
//! `Retry-After` header. The ledger classifies it as `auth`.

use super::*;
use axum::body::to_bytes;
use routectl_core::{Error, UpstreamReason};

const OPERATOR_DETAIL: &str = "oauth refresh failed for p-internal: refresh token expired or \
                               revoked; re-run `routectl login p-internal`";

fn credential(reason: UpstreamReason, parked_until_ms: Option<u64>) -> Error {
    Error::Credential {
        detail: OPERATOR_DETAIL.into(),
        reason,
        parked_until_ms,
    }
}

fn parked_for(secs: u64) -> Option<u64> {
    let now_ms = u64::try_from(crate::handlers::usage_capture::epoch_ms_now()).unwrap();
    Some(now_ms + secs * 1000)
}

async fn rendered(shape: ErrorEnvelopeShape, err: Error) -> (StatusCode, HeaderMap, Value) {
    let resp = map_error(shape, err);
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), 8 * 1024).await.unwrap();
    (status, headers, serde_json::from_slice(&bytes).unwrap())
}

fn assert_no_operator_detail(name: &str, message: &str) {
    for leak in ["p-internal", "oauth refresh failed", "routectl login"] {
        assert!(!message.contains(leak), "{name}: leaks {leak:?}: {message}");
    }
}

#[tokio::test]
async fn a_credential_error_is_a_503_auth_error_with_a_fixed_message() {
    let rows = [
        (
            "terminal refresh",
            UpstreamReason::CredentialInvalid,
            None,
            "credential-invalid",
        ),
        (
            "all seats parked",
            UpstreamReason::CredentialExpired,
            parked_for(1800),
            "credential-expired",
        ),
    ];
    let shapes = [
        (ErrorEnvelopeShape::OpenAi, "auth_error"),
        (ErrorEnvelopeShape::Anthropic, "authentication_error"),
    ];
    for (name, reason, parked_until_ms, token) in rows {
        for (shape, wire_type) in shapes {
            let (status, headers, body) =
                rendered(shape, credential(reason, parked_until_ms)).await;

            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{name} {shape:?}");
            assert_eq!(body["error"]["type"], wire_type, "{name} {shape:?}");
            assert!(
                headers.get(axum::http::header::RETRY_AFTER).is_none(),
                "{name} {shape:?}: no Retry-After"
            );
            let message = body["error"]["message"].as_str().unwrap_or("");
            assert!(
                message.contains(&format!("(reason: {token})")),
                "{name} {shape:?}: {message}"
            );
            assert_no_operator_detail(name, message);
        }
    }
}

#[tokio::test]
async fn an_all_parked_message_names_the_remaining_wait_and_a_refresh_failure_does_not() {
    let (_, _, parked) = rendered(
        ErrorEnvelopeShape::OpenAi,
        credential(UpstreamReason::CredentialExpired, parked_for(1800)),
    )
    .await;
    let (_, _, refresh) = rendered(
        ErrorEnvelopeShape::OpenAi,
        credential(UpstreamReason::CredentialInvalid, None),
    )
    .await;

    let parked = parked["error"]["message"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let refresh = refresh["error"]["message"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let secs: u64 = parked
        .split("for another ")
        .nth(1)
        .and_then(|rest| rest.split('s').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no remaining wait in {parked:?}"));
    assert!((1790..=1800).contains(&secs), "{parked}");
    assert!(!refresh.contains("for another"), "{refresh}");
}

#[test]
fn a_credential_error_takes_the_auth_row_at_every_classifying_site() {
    let err = credential(UpstreamReason::CredentialInvalid, None);

    assert_eq!(
        error_status_and_type(&err),
        (StatusCode::SERVICE_UNAVAILABLE, "auth_error"),
        "error_status_and_type"
    );
    assert_eq!(
        crate::handlers::usage_capture::error_class_of(&err),
        "auth",
        "error_class_of"
    );
    let stream = sanitize_stream_error_for_client(&err);
    assert!(
        stream.contains("(reason: credential-invalid)"),
        "sanitize_stream_error_for_client: {stream}"
    );
    assert_no_operator_detail("stream", &stream);
}
