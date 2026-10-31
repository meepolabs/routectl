use super::*;
use routectl_core::UpstreamReason;

const REVOKED: UnusableCause = UnusableCause::Revoked;
const EXPIRED: UnusableCause = UnusableCause::Expired;

fn state(circuit_failures: Option<u32>, rpm_limit: Option<u32>) -> ProviderState {
    ProviderState::new(&ProviderRuntimePolicy {
        circuit_failures,
        circuit_cooldown_ms: Some(1_000),
        rpm_limit,
        ..Default::default()
    })
}

fn live_park(t0: Instant) -> Option<CredentialPark> {
    Some(CredentialPark {
        reason: UpstreamReason::CredentialInvalid,
        until: t0 + CREDENTIAL_PARK_BACKSTOP,
    })
}

const fn unusable_at(cause: UnusableCause, since: Instant) -> Option<Unusable> {
    Some(Unusable { cause, since })
}

#[test]
fn an_unconditional_mark_sets_the_cause_and_since() {
    // Arrange
    let mut account = AccountCredential::default();
    let t0 = Instant::now();

    // Act
    let outcome = account.mark_unusable(REVOKED, t0, None);

    // Assert
    assert_eq!(outcome, MarkOutcome::Transitioned);
    assert_eq!(account.unusable(), unusable_at(REVOKED, t0));
}

#[test]
fn a_re_mark_keeps_since_only_for_the_same_cause() {
    let rows = [
        ("same cause", REVOKED, false, MarkOutcome::Unchanged),
        ("other cause", EXPIRED, true, MarkOutcome::Transitioned),
    ];
    for (name, second_cause, since_moves, expected_outcome) in rows {
        // Arrange
        let mut account = AccountCredential::default();
        let t0 = Instant::now();
        let later = t0 + Duration::from_mins(5);
        assert_eq!(
            account.mark_unusable(REVOKED, t0, None),
            MarkOutcome::Transitioned,
            "{name}: premise"
        );

        // Act
        let outcome = account.mark_unusable(second_cause, later, None);

        // Assert
        assert_eq!(outcome, expected_outcome, "{name}");
        let since = if since_moves { later } else { t0 };
        assert_eq!(
            account.unusable(),
            unusable_at(second_cause, since),
            "{name}"
        );
    }
}

#[test]
fn a_generation_gated_mark_applies_only_in_the_current_generation() {
    let rows = [
        ("current generation", false, MarkOutcome::Transitioned),
        ("stale generation", true, MarkOutcome::Stale),
    ];
    for (name, reset_between, expected_outcome) in rows {
        // Arrange
        let mut account = AccountCredential::default();
        let observed_under = account.generation();
        if reset_between {
            account.reset();
        }
        let t0 = Instant::now();

        // Act
        let outcome = account.mark_unusable(REVOKED, t0, Some(observed_under));

        // Assert
        assert_eq!(outcome, expected_outcome, "{name}");
        let applies = outcome.applied();
        let expected = applies.then_some(Unusable {
            cause: REVOKED,
            since: t0,
        });
        assert_eq!(account.unusable(), expected, "{name}");
    }
}

#[test]
fn reset_clears_the_mark_and_starts_a_new_generation() {
    // (case, park set, mark set, expected outcome)
    let rows = [
        (
            "unusable only",
            false,
            Some(REVOKED),
            ResetOutcome {
                park_lifted: false,
                unusable_cleared: Some(REVOKED),
            },
        ),
        (
            "parked and unusable",
            true,
            Some(EXPIRED),
            ResetOutcome {
                park_lifted: true,
                unusable_cleared: Some(EXPIRED),
            },
        ),
        (
            "parked only",
            true,
            None,
            ResetOutcome {
                park_lifted: true,
                unusable_cleared: None,
            },
        ),
        ("neither", false, None, ResetOutcome::default()),
    ];
    for (name, parked, mark, expected) in rows {
        // Arrange
        let mut account = AccountCredential::default();
        let t0 = Instant::now();
        if parked {
            assert!(account.park_in_generation(t0, UpstreamReason::CredentialInvalid, 0));
        }
        if let Some(cause) = mark {
            assert!(account.mark_unusable(cause, t0, None).applied(), "{name}");
        }
        let before = account.generation();

        // Act
        let outcome = account.reset();

        // Assert
        assert_eq!(outcome, expected, "{name}");
        assert_eq!(account.unusable(), None, "{name}");
        assert_eq!(account.park(), None, "{name}");
        assert_eq!(account.generation(), before.wrapping_add(1), "{name}");
    }
}

