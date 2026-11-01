//! A placeholder seat (a pool member whose credential could not be read at
//! build) is never dialed: on every surface it records itself as the last
//! target, counts no attempt, and folds a `credential_unusable` refusal with
//! no known wait into the chain's error. Its account is marked unusable at
//! install and again after a reload shares the outgoing account map.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use routectl_core::{
    ChatRequest, EligibilityReason, Error, Provider, RequestObservation, SkipCounts, UpstreamReason,
};

use super::super::credential_park_dispatch_tests::{ScriptedProvider, Step, Surface};
use super::super::{DispatchMeta, Router, RouterOptions};
use crate::config::{AliasValue, Config, PoolEntry, ProviderEntry, RetryPolicy};
use crate::pool_build::{PoolPlaceholder, PoolReport};
use crate::resolved::ResolvedModel;
use crate::runtime_state::UnusableCause;
use crate::seat_pool::SeatTarget;

const SURFACES: [Surface; 3] = [Surface::Complete, Surface::Stream, Surface::CountTokens];
const MEMBERS: [&str; 2] = ["seat-a", "seat-b"];

fn oauth_ref(member: &str) -> routectl_auth::SecretRef {
    routectl_auth::SecretRef::OAuth {
        provider: "anthropic".into(),
        label: Some(member.into()),
    }
}

fn account_of(member: &str) -> String {
    format!("anthropic#{member}")
}

/// Pooled model `pooled` over seats `seat-a`, `seat-b` behind alias `alias`;
/// each member named in `placeholders` is a placeholder seat reported with
/// cause `not_logged_in`, the others answer through `built`.
struct Pool {
    router: Router,
    built: Arc<ScriptedProvider>,
}

fn pool(placeholders: &[&str]) -> Pool {
    let built = ScriptedProvider::new("built", &[Step::Ok]);
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
            ProviderEntry::anthropic_api(format!("oauth://anthropic#{member}")),
        );
    }
    config.pools.insert(
        "the-pool".into(),
        PoolEntry::new(MEMBERS.iter().map(|m| (*m).to_string()).collect()),
    );
    config
        .aliases
        .insert("alias".into(), AliasValue::Single("pooled".into()));
    let seats: Arc<[SeatTarget]> = MEMBERS
        .iter()
        .map(|member| {
            if placeholders.contains(member) {
                SeatTarget::placeholder((*member).into(), Some(oauth_ref(member)), false)
            } else {
                SeatTarget::new(
                    (*member).into(),
                    Arc::clone(&built) as Arc<dyn Provider>,
                    Some(oauth_ref(member)),
                    false,
                )
            }
        })
        .collect();
    let model = ResolvedModel::new(
        "pooled",
        "the-pool",
        Arc::clone(&built) as Arc<dyn Provider>,
        "claude-x",
    )
    .with_seats(seats);
    let report = PoolReport {
        pool: "the-pool".into(),
        models: vec!["pooled".into()],
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
    router.install_resolved_models(BTreeMap::from([("pooled".to_string(), Arc::new(model))]));
    Pool { router, built }
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "alias".into(),
        ..Default::default()
    }
}

/// Dispatch one request on `surface` and return its meta and error, if any.
async fn dispatch(router: &Router, surface: Surface) -> (DispatchMeta, Option<Error>) {
    dispatch_request(router, surface, request()).await
}

async fn dispatch_request(
    router: &Router,
    surface: Surface,
    request: ChatRequest,
) -> (DispatchMeta, Option<Error>) {
    match surface {
        Surface::Complete => {
            let out = router
                .complete_with_options(request, RouterOptions::new())
                .await;
            (out.meta, out.result.err())
        }
        Surface::Stream => {
            let out = router
                .stream_with_options(request, RouterOptions::new())
                .await;
            (out.meta, out.result.err())
        }
        Surface::CountTokens => {
            let out = router.count_tokens_with_meta(request).await;
            (out.meta, out.result.err())
        }
    }
}

fn local_refusal(err: Option<&Error>) -> Option<(EligibilityReason, Option<Duration>)> {
    match err {
        Some(Error::LocallyUnavailable {
            reason,
            retry_after,
        }) => Some((*reason, *retry_after)),
        _ => None,
    }
}

fn unusable_since(router: &Router, member: &str) -> Option<(UnusableCause, Instant)> {
    router
        .account_record(Some(&account_of(member)))
        .and_then(|record| record.unusable())
        .map(|mark| (mark.cause, mark.since))
}

#[tokio::test]
async fn an_all_placeholder_pool_refuses_every_surface_credential_unusable_without_a_dial() {
    for surface in SURFACES {
        // Arrange
        let fx = pool(&MEMBERS);

        // Act
        let (meta, err) = dispatch(&fx.router, surface).await;

        // Assert
        assert_eq!(
            local_refusal(err.as_ref()),
            Some((EligibilityReason::CredentialUnusable, None)),
            "{surface:?}: {err:?}"
        );
        assert_eq!(meta.attempt_count, 0, "{surface:?}: nothing was attempted");
        assert_eq!(
            meta.served_provider.as_deref(),
            Some("seat-b"),
            "{surface:?}: the last seat walked is recorded"
        );
        assert_eq!(meta.served_model.as_deref(), Some("pooled"), "{surface:?}");
        assert_eq!(meta.fallback_count, 1, "{surface:?}");
        assert_eq!(fx.built.calls.load(Ordering::SeqCst), 0, "{surface:?}");
    }
}

