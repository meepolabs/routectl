//! A relocation refusal on the cloak lane stops complete, stream, and
//! count_tokens alike before any network step: the token source -- the first
//! I/O each path performs after the cloak -- is never consulted.
//!
//! Both guards: every call records the cloak classification split, and the
//! positive control runs the relocation, which can record a policy action.

use std::sync::atomic::{AtomicUsize, Ordering};

use super::*;
use async_trait::async_trait;
use routectl_core::{Message, MessageContent, Role, SystemContent, TokenSource};

#[derive(Debug, Default)]
struct CountingTokenSource {
    calls: AtomicUsize,
}

#[async_trait]
impl TokenSource for CountingTokenSource {
    async fn token(&self) -> Result<String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::Auth("halt after the cloak".into()))
    }
}

fn oauth_provider(auth: Arc<CountingTokenSource>) -> AnthropicApiProvider {
    let mut cfg = AnthropicApiConfig::new_with_auth("cloak-refusal", auth);
    cfg.auth_kind = AuthKind::OauthBearer;
    AnthropicApiProvider::new(cfg)
}

fn message(role: Role, text: &str) -> Message {
    Message {
        refusal: None,
        role,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// A non-CC request (no session capture) whose conversation is system-only.
fn system_only_request() -> ChatRequest {
    ChatRequest {
        model: "claude-sonnet-4-5".into(),
        max_tokens: Some(64),
        system: Some(SystemContent::Text("client rules".into())),
        messages: vec![message(Role::System, "turn rules")].into(),
        ..Default::default()
    }
}

fn assert_local_refusal(err: &Error) {
    assert!(
        matches!(err, Error::LocalRefusal { detail, .. }
            if detail.contains("cannot be relocated")),
        "expected the relocation refusal, got {err:?}"
    );
}

#[serial_test::serial(anthropic_api_cloak_split, anthropic_api_cloak_policy_actions)]
#[tokio::test]
async fn complete_stream_and_count_tokens_share_the_refusal_before_any_network_step() {
    let auth = Arc::new(CountingTokenSource::default());
    let provider = oauth_provider(Arc::clone(&auth));
    let req = system_only_request();

    let complete = provider.complete(req.clone()).await.map(|_| ());
    let stream = provider.stream(req.clone()).await.map(|_| ());
    let count = provider.count_tokens(req).await.map(|_| ());

    for result in [complete, stream, count] {
        assert_local_refusal(&result.expect_err("the cloak must refuse"));
    }
    assert_eq!(
        auth.calls.load(Ordering::SeqCst),
        0,
        "no path may reach token resolution after a refusal"
    );
}

#[serial_test::serial(anthropic_api_cloak_split, anthropic_api_cloak_policy_actions)]
#[tokio::test]
async fn an_assistant_only_history_proceeds_past_the_cloak_on_every_path() {
    // Positive control for the zero above: the same provider DOES reach token
    // resolution on all three paths once the relocation can land.
    let auth = Arc::new(CountingTokenSource::default());
    let provider = oauth_provider(Arc::clone(&auth));
    let mut req = system_only_request();
    req.messages = vec![message(Role::Assistant, "prior")].into();

    let _ = provider.complete(req.clone()).await;
    let _ = provider.stream(req.clone()).await;
    let _ = provider.count_tokens(req).await;

    assert_eq!(auth.calls.load(Ordering::SeqCst), 3);
}
