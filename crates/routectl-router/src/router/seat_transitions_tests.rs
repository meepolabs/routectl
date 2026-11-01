//! Live pool seat counts and the seat-transition log lines: counts read
//! without moving any gate, one WARN when a seat becomes unusable, one INFO
//! when it recovers, and nothing per request.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use routectl_core::{ChatRequest, EligibilityReason, Provider};
use routectl_testkit::{CapturedEvent, capture_events, with_capture};

use super::super::credential_park_dispatch_tests::{ScriptedProvider, Step, Surface, dispatch};
use super::super::{PoolEligibility, Router};
use crate::config::{
    AliasValue, Config, PoolEntry, ProviderEntry, ProviderRuntimePolicy, RetryPolicy,
};
use crate::pool_build::{PoolPlaceholder, PoolReport};
use crate::resolved::ResolvedModel;
use crate::runtime_state::{MarkOutcome, UnusableCause};
use crate::seat_pool::SeatTarget;

const MEMBERS: [&str; 3] = ["seat-a", "seat-b", "seat-c"];
const SURFACES: [Surface; 3] = [Surface::Complete, Surface::Stream, Surface::CountTokens];

fn account_of(member: &str) -> String {
    format!("anthropic#{member}")
}

fn state_key_of(member: &str) -> String {
    format!("pooled#{member}")
}

/// Pool `the-pool` over `MEMBERS` behind model `pooled` and alias `alias`.
/// Each member allows one request per minute and trips its breaker on one
/// failure, so an admission visibly spends the bucket. Members named in
/// `placeholders` are placeholder seats reported with cause `not_logged_in`.
fn router(placeholders: &[&str]) -> Router {
    router_for_models(placeholders, &["pooled"])
}

/// [`router`] with `the-pool` routed by every model in `nicknames`, all
/// sharing the one seat list.
fn router_for_models(placeholders: &[&str], nicknames: &[&str]) -> Router {
    let mut config = Config {
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            jitter_ms: 0,
            ..RetryPolicy::default()
        },
        ..Config::default()
    };
    for member in MEMBERS {
        config.providers.insert(
            member.into(),
            ProviderEntry::anthropic_api(format!("oauth://anthropic#{member}")).with_runtime(
                ProviderRuntimePolicy {
                    rpm_limit: Some(1),
                    circuit_failures: Some(1),
                    ..Default::default()
                },
            ),
        );
    }
    config.pools.insert(
        "the-pool".into(),
        PoolEntry::new(MEMBERS.iter().map(|m| (*m).to_string()).collect()),
    );
    config
        .aliases
        .insert("alias".into(), AliasValue::Single("pooled".into()));
    let built = ScriptedProvider::new("built", &[Step::Ok]);
    let seats: Arc<[SeatTarget]> = MEMBERS
        .iter()
        .map(|member| {
            let secret_ref = Some(routectl_auth::SecretRef::OAuth {
                provider: "anthropic".into(),
                label: Some((*member).into()),
            });
            if placeholders.contains(member) {
                SeatTarget::placeholder((*member).into(), secret_ref, false)
            } else {
                let provider = Arc::clone(&built) as Arc<dyn Provider>;
                SeatTarget::new((*member).into(), provider, secret_ref, false)
            }
        })
        .collect();
    let models: BTreeMap<String, Arc<ResolvedModel>> = nicknames
        .iter()
        .map(|nickname| {
            let provider = Arc::clone(&built) as Arc<dyn Provider>;
            let model = ResolvedModel::new(*nickname, "the-pool", provider, "claude-x")
                .with_seats(Arc::clone(&seats));
            ((*nickname).to_string(), Arc::new(model))
        })
        .collect();
    let report = PoolReport {
        pool: "the-pool".into(),
        models: nicknames.iter().map(|n| (*n).to_string()).collect(),
        configured_members: MEMBERS.len(),
        usable_members: MEMBERS.len() - placeholders.len(),
        placeholders: placeholders
            .iter()
            .map(|member| PoolPlaceholder {
                member: (*member).into(),
                provider_kind: "anthropic-api",
                cause: UnusableCause::NotLoggedIn,
                account: Some(account_of(member)),
            })
            .collect(),
    };
    let mut router = Router::new(Arc::new(config));
    router.install_pool_reports(vec![report]);
    router.install_resolved_models(models);
    router
}