#[test]
fn a_park_lift_leaves_the_unusable_mark_in_place() {
    // Arrange: a success settlement lifts a park, never an unusable mark.
    let mut account = AccountCredential::default();
    let t0 = Instant::now();
    assert!(account.park_in_generation(t0, UpstreamReason::CredentialInvalid, 0));
    assert!(account.mark_unusable(REVOKED, t0, None).applied());

    // Act
    assert!(account.clear_in_generation(account.generation()));

    // Assert
    assert_eq!(account.park(), None);
    assert_eq!(account.unusable(), unusable_at(REVOKED, t0));
}

#[test]
fn a_refresh_settle_advances_the_generation_and_clears_only_an_expired_mark() {
    // (case, mark before, reset before the settle, expected mark after,
    //  expected cleared_expired, or None for a stale settle)
    type Row = (
        &'static str,
        Option<UnusableCause>,
        bool,
        Option<UnusableCause>,
        Option<bool>,
    );
    let rows: [Row; 6] = [
        ("no mark", None, false, None, Some(false)),
        ("expired", Some(EXPIRED), false, None, Some(true)),
        (
            "revoked stays",
            Some(REVOKED),
            false,
            Some(REVOKED),
            Some(false),
        ),
        (
            "not logged in stays",
            Some(UnusableCause::NotLoggedIn),
            false,
            Some(UnusableCause::NotLoggedIn),
            Some(false),
        ),
        (
            "unreadable stays",
            Some(UnusableCause::Unreadable),
            false,
            Some(UnusableCause::Unreadable),
            Some(false),
        ),
        ("stale generation", Some(EXPIRED), true, Some(EXPIRED), None),
    ];
    for (name, mark, reset_between, mark_after, cleared) in rows {
        // Arrange
        let mut account = AccountCredential::default();
        let observed_under = account.generation();
        if reset_between {
            account.reset();
        }
        let t0 = Instant::now();
        if let Some(cause) = mark {
            assert!(account.mark_unusable(cause, t0, None).applied(), "{name}");
        }
        let before = account.generation();

        // Act
        let settlement = account.refreshed_in_generation(observed_under);

        // Assert
        let expected = cleared.map_or(RefreshSettlement::Stale, |cleared_expired| {
            RefreshSettlement::Settled {
                cleared_expired,
                generation: before.wrapping_add(1),
            }
        });
        assert_eq!(settlement, expected, "{name}");
        let generation_after = if cleared.is_some() {
            before.wrapping_add(1)
        } else {
            before
        };
        assert_eq!(account.generation(), generation_after, "{name}");
        assert_eq!(account.unusable().map(|m| m.cause), mark_after, "{name}");
    }
}

#[test]
fn an_outcome_observed_before_a_refresh_settle_is_stale_after_it() {
    for cause in [REVOKED, EXPIRED, UnusableCause::NotLoggedIn] {
        // Arrange: a delayed outcome captured the generation, then a
        // successful refresh of the same credential settled first.
        let mut account = AccountCredential::default();
        let observed_under = account.generation();
        assert!(matches!(
            account.refreshed_in_generation(observed_under),
            RefreshSettlement::Settled { .. }
        ));

        // Act
        let outcome = account.mark_unusable(cause, Instant::now(), Some(observed_under));

        // Assert
        assert_eq!(outcome, MarkOutcome::Stale, "{cause:?}");
        assert_eq!(account.unusable(), None, "{cause:?}");
    }
}

