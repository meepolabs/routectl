//! An account whose credential is known to be unusable refuses every
//! admission on the account under the account lock, before the model slot is
//! touched, until a reload resets it. Status and placement read the mark
//! without mutating any gate.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use routectl_core::{ChatRequest, EligibilityReason, Error, Provider};

use super::super::credential_park_dispatch_tests::{ScriptedProvider, Step, Surface, dispatch};
use super::super::{AliasValue, Config, Router};
use super::CREDENTIAL_UNUSABLE_GATE_KIND;
use crate::config::{PoolEntry, ProviderEntry, ProviderRuntimePolicy, RetryPolicy, SeatSelection};
use crate::resolved::ResolvedModel;
use crate::runtime_state::{
    AccountReset, MarkOutcome, RefreshSettlement, ResetOutcome, UnusableCause,
};
use crate::seat_pool::{SeatPin, SeatTarget};

const SURFACES: [Surface; 3] = [Surface::Complete, Surface::Stream, Surface::CountTokens];
const ACCOUNT_A: &str = "#p-a";

fn single_attempt() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 1,
        initial_backoff_ms: 0,
        jitter_ms: 0,
        ..RetryPolicy::default()
    }
}

/// One request per minute and a one-failure breaker, so a single admission
/// visibly spends the bucket and a forced zero cooldown leaves the slot
/// half-open-ready.
fn rpm1_entry() -> ProviderEntry {
    ProviderEntry::anthropic_api("literal:k").with_runtime(ProviderRuntimePolicy {
        rpm_limit: Some(1),
        circuit_failures: Some(1),
        ..Default::default()
    })
}

/// `m-a` on entry `p-a` and `m-b` on `p-b`, chained by alias `alias`.
struct Chain {
    router: Arc<Router>,
    a: Arc<ScriptedProvider>,
    b: Arc<ScriptedProvider>,
}

fn chain(entry: fn() -> ProviderEntry) -> Chain {
    let a = ScriptedProvider::new("p-a", &[Step::Ok]);
    let b = ScriptedProvider::new("p-b", &[Step::Ok]);
    let mut config = Config {
        retry: single_attempt(),
        ..Config::default()
    };
    config.providers.insert("p-a".into(), entry());
    config.providers.insert("p-b".into(), entry());
    config.aliases.insert(
        "alias".into(),
        AliasValue::Chain(vec!["m-a".into(), "m-b".into()]),
    );
    let models: BTreeMap<String, Arc<ResolvedModel>> = [
        ("m-a", "p-a", Arc::clone(&a)),
        ("m-b", "p-b", Arc::clone(&b)),
    ]
    .into_iter()
    .map(|(nickname, provider, scripted)| {
        let model = ResolvedModel::new(
            nickname,
            provider,
            scripted as Arc<dyn Provider>,
            format!("claude-{nickname}"),
        );
        (nickname.to_string(), Arc::new(model))
    })
    .collect();
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    Chain {
        router: Arc::new(router),
        a,
        b,
    }
}

fn plain_entry() -> ProviderEntry {
    ProviderEntry::anthropic_api("literal:k")
}

fn to(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        ..Default::default()
    }
}

/// A half-open-ready `m-a` whose rpm-1 bucket is full.
fn half_open_ready() -> Chain {
    let fx = chain(rpm1_entry);
    assert!(fx.router.force_open_breaker("m-a", Duration::ZERO));
    fx
}

/// The projected bucket level and the half-open bit of `state_key`.
fn gate_probe_state(router: &Router, state_key: &str) -> (Option<f64>, bool) {
    let status = router.gate_status_for_tests(state_key);
    (status.rpm_available, status.half_open_probe_in_flight)
}

fn unusable_refusal() -> Option<(&'static str, EligibilityReason, Option<Duration>)> {
    Some((
        CREDENTIAL_UNUSABLE_GATE_KIND,
        EligibilityReason::CredentialUnusable,
        None,
    ))
}

