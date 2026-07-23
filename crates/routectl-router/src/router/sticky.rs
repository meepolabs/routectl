//! Sticky seat ordering + capacity snapshots.

use std::time::Instant;

use super::Router;

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

    /// Resolve the sticky least-loaded seat walk order for `key` over a
    /// multi-seat pool. Resolves the pin (with its one-time overflow marker)
    /// FIRST, gathers the per-seat capacity snapshots (one lock each; N is
    /// small and locks are uncontended), then asks the pure selector for the
    /// walk order and a [`SelectionOutcome`]. On a birth it pins the chosen
    /// home (`repinned: false`); on a one-time overflow-repin it pins the new
    /// home (`repinned: true`). A healthy home, an already-repinned home, or a
    /// no-healthy-sibling case stays put with no pin write -- the one-time cap
    /// + hysteresis. Never logs the raw session key.
    ///
    /// Returns the walk order paired with a fixed-vocabulary
    /// `selection_decision` token mapped from the `SelectionOutcome`
    /// (observability only -- the pin writes, logs, and returned order are
    /// byte-for-byte unchanged from before the token was added).
    pub(super) fn sticky_seat_order(
        &self,
        seats: &[crate::seat_pool::SeatTarget],
        key: &str,
    ) -> (Vec<usize>, &'static str) {
        // A pinned state_key no longer present in this pool resolves to None
        // -> treated as a miss (re-pick), and `repinned` resets to false on
        // the fresh birth -- correct.
        let pin: Option<(usize, bool)> = self.sticky_pins.get(key).and_then(|p| {
            seats
                .iter()
                .position(|s| s.state_key == p.state_key)
                .map(|i| (i, p.repinned))
        });

        let now = Instant::now();
        let snapshots = self.gather_capacity_snapshots(seats, now);

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

        let (order, outcome) = crate::seat_pool::sticky_least_loaded_order(
            seats.len(),
            pin.map(|(i, _)| i),
            pin.is_some_and(|(_, r)| r),
            &snapshots,
            tiebreak,
        );
        let token = self.apply_sticky_outcome(key, seats, outcome);
        (order, token)
    }

    /// Gather the per-seat capacity snapshots for sticky least-loaded
    /// selection (one lock each; N is small and locks are uncontended). The
    /// overflow check needs every seat's snapshot, including the pinned
    /// home's, so this reads ALL seats (both hit and miss).
    pub(super) fn gather_capacity_snapshots(
        &self,
        seats: &[crate::seat_pool::SeatTarget],
        now: Instant,
    ) -> Vec<crate::runtime_state::CapacitySnapshot> {
        seats
            .iter()
            .map(|s| {
                self.capacity_snapshot_for(&s.state_key, now).unwrap_or(
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
        seats: &[crate::seat_pool::SeatTarget],
        outcome: crate::seat_pool::SelectionOutcome,
    ) -> &'static str {
        match outcome {
            crate::seat_pool::SelectionOutcome::Birth { home } => {
                self.sticky_pins.put(
                    key,
                    crate::seat_pool::SeatPin {
                        state_key: seats[home].state_key.clone(),
                        repinned: false,
                    },
                );
                tracing::debug!(
                    state_key = %seats[home].state_key,
                    seat_label = ?seats[home].label,
                    "sticky least-loaded birth pick: pinned session to seat"
                );
                "birth_pick"
            }
            crate::seat_pool::SelectionOutcome::OverflowRepin { home } => {
                self.sticky_pins.put(
                    key,
                    crate::seat_pool::SeatPin {
                        state_key: seats[home].state_key.clone(),
                        repinned: true,
                    },
                );
                tracing::debug!(
                    state_key = %seats[home].state_key,
                    seat_label = ?seats[home].label,
                    "sticky least-loaded overflow-repin: migrated session to healthy sibling"
                );
                "overflow_repin"
            }
            crate::seat_pool::SelectionOutcome::Stay { .. } => "sticky_stay",
            crate::seat_pool::SelectionOutcome::DeferNoHealthy => "defer_no_healthy",
        }
    }
}
