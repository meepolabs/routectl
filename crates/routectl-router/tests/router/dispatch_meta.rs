//! DispatchMeta: router-scoped accounting on Ok, Err, and gate-blocked paths.

use super::*;

#[tokio::test]
async fn dispatched_meta_on_success_carries_served_target() {
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let r = router_with_config_providers(
        &["m1", "m2"],
        vec![
            ("m1".into(), "p1".into(), "up1".into()),
            ("m2".into(), "p2".into(), "up2".into()),
        ],
        vec![
            ("p1".into(), p1 as Arc<dyn Provider>),
            ("p2".into(), p2 as Arc<dyn Provider>),
        ],
        default_test_retry(),
    );

    let Dispatched { meta, result } = r
        .complete_with_options(req("fast"), RouterOptions::new())
        .await;
    result.expect("ok");
    assert_eq!(meta.attempt_count, 1);
    assert_eq!(meta.fallback_count, 0);
    assert_eq!(meta.served_provider.as_deref(), Some("p1"));
    assert_eq!(meta.served_provider_kind.as_deref(), Some("openai-compat"));
    assert_eq!(meta.served_model.as_deref(), Some("m1"));
    assert_eq!(meta.served_upstream.as_deref(), Some("up1"));
    assert_eq!(meta.resolved_alias, "fast");
}

#[tokio::test]
async fn dispatched_meta_on_all_failed_reflects_full_walk() {
    // Both entries 503 every attempt: the chain is fully walked and the
    // terminal target is the LAST entry. Proves meta is built in the
    // outer scope, not lost on the Err return.
    let p1 = MockProvider::new("p1", vec![Behavior::Status(503)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Status(503)]);
    let r = router_with_config_providers(
        &["m1", "m2"],
        vec![
            ("m1".into(), "p1".into(), "up1".into()),
            ("m2".into(), "p2".into(), "up2".into()),
        ],
        vec![
            ("p1".into(), p1 as Arc<dyn Provider>),
            ("p2".into(), p2 as Arc<dyn Provider>),
        ],
        default_test_retry(),
    );

    let Dispatched { meta, result } = r
        .complete_with_options(req("fast"), RouterOptions::new())
        .await;
    result.expect_err("all-fail");
    assert_eq!(meta.attempt_count, 2, "one attempt per chain entry");
    assert_eq!(meta.fallback_count, 1, "one hop to the second entry");
    assert_eq!(meta.served_provider.as_deref(), Some("p2"));
    assert_eq!(meta.served_model.as_deref(), Some("m2"));
    assert_eq!(meta.resolved_alias, "fast");
}

#[tokio::test]
async fn dispatched_stream_meta_on_all_failed_reflects_full_walk() {
    let p1 = MockProvider::new("p1", vec![Behavior::Status(503)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Status(503)]);
    let r = router_with_config_providers(
        &["m1", "m2"],
        vec![
            ("m1".into(), "p1".into(), "up1".into()),
            ("m2".into(), "p2".into(), "up2".into()),
        ],
        vec![
            ("p1".into(), p1 as Arc<dyn Provider>),
            ("p2".into(), p2 as Arc<dyn Provider>),
        ],
        default_test_retry(),
    );

    let DispatchedStream { meta, result } = r
        .stream_with_options(req("fast"), RouterOptions::new())
        .await;
    result.err().expect("all-fail");
    assert_eq!(meta.attempt_count, 2);
    assert_eq!(meta.fallback_count, 1);
    assert_eq!(meta.served_provider.as_deref(), Some("p2"));
}

