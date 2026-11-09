//! The one failure decision: scope sources, the probe overlay, the
//! narrow-only `[retry.classes]` overlay, the surface rules and the debit.

use std::time::Duration;

use routectl_core::failure_class::FailureClass;
use routectl_core::{Error, FailureScope, RefusalReason, ScopeRow, UpstreamReason};

use super::*;
use crate::class_policy::{ClassPolicy, ConfigFailureClass};
use crate::config::{Config, RetryPolicy};
use crate::router::walk_state::{Exclusion, RequestWalk, WalkTarget};

const SEATS: &str = r#"
[providers.seat-a]
kind = "anthropic-api"
api_key_ref = "env://A"

[providers.seat-b]
kind = "anthropic-api"
api_key_ref = "env://B"

[providers.gateway]
kind = "openai-compat"
api_key_ref = "env://C"
base_url = "https://gateway.example.com/v1"
"#;

fn seats() -> Config {
    toml::from_str(SEATS).expect("fixture config parses")
}

fn seat_a(cfg: &Config) -> WalkTarget<'_> {
    WalkTarget {
        provider_name: "seat-a",
        entry: cfg.providers.get("seat-a"),
        upstream: "opus",
        pool: "pool-a",
    }
}

fn feature_unsupported() -> FailureClass {
    FailureClass::FeatureUnsupported {
        capability: "thinking".into(),
    }
}

fn failure<'a>(err: &'a Error, class: &'a FailureClass, target: WalkTarget<'a>) -> Failure<'a> {
    Failure::of(err, class, target, DispatchSurface::Complete, false, false)
}

fn decide_fresh(failure: &Failure<'_>, policy: &RetryPolicy) -> Disposition {
    decide(failure, policy, None, &RequestWalk::new())
}

