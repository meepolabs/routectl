//! The codex client identity reaches the wire exactly when the config's
//! `client_identity` says so, whatever the auth kind; the wire dialect
//! still follows the auth kind.

use std::sync::Arc;

use routectl_core::identity::codex::{
    CODEX_ORIGINATOR, RESIDENCY_HEADER_VALUE, codex_user_agent, is_codex_cli_user_agent,
    resolved_identity,
};
use routectl_core::{ChatRequest, Message, MessageContent, Provider, Role, StaticToken};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{AuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider};

const SESSION_ID: &str = "session-wire-1";
const INSTALLATION_ID: &str = "0f5c9e4a-2b1d-4c3e-8f7a-1d2e3f4a5b6c";

/// Every header the codex client identity adds, generated or compiled.
const IDENTITY_HEADERS: [&str; 8] = [
    "originator",
    "x-openai-internal-codex-residency",
    "version",
    "session-id",
    "x-codex-installation-id",
    "thread-id",
    "x-client-request-id",
    "x-codex-window-id",
];

fn completed_sse() -> String {
    let completed = json!({
        "id": "resp_identity",
        "object": "response",
        "status": "completed",
        "model": "gpt-5",
        "output": [{
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "pong"}]
        }],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
    });
    format!("data: {{\"type\":\"response.completed\",\"response\":{completed}}}\n\n")
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "gpt-5".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("ping".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            extras: serde_json::Map::new(),
        }]
        .into(),
        max_tokens: Some(64),
        ..Default::default()
    }
}

/// A config carrying a session id and an installation id, so an absent
/// header on the wire is the gate's doing, never a missing value.
fn config(base_url: &str, auth_kind: AuthKind, client_identity: bool) -> OpenAiResponsesConfig {
    let mut cfg = OpenAiResponsesConfig::new_with_auth(
        "openai-responses:identity",
        Arc::new(StaticToken::new("wire-token")),
    );
    cfg.base_url = base_url.to_string();
    cfg.auth_kind = auth_kind;
    cfg.account_id = (auth_kind == AuthKind::ChatgptOauth).then(|| "acct-uuid".to_string());
    cfg.session_id = Some(SESSION_ID.into());
    cfg.installation_id = Some(INSTALLATION_ID.into());
    cfg.client_identity = client_identity;
    cfg
}

/// What the upstream received for one completion.
struct Captured {
    headers: wiremock::http::HeaderMap,
    body: Value,
}

impl Captured {
    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }
}

async fn send(provider: &OpenAiResponsesProvider, server: &MockServer) -> Captured {
    provider
        .complete(request())
        .await
        .expect("complete against the mock upstream");
    let received = server.received_requests().await.expect("captured requests");
    let last = received.last().expect("one upstream request");
    Captured {
        headers: last.headers.clone(),
        body: serde_json::from_slice(&last.body).expect("json body"),
    }
}

async fn upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(completed_sse()),
        )
        .mount(&server)
        .await;
    server
}

fn installation_in_body(body: &Value) -> Option<&Value> {
    body.get("client_metadata")
        .and_then(|m| m.get("x-codex-installation-id"))
}

fn assert_no_identity(case: &str, wire: &Captured) {
    for name in IDENTITY_HEADERS {
        assert_eq!(wire.header(name), None, "{case}: {name} must not be sent");
    }
    assert!(
        installation_in_body(&wire.body).is_none(),
        "{case}: no installation id in client_metadata: {}",
        wire.body
    );
    let ua = wire.header("user-agent").unwrap_or_default();
    assert!(
        !is_codex_cli_user_agent(&ua),
        "{case}: no codex User-Agent: {ua:?}"
    );
}

