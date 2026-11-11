//! The scope lane reaches the recognizer only from the provider's own OAuth
//! bearer credential: the recorded bare 429 is recognized on `complete`,
//! `stream` and `count_tokens` there, and every other lane or near-miss
//! response stays unrecognized (target scope).
//!
//! The base URL is the exact Anthropic host resolved to the mock, so the
//! forwarded client-bearer leg is reachable; every provider is built the
//! same way and differs only in the field a row names.

use super::*;
use crate::upstream_scope::tests::bare_429;
use routectl_core::{ChatRequest, Message, MessageContent, Role, ScopeRow, StaticToken};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

#[derive(Clone, Copy, Debug)]
enum Lane {
    OAuth,
    ApiKey,
    Forwarded,
    #[cfg(feature = "bedrock")]
    Mantle,
}

#[derive(Clone, Copy, Debug)]
enum Call {
    Complete,
    Stream,
    CountTokens,
}

struct Row {
    name: &'static str,
    lane: Lane,
    call: Call,
    status: u16,
    headers: Vec<(&'static str, &'static str)>,
    body: String,
}

const BARE: &str = r#"{"type":"error","error":{"type":"rate_limit_error","message":"Error"},"request_id":"req_x"}"#;

/// The documented spend-cap 429 (rate-limits page, "Reaching your spend
/// cap"), request id replaced.
const SPEND_CAP: &str = r#"{"type":"error","error":{"type":"rate_limit_error","message":"You have reached your API usage limits: your organization has crossed its monthly API usage threshold, set based on your organization's API tier. You will regain access on 2026-09-01 at 00:00 UTC.","details":{"error_code":"enforced_spend_limit_reached"}},"request_id":"req_x"}"#;

/// A genuine account rate limit: the sentence and a reset hint.
const ACCOUNT_LIMIT: &str = r#"{"type":"error","error":{"type":"rate_limit_error","message":"This request would exceed your account's rate limit. Please try again later."},"request_id":"req_x"}"#;

fn bare_row(name: &'static str, lane: Lane, call: Call) -> Row {
    let recorded = bare_429();
    Row {
        name,
        lane,
        call,
        status: recorded.status,
        headers: Vec::new(),
        body: recorded.body,
    }
}

fn oauth_row(name: &'static str, headers: Vec<(&'static str, &'static str)>, body: &str) -> Row {
    Row {
        name,
        lane: Lane::OAuth,
        call: Call::Complete,
        status: 429,
        headers,
        body: body.to_string(),
    }
}