fn bare_rate_limit() -> Error {
    Error::upstream("p", 429, "Error").with_scope_row(Some(ScopeRow::AnthropicBareRateLimit))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Excludes {
    Nothing,
    Endpoint,
    Model,
}

fn excludes(disposition: &Disposition, target: &WalkTarget<'_>) -> Excludes {
    match disposition.exclusions.as_slice() {
        [] => Excludes::Nothing,
        [Exclusion::Endpoint(key)] if *key == target.endpoint_key() => Excludes::Endpoint,
        [Exclusion::Model { endpoint, upstream }]
            if *endpoint == target.endpoint_key() && upstream == target.upstream =>
        {
            Excludes::Model
        }
        other => panic!("unexpected exclusions {other:?}"),
    }
}

/// Every scope source, one row each. A row's retry column is the verdict on
/// a fresh walk under the default policy, so a retryable target-scoped row
/// retries and every wider or non-retryable row does not.
#[test]
fn each_scope_source_sets_its_scope_retry_and_exclusion() {
    use Excludes::{Endpoint, Model, Nothing};
    let cfg = seats();
    let target = seat_a(&cfg);
    let policy = RetryPolicy::default();
    let rows: Vec<(&str, Error, FailureClass, FailureScope, bool, Excludes)> = vec![
        (
            "recognized bare 429",
            bare_rate_limit(),
            FailureClass::RateLimited,
            FailureScope::Endpoint,
            false,
            Endpoint,
        ),
        (
            "native 400 feature unsupported",
            Error::upstream("p", 400, "x"),
            feature_unsupported(),
            FailureScope::Model,
            false,
            Model,
        ),
        (
            "native 422 context window",
            Error::upstream("p", 422, "x"),
            FailureClass::ContextWindow,
            FailureScope::Model,
            false,
            Model,
        ),
        (
            "native 400 generic",
            Error::upstream("p", 400, "x"),
            FailureClass::BadRequest,
            FailureScope::Endpoint,
            false,
            Endpoint,
        ),
        (
            "native 422 generic",
            Error::upstream("p", 422, "x"),
            FailureClass::BadRequest,
            FailureScope::Endpoint,
            false,
            Endpoint,
        ),
        (
            "native 400 remapped to a retryable class",
            Error::upstream("p", 400, "x"),
            FailureClass::ServerError,
            FailureScope::Endpoint,
            false,
            Endpoint,
        ),
        (
            "500",
            Error::upstream("p", 500, "x"),
            FailureClass::ServerError,
            FailureScope::Target,
            true,
            Nothing,
        ),
        (
            "529",
            Error::upstream("p", 529, "x"),
            FailureClass::Overloaded,
            FailureScope::Target,
            true,
            Nothing,
        ),
        (
            "network (status 0)",
            Error::upstream("p", 0, ""),
            FailureClass::NetworkError,
            FailureScope::Target,
            true,
            Nothing,
        ),
        (
            "unrecognized 429",
            Error::upstream("p", 429, "rate limited"),
            FailureClass::RateLimited,
            FailureScope::Target,
            true,
            Nothing,
        ),
        (
            "typed credential reason",
            Error::upstream("p", 401, "x")
                .with_upstream_reason(Some(UpstreamReason::CredentialInvalid)),
            FailureClass::Auth,
            FailureScope::Account,
            false,
            Nothing,
        ),
        (
            "local refusal",
            Error::Validation("x".into(), RefusalReason::SchemaDepth),
            FailureClass::BadRequest,
            FailureScope::Target,
            false,
            Nothing,
        ),
    ];
    for (name, err, class, scope, retries, exclusion) in rows {
        let disposition = decide_fresh(&failure(&err, &class, target), &policy);

        assert_eq!(disposition.scope, scope, "{name}");
        assert_eq!(
            matches!(disposition.same_target, SameProvider::Retry { .. }),
            retries,
            "{name}"
        );
        assert_eq!(excludes(&disposition, &target), exclusion, "{name}");
    }
}

/// Every scope this build knows passes through; the fold to target exists
/// for a scope added later, which no build can construct before it exists.
#[test]
fn every_known_scope_passes_through_the_unknown_scope_fold() {
    for scope in [
        FailureScope::Target,
        FailureScope::Account,
        FailureScope::Endpoint,
        FailureScope::Model,
    ] {
        assert_eq!(known_scope(scope), Some(scope), "{scope:?}");
    }
}

/// A recognized row records itself on the disposition, so the walk can
/// report which pattern widened the scope.
#[test]
fn a_recognized_row_is_carried_on_the_disposition() {
    let cfg = seats();
    let err = bare_rate_limit();
    let class = FailureClass::RateLimited;

    let disposition = decide_fresh(
        &failure(&err, &class, seat_a(&cfg)),
        &RetryPolicy::default(),
    );

    assert_eq!(
        disposition.recognized,
        Some(ScopeRow::AnthropicBareRateLimit)
    );
}

/// The probe overlay, for every failure kind: no same-target retry, no
/// debit, and every target outside the probe's pool excluded, on top of
/// whatever the scope itself excludes.
#[test]
fn a_probe_never_retries_never_debits_and_stays_in_its_pool() {
    let cfg = seats();
    let target = seat_a(&cfg);
    let policy = RetryPolicy::default();
    let rows: Vec<(&str, Error, FailureClass)> = vec![
        (
            "429",
            Error::upstream("p", 429, "x"),
            FailureClass::RateLimited,
        ),
        (
            "529",
            Error::upstream("p", 529, "x"),
            FailureClass::Overloaded,
        ),
        (
            "500",
            Error::upstream("p", 500, "x"),
            FailureClass::ServerError,
        ),
        (
            "503 overloaded",
            Error::upstream("p", 503, "x"),
            FailureClass::Overloaded,
        ),
        (
            "network",
            Error::upstream("p", 0, ""),
            FailureClass::NetworkError,
        ),
        (
            "400 generic",
            Error::upstream("p", 400, "x"),
            FailureClass::BadRequest,
        ),
        (
            "400 feature",
            Error::upstream("p", 400, "x"),
            feature_unsupported(),
        ),
        (
            "recognized bare 429",
            bare_rate_limit(),
            FailureClass::RateLimited,
        ),
        (
            "credential reason",
            Error::upstream("p", 401, "x")
                .with_upstream_reason(Some(UpstreamReason::CredentialExpired)),
            FailureClass::Auth,
        ),
        (
            "large reset hint",
            Error::upstream_with_retry_after("p", 429, "x", Some(Duration::from_hours(1))),
            FailureClass::RateLimited,
        ),
    ];
    for (name, err, class) in rows {
        let probe = Failure::of(&err, &class, target, DispatchSurface::Complete, true, false);
        let hint = rate_limit_reset_hint(&err, &policy);

        let disposition = decide(&probe, &policy, hint, &RequestWalk::new());

        assert_eq!(disposition.same_target, SameProvider::No, "{name}");
        assert_eq!(disposition.breaker, BreakerOutcome::None, "{name}");
        assert!(
            disposition
                .exclusions
                .contains(&Exclusion::OutsidePool("pool-a".into())),
            "{name}: {:?}",
            disposition.exclusions
        );
    }
}

/// The paired control for the probe rows: the same 500 off a non-probe
/// request retries, debits and sets no pool fence.
#[test]
fn a_non_probe_500_retries_debits_and_sets_no_pool_fence() {
    let cfg = seats();
    let err = Error::upstream("p", 500, "x");
    let class = FailureClass::ServerError;

    let disposition = decide_fresh(
        &failure(&err, &class, seat_a(&cfg)),
        &RetryPolicy::default(),
    );

    assert!(matches!(
        disposition.same_target,
        SameProvider::Retry { .. }
    ));
    assert_eq!(disposition.breaker, BreakerOutcome::Debit);
    assert!(disposition.exclusions.is_empty());
}

fn with_class(
    class: ConfigFailureClass,
    retry: Option<u32>,
    fallback: Option<bool>,
) -> RetryPolicy {
    let mut policy = RetryPolicy::default();
    policy
        .classes
        .insert(class, ClassPolicy { retry, fallback });
    policy
}

/// Name, error, class, policy, retries already spent, then the expected
/// retry verdict, next step and exclusion.
type NarrowRow<'a> = (
    &'a str,
    &'a Error,
    FailureClass,
    RetryPolicy,
    u32,
    (bool, Next, Excludes),
);

