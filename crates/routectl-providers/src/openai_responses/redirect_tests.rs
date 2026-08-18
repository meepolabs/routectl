//! openai-responses lane redirect behavior: a 3xx from the configured
//! host is surfaced as an upstream failure rather than followed to a
//! different host, so neither `Authorization: Bearer` nor
//! `chatgpt-account-id` can reach an unintended server.
//!
//! `OpenAiResponsesProvider::new` picks between TWO first-party clients
//! depending on whether a cookie-jar path resolves -- the cookie-backed
//! `build_with_cookie_provider` and the plain `build_no_redirect`. Both
//! are separate builder call sites, so each branch is pinned
//! independently: a regression in one is invisible to a test that only
//! exercises the other.

use super::*;
use routectl_core::{ChatRequest, MessageContent};
use routectl_testkit::ScopedEnv;
use routectl_testkit::redirect_pin::CrossHostRedirect;

fn make_provider(base_url: &str) -> OpenAiResponsesProvider {
    let cfg = OpenAiResponsesConfig {
        id: "openai-responses:test".into(),
        auth: Arc::new(StaticToken::new("test-jwt")) as Arc<dyn TokenSource>,
        account_id: Some("acct-uuid".into()),
        base_url: base_url.to_string(),
        auth_kind: AuthKind::ChatgptOauth,
        header_extras: Vec::new(),
        user_agent: None,
        session_id: None,
        installation_id: None,
        #[cfg(feature = "bedrock")]
        mantle: None,
    };
    OpenAiResponsesProvider::new(cfg)
}

fn base_req() -> ChatRequest {
    ChatRequest {
        model: "gpt-5-codex".into(),
        messages: vec![routectl_core::Message {
            refusal: None,
            role: routectl_core::Role::User,
            content: MessageContent::Text("ping".into()),
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(64),
        ..Default::default()
    }
}

/// Drive the lane against a two-server cross-host redirect and assert the
/// hop was refused. Shared by both client branches so the two tests
/// differ only in the environment that selects the branch.
async fn assert_lane_refuses_cross_host_redirect() {
    let pin = CrossHostRedirect::start().await;
    let provider = make_provider(&pin.origin_uri());

    let err = provider.complete(base_req()).await.unwrap_err();

    pin.assert_not_followed(&err, "openai-responses").await;

    let origin_hits = pin.origin.received_requests().await.unwrap();
    assert_eq!(
        origin_hits[0]
            .headers
            .get("chatgpt-account-id")
            .and_then(|v| v.to_str().ok()),
        Some("acct-uuid"),
        "the real request to the configured host must still carry chatgpt-account-id"
    );
}

/// The cookie-backed branch (`build_with_cookie_provider`): a resolvable
/// jar path selects the client that carries the Cloudflare cookie store.
/// `#[serial_test::serial]` is the `ScopedEnv` contract -- see its module
/// docs.
#[tokio::test]
#[serial_test::serial]
async fn cookie_backed_client_does_not_follow_cross_host_redirect() {
    let jar_dir = tempfile::tempdir().expect("tempdir");
    let _cookie_file = ScopedEnv::set(
        "ROUTECTL_COOKIE_FILE",
        jar_dir.path().join("chatgpt.json").as_os_str(),
    );
    assert!(
        cookies::default_cookie_path().is_some(),
        "the cookie-backed branch requires a resolvable jar path"
    );

    assert_lane_refuses_cross_host_redirect().await;
}

/// The cookie-less fallback branch (`build_no_redirect`): with neither
/// `ROUTECTL_COOKIE_FILE` nor `HOME` set, no jar path resolves and
/// construction takes the plain builder.
#[tokio::test]
#[serial_test::serial]
async fn cookie_less_client_does_not_follow_cross_host_redirect() {
    let _cookie_file = ScopedEnv::unset("ROUTECTL_COOKIE_FILE");
    let _home = ScopedEnv::unset("HOME");
    assert!(
        cookies::default_cookie_path().is_none(),
        "the cookie-less branch requires an unresolvable jar path"
    );

    assert_lane_refuses_cross_host_redirect().await;
}