#[cfg_attr(not(feature = "bedrock"), allow(clippy::unused_async))]
async fn provider_on(server: &MockServer, lane: Lane) -> AnthropicApiProvider {
    #[cfg(feature = "bedrock")]
    let mantle = match lane {
        Lane::Mantle => Some(mantle_auth().await),
        _ => None,
    };
    let addr = *server.address();
    let base_url = format!("http://api.anthropic.com:{}", addr.port());
    let cfg = AnthropicApiConfig {
        id: "scope-lane-test".into(),
        auth: std::sync::Arc::new(StaticToken::new("oat-token")),
        base_url,
        anthropic_version: "2023-06-01".into(),
        auth_kind: match lane {
            Lane::ApiKey => AuthKind::ApiKey,
            _ => AuthKind::OauthBearer,
        },
        header_extras: Vec::new(),
        user_agent: None,
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: matches!(lane, Lane::Forwarded),
        #[cfg(feature = "bedrock")]
        mantle,
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

#[cfg(feature = "bedrock")]
async fn mantle_auth() -> crate::mantle::MantleAuth {
    let creds = crate::bedrock::auth::resolve(
        &crate::bedrock::BedrockCreds::BearerKey {
            key: "mantle-bearer-key".into(),
        },
        "us-west-2",
    )
    .await
    .expect("bearer creds resolve");
    crate::mantle::MantleAuth {
        region: "us-west-2".into(),
        creds,
    }
}

fn request(lane: Lane) -> ChatRequest {
    let mut req = ChatRequest {
        model: "claude-sonnet-4-6".into(),
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
        max_tokens: Some(3072),
        ..Default::default()
    };
    if matches!(lane, Lane::Forwarded) {
        req.routectl_internal.forwarded_bearer = Some(routectl_core::ForwardedBearer::new(
            "sk-ant-oat01-forwarded".into(),
        ));
    }
    req
}

/// The scope row the provider's error carries for `row`'s response.
async fn scope_row_for(row: &Row) -> Option<ScopeRow> {
    let server = MockServer::start().await;
    let mut template = ResponseTemplate::new(row.status)
        .insert_header("content-type", "application/json")
        .set_body_string(row.body.clone());
    for (name, value) in &row.headers {
        template = template.insert_header(*name, *value);
    }
    Mock::given(method("POST"))
        .respond_with(template)
        .mount(&server)
        .await;
    let provider = provider_on(&server, row.lane).await;
    let req = request(row.lane);
    let err = match row.call {
        Call::Complete => provider.complete(req).await.err(),
        Call::Stream => provider.stream(req).await.err(),
        Call::CountTokens => provider.count_tokens(req).await.err(),
    }
    .unwrap_or_else(|| panic!("{}: the upstream error must surface", row.name));
    match &err {
        Error::Upstream { status, .. } => assert_eq!(
            *status, row.status,
            "{}: the response reached the error reader",
            row.name
        ),
        other => panic!("{}: expected an upstream error, got {other:?}", row.name),
    }
    err.scope_row()
}

#[tokio::test]
async fn recorded_bare_429_on_the_oauth_lane_is_recognized_on_every_call() {
    let rows = [
        bare_row("complete", Lane::OAuth, Call::Complete),
        bare_row("stream", Lane::OAuth, Call::Stream),
        bare_row("count_tokens", Lane::OAuth, Call::CountTokens),
    ];
    for row in rows {
        assert_eq!(
            scope_row_for(&row).await,
            Some(ScopeRow::AnthropicBareRateLimit),
            "{}",
            row.name
        );
    }
}

fn reject_rows() -> Vec<Row> {
    let duplicate_message = r#"{"type":"error","error":{"type":"rate_limit_error","message":"Error","message":"Error"}}"#;
    let rows = vec![
        oauth_row(
            "account rate limit with retry-after",
            vec![("retry-after", "30")],
            ACCOUNT_LIMIT,
        ),
        oauth_row("documented spend cap", Vec::new(), SPEND_CAP),
        bare_row("api key auth", Lane::ApiKey, Call::Complete),
        bare_row("forwarded leg", Lane::Forwarded, Call::Complete),
        bare_row(
            "count_tokens forwarded leg",
            Lane::Forwarded,
            Call::CountTokens,
        ),
        oauth_row(
            "capped body",
            Vec::new(),
            &BARE.replace(
                "req_x",
                &"x".repeat(crate::http_client::MAX_RESPONSE_BODY_BYTES),
            ),
        ),
        oauth_row(
            "error.details present",
            Vec::new(),
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"Error","details":{}}}"#,
        ),
        oauth_row(
            "error.code present",
            Vec::new(),
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"Error","code":"x"}}"#,
        ),
        oauth_row(
            "extra top-level key",
            Vec::new(),
            r#"{"type":"error","error":{"type":"rate_limit_error","message":"Error"},"extra":1}"#,
        ),
        oauth_row("duplicate message key", Vec::new(), duplicate_message),
        oauth_row(
            "unparseable retry-after header",
            vec![("retry-after", "soon")],
            BARE,
        ),
        Row {
            status: 529,
            ..bare_row("bare body on a 529", Lane::OAuth, Call::Complete)
        },
        oauth_row(
            "retry-after-ms header",
            vec![("retry-after-ms", "500")],
            BARE,
        ),
        oauth_row(
            "one anthropic-ratelimit header",
            vec![("anthropic-ratelimit-requests-remaining", "0")],
            BARE,
        ),
    ];
    #[cfg(feature = "bedrock")]
    let rows: Vec<Row> = rows
        .into_iter()
        .chain([bare_row("mantle", Lane::Mantle, Call::Complete)])
        .collect();
    rows
}

#[tokio::test]
async fn non_matching_lanes_and_responses_are_not_recognized() {
    for row in reject_rows() {
        assert_eq!(scope_row_for(&row).await, None, "{}", row.name);
    }
}
