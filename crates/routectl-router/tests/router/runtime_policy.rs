//! Tier 2: per-provider RPM limits, circuit breaker, disable-fallbacks.

use super::*;

#[tokio::test]
async fn rpm_limit_falls_through_to_next_provider() {
    let p1 = MockProvider::new("p1", vec![Behavior::Ok, Behavior::Ok, Behavior::Ok]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();
        rt.rpm_limit = Some(2);
        rt
    });
    let r = build_router_v6_full(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
        default_test_retry(),
        runtime,
    );

    // First two go to p1.
    assert_eq!(
        r.complete(req("fast"))
            .await
            .unwrap()
            .routectl_provider
            .as_deref(),
        Some("p1")
    );
    assert_eq!(
        r.complete(req("fast"))
            .await
            .unwrap()
            .routectl_provider
            .as_deref(),
        Some("p1")
    );
    // Third hits the bucket limit; falls through to p2.
    assert_eq!(
        r.complete(req("fast"))
            .await
            .unwrap()
            .routectl_provider
            .as_deref(),
        Some("p2")
    );
    assert_eq!(p1.calls(), 2);
    assert_eq!(p2.calls(), 1);
}

#[tokio::test]
async fn circuit_breaker_skips_provider_after_consecutive_failures() {
    let p1 = MockProvider::new(
        "p1",
        vec![Behavior::Status(503), Behavior::Status(503), Behavior::Ok],
    );
    let p2 = MockProvider::new("p2", vec![Behavior::Ok, Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 1;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();
        rt.circuit_failures = Some(2);
        rt.circuit_cooldown_ms = Some(30_000);
        rt
    });
    let r = build_router_v6_full(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
        rp,
        runtime,
    );

    // First two requests trip the breaker on p1, both end up on p2.
    let r1 = r.complete(req("fast")).await.unwrap();
    let r2 = r.complete(req("fast")).await.unwrap();
    assert_eq!(r1.routectl_provider.as_deref(), Some("p2"));
    assert_eq!(r2.routectl_provider.as_deref(), Some("p2"));
    // Third request: breaker is open, p1 is skipped silently and p2
    // serves it without p1 being called again.
    let r3 = r.complete(req("fast")).await.unwrap();
    assert_eq!(r3.routectl_provider.as_deref(), Some("p2"));
    assert_eq!(p1.calls(), 2);
    assert_eq!(p2.calls(), 3);
}

#[tokio::test]
async fn disable_fallbacks_propagates_first_error() {
    // First subtest: without disable_fallbacks, falls back to p2.
    let p1 = MockProvider::new("p1", vec![Behavior::Status(503)]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 1;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    let r = build_router_v6_with_retry(
        aliases.clone(),
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1.clone() as Arc<dyn Provider>),
            ("p2".into(), p2.clone() as Arc<dyn Provider>),
        ],
        rp.clone(),
    );
    let ok = r.complete(req("fast")).await.unwrap();
    assert_eq!(ok.routectl_provider.as_deref(), Some("p2"));

    // Second subtest: with disable_fallbacks, error from p1 propagates.
    let p1b = MockProvider::new("p1", vec![Behavior::Status(503)]);
    let p2b = MockProvider::new("p2", vec![Behavior::Ok]);
    let r = build_router_v6_with_retry(
        aliases,
        vec![
            ("m1".into(), "p1".into(), "m".into()),
            ("m2".into(), "p2".into(), "m".into()),
        ],
        vec![
            ("p1".into(), p1b as Arc<dyn Provider>),
            ("p2".into(), p2b.clone() as Arc<dyn Provider>),
        ],
        rp,
    );
    let mut opts = RouterOptions::new();
    opts.disable_fallbacks = true;
    let err = r
        .complete_with_options(req("fast"), opts)
        .await
        .result
        .unwrap_err();
    match err {
        Error::Upstream { status: 503, .. } => {}
        other => panic!("expected 503 from p1, got: {other:?}"),
    }
    assert_eq!(p2b.calls(), 0, "p2 should not have been touched");
}
