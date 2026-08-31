//! Process-wide, per-request-tallied counters for deliberate translation-time
//! drops in the egress `translate_*` / `build_*` functions, keyed on
//! `(lane, drop_class)`. Each fix/sweep arm calls
//! [`record_translation_drop`] once per REQUEST from its own existing
//! tally-and-flush call site (mirroring `ReasoningSkipTally` /
//! `CitationsDropTally` in `bedrock::converse::messages`) -- never once per
//! dropped block -- naming its own `lane` and `drop_class` string literals.
//! There is no shared enum or fixed array to edit, so independent arms never
//! collide on this file.
//!
//! # Crate homing (documented here, not in a standing architecture doc)
//!
//! `routectl-router` depends on `routectl-providers`, never the reverse, and
//! the drops counted here fire from PROVIDERS-side `translate_*` / `build_*`
//! functions. A counter homed on the router's own metrics struct --
//! `bedrock_validation_unmatched_total`'s shape -- cannot be incremented from
//! inside a providers-side translate function without inverting that
//! dependency edge. So these counters live here, in `routectl-providers`,
//! and the router's snapshot reads them through the `pub` functions below
//! ([`translation_drop_snapshot`]) with no new dependency edge and no call
//! back into router. `bedrock_validation_unmatched_total`'s
//! `AtomicU64` + `incr_*()` + `fetch_add(Relaxed)` shape is still the model
//! this module copies; only its crate location differs.
//!
//! # Count vs. rate
//!
//! A raw drop count with no request-volume denominator is not actionable: a
//! lane that sees one request a day and a lane that sees a million both
//! report the same absolute number after one bad turn. [`record_translation_lane_seen`]
//! counts every request a lane's translate/build path processed, dropped or
//! not, so [`TranslationDropSnapshotEntry::drop_rate`] reports the drop RATE
//! per lane instead of an uncontextualized count.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

/// `(lane, drop_class)` label pair identifying one counted drop kind. Both
/// sides are `&'static str` literals named at the call site (e.g.
/// `("bedrock-converse", "reasoning_summary_unsupported")`).
pub type DropKey = (&'static str, &'static str);

#[derive(Debug, Default)]
struct Registry {
    drops: BTreeMap<DropKey, AtomicU64>,
    lane_seen: BTreeMap<&'static str, AtomicU64>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

/// Run `f` against the registry, recovering from a poisoned lock rather than
/// propagating the panic: only reachable if a prior holder panicked while
/// holding the lock, which nothing in this module does, and a counter
/// registry is never itself a source of correctness a panic should protect.
fn with_registry_mut<T>(f: impl FnOnce(&mut Registry) -> T) -> T {
    match registry().lock() {
        Ok(mut guard) => f(&mut guard),
        Err(poisoned) => f(&mut poisoned.into_inner()),
    }
}

/// Bump the drop counter for `(lane, drop_class)` by one. Call once per
/// REQUEST from the existing tally-and-flush call site, never once per
/// dropped block -- a request with three dropped blocks of the same class
/// is one drop event, not three.
pub fn record_translation_drop(lane: &'static str, drop_class: &'static str) {
    with_registry_mut(|reg| {
        reg.drops
            .entry((lane, drop_class))
            .or_insert_with(AtomicU64::default)
            .fetch_add(1, Ordering::Relaxed);
    });
}

/// Bump the per-lane request-volume denominator by one. Call once per
/// request a lane's translate/build path processed, regardless of whether
/// anything was dropped -- the same place [`record_translation_drop`]'s
/// caller already calls `flush()` unconditionally on both the drop and
/// no-drop arm.
pub fn record_translation_lane_seen(lane: &'static str) {
    with_registry_mut(|reg| {
        reg.lane_seen
            .entry(lane)
            .or_insert_with(AtomicU64::default)
            .fetch_add(1, Ordering::Relaxed);
    });
}

/// One `(lane, drop_class)` counter's current state, paired with that
/// lane's request-volume denominator so the reader never has to compute the
/// rate against a `drops`-only view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranslationDropSnapshotEntry {
    /// The lane this drop was counted on (e.g. `"bedrock-converse"`).
    pub lane: &'static str,
    /// The deliberate-drop classification named at the call site.
    pub drop_class: &'static str,
    /// Cumulative count of requests on which this `(lane, drop_class)` drop
    /// fired at least once.
    pub drop_count: u64,
    /// Cumulative count of requests [`record_translation_lane_seen`] has
    /// observed for `lane`, regardless of drop_class. The denominator for
    /// [`drop_rate`](Self::drop_rate).
    pub lane_seen_count: u64,
}

