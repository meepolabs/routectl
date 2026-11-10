//! The failure-outcome rule shared by the dispatch surfaces: [`decide`] maps
//! one failed attempt to everything it does (the breaker outcome, a
//! same-target retry, the request-scoped exclusions, and whether the walk
//! continues). Pure decisions only; the dispatch walk applies a
//! [`Disposition`], the Router applies a `BreakerOutcome` in `runtime_gate`,
//! and probe-slot settlement stays at each call site.

use std::time::Duration;

use routectl_core::failure_class::FailureClass;
use routectl_core::{Error, FailureScope, ScopeRow};

use crate::config::RetryPolicy;

use super::class_observe::DispatchSurface;
use super::walk_state::{Exclusion, RequestWalk, WalkTarget};

/// The largest upstream reset hint we honor as an in-loop, same-provider
/// retry sleep (blocking the request thread). A reset at or below this
/// cap is folded into the next backoff sleep; a larger reset parks the
/// provider via the breaker instead, so the request falls over to a
/// sibling rather than blocking on a multi-minute (or hostile) hint.
pub(super) const INLOOP_RETRY_AFTER_CAP: Duration = Duration::from_secs(5);

/// What a failed attempt does to the seat's circuit breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum BreakerOutcome {
    /// The class does not debit: breaker health is untouched.
    None,
    /// A threshold-gated failure debit (which may open the breaker).
    Debit,
    /// Force the breaker open for the honored reset.
    Park(Duration),
}

/// The breaker outcome for `err`, classified as `class`, carrying
/// `reset_hint`. A native 400 or 422 never debits, whatever class an
/// operator remap gives it: the request, not the seat, caused it. A probe's
/// native 429 or 529 leaves the breaker alone whatever its class: a quota
/// check hitting a rate limit or an overload is a transient upstream
/// condition, not a provider fault. Otherwise a non-debiting class leaves the
/// breaker alone, and a debiting class parks the seat only when a non-probe
/// attempt carries a reset LARGER than [`INLOOP_RETRY_AFTER_CAP`]; a smaller
/// (or absent) reset, and every other probe failure, takes the
/// threshold-gated debit instead.
pub(super) fn breaker_outcome(
    err: &Error,
    class: &FailureClass,
    reset_hint: Option<Duration>,
    is_probe: bool,
) -> BreakerOutcome {
    let native = native_status(err);
    if is_request_rejection(native)
        || (is_probe && matches!(native, Some(429 | 529)))
        || !class_debits(class)
    {
        return BreakerOutcome::None;
    }
    match reset_hint {
        Some(h) if !is_probe && h > INLOOP_RETRY_AFTER_CAP => BreakerOutcome::Park(h),
        _ => BreakerOutcome::Debit,
    }
}

/// The HTTP status the upstream itself answered with, before any operator
/// remap; `None` for a transport failure (status 0) or a local error.
pub(super) const fn native_status(err: &Error) -> Option<u16> {
    match err {
        Error::Upstream { status, .. } if *status != 0 => Some(*status),
        _ => None,
    }
}

/// Whether `native` is a status by which the upstream rejected the request
/// itself (400, 422) rather than reporting its own health.
const fn is_request_rejection(native: Option<u16>) -> bool {
    matches!(native, Some(400 | 422))
}

/// Whether a failed attempt retries the same target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SameProvider {
    No,
    /// Retry the same provider; `sleep_floor` is the upstream reset the
    /// next backoff sleep must wait at least, when one is honored in-loop.
    Retry {
        sleep_floor: Option<Duration>,
    },
}

