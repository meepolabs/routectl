//! The plan's client-identity verdict, carried by the factory into the
//! anthropic-api provider, decides what reaches the wire: a managed login on
//! its own host sends the Claude Code identity, and every other entry sends
//! none of it, whatever its auth kind or host.
//!
//! Each row builds its provider config through the factory from a real
//! entry, so the verdict and the session-id read are the factory's own. Only
//! the transport is then pointed at a mock: the base keeps the
//! `api.anthropic.com` host (over plain http on the mock's port), so every
//! host-keyed decision in the egress still sees the first-party host.

use super::*;
use crate::factory::BuildOptions;
use crate::factory::build::anthropic_api_config;
use async_trait::async_trait;
use routectl_auth::{SecretRef, SecretStore};
use routectl_core::{ChatRequest, Message, MessageContent, Provider, Role, SystemContent};
use routectl_providers::anthropic_api::{AnthropicApiProvider, AuthKind};
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const LOGIN_SESSION: &str = "login-session-0001";
const IDENTITY_LINE: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const STAINLESS_PACK: &[&str] = &["x-app", "x-stainless-lang", "x-stainless-package-version"];

/// Answers every credential read, and has a login session id for any
/// reference, so a row that never reads it proves the factory skipped it.
struct LoginStore;

#[async_trait]
impl SecretStore for LoginStore {
    async fn get(&self, _: &SecretRef) -> routectl_core::Result<String> {
        Ok("stored-token".to_string())
    }
    async fn set(&self, _: &SecretRef, _: &str) -> routectl_core::Result<()> {
        Ok(())
    }
    async fn delete(&self, _: &SecretRef) -> routectl_core::Result<()> {
        Ok(())
    }
    async fn peek_session_id(&self, _: &SecretRef) -> Option<String> {
        Some(LOGIN_SESSION.to_string())
    }
}

/// What one request put on the wire.
struct Wire {
    sent: wiremock::Request,
    body: serde_json::Value,
}

impl Wire {
    fn header(&self, name: &str) -> Option<&str> {
        self.sent.headers.get(name).and_then(|v| v.to_str().ok())
    }

    fn system_text(&self) -> String {
        self.body
            .get("system")
            .map(serde_json::Value::to_string)
            .unwrap_or_default()
    }

    fn carries_identity_line(&self) -> bool {
        self.system_text().contains(IDENTITY_LINE)
    }

    fn carries_minted_user_id(&self) -> bool {
        self.body
            .get("metadata")
            .and_then(|m| m.get("user_id"))
            .is_some()
    }
}

fn anthropic(r: &str, base_url: &str, auth_kind: AuthKind) -> ProviderEntry {
    ProviderEntry::anthropic_api(r)
        .with_auth_kind(auth_kind)
        .with_base_url(base_url)
}

/// A request from a client that is not Claude Code: no session capture, so
/// a provider presenting the identity cloaks it.
fn request(forwarded: bool) -> ChatRequest {
    let mut req = ChatRequest {
        model: "claude-x".into(),
        system: Some(SystemContent::Text("client rules".into())),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extras: serde_json::Map::new(),
        }]
        .into(),
        max_tokens: Some(16),
        ..Default::default()
    };
    if forwarded {
        req.routectl_internal.forwarded_bearer = Some(routectl_core::ForwardedBearer::new(
            "sk-ant-oat01-forwarded".into(),
        ));
    }
    req
}

/// Build `entry` through the factory, point its transport at a fresh mock
/// (keeping the first-party host unless `keep_base`), send one request and
/// return what the mock received.
async fn wire_of(entry: &ProviderEntry, keep_base: bool, forwarded: bool) -> Wire {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "msg_w",
            "type": "message",
            "role": "assistant",
            "model": "claude-x",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(&server)
        .await;
    let plan = plan_provider("p", entry).expect("the entry plans");
    let store: Arc<dyn SecretStore> = Arc::new(LoginStore);
    let mut cfg = anthropic_api_config("p", entry, plan, &store, &BuildOptions::new())
        .await
        .expect("the factory builds the config");
    let addr = *server.address();
    let provider = if keep_base {
        cfg.base_url = server.uri();
        AnthropicApiProvider::new(cfg)
    } else {
        cfg.base_url = format!("http://api.anthropic.com:{}", addr.port());
        AnthropicApiProvider::new(cfg).with_host_resolved_to_for_tests(addr)
    };

    let _ = provider.complete(request(forwarded)).await;

    let mut received = server.received_requests().await.expect("recording enabled");
    assert_eq!(received.len(), 1, "exactly one request reaches the mock");
    let sent = received.remove(0);
    let body = serde_json::from_slice(&sent.body).expect("a JSON body");
    Wire { sent, body }
}