impl TranslationDropSnapshotEntry {
    /// Drops per request processed on this lane. `0.0` when the lane has
    /// not yet been marked seen -- e.g. before the dependent task wiring
    /// [`record_translation_lane_seen`] for this lane has landed -- rather
    /// than dividing by zero.
    pub fn drop_rate(&self) -> f64 {
        if self.lane_seen_count == 0 {
            0.0
        } else {
            self.drop_count as f64 / self.lane_seen_count as f64
        }
    }
}

/// Every currently-registered `(lane, drop_class)` counter, in deterministic
/// (lane, drop_class) sort order, paired with its lane's request-volume
/// denominator. Read by the router's metrics snapshot; a `(lane,
/// drop_class)` pair with a zero count is included once
/// [`record_translation_drop`] has been called for it at least once (the
/// map entry is created on first increment), matching the
/// grow-as-arms-land model this module is built for.
pub fn translation_drop_snapshot() -> Vec<TranslationDropSnapshotEntry> {
    with_registry_mut(|reg| {
        reg.drops
            .iter()
            .map(
                |(&(lane, drop_class), count)| TranslationDropSnapshotEntry {
                    lane,
                    drop_class,
                    drop_count: count.load(Ordering::Relaxed),
                    lane_seen_count: reg
                        .lane_seen
                        .get(lane)
                        .map_or(0, |c| c.load(Ordering::Relaxed)),
                },
            )
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Each test below uses its own lane/drop_class literal pair, reserved
    // to that test, so concurrent test execution against this
    // process-global registry never lets one test's counters leak into
    // another's assertions.

    #[test]
    fn record_translation_drop_creates_and_bumps_the_keyed_counter() {
        // Arrange
        let lane = "unit-test-lane-bump";
        let drop_class = "unit-test-class-bump";

        // Act
        record_translation_drop(lane, drop_class);
        record_translation_drop(lane, drop_class);

        // Assert
        let entry = translation_drop_snapshot()
            .into_iter()
            .find(|e| e.lane == lane && e.drop_class == drop_class)
            .expect("counter must appear in the snapshot after its first increment");
        assert_eq!(entry.drop_count, 2);
    }

    #[test]
    fn distinct_drop_classes_on_the_same_lane_do_not_share_a_counter() {
        // Arrange
        let lane = "unit-test-lane-distinct";
        let class_a = "unit-test-class-distinct-a";
        let class_b = "unit-test-class-distinct-b";

        // Act
        record_translation_drop(lane, class_a);
        record_translation_drop(lane, class_a);
        record_translation_drop(lane, class_b);

        // Assert
        let snapshot = translation_drop_snapshot();
        let a = snapshot
            .iter()
            .find(|e| e.lane == lane && e.drop_class == class_a)
            .expect("class_a counter present");
        let b = snapshot
            .iter()
            .find(|e| e.lane == lane && e.drop_class == class_b)
            .expect("class_b counter present");
        assert_eq!(a.drop_count, 2);
        assert_eq!(b.drop_count, 1);
    }

    #[test]
    fn drop_rate_divides_drops_by_the_lane_seen_denominator() {
        // Arrange
        let lane = "unit-test-lane-rate";
        let drop_class = "unit-test-class-rate";

        // Act
        record_translation_lane_seen(lane);
        record_translation_lane_seen(lane);
        record_translation_lane_seen(lane);
        record_translation_lane_seen(lane);
        record_translation_drop(lane, drop_class);

        // Assert
        let entry = translation_drop_snapshot()
            .into_iter()
            .find(|e| e.lane == lane && e.drop_class == drop_class)
            .expect("counter present");
        assert_eq!(entry.lane_seen_count, 4);
        assert_eq!(entry.drop_count, 1);
        assert!((entry.drop_rate() - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn drop_rate_is_zero_when_the_lane_has_not_been_marked_seen() {
        // Arrange
        let lane = "unit-test-lane-unseen";
        let drop_class = "unit-test-class-unseen";

        // Act
        record_translation_drop(lane, drop_class);

        // Assert
        let entry = translation_drop_snapshot()
            .into_iter()
            .find(|e| e.lane == lane && e.drop_class == drop_class)
            .expect("counter present");
        assert_eq!(entry.lane_seen_count, 0);
        assert_eq!(entry.drop_rate(), 0.0);
    }
}
