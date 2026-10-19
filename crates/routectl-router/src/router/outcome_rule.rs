//! The failure-outcome rule shared by the dispatch surfaces: what a failed
//! attempt does to the seat's breaker (`breaker_outcome`) and whether the
//! same provider is retried (`same_provider_retry`). Pure decisions only;
//! the Router applies a `BreakerOutcome` in `runtime_gate`, and probe-slot
//! settlement stays at each call site.

use std::time::Duration;

use routectl_core::Error;
use routectl_core::failure_class::FailureClass;

use crate::config::RetryPolicy;

use super::class_observe::DispatchSurface;
use super::dispatch::probe_fast_fail_status;

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
/// `reset_hint`. A probe fast-fail ([`probe_fast_fail`]) leaves the
/// breaker alone whatever its class: a probe's 429/529 is a transient
/// upstream condition, not a provider fault. Otherwise a non-debiting
/// class leaves the breaker alone, and a debiting class parks the seat
/// only when a non-probe attempt carries a reset LARGER than
/// [`INLOOP_RETRY_AFTER_CAP`]; a smaller (or absent) reset, and every
/// other probe failure, takes the threshold-gated debit instead.
pub(super) fn breaker_outcome(
    err: &Error,
    class: &FailureClass,
    reset_hint: Option<Duration>,
    is_probe: bool,
) -> BreakerOutcome {
    if probe_fast_fail(err, is_probe).is_some() || !class_debits(class) {
        return BreakerOutcome::None;
    }
    match reset_hint {
        Some(h) if !is_probe && h > INLOOP_RETRY_AFTER_CAP => BreakerOutcome::Park(h),
        _ => BreakerOutcome::Debit,
    }
}

/// The upstream status when a probe attempt failed with a
/// rate-limit/overload ([`probe_fast_fail_status`]), else `None`. Keyed on
/// the upstream status, not the failure class, so an operator class remap
/// cannot change it. Such a probe neither retries, falls back across
/// distinct targets, nor debits the breaker.
pub(super) const fn probe_fast_fail(err: &Error, is_probe: bool) -> Option<u16> {
    if is_probe {
        probe_fast_fail_status(err)
    } else {
        None
    }
}

/// Whether a failed attempt retries the same provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SameProvider {
    No,
    /// Retry the same provider; `sleep_floor` is the upstream reset the
    /// next backoff sleep must wait at least, when one is honored in-loop.
    Retry {
        sleep_floor: Option<Duration>,
    },
}

/// The same-provider retry decision for one failed attempt on `surface`.
///
/// Streams and token counts never retry the same provider. A completion
/// retries while under the policy's hard attempt ceiling, the per-class
/// predicate ([`should_retry_same_provider`]) allows it, and the attempt
/// did not park the breaker (the re-gate would be a guaranteed
/// CircuitOpen). A debit that opened the breaker does not suppress the
/// retry. A non-probe reset within [`INLOOP_RETRY_AFTER_CAP`] becomes the
/// retry's sleep floor.
#[allow(clippy::too_many_arguments)]
pub(super) fn same_provider_retry(
    surface: DispatchSurface,
    err: &Error,
    class: &FailureClass,
    policy: &RetryPolicy,
    attempts_made: u32,
    is_probe: bool,
    transcript_rejection: bool,
    outcome: BreakerOutcome,
    hint: Option<Duration>,
) -> SameProvider {
    match surface {
        DispatchSurface::Stream | DispatchSurface::CountTokens => SameProvider::No,
        DispatchSurface::Complete => {
            let retries = attempts_made < policy.hard_retry_cap()
                && should_retry_same_provider(
                    err,
                    class,
                    policy,
                    attempts_made,
                    is_probe,
                    transcript_rejection,
                )
                && !matches!(outcome, BreakerOutcome::Park(_));
            if !retries {
                return SameProvider::No;
            }
            SameProvider::Retry {
                sleep_floor: hint.filter(|h| !is_probe && *h <= INLOOP_RETRY_AFTER_CAP),
            }
        }
    }
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
    match err {
        Error::Upstream {
            retry_after: Some(d),
            ..
        } => Some((*d).min(policy.max_honored_retry_after())).filter(|d| !d.is_zero()),
        _ => None,
    }
}

/// The same-provider retry cap for `class` under `policy` -- the value the
/// retry branch compares `attempts_made` against. Delegates to
/// [`RetryPolicy::resolved_class`], which layers any operator per-class
/// `[retry.classes]` override on top of the baked class default. Shared by
/// [`should_retry_same_provider`] and the class-decision observability so
/// the logged cap never drifts from the cap actually enforced.
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

pub(super) fn should_retry_same_provider(
    err: &Error,
    class: &FailureClass,
    policy: &RetryPolicy,
    attempts_made: u32,
    is_probe: bool,
    transcript_rejection: bool,
) -> bool {
    // Probe fast-fail mirrors `should_fallback`: a probe must not burn
    // retry attempts against a rate-limited/overloaded provider (429 /
    // 529). All other error classes fall through to the cap below.
    if probe_fast_fail(err, is_probe).is_some() {
        return false;
    }
    // A refusal is decided by the request's shape alone, so the same seat
    // refuses it again whatever an operator's per-class retry cap says.
    // So is a rejection laid to the request's defective transcript, which
    // the seat is sent again unchanged.
    if transcript_rejection || err.refusal_reason().is_some() {
        return false;
    }
    let cap = retry_cap_for(class, policy);
    attempts_made < cap
}

#[cfg(test)]
#[path = "outcome_rule_tests.rs"]
mod outcome_rule_tests;
