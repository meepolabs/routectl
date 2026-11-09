//! Native bedrock lane redirect behavior: the SigV4 envelope
//! (`x-amz-*`, including the security token) must never ride a
//! cross-host hop, so this lane's client refuses to follow a 3xx and
//! the lane maps the surfaced status to a retryable upstream 502.
//!
//! `BedrockConfig` carries no `base_url` (the endpoint is derived from
//! the region), so a real provider is aimed at the two-server harness
//! through `with_runtime_origin_for_tests` and driven via `complete()`.
//! The error class is resolved under the `bedrock` classification
//! family, which has its own token table, so a 502 is not classified
//! for this lane by the other lanes' passing tests.

use super::*;
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
        additional_model_request_fields: None,
    }
}

#[tokio::test]
async fn lane_client_does_not_follow_cross_host_redirect() {
    let pin = CrossHostRedirect::start().await;
    let resolved = auth::resolve(&redirect_creds(), "us-west-2")
        .await
        .expect("resolve");
    let provider = BedrockProvider::new(redirect_cfg(), resolved).expect("canonical region");

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

fn redirect_request() -> ChatRequest {
    serde_json::from_value(serde_json::json!({
        "model": "claude-x",
        "max_tokens": 64,
        "messages": [{"role": "user", "content": [{"type": "text", "text": "ping"}]}],
    }))
    .expect("fixture is a canonical request")
}

#[tokio::test]
async fn complete_surfaces_cross_host_redirect_as_retryable_502() {
    let pin = CrossHostRedirect::start().await;
    let resolved = auth::resolve(&redirect_creds(), "us-west-2")
        .await
        .expect("resolve");
    let provider = BedrockProvider::new(redirect_cfg(), resolved)
        .expect("canonical region")
        .with_runtime_origin_for_tests(&pin.origin_uri());

    let err = provider
        .complete(redirect_request())
        .await
        .expect_err("a 302 from the configured host must not yield a response");

    pin.assert_not_followed(&err, "bedrock").await;
}
