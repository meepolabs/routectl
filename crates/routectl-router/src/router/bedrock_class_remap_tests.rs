//! Bedrock-specific acceptance coverage for the per-provider status
//! remap, layered on top of `provider_remap_tests`' 503->content-policy
//! and provenance coverage: a `kind = "bedrock"` provider entry whose
//! `[providers.X.class_overrides]` remaps 400 to feature-unsupported.
//!
//! The remap is behaviorally inert over the baseline for routing --
//! `BadRequest` and `FeatureUnsupported` share the same terminal
//! (retry_cap 0, fallback true, no debit) policy row -- so the
//! deliverable under test is the label + the observability events, plus
//! two regression pins: the remapped 400 must not debit the breaker or
//! retry the same provider (it must still advance the chain to a
//! fallback target), and an UNRELATED status (500) on the same
//! provider, with the remap block present, must behave exactly like no
//! remap at all.

use super::remap_test_support::{CountingFailingProvider, find_decision, req_m1, router_from_toml};
use super::*;
use routectl_testkit::with_capture;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Parse `toml_text` and install a two-leg alias chain: `m1` on
/// provider `p1`, `m2` on provider `p2`. `[aliases] alias = ["m1",
/// "m2"]` must already be present in `toml_text`.
fn two_leg_router_from_toml(
    toml_text: &str,
    leg1: Arc<dyn Provider>,
    leg2: Arc<dyn Provider>,
) -> Router {
    let config: Config = toml::from_str(toml_text).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m1".to_string(),
        Arc::new(ResolvedModel::new("m1", "p1", leg1, "wire-model-1")),
    );
    models.insert(
        "m2".to_string(),
        Arc::new(ResolvedModel::new("m2", "p2", leg2, "wire-model-2")),
    );
    router.install_resolved_models(models);
    router
}

fn req_alias() -> ChatRequest {
    ChatRequest {
        model: "alias".into(),
        messages: vec![],
        ..Default::default()
    }
}

/// Non-mutating breaker phase for the seat keyed by `state_key`.
fn circuit_phase(router: &Router, state_key: &str) -> crate::runtime_state::CircuitPhase {
    router
        .capacity_snapshot_for(state_key, Instant::now())
        .expect("seat state slot exists")
        .circuit
}

#[tokio::test]
async fn bedrock_400_remaps_to_feature_unsupported_with_operator_capability_token() {
    // Arrange: a bedrock-kind provider whose 400 is remapped to
    // feature-unsupported. A plain 400 with no upstream type/code
    // natively classifies as bad_request (checked below), so the
    // remap is the only reason this ends up feature-unsupported.
    let toml_text = r#"
[providers.p1]
kind = "bedrock"
region = "us-east-1"
creds = { kind = "default-chain" }

[providers.p1.class_overrides]
400 = "feature-unsupported"
"#;
    let provider = Arc::new(CountingFailingProvider {
        id: "p1".into(),
        status: 400,
        calls: AtomicUsize::new(0),
    });
    let router = router_from_toml(toml_text, provider);

    // Act
    let (result, events) = with_capture(router.complete(req_m1())).await;

    // Assert: the feature_unsupported event fires with the
    // operator-remap capability token and remapped=true.
    assert!(result.is_err());
    let fu = events
        .iter()
        .find(|e| e.target == "routectl::feature_unsupported")
        .expect("feature_unsupported event must fire on an operator remap");
    assert_eq!(
        fu.field("capability"),
        Some(crate::class_policy::OPERATOR_REMAP_CAPABILITY)
    );
    assert_eq!(fu.field("remapped"), Some("true"));
    assert_eq!(fu.field("provider_kind"), Some("bedrock"));

    // Assert: the class-decision event carries the original (native)
    // class alongside the remapped effective class.
    let ev = find_decision(&events);
    assert_eq!(ev.field("remapped"), Some("true"));
    assert_eq!(ev.field("remap_status"), Some("Some(400)"));
    assert_eq!(ev.field("original_class"), Some("bad_request"));
    assert_eq!(ev.field("effective_class"), Some("feature_unsupported"));
}