fn refusal_of(
    (kind, err): (&'static str, Error),
) -> (&'static str, EligibilityReason, Option<Duration>) {
    match err {
        Error::LocallyUnavailable {
            reason,
            retry_after,
        } => (kind, reason, retry_after),
        other => panic!("a gate refusal is a local refusal, got {other:?}"),
    }
}

#[test]
fn an_unusable_refusal_charges_no_rpm_token_and_claims_no_half_open_slot() {
    for probe in [false, true] {
        // Arrange
        let fx = half_open_ready();
        assert!(
            fx.router
                .mark_credential_unusable_for_tests("m-a", UnusableCause::Revoked)
        );
        let before = gate_probe_state(&fx.router, "m-a");
        assert_eq!(before, (Some(1.0), false), "probe={probe}: premise");

        // Act
        let (refusal, guard) = if probe {
            fx.router.admit_probe_dispatch("m-a")
        } else {
            fx.router.admit_dispatch("m-a")
        };

        // Assert
        assert_eq!(refusal.map(refusal_of), unusable_refusal(), "probe={probe}");
        assert!(!guard.owns_slot(), "probe={probe}");
        drop(guard);
        assert_eq!(gate_probe_state(&fx.router, "m-a"), before, "probe={probe}");
    }
}

#[test]
fn a_reset_readmits_the_account_into_the_slot_and_token_it_left_untouched() {
    // Arrange: the positive control for the refusal above; the half-open
    // slot and the token were there to take.
    let fx = half_open_ready();
    assert!(
        fx.router
            .mark_credential_unusable_for_tests("m-a", UnusableCause::Revoked)
    );
    assert!(fx.router.admit_dispatch("m-a").0.is_some(), "premise");

    // Act
    let resets = fx.router.lift_credential_parks([ACCOUNT_A]);
    let (refusal, mut guard) = fx.router.admit_dispatch("m-a");

    // Assert
    assert_eq!(
        resets,
        [AccountReset {
            account: ACCOUNT_A.to_string(),
            outcome: ResetOutcome {
                park_lifted: false,
                unusable_cleared: Some(UnusableCause::Revoked),
            },
        }]
    );
    assert!(refusal.is_none());
    assert!(
        guard.owns_slot(),
        "the readmitted request takes the re-test"
    );
    let (tokens, in_flight) = gate_probe_state(&fx.router, "m-a");
    assert!(tokens.is_some_and(|t| t < 1.0), "the token was spent");
    assert!(in_flight);
    guard.release();
}

#[test]
fn a_mark_on_an_account_no_admission_has_seen_is_lifted_by_a_reload() {
    // Arrange: nothing has admitted on `#p-a`, so the account has no
    // record until the mark creates one.
    let fx = chain(plain_entry);
    assert_eq!(
        fx.router
            .mark_credential_unusable(ACCOUNT_A, UnusableCause::NotLoggedIn, None),
        MarkOutcome::Transitioned
    );
    let generation = fx.router.credential_generation_for_tests("m-a");
    assert!(fx.router.admit_dispatch("m-a").0.is_some(), "premise");

    // Act
    fx.router.lift_credential_parks([ACCOUNT_A]);

    // Assert
    assert_eq!(
        fx.router.credential_generation_for_tests("m-a"),
        generation.wrapping_add(1)
    );
    assert!(fx.router.admit_dispatch("m-a").0.is_none());
}

#[test]
fn a_mark_gathered_before_a_reload_does_not_mark_the_reloaded_credential() {
    // Arrange
    let fx = chain(plain_entry);
    assert!(fx.router.admit_for_credential_tests("m-a").is_some());
    let observed_under = fx.router.credential_generation_for_tests("m-a");
    fx.router.lift_credential_parks([ACCOUNT_A]);

    // Act
    let marked =
        fx.router
            .mark_credential_unusable(ACCOUNT_A, UnusableCause::Revoked, Some(observed_under));

    // Assert
    assert_eq!(marked, MarkOutcome::Stale);
    assert!(fx.router.admit_dispatch("m-a").0.is_none());
}

#[test]
fn a_mark_under_a_generation_acquired_before_a_reset_is_stale_on_an_unseen_account() {
    // Arrange: nothing has admitted on `#p-a`; the acquire registers it.
    let fx = chain(plain_entry);
    let observed_under = fx.router.acquire_credential_generation(ACCOUNT_A);
    let resets = fx.router.lift_credential_parks([ACCOUNT_A]);

    // Act
    let marked =
        fx.router
            .mark_credential_unusable(ACCOUNT_A, UnusableCause::Revoked, Some(observed_under));

    // Assert
    assert_eq!(marked, MarkOutcome::Stale);
    assert!(fx.router.admit_dispatch("m-a").0.is_none());
    assert_eq!(resets.len(), 1, "the acquired account is reset");
}

#[test]
fn an_outcome_delayed_past_a_refresh_settle_is_stale() {
    for cause in [UnusableCause::Revoked, UnusableCause::Expired] {
        // Arrange: two outcomes observed the same generation; the refresh
        // success settles first.
        let fx = chain(plain_entry);
        let observed_under = fx.router.acquire_credential_generation(ACCOUNT_A);
        let settled = fx
            .router
            .credential_refreshed_in_generation(ACCOUNT_A, observed_under);

        // Act
        let marked = fx
            .router
            .mark_credential_unusable(ACCOUNT_A, cause, Some(observed_under));

        // Assert
        assert_eq!(marked, MarkOutcome::Stale, "{cause:?}");
        assert!(fx.router.admit_dispatch("m-a").0.is_none(), "{cause:?}");
        assert_eq!(
            settled,
            RefreshSettlement::Settled {
                cleared_expired: false,
                generation: observed_under.wrapping_add(1),
            },
            "{cause:?}"
        );
    }
}

#[test]
fn a_refresh_settle_in_the_current_generation_readmits_an_expired_account() {
    let rows = [("current", false, true), ("stale", true, false)];
    for (name, reset_after_mark, readmitted) in rows {
        // Arrange
        let fx = chain(plain_entry);
        let generation = fx.router.acquire_credential_generation(ACCOUNT_A);
        assert_eq!(
            fx.router
                .mark_credential_unusable(ACCOUNT_A, UnusableCause::Expired, Some(generation)),
            MarkOutcome::Transitioned,
            "{name}"
        );
        if reset_after_mark {
            fx.router.lift_credential_parks([ACCOUNT_A]);
            assert!(
                fx.router
                    .mark_credential_unusable(ACCOUNT_A, UnusableCause::Expired, None)
                    .applied()
            );
        }

        // Act
        let settled = fx
            .router
            .credential_refreshed_in_generation(ACCOUNT_A, generation);

        // Assert
        let expected = if readmitted {
            RefreshSettlement::Settled {
                cleared_expired: true,
                generation: generation.wrapping_add(1),
            }
        } else {
            RefreshSettlement::Stale
        };
        assert_eq!(settled, expected, "{name}");
        assert_eq!(
            fx.router.admit_dispatch("m-a").0.is_none(),
            readmitted,
            "{name}"
        );
    }
}

#[test]
fn status_reports_the_unusable_cause_on_every_target_of_the_account() {
    // Arrange
    let fx = chain(plain_entry);
    assert!(
        fx.router
            .mark_credential_unusable_for_tests("m-a", UnusableCause::Unreadable)
    );

    // Act
    let rows: BTreeMap<String, Option<UnusableCause>> = fx
        .router
        .status_targets(Instant::now())
        .into_iter()
        .map(|t| (t.state_key, t.gate.credential_unusable.map(|u| u.cause)))
        .collect();

    // Assert
    let expected: BTreeMap<String, Option<UnusableCause>> = [
        ("m-a".to_string(), Some(UnusableCause::Unreadable)),
        ("m-b".to_string(), None),
    ]
    .into_iter()
    .collect();
    assert_eq!(rows, expected);
}

#[test]
fn concurrent_status_and_snapshot_reads_leave_the_half_open_slot_and_token_untouched() {
    const THREADS: usize = 64;
    const CALLS: usize = 1_000;
    // Arrange
    let fx = half_open_ready();
    let account = fx.router.account_for_state_key_in_tests("m-a");

    // Act
    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|| {
                for _ in 0..CALLS {
                    let now = Instant::now();
                    assert_eq!(fx.router.status_targets(now).len(), 2);
                    assert!(
                        fx.router
                            .capacity_snapshot_for("m-a", account.as_deref(), now)
                            .is_some()
                    );
                }
            });
        }
    });

    // Assert: the one real admission still finds both side effects to take.
    let (refusal, mut guard) = fx.router.admit_dispatch("m-a");
    assert!(refusal.is_none(), "{refusal:?}");
    assert!(guard.owns_slot(), "the admission claims the half-open slot");
    let (tokens, _) = gate_probe_state(&fx.router, "m-a");
    assert!(
        tokens.is_some_and(|t| t < 1.0),
        "the admission took the one token: {tokens:?}"
    );
    guard.release();
}

