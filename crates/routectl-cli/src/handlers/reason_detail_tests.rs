//! The typed credential reason reaches every client error sink: the
//! non-stream envelope and the stream terminal event of the OpenAI chat,
//! Anthropic and Responses dialects. The token comes from the error's typed
//! reason, never from upstream text, and appears exactly once even when the
//! bounded upstream detail already names it.

use super::*;
use axum::body::to_bytes;
use routectl_core::{Error, UpstreamBody, UpstreamReason};

use crate::ingress::StreamErrorClass;
use crate::ingress::anthropic::AnthropicIngress;
use crate::ingress::openai::OpenAiIngress;
use crate::ingress::openai_responses::ResponsesIngress;

const EXPIRED: &str = "credential-expired";
const INVALID: &str = "credential-invalid";

/// One client error sink: a dialect and whether the error lands before the
/// response head (the JSON envelope) or after it (the stream terminal event).
#[derive(Clone, Copy, Debug)]
enum Sink {
    ChatEnvelope,
    AnthropicEnvelope,
    ResponsesEnvelope,
    ChatStream,
    AnthropicStream,
    ResponsesStream,
}

const SINKS: [Sink; 6] = [
    Sink::ChatEnvelope,
    Sink::AnthropicEnvelope,
    Sink::ResponsesEnvelope,
    Sink::ChatStream,
    Sink::AnthropicStream,
    Sink::ResponsesStream,
];