/// The honored rate-limit reset for `err`, clamped to the configured
/// ceiling, or `None` when `err` carries no reset hint. A reset hint is
/// present ONLY on a rate-limit/overload (429/503/529) where the
/// upstream told us when it resets (`Retry-After`, or the Codex
/// `usage_limit_reached` `resets_at` / `resets_in_seconds`), so its
/// presence is itself the rate-limit signal. The clamp bounds both the
/// in-loop honored sleep and the breaker park to
/// `RetryPolicy::max_honored_retry_after` so a hostile or buggy upstream
/// cannot pin a provider open indefinitely. A zero-duration hint (e.g.
/// `Retry-After: 0`, a past HTTP-date, or a saturated `resets_at`) yields
/// `None` so it falls through to the normal threshold-gated debit instead
/// of a degenerate zero-length park (a `force_open(.., ZERO)` would leave
/// the breaker stuck half-open without ever tripping).
pub(super) fn rate_limit_reset_hint(err: &Error, policy: &RetryPolicy) -> Option<Duration> {
    reset_hint_clamped_to(err, policy.max_honored_retry_after())
}

/// [`rate_limit_reset_hint`] against an already-resolved `ceiling`.
pub(super) fn reset_hint_clamped_to(err: &Error, ceiling: Duration) -> Option<Duration> {
    match err {
        Error::Upstream {
            retry_after: Some(d),
            ..
        } => Some((*d).min(ceiling)).filter(|d| !d.is_zero()),
        _ => None,
    }
}

/// The same-provider retry cap for `class` under `policy` -- the value the
/// request-wide retry count is compared against. Delegates to
/// [`RetryPolicy::resolved_class`], which layers any operator per-class
/// `[retry.classes]` override on top of the baked class default. Shared by
/// [`decide`] and the class-decision observability so the logged cap never
/// drifts from the cap actually enforced.
pub(super) fn retry_cap_for(class: &FailureClass, policy: &RetryPolicy) -> u32 {
    policy.resolved_class(class).0
}

/// Whether a failure of this class debits the per-seat circuit breaker's
/// health accounting. True ONLY for the fixed transient-health set --
/// conditions a fallback or a cooldown can recover from; false for
/// caller-shaped or capability faults that retrying the same seat would
/// never fix, and (fail-closed) for any class the classifier gains later.
///
/// Deliberately independent of the fallback/retry decision: routing
/// (whether to advance the chain) and health accounting (whether the seat
/// looks unhealthy) are separate concerns.
pub const fn class_debits(class: &FailureClass) -> bool {
    matches!(
        class,
        FailureClass::RateLimited
            | FailureClass::ServerError
            | FailureClass::Timeout
            | FailureClass::NetworkError
            | FailureClass::Overloaded
    )
}

/// One failed upstream attempt, as the decision reads it. Built after auth
/// recovery and learner repair have declined the failure.
#[derive(Debug, Clone, Copy)]
pub(super) struct Failure<'a> {
    pub(super) err: &'a Error,
    /// The class the classifier gave, before the target's operator remap.
    /// The scope reads it, so a remap never changes how far a failure
    /// reaches.
    pub(super) native_class: &'a FailureClass,
    /// The class after the target's operator remap. The retry cap and the
    /// fallback switch read it.
    pub(super) class: &'a FailureClass,
    /// The closed capability resolver named a capability in this failure,
    /// read with the target's provider kind against the native class. A
    /// native 400 or 422 it names is scoped to the model.
    pub(super) native_capability_typed: bool,
    /// The request is an availability probe (`max_tokens` at or under
    /// `probe_max_tokens`).
    pub(super) is_probe: bool,
    pub(super) surface: DispatchSurface,
    /// The attempt ended because its own deadline expired, which walks on
    /// every surface.
    pub(super) deadline_fired: bool,
    /// The caller asked for no fallback: the first failure ends the request.
    pub(super) disable_fallbacks: bool,
    /// The target that failed.
    pub(super) target: WalkTarget<'a>,
}

impl Failure<'_> {
    /// The recognized upstream pattern the provider attached, if any.
    fn scope_row(&self) -> Option<ScopeRow> {
        self.err.scope_row()
    }
}

/// Whether the walk moves on after a failure it does not retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Next {
    /// Try the next chain target the walk still admits.
    Walk,
    /// End the request with this failure.
    Terminal,
}

