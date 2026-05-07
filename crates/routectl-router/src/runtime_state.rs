//! Per-provider runtime gates: token-bucket rate limiter + passive
//! circuit breaker. The `Router` holds one `ProviderState` per
//! configured provider name; both the rate limiter and the breaker
//! are read+updated under a single Mutex so the router's chain walk
//! can ask "should I dispatch to this provider right now?" atomically.

use std::time::{Duration, Instant};

use crate::config::ProviderRuntimePolicy;

const DEFAULT_CIRCUIT_COOLDOWN_MS: u64 = 30_000;
const RPM_WINDOW_MS: u64 = 60_000;

/// Runtime gate for a single provider name. Created from a config
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
    circuit_cooldown: Duration,
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
        Self {
            rpm_capacity: policy.rpm_limit.map(|r| r as f64),
            rpm_tokens: policy.rpm_limit.map(|r| r as f64).unwrap_or(0.0),
            rpm_last_refill: now,
            circuit_failure_threshold: policy.circuit_failures,
            circuit_cooldown: Duration::from_millis(
                policy
                    .circuit_cooldown_ms
                    .unwrap_or(DEFAULT_CIRCUIT_COOLDOWN_MS),
            ),
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
            if now.duration_since(opened_at) < self.circuit_cooldown {
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
            }
        }
    }

    pub fn half_open_probe_in_flight(&self) -> bool {
        self.half_open_in_flight
    }

    fn refill_tokens(&mut self, now: Instant, capacity: f64) {
        let elapsed_ms = now.duration_since(self.rpm_last_refill).as_millis() as f64;
        if elapsed_ms <= 0.0 {
            return;
        }
        let refill_rate_per_ms = capacity / RPM_WINDOW_MS as f64;
        self.rpm_tokens = (self.rpm_tokens + refill_rate_per_ms * elapsed_ms).min(capacity);
        self.rpm_last_refill = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_state_always_allows() {
        let policy = ProviderRuntimePolicy::default();
        let mut s = ProviderState::new(&policy);
        for _ in 0..100 {
            assert_eq!(s.try_dispatch(Instant::now()), GateDecision::Allow);
            s.record_success();
        }
    }

    #[test]
    fn rpm_bucket_drains_and_refills() {
        let policy = ProviderRuntimePolicy {
            rpm_limit: Some(2),
            ..Default::default()
        };
        let mut s = ProviderState::new(&policy);
        let t0 = Instant::now();
        assert_eq!(s.try_dispatch(t0), GateDecision::Allow);
        s.record_success();
        assert_eq!(s.try_dispatch(t0), GateDecision::Allow);
        s.record_success();
        assert_eq!(s.try_dispatch(t0), GateDecision::RateLimited);
        let t1 = t0 + Duration::from_secs(60);
        assert_eq!(s.try_dispatch(t1), GateDecision::Allow);
    }

    #[test]
    fn circuit_opens_after_threshold_and_skips_until_cooldown() {
        let policy = ProviderRuntimePolicy {
            circuit_failures: Some(2),
            circuit_cooldown_ms: Some(1_000),
            ..Default::default()
        };
        let mut s = ProviderState::new(&policy);
        let t0 = Instant::now();
        assert_eq!(s.try_dispatch(t0), GateDecision::Allow);
        s.record_failure(t0);
        assert_eq!(s.try_dispatch(t0), GateDecision::Allow);
        s.record_failure(t0);
        assert_eq!(s.try_dispatch(t0), GateDecision::CircuitOpen);
        let t_mid = t0 + Duration::from_millis(500);
        assert_eq!(s.try_dispatch(t_mid), GateDecision::CircuitOpen);
        // After cooldown a single probe is allowed.
        let t_after = t0 + Duration::from_millis(1_500);
        assert_eq!(s.try_dispatch(t_after), GateDecision::Allow);
    }

    #[test]
    fn half_open_is_single_probe_under_concurrent_dispatches() {
        let policy = ProviderRuntimePolicy {
            circuit_failures: Some(2),
            circuit_cooldown_ms: Some(1_000),
            ..Default::default()
        };
        let mut s = ProviderState::new(&policy);
        let t0 = Instant::now();
        // Trip the breaker.
        s.try_dispatch(t0);
        s.record_failure(t0);
        s.try_dispatch(t0);
        s.record_failure(t0);
        assert_eq!(s.try_dispatch(t0), GateDecision::CircuitOpen);
        // Cooldown elapsed: first dispatch claims the half-open slot.
        let t_after = t0 + Duration::from_millis(1_500);
        assert_eq!(s.try_dispatch(t_after), GateDecision::Allow);
        // Second concurrent dispatch (probe NOT yet recorded) sees
        // CircuitOpen because someone already has the probe slot.
        assert_eq!(s.try_dispatch(t_after), GateDecision::CircuitOpen);
        // Probe fails -> breaker re-trips.
        s.record_failure(t_after);
        // Subsequent calls in the new cooldown window also see CircuitOpen.
        assert_eq!(s.try_dispatch(t_after), GateDecision::CircuitOpen);
    }

    #[test]
    fn half_open_probe_success_closes_the_breaker() {
        let policy = ProviderRuntimePolicy {
            circuit_failures: Some(2),
            circuit_cooldown_ms: Some(500),
            ..Default::default()
        };
        let mut s = ProviderState::new(&policy);
        let t0 = Instant::now();
        s.try_dispatch(t0);
        s.record_failure(t0);
        s.try_dispatch(t0);
        s.record_failure(t0);
        let t_after = t0 + Duration::from_millis(600);
        assert_eq!(s.try_dispatch(t_after), GateDecision::Allow);
        s.record_success();
        // Closed -- subsequent dispatch is Allow without needing another cooldown.
        assert_eq!(s.try_dispatch(t_after), GateDecision::Allow);
    }

    #[test]
    fn half_open_slot_released_when_rpm_refuses_the_probe() {
        // If the breaker is half-open AND the RPM bucket is empty,
        // we should NOT consume the half-open slot -- the next caller
        // (after RPM refills) should still get a probe.
        let policy = ProviderRuntimePolicy {
            rpm_limit: Some(1),
            circuit_failures: Some(1),
            circuit_cooldown_ms: Some(500),
            ..Default::default()
        };
        let mut s = ProviderState::new(&policy);
        let t0 = Instant::now();
        // Trip the breaker.
        s.try_dispatch(t0);
        s.record_failure(t0);
        // Consume the RPM token in the next cycle so the next
        // try_dispatch hits RPM-limited.
        let t_after = t0 + Duration::from_millis(700);
        // RPM is at 0/1 after the failed probe. Refill is gradual.
        // We ensure the bucket is empty by exhausting it explicitly:
        // (record_failure already returned the slot in failure path, but
        //  the bucket itself was decremented -- let's just check the
        //  half_open_slot is reclaimable when RPM refuses.)
        // Force RPM empty: drain whatever is left.
        while matches!(s.try_dispatch(t_after), GateDecision::Allow) {
            s.record_success();
        }
        // Now circuit is Closed (we just succeeded). Re-trip it.
        s.record_failure(t_after);
        // Wait cooldown; RPM still depleted in this instant.
        let t_probe = t_after + Duration::from_millis(700);
        // RPM may have refilled by t_probe; if so this test's pre-condition
        // is moot. The invariant we care about: any path that returns
        // RateLimited must NOT leave half_open_in_flight=true.
        // Force the scenario directly:
        s.rpm_tokens = 0.0;
        s.rpm_last_refill = t_probe;
        let decision = s.try_dispatch(t_probe);
        assert_eq!(decision, GateDecision::RateLimited);
        // The half-open slot is still free; bumping the clock past
        // a refill window lets the next dispatch claim it.
        let t_refill = t_probe + Duration::from_secs(60);
        assert_eq!(s.try_dispatch(t_refill), GateDecision::Allow);
    }

    #[test]
    fn success_resets_failure_counter() {
        let policy = ProviderRuntimePolicy {
            circuit_failures: Some(3),
            ..Default::default()
        };
        let mut s = ProviderState::new(&policy);
        let t = Instant::now();
        s.try_dispatch(t);
        s.record_failure(t);
        s.try_dispatch(t);
        s.record_failure(t);
        s.try_dispatch(t);
        s.record_success();
        s.try_dispatch(t);
        s.record_failure(t);
        s.try_dispatch(t);
        s.record_failure(t);
        assert_eq!(s.try_dispatch(t), GateDecision::Allow);
    }
}
