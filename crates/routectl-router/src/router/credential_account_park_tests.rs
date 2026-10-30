//! A credential park belongs to the credential's account, not to the model
//! that observed the rejection: every model on the account is refused while
//! it is parked, a model on another account is not, one model's re-test
//! after the park lapses lifts it for all, and a reload's generation fences
//! a stale rejection or success on any model of the account.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use routectl_core::{ChatRequest, Error, Provider, UpstreamReason};

use super::super::credential_park_dispatch_tests::{
    INVALID, ScriptedProvider, Step, Surface, park_reason, rejection,
};
use super::super::{AliasValue, Config, Router};
use crate::config::{PoolEntry, ProviderEntry, RetryPolicy};
use crate::resolved::ResolvedModel;
use crate::runtime_state::CREDENTIAL_PARK_BACKSTOP;

const SURFACES: [Surface; 3] = [Surface::Complete, Surface::Stream, Surface::CountTokens];

/// `m-a` and `m-a2` both on provider entry `p-a` (one account), `m-b` on
/// `p-b` (another), each answering from its own scripted provider.
struct Accounts {
    router: Router,
    a: Arc<ScriptedProvider>,
    a2: Arc<ScriptedProvider>,
    b: Arc<ScriptedProvider>,
}

fn single_attempt_config() -> Config {
    Config {
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            jitter_ms: 0,
            ..RetryPolicy::default()
        },
        ..Config::default()
    }
}

fn router_over(models: &[(&str, &str, Arc<ScriptedProvider>)]) -> Router {
    let mut config = single_attempt_config();
    for (_, provider, _) in models {
        config.providers.insert(
            (*provider).into(),
            ProviderEntry::anthropic_api("literal:k"),
        );
    }
    config.aliases.insert(
        "a-then-b".into(),
        AliasValue::Chain(vec!["m-a".into(), "m-b".into()]),
    );
    let resolved: BTreeMap<String, Arc<ResolvedModel>> = models
        .iter()
        .map(|(nickname, provider, scripted)| {
            (
                (*nickname).to_string(),
                Arc::new(ResolvedModel::new(
                    *nickname,
                    *provider,
                    scripted.clone() as Arc<dyn Provider>,
                    format!("claude-{nickname}"),
                )),
            )
        })
        .collect();
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(resolved);
    router
}

fn accounts(a: &[Step], a2: &[Step], b: &[Step]) -> Accounts {
    let a = ScriptedProvider::new("p-a", a);
    let a2 = ScriptedProvider::new("p-a", a2);
    let b = ScriptedProvider::new("p-b", b);
    let router = router_over(&[
        ("m-a", "p-a", a.clone()),
        ("m-a2", "p-a", a2.clone()),
        ("m-b", "p-b", b.clone()),
    ]);
    Accounts { router, a, a2, b }
}

fn to(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        ..Default::default()
    }
}

/// Dispatch one request on `surface`, draining a stream, and return its
/// error, if any.
async fn dispatch_err(router: &Router, surface: Surface, req: ChatRequest) -> Option<Error> {
    use futures::stream::StreamExt as _;
    match surface {
        Surface::Complete => router.complete(req).await.err(),
        Surface::Stream => match router.stream(req).await {
            Ok(mut stream) => {
                while let Some(item) = stream.next().await {
                    if let Err(e) = item {
                        return Some(e);
                    }
                }
                None
            }
            Err(e) => Some(e),
        },
        Surface::CountTokens => router.count_tokens(req).await.err(),
    }
}

fn calls(provider: &ScriptedProvider) -> usize {
    provider.calls.load(Ordering::SeqCst)
}

fn park(router: &Router, state_key: &str, until: Instant) {
    assert!(
        router.park_credential_for_tests(state_key, UpstreamReason::CredentialInvalid, until),
        "{state_key} must own a state slot",
    );
}

