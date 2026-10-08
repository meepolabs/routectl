//! The minted Claude Code identity is emitted only when the configured base
//! URL is exactly the Anthropic API host. The auth kind alone never
//! authorizes it; explicit operator `header_extras` and `user_agent` values
//! stay host-independent.

use super::client::resolve_user_agent;
use super::*;
use routectl_core::{ChatRequest, StaticToken};
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const GENUINE_HOST: &str = "https://api.anthropic.com";

const NON_ANTHROPIC_HOSTS: &[&str] = &[
    "http://127.0.0.1:18080",
    "https://api.anthropic.com.evil.test",
    "https://evil.test/api.anthropic.com",
    "https://api.anthropic.com@evil.test",
    "https://anthropic.com",
    "https://console.anthropic.com",
    "https://eu.api.anthropic.com",
    "https://api.anthropic.com.anthropic.com",
    "https://gateway.example/api",
];

const STAINLESS_PACK: &[&str] = &[
    "x-app",
    "x-stainless-lang",
    "x-stainless-runtime",
    "x-stainless-runtime-version",
    "x-stainless-package-version",
    "x-stainless-timeout",
    "x-stainless-retry-count",
    "x-stainless-arch",
    "x-stainless-os",
];

const SESSION_IDENTITY: &[&str] = &["x-claude-code-session-id", "x-client-request-id"];

fn cfg(
    base_url: &str,
    auth_kind: AuthKind,
    header_extras: Vec<(String, String)>,
    user_agent: Option<String>,
) -> AnthropicApiConfig {
    AnthropicApiConfig {
        id: "identity-host-test".into(),
        auth: std::sync::Arc::new(StaticToken::new("oat-token")),
        base_url: base_url.to_string(),
        anthropic_version: "2023-06-01".into(),
        auth_kind,
        header_extras,
        user_agent,
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: Some("session-stable-123".into()),
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
        #[cfg(feature = "bedrock")]
        mantle: None,
    }
}

fn oauth_provider(base_url: &str) -> AnthropicApiProvider {
    AnthropicApiProvider::new(cfg(base_url, AuthKind::OauthBearer, Vec::new(), None))
}

fn outbound_header_names(provider: &AnthropicApiProvider) -> Vec<String> {
    let rb = reqwest::Client::new().post("http://127.0.0.1/test");
    let (rb, _decision) = provider.build_headers(rb, &ChatRequest::default(), "test-token", None);
    let request = rb.build().expect("build outbound request");
    request
        .headers()
        .keys()
        .map(|n| n.as_str().to_ascii_lowercase())
        .collect()
}

fn outbound_header_value(provider: &AnthropicApiProvider, name: &str) -> Option<String> {
    let rb = reqwest::Client::new().post("http://127.0.0.1/test");
    let (rb, _decision) = provider.build_headers(rb, &ChatRequest::default(), "test-token", None);
    let request = rb.build().expect("build outbound request");
    request
        .headers()
        .get(name)
        .map(|v| v.to_str().expect("ascii header").to_string())
}

#[test]
fn genuine_anthropic_host_emits_the_full_minted_identity() {
    let names = outbound_header_names(&oauth_provider(GENUINE_HOST));
    for expected in STAINLESS_PACK.iter().chain(SESSION_IDENTITY) {
        assert!(
            names.iter().any(|n| n == expected),
            "{expected} must be emitted on the genuine host; got {names:?}"
        );
    }
    assert_eq!(
        resolve_user_agent(None, AuthKind::OauthBearer, GENUINE_HOST).as_deref(),
        Some(routectl_core::identity::anthropic::default_claude_code_user_agent()),
    );
}

#[test]
fn non_anthropic_hosts_receive_no_stainless_pack() {
    for base_url in NON_ANTHROPIC_HOSTS {
        let names = outbound_header_names(&oauth_provider(base_url));
        for absent in STAINLESS_PACK {
            assert!(
                !names.iter().any(|n| n == absent),
                "{absent} must not be emitted to {base_url}; got {names:?}"
            );
        }
    }
}