/// Every WARN or INFO event: the levels an operator reads by default.
fn loud(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|e| e.level <= tracing::Level::INFO)
        .collect()
}

/// `(level, event, member, cause, eligible, configured)` of each loud event.
type Line<'a> = (
    tracing::Level,
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
    Option<&'a str>,
);

fn lines(events: &[CapturedEvent]) -> Vec<Line<'_>> {
    loud(events)
        .into_iter()
        .map(|e| {
            (
                e.level,
                e.field("event"),
                e.field("member"),
                e.field("cause"),
                e.field("eligible"),
                e.field("configured"),
            )
        })
        .collect()
}

/// The projected bucket level, breaker phase and half-open bit of a seat.
fn gate_state(router: &Router, member: &str) -> (Option<f64>, String, bool) {
    let status = router.gate_status_for_tests(&state_key_of(member));
    (
        status.rpm_available,
        format!("{:?}", status.circuit),
        status.half_open_probe_in_flight,
    )
}

fn expected_counts() -> Vec<PoolEligibility> {
    vec![PoolEligibility {
        pool: "the-pool".into(),
        configured: 3,
        eligible: 1,
        by_reason: vec![
            (EligibilityReason::CredentialUnusable, 1),
            (EligibilityReason::BreakerOpen, 1),
        ],
    }]
}

#[test]
fn pool_eligibility_counts_each_seat_by_its_reason_and_reading_it_moves_no_gate() {
    // Arrange: seat-a unusable, seat-b breaker open for an hour, seat-c
    // half-open-ready with a full one-token bucket: the state a mutating
    // read would change by claiming the probe slot or spending the token.
    let router = router(&[]);
    assert!(
        router.mark_credential_unusable_for_tests(&state_key_of("seat-a"), UnusableCause::Revoked)
    );
    assert!(router.force_open_breaker(&state_key_of("seat-b"), Duration::from_hours(1)));
    assert!(router.force_open_breaker(&state_key_of("seat-c"), Duration::ZERO));
    let before = gate_state(&router, "seat-c");

    // Act
    let first = router.pool_eligibility(Instant::now());
    for _ in 0..1000 {
        let _ = router.pool_eligibility(Instant::now());
    }
    let last = router.pool_eligibility(Instant::now());

    // Assert
    assert_eq!(first, expected_counts());
    assert_eq!(last, expected_counts());
    assert_eq!(
        gate_state(&router, "seat-c"),
        before,
        "1000 reads leave the bucket and the probe slot untouched"
    );
    assert!(!before.2, "the fixture starts with no probe in flight");

    // Positive control: one real admission does move the same gate.
    assert!(
        router
            .admit_for_credential_tests(&state_key_of("seat-c"))
            .is_some()
    );
    assert_ne!(
        gate_state(&router, "seat-c"),
        before,
        "an admission spends the token"
    );
}

#[test]
fn a_pool_seat_is_eligible_when_any_model_routed_at_the_pool_admits_it() {
    // (row, breakers forced open as (model, member), expected eligible,
    // expected reasons). Model `pooled-b` shares `the-pool` with `pooled`.
    type Row = (
        &'static str,
        &'static [(&'static str, &'static str)],
        usize,
        Vec<(EligibilityReason, usize)>,
    );
    let rows: Vec<Row> = vec![
        ("every model admits every seat", &[], 3, vec![]),
        (
            "model B breaker open, model A healthy",
            &[("pooled-b", "seat-a")],
            3,
            vec![],
        ),
        (
            "both models breaker open on one seat",
            &[("pooled", "seat-a"), ("pooled-b", "seat-a")],
            2,
            vec![(EligibilityReason::BreakerOpen, 1)],
        ),
    ];
    for (name, open, eligible, by_reason) in rows {
        // Arrange
        let router = router_for_models(&[], &["pooled", "pooled-b"]);
        for (model, member) in open {
            assert!(
                router.force_open_breaker(&format!("{model}#{member}"), Duration::from_hours(1)),
                "`{name}`: the fixture has a state slot for {model}#{member}"
            );
        }

        // Act
        let counts = router.pool_eligibility(Instant::now());

        // Assert
        assert_eq!(
            counts,
            vec![PoolEligibility {
                pool: "the-pool".into(),
                configured: 3,
                eligible,
                by_reason,
            }],
            "`{name}`"
        );
    }
}

