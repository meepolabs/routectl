//! probe(): free reachability against /v1/models (ApiKey lane only).

use super::*;
use pretty_assertions::assert_eq;

/// A `TokenSource` that counts `token()` calls so the oauth-guard test
/// can prove the probe never resolves (never refreshes) a credential.
#[derive(Default)]
struct CountingTokenSource {
    token_calls: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Debug for CountingTokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CountingTokenSource").finish()
    }
}

#[async_trait::async_trait]
impl routectl_core::TokenSource for CountingTokenSource {
    async fn token(&self) -> routectl_core::Result<String> {
        self.token_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok("oat-token".into())
    }
}

#[tokio::test]
async fn probe_api_key_200_models_list_is_reachable() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
        .expect(1) // AT MOST ONE upstream request: no retry.
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri());
    assert_eq!(
        provider.probe().await,
        routectl_core::ProbeOutcome::Reachable
    );
}

#[tokio::test]
async fn probe_api_key_401_is_auth_failed_without_leaking_credential() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri());
    match provider.probe().await {
        routectl_core::ProbeOutcome::AuthFailed(reason) => {
            assert!(!reason.contains("test-key"), "reason leaked the api key");
            assert!(!reason.contains(&server.uri()), "reason leaked the url");
        }
        other => panic!("expected AuthFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn probe_api_key_403_is_auth_failed() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
        .mount(&server)
        .await;

    let provider = make_provider(&server.uri());
    assert!(matches!(
        provider.probe().await,
        routectl_core::ProbeOutcome::AuthFailed(_)
    ));
}

#[tokio::test]
async fn probe_api_key_connection_refused_is_unreachable() {
    // A closed loopback port (nothing binds 127.0.0.1:1)
    // deterministically refuses the connect.
    let provider = make_provider("http://127.0.0.1:1");
    assert!(matches!(
        provider.probe().await,
        routectl_core::ProbeOutcome::Unreachable(_)
    ));
}

/// BINDING read-only guard: an OauthBearer provider reports
/// `UnsupportedFreeProbe` and makes ZERO token-source calls -- the
/// refreshing `token()` path is never touched, and no upstream
/// request is issued.
#[tokio::test]
async fn probe_oauth_bearer_is_unsupported_with_zero_token_calls() {
    let source = std::sync::Arc::new(CountingTokenSource::default());
    let cfg = AnthropicApiConfig {
        id: "oauth-probe-test".into(),
        auth: source.clone(),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::OauthBearer,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
    };
    let provider = AnthropicApiProvider::new(cfg);

    assert_eq!(
        provider.probe().await,
        routectl_core::ProbeOutcome::UnsupportedFreeProbe
    );
    assert_eq!(
        source.token_calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "the oauth probe guard must never resolve a token",
    );
}
