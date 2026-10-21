//! Behavior that depends on whether the seat is AWS-backed, over HTTP on
//! each lane: the credential reason is attached only on a seat configured
//! for the Bedrock Mantle lane, and every 403 on such a seat is carried and
//! logged as routectl's own access-denied string.

use std::sync::Arc;

use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use routectl_core::UpstreamReason::{CredentialExpired, CredentialInvalid};
use routectl_core::{ChatRequest, ChatResponse, Error, Provider, UpstreamReason};

use crate::bedrock::BedrockCreds;
use crate::bedrock::auth::resolve;
use crate::mantle::MantleAuth;
use crate::upstream_error::reason_tests::{FIRST_PARTY_403, MANTLE_CAPTURE, captured};

/// A 403 on a Mantle seat naming a principal without any marker the
/// defense-in-depth check knows: the seat alone must send it through the
/// scrub. The fragments must appear on no surface.
const MARKER_FREE_MANTLE_403: &str = r#"{"error":{"type":"permission_denied_error","message":"role assumed-role/R/alice in account 1234-5678-9012 is not authorized"}}"#;
const MARKER_FREE_MANTLE_403_MESSAGE: &str = "bedrock access denied (permission_denied_error)";
const MARKER_FREE_FRAGMENTS: [&str; 3] = ["alice", "assumed-role", "5678-9012"];

/// Assert `err` carries the marker-free Mantle 403 as the scrubbed envelope
/// and that no fragment of it reaches the carried body or any log field.
fn assert_marker_free_scrubbed(
    lane: &str,
    err: &routectl_core::Error,
    events: &[routectl_testkit::CapturedEvent],
) {
    let routectl_core::Error::Upstream { body, .. } = err else {
        panic!("{lane}: expected Error::Upstream, got {err:?}");
    };
    let carried = err
        .parsed_body()
        .and_then(routectl_core::ParsedErrorBody::error_message);
    assert_eq!(carried, Some(MARKER_FREE_MANTLE_403_MESSAGE), "{lane}");
    assert!(
        events.iter().any(|e| e.level == tracing::Level::WARN),
        "{lane}: the failure WARN fired"
    );
    assert!(
        events.iter().any(|e| e.message == "upstream error body"),
        "{lane}: the DEBUG body line fired"
    );
    let logged = events
        .iter()
        .flat_map(|e| e.fields.iter().map(|(_, v)| v.as_str()));
    for text in std::iter::once(body.as_str()).chain(logged) {
        for fragment in MARKER_FREE_FRAGMENTS {
            assert!(!text.contains(fragment), "{lane} leaked {fragment}: {text}");
        }
    }
}

/// Which lane a row runs on.
#[derive(Clone, Copy, Debug)]
enum Lane {
    OpenAiCompat,
    OpenAiResponses,
    Anthropic,
}

const LANES: [Lane; 3] = [Lane::OpenAiCompat, Lane::OpenAiResponses, Lane::Anthropic];

async fn server_answering(status: u16, body: &str) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(status)
                .insert_header("content-type", "application/json")
                .set_body_string(body),
        )
        .mount(&server)
        .await;
    server
}

async fn mantle_auth() -> MantleAuth {
    let creds = BedrockCreds::BearerKey {
        key: "seat-test-key".into(),
    };
    MantleAuth {
        region: "us-west-2".into(),
        creds: resolve(&creds, "us-west-2").await.expect("resolve"),
    }
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: vec![routectl_core::test_utils::user_msg("hello")].into(),
        max_tokens: Some(16),
        ..Default::default()
    }
}

async fn openai_compat_provider(base_url: String, mantle: bool) -> Box<dyn Provider> {
    use crate::openai_compat::{
        HistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect,
    };
    Box::new(OpenAiCompatProvider::new(OpenAiCompatConfig {
        id: "seat-compat".into(),
        base_url,
        api_key: if mantle { String::new() } else { "k".into() },
        header_extras: vec![],
        payload_extras: None,
        reasoning_dialect: ReasoningDialect::OpenAi,
        history_reasoning: HistoryReasoning::Auto,
        user_agent: None,
        strict_translation: false,
        mantle: if mantle {
            Some(mantle_auth().await)
        } else {
            None
        },
    }))
}

async fn openai_responses_provider(base_url: String, mantle: bool) -> Box<dyn Provider> {
    use crate::openai_responses::{AuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider};
    use routectl_core::{StaticToken, TokenSource};
    Box::new(OpenAiResponsesProvider::new(OpenAiResponsesConfig {
        id: "seat-responses".into(),
        auth: Arc::new(StaticToken::new(if mantle { "" } else { "k" })) as Arc<dyn TokenSource>,
        account_id: None,
        base_url,
        auth_kind: if mantle {
            AuthKind::BedrockMantle
        } else {
            AuthKind::ApiKey
        },
        header_extras: Vec::new(),
        user_agent: None,
        session_id: None,
        installation_id: None,
        mantle: if mantle {
            Some(mantle_auth().await)
        } else {
            None
        },
    }))
}