#[test]
fn a_seat_every_model_refuses_reports_the_highest_priority_reason() {
    // Arrange: on seat-a, `pooled` reads breaker open and `pooled-b` rate
    // limited (its one-token bucket spent); the breaker outranks either way
    // round the models are walked.
    let router = router_for_models(&[], &["pooled", "pooled-b"]);
    assert!(router.force_open_breaker("pooled#seat-a", Duration::from_hours(1)));
    assert!(
        router
            .admit_for_credential_tests("pooled-b#seat-a")
            .is_some()
    );
    let rate_limited_only = router_for_models(&[], &["pooled-b"]);
    assert!(
        rate_limited_only
            .admit_for_credential_tests("pooled-b#seat-a")
            .is_some()
    );

    // Act
    let counts = router.pool_eligibility(Instant::now());
    let control = rate_limited_only.pool_eligibility(Instant::now());

    // Assert
    assert_eq!(counts[0].eligible, 2);
    assert_eq!(
        counts[0].by_reason,
        vec![(EligibilityReason::BreakerOpen, 1)]
    );
    assert_eq!(
        control[0].by_reason,
        vec![(EligibilityReason::RateLimited, 1)],
        "control: the spent bucket alone reads rate limited"
    );
}

#[test]
fn a_placeholder_seat_counts_as_credential_unusable() {
    let router = router(&["seat-b"]);

    let counts = router.pool_eligibility(Instant::now());

    assert_eq!(counts[0].configured, 3);
    assert_eq!(counts[0].eligible, 2);
    assert_eq!(
        counts[0].by_reason,
        vec![(EligibilityReason::CredentialUnusable, 1)]
    );
}

#[test]
fn a_first_mark_logs_one_warn_and_a_same_cause_re_mark_logs_nothing() {
    // Arrange
    let router = router(&[]);
    let account = account_of("seat-a");

    // Act
    let first = capture_events(|| {
        assert_eq!(
            router.mark_credential_unusable(&account, UnusableCause::Revoked, None),
            MarkOutcome::Transitioned
        );
    });
    let again = capture_events(|| {
        assert_eq!(
            router.mark_credential_unusable(&account, UnusableCause::Revoked, None),
            MarkOutcome::Unchanged
        );
    });

    // Assert
    assert_eq!(
        lines(&first),
        [(
            tracing::Level::WARN,
            Some("seat_unusable"),
            Some("seat-a"),
            Some("revoked"),
            Some("2"),
            Some("3"),
        )],
        "{first:?}"
    );
    let warn = loud(&first)[0];
    assert_eq!(warn.field("pool"), Some("the-pool"));
    assert!(lines(&again).is_empty(), "{again:?}");
}

#[test]
fn a_changed_cause_logs_a_second_warn_naming_the_new_cause() {
    let router = router(&[]);
    let account = account_of("seat-a");
    let _ = router.mark_credential_unusable(&account, UnusableCause::Expired, None);

    let events = capture_events(|| {
        let _ = router.mark_credential_unusable(&account, UnusableCause::Revoked, None);
    });

    let causes: Vec<Option<&str>> = loud(&events).iter().map(|e| e.field("cause")).collect();
    assert_eq!(causes, [Some("revoked")], "{events:?}");
}

#[test]
fn a_stale_mark_logs_nothing() {
    let router = router(&[]);
    let account = account_of("seat-a");
    let generation = router.acquire_credential_generation(&account);
    let _ = router.lift_credential_parks([account.as_str()]);

    let events = capture_events(|| {
        assert_eq!(
            router.mark_credential_unusable(&account, UnusableCause::Revoked, Some(generation)),
            MarkOutcome::Stale
        );
    });

    assert!(lines(&events).is_empty(), "{events:?}");
}

