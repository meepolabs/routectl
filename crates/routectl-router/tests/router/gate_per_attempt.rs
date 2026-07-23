//! Tier-2 hardening: gates apply per-attempt, not per-routed-request.

use super::*;

/// Each retry against the same provider should consume one RPM
/// token. With `rpm_limit = 2` and `retry_on_5xx = 3`, a provider that
/// 503s every time exhausts its bucket on the second attempt and the
/// router falls through to the next chain entry instead of completing
/// all 3 retries against an over-budget provider.
#[tokio::test]
async fn retries_consume_rpm_tokens_and_fall_through_when_bucket_empty() {
    let p1 = MockProvider::new(
        "p1",
        vec![
            Behavior::Status(503),
            Behavior::Status(503),
            Behavior::Status(503),
        ],
    );
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 1;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    rp.retry_on_5xx = Some(5);
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
        rp,
        runtime,
    );

    let resp = r.complete(req("fast")).await.unwrap();
    assert_eq!(resp.routectl_provider.as_deref(), Some("p2"));
    // p1 saw exactly 2 calls before the RPM bucket emptied; the router
    // then fell through to p2.
    assert_eq!(
        p1.calls(),
        2,
        "RPM gate must apply per-attempt, not per-request"
    );
    assert_eq!(p2.calls(), 1);
}

/// Each failed attempt should increment the breaker, not the
/// whole routed request. With `circuit_failures = 2`, a provider that
/// 503s repeatedly should trip after the second attempt and the third
/// attempt should hit a CircuitOpen gate (router falls through).
#[tokio::test]
async fn retries_count_toward_circuit_breaker_threshold() {
    let p1 = MockProvider::new(
        "p1",
        vec![
            Behavior::Status(503),
            Behavior::Status(503),
            Behavior::Status(503),
        ],
    );
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let mut rp = RetryPolicy::default();
    rp.max_attempts = 1;
    rp.initial_backoff_ms = 1;
    rp.backoff_multiplier = 1.0;
    rp.retry_on_5xx = Some(5);
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();
        rt.circuit_failures = Some(2);
        rt.circuit_cooldown_ms = Some(60_000);
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

    let resp = r.complete(req("fast")).await.unwrap();
    assert_eq!(resp.routectl_provider.as_deref(), Some("p2"));
    // p1 saw exactly 2 calls; the third would have been gate-blocked
    // because each retry increments the breaker counter.
    assert_eq!(p1.calls(), 2, "breaker must count each retry as a failure");
    assert_eq!(p2.calls(), 1);
}

/// T fix: client-side errors (400, 401, 404, ...) must NOT charge the
/// circuit breaker. They say nothing about provider health -- they are
/// the caller's mistake (malformed request, wrong auth, unknown model).
/// Repeatedly sending one should propagate the error each time, never
/// quarantine an otherwise-healthy provider.
#[tokio::test]
async fn client_errors_do_not_charge_the_circuit_breaker() {
    // 5 consecutive 400s, but the breaker is configured to trip after 2.
    let p1 = MockProvider::new(
        "p1",
        vec![
            Behavior::Status(400),
            Behavior::Status(400),
            Behavior::Status(400),
            Behavior::Status(400),
            Behavior::Status(400),
        ],
    );
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1"]);
    aliases.insert(k, v);
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();
        rt.circuit_failures = Some(2);
        rt.circuit_cooldown_ms = Some(60_000);
        rt
    });
    let r = build_router_v6_full(
        aliases,
        vec![("m1".into(), "p1".into(), "m".into())],
        vec![("p1".into(), p1.clone() as Arc<dyn Provider>)],
        default_test_retry(),
        runtime,
    );

    // Five sequential 400s. If client errors charged the breaker, the
    // third call would be gate-blocked and surface a status-0
    // CircuitOpen error instead of the upstream's 400. Assert that
    // every call reaches the provider AND that the upstream 400 is the
    // error every caller sees.
    for i in 0..5 {
        let err = r
            .complete(req("fast"))
            .await
            .expect_err(&format!("call {i} should propagate 400"));
        assert!(
            matches!(err, Error::Upstream { status: 400, .. }),
            "call {i}: expected upstream 400, got {err:?}"
        );
    }
    assert_eq!(
        p1.calls(),
        5,
        "every client-error attempt must reach the provider; breaker must NOT quarantine on 400s"
    );
}

