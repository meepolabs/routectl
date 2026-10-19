//! Pre-change rejection-contract + forward-compat extras pins for the
//! `/v1/messages/count_tokens` handler. Loaded via
//! `#[cfg(test)] #[path = "messages_count_tokens_tests.rs"] mod tests;`
//! from `messages_count_tokens.rs`.
//!
//! count_tokens is the one inference endpoint that does NOT funnel through
//! `ingress_handle`; it renders its own body-rejection / content-type /
//! parse-error responses via the shared `render_body_rejection` /
//! `render_unsupported_media_type` / `render_parse_failure` / `map_error`
//! helpers. The rejection test drives the REAL handler mounted behind the
//! same `DefaultBodyLimit` layer `server::serve::build_axum_router`
//! installs, so the `Bytes` extractor and the body-size layer are exercised
//! exactly as in production. The extras test pins that an unknown
//! top-level field round-trips into `provider_extras` (Anthropic dialect).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::extract::DefaultBodyLimit;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::Response;
use axum::routing::post;
use futures::stream::BoxStream;
use routectl_core::wire_check::{IngressDialect, IngressWire};
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result, TokenCount};
use routectl_router::{AliasValue, ProviderEntry, ResolvedModel};
use serde_json::{Value, json};
use tower::ServiceExt;

use crate::ingress::IngressAdapter;
use crate::ingress::anthropic::AnthropicIngress;
use crate::server::AppState;

const REJECT_BODY_LIMIT: usize = 1024;

fn test_state() -> Arc<AppState> {
    test_state_with_router().0
}

type RouterSwap = Arc<arc_swap::ArcSwap<routectl_router::Router>>;

fn test_state_with_router() -> (Arc<AppState>, RouterSwap) {
    let router = Arc::new(routectl_router::Router::new(Arc::new(
        routectl_router::Config::default(),
    )));
    let swap = Arc::new(arc_swap::ArcSwap::from(router));
    (AppState::for_test(Arc::clone(&swap)), swap)
}

/// `(lane, reason, count)` for every local-refusal key `swap`'s router holds.
fn local_refusals(swap: &RouterSwap) -> Vec<(&'static str, &'static str, u64)> {
    swap.load()
        .wire_check_snapshot()
        .local_refusals
        .iter()
        .map(|c| (c.lane.label(), c.reason.as_str(), c.reading.count))
        .collect()
}

fn app(state: Arc<AppState>) -> axum::Router {
    axum::Router::new()
        .route("/v1/messages/count_tokens", post(super::count_tokens))
        .layer(DefaultBodyLimit::max(REJECT_BODY_LIMIT))
        .with_state(state)
}

fn post_req(content_type: Option<&str>, body: impl Into<Body>) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens");
    if let Some(ct) = content_type {
        builder = builder.header("content-type", ct);
    }
    builder.body(body.into()).expect("request builds")
}

fn oversized_body() -> String {
    format!(
        "{{\"model\":\"m\",\"pad\":\"{}\"}}",
        "a".repeat(REJECT_BODY_LIMIT * 2)
    )
}

