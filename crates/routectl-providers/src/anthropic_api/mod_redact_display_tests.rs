//! `thinking.display` and the redact-thinking beta never ship together on
//! the own-credential legs, whichever source contributed the flag; the
//! forwarded leg keeps the client's beta set verbatim.
//!
//! Each case assembles the body through `normalize_request` and composes
//! the header through `build_headers`, the same pair the dispatch methods
//! call, so the assertion covers the real body predicate and union order.

use super::*;
use routectl_core::identity::anthropic::{
    REDACT_THINKING_BETA, default_claude_code_anthropic_betas,
};
use routectl_core::{ChatRequest, Message, MessageContent, ReasoningConfig, Role, StaticToken};
use routectl_testkit::CapturedEvent;

const OAUTH_HOST: &str = "https://api.anthropic.com";
const API_KEY_HOST: &str = "http://127.0.0.1:18080";
const DROP_LOG_FIELD: &str = "dropped_beta";
const CONTROL_PROVIDER_ID: &str = "redact-display-control";

fn cfg(base_url: &str, auth_kind: AuthKind, use_forwarded_bearer: bool) -> AnthropicApiConfig {
    AnthropicApiConfig {
        id: "redact-display-test".into(),
        auth: Arc::new(StaticToken::new("test-token")),
        base_url: base_url.into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind,
        header_extras: Vec::new(),
        user_agent: None,
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer,
        #[cfg(feature = "bedrock")]
        mantle: None,
    }
}

fn thinking_req(display: Option<&str>, client_betas: &[&str]) -> ChatRequest {
    let mut req = ChatRequest {
        model: "claude-sonnet-4-5".into(),
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
        max_tokens: Some(16_000),
        reasoning: Some(ReasoningConfig {
            max_tokens: Some(4_096),
            ..Default::default()
        }),
        ..Default::default()
    };
    req.routectl_internal.anthropic_thinking_display = display.map(str::to_string);
    req.anthropic_beta = client_betas.iter().map(|s| (*s).to_string()).collect();
    req
}

/// Assemble the wire body, compose the headers from it, and return the
/// body plus the outbound `anthropic-beta` flags.
fn compose(provider: &AnthropicApiProvider, req: &ChatRequest) -> (Value, Vec<String>) {
    let body = provider.normalize_request(req).expect("normalize");
    let client = reqwest::Client::new();
    let (rb, _decision) = provider.build_headers(
        client.post("http://127.0.0.1/test"),
        req,
        "tok",
        Some(&body),
    );
    let request = rb.build().expect("build outbound request");
    let betas = request
        .headers()
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(',').map(|b| b.trim().to_string()).collect())
        .unwrap_or_default();
    (body, betas)
}

fn has(betas: &[String], flag: &str) -> bool {
    betas.iter().any(|b| b == flag)
}

/// Captured events that report dropping the redact beta for `provider_id`.
fn redact_drops<'a>(events: &'a [CapturedEvent], provider_id: &str) -> Vec<&'a CapturedEvent> {
    events
        .iter()
        .filter(|e| e.field(DROP_LOG_FIELD) == Some(REDACT_THINKING_BETA))
        .filter(|e| e.field("provider") == Some(provider_id))
        .collect()
}

#[test]
fn display_drops_the_floor_redact_beta_on_the_oauth_lane() {
    let provider = AnthropicApiProvider::new(cfg(OAUTH_HOST, AuthKind::OauthBearer, false));
    let req = thinking_req(Some("summarized"), &[]);

    let (body, betas) = compose(&provider, &req);

    assert_eq!(body["thinking"]["display"], "summarized");
    assert!(
        !has(&betas, REDACT_THINKING_BETA),
        "display must drop the floor's redact beta; got {betas:?}"
    );
    for flag in default_claude_code_anthropic_betas()
        .iter()
        .filter(|f| **f != REDACT_THINKING_BETA)
    {
        assert!(
            has(&betas, flag),
            "floor flag {flag} must survive; got {betas:?}"
        );
    }
}

#[test]
fn no_display_keeps_the_floor_redact_beta_on_the_oauth_lane() {
    let provider = AnthropicApiProvider::new(cfg(OAUTH_HOST, AuthKind::OauthBearer, false));
    let req = thinking_req(None, &[]);

    let (body, betas) = compose(&provider, &req);

    assert!(body["thinking"].get("display").is_none());
    assert!(
        has(&betas, REDACT_THINKING_BETA),
        "without display the floor's redact beta must ship; got {betas:?}"
    );
}

#[test]
fn display_drops_a_client_sent_redact_beta_and_logs_once() {
    let provider = AnthropicApiProvider::new(cfg(API_KEY_HOST, AuthKind::ApiKey, false));
    let req = thinking_req(Some("omitted"), &[REDACT_THINKING_BETA, "client-only-beta"]);

    let mut composed = None;
    let events = routectl_testkit::capture_events(|| composed = Some(compose(&provider, &req)));
    let (body, betas) = composed.expect("compose ran inside the capture");

    assert_eq!(body["thinking"]["display"], "omitted");
    assert_eq!(betas, vec!["client-only-beta".to_string()]);
    let drops = redact_drops(&events, &provider.cfg.id);
    assert_eq!(
        drops.len(),
        1,
        "expected exactly one redact-beta drop event; captured {events:?}"
    );
}

#[test]
fn no_display_keeps_a_client_sent_redact_beta_and_logs_nothing() {
    let provider = AnthropicApiProvider::new(cfg(API_KEY_HOST, AuthKind::ApiKey, false));
    let req = thinking_req(None, &[REDACT_THINKING_BETA]);
    let control = AnthropicApiProvider::new(AnthropicApiConfig {
        id: CONTROL_PROVIDER_ID.into(),
        ..cfg(API_KEY_HOST, AuthKind::ApiKey, false)
    });
    let control_req = thinking_req(Some("omitted"), &[REDACT_THINKING_BETA]);

    let mut composed = None;
    let events = routectl_testkit::capture_events(|| {
        composed = Some(compose(&provider, &req));
        compose(&control, &control_req);
    });
    let (_body, betas) = composed.expect("compose ran inside the capture");

    assert_eq!(betas, vec![REDACT_THINKING_BETA.to_string()]);
    // The display-carrying control compose proves the capture saw the drop
    // callsite, so the zero count below is not an artifact of a dead capture.
    assert_eq!(
        redact_drops(&events, CONTROL_PROVIDER_ID).len(),
        1,
        "control compose must log its drop; captured {events:?}"
    );
    assert!(
        redact_drops(&events, &provider.cfg.id).is_empty(),
        "no display must log no drop; captured {events:?}"
    );
}

#[test]
fn forwarded_leg_keeps_the_client_redact_beta_despite_display() {
    let provider = AnthropicApiProvider::new(cfg(OAUTH_HOST, AuthKind::OauthBearer, true));
    let mut req = thinking_req(
        Some("summarized"),
        &[REDACT_THINKING_BETA, "client-only-beta"],
    );
    req.routectl_internal.forwarded_bearer = Some(routectl_core::ForwardedBearer::new(
        "sk-ant-oat01-forwarded".into(),
    ));
    assert!(
        provider.forwarded_leg(&req),
        "fixture must take the forwarded leg"
    );

    let (body, betas) = compose(&provider, &req);

    assert_eq!(body["thinking"]["display"], "summarized");
    assert_eq!(
        betas,
        vec![
            REDACT_THINKING_BETA.to_string(),
            "client-only-beta".to_string()
        ],
        "the forwarded leg must carry the client's betas verbatim"
    );
}
