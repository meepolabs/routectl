//! Pin: `Router::count_tokens` walks PAST count_tokens-incapable
//! targets to the first capable one, returning 501 NotImplemented
//! only when NO target in the chain is capable. The capability skip
//! is decided from static config (egress kind + upstream model id)
//! BEFORE dispatch -- it is operator-known, not upstream health --
//! so it never touches the breaker. A CAPABLE target that returns a
//! real upstream error propagates as today (no further walk). Every
//! admitted target shares the same Anthropic tokenizer family, so
//! walking past incapable targets does NOT reintroduce the
//! wrong-tokenizer hazard.
//!
//! Host module for this sidecar's test groups: the breaker-accounting
//! and seat-capability groups live in the `include!`d fragments at the
//! bottom and compile into THIS module, so all imports and shared
//! helpers stay here.
use super::*;
use crate::config::{AliasValue, Config, ProviderEntry, ProviderRuntimePolicy};
use crate::resolved::ResolvedModel;
use async_trait::async_trait;
use futures::stream::BoxStream;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Provider, TokenCount};
use routectl_providers::anthropic_api::AuthKind;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

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
/// can prove a target was (or was NOT) dispatched to. Whether the
/// walk admits the seat at all is decided by the matching
/// `ProviderEntry` plus the seat's upstream model id, NOT by this
/// impl -- so a skipped seat stays skipped regardless of what this
/// would have returned.
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
        #[cfg(feature = "bedrock")]
        bedrock_mantle: None,
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
        #[cfg(feature = "bedrock")]
        bedrock_mantle: None,
        runtime: ProviderRuntimePolicy::default(),
    }
}

/// A Bedrock model id whose vendor segment is not Anthropic, so a seat
/// on it counts with a different tokenizer and must never be admitted.
#[cfg(feature = "bedrock")]
const NON_ANTHROPIC_BEDROCK_MODEL: &str = "us.meta.llama4-scout-17b-instruct-v1:0";

/// A Bedrock model id that is provably Anthropic-family.
#[cfg(feature = "bedrock")]
const ANTHROPIC_BEDROCK_MODEL: &str = "us.anthropic.claude-haiku-4-5-20251001-v1:0";

/// An inference-profile ARN: the string proves no vendor, so the
/// tokenizer behind it is unknown.
#[cfg(feature = "bedrock")]
const ARN_BEDROCK_MODEL: &str =
    "arn:aws:bedrock:us-east-1:123456789012:inference-profile/some-profile";

/// A provider entry of kind "bedrock". Whether such a seat can count
/// depends on its upstream model id: the Bedrock lane counts with the
/// model's own tokenizer, so only an Anthropic-family model id is
/// admitted.
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
    /// Wire model id this seat would send upstream. `None` derives a
    /// synthetic id from the nickname, which is all a seat whose
    /// capability does not depend on the model id needs.
    upstream: Option<&'static str>,
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
        let upstream = leg
            .upstream
            .map_or_else(|| format!("upstream-{}", leg.nickname), str::to_string);
        models.insert(
            leg.nickname.to_string(),
            Arc::new(ResolvedModel::new(
                leg.nickname,
                leg.provider_name,
                provider,
                upstream,
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
    // Arrange: chain [bedrock, anthropic-api]. The bedrock seat's
    // upstream id names no Anthropic-family model, so it is not
    // count_tokens-capable and must be skipped BEFORE dispatch (no
    // call, no breaker account); the anthropic-api target serves.
    let (router, counters) = build_router(vec![
        Leg {
            nickname: "bedrock-llama",
            provider_name: "bedrock-prov",
            entry: bedrock_entry(),
            behavior: CountBehavior::Ok(99),
            upstream: Some(NON_ANTHROPIC_BEDROCK_MODEL),
        },
        Leg {
            nickname: "anthropic-haiku",
            provider_name: "anthropic-prov",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::Ok(42),
            upstream: None,
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
    // Arrange: chain [bedrock] only, on a non-Anthropic-family
    // upstream id -- no capable target anywhere.
    let (router, counters) = build_router(vec![Leg {
        nickname: "bedrock-llama",
        provider_name: "bedrock-prov",
        entry: bedrock_entry(),
        behavior: CountBehavior::Ok(7),
        upstream: Some(NON_ANTHROPIC_BEDROCK_MODEL),
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
        upstream: None,
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
            upstream: None,
        },
        Leg {
            nickname: "anthropic-haiku",
            provider_name: "anthropic-prov",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::Ok(42),
            upstream: None,
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
            upstream: None,
        },
        Leg {
            nickname: "anthropic-secondary",
            provider_name: "anthropic-prov-b",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::Ok(22),
            upstream: None,
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
            upstream: None,
        },
        Leg {
            nickname: "anthropic-secondary",
            provider_name: "anthropic-prov-b",
            entry: anthropic_api_entry(),
            behavior: CountBehavior::Ok(22),
            upstream: None,
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
        upstream: None,
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

/// A "bedrock" entry whose breaker is configured, so a test can observe
/// whether an outcome on that seat debited it (see
/// `anthropic_api_entry_with_breaker`).
#[cfg(feature = "bedrock")]
fn bedrock_entry_with_breaker(circuit_failures: u32, circuit_cooldown_ms: u64) -> ProviderEntry {
    let mut entry = bedrock_entry();
    if let ProviderEntry::Bedrock { runtime, .. } = &mut entry {
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

include!("count_tokens_breaker_tests.rs");
include!("count_tokens_seat_capability_tests.rs");