#[tokio::test]
async fn client_identity_sends_the_codex_identity_on_the_wire() {
    let server = upstream().await;
    let provider =
        OpenAiResponsesProvider::new(config(&server.uri(), AuthKind::ChatgptOauth, true));

    let first = send(&provider, &server).await;
    let second = send(&provider, &server).await;

    assert_eq!(
        first.header("user-agent").as_deref(),
        Some(codex_user_agent())
    );
    assert_eq!(
        first.header("originator").as_deref(),
        Some(CODEX_ORIGINATOR)
    );
    assert_eq!(
        first.header("x-openai-internal-codex-residency").as_deref(),
        Some(RESIDENCY_HEADER_VALUE)
    );
    assert_eq!(
        first.header("version").as_deref(),
        Some(resolved_identity().version())
    );
    assert_eq!(first.header("session-id").as_deref(), Some(SESSION_ID));
    assert_eq!(
        first.header("x-codex-installation-id").as_deref(),
        Some(INSTALLATION_ID)
    );
    assert_eq!(
        installation_in_body(&first.body),
        Some(&Value::String(INSTALLATION_ID.into()))
    );
    let thread = first.header("thread-id").expect("thread-id sent");
    assert_eq!(first.header("x-client-request-id"), Some(thread.clone()));
    assert_ne!(
        second.header("thread-id"),
        Some(thread),
        "thread-id rotates"
    );
    let window = first.header("x-codex-window-id").expect("window id sent");
    assert_eq!(second.header("x-codex-window-id"), Some(window));
}

#[tokio::test]
async fn first_party_lanes_without_client_identity_send_none_of_it() {
    for (case, auth_kind) in [
        ("responses api-key", AuthKind::ApiKey),
        ("chatgpt dialect with an env bearer", AuthKind::ChatgptOauth),
    ] {
        let server = upstream().await;
        let provider = OpenAiResponsesProvider::new(config(&server.uri(), auth_kind, false));

        let wire = send(&provider, &server).await;

        assert_no_identity(case, &wire);
        assert_eq!(
            wire.header("authorization").as_deref(),
            Some("Bearer wire-token"),
            "{case}: the credential is still sent"
        );
    }
}

/// The dialect follows the auth kind, not the identity: the chatgpt
/// dialect withholds `max_output_tokens` with or without the identity,
/// and the api-key dialect sends it.
#[tokio::test]
async fn the_wire_dialect_follows_the_auth_kind_not_the_identity() {
    for (case, auth_kind, client_identity, sends_ceiling) in [
        ("chatgpt with identity", AuthKind::ChatgptOauth, true, false),
        (
            "chatgpt without identity",
            AuthKind::ChatgptOauth,
            false,
            false,
        ),
        ("api-key", AuthKind::ApiKey, false, true),
    ] {
        let server = upstream().await;
        let provider =
            OpenAiResponsesProvider::new(config(&server.uri(), auth_kind, client_identity));

        let wire = send(&provider, &server).await;

        assert_eq!(
            wire.body.get("max_output_tokens").is_some(),
            sends_ceiling,
            "{case}: {}",
            wire.body
        );
    }
}

/// A Mantle provider never presents the codex identity, even when a caller
/// sets the flag: the AWS-signed lane is not the codex client.
#[cfg(feature = "bedrock")]
#[tokio::test]
async fn the_mantle_lane_sends_no_codex_identity_even_when_asked() {
    use crate::bedrock::BedrockCreds;
    use crate::bedrock::auth::resolve;
    use crate::mantle::MantleAuth;

    for client_identity in [false, true] {
        let server = upstream().await;
        let creds = BedrockCreds::BearerKey {
            key: "mantle-wire-bearer".into(),
        };
        let mut cfg = config(&server.uri(), AuthKind::ChatgptOauth, client_identity);
        cfg.account_id = None;
        cfg.mantle = Some(MantleAuth {
            region: "us-west-2".into(),
            creds: resolve(&creds, "us-west-2").await.expect("bearer resolves"),
        });
        let provider = OpenAiResponsesProvider::new(cfg);

        let wire = send(&provider, &server).await;

        assert_no_identity(&format!("mantle client_identity={client_identity}"), &wire);
    }
}
