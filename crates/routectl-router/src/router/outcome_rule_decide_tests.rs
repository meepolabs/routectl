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

[providers.userinfo]
kind = "openai-compat"
api_key_ref = "env://C"
base_url = "https://user:sk-live-FAKE@gateway.example.com/v1"
"#;

fn seats() -> Config {
    toml::from_str(SEATS).expect("fixture config parses")
}

fn seat_a(cfg: &Config) -> WalkTarget<'_> {
    WalkTarget::fixture("seat-a", cfg.providers.get("seat-a"), "opus", 0)
}

fn feature_unsupported() -> FailureClass {
    FailureClass::FeatureUnsupported {
        capability: "thinking".into(),
    }
}

fn failure<'a>(err: &'a Error, class: &'a FailureClass, target: WalkTarget<'a>) -> Failure<'a> {
    Failure {
        err,
        native_class: class,
        class,
        native_capability_typed: false,
        is_probe: false,
        surface: DispatchSurface::Complete,
        deadline_fired: false,
        disable_fallbacks: false,
        target,
    }
}

fn probe<'a>(err: &'a Error, class: &'a FailureClass, target: WalkTarget<'a>) -> Failure<'a> {
    Failure {
        is_probe: true,
        ..failure(err, class, target)
    }
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
    Target,
    Endpoint,
    Model,
}

fn excludes(disposition: &Disposition, target: &WalkTarget<'_>) -> Excludes {
    match disposition.exclusions.as_slice() {
        [] => Excludes::Nothing,
        [Exclusion::Target(key)] if key == target.state_key() => Excludes::Target,
        [Exclusion::Endpoint(key)] if *key == target.endpoint_key() => Excludes::Endpoint,
        [Exclusion::Model(model)] if model == target.model() => Excludes::Model,
        other => panic!("unexpected exclusions {other:?}"),
    }
}

