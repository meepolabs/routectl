//! First-party anthropic-api lane redirect behavior: pins that a 3xx
//! from the configured host is surfaced as an upstream failure rather
//! than followed to a different host, so `x-api-key` never reaches an
//! unintended server.

use super::*;
use routectl_core::Error;
use routectl_core::failure_class::{FailureClass, classify};

/// The first-party lane uses a no-redirect client, same as the mantle
/// lane: a 3xx from the configured host must never be chased to a
/// `Location` target on a DIFFERENT host, since that would carry
/// `x-api-key` off the configured host. Two separate `MockServer`
/// instances stand in for two distinct hosts -- server A answers the
/// real request with a 302 pointing at server B; server B must never
/// see a request at all.
#[tokio::test]
async fn first_party_lane_does_not_follow_cross_host_redirect() {
    let server_a = MockServer::start().await;
    let server_b = MockServer::start().await;
    // Rewrite server B's URL host from `127.0.0.1` to `localhost` -- both
    // resolve to the loopback interface, but this pins the cross-HOST
    // case literally (not just cross-port) even if a future policy
    // allows same-host redirects.
    let redirect_target = format!(
        "{}/v1/redirected",
        server_b.uri().replacen("127.0.0.1", "localhost", 1)
    );

    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", redirect_target.as_str()),
        )
        .mount(&server_a)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/redirected"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_should_not_happen",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-opus",
            "content": [{ "type": "text", "text": "should never be reached" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        })))
        .mount(&server_b)
        .await;

    let provider = make_provider(&server_a.uri());
    let req = base_req("claude-3-opus", vec![user_msg("hello")]);

    // The 302 is not followed; the provider surfaces it as an error
    // rather than chasing the Location to server B.
    let err = provider.complete(req).await.unwrap_err();
    match &err {
        Error::Upstream { status, .. } => {
            assert_eq!(
                *status, 502,
                "a 3xx must surface as a mapped upstream server fault, not the bare redirect status"
            );
        }
        other => panic!("expected Error::Upstream from an unfollowed 302, got {other:?}"),
    }
    assert_eq!(
        classify(&err, Some("anthropic-api")).class,
        FailureClass::ServerError,
        "a redirect the client refuses to follow must classify (and retry/fail over) like a server fault"
    );

    let received_a = server_a.received_requests().await.unwrap();
    assert_eq!(received_a.len(), 1, "server A must see exactly one request");
    assert_eq!(
        received_a[0]
            .headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok()),
        Some("test-key"),
        "the real request to the configured host must still carry x-api-key"
    );

    let received_b = server_b.received_requests().await.unwrap();
    assert!(
        received_b.is_empty(),
        "no-redirect client must not follow the 302 to a different host: server B saw {} request(s)",
        received_b.len()
    );
}