/// `[retry.classes]` only narrows. Its retry cap bounds the request-wide
/// count; `fallback = false` ends the walk; and neither re-enables a
/// same-target retry or lifts an exclusion the scope rule set.
#[test]
fn retry_classes_only_narrow_the_scope_rule() {
    let cfg = seats();
    let target = seat_a(&cfg);
    let bad_request = Error::upstream("p", 400, "x");
    let server = Error::upstream("p", 500, "x");
    let unrecognized_429 = Error::upstream("p", 429, "x");
    let bare = bare_rate_limit();
    let rows: Vec<NarrowRow<'_>> = vec![
        (
            "fallback = false makes a generic 400 terminal and keeps its exclusion",
            &bad_request,
            FailureClass::BadRequest,
            with_class(ConfigFailureClass::BadRequest, None, Some(false)),
            0,
            (false, Next::Terminal, Excludes::Endpoint),
        ),
        (
            "a raised retry cap does not retry an endpoint-scoped 400",
            &bad_request,
            FailureClass::BadRequest,
            with_class(ConfigFailureClass::BadRequest, Some(9), Some(true)),
            0,
            (false, Next::Walk, Excludes::Endpoint),
        ),
        (
            "a raised retry cap does not retry a recognized row",
            &bare,
            FailureClass::RateLimited,
            with_class(ConfigFailureClass::RateLimited, Some(9), Some(true)),
            0,
            (false, Next::Walk, Excludes::Endpoint),
        ),
        (
            "fallback = false on a recognized row keeps its exclusion",
            &bare,
            FailureClass::RateLimited,
            with_class(ConfigFailureClass::RateLimited, None, Some(false)),
            0,
            (false, Next::Terminal, Excludes::Endpoint),
        ),
        (
            "retry = 0 stops a retryable 429",
            &unrecognized_429,
            FailureClass::RateLimited,
            with_class(ConfigFailureClass::RateLimited, Some(0), None),
            0,
            (false, Next::Walk, Excludes::Nothing),
        ),
        (
            "cap 3 with 1 retry spent retries once more",
            &server,
            FailureClass::ServerError,
            with_class(ConfigFailureClass::ServerError, Some(3), None),
            1,
            (true, Next::Walk, Excludes::Nothing),
        ),
        (
            "cap 3 with 2 retries spent on earlier targets stops",
            &server,
            FailureClass::ServerError,
            with_class(ConfigFailureClass::ServerError, Some(3), None),
            2,
            (false, Next::Walk, Excludes::Nothing),
        ),
    ];
    for (name, err, class, policy, spent, (retries, next, exclusion)) in rows {
        let mut walk = RequestWalk::new();
        for _ in 0..spent {
            walk.spend_retry();
        }

        let disposition = decide(&failure(err, &class, target), &policy, None, &walk);

        assert_eq!(
            matches!(disposition.same_target, SameProvider::Retry { .. }),
            retries,
            "{name}"
        );
        assert_eq!(disposition.next, next, "{name}");
        assert_eq!(excludes(&disposition, &target), exclusion, "{name}");
    }
}

#[test]
fn an_unknown_class_is_terminal_and_an_unknown_provider_walks() {
    let cfg = seats();
    let target = seat_a(&cfg);
    let policy = RetryPolicy::default();
    let unknown_status = Error::upstream("p", 302, "x");
    let unknown_provider = Error::UnknownProvider("ghost".into());
    let rows = [
        ("unknown class", &unknown_status, Next::Terminal),
        ("unknown provider", &unknown_provider, Next::Walk),
    ];
    for (name, err, next) in rows {
        let disposition = decide_fresh(&failure(err, &FailureClass::Unknown, target), &policy);

        assert_eq!(disposition.next, next, "{name}");
        assert_eq!(disposition.same_target, SameProvider::No, "{name}");
    }
}

