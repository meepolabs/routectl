//! Pin: `Router::count_tokens` walks PAST count_tokens-incapable
//! targets (provider_kind != "anthropic-api") to the first capable
//! one, returning 501 NotImplemented only when NO target in the
//! chain is capable. The capability skip is keyed statically on
//! provider kind BEFORE dispatch -- a kind-skip is operator-known,
//! not upstream health -- so it never touches the breaker. A
//! CAPABLE target that returns a real upstream error propagates as
//! today (no further walk). All anthropic-api targets share the
//! same Anthropic tokenizer family, so walking past incapable kinds
//! does NOT reintroduce the wrong-tokenizer hazard.
use super::*;
use crate::config::{ProviderEntry, ProviderRuntimePolicy};
use crate::resolved::ResolvedModel;
use async_trait::async_trait;
use futures::stream::BoxStream;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Provider, TokenCount};
use routectl_providers::anthropic_api::AuthKind;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// How a mock provider's `count_tokens` should respond once it is
/// actually selected and dispatched to.
#[derive(Clone, Copy)]
enum CountBehavior {
    /// Return `Ok(TokenCount { input_tokens })`.
    Ok(u32),
    /// Return `Error::NotImplemented` (the trait-default shape).
    NotImplemented,
    /// Return `Error::Upstream { status, .. }` (a real upstream
    /// error from a capable provider).
    UpstreamError(u16),
}

/// Mock provider that records every `count_tokens` call so a test
/// can prove a target was (or was NOT) dispatched to. Its kind in
/// the capability walk is decided by the matching `ProviderEntry`
/// in config, NOT by this impl -- so a Bedrock-kind entry skips the
/// walk regardless of what this returns.
struct CountingProvider {
    id: String,
    calls: Arc<AtomicUsize>,
    behavior: CountBehavior,
}

#[async_trait]
impl Provider for CountingProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        unreachable!()
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!()
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!()
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.behavior {
            CountBehavior::Ok(n) => Ok(TokenCount {
                input_tokens: n,
                extras: Default::default(),
            }),
            CountBehavior::NotImplemented => Err(Error::NotImplemented(
                self.id.clone(),
                "count_tokens".into(),
            )),
            CountBehavior::UpstreamError(status) => {
                Err(Error::upstream(self.id.clone(), status, "boom"))
            }
        }
    }
}

/// A count_tokens-capable provider entry (kind == "anthropic-api").
fn anthropic_api_entry() -> ProviderEntry {
    ProviderEntry::AnthropicApi {
        api_key_ref: "literal:k".into(),
        base_url: "https://placeholder.invalid".into(),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::default(),
        credential_source: Default::default(),
        header_extras: BTreeMap::new(),
        payload_extras: None,
        user_agent: None,
        allowed_betas: vec![],
        forward_client_headers: vec![],
        context_management: false,
        max_thinking_entry_bytes: None,
        cache_capability: None,
        auto_emit_top_level_breakpoint: None,
        reduction_enabled: None,
        cloak: routectl_providers::anthropic_api::CloakConfig::default(),
        runtime: ProviderRuntimePolicy::default(),
    }
}

/// A count_tokens-incapable provider entry (kind == "openai-compat").
/// Always compiled, regardless of the `bedrock` feature.
fn openai_compat_entry() -> ProviderEntry {
    ProviderEntry::OpenaiCompat {
        base_url: "https://placeholder.invalid/v1".into(),
        api_key_ref: "literal:k".into(),
        header_extras: BTreeMap::new(),
        payload_extras: None,
        user_agent: None,
        cache_capability: None,
        auto_emit_top_level_breakpoint: None,
        reduction_enabled: None,
        runtime: ProviderRuntimePolicy::default(),
    }
}

/// A count_tokens-incapable provider entry (kind == "bedrock").
/// Mirrors the motivating scenario from the spec. Bedrock has no
/// count_tokens endpoint, so its kind is skipped before dispatch.
#[cfg(feature = "bedrock")]
fn bedrock_entry() -> ProviderEntry {
    use crate::config::{BedrockApiShapeConfig, BedrockCredsConfig};
    ProviderEntry::Bedrock {
        region: "us-east-1".into(),
        api_shape: BedrockApiShapeConfig::default(),
        creds: BedrockCredsConfig::DefaultChain,
        user_agent: None,
        header_extras: BTreeMap::new(),
        payload_extras: None,
        anthropic_beta: vec![],
        cache_capability: None,
        auto_emit_top_level_breakpoint: None,
        reduction_enabled: None,
        runtime: ProviderRuntimePolicy::default(),
    }
}

