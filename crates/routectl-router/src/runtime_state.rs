//! Per-model runtime gates: token-bucket rate limiter + passive
//! circuit breaker. The `Router` holds one `ProviderState` per
//! model nickname (one `[models.X]` entry); both the rate limiter
//! and the breaker are read+updated under a single Mutex so the
//! router's chain walk can ask "should I dispatch to this model
//! right now?" atomically.

use std::time::{Duration, Instant};

use crate::config::ProviderRuntimePolicy;

const DEFAULT_CIRCUIT_COOLDOWN_MS: u64 = 30_000;
const RPM_WINDOW_MS: u64 = 60_000;

/// Runtime gate for a single model nickname. Created from a config
/// `ProviderRuntimePolicy`; if all knobs are unset, the gate is a
/// transparent no-op.
///
/// Circuit-breaker state machine:
///   Closed --(consecutive_failures hits threshold)--> Open
///   Open --(cooldown elapses, single probe issued)--> HalfOpen-pending
///   HalfOpen-pending --(probe success)--> Closed
///   HalfOpen-pending --(probe failure)--> Open (re-tripped)
///
/// Only ONE concurrent caller can hold the half-open probe slot. While
/// `half_open_in_flight` is true, other `try_dispatch` callers see
/// `CircuitOpen` even though the cooldown elapsed.
#[derive(Debug)]
pub struct ProviderState {
    /// Token bucket: leaky cap of `rpm_limit` requests per 60s window.
    /// `None` means unlimited.
    rpm_capacity: Option<f64>,
    rpm_tokens: f64,
    rpm_last_refill: Instant,

    /// Circuit-breaker config and state. `None` means disabled.
    circuit_failure_threshold: Option<u32>,
    /// Baseline cooldown from config; the default for every normal
    /// failure-driven trip.
    circuit_cooldown: Duration,
    /// Cooldown governing the CURRENT open window. Equals
    /// `circuit_cooldown` for every normal trip; only `force_open` can
    /// set a different value, and it is reset back to the baseline on the
    /// next normal trip or on a successful close.
    active_cooldown: Duration,
    consecutive_failures: u32,
    circuit_opened_at: Option<Instant>,
    /// True once `try_dispatch` has authorized a half-open probe and
    /// before the result is recorded. Forces other concurrent callers
    /// to wait (return CircuitOpen) so the breaker only sees ONE
    /// probe per cooldown cycle.
    half_open_in_flight: bool,
}

/// Decision returned from the dispatch gate.
#[derive(Debug, Clone, PartialEq)]
pub enum GateDecision {
    /// Provider is healthy and within rate limits; dispatch.
    Allow,
    /// Provider exceeded its RPM cap. Treat as a fallbackable failure.
    RateLimited,
    /// Circuit is open from prior failures (or a half-open probe is
    /// already in flight). Treat as a fallbackable failure and skip
    /// this provider until the next cooldown cycle.
    CircuitOpen,
}

impl ProviderState {
    pub fn new(policy: &ProviderRuntimePolicy) -> Self {
        let now = Instant::now();
        let circuit_cooldown = Duration::from_millis(
            policy
                .circuit_cooldown_ms
                .unwrap_or(DEFAULT_CIRCUIT_COOLDOWN_MS),
        );
        Self {
            rpm_capacity: policy.rpm_limit.map(|r| r as f64),
            rpm_tokens: policy.rpm_limit.map(|r| r as f64).unwrap_or(0.0),
            rpm_last_refill: now,
            circuit_failure_threshold: policy.circuit_failures,
            circuit_cooldown,
            active_cooldown: circuit_cooldown,
            consecutive_failures: 0,
            circuit_opened_at: None,
            half_open_in_flight: false,
        }
    }