/// Everything one failure does to the request and to the failed seat.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Disposition {
    /// How far the failure reaches.
    pub(super) scope: FailureScope,
    /// What the failure does to the failed seat's breaker.
    pub(super) breaker: BreakerOutcome,
    /// Whether the same target is dialed again; when it is, `next` applies
    /// only once the retries run out.
    pub(super) same_target: SameProvider,
    /// Targets the rest of the request must skip.
    pub(super) exclusions: Vec<Exclusion>,
    /// The recognized pattern that set the scope, if any.
    pub(super) recognized: Option<ScopeRow>,
    pub(super) next: Next,
}

impl Disposition {
    /// This disposition once its breaker outcome is applied: a failed seat
    /// whose breaker then reads open, whoever opened it, leaves nothing to
    /// retry there, so the same-target retry is dropped and the walk moves on
    /// per `next`.
    #[must_use]
    pub(super) fn after_breaker(self, breaker_open: bool) -> Self {
        if breaker_open {
            Self {
                same_target: SameProvider::No,
                ..self
            }
        } else {
            self
        }
    }

    /// Record on the request's walk what this disposition spends and
    /// excludes: a same-target retry, the exclusions, the recognized
    /// pattern, and a failure the request caused. An endpoint or model scope
    /// is the request's own doing (a native request rejection or a
    /// recognized pattern of that scope); this is the one place that rule
    /// is applied.
    pub(super) fn record_on(&self, walk: &mut RequestWalk) {
        if matches!(self.same_target, SameProvider::Retry { .. }) {
            walk.spend_retry();
        }
        if matches!(self.scope, FailureScope::Endpoint | FailureScope::Model) {
            walk.count_request_caused();
        }
        for exclusion in &self.exclusions {
            walk.exclude(exclusion.clone());
        }
        if let Some(row) = self.recognized {
            walk.recognize(row);
        }
    }
}

/// Decide what `failure` does: the breaker outcome, a same-target retry,
/// the request-scoped exclusions, and whether the walk continues.
///
/// The scope comes from, in order: a recognized pattern; a typed credential
/// reason (account scope, which the credential park already enforces); a
/// native 400/422 (model scope when it names a limit of the model, see
/// [`names_a_model_limit`], endpoint scope for any other);
/// otherwise the failed target alone, retryable per its class. Only a
/// retryable target-scoped failure may retry the same target, and only on a
/// completion. `[retry.classes]` only narrows: its retry cap bounds the
/// request-wide retry count and `fallback = false` ends the walk, but
/// neither lifts an exclusion. A probe never retries, never leaves the chain
/// run of its `[models]` entry, and never dials the failed seat again under a
/// later chain entry, whatever the failure; it takes the shared debit rule
/// ([`breaker_outcome`]), which spares only its native 429 or 529.
/// `disable_fallbacks` ends the request at the first failure. Outside a
/// completion or stream, only an attempt whose deadline fired walks on.
pub(super) fn decide(
    failure: &Failure<'_>,
    policy: &RetryPolicy,
    reset_hint: Option<Duration>,
    walk: &RequestWalk,
) -> Disposition {
    let (scope, retryable) = scope_of(failure);
    let breaker = breaker_outcome(failure.err, failure.class, reset_hint, failure.is_probe);
    let same_target = if retryable && scope == FailureScope::Target {
        retry_same_target(failure, policy, breaker, reset_hint, walk)
    } else {
        SameProvider::No
    };
    Disposition {
        scope,
        breaker,
        same_target,
        exclusions: exclusions_for(failure, scope),
        recognized: failure.scope_row(),
        next: next_step(failure, policy),
    }
}

/// `scope` when this build knows how far it reaches, else `None`.
const fn known_scope(scope: FailureScope) -> Option<FailureScope> {
    match scope {
        FailureScope::Target
        | FailureScope::Account
        | FailureScope::Endpoint
        | FailureScope::Model => Some(scope),
        // A scope added after this build reads as the target alone, never
        // retried: an unknown verdict must not widen what the request skips.
        _ => None,
    }
}