async fn anthropic_provider(base_url: String, mantle: bool) -> Box<dyn Provider> {
    use crate::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};
    let mut cfg = AnthropicApiConfig::new("seat-anthropic", if mantle { "" } else { "k" });
    cfg.base_url = base_url;
    if mantle {
        cfg.mantle = Some(mantle_auth().await);
    }
    Box::new(AnthropicApiProvider::new(cfg))
}

/// `complete()` on `lane` against an upstream answering `status` / `body`,
/// on a Mantle-configured (`mantle`) or first-party seat.
async fn complete_on(
    lane: Lane,
    status: u16,
    body: &str,
    mantle: bool,
) -> routectl_core::Result<ChatResponse> {
    let server = server_answering(status, body).await;
    let provider = match lane {
        Lane::OpenAiCompat => openai_compat_provider(server.uri(), mantle).await,
        Lane::OpenAiResponses => openai_responses_provider(server.uri(), mantle).await,
        Lane::Anthropic => anthropic_provider(server.uri(), mantle).await,
    };
    provider.complete(request()).await
}

async fn capture_reason(lane: Lane, key: &str, mantle: bool) -> Option<UpstreamReason> {
    let row = captured(MANTLE_CAPTURE, key);
    match complete_on(lane, row.status, &row.body, mantle).await {
        Err(err @ Error::Upstream { .. }) => err.upstream_reason(),
        other => panic!("{lane:?} {key}: expected Error::Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn the_credential_reason_follows_the_seat() {
    let openai_rows = [
        ("mantle-chat-expired", CredentialExpired),
        ("mantle-chat-expired-security-token", CredentialExpired),
        ("mantle-chat-invalid", CredentialInvalid),
    ];
    let anthropic_rows = [
        ("mantle-anthropic-expired", CredentialExpired),
        ("mantle-anthropic-expired-security-token", CredentialExpired),
        ("mantle-anthropic-invalid", CredentialInvalid),
    ];
    for lane in LANES {
        let rows = match lane {
            Lane::Anthropic => anthropic_rows,
            Lane::OpenAiCompat | Lane::OpenAiResponses => openai_rows,
        };
        for (key, want) in rows {
            let on_mantle = capture_reason(lane, key, true).await;
            let first_party = capture_reason(lane, key, false).await;

            assert_eq!(on_mantle, Some(want), "{lane:?} mantle {key}");
            assert_eq!(first_party, None, "{lane:?} first-party {key}");
        }
    }
}

#[tokio::test]
async fn every_403_on_a_mantle_seat_is_scrubbed_without_any_marker() {
    for lane in LANES {
        let (result, events) =
            routectl_testkit::with_capture(complete_on(lane, 403, MARKER_FREE_MANTLE_403, true))
                .await;

        let err = result.expect_err("a 403 is an error");
        assert_marker_free_scrubbed(&format!("{lane:?}"), &err, &events);
    }
}

#[tokio::test]
async fn a_first_party_403_naming_no_principal_on_a_first_party_seat_is_carried_as_read() {
    for lane in LANES {
        let err = complete_on(lane, 403, FIRST_PARTY_403, false)
            .await
            .expect_err("a 403 is an error");

        let Error::Upstream { body, .. } = &err else {
            panic!("{lane:?}: expected Error::Upstream, got {err:?}");
        };
        assert_eq!(body, FIRST_PARTY_403, "{lane:?}");
    }
}

/// A 200 whose body is an inline `{"error":{...}}` envelope with a numeric
/// 403 code, naming a principal without any marker.
const INLINE_MARKER_FREE_403: &str = r#"{"error":{"code":403,"message":"role assumed-role/R/alice in account 1234-5678-9012 is not authorized"}}"#;

#[tokio::test]
async fn an_inline_2xx_403_on_a_mantle_seat_is_scrubbed() {
    let err = complete_on(Lane::OpenAiCompat, 200, INLINE_MARKER_FREE_403, true)
        .await
        .expect_err("an inline error envelope is an error");

    let carried = err
        .parsed_body()
        .and_then(routectl_core::ParsedErrorBody::error_message);
    assert_eq!(carried, Some("bedrock access denied"));
    let Error::Upstream { body, .. } = &err else {
        panic!("expected Error::Upstream, got {err:?}");
    };
    for fragment in MARKER_FREE_FRAGMENTS {
        assert!(!body.contains(fragment), "leaked {fragment}: {body}");
    }
}

#[tokio::test]
async fn an_inline_2xx_403_on_a_first_party_seat_keeps_its_envelope() {
    let err = complete_on(Lane::OpenAiCompat, 200, INLINE_MARKER_FREE_403, false)
        .await
        .expect_err("an inline error envelope is an error");

    let Error::Upstream { body, .. } = &err else {
        panic!("expected Error::Upstream, got {err:?}");
    };
    assert_eq!(body, INLINE_MARKER_FREE_403);
}