/// One leg of a test chain: a provider entry + the matching mock
/// provider behavior.
struct Leg {
    nickname: &'static str,
    provider_name: &'static str,
    entry: ProviderEntry,
    behavior: CountBehavior,
}

/// Build a router whose alias `"alias"` resolves to the given legs
/// in order. Returns the router and the per-leg call counters (same
/// order as `legs`).
fn build_router(legs: Vec<Leg>) -> (Router, Vec<Arc<AtomicUsize>>) {
    let mut config = Config::default();
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    let mut counters: Vec<Arc<AtomicUsize>> = Vec::with_capacity(legs.len());
    let mut chain: Vec<String> = Vec::with_capacity(legs.len());

    for leg in legs {
        config
            .providers
            .insert(leg.provider_name.to_string(), leg.entry);
        let calls = Arc::new(AtomicUsize::new(0));
        counters.push(calls.clone());
        let provider: Arc<dyn Provider> = Arc::new(CountingProvider {
            id: leg.provider_name.to_string(),
            calls,
            behavior: leg.behavior,
        });
        models.insert(
            leg.nickname.to_string(),
            Arc::new(ResolvedModel::new(
                leg.nickname,
                leg.provider_name,
                provider,
                format!("upstream-{}", leg.nickname),
            )),
        );
        chain.push(leg.nickname.to_string());
    }

    config
        .aliases
        .insert("alias".into(), AliasValue::Chain(chain));
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    (router, counters)
}

fn count_req() -> ChatRequest {
    ChatRequest {
        model: "alias".into(),
        ..Default::default()
    }
}

#[cfg(feature = "bedrock")]
#[tokio::test]
async fn walks_past_incapable_bedrock_to_capable_anthropic() {
    // Arrange: chain [bedrock, anthropic-api]. Bedrock is not
    // count_tokens-capable and must be skipped BEFORE dispatch
    // (no call, no breaker account); the anthropic-api target
    // serves the count.
    let (router, counters) = build_router(vec![
        Leg {
            nickname: "bedrock-haiku",
            provider_name: "bedrock-prov",
            entry: bedrock_entry(),
            behavior: CountBehavior::Ok(99),
        },
        Leg {
            nickname: "anthropic-haiku",
            provider_name: "anthropic-prov",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::Ok(42),
        },
    ]);

    // Act
    let tc = router
        .count_tokens(count_req())
        .await
        .expect("capable target serves the count");

    // Assert: anthropic-api served (42), bedrock never called.
    assert_eq!(tc.input_tokens, 42);
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        0,
        "incapable bedrock target must be skipped, not dispatched",
    );
    assert_eq!(
        counters[1].load(Ordering::SeqCst),
        1,
        "capable anthropic-api target must serve the count",
    );
}

#[cfg(feature = "bedrock")]
#[tokio::test]
async fn all_incapable_chain_returns_not_implemented() {
    // Arrange: chain [bedrock] only -- no capable target anywhere.
    let (router, counters) = build_router(vec![Leg {
        nickname: "bedrock-haiku",
        provider_name: "bedrock-prov",
        entry: bedrock_entry(),
        behavior: CountBehavior::Ok(7),
    }]);

    // Act
    let err = router.count_tokens(count_req()).await.unwrap_err();

    // Assert: terminal 501, provider never touched.
    match err {
        Error::NotImplemented(model, msg) => {
            assert_eq!(model, "alias");
            assert!(
                msg.contains("count_tokens"),
                "message must name the operation; got: {msg}",
            );
        }
        other => panic!("expected Error::NotImplemented; got {other:?}"),
    }
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        0,
        "no capable target -> nothing dispatched",
    );
}

