use super::*;
use routectl_core::UpstreamReason;

const EXPIRED: UpstreamReason = UpstreamReason::CredentialExpired;

fn state_with_breaker(circuit_failures: Option<u32>) -> ProviderState {
    ProviderState::new(&ProviderRuntimePolicy {
        circuit_failures,
        circuit_cooldown_ms: Some(1_000),
        ..Default::default()
    })
}

fn parked_refusal(at: Instant) -> GateDecision {
    GateDecision::CredentialParked {
        reason: EXPIRED,
        until: at + CREDENTIAL_PARK_BACKSTOP,
    }
}

#[test]
fn a_parked_credential_refuses_dispatch_with_its_reason_and_deadline() {
    // Arrange
    let mut s = state_with_breaker(None);
    let t0 = Instant::now();
    s.park_credential(t0, EXPIRED);

    // Act
    let decision = s.try_dispatch(t0 + Duration::from_secs(1));

    // Assert
    assert_eq!(decision, parked_refusal(t0));
}

#[test]
fn breaker_paths_leave_an_existing_credential_park_intact() {
    type BreakerCall = fn(&mut ProviderState, Instant);
    let rows: [(&str, BreakerCall); 3] = [
        ("record_failure", |s, now| {
            s.record_failure(now, LastOutcome::Http5xx);
        }),
        ("record_success", |s, now| s.record_success(now)),
        ("force_open", |s, now| s.force_open(now, Duration::ZERO)),
    ];
    for (name, breaker_call) in rows {
        // Arrange
        let mut s = state_with_breaker(Some(1));
        let t0 = Instant::now();
        s.park_credential(t0, EXPIRED);

        // Act
        breaker_call(&mut s, t0);

        // Assert
        assert_eq!(
            s.try_dispatch(t0 + Duration::from_secs(5)),
            parked_refusal(t0),
            "{name} must not lift the credential park",
        );
    }
}

#[test]
fn a_lapsed_park_admits_exactly_one_half_open_retest() {
    // Arrange
    let mut s = state_with_breaker(None);
    let t0 = Instant::now();
    s.park_credential(t0, EXPIRED);
    let lapsed = t0 + CREDENTIAL_PARK_BACKSTOP;

    // Act
    let first = s.try_dispatch_admitting(lapsed);
    let second = s.try_dispatch(lapsed);

    // Assert
    assert_eq!(first.decision, GateDecision::Allow);
    assert!(first.claimed_half_open, "the re-test owns the probe slot");
    assert_eq!(
        second,
        parked_refusal(t0),
        "a concurrent caller is refused while the re-test is in flight",
    );
}

#[test]
fn a_background_probe_never_takes_a_lapsed_park_retest() {
    // Arrange
    let mut s = state_with_breaker(None);
    let t0 = Instant::now();
    s.park_credential(t0, EXPIRED);
    let lapsed = t0 + CREDENTIAL_PARK_BACKSTOP;

    // Act
    let probe = s.try_dispatch_probe(lapsed);
    let client = s.try_dispatch_admitting(lapsed);

    // Assert
    assert_eq!(probe.decision, parked_refusal(t0));
    assert!(!probe.claimed_half_open);
    assert!(
        client.claimed_half_open,
        "the declined probe must leave the re-test for a client request",
    );
}

#[test]
fn a_retest_refused_by_rpm_hands_the_slot_back() {
    // Arrange: one RPM token, spent before the park lapses.
    let mut s = ProviderState::new(&ProviderRuntimePolicy {
        rpm_limit: Some(1),
        ..Default::default()
    });
    let t0 = Instant::now();
    assert_eq!(s.try_dispatch(t0), GateDecision::Allow);
    s.record_success(t0);
    s.park_credential(t0, EXPIRED);
    let lapsed = t0 + Duration::from_millis(1);
    s.park_credential_until(EXPIRED, lapsed);

    // Act
    let refused = s.try_dispatch(lapsed);

    // Assert
    assert_eq!(refused, GateDecision::RateLimited);
    assert!(
        !s.half_open_probe_in_flight(),
        "a re-test the RPM gate refused must not keep the probe slot",
    );
}