/// The scope of `failure` and whether retrying within it can succeed.
fn scope_of(failure: &Failure<'_>) -> (FailureScope, bool) {
    if let Some(row) = failure.scope_row() {
        return known_scope(row.scope()).map_or((FailureScope::Target, false), |scope| {
            (scope, row.retryable())
        });
    }
    if failure.err.upstream_reason().is_some() {
        return (FailureScope::Account, false);
    }
    if is_request_rejection(native_status(failure.err)) {
        let scope = if names_a_model_limit(failure) {
            FailureScope::Model
        } else {
            FailureScope::Endpoint
        };
        return (scope, false);
    }
    // A local refusal is decided by the request's shape alone, so the same
    // target refuses it again.
    (FailureScope::Target, failure.err.refusal_reason().is_none())
}

/// Whether a native request rejection names a limit of the model rather
/// than a fault every model on the endpoint would answer the same way: the
/// classifier typed it as an unsupported feature or an exceeded context
/// window, or the closed capability resolver named the capability. Nothing
/// else narrows a 400 to the model, so an untyped context-overflow 400 (the
/// Anthropic "prompt is too long" answer, which neither the classifier nor
/// the resolver recognizes) keeps endpoint scope.
const fn names_a_model_limit(failure: &Failure<'_>) -> bool {
    failure.native_capability_typed
        || matches!(
            failure.native_class,
            FailureClass::FeatureUnsupported { .. } | FailureClass::ContextWindow
        )
}

fn retry_same_target(
    failure: &Failure<'_>,
    policy: &RetryPolicy,
    breaker: BreakerOutcome,
    reset_hint: Option<Duration>,
    walk: &RequestWalk,
) -> SameProvider {
    let surface_retries = matches!(failure.surface, DispatchSurface::Complete);
    // A class cap of N allows N - 1 retries across the whole request.
    let within_cap = walk.retries_used().saturating_add(1) < retry_cap_for(failure.class, policy);
    if !surface_retries
        || failure.is_probe
        || failure.disable_fallbacks
        || matches!(breaker, BreakerOutcome::Park(_))
        || !within_cap
    {
        return SameProvider::No;
    }
    SameProvider::Retry {
        sleep_floor: reset_hint.filter(|h| *h <= INLOOP_RETRY_AFTER_CAP),
    }
}

/// The exclusions `scope` sets, plus a probe's fence
/// ([`Exclusion::probe_fence`]). The endpoint key is derived here, only for
/// a scope that needs one. A target-scoped failure excludes the failed seat
/// under every later chain entry naming it; a same-target retry is decided
/// before the walk reads exclusions, so it is unaffected.
fn exclusions_for(failure: &Failure<'_>, scope: FailureScope) -> Vec<Exclusion> {
    let target = &failure.target;
    let mut exclusions = Vec::new();
    match scope {
        FailureScope::Endpoint => exclusions.push(Exclusion::Endpoint(target.endpoint_key())),
        FailureScope::Model => exclusions.push(Exclusion::Model(target.model().to_owned())),
        FailureScope::Target => exclusions.push(Exclusion::Target(target.state_key().to_owned())),
        FailureScope::Account => {}
        // Unreachable: `scope_of` already folded an unknown scope to target.
        _ => {}
    }
    if failure.is_probe {
        exclusions.extend(Exclusion::probe_fence(target));
    }
    exclusions
}

/// Whether the walk moves on. An [`Error::UnknownProvider`] entry is walked
/// past by the dispatch walk before it dials, not decided here.
fn next_step(failure: &Failure<'_>, policy: &RetryPolicy) -> Next {
    let fallback = policy.resolved_class(failure.class).1;
    let surface_walks = failure.deadline_fired || failure.surface.health_error_walks();
    if fallback && surface_walks && !failure.disable_fallbacks {
        Next::Walk
    } else {
        Next::Terminal
    }
}

#[cfg(test)]
#[path = "outcome_rule_tests.rs"]
mod outcome_rule_tests;

#[cfg(test)]
#[path = "outcome_rule_decide_tests.rs"]
mod outcome_rule_decide_tests;