#[tokio::test]
async fn all_incapable_openai_compat_chain_returns_not_implemented() {
    // Feature-independent twin of the bedrock-only case: a single
    // openai-compat leg is also count_tokens-incapable.
    let (router, counters) = build_router(vec![Leg {
        nickname: "compat-model",
        provider_name: "compat-prov",
        entry: openai_compat_entry(),
        behavior: CountBehavior::Ok(7),
    }]);

    let err = router.count_tokens(count_req()).await.unwrap_err();

    match err {
        Error::NotImplemented(model, msg) => {
            assert_eq!(model, "alias");
            assert!(msg.contains("count_tokens"), "got: {msg}");
        }
        other => panic!("expected Error::NotImplemented; got {other:?}"),
    }
    assert_eq!(counters[0].load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn walks_past_incapable_openai_compat_to_capable_anthropic() {
    // Feature-independent walk: chain [openai-compat, anthropic-api].
    // The openai-compat leg cannot count_tokens and must be skipped
    // BEFORE dispatch; the anthropic-api leg serves. Pins the
    // skip-then-advance path on builds without the `bedrock` feature
    // (the bedrock-gated twin only runs with that feature compiled in).
    let (router, counters) = build_router(vec![
        Leg {
            nickname: "compat-model",
            provider_name: "compat-prov",
            entry: openai_compat_entry(),
            behavior: CountBehavior::Ok(99),
        },
        Leg {
            nickname: "anthropic-haiku",
            provider_name: "anthropic-prov",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::Ok(42),
        },
    ]);

    // Act
    let tc = router
        .count_tokens(count_req())
        .await
        .expect("capable target serves the count");

    // Assert: anthropic-api served (42), openai-compat never called.
    assert_eq!(tc.input_tokens, 42);
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        0,
        "incapable openai-compat target must be skipped, not dispatched",
    );
    assert_eq!(
        counters[1].load(Ordering::SeqCst),
        1,
        "capable anthropic-api target must serve the count",
    );
}

#[tokio::test]
async fn capable_primary_serves_unchanged() {
    // Arrange: chain [anthropic-api, anthropic-api]. The capable
    // primary serves; the second leg is never reached.
    let (router, counters) = build_router(vec![
        Leg {
            nickname: "anthropic-primary",
            provider_name: "anthropic-prov-a",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::Ok(11),
        },
        Leg {
            nickname: "anthropic-secondary",
            provider_name: "anthropic-prov-b",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::Ok(22),
        },
    ]);

    let tc = router
        .count_tokens(count_req())
        .await
        .expect("primary serves");

    assert_eq!(tc.input_tokens, 11, "first capable target serves");
    assert_eq!(counters[0].load(Ordering::SeqCst), 1);
    assert_eq!(
        counters[1].load(Ordering::SeqCst),
        0,
        "second target must not be reached when primary is capable",
    );
}

#[tokio::test]
async fn capable_target_upstream_error_propagates_without_walking() {
    // Arrange: chain [anthropic-api(500), anthropic-api(ok)]. The
    // selected capable target returns a real upstream error; it
    // MUST propagate and MUST NOT walk to the later capable entry
    // (try-and-fallback is reserved for the messages path -- a
    // kind-skip is operator-known, an upstream error is not).
    let (router, counters) = build_router(vec![
        Leg {
            nickname: "anthropic-primary",
            provider_name: "anthropic-prov-a",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::UpstreamError(500),
        },
        Leg {
            nickname: "anthropic-secondary",
            provider_name: "anthropic-prov-b",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::Ok(22),
        },
    ]);

    let err = router.count_tokens(count_req()).await.unwrap_err();

    assert!(
        matches!(err, Error::Upstream { status: 500, .. }),
        "upstream error must propagate; got {err:?}",
    );
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        1,
        "primary attempted once"
    );
    assert_eq!(
        counters[1].load(Ordering::SeqCst),
        0,
        "must NOT walk to a later target on a real upstream error",
    );
}

