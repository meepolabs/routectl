//! A 403 naming a principal is scrubbed on every surface of the anthropic
//! lane: the carried envelope, the client message, and the failure WARN and
//! DEBUG lines `complete()` emits.

use super::*;
use crate::upstream_error::reason_tests::{
    FIRST_PARTY_403, PRINCIPAL_403_ROWS, assert_principal_scrubbed,
};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn provider_answering(status: u16, body: &str) -> (MockServer, AnthropicApiProvider) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(status)
                .insert_header("content-type", "application/json")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    let mut cfg = AnthropicApiConfig::new("anthropic:test", "test-key");
    cfg.base_url = server.uri();
    (server, AnthropicApiProvider::new(cfg))
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "claude-test".into(),
        messages: vec![routectl_core::test_utils::user_msg("hello")].into(),
        max_tokens: Some(16),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_403_naming_a_principal_is_scrubbed_on_every_surface() {
    for row in &PRINCIPAL_403_ROWS {
        let (_server, provider) = provider_answering(403, row.1).await;

        let (result, events) = routectl_testkit::with_capture(provider.complete(request())).await;

        let err = result.expect_err("a 403 is an error");
        assert_principal_scrubbed("anthropic-api", row, &err, &events);
    }
}

#[tokio::test]
async fn a_first_party_403_naming_no_principal_is_carried_as_read() {
    let (_server, provider) = provider_answering(403, FIRST_PARTY_403).await;

    let err = provider
        .complete(request())
        .await
        .expect_err("a 403 is an error");

    let Error::Upstream { body, .. } = &err else {
        panic!("expected Error::Upstream, got {err:?}");
    };
    assert_eq!(body, FIRST_PARTY_403);
}