#[tokio::test]
async fn a_mark_landing_after_the_snapshot_is_honored_at_admission() {
    // Arrange: `m-a` refuses on its open breaker, and its admission is where
    // the mark on `m-b`'s account lands, after placement read `m-b` as
    // dispatchable.
    let fx = chain(plain_entry);
    assert!(fx.router.force_open_breaker("m-a", Duration::from_hours(1)));
    let account_b = fx.router.account_for_state_key_in_tests("m-b");
    let snapshot = fx
        .router
        .capacity_snapshot_for("m-b", account_b.as_deref(), Instant::now())
        .expect("m-b owns a slot");
    assert_eq!(snapshot.reason(), None, "premise: m-b reads dispatchable");
    let marker = Arc::clone(&fx.router);
    super::super::runtime_gate::set_after_admission_hook(move || {
        assert!(marker.mark_credential_unusable_for_tests("m-b", UnusableCause::Revoked));
    });

    // Act
    let err = fx.router.complete(to("alias")).await.err();

    // Assert
    assert_eq!(fx.b.calls.load(Ordering::SeqCst), 0, "m-b is never dialed");
    assert!(
        matches!(
            err,
            Some(Error::LocallyUnavailable {
                reason: EligibilityReason::CredentialUnusable,
                ..
            })
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn refusals_of_an_unusable_seat_log_at_debug_and_never_warn() {
    const REQUESTS: usize = 10;
    // (case, also mark the fallback unusable)
    let rows = [("primary unusable", false), ("every target unusable", true)];
    for surface in SURFACES {
        for (case, both) in rows {
            // Arrange
            let fx = chain(plain_entry);
            let mut marked = vec!["m-a"];
            if both {
                marked.push("m-b");
            }
            for key in marked {
                assert!(
                    fx.router
                        .mark_credential_unusable_for_tests(key, UnusableCause::Revoked)
                );
            }

            // Act
            let ((), events) = routectl_testkit::with_capture(Box::pin(async {
                for _ in 0..REQUESTS {
                    dispatch(&fx.router, surface, to("alias")).await;
                }
            }))
            .await;

            // Assert
            let label = format!("{surface:?}, {case}");
            let warns: Vec<&str> = events
                .iter()
                .filter(|e| e.level == tracing::Level::WARN)
                .map(|e| e.message.as_str())
                .collect();
            assert_eq!(warns, Vec::<&str>::new(), "{label}");
            let refusal_debugs = events
                .iter()
                .filter(|e| {
                    e.level == tracing::Level::DEBUG
                        && e.field("gate_kind") == Some(CREDENTIAL_UNUSABLE_GATE_KIND)
                })
                .count();
            assert!(
                refusal_debugs >= REQUESTS,
                "{label}: every request was refused at least once ({refusal_debugs})"
            );
            assert_eq!(fx.a.calls.load(Ordering::SeqCst), 0, "{label}");
            let expected_b_calls = if both { 0 } else { REQUESTS };
            assert_eq!(
                fx.b.calls.load(Ordering::SeqCst),
                expected_b_calls,
                "{label}: every surface walks past this refusal"
            );
        }
    }
}

/// Pooled model `pooled` over a sticky pool of OAuth seats `seat-a`,
/// `seat-b`, with each seat's scripted provider.
fn sticky_pool() -> (Router, Arc<ScriptedProvider>, Arc<ScriptedProvider>) {
    let members = ["seat-a", "seat-b"];
    let mut config = Config {
        retry: single_attempt(),
        ..Config::default()
    };
    for member in members {
        config.providers.insert(
            member.into(),
            ProviderEntry::anthropic_api(format!("oauth://{member}")),
        );
    }
    config.pools.insert(
        "the-pool".into(),
        PoolEntry::new(members.iter().map(|m| (*m).to_string()).collect())
            .with_seat_selection(SeatSelection::StickyLeastLoaded),
    );
    let providers = [
        ScriptedProvider::new("seat-a", &[Step::Ok]),
        ScriptedProvider::new("seat-b", &[Step::Ok]),
    ];
    let seats: Arc<[SeatTarget]> = members
        .iter()
        .zip(&providers)
        .map(|(member, provider)| {
            SeatTarget::new(
                (*member).to_string(),
                Arc::clone(provider) as Arc<dyn Provider>,
                Some(routectl_auth::SecretRef::OAuth {
                    provider: (*member).to_string(),
                    label: None,
                }),
                false,
            )
        })
        .collect();
    let model = ResolvedModel::new(
        "pooled",
        "the-pool",
        Arc::clone(&seats[0].provider),
        "claude-x",
    )
    .with_seats(seats);
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(BTreeMap::from([("pooled".to_string(), Arc::new(model))]));
    let [a, b] = providers;
    (router, a, b)
}

fn session_request() -> ChatRequest {
    let mut req = to("pooled");
    req.routectl_internal.inbound_session_key = Some("S".into());
    req
}

fn pinned(router: &Router) -> Option<SeatPin> {
    router
        .sticky_pins
        .get(&super::super::chain::sticky_pin_key("S", "the-pool"))
}

#[tokio::test]
async fn a_session_pinned_to_an_unusable_seat_moves_and_stays_after_recovery() {
    // Arrange
    let (router, a, b) = sticky_pool();
    router.sticky_pins.put(
        &super::super::chain::sticky_pin_key("S", "the-pool"),
        SeatPin {
            member: "seat-a".into(),
            repinned: false,
        },
    );
    assert!(router.mark_credential_unusable_for_tests("pooled#seat-a", UnusableCause::Revoked));

    // Act
    let moved = router.complete(session_request()).await;
    let pin_after_move = pinned(&router).map(|p| p.member);
    assert!(router.reset_credential_for_tests("pooled#seat-a"));
    let after_recovery = router.complete(session_request()).await;

    // Assert
    assert!(moved.is_ok(), "{:?}", moved.err());
    assert!(after_recovery.is_ok(), "{:?}", after_recovery.err());
    assert_eq!(pin_after_move.as_deref(), Some("seat-b"));
    assert_eq!(
        pinned(&router).map(|p| p.member).as_deref(),
        Some("seat-b"),
        "nothing moves the pin back"
    );
    assert_eq!(a.calls.load(Ordering::SeqCst), 0);
    assert_eq!(b.calls.load(Ordering::SeqCst), 2);
}
