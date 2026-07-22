//! End-to-end coverage for the per-provider status remap
//! (`[providers.X.class_overrides]`) parsed through the REAL TOML
//! path -- exercising `ConfigFailureClass::to_failure_class`, not a
//! hand-built `FailureClass` -- and its effect on debit / same-
//! provider retry / fallback plus the class-decision provenance
//! fields (`original_class` / `effective_class` / `remapped` /
//! `remap_status`) and the `feature_unsupported` event's `remapped`
//! field.

use super::remap_test_support::{CountingFailingProvider, find_decision, req_m1, router_from_toml};
use super::*;
use routectl_testkit::with_capture;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn override_stops_debit_and_same_provider_retry_but_keeps_fallback_true() {
    // Arrange: baseline would allow 2 same-provider retries on a 5xx
    // (retry_on_5xx = 2); the operator remaps THIS provider's 503 to
    // content-policy (baked cap 0, fallback true).
    let toml_text = r#"
[retry]
max_attempts = 3
retry_on_5xx = 2

[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"

[providers.p1.class_overrides]
503 = "content-policy"
"#;
    let provider = Arc::new(CountingFailingProvider {
        id: "p1".into(),
        status: 503,
        calls: AtomicUsize::new(0),
    });
    let router = router_from_toml(toml_text, provider.clone());

    // Act
    let (result, events) = with_capture(router.complete(req_m1())).await;

    // Assert: the remap's retry_cap of 0 means exactly one call --
    // no same-provider retry fired.
    assert!(result.is_err());
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "content-policy's baked retry cap is 0"
    );
    let ev = find_decision(&events);
    assert_eq!(ev.field("remapped"), Some("true"));
    assert_eq!(ev.field("remap_status"), Some("Some(503)"));
    assert_eq!(ev.field("original_class"), Some("server_error"));
    assert_eq!(ev.field("effective_class"), Some("content_policy"));
    assert_eq!(
        ev.field("debit"),
        Some("false"),
        "content-policy never debits"
    );
    assert_eq!(ev.field("retry_cap"), Some("0"));
    assert_eq!(
        ev.field("fallback"),
        Some("true"),
        "content-policy still falls back by baked default"
    );
}

#[tokio::test]
async fn without_override_503_debits_and_retries_per_baseline() {
    // Arrange: identical policy, no `class_overrides` -- 503 stays
    // ServerError and follows the baked debit + retry_on_5xx cap.
    let toml_text = r#"
[retry]
max_attempts = 3
retry_on_5xx = 2

[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"
"#;
    let provider = Arc::new(CountingFailingProvider {
        id: "p1".into(),
        status: 503,
        calls: AtomicUsize::new(0),
    });
    let router = router_from_toml(toml_text, provider.clone());

    // Act
    let (result, events) = with_capture(router.complete(req_m1())).await;

    // Assert: the baked retry_on_5xx=2 cap is exhausted before
    // falling back, so the provider is dispatched twice.
    assert!(result.is_err());
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "baseline retries the same provider up to retry_on_5xx"
    );
    let ev = find_decision(&events);
    assert_eq!(ev.field("remapped"), Some("false"));
    assert_eq!(ev.field("remap_status"), Some("None"));
    assert_eq!(ev.field("original_class"), Some("server_error"));
    assert_eq!(
        ev.field("effective_class"),
        ev.field("original_class"),
        "no remap means effective == original"
    );
    assert_eq!(ev.field("debit"), Some("true"));
    assert_eq!(ev.field("retry_cap"), Some("2"));
}

#[tokio::test]
async fn feature_unsupported_event_remapped_true_when_target_is_operator_remap() {
    // Arrange: the operator remaps 429 (native RateLimited) to
    // feature-unsupported -- the classifier never produced this
    // lift; it is entirely config-sourced.
    let toml_text = r#"
[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"

[providers.p1.class_overrides]
429 = "feature-unsupported"
"#;
    let provider = Arc::new(CountingFailingProvider {
        id: "p1".into(),
        status: 429,
        calls: AtomicUsize::new(0),
    });
    let router = router_from_toml(toml_text, provider);

    // Act
    let (result, events) = with_capture(router.complete(req_m1())).await;

    // Assert
    assert!(result.is_err());
    let ev = events
        .iter()
        .find(|e| e.target == "routectl::feature_unsupported")
        .expect("feature_unsupported event must fire on an operator remap");
    assert_eq!(
        ev.field("capability"),
        Some(crate::class_policy::OPERATOR_REMAP_CAPABILITY)
    );
    assert_eq!(
        ev.field("remapped"),
        Some("true"),
        "an operator remap into feature-unsupported must be flagged"
    );
}

#[tokio::test]
async fn retry_classes_fallback_only_override_leaves_retry_cap_at_baked_value() {
    // Review-nit regression: `[retry.classes.server-error]` sets
    // ONLY `fallback`, leaving `retry` unset. A sparse leaf-merge
    // bug would zero out the cap instead of deferring to the baked
    // `retry_on_5xx`. No `class_overrides` involved -- this pins the
    // GLOBAL per-class overlay, independent of the per-provider
    // remap this task adds.
    let toml_text = r#"
[retry]
max_attempts = 3
retry_on_5xx = 4

[retry.classes.server-error]
fallback = false

[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"
"#;
    let provider = Arc::new(CountingFailingProvider {
        id: "p1".into(),
        status: 500,
        calls: AtomicUsize::new(0),
    });
    let router = router_from_toml(toml_text, provider);

    // Act
    let (result, events) = with_capture(router.complete(req_m1())).await;

    // Assert
    assert!(result.is_err());
    let ev = find_decision(&events);
    assert_eq!(
        ev.field("retry_cap"),
        Some("4"),
        "a fallback-only override must not disturb the baked retry cap"
    );
    assert_eq!(ev.field("fallback"), Some("false"));
    assert_eq!(ev.field("remapped"), Some("false"));
}
