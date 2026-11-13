//! Every upstream-failure log line on this lane renders the raw upstream
//! message through the scrub: the shared WARN on `complete`, `stream` and
//! `count_tokens`, the count_tokens 501 DEBUG line, and the OAuth 4xx
//! beta-decision WARN. Each row plants a credential in the upstream
//! `error.message` and checks the line's `body_excerpt` keeps the clean
//! sentinel and none of the credential.

use super::*;
use crate::upstream_log::tests::{assert_excerpt_scrubbed, planted_messages};
use routectl_core::StaticToken;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

const UPSTREAM_ERROR: &str = "upstream error";
const UPSTREAM_AUTH: &str = "upstream auth failed";
const BETA_CONTEXT: &str = "anthropic-api oauth 4xx beta decision context";
const COUNT_501: &str =
    "count_tokens unsupported by upstream (501); router walks to next capable seat";

#[derive(Clone, Copy, Debug)]
enum Call {
    Complete,
    Stream,
    CountTokens,
}

struct Sink {
    name: &'static str,
    call: Call,
    status: u16,
    message: &'static str,
    level: tracing::Level,
}

const SINKS: [Sink; 7] = [
    Sink {
        name: "complete error",
        call: Call::Complete,
        status: 500,
        message: UPSTREAM_ERROR,
        level: tracing::Level::WARN,
    },
    Sink {
        name: "complete auth",
        call: Call::Complete,
        status: 401,
        message: UPSTREAM_AUTH,
        level: tracing::Level::WARN,
    },
    Sink {
        name: "stream error",
        call: Call::Stream,
        status: 500,
        message: UPSTREAM_ERROR,
        level: tracing::Level::WARN,
    },
    Sink {
        name: "count_tokens error",
        call: Call::CountTokens,
        status: 500,
        message: UPSTREAM_ERROR,
        level: tracing::Level::WARN,
    },
    Sink {
        name: "count_tokens 501",
        call: Call::CountTokens,
        status: 501,
        message: COUNT_501,
        level: tracing::Level::DEBUG,
    },
    Sink {
        name: "beta 4xx on complete",
        call: Call::Complete,
        status: 400,
        message: BETA_CONTEXT,
        level: tracing::Level::WARN,
    },
    Sink {
        name: "beta 4xx on count_tokens",
        call: Call::CountTokens,
        status: 400,
        message: BETA_CONTEXT,
        level: tracing::Level::WARN,
    },
];

/// An own-token OAuth provider on the exact Anthropic host, resolved to
/// the mock, so the beta-decision WARN's lane gate holds.
fn oauth_provider_on(server: &MockServer) -> AnthropicApiProvider {
    let addr = *server.address();
    let cfg = AnthropicApiConfig {
        id: "warn-scrub-test".into(),
        auth: std::sync::Arc::new(StaticToken::new("oat-token")),
        base_url: format!("http://api.anthropic.com:{}", addr.port()),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::OauthBearer,
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
    };
    let mut provider = AnthropicApiProvider::new(cfg);
    provider.client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .resolve("api.anthropic.com", addr)
        .build()
        .expect("test client");
    provider
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "claude-sonnet-4-6".into(),
        messages: vec![routectl_core::test_utils::user_msg("hi")].into(),
        max_tokens: Some(64),
        ..Default::default()
    }
}

fn error_envelope(message: &str) -> String {
    serde_json::json!({
        "type": "error",
        "error": {"type": "api_error", "message": message},
    })
    .to_string()
}

async fn drive(call: Call, provider: &AnthropicApiProvider) -> Option<Error> {
    match call {
        Call::Complete => provider.complete(request()).await.err(),
        Call::Stream => provider.stream(request()).await.err(),
        Call::CountTokens => provider.count_tokens(request()).await.err(),
    }
}

#[tokio::test]
async fn every_upstream_failure_line_scrubs_the_raw_message() {
    for sink in &SINKS {
        for (row, text, planted) in planted_messages() {
            let name = format!("{}/{row}", sink.name);
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(sink.status)
                        .insert_header("content-type", "application/json")
                        .set_body_string(error_envelope(&text)),
                )
                .mount(&server)
                .await;
            let provider = oauth_provider_on(&server);

            let (err, events) = routectl_testkit::with_capture(drive(sink.call, &provider)).await;

            assert!(
                matches!(err, Some(Error::Upstream { status, .. }) if status == sink.status),
                "{name}: expected the upstream error, got {err:?}"
            );
            let lines: Vec<_> = events
                .iter()
                .filter(|e| e.message == sink.message)
                .collect();
            assert_eq!(lines.len(), 1, "{name}: captured {events:?}");
            assert_eq!(lines[0].level, sink.level, "{name}");
            assert_excerpt_scrubbed(&name, lines[0].field("body_excerpt"), planted);
        }
    }
}

#[tokio::test]
async fn every_upstream_failure_line_scrubs_a_body_without_a_message_uncut() {
    use crate::upstream_log::tests::planted_bodies_without_message;
    for sink in &SINKS {
        for (row, body, planted) in planted_bodies_without_message() {
            let name = format!("{}/{row}", sink.name);
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(sink.status).set_body_string(body))
                .mount(&server)
                .await;
            let provider = oauth_provider_on(&server);

            let (err, events) = routectl_testkit::with_capture(drive(sink.call, &provider)).await;

            assert!(
                matches!(err, Some(Error::Upstream { status, .. }) if status == sink.status),
                "{name}: expected the upstream error, got {err:?}"
            );
            let lines: Vec<_> = events
                .iter()
                .filter(|e| e.message == sink.message)
                .collect();
            assert_eq!(lines.len(), 1, "{name}: captured {events:?}");
            assert_excerpt_scrubbed(&name, lines[0].field("body_excerpt"), planted);
        }
    }
}