/// Every scope source, one row each. A row's retry column is the verdict on
/// a fresh walk under the default policy, so a retryable target-scoped row
/// retries and every wider or non-retryable row does not. A target-scoped
/// failure excludes the failed seat from the rest of the walk; the account
/// scope leaves that to the credential park.
#[test]
fn each_scope_source_sets_its_scope_retry_and_exclusion() {
    use Excludes::{Endpoint, Model, Nothing, Target};
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
            "500",
            Error::upstream("p", 500, "x"),
            FailureClass::ServerError,
            FailureScope::Target,
            true,
            Target,
        ),
        (
            "529",
            Error::upstream("p", 529, "x"),
            FailureClass::Overloaded,
            FailureScope::Target,
            true,
            Target,
        ),
        (
            "network (status 0)",
            Error::upstream("p", 0, ""),
            FailureClass::NetworkError,
            FailureScope::Target,
            true,
            Target,
        ),
        (
            "unrecognized 429",
            Error::upstream("p", 429, "rate limited"),
            FailureClass::RateLimited,
            FailureScope::Target,
            true,
            Target,
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
            Target,
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

/// The probe overlay, for every failure kind: no same-target retry, and the
/// failed seat and every target outside the probe's chain run excluded, on
/// top of whatever the scope itself excludes. The debit is the shared rule's: only a native
/// 429 or 529 spares the breaker; a probe's 5xx or network failure debits
/// like any other request's, and a request-rejecting 400 debits nothing.
#[test]
fn a_probe_never_retries_stays_in_its_run_and_takes_the_shared_debit() {
    use BreakerOutcome::{Debit, None as Untouched};
    let cfg = seats();
    let target = seat_a(&cfg);
    let policy = RetryPolicy::default();
    let rows: Vec<(&str, Error, FailureClass, BreakerOutcome)> = vec![
        (
            "429",
            Error::upstream("p", 429, "x"),
            FailureClass::RateLimited,
            Untouched,
        ),
        (
            "529",
            Error::upstream("p", 529, "x"),
            FailureClass::Overloaded,
            Untouched,
        ),
        (
            "500 debits",
            Error::upstream("p", 500, "x"),
            FailureClass::ServerError,
            Debit,
        ),
        (
            "503 overloaded debits",
            Error::upstream("p", 503, "x"),
            FailureClass::Overloaded,
            Debit,
        ),
        (
            "network (status 0) debits",
            Error::upstream("p", 0, ""),
            FailureClass::NetworkError,
            Debit,
        ),
        (
            "400 generic",
            Error::upstream("p", 400, "x"),
            FailureClass::BadRequest,
            Untouched,
        ),
        (
            "400 feature",
            Error::upstream("p", 400, "x"),
            feature_unsupported(),
            Untouched,
        ),
        (
            "recognized bare 429",
            bare_rate_limit(),
            FailureClass::RateLimited,
            Untouched,
        ),
        (
            "credential reason",
            Error::upstream("p", 401, "x")
                .with_upstream_reason(Some(UpstreamReason::CredentialExpired)),
            FailureClass::Auth,
            Untouched,
        ),
        (
            "429 with a large reset hint",
            Error::upstream_with_retry_after("p", 429, "x", Some(Duration::from_hours(1))),
            FailureClass::RateLimited,
            Untouched,
        ),
    ];
    for (name, err, class, breaker) in rows {
        let hint = rate_limit_reset_hint(&err, &policy);

        let disposition = decide(
            &probe(&err, &class, target),
            &policy,
            hint,
            &RequestWalk::new(),
        );

        assert_eq!(disposition.same_target, SameProvider::No, "{name}");
        assert_eq!(disposition.breaker, breaker, "{name}");
        for fence in Exclusion::probe_fence(&target) {
            assert!(
                disposition.exclusions.contains(&fence),
                "{name}: {fence:?} in {:?}",
                disposition.exclusions
            );
        }
    }
}

/// The paired control for the probe rows: the same 500 off a non-probe
/// request retries, debits and sets no pool fence; it excludes only the
/// failed seat.
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
    assert_eq!(
        disposition.exclusions,
        vec![Exclusion::Target(seat_a(&cfg).state_key().to_owned())]
    );
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
            (false, Next::Walk, Excludes::Target),
        ),
        (
            "cap 3 with 1 retry spent retries once more",
            &server,
            FailureClass::ServerError,
            with_class(ConfigFailureClass::ServerError, Some(3), None),
            1,
            (true, Next::Walk, Excludes::Target),
        ),
        (
            "cap 3 with 2 retries spent on earlier targets stops",
            &server,
            FailureClass::ServerError,
            with_class(ConfigFailureClass::ServerError, Some(3), None),
            2,
            (false, Next::Walk, Excludes::Target),
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

/// An unknown provider reaching the decision is classified Unknown and,
/// like any Unknown-class failure, ends the request without a retry.
#[test]
fn an_unknown_provider_error_is_terminal() {
    let cfg = seats();
    let err = Error::UnknownProvider("absent".into());
    let class = routectl_core::failure_class::classify(&err, None).class;

    let disposition = decide_fresh(
        &failure(&err, &class, seat_a(&cfg)),
        &RetryPolicy::default(),
    );

    assert_eq!(class, FailureClass::Unknown);
    assert_eq!(disposition.scope, FailureScope::Target);
    assert_eq!(disposition.same_target, SameProvider::No);
    assert_eq!(disposition.next, Next::Terminal);
}

/// A failure's Debug prints its target's provider name, `[models]` entry and
/// run, never the credential bytes the provider entry's base URL carries.
#[test]
fn a_failure_debug_never_prints_base_url_credentials() {
    let cfg = seats();
    let target = WalkTarget::fixture("userinfo", cfg.providers.get("userinfo"), "opus", 0);
    let err = Error::upstream("p", 500, "x");
    let class = FailureClass::ServerError;

    let printed = format!("{:?}", failure(&err, &class, target));

    assert!(!printed.contains("sk-live-FAKE"), "{printed}");
    assert!(printed.contains("userinfo"), "{printed}");
}

/// The baked Unknown-class policy ends the request.
#[test]
fn an_unknown_class_is_terminal() {
    let cfg = seats();
    let err = Error::upstream("p", 302, "x");

    let disposition = decide_fresh(
        &failure(&err, &FailureClass::Unknown, seat_a(&cfg)),
        &RetryPolicy::default(),
    );

    assert_eq!(disposition.next, Next::Terminal);
    assert_eq!(disposition.same_target, SameProvider::No);
}

/// Only a completion retries the same target. A health error walks on
/// the message surfaces; a count walks on one only when the attempt's own
/// deadline fired, and otherwise ends the request.
#[test]
fn surfaces_retry_and_walk_by_their_own_rules() {
    let cfg = seats();
    let server = Error::upstream("p", 500, "x");
    let network = Error::upstream("p", 0, "timed out");
    let policy = RetryPolicy::default();
    let rows = [
        (
            "complete 500",
            DispatchSurface::Complete,
            &server,
            FailureClass::ServerError,
            false,
            true,
            Next::Walk,
        ),
        (
            "stream 500",
            DispatchSurface::Stream,
            &server,
            FailureClass::ServerError,
            false,
            false,
            Next::Walk,
        ),
        (
            "count_tokens 500",
            DispatchSurface::CountTokens,
            &server,
            FailureClass::ServerError,
            false,
            false,
            Next::Terminal,
        ),
        (
            "count_tokens network",
            DispatchSurface::CountTokens,
            &network,
            FailureClass::NetworkError,
            false,
            false,
            Next::Terminal,
        ),
        (
            "count_tokens deadline fired",
            DispatchSurface::CountTokens,
            &network,
            FailureClass::Timeout,
            true,
            false,
            Next::Walk,
        ),
    ];
    for (name, surface, err, class, deadline_fired, retries, next) in rows {
        let failure = Failure {
            surface,
            deadline_fired,
            ..failure(err, &class, seat_a(&cfg))
        };

        let disposition = decide(&failure, &policy, None, &RequestWalk::new());

        assert_eq!(
            matches!(disposition.same_target, SameProvider::Retry { .. }),
            retries,
            "{name}"
        );
        assert_eq!(disposition.next, next, "{name}");
    }
}

/// With fallbacks disabled the first failure ends the request: no
/// same-target retry and no walk, even for a failure that would do both.
#[test]
fn disabled_fallbacks_end_the_request_at_the_first_failure() {
    let cfg = seats();
    let err = Error::upstream("p", 500, "x");
    let class = FailureClass::ServerError;
    let policy = RetryPolicy::default();
    let rows = [
        ("fallbacks enabled", false, true, Next::Walk),
        ("fallbacks disabled", true, false, Next::Terminal),
    ];
    for (name, disable_fallbacks, retries, next) in rows {
        let failure = Failure {
            disable_fallbacks,
            ..failure(&err, &class, seat_a(&cfg))
        };

        let disposition = decide(&failure, &policy, None, &RequestWalk::new());

        assert_eq!(
            matches!(disposition.same_target, SameProvider::Retry { .. }),
            retries,
            "{name}"
        );
        assert_eq!(disposition.next, next, "{name}");
    }
}

/// The scope reads the class the classifier gave, not the operator's
/// remap: a generic 400 remapped to feature-unsupported or to a retryable
/// class stays endpoint scope and is not retried, and a native
/// feature-unsupported 400 remapped to bad-request stays model scope.
#[test]
fn the_scope_reads_the_native_class_before_any_remap() {
    use Excludes::{Endpoint, Model};
    let cfg = seats();
    let target = seat_a(&cfg);
    let err = Error::upstream("p", 400, "x");
    let feature = feature_unsupported();
    let rows = [
        (
            "generic 400 remapped to feature-unsupported",
            FailureClass::BadRequest,
            feature.clone(),
            FailureScope::Endpoint,
            Endpoint,
        ),
        (
            "generic 400 remapped to server-error",
            FailureClass::BadRequest,
            FailureClass::ServerError,
            FailureScope::Endpoint,
            Endpoint,
        ),
        (
            "feature 400 remapped to bad-request",
            feature.clone(),
            FailureClass::BadRequest,
            FailureScope::Model,
            Model,
        ),
    ];
    for (name, native, remapped, scope, exclusion) in rows {
        let failure = Failure {
            native_class: &native,
            ..failure(&err, &remapped, target)
        };

        let disposition = decide_fresh(&failure, &RetryPolicy::default());

        assert_eq!(disposition.scope, scope, "{name}");
        assert_eq!(disposition.same_target, SameProvider::No, "{name}");
        assert_eq!(excludes(&disposition, &target), exclusion, "{name}");
    }
}

/// A generic native 400 is scoped to the model only when the capability
/// resolver named a capability in it: a resolver hit narrows it to the
/// `[models]` entry, and without one it keeps endpoint scope, even when an
/// operator remap calls it an unsupported feature.
#[test]
fn a_bad_request_is_model_scoped_only_on_a_resolver_hit() {
    use Excludes::{Endpoint, Model};
    let cfg = seats();
    let target = seat_a(&cfg);
    let err = Error::upstream("p", 400, "x");
    let bad_request = FailureClass::BadRequest;
    let feature = feature_unsupported();
    let rows = [
        (
            "resolver hit",
            &bad_request,
            true,
            FailureScope::Model,
            Model,
        ),
        (
            "no resolver hit",
            &bad_request,
            false,
            FailureScope::Endpoint,
            Endpoint,
        ),
        (
            "remapped to feature-unsupported, no resolver hit",
            &feature,
            false,
            FailureScope::Endpoint,
            Endpoint,
        ),
    ];
    for (name, remapped, native_capability_typed, scope, exclusion) in rows {
        let failure = Failure {
            native_class: &bad_request,
            native_capability_typed,
            ..failure(&err, remapped, target)
        };

        let disposition = decide_fresh(&failure, &RetryPolicy::default());

        assert_eq!(disposition.scope, scope, "{name}");
        assert_eq!(disposition.same_target, SameProvider::No, "{name}");
        assert_eq!(excludes(&disposition, &target), exclusion, "{name}");
    }
}

/// A model-scoped failure excludes the `[models]` entry, not the upstream
/// id: a second entry sending the same upstream to the same origin (a
/// larger-window variant gated by a beta header) stays reachable.
#[test]
fn a_context_window_400_leaves_another_entry_with_the_same_upstream_admitted() {
    let cfg = seats();
    let failed = seat_a(&cfg);
    let same_entry_sibling = WalkTarget::fixture("seat-b", cfg.providers.get("seat-b"), "opus", 0);
    let wide_window = WalkTarget::fixture("seat-a", cfg.providers.get("seat-a"), "opus-1m", 2);
    let err = Error::upstream("p", 400, "prompt is too long");
    let class = FailureClass::ContextWindow;
    let mut walk = RequestWalk::new();

    let disposition = decide_fresh(&failure(&err, &class, failed), &RetryPolicy::default());
    for exclusion in disposition.exclusions {
        walk.exclude(exclusion);
    }

    assert!(
        !walk.admits(&same_entry_sibling),
        "the failed entry's seats are excluded"
    );
    assert!(
        walk.admits(&wide_window),
        "the larger-window entry stays admitted"
    );
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
    let sibling = WalkTarget::fixture("seat-b", cfg.providers.get("seat-b"), "opus", 0);
    let other_origin = WalkTarget::fixture("gateway", cfg.providers.get("gateway"), "opus", 0);
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
