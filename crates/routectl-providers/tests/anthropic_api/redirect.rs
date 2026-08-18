//! First-party anthropic-api lane redirect behavior: pins that a 3xx
//! from the configured host is surfaced as an upstream failure rather
//! than followed to a different host, so `x-api-key` never reaches an
//! unintended server.

use super::*;
use routectl_testkit::redirect_pin::CrossHostRedirect;

/// The first-party lane uses a no-redirect client, same as the mantle
/// lane: a 3xx from the configured host must never be chased to a
/// `Location` target on a DIFFERENT host, since that would carry
/// `x-api-key` off the configured host. Two separate `MockServer`
/// instances stand in for two distinct hosts -- the origin answers the
/// real request with a 302 pointing at the target; the target answers
/// 200 (so a followed hop would look like a success) and must never see
/// a request at all.
#[tokio::test]
async fn first_party_lane_does_not_follow_cross_host_redirect() {
    let pin = CrossHostRedirect::start().await;

    let provider = make_provider(&pin.origin_uri());
    let req = base_req("claude-3-opus", vec![user_msg("hello")]);

    // The 302 is not followed; the provider surfaces it as an error
    // rather than chasing the Location to the target host.
    let err = provider.complete(req).await.unwrap_err();
    pin.assert_not_followed(&err, "anthropic-api").await;

    let origin_hits = pin.origin.received_requests().await.unwrap();
    assert_eq!(
        origin_hits[0]
            .headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok()),
        Some("test-key"),
        "the real request to the configured host must still carry x-api-key"
    );
}