    /// Decide whether to dispatch right now. If `Allow`, the caller
    /// MUST follow up with `record_success` or `record_failure`
    /// (the breaker depends on every Allow being closed out).
    pub fn try_dispatch(&mut self, now: Instant) -> GateDecision {
        if let Some(opened_at) = self.circuit_opened_at {
            if now.duration_since(opened_at) < self.active_cooldown {
                return GateDecision::CircuitOpen;
            }
            // Cooldown elapsed. Allow exactly one probe through; other
            // callers see CircuitOpen until the probe records its
            // outcome. We do NOT clear `circuit_opened_at` or reset
            // the failure counter here -- only `record_success` /
            // `record_failure` mutate that state, ensuring the breaker
            // is closed only after a verified success.
            if self.half_open_in_flight {
                return GateDecision::CircuitOpen;
            }
            self.half_open_in_flight = true;
            // Fall through to RPM check below.
        }

        if let Some(capacity) = self.rpm_capacity {
            self.refill_tokens(now, capacity);
            if self.rpm_tokens < 1.0 {
                // The RPM gate refused, so we're NOT actually issuing
                // the probe; release the half-open slot if we just
                // claimed it.
                if self.circuit_opened_at.is_some() {
                    self.half_open_in_flight = false;
                }
                return GateDecision::RateLimited;
            }
            self.rpm_tokens -= 1.0;
        }

        GateDecision::Allow
    }

    /// Mark the most recent dispatch as successful. Resets the
    /// circuit-breaker failure counter and closes a tripped circuit
    /// if this was a half-open probe.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.circuit_opened_at = None;
        self.half_open_in_flight = false;
        self.active_cooldown = self.circuit_cooldown;
    }

    /// Mark the most recent dispatch as failed. Trips the breaker
    /// when consecutive failures hit the configured threshold. If
    /// this was a half-open probe, re-trip the breaker by setting
    /// a fresh `circuit_opened_at = now`.
    pub fn record_failure(&mut self, now: Instant) {
        let was_half_open_probe = self.half_open_in_flight;
        self.half_open_in_flight = false;

        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if let Some(threshold) = self.circuit_failure_threshold {
            // Re-trip the breaker if a half-open probe failed (regardless of
            // counter), or if the counter just crossed the threshold for
            // the first time.
            if was_half_open_probe || self.consecutive_failures >= threshold {
                self.circuit_opened_at = Some(now);
                // A normal failure-driven trip always uses the baseline
                // cooldown, discarding any custom park set by force_open.
                self.active_cooldown = self.circuit_cooldown;
            }
        }
    }

    /// Park the provider immediately for `cooldown`, bypassing the
    /// consecutive-failure threshold. Used when an upstream sent an
    /// explicit reset hint (e.g. a rate-limit reset that is hours away):
    /// a single such signal should open the circuit at once rather than
    /// waiting for `circuit_failure_threshold` failures to accumulate.
    /// The custom `cooldown` governs only THIS open window; the next
    /// normal trip or a successful close restores the baseline. Recovery
    /// after the cooldown still flows through the single half-open probe.
    ///
    /// The caller is responsible for clamping `cooldown` to any
    /// configured ceiling before calling; `force_open` honors whatever it
    /// is given. The consecutive-failure counter is left untouched -- the
    /// open state is driven by `circuit_opened_at` + `active_cooldown`,
    /// independent of the counter.
    ///
    /// Call only from a context that does NOT hold the half-open probe
    /// slot. The router satisfies this: observing the upstream error that
    /// carries the reset implies the caller reached the provider, which
    /// implies it owns the slot this call then releases. Calling it while
    /// another caller holds the slot would reset that caller's state.
    pub fn force_open(&mut self, now: Instant, cooldown: Duration) {
        self.circuit_opened_at = Some(now);
        self.active_cooldown = cooldown;
        // Release any in-flight probe slot so the new park window starts
        // clean, mirroring the failure path.
        self.half_open_in_flight = false;
    }

    /// Release a half-open probe slot claimed by `try_dispatch` WITHOUT
    /// recording an outcome. Used when the dispatch path declines to
    /// count an attempt as a provider fault (e.g. a probe fast-fail on a
    /// transient 429/529, or a non-fallbackable client error) but must
    /// still free the slot it claimed so the next probe can proceed
    /// after cooldown. Deliberately does NOT touch
    /// `consecutive_failures` or `circuit_opened_at`: releasing a
    /// claimed slot is not a failure observation, so the breaker's trip
    /// state and counter stay unchanged.
    pub fn release_probe_slot(&mut self) {
        self.half_open_in_flight = false;
    }

    pub fn half_open_probe_in_flight(&self) -> bool {
        self.half_open_in_flight
    }

    fn refill_tokens(&mut self, now: Instant, capacity: f64) {
        if now.duration_since(self.rpm_last_refill).as_millis() == 0 {
            // No measurable time elapsed; leave the anchor untouched
            // (matches the pre-refactor early return).
            return;
        }
        self.rpm_tokens = self.projected_tokens(now, capacity);
        self.rpm_last_refill = now;
    }

    /// Pure, non-mutating projection of the bucket level at `now`. Shared
    /// by `refill_tokens` (which stores the result) and `capacity_snapshot`
    /// (which does not), so the leak arithmetic never diverges. `elapsed`
    /// at or below zero yields the current level unchanged.
    fn projected_tokens(&self, now: Instant, capacity: f64) -> f64 {
        let elapsed_ms = now.duration_since(self.rpm_last_refill).as_millis() as f64;
        if elapsed_ms <= 0.0 {
            return self.rpm_tokens;
        }
        let refill_rate_per_ms = capacity / RPM_WINDOW_MS as f64;
        (self.rpm_tokens + refill_rate_per_ms * elapsed_ms).min(capacity)
    }

    /// Read the gate's capacity WITHOUT mutating it. Takes `&self`, so the
    /// borrow checker forbids touching the token bucket or the half-open
    /// probe slot; it must never call `try_dispatch` or `refill_tokens`.
    /// Projects the lazily-refilled bucket level at `now` and classifies
    /// the breaker phase, mirroring what `try_dispatch` would observe.
    pub fn capacity_snapshot(&self, now: Instant) -> CapacitySnapshot {
        let rpm_available = self
            .rpm_capacity
            .map(|capacity| self.projected_tokens(now, capacity));

        let circuit = match self.circuit_opened_at {
            None => CircuitPhase::Closed,
            Some(opened_at) => {
                let cooldown_elapsed = now.duration_since(opened_at) >= self.active_cooldown;
                if !cooldown_elapsed || self.half_open_in_flight {
                    CircuitPhase::Open
                } else {
                    CircuitPhase::HalfOpenReady
                }
            }
        };

        CapacitySnapshot {
            rpm_available,
            circuit,
        }
    }
}

