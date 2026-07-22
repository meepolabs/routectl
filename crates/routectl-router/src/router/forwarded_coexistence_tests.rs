//! The per-provider passthrough model dissolves the earlier whole-chain
//! forwarded-passthrough gate (`enforce_forwarded_anthropic_target` /
//! `target_is_anthropic_egress`
//! / `FORWARDED_EGRESS_KIND`, deleted): `credential_source = "forwarded"`
//! is now a PER-PROVIDER config, not a request-global mode switch, so
//! an alias routes exactly like any other -- no whole-chain refusal, no
//! steering. These tests cover what replaces it:
//!
//! - A request carrying a captured forwarded bearer no longer bends
//!   routing: an alias to an OWN-credential provider (any kind, mixed
//!   chain or not) dispatches with that provider's own credentials,
//!   and is never refused up front.
//! - A forwarded-CREDENTIAL target (an `anthropic-api` provider with
//!   `credential_source = "forwarded"`) with NO captured bearer
//!   refuses cleanly BEFORE egress -- the compensating guard paired
//!   with the gate deletion -- and the guard is per-target, so a
//!   chain that never reaches that target is unaffected.
//!
//! The broader "still refreshes and falls back" coexistence
//! regression -- a MITM-marked request routed to an OWN-credential
//! Anthropic provider behaves exactly as before the change, and the
//! floating bearer
//! is never consumed by it -- lives in `forwarded_auth_terminal_tests`,
//! next to the terminal-bypass mocks it reuses.
use super::*;
use crate::config::{CredentialSource, ProviderEntry, ProviderRuntimePolicy};
use crate::resolved::ResolvedModel;
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use routectl_core::schema::ForwardedBearer;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Provider, TokenCount};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The forwarded token used across tests. Distinctive so any leak
/// into a log field, log message, or client error is unmistakable.
const FORWARDED_TOKEN: &str = "sk-ant-oat01-FORWARDED-SECRET-must-never-surface";

/// Mock provider that records every dispatch call so a test can prove
/// a target was (or was NOT) reached. Every method returns a benign
/// success, so the ONLY reason a call count stays zero is that the
/// router refused BEFORE dispatch.
struct RecordingProvider {
    id: String,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Ok(ChatResponse::default())
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse::default())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(futures::stream::once(async move { Ok(ChatChunk::default()) }).boxed())
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(TokenCount::default())
    }
}

/// An `anthropic-api` provider entry on the default (api.anthropic.com)
/// host, with `credential_source` set per `forwarded`.
fn anthropic_entry(forwarded: bool) -> ProviderEntry {
    let entry = ProviderEntry::anthropic_api("literal:k");
    if forwarded {
        entry.with_credential_source(CredentialSource::Forwarded)
    } else {
        entry
    }
}

/// A non-`anthropic-api`, OWN-credential provider entry.
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

/// One leg of a test chain: config entry + matching recording mock.
struct Leg {
    nickname: &'static str,
    provider_name: &'static str,
    entry: ProviderEntry,
}

