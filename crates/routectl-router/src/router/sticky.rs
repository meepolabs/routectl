//! Sticky seat ordering + capacity snapshots.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::quota::placement::{QuotaDecision, SeatQuota};

use super::Router;

/// Minimum seconds between two quota-placement diagnostics in one process.
///
/// The fall-through arms fire on EVERY birth pick of an unobserved pool, which
/// on a fresh process is every new conversation, so an unthrottled line would
/// turn the normal cap-dormant state into a log stream. The counters below
/// stay exact regardless; the emitted line reports their running totals.
const QUOTA_PLACEMENT_LOG_INTERVAL_SECS: u64 = 300;

/// Epoch-second stamp of the last emitted quota-placement diagnostic. A
/// process-wide stamp, not a per-pool one: what this bounds is repeats across
/// REQUESTS, which no per-request scope can see.
static QUOTA_PLACEMENT_LOG_STAMP: AtomicU64 = AtomicU64::new(0);

/// Seconds since the Unix epoch, `0` on a pre-epoch clock (which then
/// suppresses rather than emits -- the safe direction for a bounded log).
fn now_epoch_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// Claim the right to emit one quota-placement line at `now_secs`, or refuse
/// because the interval has not elapsed. One compare-and-swap, so concurrent
/// claimants yield exactly one winner; `saturating_sub` makes a backwards
/// clock jump suppress rather than re-open the window.
fn claim_quota_placement_log(now_secs: u64) -> bool {
    let last = QUOTA_PLACEMENT_LOG_STAMP.load(Ordering::Relaxed);
    if now_secs.saturating_sub(last) < QUOTA_PLACEMENT_LOG_INTERVAL_SECS {
        return false;
    }
    QUOTA_PLACEMENT_LOG_STAMP
        .compare_exchange(last, now_secs, Ordering::Relaxed, Ordering::Relaxed)
        .is_ok()
}

impl Router {
    /// Non-mutating read of the capacity gate for the seat / model keyed by
    /// `state_key`. Returns `None` when no state slot exists. This is the
    /// `&self`-borrow read surface used by sticky least-loaded selection; it
    /// must never go through the `try_dispatch`-based `breaker_open_for`
    /// anti-pattern, which would claim a half-open probe slot just to read.
    pub(super) fn capacity_snapshot_for(
        &self,
        state_key: &str,
        now: Instant,
    ) -> Option<crate::runtime_state::CapacitySnapshot> {
        self.state
            .get(state_key)
            .map(|s| s.lock().capacity_snapshot(now))
    }

    /// The per-seat subscription-quota tiers for a birth pick over this pool,
    /// or an EMPTY vec when quota must contribute nothing.
    ///
    /// Empty is the whole of what the kill switch does on the placement side:
    /// off, this returns before any store read, and the pure selector then
    /// ranks on RPM headroom exactly as it did before quota existed -- no cap
    /// consulted, no quota state touched, no diagnostic emitted. The feed and
    /// the store keep running either way, so re-enabling is instant rather
    /// than a re-observe.
    ///
    /// Also empty for a provider that curates no short recovering window,
    /// which is how an uncurated egress stays dormant by construction.
    ///
    /// The provider kind is read off each SEAT's own `[providers]` entry
    /// rather than the model's `provider` value: a pool-backed model's
    /// `provider` names the pool, so resolving the kind from it would miss and
    /// leave every pool permanently uncurated (silently dormant quota).
    fn quota_tiers_for_birth(&self, seats: &[crate::seat_pool::SeatTarget]) -> Vec<SeatQuota> {
        if !self.config.seat_quota.enabled {
            return Vec::new();
        }
        let provider_kind = seats.first().and_then(|seat| {
            self.config
                .providers
                .get(&seat.provider_name)
                .map(crate::config::ProviderEntry::kind_str)
        });
        let keys: Vec<Option<crate::quota::key::SeatKey>> = seats
            .iter()
            .map(|seat| crate::quota::key::seat_key_for_secret_ref(seat.auth_secret_ref.as_ref()))
            .collect();
        crate::quota::placement::seat_tiers(
            &self.quota_store,
            &keys,
            provider_kind,
            &crate::quota::freshness::ObservationStamp::now(),
        )
    }