/// Read-only classification of the circuit breaker, mirroring the decision
/// `try_dispatch` would make from the same state -- without mutating it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitPhase {
    /// `circuit_opened_at == None`. `try_dispatch` would fall through the
    /// breaker and (RPM permitting) return `Allow`.
    Closed,
    /// Breaker is currently blocking: still within cooldown
    /// (`now - opened_at < active_cooldown`), OR cooldown elapsed but a
    /// half-open probe is already in flight. `try_dispatch` would return
    /// `CircuitOpen`.
    Open,
    /// Cooldown elapsed AND no probe is in flight: the next `try_dispatch`
    /// would claim the half-open probe slot and (RPM permitting) return
    /// `Allow`.
    HalfOpenReady,
}

/// Non-mutating view of a `ProviderState`'s capacity at a given instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CapacitySnapshot {
    /// Projected available RPM tokens right now. `None` when the policy is
    /// unlimited; otherwise the lazily-refilled bucket level computed
    /// without storing it back.
    pub rpm_available: Option<f64>,
    /// Read-only circuit-breaker phase.
    pub circuit: CircuitPhase,
}

impl CapacitySnapshot {
    /// Predict whether `try_dispatch` would return `Allow` from this state,
    /// without mutating anything: the circuit is not `Open` (i.e. `Closed`
    /// or `HalfOpenReady`) AND there is RPM headroom (unlimited, or at least
    /// one whole token available).
    pub fn is_dispatchable(&self) -> bool {
        let circuit_ok = self.circuit != CircuitPhase::Open;
        let rpm_ok = match self.rpm_available {
            None => true,
            Some(tokens) => tokens >= 1.0,
        };
        circuit_ok && rpm_ok
    }
}

#[cfg(test)]
#[path = "runtime_state_tests.rs"]
mod tests;
