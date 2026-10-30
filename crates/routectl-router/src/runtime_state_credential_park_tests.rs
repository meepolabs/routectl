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

fn parked_at(t0: Instant) -> Option<CredentialPark> {
    Some(CredentialPark {
        reason: EXPIRED,
        until: t0 + CREDENTIAL_PARK_BACKSTOP,
    })
}

fn parked_refusal(at: Instant) -> GateDecision {
    GateDecision::CredentialParked {
        reason: EXPIRED,
        until: at + CREDENTIAL_PARK_BACKSTOP,
    }
}

fn client(s: &mut ProviderState, now: Instant, park: Option<CredentialPark>) -> GateAdmission {
    s.admit_under_park(now, AdmissionKind::ClientDispatch, park)
}

#[test]
fn a_parked_credential_refuses_dispatch_with_its_reason_and_deadline() {
    // Arrange
    let mut s = state_with_breaker(None);
    let t0 = Instant::now();

    // Act
    let admission = client(&mut s, t0 + Duration::from_secs(1), parked_at(t0));

    // Assert
    assert_eq!(admission.decision, parked_refusal(t0));
    assert!(!admission.claimed_half_open);
}

#[test]
fn a_lapsed_park_admits_exactly_one_half_open_retest_per_slot() {
    // Arrange
    let mut s = state_with_breaker(None);
    let t0 = Instant::now();
    let lapsed = t0 + CREDENTIAL_PARK_BACKSTOP;

    // Act
    let first = client(&mut s, lapsed, parked_at(t0));
    let second = client(&mut s, lapsed, parked_at(t0));

    // Assert
    assert_eq!(first.decision, GateDecision::Allow);
    assert!(first.claimed_half_open, "the re-test owns the probe slot");
    assert_eq!(
        second.decision,
        parked_refusal(t0),
        "a concurrent caller is refused while the re-test is in flight",
    );
}

#[test]
fn a_background_probe_never_takes_a_lapsed_park_retest() {
    // Arrange
    let mut s = state_with_breaker(None);
    let t0 = Instant::now();
    let lapsed = t0 + CREDENTIAL_PARK_BACKSTOP;

    // Act
    let probe = s.admit_under_park(lapsed, AdmissionKind::BackgroundProbe, parked_at(t0));
    let client = client(&mut s, lapsed, parked_at(t0));

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
    let lapsed = t0 + Duration::from_millis(1);
    let park = Some(CredentialPark {
        reason: EXPIRED,
        until: lapsed,
    });

    // Act
    let refused = client(&mut s, lapsed, park);

    // Assert
    assert_eq!(refused.decision, GateDecision::RateLimited);
    assert!(
        !s.half_open_probe_in_flight(),
        "a re-test the RPM gate refused must not keep the probe slot",
    );
}

#[test]
fn a_retest_settles_by_reparking_or_clearing_the_account() {
    let rows: [(&str, bool, bool); 2] = [
        ("re-parked on a reason", true, false),
        ("cleared on success", false, true),
    ];
    for (name, reparked, dispatchable_after) in rows {
        // Arrange: a lapsed park whose re-test the router is settling.
        let mut s = state_with_breaker(None);
        let t0 = Instant::now();
        let lapsed = t0 + CREDENTIAL_PARK_BACKSTOP;
        assert!(client(&mut s, lapsed, parked_at(t0)).claimed_half_open);

        // Act
        s.release_probe_slot();
        let after = reparked.then(|| parked_at(lapsed)).flatten();

        // Assert
        let next = client(&mut s, lapsed + Duration::from_secs(1), after);
        assert_eq!(
            next.decision == GateDecision::Allow,
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
    let lapsed = t0 + CREDENTIAL_PARK_BACKSTOP;
    assert!(client(&mut s, lapsed, parked_at(t0)).claimed_half_open);

    // Act
    let opened = s.record_failure(lapsed, LastOutcome::Http5xx);

    // Assert
    assert!(opened, "a failed re-test re-trips like any half-open probe");
    assert_eq!(s.try_dispatch(lapsed), GateDecision::CircuitOpen);
}

#[test]
fn gate_status_reports_the_park_reason_and_time_left() {
    // Arrange
    let s = state_with_breaker(None);
    let t0 = Instant::now();
    let park = Some(CredentialPark {
        reason: UpstreamReason::CredentialInvalid,
        until: t0 + CREDENTIAL_PARK_BACKSTOP,
    });
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
        let status = s.gate_status_under(read_at, park);

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
    assert_eq!(
        s.gate_status_under(t0, None).credential_park,
        None,
        "unparked"
    );
}

#[test]
fn capacity_snapshot_reads_a_credential_blocked_seat_as_not_dispatchable() {
    let t0 = Instant::now();
    let lapsed = t0 + CREDENTIAL_PARK_BACKSTOP;
    let rows: [(&str, bool, Instant, bool, bool); 4] = [
        ("never parked", false, t0, false, true),
        ("active park", true, t0, false, false),
        ("lapsed park, re-test free", true, lapsed, false, true),
        ("lapsed park, re-test in flight", true, lapsed, true, false),
    ];
    for (name, parked, read_at, retest_in_flight, dispatchable) in rows {
        // Arrange
        let mut s = state_with_breaker(None);
        let park = parked.then(|| parked_at(t0)).flatten();
        if retest_in_flight {
            assert!(client(&mut s, lapsed, park).claimed_half_open);
        }

        // Act
        let snapshot = s.capacity_snapshot_under(read_at, park);

        // Assert
        assert_eq!(snapshot.is_dispatchable(), dispatchable, "{name}");
        assert_eq!(snapshot.credential_blocked, !dispatchable, "{name}");
    }
}

#[test]
fn a_rejection_parks_only_in_the_credential_generation_it_was_admitted_under() {
    let rows: [(&str, bool, bool, bool); 3] = [
        (
            "no reset between admission and rejection",
            false,
            false,
            true,
        ),
        ("a reset with nothing parked", false, true, false),
        ("a reset that lifted a park", true, true, false),
    ];
    for (name, parked_before, reset, parks) in rows {
        // Arrange
        let mut account = AccountCredential::default();
        let t0 = Instant::now();
        if parked_before {
            account.park_until(UpstreamReason::CredentialInvalid, t0);
        }
        let admitted_under = account.generation();
        if reset {
            assert_eq!(account.reset(), parked_before, "{name}");
        }

        // Act
        let parked = account.park_in_generation(t0, EXPIRED, admitted_under);

        // Assert
        assert_eq!(parked, parks, "{name}");
        let expected = parks.then(|| CredentialPark {
            reason: EXPIRED,
            until: t0 + CREDENTIAL_PARK_BACKSTOP,
        });
        assert_eq!(account.park(), expected, "{name}");
    }
}

#[test]
fn a_success_lifts_the_park_only_in_the_generation_it_was_admitted_under() {
    for (name, stale, lifted) in [("current", false, true), ("stale", true, false)] {
        // Arrange: a park set after an optional reset.
        let mut account = AccountCredential::default();
        let admitted_under = account.generation();
        if stale {
            account.reset();
        }
        let t0 = Instant::now();
        assert!(account.park_in_generation(t0, EXPIRED, account.generation()));

        // Act
        let eligible = account.clear_in_generation(admitted_under);

        // Assert
        assert_eq!(eligible, lifted, "{name}");
        assert_eq!(account.park().is_none(), lifted, "{name}");
    }
}