    /// Count one quota-placement decision and emit at most one throttled line
    /// per interval carrying every arm's running total.
    ///
    /// Nothing here names a session, a credential, an account or a header: the
    /// only identifiers are the routectl-internal model nickname and the arm
    /// that ran, and the only figures are counters.
    fn record_quota_placement(&self, decision: QuotaDecision, nickname: &str) {
        let totals = match decision {
            // Off, or nothing to decide on. A dormant pick is not an event:
            // counting it would make the switch observable in the very
            // diagnostics its OFF position must leave silent.
            QuotaDecision::Dormant => return,
            _ => self.metrics.incr_quota_placement(decision),
        };
        if !claim_quota_placement_log(now_epoch_secs()) {
            return;
        }
        if decision.placed() {
            tracing::debug!(
                event = "quota_placement",
                model = %nickname,
                arm = ?decision,
                below_cap_total = totals.below_cap,
                all_capped_total = totals.all_capped,
                mixed_unknown_total = totals.mixed_unknown,
                all_unknown_total = totals.all_unknown,
                "subscription-quota partition chose the birth seat",
            );
        } else {
            tracing::warn!(
                event = "quota_placement_fallback",
                model = %nickname,
                arm = ?decision,
                below_cap_total = totals.below_cap,
                all_capped_total = totals.all_capped,
                mixed_unknown_total = totals.mixed_unknown,
                all_unknown_total = totals.all_unknown,
                "subscription-quota evidence was incomplete for this pool; \
                 birth seat chosen by the unchanged capacity ranking",
            );
        }
    }

    /// The walk order for a KEYLESS request on a multi-seat sticky pool, plus
    /// the decision token to record.
    ///
    /// No session key means no pin to read and none to write, so this makes a
    /// placement and not a sticky decision. It still consults quota: a keyless
    /// request has no warm cache to protect, and cache preservation is the only
    /// thing the milestone ranks above quota fairness.
    ///
    /// Falls back to the unchanged fill-first walk whenever the partition
    /// declines, and reports the fall-back through the same token the collapse
    /// has always used so an operator's view of that regime does not change.
    pub(super) fn keyless_seat_order(
        &self,
        seats: &[crate::seat_pool::SeatTarget],
        model: &crate::resolved::ResolvedModel,
    ) -> (Vec<usize>, Option<&'static str>) {
        let nickname = model.nickname.as_str();
        let quota = self.quota_tiers_for_birth(seats);
        let mut decision = QuotaDecision::Dormant;
        let ordered = if quota.is_empty() {
            None
        } else {
            let now = Instant::now();
            let snapshots = self.gather_capacity_snapshots(seats, nickname, now);
            crate::seat_pool::keyless_quota_order(
                seats.len(),
                &snapshots,
                &quota,
                self.sticky_pins.next_tiebreak(),
                &mut decision,
            )
        };
        self.record_quota_placement(decision, nickname);
        match ordered {
            Some(order) => (order, Some("keyless_quota")),
            None => (
                crate::seat_pool::seat_order_for_request(
                    model.rotation_key(),
                    seats.len(),
                    crate::config::SeatSelection::StickyLeastLoaded,
                    &self.round_robin,
                ),
                Some("keyless_fill_first"),
            ),
        }
    }