#[tokio::test]
async fn a_managed_login_on_its_host_sends_the_claude_code_identity() {
    let entry = anthropic(
        "oauth://anthropic",
        "https://api.anthropic.com",
        AuthKind::OauthBearer,
    );

    let wire = wire_of(&entry, false, false).await;

    for name in STAINLESS_PACK {
        assert!(wire.header(name).is_some(), "{name} missing");
    }
    assert!(
        wire.header("user-agent")
            .is_some_and(|ua| ua.starts_with("claude-cli/")),
        "user-agent: {:?}",
        wire.header("user-agent")
    );
    assert_eq!(wire.header("x-claude-code-session-id"), Some(LOGIN_SESSION));
    assert!(
        wire.carries_identity_line(),
        "system: {}",
        wire.system_text()
    );
    assert!(wire.carries_minted_user_id(), "body: {}", wire.body);
}

/// One absence row: an entry and how to reach it on the wire.
struct Absent {
    label: &'static str,
    entry: ProviderEntry,
    keep_base: bool,
    forwarded: bool,
}

fn absence_rows() -> Vec<Absent> {
    #[cfg_attr(not(feature = "bedrock"), allow(unused_mut))]
    let mut rows = vec![
        Absent {
            label: "api key on api.anthropic.com",
            entry: anthropic(
                "env://ANTHROPIC_API_KEY",
                "https://api.anthropic.com",
                AuthKind::ApiKey,
            ),
            keep_base: false,
            forwarded: false,
        },
        Absent {
            label: "env bearer on api.anthropic.com",
            entry: anthropic(
                "env://ANTHROPIC_TOKEN",
                "https://api.anthropic.com",
                AuthKind::OauthBearer,
            ),
            keep_base: false,
            forwarded: false,
        },
        Absent {
            label: "file bearer on api.anthropic.com",
            entry: anthropic(
                "file:///nonexistent/routectl-identity-test-token",
                "https://api.anthropic.com",
                AuthKind::OauthBearer,
            ),
            keep_base: false,
            forwarded: false,
        },
        Absent {
            label: "bearer on an OpenRouter-like host",
            entry: anthropic(
                "env://OPENROUTER_API_KEY",
                "https://openrouter.ai/api",
                AuthKind::OauthBearer,
            ),
            keep_base: true,
            forwarded: false,
        },
        Absent {
            label: "forwarded leg",
            entry: ProviderEntry::anthropic_api("")
                .with_auth_kind(AuthKind::OauthBearer)
                .with_credential_source(crate::config::CredentialSource::Forwarded),
            keep_base: false,
            forwarded: true,
        },
    ];
    #[cfg(feature = "bedrock")]
    rows.push(Absent {
        label: "anthropic mantle",
        entry: {
            let cfg: crate::config::Config = toml::from_str(
                "[providers.p]\n\
                 kind = \"anthropic-api\"\n\
                 api_key_ref = \"\"\n\
                 bedrock_mantle = { region = \"us-east-1\", creds = { kind = \"bearer-key\", \
                 key_ref = \"env://ROUTECTL_IDENTITY_TEST_MANTLE\" } }\n",
            )
            .expect("fixture parses");
            cfg.providers.into_values().next().expect("one provider")
        },
        keep_base: true,
        forwarded: false,
    });
    rows
}

/// Every part of the identity `wire` carries, by name.
fn identity_on(wire: &Wire) -> Vec<String> {
    let mut found: Vec<String> = STAINLESS_PACK
        .iter()
        .chain(&["x-claude-code-session-id", "x-client-request-id"])
        .filter(|name| wire.header(name).is_some())
        .map(|name| (*name).to_string())
        .collect();
    if wire
        .header("user-agent")
        .is_some_and(|ua| ua.starts_with("claude-cli/"))
    {
        found.push("claude code user-agent".to_string());
    }
    if wire.carries_identity_line() {
        found.push("cloak identity line".to_string());
    }
    if wire.carries_minted_user_id() {
        found.push("cloak metadata user_id".to_string());
    }
    found
}

#[tokio::test]
async fn every_other_entry_sends_no_identity_and_no_cloak() {
    let mut violations = Vec::new();
    for row in absence_rows() {
        let wire = wire_of(&row.entry, row.keep_base, row.forwarded).await;

        let found = identity_on(&wire);

        if !found.is_empty() {
            violations.push(format!("{}: {found:?}", row.label));
        }
    }
    assert!(violations.is_empty(), "{violations:#?}");
}