/// Only a completion retries the same target; a committed stream also
/// never moves to another target. The uncommitted stream row is the
/// control that a stream failure still walks.
#[test]
fn surfaces_never_retry_and_a_committed_stream_never_falls_back() {
    let cfg = seats();
    let err = Error::upstream("p", 500, "x");
    let class = FailureClass::ServerError;
    let policy = RetryPolicy::default();
    let rows = [
        (
            "complete",
            DispatchSurface::Complete,
            false,
            true,
            Next::Walk,
        ),
        ("stream", DispatchSurface::Stream, false, false, Next::Walk),
        (
            "count_tokens",
            DispatchSurface::CountTokens,
            false,
            false,
            Next::Walk,
        ),
        (
            "committed stream",
            DispatchSurface::Stream,
            true,
            false,
            Next::Terminal,
        ),
    ];
    for (name, surface, committed, retries, next) in rows {
        let failure = Failure::of(&err, &class, seat_a(&cfg), surface, false, committed);

        let disposition = decide(&failure, &policy, None, &RequestWalk::new());

        assert_eq!(
            matches!(disposition.same_target, SameProvider::Retry { .. }),
            retries,
            "{name}"
        );
        assert_eq!(disposition.next, next, "{name}");
    }
}

/// The debit keys on the native status: a native 400/422 never debits
/// whatever class an operator remaps it to, while the same remap on a 5xx
/// still debits (the control that the remapped class is a debiting one).
#[test]
fn a_native_400_never_debits_whatever_its_remapped_class() {
    let cfg = seats();
    let policy = RetryPolicy::default();
    let rows = [
        (
            "400 remapped to server-error",
            400,
            FailureClass::ServerError,
            BreakerOutcome::None,
        ),
        (
            "400 remapped to rate-limited",
            400,
            FailureClass::RateLimited,
            BreakerOutcome::None,
        ),
        (
            "422 remapped to overloaded",
            422,
            FailureClass::Overloaded,
            BreakerOutcome::None,
        ),
        (
            "503 as server-error",
            503,
            FailureClass::ServerError,
            BreakerOutcome::Debit,
        ),
    ];
    for (name, status, class, expected) in rows {
        let err = Error::upstream("p", status, "x");

        let disposition = decide_fresh(&failure(&err, &class, seat_a(&cfg)), &policy);

        assert_eq!(disposition.breaker, expected, "{name}");
        assert_eq!(
            breaker_outcome(&err, &class, None, false),
            expected,
            "{name}: shared rule"
        );
    }
}

/// A recognized bare 429 debits its target once and is never retried: its
/// endpoint exclusion covers the target that failed, so the walk cannot
/// dial it, or a sibling seat on the same origin, again.
#[test]
fn a_recognized_bare_429_debits_once_and_is_not_retried() {
    let cfg = seats();
    let target = seat_a(&cfg);
    let sibling = WalkTarget {
        provider_name: "seat-b",
        entry: cfg.providers.get("seat-b"),
        ..target
    };
    let other_origin = WalkTarget {
        provider_name: "gateway",
        entry: cfg.providers.get("gateway"),
        ..target
    };
    let err = bare_rate_limit();
    let class = FailureClass::RateLimited;
    let mut walk = RequestWalk::new();

    let disposition = decide(
        &failure(&err, &class, target),
        &RetryPolicy::default(),
        None,
        &walk,
    );
    for exclusion in disposition.exclusions.clone() {
        walk.exclude(exclusion);
    }

    assert_eq!(disposition.breaker, BreakerOutcome::Debit);
    assert_eq!(disposition.same_target, SameProvider::No);
    assert!(
        !walk.admits(&target),
        "the debited target is not dialed again"
    );
    assert!(
        !walk.admits(&sibling),
        "nor its sibling seat on the same origin"
    );
    assert!(walk.admits(&other_origin), "another origin stays reachable");
}

/// A small reset hint becomes the retry's sleep floor; a hint over the
/// in-loop cap parks the seat instead of retrying it.
#[test]
fn a_reset_hint_floors_the_retry_or_parks_the_seat() {
    let cfg = seats();
    let class = FailureClass::RateLimited;
    let small = Duration::from_secs(2);
    let large = Duration::from_hours(1);
    let rows = [
        (
            "small hint",
            small,
            SameProvider::Retry {
                sleep_floor: Some(small),
            },
            BreakerOutcome::Debit,
        ),
        (
            "large hint",
            large,
            SameProvider::No,
            BreakerOutcome::Park(large),
        ),
    ];
    for (name, hint, retry, breaker) in rows {
        let err = Error::upstream_with_retry_after("p", 429, "x", Some(hint));

        let disposition = decide(
            &failure(&err, &class, seat_a(&cfg)),
            &RetryPolicy::default(),
            Some(hint),
            &RequestWalk::new(),
        );

        assert_eq!(disposition.same_target, retry, "{name}");
        assert_eq!(disposition.breaker, breaker, "{name}");
    }
}