#[tokio::test]
async fn a_typed_rejection_parks_every_model_on_the_account_and_no_other() {
    for surface in SURFACES {
        // Arrange: m-a's upstream rejects the credential with a typed reason.
        let fx = accounts(&[Step::Reject(403, INVALID)], &[Step::Ok], &[Step::Ok]);
        let label = format!("{surface:?}");
        assert!(dispatch_err(&fx.router, surface, to("m-a")).await.is_some());

        // Act
        let sibling = dispatch_err(&fx.router, surface, to("m-a2")).await;
        let other = dispatch_err(&fx.router, surface, to("m-b")).await;

        // Assert: the sibling is refused with the park, never dialed.
        let Some(Error::Credential {
            reason,
            parked_until_ms: Some(_),
            ..
        }) = sibling
        else {
            panic!("{label}: the sibling must be refused by the park, got {sibling:?}");
        };
        assert_eq!(Some(reason), INVALID, "{label}");
        assert_eq!(
            calls(&fx.a2),
            0,
            "{label}: the sibling must never be dialed"
        );
        // The other account is still admitted and dialed.
        assert!(
            other.is_none(),
            "{label}: the other account serves, got {other:?}"
        );
        assert_eq!(calls(&fx.b), 1, "{label}");
        assert_eq!(park_reason(&fx.router, "m-b"), None, "{label}");
    }
}

#[tokio::test]
async fn a_sibling_retest_after_the_park_lapses_lifts_it_for_the_whole_account() {
    for surface in SURFACES {
        // Arrange: the account parked through m-a, already lapsed.
        let fx = accounts(&[Step::Ok], &[Step::Ok], &[Step::Ok]);
        let label = format!("{surface:?}");
        park(&fx.router, "m-a", Instant::now());

        // Act: m-a2 takes its own re-test, which succeeds.
        let retest = dispatch_err(&fx.router, surface, to("m-a2")).await;
        let (refusal, guard) = fx.router.admit_dispatch("m-a", "p-a");

        // Assert
        assert!(
            retest.is_none(),
            "{label}: the re-test serves, got {retest:?}"
        );
        assert_eq!(calls(&fx.a2), 1, "{label}");
        assert_eq!(
            park_reason(&fx.router, "m-a"),
            None,
            "{label}: lifted for m-a"
        );
        assert!(refusal.is_none(), "{label}: m-a is admitted next");
        assert!(
            !guard.owns_slot(),
            "{label}: m-a needs no re-test of its own once a sibling's lifted the park",
        );
        assert_eq!(calls(&fx.a), 0, "{label}");
    }
}

#[tokio::test]
async fn a_lapsed_account_park_admits_one_retest_per_model_slot() {
    // Arrange
    let fx = accounts(&[Step::Ok], &[Step::Ok], &[Step::Ok]);
    park(&fx.router, "m-a", Instant::now());

    // Act: two concurrent requests on m-a, then one on its sibling m-a2.
    let (first, first_guard) = fx.router.admit_dispatch("m-a", "p-a");
    let (second, second_guard) = fx.router.admit_dispatch("m-a", "p-a");
    let (sibling, sibling_guard) = fx.router.admit_dispatch("m-a2", "p-a");

    // Assert
    assert!(
        first.is_none() && first_guard.owns_slot(),
        "the first takes m-a's re-test"
    );
    assert!(
        matches!(
            second,
            Some((super::CREDENTIAL_PARKED_GATE_KIND, Error::Credential { .. }))
        ),
        "the second on m-a is refused by the park, got {second:?}",
    );
    assert!(!second_guard.owns_slot());
    assert!(
        sibling.is_none() && sibling_guard.owns_slot(),
        "m-a2 re-tests through its own slot, got {sibling:?}",
    );
}

#[tokio::test]
async fn a_model_added_on_a_parked_account_is_refused_after_carry_over() {
    for surface in SURFACES {
        // Arrange: the old router parks the account through its only model.
        let old_a = ScriptedProvider::new("p-a", &[Step::Ok]);
        let old = router_over(&[("m-a", "p-a", old_a)]);
        park(&old, "m-a", Instant::now() + CREDENTIAL_PARK_BACKSTOP);
        let new_a = ScriptedProvider::new("p-a", &[Step::Ok]);
        let added = ScriptedProvider::new("p-a", &[Step::Ok]);
        let mut new = router_over(&[("m-a", "p-a", new_a), ("m-new", "p-a", added.clone())]);

        // Act
        new.carry_over_runtime_state_from(&old);
        let err = dispatch_err(&new, surface, to("m-new")).await;

        // Assert
        let label = format!("{surface:?}");
        assert!(
            matches!(
                err,
                Some(Error::Credential {
                    parked_until_ms: Some(_),
                    ..
                })
            ),
            "{label}: the new model is refused while the park is live, got {err:?}",
        );
        assert_eq!(calls(&added), 0, "{label}");
    }
}

