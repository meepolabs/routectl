//! Native bedrock lane redirect behavior: the SigV4 envelope
//! (`x-amz-*`, including the security token) must never ride a
//! cross-host hop, so this lane's client refuses to follow a 3xx and
//! the lane maps the surfaced status to a retryable upstream 502.
//!
//! Unlike every other credentialed lane, `BedrockConfig` carries no
//! `base_url`: the endpoint is derived from the region
//! (`endpoint::bedrock_runtime_url`), so `complete()` cannot be pointed
//! at a mock server and the two halves of the posture are pinned
//! separately.
//!
//!   - The refusal is pinned against the provider's OWN client -- the
//!     exact `reqwest::Client` a lane-local `Client::builder()`
//!     regression would replace -- driven through the same two-server
//!     harness the other lanes use.
//!   - The error class is pinned on the mapping the lane's `300..400`
//!     arm calls, resolved under the `bedrock` classification family
//!     (which has its own token table, so a 502 is not classified for
//!     this lane by the other lanes' passing tests).

use super::*;
use routectl_core::failure_class::{FailureClass, classify};
use routectl_testkit::redirect_pin::CrossHostRedirect;

const PROVIDER_ID: &str = "bedrock:redirect";

fn redirect_creds() -> BedrockCreds {
    BedrockCreds::BearerKey {
        key: "test-bearer-key".into(),
    }
}

fn redirect_cfg() -> BedrockConfig {
    BedrockConfig {
        id: PROVIDER_ID.into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-haiku-4-5".into(),
        api_shape: BedrockApiShape::Invoke,
        creds: redirect_creds(),
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        allowed_betas: Vec::new(),
        allowed_body_fields: Vec::new(),
        additional_model_request_fields: None,
        adaptive_thinking: None,
    }
}

#[tokio::test]
async fn lane_client_does_not_follow_cross_host_redirect() {
    let pin = CrossHostRedirect::start().await;
    let resolved = auth::resolve(&redirect_creds(), "us-west-2")
        .await
        .expect("resolve");
    let provider = BedrockProvider::new(redirect_cfg(), resolved);

    let resp = provider
        .client
        .post(pin.origin_uri())
        .body(Vec::<u8>::new())
        .send()
        .await
        .expect("the origin must answer");

    // The origin's 302 came back as-is rather than as the target's 200,
    // and the target was never dialed.
    assert_eq!(
        resp.status().as_u16(),
        302,
        "the lane's client must hand back the redirect itself, not the target's response"
    );
    pin.assert_target_untouched().await;
}

#[test]
fn unfollowed_redirect_classifies_as_a_retryable_server_fault() {
    let err = crate::http_client::redirect_not_followed_error(PROVIDER_ID);

    match &err {
        Error::Upstream { status, .. } => assert_eq!(
            *status, 502,
            "a 3xx must surface as a mapped upstream server fault, not the bare redirect status"
        ),
        other => panic!("expected Error::Upstream from an unfollowed 302, got {other:?}"),
    }
    assert_eq!(
        classify(&err, Some("bedrock")).class,
        FailureClass::ServerError,
        "a redirect the client refuses to follow must classify (and retry / fail over) like a server fault"
    );
}