#[tokio::test]
async fn a_placeholder_seat_is_walked_past_to_a_built_seat_on_every_surface() {
    for surface in SURFACES {
        // Arrange
        let fx = pool(&["seat-a"]);

        // Act
        let (meta, err) = dispatch(&fx.router, surface).await;

        // Assert
        assert!(err.is_none(), "{surface:?}: {err:?}");
        assert_eq!(
            meta.served_provider.as_deref(),
            Some("seat-b"),
            "{surface:?}"
        );
        assert_eq!(meta.attempt_count, 1, "{surface:?}: only seat-b was dialed");
        assert_eq!(fx.built.calls.load(Ordering::SeqCst), 1, "{surface:?}");
    }
}

#[tokio::test]
async fn a_placeholder_beside_a_parked_seat_refuses_unusable_with_the_parks_wait() {
    for surface in SURFACES {
        // Arrange: seat-b's account is parked for two minutes.
        let fx = pool(&["seat-a"]);
        assert!(fx.router.park_credential_for_tests(
            "pooled#seat-b",
            UpstreamReason::CredentialExpired,
            Instant::now() + Duration::from_mins(2),
        ));

        // Act
        let (meta, err) = dispatch(&fx.router, surface).await;

        // Assert
        let (reason, wait) = local_refusal(err.as_ref())
            .unwrap_or_else(|| panic!("{surface:?}: a local refusal, got {err:?}"));
        assert_eq!(reason, EligibilityReason::CredentialUnusable, "{surface:?}");
        let wait = wait.unwrap_or_else(|| panic!("{surface:?}: the park's wait is known"));
        assert!(
            wait > Duration::from_secs(115) && wait <= Duration::from_mins(2),
            "{surface:?}: {wait:?}"
        );
        assert_eq!(meta.attempt_count, 0, "{surface:?}");
        assert_eq!(fx.built.calls.load(Ordering::SeqCst), 0, "{surface:?}");
    }
}

#[tokio::test]
async fn a_placeholder_seat_counts_one_credential_unusable_skip_on_every_surface() {
    let rows: [(&str, bool, SkipCounts); 2] = [
        (
            "seat-b serves",
            false,
            SkipCounts {
                credential_unusable: 1,
                ..SkipCounts::default()
            },
        ),
        (
            "seat-b parked",
            true,
            SkipCounts {
                credential_unusable: 1,
                account_parked: 1,
                ..SkipCounts::default()
            },
        ),
    ];
    for surface in SURFACES {
        for (name, park_b, expected) in rows {
            // Arrange
            let fx = pool(&["seat-a"]);
            if park_b {
                assert!(fx.router.park_credential_for_tests(
                    "pooled#seat-b",
                    UpstreamReason::CredentialExpired,
                    Instant::now() + Duration::from_mins(2),
                ));
            }
            let observation = RequestObservation::new();
            let mut req = request();
            req.routectl_internal.observation = Some(observation.clone());

            // Act
            let (meta, _) = dispatch_request(&fx.router, surface, req).await;

            // Assert
            let skips = observation.snapshot().skips;
            assert_eq!(skips, expected, "{surface:?} {name}");
            let dialed = u64::from(meta.attempt_count > 0);
            assert_eq!(
                skips.total() + dialed,
                2,
                "{surface:?} {name}: both evaluated"
            );
        }
    }
}

#[test]
fn installing_the_build_report_marks_each_placeholder_account_unusable() {
    // Arrange / Act
    let fx = pool(&["seat-a"]);

    // Assert
    assert_eq!(
        unusable_since(&fx.router, "seat-a").map(|(cause, _)| cause),
        Some(UnusableCause::NotLoggedIn)
    );
    assert_eq!(unusable_since(&fx.router, "seat-b"), None);
}

#[test]
fn a_seat_built_before_a_reload_and_a_placeholder_after_it_is_marked_on_the_shared_map() {
    // Arrange: the outgoing router built seat-a; its map holds no mark.
    let before = pool(&[]);
    assert_eq!(unusable_since(&before.router, "seat-a"), None);
    let mut after = pool(&["seat-a"]);

    // Act
    after.router.carry_over_runtime_state_from(&before.router);

    // Assert: the shared map, as both routers see it, now carries the mark.
    assert_eq!(
        unusable_since(&after.router, "seat-a").map(|(cause, _)| cause),
        Some(UnusableCause::NotLoggedIn)
    );
    assert!(Arc::ptr_eq(
        &after.router.credential_accounts,
        &before.router.credential_accounts
    ));
}

#[test]
fn an_unchanged_placeholder_keeps_its_mark_and_since_across_a_carry_over() {
    // Arrange
    let before = pool(&["seat-a"]);
    let first = unusable_since(&before.router, "seat-a").expect("marked at install");
    std::thread::sleep(Duration::from_millis(5));
    let mut after = pool(&["seat-a"]);

    // Act
    after.router.carry_over_runtime_state_from(&before.router);

    // Assert
    assert_eq!(unusable_since(&after.router, "seat-a"), Some(first));
}