#[test]
fn a_retest_settles_by_reparking_or_clearing() {
    type Settle = fn(&mut ProviderState, Instant);
    let rows: [(&str, Settle, bool); 2] = [
        (
            "re-parked on a reason",
            |s, now| s.park_credential(now, EXPIRED),
            false,
        ),
        ("cleared on success", |s, _| s.clear_credential_park(), true),
    ];
    for (name, settle, dispatchable_after) in rows {
        // Arrange: a lapsed park whose re-test the router is settling.
        let mut s = state_with_breaker(None);
        let t0 = Instant::now();
        s.park_credential(t0, EXPIRED);
        let lapsed = t0 + CREDENTIAL_PARK_BACKSTOP;
        assert!(s.try_dispatch_admitting(lapsed).claimed_half_open);

        // Act
        settle(&mut s, lapsed);
        s.release_probe_slot();

        // Assert
        let next = s.try_dispatch(lapsed + Duration::from_secs(1));
        assert_eq!(
            next == GateDecision::Allow,
            dispatchable_after,
            "{name}: got {next:?}",
        );
    }
}

#[test]
fn a_failed_retest_without_a_reason_trips_a_configured_breaker() {
    // Arrange
    let mut s = state_with_breaker(Some(5));
    let t0 = Instant::now();
    s.park_credential(t0, EXPIRED);
    let lapsed = t0 + CREDENTIAL_PARK_BACKSTOP;
    assert!(s.try_dispatch_admitting(lapsed).claimed_half_open);

    // Act
    let opened = s.record_failure(lapsed, LastOutcome::Http5xx);

    // Assert
    assert!(opened, "a failed re-test re-trips like any half-open probe");
    assert_eq!(s.try_dispatch(lapsed), GateDecision::CircuitOpen);
}

#[test]
fn gate_status_reports_the_park_reason_and_time_left() {
    // Arrange
    let mut s = state_with_breaker(None);
    let t0 = Instant::now();
    s.park_credential(t0, UpstreamReason::CredentialInvalid);
    let rows = [
        (
            "active",
            t0 + Duration::from_mins(10),
            Duration::from_mins(50),
        ),
        ("lapsed", t0 + CREDENTIAL_PARK_BACKSTOP * 2, Duration::ZERO),
    ];
    for (name, read_at, remaining) in rows {
        // Act
        let status = s.gate_status(read_at);

        // Assert
        assert_eq!(
            status.credential_park,
            Some(CredentialParkStatus {
                reason: UpstreamReason::CredentialInvalid,
                remaining,
            }),
            "{name}",
        );
    }
    s.clear_credential_park();
    assert_eq!(s.gate_status(t0).credential_park, None, "cleared");
}

#[test]
fn capacity_snapshot_reads_a_credential_blocked_seat_as_not_dispatchable() {
    let t0 = Instant::now();
    let lapsed = t0 + CREDENTIAL_PARK_BACKSTOP;
    let rows: [(&str, bool, Instant, bool); 4] = [
        ("never parked", false, t0, true),
        ("active park", true, t0, false),
        ("lapsed park, re-test free", true, lapsed, true),
        ("lapsed park, re-test in flight", true, lapsed, false),
    ];
    for (i, (name, parked, read_at, dispatchable)) in rows.into_iter().enumerate() {
        // Arrange
        let mut s = state_with_breaker(None);
        if parked {
            s.park_credential(t0, EXPIRED);
        }
        if i == 3 {
            assert!(s.try_dispatch_admitting(lapsed).claimed_half_open);
        }

        // Act
        let snapshot = s.capacity_snapshot(read_at);

        // Assert
        assert_eq!(snapshot.is_dispatchable(), dispatchable, "{name}");
    }
}