async fn envelope(adapter: &dyn IngressAdapter, err: Error) -> Value {
    let resp = map_error(adapter.error_envelope_shape(), err);
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn stream_terminal(adapter: &dyn IngressAdapter, err: &Error) -> Value {
    let mut state = adapter.new_stream_state(&StreamRequestContext::default());
    let message = sanitize_stream_error_for_client(err);
    let class = StreamErrorClass::from_error(err);
    let events = adapter.render_error_eos(state.as_mut(), &message, &class);
    events
        .iter()
        .filter_map(|ev| serde_json::from_str::<Value>(&ev.data).ok())
        .map(|payload| payload.get("response").cloned().unwrap_or(payload))
        .find(|payload| payload.get("error").is_some_and(Value::is_object))
        .unwrap_or_else(|| panic!("no terminal error event in {events:?}"))
}

/// The `error` object a client of `sink` reads for `err`.
async fn client_error(sink: Sink, err: Error) -> Value {
    let rendered = match sink {
        Sink::ChatEnvelope => envelope(&OpenAiIngress, err).await,
        Sink::AnthropicEnvelope => envelope(&AnthropicIngress, err).await,
        Sink::ResponsesEnvelope => envelope(&ResponsesIngress, err).await,
        Sink::ChatStream => stream_terminal(&OpenAiIngress, &err),
        Sink::AnthropicStream => stream_terminal(&AnthropicIngress, &err),
        Sink::ResponsesStream => stream_terminal(&ResponsesIngress, &err),
    };
    rendered["error"].clone()
}

async fn client_message(sink: Sink, err: Error) -> String {
    client_error(sink, err).await["message"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// A Mantle 401 whose upstream message does not name the reason.
fn mantle_invalid() -> Error {
    let body = json!({"error": {"code": "invalid_api_key", "message": "Invalid bearer token",
                                "param": null, "type": "permission_denied_error"}});
    Error::upstream_full(
        "p-internal",
        401,
        body.to_string(),
        None,
        Some("permission_denied_error".into()),
        Some("invalid_api_key".into()),
    )
    .with_upstream_reason(Some(UpstreamReason::CredentialInvalid))
}

/// A Bedrock 403 the provider already rendered with its reason segment.
fn bedrock_expired() -> Error {
    Error::upstream_with_body(
        "p-internal",
        403,
        UpstreamBody::error_envelope(
            "bedrock access denied (AccessDeniedException): credential-expired",
        ),
        None,
        Some("AccessDeniedException".into()),
        None,
    )
    .with_upstream_reason(Some(UpstreamReason::CredentialExpired))
}

/// A local credential error, whose fixed message already names the reason.
fn local_credential() -> Error {
    Error::Credential {
        detail: "oauth refresh failed for p-internal".into(),
        reason: UpstreamReason::CredentialExpired,
        parked_until_ms: None,
    }
}

/// An upstream detail longer than the client bound whose own text names the
/// reason only past the cut, so the bounded detail does not.
fn oversized_expired() -> Error {
    let long = format!("{} {EXPIRED}", "x".repeat(MAX_UPSTREAM_DETAIL_CHARS));
    Error::upstream_with_body(
        "p-internal",
        401,
        UpstreamBody::error_envelope(&long),
        None,
        Some("authentication_error".into()),
        None,
    )
    .with_upstream_reason(Some(UpstreamReason::CredentialExpired))
}

#[tokio::test]
async fn a_reasoned_error_names_its_token_exactly_once_on_every_sink() {
    type Row = (&'static str, fn() -> Error, &'static str);
    let rows: [Row; 4] = [
        ("detail without the token", mantle_invalid, INVALID),
        ("provider detail with the token", bedrock_expired, EXPIRED),
        ("local credential message", local_credential, EXPIRED),
        ("token truncated off the detail", oversized_expired, EXPIRED),
    ];
    for (name, err, token) in rows {
        for sink in SINKS {
            let message = client_message(sink, err()).await;

            assert_eq!(
                message.matches(token).count(),
                1,
                "{name} {sink:?}: {message}"
            );
        }
    }
}

#[tokio::test]
async fn an_upstream_error_without_its_detail_token_gains_the_reason_suffix() {
    for sink in SINKS {
        let message = client_message(sink, mantle_invalid()).await;

        assert!(
            message.ends_with(&format!("Invalid bearer token (reason: {INVALID})")),
            "{sink:?}: {message}"
        );
    }
}

#[tokio::test]
async fn upstream_text_naming_a_token_without_a_typed_reason_gains_no_suffix() {
    let body = json!({"error": {"message": format!("key rejected: {EXPIRED}"),
                                "type": "authentication_error"}});
    for sink in SINKS {
        let err = Error::upstream_full(
            "p-internal",
            401,
            body.to_string(),
            None,
            Some("authentication_error".into()),
            None,
        );

        let message = client_message(sink, err).await;

        assert!(
            message.ends_with(&format!("key rejected: {EXPIRED}")),
            "{sink:?}: {message}"
        );
        assert!(!message.contains("(reason:"), "{sink:?}: {message}");
    }
}

#[tokio::test]
async fn the_suffix_follows_the_bounded_detail() {
    let bound_marker = "... [truncated]";
    for sink in SINKS {
        let message = client_message(sink, oversized_expired()).await;

        assert!(
            message.ends_with(&format!("{bound_marker} (reason: {EXPIRED})")),
            "{sink:?}: {message}"
        );
    }
}

#[tokio::test]
async fn the_reason_leaves_the_envelope_type_and_code_unchanged() {
    let rows = [
        (
            Sink::ChatEnvelope,
            "permission_denied_error",
            Some("invalid_api_key"),
        ),
        (
            Sink::ResponsesEnvelope,
            "permission_denied_error",
            Some("invalid_api_key"),
        ),
        (Sink::AnthropicEnvelope, "authentication_error", None),
        (
            Sink::ChatStream,
            "permission_denied_error",
            Some("invalid_api_key"),
        ),
        (
            Sink::ResponsesStream,
            "permission_denied_error",
            Some("invalid_api_key"),
        ),
        (Sink::AnthropicStream, "authentication_error", None),
    ];
    for (sink, wire_type, wire_code) in rows {
        let error = client_error(sink, mantle_invalid()).await;

        assert_eq!(error["type"], wire_type, "{sink:?}");
        if let Some(code) = wire_code {
            assert_eq!(error["code"], code, "{sink:?}");
        }
    }
}