#[tokio::test]
async fn ten_refused_requests_log_nothing_at_warn_or_info() {
    for surface in SURFACES {
        // Arrange: every seat unusable, so every request is refused locally.
        let router = router(&[]);
        for member in MEMBERS {
            let _ =
                router.mark_credential_unusable(&account_of(member), UnusableCause::Revoked, None);
        }

        // Act
        let (served, events) = with_capture(async {
            let mut served = 0;
            for _ in 0..10 {
                let req = ChatRequest {
                    model: "alias".into(),
                    ..Default::default()
                };
                served += usize::from(dispatch(&router, surface, req).await);
            }
            served
        })
        .await;

        // Assert
        assert_eq!(served, 0, "{surface:?}: every request is refused");
        assert!(lines(&events).is_empty(), "{surface:?}: {events:?}");
    }
}

#[test]
fn a_reset_that_clears_a_mark_logs_one_info_and_a_reset_with_nothing_to_clear_logs_none() {
    // Arrange
    let router = router(&[]);
    let marked = account_of("seat-a");
    let clean = account_of("seat-b");
    let _ = router.mark_credential_unusable(&marked, UnusableCause::Revoked, None);
    let _ = router.acquire_credential_generation(&clean);

    // Act
    let events = capture_events(|| {
        let _ = router.lift_credential_parks([marked.as_str(), clean.as_str()]);
    });

    // Assert
    assert_eq!(
        lines(&events),
        [(
            tracing::Level::INFO,
            Some("seat_recovered"),
            Some("seat-a"),
            Some("revoked"),
            Some("3"),
            Some("3"),
        )],
        "{events:?}"
    );
}

#[test]
fn a_refresh_that_clears_an_expired_mark_logs_one_info() {
    let router = router(&[]);
    let account = account_of("seat-a");
    let _ = router.mark_credential_unusable(&account, UnusableCause::Expired, None);
    let generation = router.acquire_credential_generation(&account);

    let events = capture_events(|| {
        let _ = router.credential_refreshed_in_generation(&account, generation);
    });

    let events_seen: Vec<_> = loud(&events)
        .iter()
        .map(|e| (e.level, e.field("event"), e.field("cause")))
        .collect();
    assert_eq!(
        events_seen,
        [(
            tracing::Level::INFO,
            Some("seat_recovered"),
            Some("expired")
        )]
    );
}

#[test]
fn an_account_no_pool_holds_logs_its_account_and_cause_alone() {
    let router = router(&[]);

    let events = capture_events(|| {
        let _ = router.mark_credential_unusable("other#seat-z", UnusableCause::Revoked, None);
    });

    let loud = loud(&events);
    assert_eq!(loud.len(), 1, "{events:?}");
    assert_eq!(loud[0].field("event"), Some("seat_unusable"));
    assert_eq!(loud[0].field("account"), Some("other#seat-z"));
    assert_eq!(loud[0].field("pool"), None);
}

#[test]
fn a_build_marks_placeholders_quietly_and_boot_announces_each_once() {
    // Act
    let (router, built) = {
        let mut router = None;
        let events = capture_events(|| router = Some(self::router(&["seat-a", "seat-b"])));
        (router.expect("built"), events)
    };
    let announced = capture_events(|| router.log_placeholder_seats());

    // Assert
    assert!(
        lines(&built).is_empty(),
        "the build itself is quiet: {built:?}"
    );
    let members: Vec<_> = loud(&announced)
        .iter()
        .map(|e| (e.field("event"), e.field("member"), e.field("cause")))
        .collect();
    assert_eq!(
        members,
        [
            (Some("seat_unusable"), Some("seat-a"), Some("not_logged_in")),
            (Some("seat_unusable"), Some("seat-b"), Some("not_logged_in")),
        ]
    );
}

#[test]
fn a_reload_logs_only_the_placeholders_that_are_new_on_the_live_map() {
    // (case, placeholders after the reload, members expected to log)
    let rows: [(&str, &[&str], &[&str]); 2] = [
        ("unchanged placeholder", &["seat-a"], &[]),
        ("one new placeholder", &["seat-a", "seat-b"], &["seat-b"]),
    ];
    for (case, after, expected) in rows {
        // Arrange
        let live = router(&["seat-a"]);

        // Act
        let events = capture_events(|| {
            let mut next = router(after);
            next.carry_over_runtime_state_from(&live);
        });

        // Assert
        let members: Vec<_> = loud(&events).iter().map(|e| e.field("member")).collect();
        let expected: Vec<_> = expected.iter().map(|m| Some(*m)).collect();
        assert_eq!(members, expected, "{case}: {events:?}");
    }
}
