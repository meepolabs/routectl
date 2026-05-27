//! Per-source-port rate limit for the OAuth loopback callback server.
//!
//! Why this exists: the redirect URI binds on 127.0.0.1 and is reachable
//! by any co-resident local process. The state-validation gate in
//! `callback_handler` already returns 400 instead of terminating the
//! listener on unauthenticated hits (so a co-resident process cannot
//! abort an in-flight login by sending an arbitrary GET). What it does
//! NOT do is bound how fast such a process can churn 4xx rejections;
//! left unchecked, a noisy local process could saturate the handler with
//! 400s indefinitely until the 120s `LoginTimeout` fires, drowning the
//! legitimate browser callback. This tracker turns sustained abuse from
//! one source port into a 429 instead of a 400, which short-circuits
//! the handler's work per request and makes saturation cheap to detect.
//!
//! Design choices, deliberately documented for the next reader:
//!
//! - Sliding window, not token bucket. Simpler to reason about for the
//!   "did this port just spew N rejections?" question; a token bucket
//!   would also work but adds rate / burst-capacity tuning we don't
//!   need.
//! - Per source port, not per source IP. The listener binds on
//!   127.0.0.1; every hit has source IP 127.0.0.1, so the only useful
//!   distinguishing key is the OS-assigned source port of the
//!   connecting socket.
//! - Two windows, one per port AND one listener-wide. The per-port
//!   guard catches a single noisy process; the global guard catches a
//!   port-spray attacker who opens a fresh loopback connection per hit
//!   so no per-port bucket ever crosses its threshold. A request is
//!   rate-limited if EITHER window is at or above its threshold.
//! - Only count REJECTED (state-invalid) hits. A genuine browser
//!   callback echoes the CSRF state and bypasses this tracker
//!   entirely, so even a refresh / prefetch never approaches the
//!   threshold.
//! - LRU eviction at a fixed capacity (per-port only). Without a cap, a
//!   misbehaving process could spray random source ports to fill
//!   memory; the per-port tracker holds at most `CAPACITY` entries and
//!   drops the least-recently-touched port to make room for a new one.
//!   The global window does not need LRU (it is a single growing
//!   window) and is bounded by capping the timestamp queue at
//!   `2 * global_threshold`, mirroring the per-bucket cap.
//!
//! Tuning rationale (defaults below):
//! - Window 10s: long enough that a misbehaving process is clearly
//!   abusive, short enough that a port that briefly burst recovers
//!   automatically.
//! - Per-port threshold 30: a real browser callback fires once or twice
//!   per login (success page, plus maybe a favicon prefetch); 30 unique
//!   rejected hits within 10s from one source port is well outside any
//!   legitimate pattern.
//! - Global threshold 60: 2x the per-port budget. Still well above the
//!   1-2 rejected hits a legitimate browser callback can produce (and
//!   the legitimate path bypasses this tracker entirely), but low
//!   enough that a port-spray attacker churning fresh connections is
//!   caught after the first burst.
//! - Capacity 256: small enough to be bounded memory under
//!   adversarial port-spray, large enough that legitimate
//!   parallel-tab oddities never evict each other.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Outcome of recording a rejected hit for a given port.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Below the threshold; the caller should respond 400 as usual.
    Admitted,
    /// At or above the threshold; the caller should respond 429.
    RateLimited,
}

const DEFAULT_WINDOW: Duration = Duration::from_secs(10);
const DEFAULT_THRESHOLD: usize = 30;
const DEFAULT_GLOBAL_THRESHOLD: usize = 60;
const DEFAULT_CAPACITY: usize = 256;

/// Per-port sliding-window rejection counter with LRU eviction, plus a
/// listener-wide rejection window for the connection-cycling threat.
///
/// `entries` is ordered front-to-back as least-recently-touched to
/// most-recently-touched. Every `record_rejection` either touches an
/// existing port (move it to the back) or inserts a new one (push to
/// the back, evicting the front if at capacity).
///
/// `global_window` is a single sliding window of REJECTED hit timestamps
/// across ALL source ports. It defends against a local attacker who
/// opens a fresh loopback connection per request: every hit gets a new
/// ephemeral source port, so no per-port bucket ever fills up; the
/// global window catches that pattern by counting raw rejection volume
/// regardless of source.
pub(crate) struct RateLimitTracker {
    entries: VecDeque<(u16, VecDeque<Instant>)>,
    global_window: VecDeque<Instant>,
    window: Duration,
    threshold: usize,
    global_threshold: usize,
    capacity: usize,
}