#[tokio::test]
async fn dispatched_stream_meta_on_success_captures_winning_entry() {
    // First entry fails to open the stream; second serves. Meta must
    // carry the second (winning) entry's served_* fields, captured
    // synchronously at the Ok(stream) arm.
    let p1 = MockProvider::new("p1", vec![Behavior::Status(503)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let r = router_with_config_providers(
        &["m1", "m2"],
        vec![
            ("m1".into(), "p1".into(), "up1".into()),
            ("m2".into(), "p2".into(), "up2".into()),
        ],
        vec![
            ("p1".into(), p1 as Arc<dyn Provider>),
            ("p2".into(), p2 as Arc<dyn Provider>),
        ],
        default_test_retry(),
    );

    let DispatchedStream { meta, result } = r
        .stream_with_options(req("fast"), RouterOptions::new())
        .await;
    let mut s = result.expect("stream ok");
    assert!(
        s.next().await.is_some(),
        "winning stream must yield a chunk"
    );
    assert_eq!(meta.attempt_count, 2);
    assert_eq!(meta.fallback_count, 1);
    assert_eq!(meta.served_provider.as_deref(), Some("p2"));
    assert_eq!(meta.served_provider_kind.as_deref(), Some("openai-compat"));
    assert_eq!(meta.served_model.as_deref(), Some("m2"));
    assert_eq!(meta.served_upstream.as_deref(), Some("up2"));
}

/// A gate-blocked dispatch (circuit breaker fires before any upstream
/// contact) records the refused provider in `served_provider` via
/// `mark_target`, but never increments `attempt_count` because no
/// upstream was touched. Single-entry chain so the block is terminal.
#[tokio::test]
async fn gate_blocked_dispatch_has_zero_attempts_but_named_provider() {
    // Arrange: single-entry chain, breaker trips after one failure.
    let p1 = MockProvider::new("p1", vec![Behavior::Status(503), Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 1;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();
        rt.circuit_failures = Some(1);
        rt.circuit_cooldown_ms = Some(60_000);
        rt
    });
    let r = build_router_v6_full(
        aliases,
        vec![("m1".into(), "p1".into(), "m".into())],
        vec![("p1".into(), p1.clone() as Arc<dyn Provider>)],
        rp,
        runtime,
    );

    // Prime the breaker: the first request 503s and trips it open.
    let first = r
        .complete_with_options(req("fast"), RouterOptions::new())
        .await;
    assert!(first.result.is_err(), "first request fails on the 503");

    // Act: the second request is gate-blocked before any upstream call.
    let blocked = r
        .complete_with_options(req("fast"), RouterOptions::new())
        .await;

    // Assert: no upstream was touched (attempt_count stays 0), but the
    // refused provider is still named.
    assert!(
        blocked.result.is_err(),
        "gate block is terminal on a single-entry chain"
    );
    assert_eq!(blocked.meta.attempt_count, 0);
    assert_eq!(blocked.meta.served_provider.as_deref(), Some("p1"));
    // p1 was called exactly once (the priming 503); the gate-blocked
    // request never reached it.
    assert_eq!(p1.calls(), 1);
}

/// A SINGLE-TARGET dispatch records the token-estimate calibration
/// numerator. The window gate returns a one-entry chain untouched before it
/// ever computes an estimate, so harvesting the estimate there would silently
/// restrict the evidence to multi-target chains. This pins the real end-to-end
/// path: one alias, one model, one provider, and the estimate still arrives on
/// the meta the usage capture reads.
#[tokio::test]
async fn single_target_dispatch_records_the_calibration_estimate() {
    // Arrange: a one-entry chain -- nothing to fall back to.
    let p1 = MockProvider::new("p1", vec![Behavior::Ok]);
    let r = router_with_config_providers(
        &["m1"],
        vec![("m1".into(), "p1".into(), "up1".into())],
        vec![("p1".into(), p1 as Arc<dyn Provider>)],
        default_test_retry(),
    );

    // Act
    let Dispatched { meta, result } = r
        .complete_with_options(req("fast"), RouterOptions::new())
        .await;
    result.expect("ok");

    // Assert: one target served, and the estimate is a real positive count
    // for the dispatched payload rather than an absent or zero value.
    assert_eq!(meta.fallback_count, 0, "sanity: single-target chain");
    assert!(
        meta.calib_estimated_tokens.is_some_and(|est| est > 0),
        "a single-target dispatch must still record the estimate, got {:?}",
        meta.calib_estimated_tokens,
    );
}