/// A stream that emits one chunk and then errors mid-stream still
/// CHARGES the breaker: under the first-chunk-close contract the
/// delivered first chunk closes the breaker, but the mid-stream error
/// is then recorded and re-trips it per the configured threshold. With
/// `circuit_failures = 1` a single first-chunk-then-error stream trips
/// p1's breaker, quarantining it so the next request falls through to
/// p2. (Higher thresholds tolerate more first-chunk-then-error flaps
/// before tripping -- the documented fast-flap tradeoff of closing on
/// the first chunk.)
#[tokio::test]
async fn stream_mid_failure_charges_the_breaker() {
    use futures::StreamExt;
    // Provider whose stream emits one chunk then errors mid-stream on
    // every call. circuit_failures = 1 -> a single mid-stream error
    // (after the first-chunk close) re-trips the breaker.
    let p1 = MockProvider::new(
        "p1",
        vec![
            Behavior::StreamMidErrors,
            Behavior::StreamMidErrors,
            Behavior::StreamMidErrors,
        ],
    );
    let p2 = MockProvider::new("p2", vec![Behavior::Ok, Behavior::Ok, Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();
        rt.circuit_failures = Some(1);
        rt.circuit_cooldown_ms = Some(60_000);
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

    // First request: starts streaming from p1, gets one chunk (closes
    // the breaker), then errors mid-stream. The mid-stream error is
    // charged and re-trips the breaker (threshold 1). Drain the stream
    // so the wrapper records the failure.
    let mut s = r.stream(req("fast")).await.expect("stream open");
    let mut count = 0;
    while s.next().await.is_some() {
        count += 1;
    }
    drop(s);
    assert!(count >= 2, "expected at least one chunk + one error");

    // Second request: p1's circuit is now open. The router should
    // gate-block p1 and fall through to p2 without ever calling p1.
    let calls_before = p1.calls();
    let mut s = r.stream(req("fast")).await.expect("stream open");
    let mut got_chunk = false;
    while let Some(item) = s.next().await {
        if item.is_ok() {
            got_chunk = true;
        }
    }
    assert!(got_chunk, "p2 should answer once p1's breaker is open");
    assert_eq!(
        p1.calls(),
        calls_before,
        "p1 must be skipped while breaker is open"
    );
}

/// On a HEALTHY (closed) breaker, mid-stream failures must
/// accumulate toward `circuit_failures` across multiple streams. The
/// first chunk of a healthy-state stream must NOT reset the failure
/// counter -- only a half-open probe's first chunk releases the slot and
/// closes the breaker. With `circuit_failures = 3`, three consecutive
/// first-chunk-then-mid-error streams must TRIP p1's breaker, so the
/// fourth request is gate-blocked and falls through to p2.
///
/// RED before the gating fix: the call-site `record_success` fired
/// UNCONDITIONALLY on every first chunk, zeroing `consecutive_failures`
/// before each mid-stream `record_failure` could accumulate. The counter
/// never reached 3, the breaker never tripped, and p1 kept being dialed.
#[tokio::test]
async fn healthy_stream_mid_failures_accumulate_to_trip_the_breaker() {
    use futures::StreamExt;
    // p1 errors mid-stream (after one chunk) on every call. p2 answers
    // cleanly. circuit_failures = 3 -> three mid-stream errors trip p1.
    let p1 = MockProvider::new(
        "p1",
        vec![
            Behavior::StreamMidErrors,
            Behavior::StreamMidErrors,
            Behavior::StreamMidErrors,
            Behavior::StreamMidErrors,
        ],
    );
    let p2 = MockProvider::new("p2", vec![Behavior::Ok, Behavior::Ok, Behavior::Ok]);
    let mut aliases = BTreeMap::new();
    let (k, v) = chain_alias("fast", &["m1", "m2"]);
    aliases.insert(k, v);
    let mut runtime = BTreeMap::new();
    runtime.insert("p1".into(), {
        let mut rt = ProviderRuntimePolicy::default();
        rt.circuit_failures = Some(3);
        rt.circuit_cooldown_ms = Some(60_000);
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

    // Drive three first-chunk-then-mid-error streams from p1. Drain each
    // so the wrap records the mid-stream failure. The breaker starts
    // CLOSED, so these are healthy-state streams: the first chunk must
    // NOT reset the counter; each mid-stream error accumulates 1 -> 2 ->
    // 3 and the third trips the breaker.
    for i in 0..3 {
        let mut s = r.stream(req("fast")).await.expect("stream open");
        while s.next().await.is_some() {}
        drop(s);
        assert!(
            p1.calls() > i,
            "p1 must be dialed for healthy-state stream {i}"
        );
    }
    let p1_after_trip = p1.calls();

    // Fourth request: p1's breaker must now be OPEN. The router
    // gate-blocks p1 and falls through to p2 without dialing p1 again.
    let mut s = r.stream(req("fast")).await.expect("stream open");
    let mut got_chunk = false;
    while let Some(item) = s.next().await {
        if item.is_ok() {
            got_chunk = true;
        }
    }
    assert!(got_chunk, "p2 answers once p1's breaker has tripped");
    assert_eq!(
        p1.calls(),
        p1_after_trip,
        "p1 must be quarantined after 3 accumulated mid-stream failures; \
         the first chunk of a healthy-state stream must not reset the counter",
    );
}

/// Under concurrent dispatches after cooldown, only ONE caller
/// should hit the upstream as the half-open probe; the other should
/// see a CircuitOpen gate and fall through.
// `start_paused` requires the `current_thread` runtime, but this test
// is fundamentally about real parallelism between two spawned tasks
// racing on the half-open slot, so we keep the multi-thread runtime
// and use generous wall-clock margins (CI-safe) instead.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn half_open_probe_is_single_under_concurrent_load() {
    use std::sync::Arc as StdArc;
    // Trip p1's breaker by feeding 2 failures inline.
    let p1 = MockProvider::new(
        "p1",
        vec![
            Behavior::Status(503),
            Behavior::Status(503),
            // After the breaker trips: deliberately make the probe
            // slow so concurrent callers race the half-open slot.
            Behavior::OkSlow,
            Behavior::OkSlow,
        ],
    );
    let p2 = MockProvider::new("p2", vec![Behavior::Ok, Behavior::Ok, Behavior::Ok]);
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
        // 250ms cooldown -- generous so the wait below is safely
        // past it even on a contended runner.
        rt.circuit_cooldown_ms = Some(250);
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
    let r = StdArc::new(r);

    // Trip the breaker.
    r.complete(req("fast")).await.unwrap();
    r.complete(req("fast")).await.unwrap();
    // Two p1 calls already done; breaker now open.
    let p1_after_trip = p1.calls();
    assert_eq!(p1_after_trip, 2);

    // Wait for cooldown (250ms cooldown, sleep 350ms).
    tokio::time::sleep(std::time::Duration::from_millis(350)).await;

    // Fire two concurrent requests. Exactly one should reach p1 as
    // the half-open probe; the other should see CircuitOpen and fall
    // through to p2.
    let r1 = r.clone();
    let r2 = r.clone();
    let (a, b) = tokio::join!(
        tokio::spawn(async move { r1.complete(req("fast")).await }),
        tokio::spawn(async move { r2.complete(req("fast")).await }),
    );
    let a = a.unwrap().unwrap();
    let b = b.unwrap().unwrap();

    let providers: Vec<_> = [a, b]
        .iter()
        .map(|r| r.routectl_provider.clone().unwrap_or_default())
        .collect();
    // Exactly one of the two requests went to p1 (the probe); the
    // other was deflected to p2 by the half-open guard.
    let p1_count = providers.iter().filter(|p| p.as_str() == "p1").count();
    let p2_count = providers.iter().filter(|p| p.as_str() == "p2").count();
    assert_eq!(
        p1_count, 1,
        "exactly one half-open probe expected: {providers:?}"
    );
    assert_eq!(
        p2_count, 1,
        "the other concurrent request must fall through: {providers:?}"
    );
    // p1 saw exactly 1 additional call (the probe) on top of the trip calls.
    assert_eq!(p1.calls(), p1_after_trip + 1);
}

// Paused-time would be ideal here but the breaker tracks cooldowns
// against `std::time::Instant`, which is not affected by Tokio's
// paused-time clock. Use generous wall-clock margins instead.
#[tokio::test]
async fn dropped_stream_releases_half_open_probe_and_reopens_breaker() {
    let p1 = MockProvider::new(
        "p1",
        vec![Behavior::Status(503), Behavior::Ok, Behavior::Ok],
    );
    let p2 = MockProvider::new("p2", vec![Behavior::Ok, Behavior::Ok, Behavior::Ok]);
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
        rt.circuit_failures = Some(1);
        // 200ms cooldown -- the sleeps below use 350ms margin so the
        // assertion fires even on a contended runner.
        rt.circuit_cooldown_ms = Some(200);
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

    let first = r.complete(req("fast")).await.unwrap();
    assert_eq!(first.routectl_provider.as_deref(), Some("p2"));
    assert_eq!(p1.calls(), 1);

    tokio::time::sleep(std::time::Duration::from_millis(350)).await;

    let stream = r.stream(req("fast")).await.unwrap();
    drop(stream);
    assert_eq!(p1.calls(), 2, "half-open probe should reach p1 once");

    tokio::time::sleep(std::time::Duration::from_millis(350)).await;

    let recovered = r.complete(req("fast")).await.unwrap();
    assert_eq!(recovered.routectl_provider.as_deref(), Some("p1"));
    assert_eq!(
        p1.calls(),
        3,
        "dropped stream must not leak the half-open slot"
    );
}

#[tokio::test]
async fn dropped_steady_state_stream_does_not_trip_breaker() {
    let p1 = MockProvider::new("p1", vec![Behavior::Ok, Behavior::Ok]);
    let p2 = MockProvider::new("p2", vec![Behavior::Ok]);
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
        rt.circuit_failures = Some(1);
        rt.circuit_cooldown_ms = Some(50);
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

    let mut stream = r.stream(req("fast")).await.unwrap();
    let first = stream.next().await.transpose().unwrap();
    assert!(first.is_some(), "expected first chunk before cancel");
    drop(stream);

    let recovered = r.complete(req("fast")).await.unwrap();
    assert_eq!(recovered.routectl_provider.as_deref(), Some("p1"));
    assert_eq!(
        p1.calls(),
        2,
        "client cancel after first chunk must not open the breaker"
    );
    assert_eq!(p2.calls(), 0);
}