/// When the attempt is settled, relative to a reload that advanced the
/// account's generation.
#[derive(Clone, Copy, Debug)]
enum Settle {
    /// A typed rejection on `m-a2` of an attempt admitted on `m-a`'s account.
    Rejection,
    /// A success on `m-a` while `m-a2` holds a park set after the admission.
    Success,
}

#[tokio::test]
async fn the_account_generation_fences_a_stale_settlement_on_any_model_of_the_account() {
    let rows = [
        ("rejection, no reload", Settle::Rejection, false, INVALID),
        (
            "rejection admitted before a reload",
            Settle::Rejection,
            true,
            None,
        ),
        (
            "success admitted before the park's generation",
            Settle::Success,
            true,
            INVALID,
        ),
        (
            "success in the park's generation",
            Settle::Success,
            false,
            None,
        ),
    ];
    for (name, settle, reload, expected) in rows {
        // Arrange: an attempt admitted on m-a under the account's generation.
        let fx = accounts(&[Step::Ok], &[Step::Ok], &[Step::Ok]);
        let admitted = fx
            .router
            .admit_for_credential_tests("m-a")
            .expect("a healthy account admits");
        if reload {
            fx.router.clear_all_credential_parks();
        }

        // Act
        match settle {
            Settle::Rejection => {
                fx.router.settle_credential_rejection_for_tests(
                    "m-a2",
                    &rejection("p-a", 403, INVALID),
                    admitted,
                );
            }
            Settle::Success => {
                let current = fx
                    .router
                    .admit_for_credential_tests("m-a2")
                    .expect("a healthy account admits");
                assert!(fx.router.settle_credential_rejection_for_tests(
                    "m-a2",
                    &rejection("p-a", 403, INVALID),
                    current,
                ));
                fx.router
                    .settle_seat_success_for_tests("m-a", true, admitted);
            }
        }

        // Assert: read through both models; one account, one answer.
        assert_eq!(park_reason(&fx.router, "m-a"), expected, "{name}");
        assert_eq!(park_reason(&fx.router, "m-a2"), expected, "{name}");
    }
}

#[tokio::test]
async fn a_background_probe_on_a_parked_account_is_refused_without_dialing_or_claiming() {
    use super::super::probe_test_support::{
        GROUNDED_PATH, OkProvider, lane, remote_router_with_lanes,
    };
    use crate::field_verdict::FieldVerdictKey;
    use crate::probe_scheduler::ProbeValidator;
    use std::sync::atomic::AtomicUsize;

    let now = Instant::now();
    let rows = [
        ("live park", Some(now + CREDENTIAL_PARK_BACKSTOP), 0),
        ("lapsed park", Some(now), 0),
        ("unparked control", None, 1),
    ];
    for (name, parked_until, expected_dials) in rows {
        // Arrange: m0 and m1 share one provider entry, so one account; the
        // probe targets m1's lane, the park was set through m0.
        let provider = Arc::new(OkProvider {
            count_calls: AtomicUsize::new(0),
        });
        let router = remote_router_with_lanes(provider.clone(), 2);
        if let Some(until) = parked_until {
            park(&router, "m0", until);
        }
        let key =
            FieldVerdictKey::new(&lane("m1"), GROUNDED_PATH, "anthropic-api").expect("identity");
        router.activate_probe_lane(&key, ProbeValidator::CountTokens);

        // Act
        let ran = router.run_due_probes().await;

        // Assert
        assert_eq!(ran, 1, "{name}: premise, the probe was leased and run");
        assert_eq!(
            provider.count_calls.load(Ordering::SeqCst),
            expected_dials,
            "{name}"
        );
        assert!(
            !router.gate_status_for_tests("m1").half_open_probe_in_flight,
            "{name}: a refused probe claims no half-open slot",
        );
    }
}

#[tokio::test]
async fn status_reports_the_account_park_on_every_target_of_the_account() {
    // Arrange
    let fx = accounts(&[Step::Ok], &[Step::Ok], &[Step::Ok]);
    park(&fx.router, "m-a", Instant::now() + CREDENTIAL_PARK_BACKSTOP);

    // Act
    let rows: BTreeMap<String, Option<UpstreamReason>> = fx
        .router
        .status_targets(Instant::now())
        .into_iter()
        .map(|t| (t.state_key, t.gate.credential_park.map(|p| p.reason)))
        .collect();

    // Assert: one row per target, no dedup.
    let expected: BTreeMap<String, Option<UpstreamReason>> = [
        ("m-a".to_string(), INVALID),
        ("m-a2".to_string(), INVALID),
        ("m-b".to_string(), None),
    ]
    .into_iter()
    .collect();
    assert_eq!(rows, expected);
}

