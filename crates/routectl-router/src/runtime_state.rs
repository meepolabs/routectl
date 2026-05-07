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
}

/// Decision returned from the dispatch gate.
#[derive(Debug, Clone, PartialEq)]
pub enum GateDecision {
    /// Provider is healthy and within rate limits; dispatch.
    Allow,
    /// Provider exceeded its RPM cap. Treat as a fallbackable failure.
    RateLimited,
    /// Circuit is open from prior failures. Treat as a fallbackable
    /// failure and skip this provider until the cooldown elapses.
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
        }
    }

    /// Decide whether to dispatch right now. If `Allow`, the caller
    /// must follow up with `record_success` or `record_failure`.
    pub fn try_dispatch(&mut self, now: Instant) -> GateDecision {
        if let Some(opened_at) = self.circuit_opened_at {
            if now.duration_since(opened_at) < self.circuit_cooldown {
                return GateDecision::CircuitOpen;
            }
            // Cooldown elapsed; reset to half-open by clearing the
            // opened_at marker. The next call gets one shot to succeed.
            self.circuit_opened_at = None;
            self.consecutive_failures = 0;
        }

        if let Some(capacity) = self.rpm_capacity {
            self.refill_tokens(now, capacity);
            if self.rpm_tokens < 1.0 {
                return GateDecision::RateLimited;
            }
            self.rpm_tokens -= 1.0;
        }

        GateDecision::Allow
    }

    /// Mark the most recent dispatch as successful. Resets the
    /// circuit-breaker failure counter.
    pub fn record_success(&mut self) {
        self.consecutive_failures = 0;
        self.circuit_opened_at = None;
    }

    /// Mark the most recent dispatch as failed. Trips the breaker
    /// when consecutive failures hit the configured threshold.
    pub fn record_failure(&mut self, now: Instant) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if let Some(threshold) = self.circuit_failure_threshold {
            if self.consecutive_failures >= threshold {
                self.circuit_opened_at = Some(now);
            }
        }
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
        // First two go through.
        assert_eq!(s.try_dispatch(t0), GateDecision::Allow);
        s.record_success();
        assert_eq!(s.try_dispatch(t0), GateDecision::Allow);
        s.record_success();
        // Third in the same instant gets rate-limited.
        assert_eq!(s.try_dispatch(t0), GateDecision::RateLimited);
        // After 60s we should have a full capacity refill.
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
        // Two failures trip the breaker.
        assert_eq!(s.try_dispatch(t0), GateDecision::Allow);
        s.record_failure(t0);
        assert_eq!(s.try_dispatch(t0), GateDecision::Allow);
        s.record_failure(t0);
        // Now the breaker is open.
        assert_eq!(s.try_dispatch(t0), GateDecision::CircuitOpen);
        // Mid-cooldown -- still open.
        let t_mid = t0 + Duration::from_millis(500);
        assert_eq!(s.try_dispatch(t_mid), GateDecision::CircuitOpen);
        // After cooldown -- one probe allowed.
        let t_after = t0 + Duration::from_millis(1_500);
        assert_eq!(s.try_dispatch(t_after), GateDecision::Allow);
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
        // Two failures in a row but threshold is 3.
        s.try_dispatch(t);
        s.record_success();
        // Counter reset; two more failures should NOT trip the breaker.
        s.try_dispatch(t);
        s.record_failure(t);
        s.try_dispatch(t);
        s.record_failure(t);
        assert_eq!(s.try_dispatch(t), GateDecision::Allow);
    }
}
