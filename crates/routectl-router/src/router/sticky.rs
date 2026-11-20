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
    /// multi-seat pool. See [`Self::sticky_seat_placement`].
    pub(super) fn sticky_seat_order(
        &self,
        seats: &[crate::seat_pool::SeatTarget],
        key: &str,
        nickname: &str,
        upstream: &str,
    ) -> Vec<usize> {
        self.sticky_seat_placement(seats, key, nickname, upstream).0
    }

    /// The sticky least-loaded walk order for `key`, and which pin write, if
    /// any, this turn won.
    ///
    /// Resolves the pin FIRST, gathers the per-seat capacity snapshots (one
    /// lock each; N is small and locks are uncontended), then asks the pure
    /// selector for the walk order and a
    /// [`SelectionOutcome`](crate::seat_pool::SelectionOutcome), and applies
    /// it with [`Self::apply_sticky_outcome`]. A turn that lost its pin
    /// write to a concurrent turn of the same conversation walks from the
    /// winner's seat instead of its own pick. Never logs the raw session key.
    ///
    /// Pace-rule standings, for the conversation's upstream model
    /// `upstream`, are read only when a pick is made: a birth, or a move off
    /// a home with a lasting outage. A serving pin never reads usage, so no
    /// reading can move a warm conversation and a recovered seat never pulls
    /// one back.
    pub(super) fn sticky_seat_placement(
        &self,
        seats: &[crate::seat_pool::SeatTarget],
        key: &str,
        nickname: &str,
        upstream: &str,
    ) -> (Vec<usize>, PinWrite) {
        // A pinned member no longer present in this pool resolves to None
        // and is treated as a miss; the birth then replaces that pin.
        let held = self.sticky_pins.get(key);
        let pin: Option<usize> = held
            .as_ref()
            .and_then(|p| seats.iter().position(|s| s.provider_name == p.member));

        let now = Instant::now();
        let snapshots = self.gather_capacity_snapshots(seats, nickname, now);

        let will_attempt_pick = pin
            .is_none_or(|home| crate::quota::placement::move_trigger(&snapshots[home]).is_some());
        let (tiebreak, standings) = if will_attempt_pick {
            (
                self.sticky_pins.next_tiebreak(),
                self.birth_standings(seats, upstream),
            )
        } else {
            (0, Vec::new())
        };

        let (order, outcome) = crate::seat_pool::sticky_least_loaded_order(
            seats.len(),
            pin,
            &snapshots,
            tiebreak,
            &standings,
        );
        let expected = held.as_ref().map(|p| p.member.as_str());
        let write = self.apply_sticky_outcome(key, nickname, seats, outcome, expected);
        match write {
            PinWrite::Adopted { home: Some(home) } => {
                (crate::seat_pool::order_home_first(home, seats.len()), write)
            }
            _ => (order, write),
        }
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

    /// Apply the pin write implied by `outcome` and report which one this
    /// turn won. `expected_member` is the pin the decision was made from:
    /// `None` when the key held none, else its member (the home, or a member
    /// this pool no longer serves). Every write is conditional on the key
    /// still holding exactly that, so of several turns deciding from one pin,
    /// one writes and the rest adopt the winner's pin and write nothing. A
    /// stay or deferred pick writes nothing. Never logs the raw session key.
    pub(super) fn apply_sticky_outcome(
        &self,
        key: &str,
        nickname: &str,
        seats: &[crate::seat_pool::SeatTarget],
        outcome: crate::seat_pool::SelectionOutcome,
        expected_member: Option<&str>,
    ) -> PinWrite {
        use crate::seat_pool::{SeatPin, SelectionOutcome};

        let adopt = |winner: Option<SeatPin>| PinWrite::Adopted {
            home: winner.and_then(|w| seats.iter().position(|s| s.provider_name == w.member)),
        };
        match outcome {
            SelectionOutcome::Birth { home } => {
                let member = seats[home].provider_name.clone();
                let pin = SeatPin {
                    member: member.clone(),
                };
                let claimed = match expected_member {
                    None => self.sticky_pins.claim_if_absent(key, pin).map_err(Some),
                    Some(stale) => self.sticky_pins.repin_if(key, stale, pin),
                };
                match claimed {
                    Ok(()) => {
                        tracing::debug!(
                            state_key = %routectl_core::sanitize_for_log(&seats[home].state_key_for(nickname)),
                            member = %member,
                            "sticky least-loaded birth pick: pinned session to seat"
                        );
                        PinWrite::Born { home }
                    }
                    Err(winner) => adopt(winner),
                }
            }
            SelectionOutcome::Repin { home, trigger } => {
                let Some(expected) = expected_member else {
                    return PinWrite::None;
                };
                let member = seats[home].provider_name.clone();
                match self.sticky_pins.repin_if(
                    key,
                    expected,
                    SeatPin {
                        member: member.clone(),
                    },
                ) {
                    Ok(()) => {
                        tracing::debug!(
                            state_key = %routectl_core::sanitize_for_log(&seats[home].state_key_for(nickname)),
                            member = %member,
                            trigger = trigger.as_str(),
                            "sticky least-loaded repin: moved session off a seat with a lasting outage"
                        );
                        PinWrite::Moved { home, trigger }
                    }
                    Err(winner) => adopt(winner),
                }
            }
            SelectionOutcome::Stay { .. } | SelectionOutcome::DeferNoHealthy => PinWrite::None,
        }
    }
}

/// Which pin write one sticky selection made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PinWrite {
    /// Nothing to write: the home kept the conversation, or no seat could
    /// take a new one.
    None,
    /// This turn's birth claim won; the conversation is pinned to `home`.
    Born { home: usize },
    /// This turn's move won: the pin left its home for `trigger` and now
    /// names `home`.
    Moved {
        home: usize,
        trigger: routectl_core::EligibilityReason,
    },
    /// A concurrent turn of the same conversation wrote first; this turn
    /// walks from that pin's seat, `None` when the pin names no seat here
    /// (it was evicted, or names a member this pool no longer serves).
    Adopted { home: Option<usize> },
}
