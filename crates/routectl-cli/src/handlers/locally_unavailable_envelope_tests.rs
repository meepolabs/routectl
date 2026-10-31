//! A local eligibility refusal reaches every client the same way: HTTP 503
//! whose error type is the reason token on the OpenAI chat, Anthropic and
//! Responses envelopes, a `Retry-After` header carrying the wait rounded up
//! and clamped to 1..=3600 s (absent when unknown), and the same token on the
//! terminal event of a stream whose head already went out.

use std::time::Duration;

use super::*;
use axum::body::to_bytes;
use axum::http::header::RETRY_AFTER;
use routectl_core::{EligibilityReason, Error};

use crate::ingress::StreamErrorClass;
use crate::ingress::anthropic::AnthropicIngress;
use crate::ingress::openai::OpenAiIngress;
use crate::ingress::openai_responses::ResponsesIngress;

const REASONS: [EligibilityReason; 6] = [
    EligibilityReason::CredentialUnusable,
    EligibilityReason::AccountParked,
    EligibilityReason::BreakerOpen,
    EligibilityReason::RateLimited,
    EligibilityReason::Capability,
    EligibilityReason::ContextWindow,
];

type WaitRow = (&'static str, Option<Duration>, Option<&'static str>);

const WAITS: [WaitRow; 4] = [
    ("whole seconds", Some(Duration::from_secs(30)), Some("30")),
    ("sub-second", Some(Duration::from_millis(250)), Some("1")),
    ("past the cap", Some(Duration::from_hours(2)), Some("3600")),
    ("unknown", None, None),
];

fn unavailable(reason: EligibilityReason, retry_after: Option<Duration>) -> Error {
    Error::LocallyUnavailable {
        reason,
        retry_after,
    }
}

fn ingresses() -> [(&'static str, &'static dyn IngressAdapter); 3] {
    [
        ("chat completions", &OpenAiIngress),
        ("messages", &AnthropicIngress),
        ("responses", &ResponsesIngress),
    ]
}

async fn rendered(adapter: &dyn IngressAdapter, err: Error) -> (StatusCode, HeaderMap, Value) {
    let resp = map_error(adapter.error_envelope_shape(), err);
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
    (status, headers, serde_json::from_slice(&bytes).unwrap())
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
        .find_map(|payload| payload.get("error").filter(|e| e.is_object()).cloned())
        .unwrap_or_else(|| panic!("no terminal error event in {events:?}"))
}

fn assert_types_are_the_token(adapter: &dyn IngressAdapter, error: &Value, token: &str, row: &str) {
    assert_eq!(error["type"], token, "{row}: error.type");
    if adapter.error_envelope_shape() == ErrorEnvelopeShape::OpenAi {
        assert_eq!(error["code"], token, "{row}: error.code");
    }
}

#[tokio::test]
async fn every_ingress_answers_503_with_the_token_and_the_clamped_wait() {
    for (ingress, adapter) in ingresses() {
        for reason in REASONS {
            for (wait_name, wait, header) in WAITS {
                let row = format!("{ingress} {reason:?} {wait_name}");

                let (status, headers, body) = rendered(adapter, unavailable(reason, wait)).await;

                assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{row}");
                assert_types_are_the_token(adapter, &body["error"], reason.as_str(), &row);
                let got = headers.get(RETRY_AFTER).map(|v| v.to_str().unwrap());
                assert_eq!(got, header, "{row}: Retry-After");
                let message = body["error"]["message"].as_str().unwrap_or("");
                assert!(message.contains(reason.as_str()), "{row}: {message}");
                match header {
                    Some(secs) => assert!(message.ends_with(&format!("{secs}s")), "{row}"),
                    None => assert!(!message.contains("retry after"), "{row}: {message}"),
                }
            }
        }
    }
}

#[test]
fn a_committed_stream_ends_with_the_token_on_both_wire_shapes() {
    for (ingress, adapter) in ingresses() {
        for reason in REASONS {
            let row = format!("{ingress} {reason:?}");
            let err = unavailable(reason, Some(Duration::from_secs(30)));

            let error = stream_terminal(adapter, &err);

            assert_types_are_the_token(adapter, &error, reason.as_str(), &row);
            assert_ne!(error["type"], crate::ingress::STREAM_ERROR_TYPE, "{row}");
            let message = error["message"].as_str().unwrap_or("");
            assert!(
                message.contains(&format!("(reason: {})", reason.as_str())),
                "{row}: {message}"
            );
            assert!(!message.contains("upstream"), "{row}: {message}");
        }
    }
}

#[test]
fn the_anthropic_pass_through_is_keyed_on_the_local_type_not_the_upstream_one() {
    let from_upstream = anthropic_error_type(
        "upstream_error",
        StatusCode::TOO_MANY_REQUESTS,
        Some("rate_limited"),
    );

    assert_eq!(from_upstream, "rate_limit_error");
}

#[test]
fn error_class_of_names_each_variant_family() {
    let rows: [(Error, &str); 15] = [
        (
            unavailable(EligibilityReason::BreakerOpen, None),
            "locally_unavailable",
        ),
        (Error::upstream("p", 500, "x"), "upstream"),
        (Error::Streaming("s".into()), "streaming"),
        (
            Error::Validation("v".into(), routectl_core::RefusalReason::Unparseable),
            "validation",
        ),
        (
            Error::LocalRefusal {
                provider: "p".into(),
                detail: "d".into(),
                reason: routectl_core::RefusalReason::CloakRelocation,
            },
            "local_refusal",
        ),
        (Error::Auth("a".into()), "auth"),
        (
            Error::Credential {
                detail: "d".into(),
                reason: routectl_core::UpstreamReason::CredentialInvalid,
                parked_until_ms: None,
            },
            "auth",
        ),
        (Error::Config("c".into()), "config"),
        (Error::Internal("i".into()), "internal"),
        (
            Error::NotImplemented("p".into(), "d".into()),
            "not_implemented",
        ),
        (
            Error::capability_unsupported_everywhere("advisor", ["lane-a"]),
            "capability_unsupported",
        ),
        (Error::UnknownAlias("a".into()), "unknown_alias"),
        (Error::UnknownProvider("p".into()), "unknown_provider"),
        (
            Error::normalize_request("p", "m", routectl_core::RefusalReason::Unparseable),
            "normalize_request",
        ),
        (Error::normalize_response("p", "m"), "normalize_response"),
    ];
    for (err, class) in rows {
        assert_eq!(
            crate::handlers::usage_capture::error_class_of(&err),
            class,
            "{err:?}"
        );
    }
}
