//! A BACKGROUND PROBE must not mutate client-facing shared state on this lane.
//!
//! Two pieces of shared state are at stake, and both are read by CLIENT traffic
//! rather than by the probe that would write them:
//!
//! - the unified-quota `representative-claim` record plus its once-per-transition
//!   overage-flip log, which describe what client requests last observed about
//!   billing attribution;
//! - the context-management thinking cache, keyed by `(provider_id, tool_use_id)`
//!   and re-injected on the NEXT client turn, out of a capacity-bounded LRU.
//!
//! Every absence assertion here is paired with a POSITIVE CONTROL: the same
//! fixture, the same headers or body, sent as ordinary client traffic, which DOES
//! move the state. A "nothing happened" assertion on a fixture that could never
//! have moved anything is free, and the mutation checks recorded on this module's
//! tests depend on both directions being live.
//!
//! Declared on `mod.rs` via `#[cfg(test)] #[path = ...]`.

use super::*;
use routectl_core::{ChatRequest, Message, MessageContent, Role};
use routectl_testkit::with_capture;
use serde_json::json;
use wiremock::matchers::{method, path as wm_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The provider id every fixture here installs, so a log assertion can key on it.
const PROVIDER_ID: &str = "background-isolation-test";

/// The `tool_use` id the cache-seeding body carries. A cache entry is keyed by
/// `(provider_id, tool_use_id)`, so this is what a read looks for.
const TOOL_USE_ID: &str = "toolu_isolation_01";

/// The representative-claim the fixture headers report. `overage` is the value
/// that makes the FIRST observation a transition (the instance starts at `None`),
/// which is what produces a flip log at all.
const OVERAGE_CLAIM: &str = "overage";

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

/// An ordinary CLIENT request. `background_probe` defaults to false, which is
/// exactly what makes this the positive control.
fn client_req() -> ChatRequest {
    ChatRequest {
        model: "claude-3-opus".into(),
        messages: vec![user_msg("hi")].into(),
        max_tokens: Some(2048),
        ..Default::default()
    }
}

/// The same request marked as routectl's own background traffic -- the ONE field
/// that differs from the control, so any behavioral difference is attributable to
/// it alone.
fn background_req() -> ChatRequest {
    let mut req = client_req();
    req.routectl_internal.background_probe = true;
    req
}

/// A provider with `context_management` set as given, so the cache cases can run
/// the emulation path and the quota cases can skip it.
fn provider_with(base_url: &str, context_management: bool) -> AnthropicApiProvider {
    let cfg = AnthropicApiConfig {
        id: PROVIDER_ID.into(),
        auth: std::sync::Arc::new(routectl_core::StaticToken::new("test-key")),
        base_url: base_url.to_string(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas: Vec::new(),
        forward_client_headers: Vec::new(),
        context_management,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
        #[cfg(feature = "bedrock")]
        mantle: None,
    };
    AnthropicApiProvider::new(cfg)
}

/// A plain 200 body with no thinking or tool blocks.
fn plain_body() -> serde_json::Value {
    json!({
        "id": "msg_isolation",
        "type": "message",
        "role": "assistant",
        "model": "claude-3-opus",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 5, "output_tokens": 3},
        "content": [{"type": "text", "text": "ok"}]
    })
}

/// A 200 body shaped to SEED THE CACHE: a thinking block followed by a `tool_use`
/// block, which is the exact pair `extract_tool_thinking` emits an entry for.
///
/// The positive control proves this fixture really does seed, so the background
/// case's empty-cache assertion is about the guard rather than about a body that
/// could never have produced an entry.
fn cache_seeding_body() -> serde_json::Value {
    json!({
        "id": "msg_isolation_cache",
        "type": "message",
        "role": "assistant",
        "model": "claude-3-opus",
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 5, "output_tokens": 3},
        "content": [
            {"type": "thinking", "thinking": "deliberating", "signature": "sig-abc"},
            {
                "type": "tool_use",
                "id": TOOL_USE_ID,
                "name": "search",
                "input": {"q": "x"}
            }
        ]
    })
}

/// Attach the unified-quota family reporting `claim`.
fn with_quota_headers(tmpl: ResponseTemplate, claim: &str) -> ResponseTemplate {
    tmpl.append_header("anthropic-ratelimit-unified-status", "allowed")
        .append_header("anthropic-ratelimit-unified-overage-status", "allowed")
        .append_header("anthropic-ratelimit-unified-5h-utilization", "0.91")
        .append_header("anthropic-ratelimit-unified-overage-utilization", "0.05")
        .append_header("anthropic-ratelimit-unified-representative-claim", claim)
        .append_header("anthropic-ratelimit-unified-reset", "2026-06-09T12:00:00Z")
}