#[tokio::test]
async fn single_capable_seat_not_implemented_yields_terminal_not_implemented_once() {
    // A single capable (anthropic-api) seat that returns a local
    // NotImplemented is a capability error: it is dispatched exactly
    // once (no same-seat retry), the walk exhausts, and the CLIENT
    // sees the terminal walk-exhausted NotImplemented (named by the
    // ALIAS), NOT the seat's verbatim error.
    let (router, counters) = build_router(vec![Leg {
        nickname: "anthropic-only",
        provider_name: "anthropic-prov",
        entry: anthropic_api_entry(),
        behavior: CountBehavior::NotImplemented,
    }]);

    let err = router.count_tokens(count_req()).await.unwrap_err();

    match err {
        Error::NotImplemented(model, _) => assert_eq!(
            model, "alias",
            "must surface the terminal (alias-named) error, not the seat's verbatim one",
        ),
        other => panic!("expected Error::NotImplemented, got {other:?}"),
    }
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        1,
        "selected capable seat is dispatched once, no same-seat retry",
    );
}

/// A count_tokens-capable entry (kind == "anthropic-api") whose
/// breaker is configured: `circuit_failures` trip it, and a tripped
/// breaker holds for `circuit_cooldown_ms`. Lets a test observe
/// whether an outcome DEBITED the breaker: a debit (`record_failure`)
/// re-trips with the baseline cooldown (-> Open), while a no-debit
/// release leaves the armed zero-cooldown state (-> HalfOpenReady) or
/// a closed breaker (-> Closed).
fn anthropic_api_entry_with_breaker(
    circuit_failures: u32,
    circuit_cooldown_ms: u64,
) -> ProviderEntry {
    let mut entry = anthropic_api_entry();
    if let ProviderEntry::AnthropicApi { runtime, .. } = &mut entry {
        runtime.circuit_failures = Some(circuit_failures);
        runtime.circuit_cooldown_ms = Some(circuit_cooldown_ms);
    }
    entry
}

/// Whether the seat keyed by `state_key` currently holds the half-open
/// probe slot.
fn half_open_in_flight(router: &Router, state_key: &str) -> bool {
    router
        .state
        .get(state_key)
        .expect("seat state slot exists")
        .lock()
        .half_open_probe_in_flight()
}

/// Non-mutating breaker phase for the seat keyed by `state_key`.
fn circuit_phase(router: &Router, state_key: &str) -> crate::runtime_state::CircuitPhase {
    router
        .capacity_snapshot_for(state_key, Instant::now())
        .expect("seat state slot exists")
        .circuit
}

#[tokio::test]
async fn wire_501_on_half_open_probe_releases_slot_without_debiting_breaker() {
    // The incident pin. A capable-by-kind seat whose upstream cannot
    // count returns a WIRE 501. On a half-open count_tokens probe this
    // must be treated as a capability signal: release the probe slot
    // and leave the shared breaker un-debited. Recording it as a
    // health failure would re-trip the breaker (baseline cooldown) and
    // starve completions that gate on the same per-seat breaker.
    let (router, counters) = build_router(vec![Leg {
        nickname: "anthropic-only",
        provider_name: "anthropic-prov",
        entry: anthropic_api_entry_with_breaker(1, 60_000),
        behavior: CountBehavior::UpstreamError(501),
    }]);
    assert!(
        router.force_open_breaker("anthropic-only", Duration::ZERO),
        "seat breaker slot must exist to arm half-open",
    );

    let _ = router.count_tokens(count_req()).await;

    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        1,
        "the half-open probe must reach the upstream exactly once",
    );
    assert!(
        !half_open_in_flight(&router, "anthropic-only"),
        "a capability wire-501 must release the half-open probe slot",
    );
    assert_eq!(
        circuit_phase(&router, "anthropic-only"),
        crate::runtime_state::CircuitPhase::HalfOpenReady,
        "a capability wire-501 must NOT debit the breaker: no record_failure, \
             so the breaker keeps its armed zero-cooldown state (HalfOpenReady) \
             rather than re-tripping Open with the 60s baseline",
    );
}