    /// Resolve the sticky least-loaded seat walk order for `key` over a
    /// multi-seat pool. Resolves the pin (with its one-time overflow marker)
    /// FIRST, gathers the per-seat capacity snapshots (one lock each; N is
    /// small and locks are uncontended), then asks the pure selector for the
    /// walk order and a
    /// [`SelectionOutcome`](crate::seat_pool::SelectionOutcome). On a birth it
    /// pins the chosen home (`repinned: false`); on a one-time overflow-repin
    /// it pins the new home (`repinned: true`). A healthy home, an
    /// already-repinned home, or a no-healthy-sibling case stays put with no
    /// pin write -- the one-time cap + hysteresis. Never logs the raw session
    /// key.
    ///
    /// Subscription-quota tiers are gathered for the BIRTH candidate set only
    /// (the pure selector ignores them on every other path), and only while
    /// the kill switch is on. A healthy pin therefore never reads quota state
    /// at all, so no soft cap can move a warm session.
    ///
    /// Returns the walk order paired with a fixed-vocabulary
    /// `selection_decision` token mapped from the `SelectionOutcome`
    /// (observability only -- the pin writes, logs, and returned order are
    /// byte-for-byte unchanged from before the token was added). The quota
    /// partition changes WHICH seat a birth picks and never that vocabulary.
    pub(super) fn sticky_seat_order(
        &self,
        seats: &[crate::seat_pool::SeatTarget],
        key: &str,
        nickname: &str,
    ) -> (Vec<usize>, &'static str) {
        // A pinned member no longer present in this pool resolves to None
        // -> treated as a miss (re-pick), and `repinned` resets to false on
        // the fresh birth -- correct.
        let pin: Option<(usize, bool)> = self.sticky_pins.get(key).and_then(|p| {
            seats
                .iter()
                .position(|s| s.provider_name == p.member)
                .map(|i| (i, p.repinned))
        });

        let now = Instant::now();
        let snapshots = self.gather_capacity_snapshots(seats, nickname, now);

        // Advance the anti-herd counter only when a pick is actually
        // attempted: a miss, or a hit whose home is non-dispatchable and not
        // yet repinned. A sticky-stay does not consume tiebreak.
        let will_attempt_pick = match pin {
            None => true,
            Some((home, repinned)) => !snapshots[home].is_dispatchable() && !repinned,
        };
        let tiebreak = if will_attempt_pick {
            self.sticky_pins.next_tiebreak()
        } else {
            0
        };

        // Read quota ONLY for a genuine birth. A hit -- healthy or migrating
        // -- never consults it, so the store is not even touched.
        let quota = if pin.is_none() {
            self.quota_tiers_for_birth(seats)
        } else {
            Vec::new()
        };

        let (order, outcome, quota_decision) = crate::seat_pool::sticky_least_loaded_order(
            seats.len(),
            pin.map(|(i, _)| i),
            pin.is_some_and(|(_, r)| r),
            &snapshots,
            tiebreak,
            &quota,
        );
        self.record_quota_placement(quota_decision, nickname);
        let token = self.apply_sticky_outcome(key, nickname, seats, outcome);
        (order, token)
    }

    /// Gather the per-seat capacity snapshots for sticky least-loaded
    /// selection (one lock each; N is small and locks are uncontended). The
    /// overflow check needs every seat's snapshot, including the pinned
    /// home's, so this reads ALL seats (both hit and miss).
    pub(super) fn gather_capacity_snapshots(
        &self,
        seats: &[crate::seat_pool::SeatTarget],
        nickname: &str,
        now: Instant,
    ) -> Vec<crate::runtime_state::CapacitySnapshot> {
        seats
            .iter()
            .map(|s| {
                self.capacity_snapshot_for(&s.state_key_for(nickname), now)
                    .unwrap_or(
                        // Defensive: a seat with no state slot should never
                        // happen (install creates one per seat). If it does,
                        // fail safe -- treat it as non-dispatchable so it is
                        // excluded from a pick rather than chosen as the most-
                        // attractive home. It still appears in the fallback
                        // order, and the existing gate stays authoritative.
                        crate::runtime_state::CapacitySnapshot {
                            rpm_available: Some(0.0),
                            circuit: crate::runtime_state::CircuitPhase::Open,
                        },
                    )
            })
            .collect()
    }

    /// Apply the pin write implied by `outcome` and return the fixed-vocabulary
    /// `selection_decision` token. A birth pins the chosen home
    /// (`repinned: false`); a one-time overflow-repin pins the new home
    /// (`repinned: true`); a stay or no-healthy case writes nothing. Never
    /// logs the raw session key.
    pub(super) fn apply_sticky_outcome(
        &self,
        key: &str,
        nickname: &str,
        seats: &[crate::seat_pool::SeatTarget],
        outcome: crate::seat_pool::SelectionOutcome,
    ) -> &'static str {
        match outcome {
            crate::seat_pool::SelectionOutcome::Birth { home } => {
                let member = seats[home].provider_name.clone();
                tracing::debug!(
                    state_key = %seats[home].state_key_for(nickname),
                    member = %member,
                    "sticky least-loaded birth pick: pinned session to seat"
                );
                self.sticky_pins.put(
                    key,
                    crate::seat_pool::SeatPin {
                        member,
                        repinned: false,
                    },
                );
                "birth_pick"
            }
            crate::seat_pool::SelectionOutcome::OverflowRepin { home } => {
                let member = seats[home].provider_name.clone();
                tracing::debug!(
                    state_key = %seats[home].state_key_for(nickname),
                    member = %member,
                    "sticky least-loaded overflow-repin: migrated session to healthy sibling"
                );
                self.sticky_pins.put(
                    key,
                    crate::seat_pool::SeatPin {
                        member,
                        repinned: true,
                    },
                );
                "overflow_repin"
            }
            crate::seat_pool::SelectionOutcome::Stay { .. } => "sticky_stay",
            crate::seat_pool::SelectionOutcome::DeferNoHealthy => "defer_no_healthy",
        }
    }
}
