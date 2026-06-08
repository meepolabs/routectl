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
    fn release_probe_slot_frees_slot_without_recording_outcome() {
        // A probe fast-fail releases the claimed half-open slot WITHOUT
        // counting a failure. The slot must clear, the failure counter
        // and trip timestamp must stay put, and the next dispatch in the
        // still-open window must be granted a fresh probe (proving the
        // breaker is not locked).
        let policy = ProviderRuntimePolicy {
            circuit_failures: Some(1),
            circuit_cooldown_ms: Some(500),
            ..Default::default()
        };
        let mut s = ProviderState::new(&policy);
        let t0 = Instant::now();
        // Trip the breaker (threshold = 1).
        assert_eq!(s.try_dispatch(t0), GateDecision::Allow);
        s.record_failure(t0);
        assert_eq!(s.try_dispatch(t0), GateDecision::CircuitOpen);
        // After cooldown, the first dispatch claims the half-open slot.
        let t_after = t0 + Duration::from_millis(600);
        assert_eq!(s.try_dispatch(t_after), GateDecision::Allow);
        assert!(s.half_open_probe_in_flight(), "probe slot must be claimed");
        let failures_before = s.consecutive_failures;
        let opened_before = s.circuit_opened_at;

        // Release WITHOUT recording success/failure.
        s.release_probe_slot();
        assert!(
            !s.half_open_probe_in_flight(),
            "release_probe_slot must clear the half-open slot",
        );
        assert_eq!(
            s.consecutive_failures, failures_before,
            "release_probe_slot must NOT change the failure counter",
        );
        assert_eq!(
            s.circuit_opened_at, opened_before,
            "release_probe_slot must NOT change the trip timestamp",
        );

        // The breaker is NOT locked: the next dispatch in the still-open
        // window is granted a fresh probe because the slot is free again.
        assert_eq!(s.try_dispatch(t_after), GateDecision::Allow);
        assert!(s.half_open_probe_in_flight());
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

    #[test]
    fn force_open_parks_immediately_bypassing_threshold() {
        // Arrange: a high failure threshold so the counter-driven trip
        // would never fire on a single failure.
        let policy = ProviderRuntimePolicy {
            circuit_failures: Some(5),
            ..Default::default()
        };
        let mut s = ProviderState::new(&policy);
        let t0 = Instant::now();

        // Act: park immediately after ZERO failures.
        s.force_open(t0, Duration::from_secs(10));

        // Assert: open right away, single probe only after the custom cooldown.
        assert_eq!(s.try_dispatch(t0), GateDecision::CircuitOpen);
        let t_after = t0 + Duration::from_secs(11);
        assert_eq!(s.try_dispatch(t_after), GateDecision::Allow);
    }

    #[test]
    fn force_open_cooldown_outlasts_default() {
        // Arrange: a tiny 1s default cooldown.
        let policy = ProviderRuntimePolicy {
            circuit_cooldown_ms: Some(1_000),
            ..Default::default()
        };
        let mut s = ProviderState::new(&policy);
        let t0 = Instant::now();

        // Act: park for 60s, far longer than the 1s default.
        s.force_open(t0, Duration::from_secs(60));

        // Assert: still open at t0+5s (default would already be elapsed),
        // probe only after the custom 60s window.
        assert_eq!(
            s.try_dispatch(t0 + Duration::from_secs(5)),
            GateDecision::CircuitOpen,
        );
        assert_eq!(
            s.try_dispatch(t0 + Duration::from_secs(61)),
            GateDecision::Allow,
        );
    }

    #[test]
    fn force_open_releases_inflight_probe_slot() {
        // Arrange: trip the breaker normally, then advance past cooldown
        // and claim the half-open slot.
        let policy = ProviderRuntimePolicy {
            circuit_failures: Some(1),
            circuit_cooldown_ms: Some(500),
            ..Default::default()
        };
        let mut s = ProviderState::new(&policy);
        let t0 = Instant::now();
        s.try_dispatch(t0);
        s.record_failure(t0);
        let t_after = t0 + Duration::from_millis(600);
        assert_eq!(s.try_dispatch(t_after), GateDecision::Allow);
        assert!(s.half_open_probe_in_flight(), "probe slot must be claimed");

        // Act: force_open while a probe is in flight.
        s.force_open(t_after, Duration::from_secs(30));

        // Assert: the in-flight slot is released and the breaker is open
        // again for the new window.
        assert!(
            !s.half_open_probe_in_flight(),
            "force_open must release the in-flight probe slot",
        );
        assert_eq!(s.try_dispatch(t_after), GateDecision::CircuitOpen);
    }

    #[test]
    fn force_open_then_probe_success_resets_to_default() {
        // Arrange: a 1s default cooldown, then a 60s custom park.
        let policy = ProviderRuntimePolicy {
            circuit_failures: Some(1),
            circuit_cooldown_ms: Some(1_000),
            ..Default::default()
        };
        let mut s = ProviderState::new(&policy);
        let t0 = Instant::now();
        s.force_open(t0, Duration::from_secs(60));

        // Act: the custom park elapses, the single probe succeeds.
        let t_probe = t0 + Duration::from_secs(61);
        assert_eq!(s.try_dispatch(t_probe), GateDecision::Allow);
        s.record_success();

        // Assert: a NORMAL threshold trip now uses the DEFAULT cooldown
        // (1s), not the stale 60s custom park.
        s.try_dispatch(t_probe);
        s.record_failure(t_probe);
        assert_eq!(s.try_dispatch(t_probe), GateDecision::CircuitOpen);
        // Still open just before the 1s default elapses.
        assert_eq!(
            s.try_dispatch(t_probe + Duration::from_millis(500)),
            GateDecision::CircuitOpen,
        );
        // Allowed once the 1s default cooldown elapses -- proving the
        // window is the default, not 60s.
        assert_eq!(
            s.try_dispatch(t_probe + Duration::from_millis(1_500)),
            GateDecision::Allow,
        );
    }

    #[test]
    fn force_open_recovery_is_single_probe() {
        // Arrange: park immediately for a custom window.
        let policy = ProviderRuntimePolicy {
            circuit_failures: Some(5),
            ..Default::default()
        };
        let mut s = ProviderState::new(&policy);
        let t0 = Instant::now();
        s.force_open(t0, Duration::from_secs(10));

        // Act: the custom cooldown elapses; the first dispatch claims the
        // single probe, a concurrent second dispatch (probe not yet
        // recorded) is refused.
        let t_after = t0 + Duration::from_secs(11);

        // Assert: single-probe invariant holds under force_open.
        assert_eq!(s.try_dispatch(t_after), GateDecision::Allow);
        assert_eq!(s.try_dispatch(t_after), GateDecision::CircuitOpen);
    }
}