#[test]
fn non_anthropic_hosts_receive_no_session_or_request_identity() {
    for base_url in NON_ANTHROPIC_HOSTS {
        let names = outbound_header_names(&oauth_provider(base_url));
        for absent in SESSION_IDENTITY {
            assert!(
                !names.iter().any(|n| n == absent),
                "{absent} must not be emitted to {base_url}; got {names:?}"
            );
        }
    }
}

#[test]
fn non_anthropic_hosts_resolve_no_default_claude_code_user_agent() {
    for base_url in NON_ANTHROPIC_HOSTS {
        assert_eq!(
            resolve_user_agent(None, AuthKind::OauthBearer, base_url),
            None,
            "no default Claude Code UA for {base_url}"
        );
    }
}

#[test]
fn non_anthropic_hosts_keep_the_protocol_headers() {
    for base_url in NON_ANTHROPIC_HOSTS {
        let provider = oauth_provider(base_url);
        assert_eq!(
            outbound_header_value(&provider, "anthropic-version").as_deref(),
            Some("2023-06-01"),
            "anthropic-version must survive on {base_url}"
        );
    }
}

#[test]
fn operator_header_extras_still_reach_non_anthropic_hosts() {
    let extras = vec![
        ("x-stainless-timeout".to_string(), "999".to_string()),
        ("x-app".to_string(), "operator-app".to_string()),
    ];
    for base_url in NON_ANTHROPIC_HOSTS {
        let provider =
            AnthropicApiProvider::new(cfg(base_url, AuthKind::OauthBearer, extras.clone(), None));
        assert_eq!(
            outbound_header_value(&provider, "x-stainless-timeout").as_deref(),
            Some("999"),
            "operator x-stainless-timeout must reach {base_url}"
        );
        assert_eq!(
            outbound_header_value(&provider, "x-app").as_deref(),
            Some("operator-app"),
            "operator x-app must reach {base_url}"
        );
        assert!(
            outbound_header_value(&provider, "x-stainless-lang").is_none(),
            "an operator override must not drag the rest of the pack along on {base_url}"
        );
    }
}

#[test]
fn operator_user_agent_still_resolves_on_non_anthropic_hosts() {
    for base_url in NON_ANTHROPIC_HOSTS {
        assert_eq!(
            resolve_user_agent(Some("op-ua/9.9"), AuthKind::OauthBearer, base_url).as_deref(),
            Some("op-ua/9.9"),
            "operator UA must win on {base_url}"
        );
    }
}

async fn observed_user_agent(user_agent: Option<String>) -> Option<String> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wm_path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "msg_ua",
            "type": "message",
            "role": "assistant",
            "model": "claude-3-opus",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1},
            "content": [{"type": "text", "text": "ok"}]
        })))
        .mount(&server)
        .await;
    let provider = AnthropicApiProvider::new(cfg(
        &server.uri(),
        AuthKind::OauthBearer,
        Vec::new(),
        user_agent,
    ));
    let req = ChatRequest {
        model: "claude-3-opus".into(),
        max_tokens: Some(8),
        ..Default::default()
    };
    let _ = provider.complete(req).await;
    let received = server.received_requests().await.expect("recording enabled");
    assert_eq!(received.len(), 1, "exactly one request must reach the mock");
    received[0]
        .headers
        .get("user-agent")
        .map(|v| v.to_str().expect("ascii UA").to_string())
}

#[tokio::test]
async fn loopback_wire_carries_no_default_claude_code_user_agent() {
    let observed = observed_user_agent(None).await;
    assert!(
        !observed
            .as_deref()
            .is_some_and(|ua| ua.starts_with("claude-cli/")),
        "a loopback base must not present the Claude Code UA; got {observed:?}"
    );
}

#[tokio::test]
async fn loopback_wire_carries_the_operator_user_agent() {
    let observed = observed_user_agent(Some("op-ua/9.9".into())).await;
    assert_eq!(observed.as_deref(), Some("op-ua/9.9"));
}