#[tokio::test]
async fn bedrock_remapped_400_does_not_debit_breaker_and_chain_advances_to_fallback() {
    // Arrange: p1 (bedrock) remaps 400 to feature-unsupported and
    // carries a hair-trigger breaker (circuit_failures = 1); p2 is
    // the fallback leg. Repeated calls well past the threshold must
    // never trip p1's breaker (feature-unsupported never debits), and
    // every call must still reach p2 (no same-provider retry on p1,
    // whose remapped retry_cap is the baked 0).
    let toml_text = r#"
[providers.p1]
kind = "bedrock"
region = "us-east-1"
creds = { kind = "default-chain" }
circuit_failures = 1

[providers.p1.class_overrides]
400 = "feature-unsupported"

[providers.p2]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"

[aliases]
alias = ["m1", "m2"]
"#;
    let p1 = Arc::new(CountingFailingProvider {
        id: "p1".into(),
        status: 400,
        calls: AtomicUsize::new(0),
    });
    let p2 = Arc::new(CountingFailingProvider {
        id: "p2".into(),
        status: 400,
        calls: AtomicUsize::new(0),
    });
    let router = two_leg_router_from_toml(toml_text, p1.clone(), p2.clone());

    // Act: fire well past the configured circuit_failures=1 threshold.
    const ATTEMPTS: usize = 3;
    for _ in 0..ATTEMPTS {
        let result = router.complete(req_alias()).await;
        assert!(result.is_err());
    }

    // Assert: no same-provider retry on the remap (retry_cap 0) --
    // p1 is dispatched exactly once per request.
    assert_eq!(
        p1.calls.load(Ordering::SeqCst),
        ATTEMPTS,
        "the remapped 400's baked retry_cap is 0: no same-provider retry"
    );
    // Assert: the chain advances to the fallback target every time.
    assert_eq!(
        p2.calls.load(Ordering::SeqCst),
        ATTEMPTS,
        "feature-unsupported still falls back to the next chain entry"
    );
    // Assert: the breaker never trips, even past its threshold --
    // feature-unsupported never debits.
    assert_eq!(
        circuit_phase(&router, "m1"),
        crate::runtime_state::CircuitPhase::Closed,
        "a remapped feature-unsupported outcome must never debit the breaker"
    );
}

#[tokio::test]
async fn bedrock_500_with_remap_block_present_behaves_like_no_remap() {
    // Arrange: identical to `provider_remap_tests::
    // without_override_503_debits_and_retries_per_baseline`'s
    // baseline, but on a bedrock-kind provider that ALSO carries a
    // class_overrides block -- for an unrelated status (400). A 500
    // must debit and retry per retry_on_5xx exactly as if the remap
    // block were absent.
    let toml_text = r#"
[retry]
max_attempts = 3
retry_on_5xx = 2

[providers.p1]
kind = "bedrock"
region = "us-east-1"
creds = { kind = "default-chain" }

[providers.p1.class_overrides]
400 = "feature-unsupported"
"#;
    let provider = Arc::new(CountingFailingProvider {
        id: "p1".into(),
        status: 500,
        calls: AtomicUsize::new(0),
    });
    let router = router_from_toml(toml_text, provider.clone());

    // Act
    let (result, events) = with_capture(router.complete(req_m1())).await;

    // Assert: the baked retry_on_5xx=2 cap is exhausted before
    // falling back -- the presence of an unrelated remap block for
    // 400 changes nothing about the 500 path.
    assert!(result.is_err());
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "normal 5xx traffic retries the same provider up to retry_on_5xx \
             regardless of an unrelated class_overrides entry"
    );
    let ev = find_decision(&events);
    assert_eq!(ev.field("remapped"), Some("false"));
    assert_eq!(ev.field("remap_status"), Some("None"));
    assert_eq!(ev.field("original_class"), Some("server_error"));
    assert_eq!(
        ev.field("effective_class"),
        ev.field("original_class"),
        "no remap for status 500 means effective == original"
    );
    assert_eq!(ev.field("debit"), Some("true"));
    assert_eq!(ev.field("retry_cap"), Some("2"));
}