#[tokio::test]
async fn local_not_implemented_on_half_open_probe_releases_slot_without_debiting() {
    // Guards the already-exempt case: a local Error::NotImplemented
    // from the selected capable seat is a capability signal and must
    // behave exactly like the wire-501 -- release the half-open slot,
    // no breaker debit.
    let (router, counters) = build_router(vec![Leg {
        nickname: "anthropic-only",
        provider_name: "anthropic-prov",
        entry: anthropic_api_entry_with_breaker(1, 60_000),
        behavior: CountBehavior::NotImplemented,
    }]);
    assert!(
        router.force_open_breaker("anthropic-only", Duration::ZERO),
        "seat breaker slot must exist to arm half-open",
    );

    let _ = router.count_tokens(count_req()).await;

    assert_eq!(counters[0].load(Ordering::SeqCst), 1);
    assert!(
        !half_open_in_flight(&router, "anthropic-only"),
        "a capability NotImplemented must release the half-open probe slot",
    );
    assert_eq!(
        circuit_phase(&router, "anthropic-only"),
        crate::runtime_state::CircuitPhase::HalfOpenReady,
        "a capability NotImplemented must NOT debit the breaker",
    );
}

#[tokio::test]
async fn walks_to_next_capable_seat_on_wire_501_and_returns_its_count() {
    // Chain [anthropic-api(501), anthropic-api(ok)]. The selected
    // capable seat returns a capability wire-501; count_tokens must
    // advance to the NEXT capable seat and return its count -- not
    // surface the 501 to the client. The first seat's breaker must
    // NOT be debited.
    let (router, counters) = build_router(vec![
        Leg {
            nickname: "anthropic-first",
            provider_name: "anthropic-prov-a",
            entry: anthropic_api_entry_with_breaker(1, 60_000),
            behavior: CountBehavior::UpstreamError(501),
        },
        Leg {
            nickname: "anthropic-second",
            provider_name: "anthropic-prov-b",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::Ok(42),
        },
    ]);

    let tc = router
        .count_tokens(count_req())
        .await
        .expect("walk must reach the second capable seat and return its count");

    assert_eq!(
        tc.input_tokens, 42,
        "the second capable seat serves the count",
    );
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        1,
        "first seat attempted once",
    );
    assert_eq!(
        counters[1].load(Ordering::SeqCst),
        1,
        "walk advanced to the second seat",
    );
    assert_eq!(
        circuit_phase(&router, "anthropic-first"),
        crate::runtime_state::CircuitPhase::Closed,
        "a capability 501 must not debit the first seat's breaker (stays Closed)",
    );
}

#[tokio::test]
async fn walk_terminates_with_not_implemented_when_all_capable_seats_501() {
    // Every capable seat returns a capability error. The walk must
    // visit each seat at most once (bounded upstream calls) and
    // terminate with the stable Error::NotImplemented rather than
    // looping or leaking the last upstream's raw 501 to the client.
    let (router, counters) = build_router(vec![
        Leg {
            nickname: "anthropic-first",
            provider_name: "anthropic-prov-a",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::UpstreamError(501),
        },
        Leg {
            nickname: "anthropic-second",
            provider_name: "anthropic-prov-b",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::UpstreamError(501),
        },
    ]);

    let err = router.count_tokens(count_req()).await.unwrap_err();

    match err {
        Error::NotImplemented(model, msg) => {
            assert_eq!(model, "alias");
            assert!(
                msg.contains("count_tokens"),
                "message must name the operation; got: {msg}",
            );
        }
        other => panic!("expected a terminal Error::NotImplemented; got {other:?}"),
    }
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        1,
        "first seat visited exactly once",
    );
    assert_eq!(
        counters[1].load(Ordering::SeqCst),
        1,
        "second seat visited exactly once (no re-visit, no loop)",
    );
}

#[tokio::test]
async fn non_capability_429_debits_and_returns_without_walking() {
    // Scope guard: a 429 is a HEALTH error, not a capability error. It
    // must keep today's behavior -- debit the breaker and propagate --
    // and must NOT walk to a later capable seat.
    let (router, counters) = build_router(vec![
        Leg {
            nickname: "anthropic-first",
            provider_name: "anthropic-prov-a",
            entry: anthropic_api_entry_with_breaker(1, 60_000),
            behavior: CountBehavior::UpstreamError(429),
        },
        Leg {
            nickname: "anthropic-second",
            provider_name: "anthropic-prov-b",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::Ok(42),
        },
    ]);

    let err = router.count_tokens(count_req()).await.unwrap_err();

    assert!(
        matches!(err, Error::Upstream { status: 429, .. }),
        "a 429 must propagate verbatim; got {err:?}",
    );
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        1,
        "first seat attempted once",
    );
    assert_eq!(
        counters[1].load(Ordering::SeqCst),
        0,
        "a health error must NOT walk to a later capable seat",
    );
    assert_eq!(
        circuit_phase(&router, "anthropic-first"),
        crate::runtime_state::CircuitPhase::Open,
        "a 429 must debit the breaker (threshold 1 -> Open)",
    );
}