async fn body_to_value(resp: Response) -> Value {
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

async fn drive(state: Arc<AppState>, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app(state).oneshot(req).await.expect("router is infallible");
    let status = resp.status();
    (status, body_to_value(resp).await)
}

/// count_tokens is Anthropic-dialect: the routectl-owned envelope shape and
/// classifier are pinned byte-for-byte; the human message string is asserted
/// only non-empty (the hand-rolled renderer owns its wording), so the pin
/// never couples to a specific message string.
fn assert_anthropic_reject(status: StatusCode, body: &Value, expected: StatusCode) {
    assert_eq!(status, expected, "count_tokens rejection status");
    let msg = body["error"]["message"]
        .as_str()
        .expect("anthropic envelope carries error.message string");
    assert!(!msg.is_empty(), "rejection message must be non-empty");
    assert_eq!(
        *body,
        json!({
            "type": "error",
            "error": { "type": "invalid_request_error", "message": msg }
        }),
        "exact Anthropic rejection envelope"
    );
}

#[tokio::test]
async fn count_tokens_rejection_contract_pins_status_and_envelope() {
    // JSON syntax error -> 400 + Anthropic envelope.
    let state = test_state();
    let (status, body) = drive(
        state,
        post_req(Some("application/json"), "{ not valid json"),
    )
    .await;
    assert_anthropic_reject(status, &body, StatusCode::BAD_REQUEST);

    // Wrong content-type -> 415.
    let state = test_state();
    let (status, body) = drive(state, post_req(Some("text/plain"), "{}")).await;
    assert_anthropic_reject(status, &body, StatusCode::UNSUPPORTED_MEDIA_TYPE);

    // Oversized body -> 413 (DefaultBodyLimit layer).
    let state = test_state();
    let (status, body) = drive(state, post_req(Some("application/json"), oversized_body())).await;
    assert_anthropic_reject(status, &body, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn a_count_tokens_body_the_ingress_refuses_counts_under_the_anthropic_local_lane() {
    let cases: [(&str, &str, &str); 2] = [
        ("malformed_json", "{ not valid json", "unparseable"),
        (
            "non_string_thinking_display",
            r#"{"model":"m","messages":[],"thinking":{"type":"enabled","budget_tokens":2048,"display":1}}"#,
            "field-type",
        ),
    ];

    for (name, body, reason) in cases {
        let (state, swap) = test_state_with_router();

        let (status, _) = drive(state, post_req(Some("application/json"), body.to_owned())).await;

        assert_eq!(status, StatusCode::BAD_REQUEST, "{name}");
        assert_eq!(
            local_refusals(&swap),
            [("anthropic->local", reason, 1)],
            "{name}"
        );
    }
}

#[test]
fn count_tokens_preserves_unknown_top_level_field_into_provider_extras() {
    let body = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "max_tokens": 1024,
        "future_unknown_knob": {"nested": [1, 2, 3]}
    });
    let req = AnthropicIngress
        .parse_request_value(&HeaderMap::new(), body)
        .expect("valid Anthropic body parses");
    let extras = req
        .provider_extras
        .expect("unknown top-level field must round-trip into provider_extras");
    assert_eq!(extras["future_unknown_knob"], json!({"nested": [1, 2, 3]}));
}

/// A drifted client `User-Agent` on a count_tokens request is observed by
/// the state-owned drift guard. count_tokens does NOT funnel through
/// `ingress_handle`, so this path needs its own observation or base-url
/// context sizing traffic produces no signal at all.
#[tokio::test]
async fn count_tokens_observes_a_drifted_client_version_before_dispatch() {
    let state = test_state();
    let mut req = post_req(
        Some("application/json"),
        r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"max_tokens":16}"#,
    );
    req.headers_mut().insert(
        axum::http::header::USER_AGENT,
        DRIFTED_CLIENT_UA.parse().unwrap(),
    );

    let _resp = app(Arc::clone(&state))
        .oneshot(req)
        .await
        .expect("router is infallible");

    assert!(
        !state.cc_pin_drift.observe_version(Some(DRIFTED_VERSION)),
        "the handler must have already recorded this version, so a re-observation is a no-op"
    );
}

/// The observation happens even though this request cannot dispatch (no
/// count_tokens-capable provider is configured), which is what "after
/// parse, before dispatch" buys: the signal does not depend on the
/// upstream being reachable.
#[tokio::test]
async fn count_tokens_records_the_observation_independently_of_dispatch_outcome() {
    let state = test_state();
    let mut req = post_req(
        Some("application/json"),
        r#"{"model":"m","messages":[{"role":"user","content":"hi"}],"max_tokens":16}"#,
    );
    req.headers_mut().insert(
        axum::http::header::USER_AGENT,
        DRIFTED_CLIENT_UA.parse().unwrap(),
    );

    let resp = app(Arc::clone(&state))
        .oneshot(req)
        .await
        .expect("router is infallible");

    assert_ne!(
        resp.status(),
        StatusCode::OK,
        "no capable provider is configured, so this request does not succeed"
    );
    assert!(!state.cc_pin_drift.observe_version(Some(DRIFTED_VERSION)));
}