#[test]
fn the_snapshot_reason_names_the_highest_priority_refusal() {
    let t0 = Instant::now();
    // (case, unusable, park, open breaker, probe in flight, drain rpm,
    //  expected reason)
    type Row = (
        &'static str,
        bool,
        bool,
        bool,
        bool,
        bool,
        Option<EligibilityReason>,
    );
    let rows: [Row; 7] = [
        (
            "unusable + parked + open",
            true,
            true,
            true,
            false,
            false,
            Some(EligibilityReason::CredentialUnusable),
        ),
        (
            "unusable alone",
            true,
            false,
            false,
            false,
            false,
            Some(EligibilityReason::CredentialUnusable),
        ),
        (
            "parked + open",
            false,
            true,
            true,
            false,
            false,
            Some(EligibilityReason::AccountParked),
        ),
        (
            "open",
            false,
            false,
            true,
            false,
            false,
            Some(EligibilityReason::BreakerOpen),
        ),
        (
            "half-open probe in flight",
            false,
            false,
            false,
            true,
            false,
            Some(EligibilityReason::BreakerOpen),
        ),
        (
            "rpm 0",
            false,
            false,
            false,
            false,
            true,
            Some(EligibilityReason::RateLimited),
        ),
        ("healthy", false, false, false, false, false, None),
    ];
    for (name, unusable, parked, open, probe_in_flight, drain, expected) in rows {
        // Arrange
        let mut s = state(Some(1), Some(1));
        if open {
            s.force_open(t0, Duration::from_hours(1));
        }
        if probe_in_flight {
            s.force_open(t0, Duration::ZERO);
            assert!(s.try_dispatch_admitting(t0).claimed_half_open, "{name}");
        }
        if drain {
            assert_eq!(s.try_dispatch(t0), GateDecision::Allow, "{name}");
        }
        let park = parked.then(|| live_park(t0)).flatten();
        let mark = unusable.then(|| unusable_at(REVOKED, t0)).flatten();

        // Act
        let snapshot = s.capacity_snapshot_under(t0, park, mark);

        // Assert
        assert_eq!(snapshot.reason(), expected, "{name}");
        assert_eq!(snapshot.is_dispatchable(), expected.is_none(), "{name}");
    }
}

#[test]
fn the_snapshot_credential_state_prefers_unusable_over_parked() {
    let t0 = Instant::now();
    let rows = [
        ("neither", false, false, CredentialState::Usable),
        ("parked", false, true, CredentialState::Parked),
        ("unusable", true, false, CredentialState::Unusable),
        ("both", true, true, CredentialState::Unusable),
    ];
    for (name, unusable, parked, expected) in rows {
        // Arrange
        let s = state(Some(1), None);
        let park = parked.then(|| live_park(t0)).flatten();
        let mark = unusable.then(|| unusable_at(REVOKED, t0)).flatten();

        // Act
        let snapshot = s.capacity_snapshot_under(t0, park, mark);

        // Assert
        assert_eq!(snapshot.credential, expected, "{name}");
    }
}

#[test]
fn gate_status_reports_the_unusable_cause_and_its_age() {
    // Arrange
    let s = state(Some(1), None);
    let t0 = Instant::now();
    let read_at = t0 + Duration::from_mins(7);

    // Act
    let marked = s.gate_status_under(read_at, None, unusable_at(EXPIRED, t0));
    let unmarked = s.gate_status_under(read_at, None, None);

    // Assert
    assert_eq!(
        marked.credential_unusable,
        Some(CredentialUnusableStatus {
            cause: EXPIRED,
            since_elapsed: Duration::from_mins(7),
        })
    );
    assert_eq!(unmarked.credential_unusable, None);
}

#[test]
fn every_cause_has_a_distinct_snake_case_token() {
    let rows = [
        (UnusableCause::NotLoggedIn, "not_logged_in"),
        (UnusableCause::Unreadable, "unreadable"),
        (UnusableCause::Expired, "expired"),
        (UnusableCause::Revoked, "revoked"),
    ];
    for (cause, token) in rows {
        assert_eq!(cause.as_str(), token, "{cause:?}");
    }
}
