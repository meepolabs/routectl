//! Sticky seat ordering + capacity snapshots.

use std::time::Instant;

use crate::quota::placement::Standing;

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
        account: Option<&str>,
        now: Instant,
    ) -> Option<crate::runtime_state::CapacitySnapshot> {
        let record = self.account_record(account).unwrap_or_default();
        self.state.get(state_key).map(|s| {
            s.lock()
                .capacity_snapshot_under(now, record.park(), record.unusable())
        })
    }

    /// Each seat's pace-rule standing for a conversation on upstream model
    /// `model`, index-aligned with `seats`, or EMPTY for a dormant pool (no
    /// seat has an account the meter can read), which then places on
    /// headroom and rotation alone.
    ///
    /// A seat whose account has no reading yet stands unread, so a pool with
    /// no readings at all (a fresh process) also places on headroom and
    /// rotation: every seat ties in the unread bucket.
    pub(super) fn birth_standings(
        &self,
        seats: &[crate::seat_pool::SeatTarget],
        model: &str,
    ) -> Vec<Standing> {
        let keys: Vec<Option<crate::quota::key::SeatKey>> = seats
            .iter()
            .map(|seat| crate::quota::key::seat_key_for_secret_ref(seat.auth_secret_ref.as_ref()))
            .collect();
        if keys.iter().all(Option::is_none) {
            return Vec::new();
        }
        let now = crate::quota::freshness::ObservationStamp::now();
        let readings = self.quota_store.readings(&now).accounts;
        keys.iter()
            .map(|key| {
                let reading = key.as_ref().and_then(|key| {
                    readings
                        .iter()
                        .find(|reading| reading.account == key.as_str())
                });
                crate::quota::placement::standing(reading, model, &now)
            })
            .collect()
    }

    /// The walk order for a KEYLESS request on a multi-seat sticky pool.
    ///
    /// No session key means no pin to read and none to write, so this makes a
    /// placement and not a sticky decision. It places exactly as a birth does,
    /// since a keyless request has no warm cache to protect. A dormant pool,
    /// or one with nothing dispatchable, keeps the unchanged fill-first walk.
    pub(super) fn keyless_seat_order(
        &self,
        seats: &[crate::seat_pool::SeatTarget],
        model: &crate::resolved::ResolvedModel,
    ) -> Vec<usize> {
        let standings = self.birth_standings(seats, &model.upstream);
        let ordered = if standings.is_empty() {
            None
        } else {
            let snapshots = self.gather_capacity_snapshots(seats, &model.nickname, Instant::now());
            crate::seat_pool::keyless_quota_order(
                seats.len(),
                &snapshots,
                &standings,
                self.sticky_pins.next_tiebreak(),
            )
        };
        ordered.unwrap_or_else(|| crate::seat_pool::fill_first_order(seats.len()))
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
    /// Pace-rule standings are read for a BIRTH only, for the conversation's
    /// upstream model `upstream`. A healthy pin never reads usage at all, so
    /// no reading can move a warm session; the rule changes WHICH seat a birth
    /// picks, never whether or how the pin is written.
    pub(super) fn sticky_seat_order(
        &self,
        seats: &[crate::seat_pool::SeatTarget],
        key: &str,
        nickname: &str,
        upstream: &str,
    ) -> Vec<usize> {
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

        // Read usage ONLY for a genuine birth. A hit -- healthy or migrating
        // -- never consults it, so the store is not even touched.
        let standings = if pin.is_none() {
            self.birth_standings(seats, upstream)
        } else {
            Vec::new()
        };

        let (order, outcome) = crate::seat_pool::sticky_least_loaded_order(
            seats.len(),
            pin.map(|(i, _)| i),
            pin.is_some_and(|(_, r)| r),
            &snapshots,
            tiebreak,
            &standings,
        );
        self.apply_sticky_outcome(key, nickname, seats, outcome);
        order
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
                self.capacity_snapshot_for(&s.state_key_for(nickname), s.account(), now)
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
                            credential: crate::runtime_state::CredentialState::Usable,
                            outage_remaining: None,
                        },
                    )
            })
            .collect()
    }

    /// Apply the pin write implied by `outcome`. A birth pins the chosen home
    /// (`repinned: false`); a one-time overflow-repin pins the new home
    /// (`repinned: true`); a stay or no-healthy case writes nothing. Never
    /// logs the raw session key.
    pub(super) fn apply_sticky_outcome(
        &self,
        key: &str,
        nickname: &str,
        seats: &[crate::seat_pool::SeatTarget],
        outcome: crate::seat_pool::SelectionOutcome,
    ) {
        match outcome {
            crate::seat_pool::SelectionOutcome::Birth { home } => {
                let member = seats[home].provider_name.clone();
                tracing::debug!(
                    state_key = %routectl_core::sanitize_for_log(&seats[home].state_key_for(nickname)),
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
            }
            crate::seat_pool::SelectionOutcome::OverflowRepin { home } => {
                let member = seats[home].provider_name.clone();
                tracing::debug!(
                    state_key = %routectl_core::sanitize_for_log(&seats[home].state_key_for(nickname)),
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
            }
            crate::seat_pool::SelectionOutcome::Stay { .. }
            | crate::seat_pool::SelectionOutcome::DeferNoHealthy => {}
        }
    }
}