/// A malformed body is rejected BEFORE the observation, so a client that
/// cannot even be parsed contributes no version record.
#[tokio::test]
async fn count_tokens_does_not_observe_when_the_body_never_parsed() {
    let state = test_state();
    let mut req = post_req(Some("application/json"), "{ not valid json");
    req.headers_mut().insert(
        axum::http::header::USER_AGENT,
        DRIFTED_CLIENT_UA.parse().unwrap(),
    );

    let _resp = app(Arc::clone(&state))
        .oneshot(req)
        .await
        .expect("router is infallible");

    assert!(
        state.cc_pin_drift.observe_version(Some(DRIFTED_VERSION)),
        "nothing was observed on a rejected request, so this is the first sighting"
    );
}

/// Records the ingress wire each count call carries.
struct WireProbe {
    wires: std::sync::Mutex<Vec<Option<IngressWire>>>,
}

#[async_trait::async_trait]
impl Provider for WireProbe {
    fn id(&self) -> &'static str {
        "wire-probe"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("wire-probe", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(Error::normalize_response("wire-probe", "unused"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::normalize_response("wire-probe", "unused"))
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.wires
            .lock()
            .expect("wires lock")
            .push(req.routectl_internal.ingress_wire);
        Ok(TokenCount {
            input_tokens: 7,
            extras: Default::default(),
        })
    }
}

fn state_counting_with(probe: &Arc<WireProbe>) -> Arc<AppState> {
    let config = routectl_router::Config {
        providers: BTreeMap::from([("p".to_owned(), ProviderEntry::anthropic_api("literal:k"))]),
        aliases: BTreeMap::from([("a".to_owned(), AliasValue::Single("m".to_owned()))]),
        ..routectl_router::Config::default()
    };
    let mut router = routectl_router::Router::new(Arc::new(config));
    let provider = Arc::clone(probe) as Arc<dyn Provider>;
    router.install_resolved_models(BTreeMap::from([(
        "m".to_owned(),
        Arc::new(ResolvedModel::new("m", "p", provider, "upstream")),
    )]));
    AppState::for_test(Arc::new(arc_swap::ArcSwap::from_pointee(router)))
}

#[tokio::test]
async fn count_tokens_captures_the_ingress_wire_after_parse() {
    let probe = Arc::new(WireProbe {
        wires: std::sync::Mutex::new(Vec::new()),
    });
    let body = r#"{"model":"a","messages":[{"role":"user","content":"hi"}]}"#;
    let mut req = post_req(Some("application/json"), body);
    req.headers_mut().insert(
        axum::http::header::USER_AGENT,
        DRIFTED_CLIENT_UA.parse().unwrap(),
    );

    let (status, counted) = drive(state_counting_with(&probe), req).await;

    assert_eq!(status, StatusCode::OK, "{counted}");
    let wires = probe.wires.lock().expect("wires lock");
    let [Some(wire)] = wires.as_slice() else {
        panic!("one count call carrying the ingress wire, got {wires:?}");
    };
    assert_eq!(wire.dialect, IngressDialect::Anthropic);
    assert_eq!(wire.bytes.as_ref(), body.as_bytes(), "the posted bytes");
    assert_eq!(wire.client_version.as_deref(), Some(DRIFTED_VERSION));
}

/// A version well clear of anything routectl could mint, so the assertions
/// above test drift rather than coincidence with the compiled pin.
const DRIFTED_VERSION: &str = "99.9.9";
const DRIFTED_CLIENT_UA: &str = "claude-cli/99.9.9 (external, sdk-cli)";
