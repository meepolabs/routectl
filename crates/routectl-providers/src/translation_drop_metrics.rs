//! Process-wide, per-request-tallied counters for deliberate translation-time
//! losses in the egress `translate_*` / `build_*` functions, keyed on
//! `(lane, class)`. Two vocabularies live here -- DROPS and POLICY ACTIONS,
//! distinguished under "Drop vs. policy action" below. Each fix/sweep arm calls
//! [`record_translation_drop`] once per REQUEST from its own existing
//! tally-and-flush call site (mirroring `ReasoningSkipTally` /
//! `CitationsDropTally` in `bedrock::converse::messages`) -- never once per
//! dropped block -- naming its own `lane` and `drop_class` string literals.
//! There is no shared enum or fixed array to edit, so independent arms never
//! collide on this file.
//!
//! # Drop vs. policy action: which counter an arm belongs on
//!
//! The axis is WHOSE decision forces the loss, not whether a field exists to
//! carry the value. A DROP is a loss the upstream contract COMPELS: no field
//! carries the value, or the upstream rejects the combination that would carry
//! it ([`record_translation_drop`]). A POLICY ACTION is a loss routectl ITSELF
//! chose while the upstream would have accepted the value -- a privacy strip, a
//! managed-key precedence guard ([`record_translation_policy_action`]).
//!
//! "Could the wire carry this value?" is the WRONG test, and it misclassifies
//! real arms in this tree. Worked examples, because a future implementer
//! pattern-matches against these rather than against the rule:
//!
//! - `forcing_tool_choice_without_tools`
//!   (`openai_compat/wire_lift/tool_choice.rs`) is a DROP. The wire carries
//!   `tool_choice: "required"` fine; the upstream 400s on that value with no
//!   tools to force, so the COMBINATION is what is unrepresentable.
//! - The thinking strip when `toolChoice` forces tool use
//!   (`bedrock/converse/extras.rs`) is a DROP for the same reason: Anthropic
//!   forbids the pairing, so the upstream compels the loss.
//! - The `thinking.display` strip is a DROP: it goes because acceptance on
//!   this lane is unverified, which is a fact about the upstream contract.
//! - `client_fingerprint_stripped` is a POLICY ACTION. The bag would carry the
//!   fingerprint and the upstream would accept it; routectl withholds it from
//!   a third-party upstream on the user's behalf.
//! - The two `*_managed_key_conflict` guards are POLICY ACTIONS: the upstream
//!   would accept the override, and routectl refuses it to keep its own
//!   derived value authoritative.
//!
//! The short form: if removing the upstream's objection would let the value
//! ride, it is a drop. If the only thing standing between the value and the
//! wire is a routectl decision, it is a policy action.
//!
//! The two counters exist separately because they answer different
//! questions and one swamps the other: a policy action that fires on nearly
//! every request (a fingerprint strip) drives a shared `drop_rate()` to
//! ~100% forever, which teaches a reader to ignore the number that is
//! supposed to reveal fidelity loss. The two class vocabularies are
//! therefore DISJOINT -- no class literal appears in both a
//! [`record_translation_drop`] and a [`record_translation_policy_action`]
//! call.
//!
//! Both numerators share the one denominator, [`record_translation_lane_seen`]:
//! it counts request volume per lane, the same quantity for either rate, and
//! a lane has exactly one call site for it.
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
//! ([`translation_drop_snapshot`], [`translation_policy_action_snapshot`])
//! with no new dependency edge and no call back into router.
//!
//! The synchronization differs from that precedent deliberately, and the
//! difference is worth naming: `bedrock_validation_unmatched_total` is a
//! lock-free top-level field under a FIXED key, so an atomic with no lock is
//! exactly right there. This registry keys on a DYNAMIC `(lane, drop_class)`
//! pair discovered as call sites land, so the map itself needs a lock -- and
//! once every access already holds that lock exclusively, per-entry atomics
//! would buy nothing and would imply to a reader that lock-free access is
//! possible. Plain `u64` under the mutex is the honest shape.
//!
//! # Count vs. rate
//!
//! A raw drop count with no request-volume denominator is not actionable: a
//! lane that sees one request a day and a lane that sees a million both
//! report the same absolute number after one bad turn. [`record_translation_lane_seen`]
//! counts every request a lane's translate/build path processed, dropped or
//! not, so [`TranslationDropSnapshotEntry::drop_rate`] reports the drop RATE
//! per lane instead of an uncontextualized count.
//!
//! # Testing against this registry
//!
//! The registry is PROCESS-GLOBAL and the test runner is multi-threaded, so
//! any test asserting an exact counter value or delta races every other test
//! that touches the same key. Guard all of them with the same
//! `#[serial_test::serial(<lane>_<class>)]` name -- including tests that
//! only trigger the loss incidentally, not just the one reading the counter
//! back. A guard whose name no sibling shares excludes nothing. The rule is
//! IDENTICAL for both vocabularies: a policy-action arm owes its guard on
//! exactly the same terms as a drop arm.
//!
//! This is not hypothetical: a delta assertion added without covering its
//! already-existing sibling failed three times in six runs at 32 threads,
//! and passed every single-threaded run before that.
//!
//! Prefer a delta (read before, act, read after) over an absolute value.
//! Counters accumulate across the whole test binary, so an absolute
//! expectation is wrong the moment a second test touches the key.

