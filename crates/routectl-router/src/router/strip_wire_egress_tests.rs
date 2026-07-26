//! Proves the strip-and-proceed ACT side is real end to end: a seeded
//! learned negative drives a strip through the REAL anthropic-api egress
//! against a wiremock upstream, and the OUTBOUND wire body is asserted --
//! not an in-process request clone. The anthropic-api egress passes a
//! built-in tool through verbatim as `AnthropicTool::Builtin`, so the
//! advisor tool is wire-visible; a strip that removes it is observable on
//! the bytes the upstream actually received. Also pins the capture leg's
//! current dormancy: no droppable capability is learnable yet, because no
//! grounded rejection envelope exists for one.

use super::*;
use crate::config::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy};
use crate::factory::{BuildOptions, build_resolved_models};
use crate::learned_capability::ExportedEntry;
use crate::router::RouterOptions;
use routectl_auth::{MemoryStore, SecretStore};
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use routectl_core::{Message, MessageContent, Role, ToolDef};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The grounded `anthropic_beta` token that enables context management.
/// An operator floor pinning this token makes stripping the capability a
/// false success (the egress re-adds it), so a pinned capability must
/// route away instead of stripping.
const CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";

/// Single-attempt, fast-backoff retry so a chain walk falls back promptly
/// without wall-clock sleeps.
fn fast_retry() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 1,
        initial_backoff_ms: 1,
        backoff_multiplier: 1.0,
        ..RetryPolicy::default()
    }
}

/// One chain member: a model nickname pointed at an anthropic-api provider
/// whose `base_url` is a wiremock URL. `pinned_beta` seeds a provider
/// `anthropic-beta` header floor so a beta-flag strip on that target would
/// be re-added on the wire (the operator-pin path).
struct WireUpstream {
    nickname: &'static str,
    provider_name: &'static str,
    base_url: String,
    pinned_beta: Option<&'static str>,
}

impl WireUpstream {
    fn plain(nickname: &'static str, provider_name: &'static str, base_url: &str) -> Self {
        Self {
            nickname,
            provider_name,
            base_url: base_url.to_string(),
            pinned_beta: None,
        }
    }

    fn pinned(
        nickname: &'static str,
        provider_name: &'static str,
        base_url: &str,
        beta: &'static str,
    ) -> Self {
        Self {
            pinned_beta: Some(beta),
            ..Self::plain(nickname, provider_name, base_url)
        }
    }
}

/// Build a router whose `alias` resolves to `chain` (nicknames in order),
/// with `[capability]` enabled. Providers are real anthropic-api egresses
/// pointed at the wiremock URLs; a `state_key` equals its model nickname.
async fn build_wire_router(upstreams: &[WireUpstream], alias: &str, chain: &[&str]) -> Router {
    let mut providers = BTreeMap::new();
    let mut models = BTreeMap::new();
    for u in upstreams {
        let mut entry = ProviderEntry::anthropic_api(crate::test_secret::file_ref("test-key"))
            .with_base_url(&u.base_url);
        if let Some(beta) = u.pinned_beta {
            let mut headers = BTreeMap::new();
            headers.insert("anthropic-beta".to_string(), beta.to_string());
            entry = entry.with_header_extras(headers);
        }
        providers.insert(u.provider_name.to_string(), entry);
        models.insert(
            u.nickname.to_string(),
            ModelEntry::new(u.provider_name, "upstream-model"),
        );
    }

    let mut aliases = BTreeMap::new();
    let value = if chain.len() == 1 {
        AliasValue::Single(chain[0].to_string())
    } else {
        AliasValue::Chain(chain.iter().map(|s| (*s).to_string()).collect())
    };
    aliases.insert(alias.to_string(), value);

    let mut cfg = Config {
        providers,
        models,
        aliases,
        retry: fast_retry(),
        ..Config::default()
    };
    cfg.capability.enabled = true;
    cfg.capability.decay_hours = 48;

    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
    let (resolved, failed) = build_resolved_models(&cfg, store, BuildOptions::default())
        .await
        .expect("build_resolved_models");
    assert!(failed.is_empty(), "provider build failures: {failed:?}");

    let mut router = Router::new(Arc::new(cfg));
    router.install_resolved_models(resolved);
    router
}

/// A wiremock anthropic-api upstream answering `POST /v1/messages` with a
/// single `(status, body)` on every call.
async fn upstream(status: u16, body: Value) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(&server)
        .await;
    server
}

/// A minimal valid Anthropic Messages success body.
fn anthropic_ok() -> Value {
    json!({
        "id": "msg_ok",
        "type": "message",
        "role": "assistant",
        "model": "upstream-model",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1},
        "content": [{"type": "text", "text": "ok"}]
    })
}