/// Mount one 200 carrying `body` plus the quota family, and build the provider.
async fn mount(
    body: serde_json::Value,
    context_management: bool,
) -> (MockServer, AnthropicApiProvider) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wm_path("/v1/messages"))
        .respond_with(with_quota_headers(
            ResponseTemplate::new(200).set_body_json(body),
            OVERAGE_CLAIM,
        ))
        .mount(&server)
        .await;
    let uri = server.uri();
    (server, provider_with(&uri, context_management))
}

/// How many overage-flip lines the captured events carry.
fn flip_count(events: &[routectl_testkit::CapturedEvent]) -> usize {
    events
        .iter()
        .filter(|e| {
            e.message
                .contains("anthropic subscription billing flipped to overage")
                || e.message
                    .contains("anthropic subscription billing recovered from overage")
        })
        .count()
}

// ---------------------------------------------------------------------------
// The unified-quota claim record and its flip log
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn a_background_response_leaves_the_quota_claim_state_and_flip_log_untouched() {
    // The claim record is a per-instance account of what CLIENT traffic last saw,
    // and the flip log fires once per TRANSITION. A probe landing between two
    // client requests would either forge a transition the client stream never
    // underwent or swallow the real one -- and an operator reading the
    // billing-attribution line would see a flip caused by routectl's own
    // scheduling rather than by anything a client did.
    let (_server, provider) = mount(plain_body(), false).await;

    let (resp, events) = with_capture(async { provider.complete(background_req()).await }).await;

    let resp = resp.expect("the background call itself must still succeed");
    assert_eq!(
        flip_count(&events),
        0,
        "a background response must emit no overage-flip line: {events:#?}",
    );
    // WIRE BEHAVIOR UNCHANGED: the per-response carrier still rides upward. It is
    // per-response data rather than shared state, so withholding it would change
    // what the caller sees -- the guard is scoped to the MUTATION alone.
    assert!(
        resp.upstream_meta
            .as_ref()
            .and_then(|meta| meta.anthropic_unified.as_ref())
            .is_some(),
        "the background response must still carry its own quota meta",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_client_response_does_flip_the_quota_claim_state() {
    // POSITIVE CONTROL for the case above: the SAME headers on the SAME fixture,
    // differing only in `background_probe`, DO produce exactly one flip line.
    // Without this, the zero above would be satisfied by a fixture whose headers
    // could never have flipped anything.
    let (_server, provider) = mount(plain_body(), false).await;

    let (resp, events) = with_capture(async { provider.complete(client_req()).await }).await;

    resp.expect("the client call must succeed");
    assert_eq!(
        flip_count(&events),
        1,
        "client traffic must still flip the claim state exactly once: {events:#?}",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_background_response_cannot_swallow_a_later_clients_flip() {
    // The CONSEQUENCE of the guard, stated as behavior rather than as an absence:
    // with the probe first, the client request that follows still sees the
    // transition. Were the probe to write the claim record, the client's own
    // request would read a steady state and its flip would go unreported -- the
    // failure mode an operator would never see, because the missing line looks
    // exactly like a lane that did not flip.
    //
    // The two calls are captured SEPARATELY, and that is load-bearing. A single
    // capture counting one flip across both is satisfied by the mutation it exists
    // to catch: with the guard removed the probe forges the flip and the client
    // then reads steady state, so the total is still one. Measured -- the combined
    // form passed against the ungated code. Attribution needs the flip to be
    // observed inside the CLIENT call's own capture window.
    let (_server, provider) = mount(plain_body(), false).await;

    let ((), probe_events) = with_capture(async {
        provider
            .complete(background_req())
            .await
            .expect("probe call");
    })
    .await;
    let ((), client_events) = with_capture(async {
        provider.complete(client_req()).await.expect("client call");
    })
    .await;

    assert_eq!(
        flip_count(&probe_events),
        0,
        "premise: the probe itself must have reported no flip: {probe_events:#?}",
    );
    assert_eq!(
        flip_count(&client_events),
        1,
        "the CLIENT request must report its own flip -- a probe that had written \
         the claim record would leave this at zero: {client_events:#?}",
    );
}

// ---------------------------------------------------------------------------
// The context-management thinking cache
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn a_background_response_cannot_seed_the_thinking_cache() {
    // The cache is keyed by `(provider_id, tool_use_id)` and re-injected on the
    // next CLIENT turn, out of a capacity-bounded LRU. A probe-seeded entry would
    // inject thinking no client conversation produced, and would evict an entry a
    // real conversation is still going to need.
    let (_server, provider) = mount(cache_seeding_body(), true).await;
    assert_eq!(
        provider.thinking_cache.read().expect("lock").len(),
        0,
        "premise: the cache starts empty",
    );

    let resp = provider.complete(background_req()).await;

    resp.expect("the background call itself must still succeed");
    assert_eq!(
        provider.thinking_cache.read().expect("lock").len(),
        0,
        "a background response must not populate the shared thinking cache",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_client_response_does_seed_the_thinking_cache() {
    // POSITIVE CONTROL for the case above, on the SAME thinking+tool_use body:
    // client traffic still seeds exactly one entry. This is what makes the empty
    // cache above evidence about the guard rather than about a fixture that
    // produces no extractable pair.
    let (_server, provider) = mount(cache_seeding_body(), true).await;

    let resp = provider.complete(client_req()).await;

    resp.expect("the client call must succeed");
    assert_eq!(
        provider.thinking_cache.read().expect("lock").len(),
        1,
        "client traffic must still seed the cache, or the background case proves \
         nothing about the guard",
    );
}

/// An SSE body whose thinking block is followed by a `tool_use` block -- the
/// shape whose post-stream drain writes one cache entry.
///
/// Spelled out here rather than reused from the state-machine sidecar because
/// this case's subject is the PROVIDER path (the drain guarded by the
/// context-management flag), not the parser that fills `pending_cache_writes`.
fn cache_seeding_sse_body() -> String {
    concat!(
        "event: message_start\n",
        "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_iso_sse\",\"model\":\"claude-3-opus\",\"usage\":{\"input_tokens\":5,\"output_tokens\":0}}}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"deliberating\"}}\n\n",
        "event: content_block_delta\n",
        "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-abc\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
        "event: content_block_start\n",
        "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_isolation_sse\",\"name\":\"search\"}}\n\n",
        "event: content_block_stop\n",
        "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
        "event: message_stop\n",
        "data: {\"type\":\"message_stop\"}\n\n",
    )
    .to_string()
}

/// Mount the cache-seeding SSE body and build a context-management provider.
async fn mount_stream() -> (MockServer, AnthropicApiProvider) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wm_path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(cache_seeding_sse_body())
                .append_header("content-type", "text/event-stream"),
        )
        .mount(&server)
        .await;
    let uri = server.uri();
    (server, provider_with(&uri, true))
}

/// Drain `req`'s stream to completion so the post-stream cache tail runs.
async fn drain_stream(provider: &AnthropicApiProvider, mut req: ChatRequest) {
    use futures::StreamExt;
    req.stream = Some(true);
    let mut stream = provider.stream(req).await.expect("the stream must open");
    while let Some(chunk) = stream.next().await {
        chunk.expect("every chunk must parse");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_background_stream_cannot_seed_the_thinking_cache() {
    // The STREAM path's own guard. Its cache write happens in the post-stream
    // drain rather than inline, so it is a separate site from the complete() one
    // and needs its own behavioral pin -- the source guard alone would catch a
    // removal here, but only as a count, not as an effect.
    let (_server, provider) = mount_stream().await;

    drain_stream(&provider, background_req()).await;

    assert_eq!(
        provider.thinking_cache.read().expect("lock").len(),
        0,
        "a background stream must not populate the shared thinking cache",
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_client_stream_does_seed_the_thinking_cache() {
    // POSITIVE CONTROL for the stream case: the SAME SSE body as client traffic
    // seeds one entry, so the empty cache above is about the guard rather than
    // about an SSE fixture that accumulates no pending write.
    let (_server, provider) = mount_stream().await;

    drain_stream(&provider, client_req()).await;

    assert_eq!(
        provider.thinking_cache.read().expect("lock").len(),
        1,
        "client streaming traffic must still seed the cache",
    );
}

#[test]
fn both_client_facing_state_writes_are_gated_on_the_same_predicate() {
    // A SOURCE guard over coverage, for the same reason the recorder-call guard
    // exists: a write added later without the gate is invisible to a behavioral
    // test that happens not to drive its shape. Both the complete() and stream()
    // cache paths must consult the predicate, and the quota mutation must consult
    // it in the client boundary.
    //
    // Counted rather than merely `contains`-checked: this file's two cache paths
    // need TWO gates, and a single one would satisfy a containment check while
    // leaving the other path writing on probe traffic.
    const MOD_SRC: &str = include_str!("mod.rs");
    const CLIENT_SRC: &str = include_str!("client.rs");

    let cache_gates = MOD_SRC.matches("is_client_traffic(&req)").count();
    assert_eq!(
        cache_gates, 2,
        "the complete() and stream() cache paths each need their own gate; found \
         {cache_gates}",
    );
    assert!(
        CLIENT_SRC.contains(
            "if super::is_client_traffic(req) {\n            self.log_overage_flip(&quota);"
        ),
        "the quota claim-state mutation must sit behind the eligibility gate",
    );
}
