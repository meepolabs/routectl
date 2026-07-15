use super::*;

#[test]
fn unlimited_state_always_allows() {
    let policy = ProviderRuntimePolicy::default();
    let mut s = ProviderState::new(&policy);
    for _ in 0..100 {
        assert_eq!(s.try_dispatch(Instant::now()), GateDecision::Allow);
        s.record_success();
    }
}

#[test]
fn rpm_bucket_drains_and_refills() {
    let policy = ProviderRuntimePolicy {
        rpm_limit: Some(2),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();
    assert_eq!(s.try_dispatch(t0), GateDecision::Allow);
    s.record_success();
    assert_eq!(s.try_dispatch(t0), GateDecision::Allow);
    s.record_success();
    assert_eq!(s.try_dispatch(t0), GateDecision::RateLimited);
    let t1 = t0 + Duration::from_mins(1);
    assert_eq!(s.try_dispatch(t1), GateDecision::Allow);
}

#[test]
fn circuit_opens_after_threshold_and_skips_until_cooldown() {
    let policy = ProviderRuntimePolicy {
        circuit_failures: Some(2),
        circuit_cooldown_ms: Some(1_000),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();
    assert_eq!(s.try_dispatch(t0), GateDecision::Allow);
    s.record_failure(t0);
    assert_eq!(s.try_dispatch(t0), GateDecision::Allow);
    s.record_failure(t0);
    assert_eq!(s.try_dispatch(t0), GateDecision::CircuitOpen);
    let t_mid = t0 + Duration::from_millis(500);
    assert_eq!(s.try_dispatch(t_mid), GateDecision::CircuitOpen);
    // After cooldown a single probe is allowed.
    let t_after = t0 + Duration::from_millis(1_500);
    assert_eq!(s.try_dispatch(t_after), GateDecision::Allow);
}

#[test]
fn release_probe_slot_frees_slot_without_recording_outcome() {
    // A probe fast-fail releases the claimed half-open slot WITHOUT
    // counting a failure. The slot must clear, the failure counter
    // and trip timestamp must stay put, and the next dispatch in the
    // still-open window must be granted a fresh probe (proving the
    // breaker is not locked).
    let policy = ProviderRuntimePolicy {
        circuit_failures: Some(1),
        circuit_cooldown_ms: Some(500),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();
    // Trip the breaker (threshold = 1).
    assert_eq!(s.try_dispatch(t0), GateDecision::Allow);
    s.record_failure(t0);
    assert_eq!(s.try_dispatch(t0), GateDecision::CircuitOpen);
    // After cooldown, the first dispatch claims the half-open slot.
    let t_after = t0 + Duration::from_millis(600);
    assert_eq!(s.try_dispatch(t_after), GateDecision::Allow);
    assert!(s.half_open_probe_in_flight(), "probe slot must be claimed");
    let failures_before = s.consecutive_failures;
    let opened_before = s.circuit_opened_at;

    // Release WITHOUT recording success/failure.
    s.release_probe_slot();
    assert!(
        !s.half_open_probe_in_flight(),
        "release_probe_slot must clear the half-open slot",
    );
    assert_eq!(
        s.consecutive_failures, failures_before,
        "release_probe_slot must NOT change the failure counter",
    );
    assert_eq!(
        s.circuit_opened_at, opened_before,
        "release_probe_slot must NOT change the trip timestamp",
    );

    // The breaker is NOT locked: the next dispatch in the still-open
    // window is granted a fresh probe because the slot is free again.
    assert_eq!(s.try_dispatch(t_after), GateDecision::Allow);
    assert!(s.half_open_probe_in_flight());
}

#[test]
fn half_open_is_single_probe_under_concurrent_dispatches() {
    let policy = ProviderRuntimePolicy {
        circuit_failures: Some(2),
        circuit_cooldown_ms: Some(1_000),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();
    // Trip the breaker.
    s.try_dispatch(t0);
    s.record_failure(t0);
    s.try_dispatch(t0);
    s.record_failure(t0);
    assert_eq!(s.try_dispatch(t0), GateDecision::CircuitOpen);
    // Cooldown elapsed: first dispatch claims the half-open slot.
    let t_after = t0 + Duration::from_millis(1_500);
    assert_eq!(s.try_dispatch(t_after), GateDecision::Allow);
    // Second concurrent dispatch (probe NOT yet recorded) sees
    // CircuitOpen because someone already has the probe slot.
    assert_eq!(s.try_dispatch(t_after), GateDecision::CircuitOpen);
    // Probe fails -> breaker re-trips.
    s.record_failure(t_after);
    // Subsequent calls in the new cooldown window also see CircuitOpen.
    assert_eq!(s.try_dispatch(t_after), GateDecision::CircuitOpen);
}

#[test]
fn half_open_probe_success_closes_the_breaker() {
    let policy = ProviderRuntimePolicy {
        circuit_failures: Some(2),
        circuit_cooldown_ms: Some(500),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();
    s.try_dispatch(t0);
    s.record_failure(t0);
    s.try_dispatch(t0);
    s.record_failure(t0);
    let t_after = t0 + Duration::from_millis(600);
    assert_eq!(s.try_dispatch(t_after), GateDecision::Allow);
    s.record_success();
    // Closed -- subsequent dispatch is Allow without needing another cooldown.
    assert_eq!(s.try_dispatch(t_after), GateDecision::Allow);
}

#[test]
fn half_open_slot_released_when_rpm_refuses_the_probe() {
    // If the breaker is half-open AND the RPM bucket is empty,
    // we should NOT consume the half-open slot -- the next caller
    // (after RPM refills) should still get a probe.
    let policy = ProviderRuntimePolicy {
        rpm_limit: Some(1),
        circuit_failures: Some(1),
        circuit_cooldown_ms: Some(500),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();
    // Trip the breaker.
    s.try_dispatch(t0);
    s.record_failure(t0);
    // Consume the RPM token in the next cycle so the next
    // try_dispatch hits RPM-limited.
    let t_after = t0 + Duration::from_millis(700);
    // RPM is at 0/1 after the failed probe. Refill is gradual.
    // We ensure the bucket is empty by exhausting it explicitly:
    // (record_failure already returned the slot in failure path, but
    //  the bucket itself was decremented -- let's just check the
    //  half_open_slot is reclaimable when RPM refuses.)
    // Force RPM empty: drain whatever is left.
    while matches!(s.try_dispatch(t_after), GateDecision::Allow) {
        s.record_success();
    }
    // Now circuit is Closed (we just succeeded). Re-trip it.
    s.record_failure(t_after);
    // Wait cooldown; RPM still depleted in this instant.
    let t_probe = t_after + Duration::from_millis(700);
    // RPM may have refilled by t_probe; if so this test's pre-condition
    // is moot. The invariant we care about: any path that returns
    // RateLimited must NOT leave half_open_in_flight=true.
    // Force the scenario directly:
    s.rpm_tokens = 0.0;
    s.rpm_last_refill = t_probe;
    let decision = s.try_dispatch(t_probe);
    assert_eq!(decision, GateDecision::RateLimited);
    // The half-open slot is still free; bumping the clock past
    // a refill window lets the next dispatch claim it.
    let t_refill = t_probe + Duration::from_mins(1);
    assert_eq!(s.try_dispatch(t_refill), GateDecision::Allow);
}

#[test]
fn success_resets_failure_counter() {
    let policy = ProviderRuntimePolicy {
        circuit_failures: Some(3),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t = Instant::now();
    s.try_dispatch(t);
    s.record_failure(t);
    s.try_dispatch(t);
    s.record_failure(t);
    s.try_dispatch(t);
    s.record_success();
    s.try_dispatch(t);
    s.record_failure(t);
    s.try_dispatch(t);
    s.record_failure(t);
    assert_eq!(s.try_dispatch(t), GateDecision::Allow);
}

#[test]
fn force_open_parks_immediately_bypassing_threshold() {
    // Arrange: a high failure threshold so the counter-driven trip
    // would never fire on a single failure.
    let policy = ProviderRuntimePolicy {
        circuit_failures: Some(5),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();

    // Act: park immediately after ZERO failures.
    s.force_open(t0, Duration::from_secs(10));

    // Assert: open right away, single probe only after the custom cooldown.
    assert_eq!(s.try_dispatch(t0), GateDecision::CircuitOpen);
    let t_after = t0 + Duration::from_secs(11);
    assert_eq!(s.try_dispatch(t_after), GateDecision::Allow);
}

#[test]
fn force_open_cooldown_outlasts_default() {
    // Arrange: a tiny 1s default cooldown.
    let policy = ProviderRuntimePolicy {
        circuit_cooldown_ms: Some(1_000),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();

    // Act: park for 60s, far longer than the 1s default.
    s.force_open(t0, Duration::from_mins(1));

    // Assert: still open at t0+5s (default would already be elapsed),
    // probe only after the custom 60s window.
    assert_eq!(
        s.try_dispatch(t0 + Duration::from_secs(5)),
        GateDecision::CircuitOpen,
    );
    assert_eq!(
        s.try_dispatch(t0 + Duration::from_secs(61)),
        GateDecision::Allow,
    );
}

#[test]
fn force_open_releases_inflight_probe_slot() {
    // Arrange: trip the breaker normally, then advance past cooldown
    // and claim the half-open slot.
    let policy = ProviderRuntimePolicy {
        circuit_failures: Some(1),
        circuit_cooldown_ms: Some(500),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();
    s.try_dispatch(t0);
    s.record_failure(t0);
    let t_after = t0 + Duration::from_millis(600);
    assert_eq!(s.try_dispatch(t_after), GateDecision::Allow);
    assert!(s.half_open_probe_in_flight(), "probe slot must be claimed");

    // Act: force_open while a probe is in flight.
    s.force_open(t_after, Duration::from_secs(30));

    // Assert: the in-flight slot is released and the breaker is open
    // again for the new window.
    assert!(
        !s.half_open_probe_in_flight(),
        "force_open must release the in-flight probe slot",
    );
    assert_eq!(s.try_dispatch(t_after), GateDecision::CircuitOpen);
}

#[test]
fn force_open_then_probe_success_resets_to_default() {
    // Arrange: a 1s default cooldown, then a 60s custom park.
    let policy = ProviderRuntimePolicy {
        circuit_failures: Some(1),
        circuit_cooldown_ms: Some(1_000),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();
    s.force_open(t0, Duration::from_mins(1));

    // Act: the custom park elapses, the single probe succeeds.
    let t_probe = t0 + Duration::from_secs(61);
    assert_eq!(s.try_dispatch(t_probe), GateDecision::Allow);
    s.record_success();

    // Assert: a NORMAL threshold trip now uses the DEFAULT cooldown
    // (1s), not the stale 60s custom park.
    s.try_dispatch(t_probe);
    s.record_failure(t_probe);
    assert_eq!(s.try_dispatch(t_probe), GateDecision::CircuitOpen);
    // Still open just before the 1s default elapses.
    assert_eq!(
        s.try_dispatch(t_probe + Duration::from_millis(500)),
        GateDecision::CircuitOpen,
    );
    // Allowed once the 1s default cooldown elapses -- proving the
    // window is the default, not 60s.
    assert_eq!(
        s.try_dispatch(t_probe + Duration::from_millis(1_500)),
        GateDecision::Allow,
    );
}

#[test]
fn force_open_recovery_is_single_probe() {
    // Arrange: park immediately for a custom window.
    let policy = ProviderRuntimePolicy {
        circuit_failures: Some(5),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();
    s.force_open(t0, Duration::from_secs(10));

    // Act: the custom cooldown elapses; the first dispatch claims the
    // single probe, a concurrent second dispatch (probe not yet
    // recorded) is refused.
    let t_after = t0 + Duration::from_secs(11);

    // Assert: single-probe invariant holds under force_open.
    assert_eq!(s.try_dispatch(t_after), GateDecision::Allow);
    assert_eq!(s.try_dispatch(t_after), GateDecision::CircuitOpen);
}

#[test]
fn snapshot_unlimited_policy_is_dispatchable() {
    let policy = ProviderRuntimePolicy::default();
    let s = ProviderState::new(&policy);
    let snap = s.capacity_snapshot(Instant::now());
    assert_eq!(snap.rpm_available, None);
    assert_eq!(snap.circuit, CircuitPhase::Closed);
    assert!(snap.is_dispatchable());
}

#[test]
fn snapshot_reflects_drained_bucket() {
    let policy = ProviderRuntimePolicy {
        rpm_limit: Some(5),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();
    // Drain 3 tokens via real dispatch+success.
    for _ in 0..3 {
        assert_eq!(s.try_dispatch(t0), GateDecision::Allow);
        s.record_success();
    }
    let snap = s.capacity_snapshot(t0);
    // 5 - 3 = 2 tokens remaining (no refill at the same instant).
    let available = snap.rpm_available.expect("limited policy has Some");
    assert!(
        (available - 2.0).abs() < 1e-6,
        "expected ~2.0 tokens, got {available}",
    );
}

#[test]
fn snapshot_projects_refill_without_storing_it() {
    let policy = ProviderRuntimePolicy {
        rpm_limit: Some(4),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();
    // Drain the bucket fully.
    for _ in 0..4 {
        assert_eq!(s.try_dispatch(t0), GateDecision::Allow);
        s.record_success();
    }
    // Project a full window into the future WITHOUT any try_dispatch
    // in between -- the snapshot must compute the refilled level itself.
    let t_future = t0 + Duration::from_millis(RPM_WINDOW_MS);
    let snap = s.capacity_snapshot(t_future);
    let available = snap.rpm_available.expect("limited policy has Some");
    assert!(
        (available - 4.0).abs() < 1e-6,
        "expected refilled to ~4.0, got {available}",
    );
    // The projection was not stored: the live bucket is still drained
    // at t0 (rpm_last_refill unchanged), so a try_dispatch at t0 fails.
    assert_eq!(s.try_dispatch(t0), GateDecision::RateLimited);
}

#[test]
fn snapshot_does_not_consume_tokens() {
    let policy = ProviderRuntimePolicy {
        rpm_limit: Some(3),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();
    // Several snapshots at the same instant must read identically.
    let first = s.capacity_snapshot(t0);
    let second = s.capacity_snapshot(t0);
    let third = s.capacity_snapshot(t0);
    assert_eq!(first, second);
    assert_eq!(second, third);
    assert_eq!(first.rpm_available, Some(3.0));
    // The bucket is still full: three real dispatches succeed.
    for _ in 0..3 {
        assert_eq!(s.try_dispatch(t0), GateDecision::Allow);
        s.record_success();
    }
    assert_eq!(s.try_dispatch(t0), GateDecision::RateLimited);
}

#[test]
fn snapshot_half_open_ready_does_not_claim_probe_slot() {
    // THE anti-pattern regression guard: a non-mutating read of a
    // recovered breaker must NOT claim the half-open probe slot the
    // way a try_dispatch-based "read" would.
    let policy = ProviderRuntimePolicy {
        circuit_cooldown_ms: Some(500),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();
    // Park the breaker with a short cooldown.
    s.force_open(t0, Duration::from_millis(500));
    // Snapshot just after cooldown elapses, no probe in flight.
    let t_ready = t0 + Duration::from_millis(501);
    let snap = s.capacity_snapshot(t_ready);
    assert_eq!(snap.circuit, CircuitPhase::HalfOpenReady);
    assert!(snap.is_dispatchable());
    // The snapshot did NOT claim the slot: a SUBSEQUENT real dispatch
    // still gets the probe. Had the snapshot used try_dispatch, the
    // slot would already be claimed and this would be CircuitOpen.
    assert!(!s.half_open_probe_in_flight());
    assert_eq!(s.try_dispatch(t_ready), GateDecision::Allow);
}

#[test]
fn gate_status_mirrors_capacity_snapshot_and_adds_probe_flag() {
    // gate_status must report the SAME rpm_available and circuit phase as
    // capacity_snapshot for identical state, plus the probe-in-flight bool.
    let policy = ProviderRuntimePolicy {
        rpm_limit: Some(4),
        circuit_cooldown_ms: Some(500),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();

    // Closed, full bucket.
    let snap = s.capacity_snapshot(t0);
    let status = s.gate_status(t0);
    assert_eq!(status.rpm_available, snap.rpm_available);
    assert_eq!(status.circuit, snap.circuit);
    assert!(!status.half_open_probe_in_flight);

    // Park the breaker, then claim the half-open probe with a real
    // dispatch: the phase folds to Open AND the explicit bool is true.
    s.force_open(t0, Duration::from_millis(500));
    let t_ready = t0 + Duration::from_millis(501);
    assert_eq!(s.try_dispatch(t_ready), GateDecision::Allow);
    let snap = s.capacity_snapshot(t_ready);
    let status = s.gate_status(t_ready);
    assert_eq!(status.circuit, snap.circuit);
    assert_eq!(status.circuit, CircuitPhase::Open);
    assert!(status.half_open_probe_in_flight);
}

#[test]
fn gate_status_half_open_ready_does_not_claim_probe_slot() {
    // Parallels snapshot_half_open_ready_does_not_claim_probe_slot: a
    // non-mutating gate_status read of a recovered breaker must NOT claim
    // the half-open probe slot the way a try_dispatch-based "read" would.
    let policy = ProviderRuntimePolicy {
        circuit_cooldown_ms: Some(500),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();
    s.force_open(t0, Duration::from_millis(500));
    let t_ready = t0 + Duration::from_millis(501);
    let status = s.gate_status(t_ready);
    assert_eq!(status.circuit, CircuitPhase::HalfOpenReady);
    assert!(!status.half_open_probe_in_flight);
    // The read did NOT claim the slot: a subsequent real dispatch still
    // gets the probe.
    assert!(!s.half_open_probe_in_flight());
    assert_eq!(s.try_dispatch(t_ready), GateDecision::Allow);
}

#[test]
fn snapshot_open_during_cooldown_is_not_dispatchable() {
    let policy = ProviderRuntimePolicy {
        circuit_cooldown_ms: Some(1_000),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();
    s.force_open(t0, Duration::from_secs(1));
    // Before cooldown elapses.
    let snap = s.capacity_snapshot(t0 + Duration::from_millis(500));
    assert_eq!(snap.circuit, CircuitPhase::Open);
    assert!(!snap.is_dispatchable());
}

#[test]
fn snapshot_rpm_exhausted_with_closed_circuit_is_not_dispatchable() {
    let policy = ProviderRuntimePolicy {
        rpm_limit: Some(1),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();
    // Drain the single token; circuit stays Closed.
    assert_eq!(s.try_dispatch(t0), GateDecision::Allow);
    s.record_success();
    let snap = s.capacity_snapshot(t0);
    assert_eq!(snap.circuit, CircuitPhase::Closed);
    let available = snap.rpm_available.expect("limited policy has Some");
    assert!(available < 1.0, "expected < 1.0, got {available}");
    assert!(!snap.is_dispatchable());
}

#[test]
fn snapshot_half_open_ready_but_rpm_exhausted_is_not_dispatchable() {
    // The subtlest claim: a recovered breaker (HalfOpenReady) with an
    // empty RPM bucket must NOT be dispatchable, and the prediction must
    // match the gate -- a real try_dispatch returns RateLimited and
    // leaves the probe slot released (the RPM refusal frees it).
    let policy = ProviderRuntimePolicy {
        rpm_limit: Some(1),
        circuit_cooldown_ms: Some(500),
        ..Default::default()
    };
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();
    // Park the breaker, then drain the single token.
    s.force_open(t0, Duration::from_millis(500));
    s.rpm_tokens = 0.0;
    s.rpm_last_refill = t0;
    // Snapshot just after the cooldown elapses, no probe in flight.
    let t_ready = t0 + Duration::from_millis(501);
    let snap = s.capacity_snapshot(t_ready);
    assert_eq!(snap.circuit, CircuitPhase::HalfOpenReady);
    assert!(!snap.is_dispatchable());
    // The prediction matches the gate: try_dispatch refuses on RPM and
    // releases the half-open slot it briefly claimed.
    assert_eq!(s.try_dispatch(t_ready), GateDecision::RateLimited);
    assert!(!s.half_open_probe_in_flight());
}

#[test]
fn snapshot_at_exact_cooldown_instant_is_half_open_ready() {
    // Boundary: now - opened_at == active_cooldown. try_dispatch uses
    // `< active_cooldown` to mean "still open", so the exact instant is
    // treated as past-cooldown; the snapshot's `>=` complement must
    // agree and report HalfOpenReady.
    let policy = ProviderRuntimePolicy::default();
    let mut s = ProviderState::new(&policy);
    let t0 = Instant::now();
    let cooldown = Duration::from_millis(500);
    s.force_open(t0, cooldown);
    let snap = s.capacity_snapshot(t0 + cooldown);
    assert_eq!(snap.circuit, CircuitPhase::HalfOpenReady);
}