#[tokio::test]
async fn the_sticky_snapshot_reads_every_seat_of_a_parked_account_as_blocked() {
    // Arrange: two pooled models over one two-member OAuth pool; the park is
    // set through the first model's seat on member `pool-a`.
    let members = ["pool-a", "pool-b"];
    let mut config = single_attempt_config();
    for member in members {
        config.providers.insert(
            member.into(),
            ProviderEntry::anthropic_api(format!("oauth://{member}")),
        );
    }
    config.pools.insert(
        "the-pool".into(),
        PoolEntry::new(members.iter().map(|m| (*m).to_string()).collect()),
    );
    let seats: Arc<[crate::seat_pool::SeatTarget]> = members
        .iter()
        .map(|member| crate::seat_pool::SeatTarget {
            provider_name: (*member).to_string(),
            provider: ScriptedProvider::new(member, &[Step::Ok]) as Arc<dyn Provider>,
            auth_secret_ref: Some(routectl_auth::SecretRef::OAuth {
                provider: (*member).to_string(),
                label: None,
            }),
        })
        .collect();
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    for nickname in ["pooled", "pooled2"] {
        models.insert(
            nickname.into(),
            Arc::new(
                ResolvedModel::new(nickname, "the-pool", seats[0].provider.clone(), "claude-x")
                    .with_seats(Arc::clone(&seats)),
            ),
        );
    }
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    park(
        &router,
        "pooled#pool-a",
        Instant::now() + CREDENTIAL_PARK_BACKSTOP,
    );

    // Act
    let snapshots = router.gather_capacity_snapshots(&seats, "pooled2", Instant::now());

    // Assert
    let blocked: Vec<bool> = snapshots.iter().map(|s| s.credential_blocked).collect();
    assert_eq!(
        blocked,
        [true, false],
        "only the parked account's seat is blocked"
    );
}

#[tokio::test]
async fn the_park_warn_names_the_account() {
    // Arrange
    let fx = accounts(&[Step::Reject(403, INVALID)], &[Step::Ok], &[Step::Ok]);

    // Act
    let (_, events) = routectl_testkit::with_capture(Box::pin(async {
        dispatch_err(&fx.router, Surface::Complete, to("m-a")).await
    }))
    .await;

    // Assert
    let warn = events
        .iter()
        .find(|e| e.message == "seat credential rejected; parking the seat")
        .expect("the park warns");
    assert_eq!(warn.level, tracing::Level::WARN);
    assert_eq!(warn.field("account"), Some("#p-a"));
}

#[test]
fn concurrent_admission_settlement_and_reads_across_one_account_never_deadlock() {
    const THREADS: usize = 8;
    const ROUNDS: usize = 2_000;
    // Arrange
    let fx = accounts(&[Step::Ok], &[Step::Ok], &[Step::Ok]);
    let router = Arc::new(fx.router);
    let (done_tx, done_rx) = std::sync::mpsc::channel();

    // Act: every thread mixes every account-touching path over both models
    // of the account.
    for t in 0..THREADS {
        let router = Arc::clone(&router);
        let done_tx = done_tx.clone();
        std::thread::spawn(move || {
            let model = if t % 2 == 0 { "m-a" } else { "m-a2" };
            for round in 0..ROUNDS {
                match (t + round) % 6 {
                    0 => {
                        let (_, mut guard) = router.admit_dispatch(model, "p-a");
                        guard.release();
                    }
                    1 => {
                        let generation = router.admit_for_credential_tests(model).unwrap_or(0);
                        router.settle_seat_success_for_tests(model, true, generation);
                    }
                    2 => {
                        let generation = router.admit_for_credential_tests(model).unwrap_or(0);
                        router.settle_credential_rejection_for_tests(
                            model,
                            &rejection("p-a", 403, INVALID),
                            generation,
                        );
                    }
                    3 => {
                        let _ = router.status_targets(Instant::now());
                    }
                    4 => {
                        router.clear_all_credential_parks();
                    }
                    _ => {
                        park(&router, model, Instant::now());
                    }
                }
            }
            let _ = done_tx.send(());
        });
    }
    drop(done_tx);

    // Assert
    for _ in 0..THREADS {
        done_rx
            .recv_timeout(Duration::from_mins(1))
            .expect("every thread finishes: no lock-order deadlock");
    }
}