#[tokio::test]
async fn non_retryable_4xx_leaves_breaker_closed() {
    // A caller-shaped 4xx (BadRequest class) from a capable count_tokens
    // seat must NOT debit the per-seat breaker that also gates
    // completions and streams. The debit keys off the failure CLASS, so
    // a repeated 4xx storm here leaves the shared breaker CLOSED and
    // every dispatch keeps reaching the seat.
    let (router, counters) = build_router(vec![Leg {
        nickname: "anthropic-only",
        provider_name: "anthropic-prov",
        entry: anthropic_api_entry_with_breaker(2, 60_000),
        behavior: CountBehavior::UpstreamError(400),
    }]);

    for _ in 0..4 {
        let err = router.count_tokens(count_req()).await.unwrap_err();
        assert!(
            matches!(err, Error::Upstream { status: 400, .. }),
            "a count_tokens 4xx must surface verbatim; got {err:?}",
        );
    }

    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        4,
        "a non-debiting 4xx must never trip the breaker, so every \
             dispatch reaches the capable seat",
    );
    assert_eq!(
        circuit_phase(&router, "anthropic-only"),
        crate::runtime_state::CircuitPhase::Closed,
        "a non-retryable 4xx storm must leave the count_tokens seat \
             breaker CLOSED (BadRequest class does not debit)",
    );
}

#[tokio::test]
async fn health_5xx_still_debits_breaker() {
    // Complement to the 4xx case: a 5xx (ServerError class) from a
    // capable count_tokens seat is a health failure and must still debit
    // and trip the shared per-seat breaker.
    let (router, counters) = build_router(vec![Leg {
        nickname: "anthropic-only",
        provider_name: "anthropic-prov",
        entry: anthropic_api_entry_with_breaker(1, 60_000),
        behavior: CountBehavior::UpstreamError(503),
    }]);

    let err = router.count_tokens(count_req()).await.unwrap_err();

    assert!(
        matches!(err, Error::Upstream { status: 503, .. }),
        "a count_tokens 5xx must surface verbatim; got {err:?}",
    );
    assert_eq!(counters[0].load(Ordering::SeqCst), 1);
    assert_eq!(
        circuit_phase(&router, "anthropic-only"),
        crate::runtime_state::CircuitPhase::Open,
        "a count_tokens 5xx (ServerError class) must debit and trip the \
             breaker (threshold 1 -> Open)",
    );
}

#[tokio::test]
async fn walk_reruns_gate_on_next_seat_and_respects_open_breaker() {
    // Guardrail: the capability walk must re-run the gate on each new
    // seat. If the next capable seat's breaker is open, the walk must
    // NOT bypass it -- the gate blocks the dispatch and the
    // circuit-open error surfaces (the seat is never called).
    let (router, counters) = build_router(vec![
        Leg {
            nickname: "anthropic-first",
            provider_name: "anthropic-prov-a",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::UpstreamError(501),
        },
        Leg {
            nickname: "anthropic-second",
            provider_name: "anthropic-prov-b",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::Ok(42),
        },
    ]);
    // Park the second seat's breaker open for a long, un-elapsed
    // cooldown so its gate returns CircuitOpen (not a half-open probe
    // admission).
    assert!(
        router.force_open_breaker("anthropic-second", Duration::from_hours(1)),
        "second seat breaker slot must exist",
    );

    let err = router.count_tokens(count_req()).await.unwrap_err();

    assert!(
        matches!(&err, Error::Upstream { status: 0, body, .. } if body.contains("circuit breaker")),
        "the walk must re-gate the second seat and surface its open-breaker block; got {err:?}",
    );
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        1,
        "first seat attempted once (capability 501)",
    );
    assert_eq!(
        counters[1].load(Ordering::SeqCst),
        0,
        "an open breaker on the walked-to seat must block the dispatch, not be bypassed",
    );
}