use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

/// `(lane, drop_class)` label pair identifying one counted drop kind. Both
/// sides are `&'static str` literals named at the call site (e.g.
/// `("bedrock-converse", "reasoning_summary_unsupported")`). Policy-action
/// counters key on the same pair, in their own map.
pub type DropKey = (&'static str, &'static str);

#[derive(Debug, Default)]
struct Registry {
    drops: BTreeMap<DropKey, u64>,
    /// Kept in the same struct under the same mutex as `drops` because a
    /// second mutex would add a lock-ordering rule for no benefit: nothing
    /// holds both, and every access is already exclusive. Note this does NOT
    /// give a reader a consistent cross-map view -- the two snapshot
    /// accessors take the lock separately, so a pair read through them can
    /// straddle a concurrent increment. Nothing needs that consistency
    /// today; add a single both-maps accessor if something ever does.
    policy_actions: BTreeMap<DropKey, u64>,
    lane_seen: BTreeMap<&'static str, u64>,
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
        let count = reg.drops.entry((lane, drop_class)).or_default();
        *count = count.saturating_add(1);
    });
}

/// Bump the policy-action counter for `(lane, policy_class)` by one. Call
/// once per REQUEST from the existing tally-and-flush call site, never once
/// per withheld key -- a request whose extras collide on three managed keys
/// is one policy-action event, not three.
///
/// For content routectl WITHHELD or REFUSED TO OVERRIDE, where the wire
/// could have carried it. Content the wire cannot represent goes to
/// [`record_translation_drop`] instead; no class literal belongs to both.
pub fn record_translation_policy_action(lane: &'static str, policy_class: &'static str) {
    with_registry_mut(|reg| {
        let count = reg.policy_actions.entry((lane, policy_class)).or_default();
        *count = count.saturating_add(1);
    });
}