impl Default for RateLimitTracker {
    fn default() -> Self {
        Self::with_config(
            DEFAULT_WINDOW,
            DEFAULT_THRESHOLD,
            DEFAULT_GLOBAL_THRESHOLD,
            DEFAULT_CAPACITY,
        )
    }
}

impl RateLimitTracker {
    /// Build a tracker with explicit knobs. The defaults from `Default`
    /// are the production values; this constructor exists so tests can
    /// pin a small threshold / short window / tiny capacity and exercise
    /// the eviction and decision logic deterministically.
    pub(crate) fn with_config(
        window: Duration,
        threshold: usize,
        global_threshold: usize,
        capacity: usize,
    ) -> Self {
        // Zero capacity would never track anything (every hit Admitted),
        // zero threshold would 429 on the first hit per port, and zero
        // global_threshold would 429 on the very first hit ever. All
        // are bug-class inputs; assert.
        assert!(capacity > 0, "rate limit capacity must be > 0");
        assert!(threshold > 0, "rate limit threshold must be > 0");
        assert!(
            global_threshold > 0,
            "rate limit global threshold must be > 0"
        );
        Self {
            entries: VecDeque::with_capacity(capacity),
            global_window: VecDeque::new(),
            window,
            threshold,
            global_threshold,
            capacity,
        }
    }

    /// Record a rejected hit from `port` at time `now`. Returns whether
    /// the caller should respond 400 (`Admitted`) or 429 (`RateLimited`).
    ///
    /// A hit is rate-limited if EITHER:
    /// - the per-port bucket for `port` is at or above `threshold`, OR
    /// - the listener-wide `global_window` is at or above
    ///   `global_threshold`.
    ///
    /// Each window is independently capped (per-port at `2 * threshold`,
    /// global at `2 * global_threshold`) to bound memory under sustained
    /// abuse; once a ceiling is hit, new rate-limited hits stop pushing
    /// to that window. This keeps each window clearing as timestamps
    /// roll out, even if traffic keeps hammering.
    pub(crate) fn record_rejection(&mut self, port: u16, now: Instant) -> Decision {
        // --- Per-port bucket: touch (LRU move) + prune + check + push.
        self.touch_or_insert(port);
        // INVARIANT: touch_or_insert always pushes an entry to the back
        // (either moves an existing one or inserts a fresh one), so
        // back_mut is always Some after the call. capacity > 0 is
        // asserted in with_config.
        let bucket = &mut self
            .entries
            .back_mut()
            .expect("invariant: touch_or_insert always leaves an entry at back")
            .1;
        // Evict timestamps older than the window so `bucket.len()`
        // reflects only live entries.
        while let Some(&front) = bucket.front() {
            if now.duration_since(front) > self.window {
                bucket.pop_front();
            } else {
                break;
            }
        }
        let port_at_limit = bucket.len() >= self.threshold;
        // Cap at 2 * threshold so a port that keeps hammering stays
        // rate-limited until the ORIGINAL burst rolls out of the
        // window, but memory does not grow unboundedly.
        if bucket.len() < self.threshold * 2 {
            bucket.push_back(now);
        }

        // --- Global window: prune + check + push. Same shape as the
        // per-port bucket: prune expired timestamps, check at-limit,
        // push capped at `2 * global_threshold`.
        while let Some(&front) = self.global_window.front() {
            if now.duration_since(front) > self.window {
                self.global_window.pop_front();
            } else {
                break;
            }
        }
        let global_at_limit = self.global_window.len() >= self.global_threshold;
        if self.global_window.len() < self.global_threshold * 2 {
            self.global_window.push_back(now);
        }

        if port_at_limit || global_at_limit {
            Decision::RateLimited
        } else {
            Decision::Admitted
        }
    }