/// A plausible advisor-tool rejection: a generic `invalid_request_error`
/// whose free-text message names the advisor tool. It classifies as
/// `BadRequest`, and the resolver has no grounded phrase to attribute it
/// to a capability -- so today it produces no learn (the dormancy this
/// suite pins).
fn advisor_rejection_400() -> Value {
    json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": "The advisor tool is not supported for this model."
        }
    })
}

fn user_msg(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// A request carrying an advisor built-in tool, so `derive_feature_keys`
/// yields `advisor` and the anthropic-api egress emits the tool verbatim.
fn advisor_req(alias: &str) -> ChatRequest {
    ChatRequest {
        model: alias.to_string(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(2048),
        tools: Some(vec![ToolDef::Other(
            json!({"type": "advisor", "name": "advisor"}),
        )]),
        ..Default::default()
    }
}

/// A request carrying a `context_management` built-in tool (so the key is
/// derived) plus the beta token an operator floor would re-add on the
/// wire.
fn context_management_req(alias: &str) -> ChatRequest {
    ChatRequest {
        model: alias.to_string(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(2048),
        tools: Some(vec![ToolDef::Other(
            json!({"type": "context_management", "name": "cm"}),
        )]),
        anthropic_beta: vec![CONTEXT_MANAGEMENT_BETA.to_string()],
        ..Default::default()
    }
}

/// An acting (non-expired) self-identifying learned negative for
/// `(state_key, feature_key)`. `feature_key` is stored verbatim -- the
/// caller chooses a canonical or non-canonical token.
fn acting_negative(state_key: &str, feature_key: &str) -> ExportedEntry {
    let base = Instant::now();
    ExportedEntry {
        state_key: state_key.into(),
        feature_key: feature_key.into(),
        signal: SignalTier::SelfIdentifying,
        observations: 1,
        first_seen: base,
        last_seen: base,
        expires_at: base + Duration::from_hours(48),
        phase: FailurePhase::F1,
        source: EvidenceSource::Live,
        in_flight: false,
        consecutive_failed_probes: 0,
    }
}

/// The `type` strings of the built-in tools on an outbound wire body.
fn wire_tool_types(body: &Value) -> Vec<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.get("type").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

async fn hits(server: &MockServer) -> usize {
    server.received_requests().await.map_or(0, |r| r.len())
}

async fn last_request_body(server: &MockServer) -> Value {
    let reqs = server.received_requests().await.expect("received requests");
    let last = reqs.last().expect("at least one request received");
    serde_json::from_slice(&last.body).expect("outbound request body is JSON")
}

#[tokio::test]
async fn advisor_strip_removes_tool_from_real_wire_body_and_succeeds() {
    // A seeded acting negative for the canonical `advisor` key drives a
    // strip through the REAL egress. The advisor tool the request carries
    // must be ABSENT from the bytes the upstream received, the request
    // must succeed, and the decision must surface `outcome = applied`.
    let a = upstream(200, anthropic_ok()).await;
    let router = build_wire_router(
        &[WireUpstream::plain("m_a", "prov_a", &a.uri())],
        "solo",
        &["m_a"],
    )
    .await;
    router
        .learned_capabilities
        .import_entries(vec![acting_negative("m_a", "advisor")]);

    let (d, events) = routectl_testkit::with_capture(
        router.complete_with_options(advisor_req("solo"), RouterOptions::default()),
    )
    .await;

    assert!(
        d.result.is_ok(),
        "the stripped request must succeed on the real egress: {:?}",
        d.result.err(),
    );
    assert_eq!(hits(&a).await, 1);

    // WIRE GUARD: the advisor tool did not cross the wire after the strip.
    let sent = last_request_body(&a).await;
    assert!(
        !wire_tool_types(&sent).iter().any(|t| t == "advisor"),
        "the advisor tool must be removed from the outbound wire body; body = {sent}",
    );

    // The strip decision fired with the applied outcome -- not a
    // probe-bypass or a no-op.
    let warn = events
        .iter()
        .find(|e| e.message == "capability_strip_decision")
        .expect("a real strip must emit a capability_strip_decision WARN");
    assert_eq!(warn.level, tracing::Level::WARN);
    assert_eq!(warn.field("event"), Some("strip"));
    assert_eq!(warn.field("state_key"), Some("m_a"));
    assert_eq!(warn.field("capability_key"), Some("advisor"));
    assert_eq!(warn.field("outcome"), Some("applied"));
    assert_eq!(router.metrics.strip_total(), 1);
}

#[tokio::test]
async fn non_canonical_registry_token_does_not_strip_advisor_tool() {
    // The strip is keyed on the canonical `advisor` capability. A negative
    // seeded under a different token never matches the request-derived
    // canonical key, so the advisor tool survives onto the wire and no
    // strip is applied -- the canonical-key guard.
    let a = upstream(200, anthropic_ok()).await;
    let router = build_wire_router(
        &[WireUpstream::plain("m_a", "prov_a", &a.uri())],
        "solo",
        &["m_a"],
    )
    .await;
    router
        .learned_capabilities
        .import_entries(vec![acting_negative("m_a", "advisor_helper")]);

    let (d, events) = routectl_testkit::with_capture(
        router.complete_with_options(advisor_req("solo"), RouterOptions::default()),
    )
    .await;

    assert!(d.result.is_ok());
    let sent = last_request_body(&a).await;
    assert!(
        wire_tool_types(&sent).iter().any(|t| t == "advisor"),
        "a non-canonical registry key must not strip the advisor tool; body = {sent}",
    );
    assert_eq!(router.metrics.strip_total(), 0);
    assert!(
        events.iter().all(|e| {
            e.message != "capability_strip_decision" || e.field("outcome") != Some("applied")
        }),
        "no strip must be applied for a non-canonical registry key",
    );
}

#[tokio::test]
async fn pinned_beta_capability_routes_away_instead_of_stripping() {
    // A's provider pins the context-management beta, so stripping the
    // capability would be silently re-added on the wire -- a false
    // success. The target must route away (tail-demoted) and B, which
    // carries no negative, serves first. A is never dialed, and no strip
    // is applied.
    let a = upstream(200, anthropic_ok()).await;
    let b = upstream(200, anthropic_ok()).await;
    let router = build_wire_router(
        &[
            WireUpstream::pinned("m_a", "prov_a", &a.uri(), CONTEXT_MANAGEMENT_BETA),
            WireUpstream::plain("m_b", "prov_b", &b.uri()),
        ],
        "chain",
        &["m_a", "m_b"],
    )
    .await;
    router
        .learned_capabilities
        .import_entries(vec![acting_negative("m_a", "context_management")]);

    let (d, events) = routectl_testkit::with_capture(
        router.complete_with_options(context_management_req("chain"), RouterOptions::default()),
    )
    .await;

    assert!(d.result.is_ok());
    assert_eq!(
        d.meta.served_provider.as_deref(),
        Some("prov_b"),
        "the pinned-beta target routes away; B serves",
    );
    assert_eq!(
        hits(&a).await,
        0,
        "a pinned-beta capability must route away, never be dialed and stripped",
    );
    assert_eq!(hits(&b).await, 1);
    assert_eq!(router.metrics.strip_total(), 0);
    assert!(
        events.iter().all(|e| {
            e.message != "capability_strip_decision" || e.field("outcome") != Some("applied")
        }),
        "a pinned capability must not be stripped",
    );
}

#[tokio::test]
async fn strip_capture_loop_is_dormant_no_droppable_is_learnable() {
    // DORMANCY PIN. The act side (strip) is grounded and proven above, but
    // the CAPTURE side of the loop for a droppable capability is not: no
    // real advisor-rejection envelope has been captured, so the resolver
    // cannot attribute an advisor 400 to the `advisor` capability, and the
    // request-membership gate never gets the chance to admit a learn.
    //
    // This test drives a genuine advisor-tool request that crosses the
    // wire and meets a plausible capability rejection, and asserts NO
    // learn event occurs. When a grounded advisor rejection envelope is
    // captured and the resolver learns to attribute it, the request-
    // membership gate (the request derives `advisor`) will admit the
    // learn, this `is_empty` assertion will FAIL, and whoever grounds the
    // envelope must convert this dormant guard into the real capture-leg
    // end-to-end test that asserts the learn + subsequent strip. The
    // failure is the signal; the dormancy is load-bearing, not silent.
    let a = upstream(400, advisor_rejection_400()).await;
    let router = build_wire_router(
        &[WireUpstream::plain("m_a", "prov_a", &a.uri())],
        "solo",
        &["m_a"],
    )
    .await;

    // No seed: this exercises the capture (learn) path, not the act path.
    let d = router
        .complete_with_options(advisor_req("solo"), RouterOptions::default())
        .await;

    assert!(
        matches!(d.result, Err(Error::Upstream { status: 400, .. })),
        "the upstream rejected the advisor request: {:?}",
        d.result,
    );
    // The advisor tool genuinely crossed the wire -- the rejection is for a
    // request that really carried the capability, not a stripped shape.
    let sent = last_request_body(&a).await;
    assert!(
        wire_tool_types(&sent).iter().any(|t| t == "advisor"),
        "the advisor tool must have crossed the wire; body = {sent}",
    );
    assert!(
        d.meta.learned_capabilities.is_empty(),
        "a droppable capability is not yet learnable: no grounded rejection envelope exists",
    );
    assert!(
        router.learned_capabilities.is_empty(),
        "the capture path must leave the registry untouched while the loop is dormant",
    );
}