/// Bump the per-lane request-volume denominator by one. Call once per
/// request a lane's translate/build path processed, regardless of whether
/// anything was dropped -- the same place [`record_translation_drop`]'s
/// caller already calls `flush()` unconditionally on both the drop and
/// no-drop arm. One call site per lane: it is the shared denominator for
/// both the drop and the policy-action numerator, so a second site would
/// understate both rates for the whole lane.
pub fn record_translation_lane_seen(lane: &'static str) {
    with_registry_mut(|reg| {
        let count = reg.lane_seen.entry(lane).or_default();
        *count = count.saturating_add(1);
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
    #[must_use]
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
#[must_use]
pub fn translation_drop_snapshot() -> Vec<TranslationDropSnapshotEntry> {
    with_registry_mut(|reg| {
        reg.drops
            .iter()
            .map(
                |(&(lane, drop_class), &count)| TranslationDropSnapshotEntry {
                    lane,
                    drop_class,
                    drop_count: count,
                    lane_seen_count: reg.lane_seen.get(lane).copied().unwrap_or(0),
                },
            )
            .collect()
    })
}

/// One `(lane, policy_class)` counter's current state, paired with that
/// lane's request-volume denominator. Sibling to
/// [`TranslationDropSnapshotEntry`], kept a distinct type so a reader
/// cannot mix a withheld-by-policy count into a representability-loss one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TranslationPolicyActionSnapshotEntry {
    /// The lane this policy action was counted on (e.g. `"bedrock-converse"`).
    pub lane: &'static str,
    /// The policy-action classification named at the call site.
    pub policy_class: &'static str,
    /// Cumulative count of requests on which this `(lane, policy_class)`
    /// action fired at least once.
    pub action_count: u64,
    /// Cumulative count of requests [`record_translation_lane_seen`] has
    /// observed for `lane`, regardless of class. The denominator for
    /// [`action_rate`](Self::action_rate), shared with the drop counter.
    pub lane_seen_count: u64,
}

impl TranslationPolicyActionSnapshotEntry {
    /// Policy actions per request processed on this lane. `0.0` when the
    /// lane has not yet been marked seen, rather than dividing by zero.
    #[must_use]
    pub fn action_rate(&self) -> f64 {
        if self.lane_seen_count == 0 {
            0.0
        } else {
            self.action_count as f64 / self.lane_seen_count as f64
        }
    }
}

/// Every currently-registered `(lane, policy_class)` counter, in
/// deterministic `(lane, policy_class)` sort order, paired with its lane's
/// request-volume
/// denominator. Mirrors [`translation_drop_snapshot`]: an entry appears
/// once [`record_translation_policy_action`] has been called for its key at
/// least once.
#[must_use]
pub fn translation_policy_action_snapshot() -> Vec<TranslationPolicyActionSnapshotEntry> {
    with_registry_mut(|reg| {
        reg.policy_actions
            .iter()
            .map(
                |(&(lane, policy_class), &count)| TranslationPolicyActionSnapshotEntry {
                    lane,
                    policy_class,
                    action_count: count,
                    lane_seen_count: reg.lane_seen.get(lane).copied().unwrap_or(0),
                },
            )
            .collect()
    })
}

/// One lane's request-volume denominator: how many requests
/// [`record_translation_lane_seen`] has observed for `lane`. `0` when the lane
/// has never been marked seen.
///
/// Exists because [`translation_drop_snapshot`] exposes the denominator only
/// hanging off a `(lane, drop_class)` row, so reading it before any drop class
/// has fired required SEEDING a throwaway drop entry -- a read that writes.
/// Three separate test surfaces independently invented that workaround, one of
/// them mutating the registry on every read. Reading the denominator is a
/// legitimate question on its own, so it gets its own accessor.
#[must_use]
pub fn translation_lane_seen(lane: &str) -> u64 {
    with_registry_mut(|reg| reg.lane_seen.get(lane).copied().unwrap_or(0))
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
    fn translation_lane_seen_reads_the_denominator_without_seeding_a_drop_row() {
        // Arrange
        let lane = "unit-test-lane-accessor";

        // Act
        record_translation_lane_seen(lane);
        record_translation_lane_seen(lane);

        // Assert: the count is readable with NO drop class on the lane, which
        // is the whole reason this accessor exists -- the snapshot exposes the
        // denominator only via a (lane, drop_class) row.
        assert_eq!(translation_lane_seen(lane), 2);
        assert!(
            !translation_drop_snapshot().iter().any(|e| e.lane == lane),
            "reading the denominator must not create a drop row"
        );
    }

    #[test]
    fn translation_lane_seen_is_zero_for_a_lane_never_marked_seen() {
        assert_eq!(translation_lane_seen("unit-test-lane-never-seen"), 0);
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
        // Exact comparison is intentional here, unlike the epsilon check in
        // the computed-rate test: the zero-denominator branch returns the
        // literal 0.0 rather than dividing, so there is no rounding to admit.
        assert_eq!(entry.drop_rate(), 0.0);
    }

    #[test]
    fn record_translation_policy_action_creates_and_bumps_the_keyed_counter() {
        // Arrange
        let lane = "unit-test-lane-policy-bump";
        let policy_class = "unit-test-class-policy-bump";

        // Act
        record_translation_policy_action(lane, policy_class);
        record_translation_policy_action(lane, policy_class);

        // Assert
        let entry = translation_policy_action_snapshot()
            .into_iter()
            .find(|e| e.lane == lane && e.policy_class == policy_class)
            .expect("counter must appear in the snapshot after its first increment");
        assert_eq!(entry.action_count, 2);
    }

    #[test]
    fn the_same_lane_and_class_pair_counts_independently_in_each_map() {
        // Arrange: ONE pair, recorded into BOTH maps. This is what proves
        // separate storage rather than one map with two front doors -- and
        // it is the disjointness property the two vocabularies rest on, so the
        // fixture performs the experiment rather than describing it.
        let lane = "unit-test-lane-separation";
        let class = "unit-test-class-separation";

        // Act
        record_translation_policy_action(lane, class);
        record_translation_drop(lane, class);

        // Assert -- each map reports exactly its own single increment for the
        // shared pair; neither saw the other's write.
        let policy = translation_policy_action_snapshot()
            .into_iter()
            .find(|e| e.lane == lane && e.policy_class == class)
            .expect("the policy action must be recorded on its own counter");
        let drop = translation_drop_snapshot()
            .into_iter()
            .find(|e| e.lane == lane && e.drop_class == class)
            .expect("the drop must be recorded on its own counter");
        assert_eq!(policy.action_count, 1);
        assert_eq!(drop.drop_count, 1);
    }

    #[test]
    fn a_drop_never_lands_in_the_policy_action_snapshot() {
        // Arrange
        let lane = "unit-test-lane-separation-reverse";
        let class = "unit-test-class-separation-reverse";

        // Act
        record_translation_drop(lane, class);

        // Assert
        assert!(
            !translation_policy_action_snapshot()
                .iter()
                .any(|e| e.lane == lane),
            "a drop must not appear as a policy action"
        );
        assert!(
            translation_drop_snapshot()
                .iter()
                .any(|e| e.lane == lane && e.drop_class == class),
            "the drop must be recorded on its own counter"
        );
    }

    #[test]
    fn action_rate_divides_policy_actions_by_the_shared_lane_denominator() {
        // Arrange: one lane, one denominator, both numerators on it -- the
        // shape that proves the policy counter reads the SAME lane_seen map
        // the drop counter does rather than a second denominator.
        let lane = "unit-test-lane-shared-denominator";
        let drop_class = "unit-test-class-shared-drop";
        let policy_class = "unit-test-class-shared-policy";

        // Act
        for _ in 0..4 {
            record_translation_lane_seen(lane);
        }
        record_translation_drop(lane, drop_class);
        record_translation_policy_action(lane, policy_class);
        record_translation_policy_action(lane, policy_class);

        // Assert
        let policy = translation_policy_action_snapshot()
            .into_iter()
            .find(|e| e.lane == lane && e.policy_class == policy_class)
            .expect("policy counter present");
        let drop = translation_drop_snapshot()
            .into_iter()
            .find(|e| e.lane == lane && e.drop_class == drop_class)
            .expect("drop counter present");
        assert_eq!(policy.lane_seen_count, 4);
        assert_eq!(drop.lane_seen_count, 4);
        assert!((policy.action_rate() - 0.5).abs() < f64::EPSILON);
        assert!((drop.drop_rate() - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn action_rate_is_zero_when_the_lane_has_not_been_marked_seen() {
        // Arrange
        let lane = "unit-test-lane-policy-unseen";
        let policy_class = "unit-test-class-policy-unseen";

        // Act
        record_translation_policy_action(lane, policy_class);

        // Assert
        let entry = translation_policy_action_snapshot()
            .into_iter()
            .find(|e| e.lane == lane && e.policy_class == policy_class)
            .expect("counter present");
        assert_eq!(entry.lane_seen_count, 0);
        // Exact comparison is intentional, as in the drop-counter mirror: the
        // zero-denominator branch returns the literal 0.0 rather than
        // dividing, so there is no rounding to admit.
        assert_eq!(entry.action_rate(), 0.0);
    }
}