    /// Move the matching port's bucket to the back of `entries` (LRU
    /// touch), or insert a fresh empty bucket at the back. If inserting
    /// would exceed `capacity`, evict the front (LRU) entry first.
    ///
    /// O(n) scan + O(n) VecDeque shift on a hit. Acceptable for
    /// CAPACITY <= 256; a HashMap<u16, list_node> + intrusive list would
    /// be O(1) if this ever needs to scale.
    fn touch_or_insert(&mut self, port: u16) {
        if let Some(idx) = self.entries.iter().position(|(p, _)| *p == port) {
            let entry = self
                .entries
                .remove(idx)
                .expect("position returned a valid index");
            self.entries.push_back(entry);
            return;
        }
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back((port, VecDeque::new()));
    }

    #[cfg(test)]
    pub(crate) fn tracked_ports(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(crate) fn contains_port(&self, port: u16) -> bool {
        self.entries.iter().any(|(p, _)| *p == port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Global threshold high enough that no test below trips it on its
    /// own traffic volume. Tests that specifically exercise the global
    /// window pin a small explicit value; everything else uses this
    /// constant so the per-port assertions are not contaminated.
    const TEST_GLOBAL_THRESHOLD: usize = 10_000;

    #[test]
    fn rate_limit_helper_admits_below_threshold_rejects_above() {
        // Arrange: small threshold so we don't pump 30 entries by hand.
        let mut t =
            RateLimitTracker::with_config(Duration::from_secs(10), 3, TEST_GLOBAL_THRESHOLD, 256);
        let now = Instant::now();
        let port = 5555;

        // Act + Assert: the first `threshold` rejected hits are Admitted
        // (caller will return 400); subsequent hits within the window
        // are RateLimited (caller will return 429).
        assert_eq!(t.record_rejection(port, now), Decision::Admitted);
        assert_eq!(t.record_rejection(port, now), Decision::Admitted);
        assert_eq!(t.record_rejection(port, now), Decision::Admitted);
        assert_eq!(t.record_rejection(port, now), Decision::RateLimited);
        assert_eq!(t.record_rejection(port, now), Decision::RateLimited);
    }

    #[test]
    fn rate_limit_admits_again_after_window_expires() {
        // Arrange: short window so we can fast-forward via Instant math.
        let mut t =
            RateLimitTracker::with_config(Duration::from_millis(10), 2, TEST_GLOBAL_THRESHOLD, 256);
        let port = 5555;
        let t0 = Instant::now();

        // Act: hit the threshold within the window.
        assert_eq!(t.record_rejection(port, t0), Decision::Admitted);
        assert_eq!(t.record_rejection(port, t0), Decision::Admitted);
        assert_eq!(t.record_rejection(port, t0), Decision::RateLimited);

        // Assert: once the prior timestamps roll out of the window, a
        // new hit is admitted again. This is the auto-recovery path:
        // ports that briefly burst are not punished forever.
        let later = t0 + Duration::from_millis(20);
        assert_eq!(t.record_rejection(port, later), Decision::Admitted);
    }

    #[test]
    fn rate_limit_per_port_independent() {
        // Arrange.
        let mut t =
            RateLimitTracker::with_config(Duration::from_secs(10), 2, TEST_GLOBAL_THRESHOLD, 256);
        let now = Instant::now();

        // Act + Assert: different ports must not share a budget.
        assert_eq!(t.record_rejection(1000, now), Decision::Admitted);
        assert_eq!(t.record_rejection(1000, now), Decision::Admitted);
        assert_eq!(t.record_rejection(1000, now), Decision::RateLimited);
        // Port 2000 starts fresh.
        assert_eq!(t.record_rejection(2000, now), Decision::Admitted);
        assert_eq!(t.record_rejection(2000, now), Decision::Admitted);
        assert_eq!(t.record_rejection(2000, now), Decision::RateLimited);
    }

    #[test]
    fn rate_limit_evicts_lru_when_tracker_full() {
        // Arrange: capacity 3 so we can fill it deterministically.
        let mut t =
            RateLimitTracker::with_config(Duration::from_secs(10), 30, TEST_GLOBAL_THRESHOLD, 3);
        let now = Instant::now();

        // Act: fill the tracker.
        t.record_rejection(1, now);
        t.record_rejection(2, now);
        t.record_rejection(3, now);
        assert_eq!(t.tracked_ports(), 3);
        assert!(t.contains_port(1));

        // Insert a 4th distinct port. Port 1 (LRU) must be evicted.
        t.record_rejection(4, now);

        // Assert: tracker still bounded; LRU port gone, others retained.
        assert_eq!(t.tracked_ports(), 3);
        assert!(!t.contains_port(1), "LRU port should have been evicted");
        assert!(t.contains_port(2));
        assert!(t.contains_port(3));
        assert!(t.contains_port(4));
    }

    #[test]
    fn rate_limit_touch_resets_lru_position() {
        // Arrange: same capacity-3 tracker.
        let mut t =
            RateLimitTracker::with_config(Duration::from_secs(10), 30, TEST_GLOBAL_THRESHOLD, 3);
        let now = Instant::now();
        t.record_rejection(1, now);
        t.record_rejection(2, now);
        t.record_rejection(3, now);

        // Act: touch port 1 -> moves it to the back (most-recent).
        t.record_rejection(1, now);
        // Now port 2 is LRU; inserting a new port should evict 2.
        t.record_rejection(4, now);

        // Assert: touched port survived; the actual LRU was evicted.
        assert!(t.contains_port(1));
        assert!(!t.contains_port(2));
        assert!(t.contains_port(3));
        assert!(t.contains_port(4));
    }

    #[test]
    fn rate_limit_bucket_is_bounded_under_sustained_abuse() {
        // Arrange.
        let mut t =
            RateLimitTracker::with_config(Duration::from_secs(10), 5, TEST_GLOBAL_THRESHOLD, 256);
        let now = Instant::now();

        // Act: hammer one port far past the threshold.
        for _ in 0..1_000 {
            t.record_rejection(7777, now);
        }

        // Assert: tracker is still sane (no panic, no unbounded growth
        // detectable via tracked_ports). The internal bucket cap of
        // `2 * threshold` is verified by the absence of crashes /
        // hangs and by the tracker still holding exactly one entry.
        assert!(t.contains_port(7777));
        assert_eq!(t.tracked_ports(), 1);
    }

    /// Threat-model regression: a port-spray attacker opens a fresh
    /// loopback connection per hit, so each request lands on a distinct
    /// ephemeral source port and no per-port bucket ever crosses the
    /// per-port threshold. The listener-wide window must catch this
    /// pattern by counting raw rejection volume regardless of source.
    #[test]
    fn rate_limit_global_window_catches_port_spray() {
        // Arrange: per-port threshold is effectively unlimited (each
        // distinct port only ever sees one hit), global threshold is
        // small (4) so we can prove the global guard is the gate.
        let mut t = RateLimitTracker::with_config(
            Duration::from_secs(10),
            // Per-port threshold high enough that a single hit per
            // port (the port-spray attacker's pattern) cannot fill any
            // bucket.
            10_000,
            // Global threshold: fires after 4 rejections accumulate
            // across all source ports.
            4,
            256,
        );
        let now = Instant::now();

        // Act: hit 4 distinct ports, one rejection each. Each per-port
        // bucket has exactly 1 entry (well under 10_000); the global
        // window climbs 0 -> 1 -> 2 -> 3 -> 4. Each pre-push check
        // sees a count strictly less than 4, so all 4 are Admitted.
        assert_eq!(t.record_rejection(1, now), Decision::Admitted);
        assert_eq!(t.record_rejection(2, now), Decision::Admitted);
        assert_eq!(t.record_rejection(3, now), Decision::Admitted);
        assert_eq!(t.record_rejection(4, now), Decision::Admitted);

        // Assert: 5th distinct port. Per-port bucket would still admit
        // (1 entry, far below 10_000), but the global window's
        // pre-push count is 4 -- at the threshold -- so the hit is
        // rate-limited. The global guard caught the port-spray.
        assert_eq!(t.record_rejection(5, now), Decision::RateLimited);
        // Subsequent fresh ports remain rate-limited until the global
        // window drains.
        assert_eq!(t.record_rejection(6, now), Decision::RateLimited);
        assert_eq!(t.record_rejection(7, now), Decision::RateLimited);
    }
}
