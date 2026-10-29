//! A client-derived header value the HTTP stack cannot send fails the
//! request build on every send path, before anything leaves the process.
//! That local failure is `Error::Internal`, never a status-0 network error,
//! and its message never echoes the offending value.

use routectl_core::failure_class::{FailureClass, classify};
use routectl_core::{ChatRequest, Message, MessageContent, Provider, Role};

use super::*;

const PROVIDER_ID: &str = "build-error-seat";
const UNSENDABLE_BETA: &str = "client-flag\nx";

/// An api-key seat on a host nothing listens on: a request that got past
/// the build would fail as a network error, not as `Internal`.
fn provider() -> AnthropicApiProvider {
    AnthropicApiProvider::new(AnthropicApiConfig {
        id: PROVIDER_ID.into(),
        auth: std::sync::Arc::new(routectl_core::StaticToken::new("test-key")),
        base_url: "http://127.0.0.1:9".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
        #[cfg(feature = "bedrock")]
        mantle: None,
    })
}

fn request(beta: &str) -> ChatRequest {
    ChatRequest {
        model: "claude-x".into(),
        max_tokens: Some(16),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        anthropic_beta: vec![beta.into()],
        ..Default::default()
    }
}

#[derive(Debug, Clone, Copy)]
enum SendPath {
    Complete,
    Stream,
    CountTokens,
}

async fn send(path: SendPath, req: ChatRequest) -> routectl_core::Error {
    let provider = provider();
    match path {
        SendPath::Complete => provider.complete(req).await.map(|_| ()),
        SendPath::Stream => provider.stream(req).await.map(|_| ()),
        SendPath::CountTokens => provider.count_tokens(req).await.map(|_| ()),
    }
    .expect_err("nothing is listening on the seat's host")
}

#[tokio::test]
async fn an_unsendable_client_header_fails_the_build_as_internal_on_every_send_path() {
    for path in [SendPath::Complete, SendPath::Stream, SendPath::CountTokens] {
        let err = send(path, request(UNSENDABLE_BETA)).await;

        let routectl_core::Error::Internal(message) = &err else {
            panic!("{path:?}: expected Error::Internal, got {err:?}");
        };
        assert!(
            message.starts_with(&format!("provider `{PROVIDER_ID}`: build request: ")),
            "{path:?}: {message}"
        );
        assert!(!message.contains("client-flag"), "{path:?}: {message}");
        assert_eq!(
            classify(&err, None).class,
            FailureClass::Unknown,
            "{path:?}"
        );
    }
}

/// Control: the same request with a sendable flag gets past the build and
/// fails as the status-0 network error a refused connection is.
#[tokio::test]
async fn a_sendable_client_header_reaches_the_network_on_every_send_path() {
    for path in [SendPath::Complete, SendPath::Stream, SendPath::CountTokens] {
        let err = send(path, request("client-flag")).await;

        assert!(
            matches!(err, routectl_core::Error::Upstream { status: 0, .. }),
            "{path:?}: expected a status-0 upstream error, got {err:?}"
        );
    }
}