/// Build a router whose alias `"alias"` resolves to `legs` in order.
/// Returns the router and the per-leg dispatch-call counters (in leg
/// order) so a test can prove which target was reached.
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
        let provider: Arc<dyn Provider> = Arc::new(RecordingProvider {
            id: leg.provider_name.to_string(),
            calls,
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

fn plain_req() -> ChatRequest {
    ChatRequest {
        model: "alias".into(),
        ..Default::default()
    }
}

fn forwarded_req() -> ChatRequest {
    let mut req = plain_req();
    req.routectl_internal.forwarded_bearer =
        Some(ForwardedBearer::new(FORWARDED_TOKEN.to_string()));
    req
}

// ---- coexistence: a captured bearer no longer steers routing ----

#[tokio::test]
async fn complete_forwarded_bearer_present_routes_to_own_compat_normally() {
    let (router, counters) = build_router(vec![Leg {
        nickname: "compat",
        provider_name: "compat-prov",
        entry: openai_compat_entry(),
    }]);

    router
        .complete(forwarded_req())
        .await
        .expect("an OWN-credential target dispatches regardless of a captured bearer");

    assert_eq!(counters[0].load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn complete_forwarded_bearer_present_mixed_chain_own_first_entry_dispatches_normally() {
    // Chain [own-anthropic, openai-compat], request carries a
    // captured bearer. The earlier whole-chain gate would have refused
    // this up front because entry 1 is non-Anthropic; routing is now
    // purely by
    // alias -- the first entry succeeds and the second is never
    // reached.
    let (router, counters) = build_router(vec![
        Leg {
            nickname: "anthropic",
            provider_name: "anthropic-prov",
            entry: anthropic_entry(false),
        },
        Leg {
            nickname: "compat",
            provider_name: "compat-prov",
            entry: openai_compat_entry(),
        },
    ]);

    router
        .complete(forwarded_req())
        .await
        .expect("a mixed OWN-credential chain is never refused up front");

    assert_eq!(counters[0].load(Ordering::SeqCst), 1);
    assert_eq!(
        counters[1].load(Ordering::SeqCst),
        0,
        "first entry succeeds, so the second is never reached",
    );
}

#[tokio::test]
async fn count_tokens_forwarded_bearer_present_routes_to_own_compat_capability_error() {
    // openai-compat is not count_tokens-capable by kind: without any
    // forwarded gate, the walk simply reports NotImplemented, same as
    // a plain request -- a captured bearer must not change this.
    let (router, counters) = build_router(vec![Leg {
        nickname: "compat",
        provider_name: "compat-prov",
        entry: openai_compat_entry(),
    }]);

    let err = router.count_tokens(forwarded_req()).await.unwrap_err();

    assert!(matches!(err, Error::NotImplemented(..)), "got {err:?}");
    assert_eq!(counters[0].load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stream_forwarded_bearer_present_routes_to_own_compat_normally() {
    let (router, counters) = build_router(vec![Leg {
        nickname: "compat",
        provider_name: "compat-prov",
        entry: openai_compat_entry(),
    }]);

    let _stream = router
        .stream(forwarded_req())
        .await
        .expect("an OWN-credential target dispatches regardless of a captured bearer");

    assert_eq!(counters[0].load(Ordering::SeqCst), 1);
}

// ---- missing-bearer terminal guard ----
//
// A forwarded-CREDENTIAL target (provider `credential_source =
// "forwarded"`) with NO captured bearer must refuse cleanly BEFORE
// egress -- never an ambiguous upstream 401 -- in all three dispatch
// paths. The guard is per-target: it fires only for the target about
// to be dispatched to.

#[tokio::test]
async fn complete_forwarded_target_missing_bearer_refused_before_dispatch() {
    let (router, counters) = build_router(vec![Leg {
        nickname: "fwd",
        provider_name: "fwd-prov",
        entry: anthropic_entry(true),
    }]);

    let err = router.complete(plain_req()).await.unwrap_err();

    assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    assert!(
        err.to_string().contains("missing_forwarded_bearer"),
        "refuse message must carry the reason; got: {err}",
    );
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        0,
        "a forwarded target with no captured bearer must never be dispatched to",
    );
}

#[tokio::test]
async fn count_tokens_forwarded_target_missing_bearer_refused_before_dispatch() {
    let (router, counters) = build_router(vec![Leg {
        nickname: "fwd",
        provider_name: "fwd-prov",
        entry: anthropic_entry(true),
    }]);

    let err = router.count_tokens(plain_req()).await.unwrap_err();

    assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    assert!(err.to_string().contains("missing_forwarded_bearer"));
    assert_eq!(counters[0].load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn stream_forwarded_target_missing_bearer_refused_before_dispatch() {
    let (router, counters) = build_router(vec![Leg {
        nickname: "fwd",
        provider_name: "fwd-prov",
        entry: anthropic_entry(true),
    }]);

    let err = router.stream(plain_req()).await.err().expect("refused");

    assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    assert!(err.to_string().contains("missing_forwarded_bearer"));
    assert_eq!(counters[0].load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn complete_forwarded_target_with_captured_bearer_dispatches_normally() {
    let (router, counters) = build_router(vec![Leg {
        nickname: "fwd",
        provider_name: "fwd-prov",
        entry: anthropic_entry(true),
    }]);

    router
        .complete(forwarded_req())
        .await
        .expect("a forwarded target with a captured bearer must dispatch");

    assert_eq!(counters[0].load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn complete_mixed_chain_own_first_entry_succeeds_forwarded_missing_bearer_never_reached() {
    // Per-target guard: a chain whose first (OWN-credential) entry
    // succeeds never reaches the forwarded second entry, so the
    // missing-bearer guard never fires even though the request has
    // no captured bearer at all.
    let (router, counters) = build_router(vec![
        Leg {
            nickname: "own",
            provider_name: "own-prov",
            entry: anthropic_entry(false),
        },
        Leg {
            nickname: "fwd",
            provider_name: "fwd-prov",
            entry: anthropic_entry(true),
        },
    ]);

    router
        .complete(plain_req())
        .await
        .expect("the first entry succeeds without ever touching the forwarded target");

    assert_eq!(counters[0].load(Ordering::SeqCst), 1);
    assert_eq!(
        counters[1].load(Ordering::SeqCst),
        0,
        "the forwarded second entry must never be reached",
    );
}

#[tokio::test]
async fn missing_bearer_refuse_client_error_carries_no_token() {
    // The client never captured a bearer in this scenario, so there
    // is nothing to leak -- but pin that the refuse message stays
    // generic (reason only) and never echoes request content.
    let (router, _counters) = build_router(vec![Leg {
        nickname: "fwd",
        provider_name: "fwd-prov",
        entry: anthropic_entry(true),
    }]);

    let err = router.complete(plain_req()).await.unwrap_err();
    let client_msg = err.to_string();

    assert!(!client_msg.contains(FORWARDED_TOKEN));
}

// ---- real-factory build integration ----
//
// Every test above wires a `RecordingProvider` mock straight onto a
// `ResolvedModel`, bypassing `crate::factory::build_provider`
// entirely. That is exactly why a factory-side bug (unconditionally
// resolving a token from a forwarded entry's guaranteed-empty
// `api_key_ref`) could break `serve` while every dispatch-behavior
// test here kept passing. This test drives the REAL factory build
// for the forwarded leg so the two layers are exercised together.

#[tokio::test]
async fn forwarded_target_built_via_real_factory_still_refuses_missing_bearer() {
    let entry = anthropic_entry(true);
    let secrets: Arc<dyn routectl_auth::SecretStore> = Arc::new(routectl_auth::MemoryStore::new());
    let provider = crate::factory::build_provider("fwd-prov", &entry, secrets)
        .await
        .expect("a valid forwarded provider entry must build");

    let mut config = Config::default();
    config.providers.insert("fwd-prov".into(), entry);
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "fwd".into(),
        Arc::new(ResolvedModel::new(
            "fwd",
            "fwd-prov",
            provider,
            "upstream-fwd".to_string(),
        )),
    );
    config
        .aliases
        .insert("alias".into(), AliasValue::Chain(vec!["fwd".into()]));
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);

    // No captured bearer: the router's missing-bearer guard must
    // refuse cleanly before ever calling into the real provider
    // (which would otherwise try to egress with no credential).
    let err = router.complete(plain_req()).await.unwrap_err();

    assert!(matches!(err, Error::Validation(_)), "got {err:?}");
    assert!(
        err.to_string().contains("missing_forwarded_bearer"),
        "got: {err}",
    );
}
