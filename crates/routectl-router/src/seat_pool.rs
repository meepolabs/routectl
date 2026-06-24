//! Seat-pool expansion for OAuth credential pools.
//!
//! A model whose primary `api_key_ref` is a bare-pool `oauth://<provider>`
//! (no `#label`) backed by MORE THAN ONE stored seat dispatches across all
//! of its seats. The seat set is fixed at build time: the factory builds one
//! [`SeatTarget`] (a seat-pinned provider + its own `state_key`) per seat.
//!
//! At request time the router asks [`seat_order_for_request`] for the order
//! in which to walk those seats. The seats then slot into the existing
//! fallback chain as ordinary dispatch hops -- the per-target circuit
//! breaker, retry caps, probe fast-fail, and D1 `Retry-After` park all key
//! off the per-seat `state_key`, so seat rotation and cooling are delivered
//! by machinery that already exists. This module owns only the expansion
//! glue and the round-robin counter, keeping it out of the oversized
//! `router.rs`.

use std::collections::BTreeMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use lru::LruCache;
use parking_lot::Mutex;
use routectl_auth::SecretRef;
use routectl_core::Provider;

use crate::config::SeatSelection;
use crate::runtime_state::{CapacitySnapshot, CircuitPhase};

/// One credential seat of a pooled model: a seat-pinned provider instance
/// plus the runtime-state key that gives this seat its OWN circuit breaker
/// and RPM bucket entry in `Router.state`. Built once at startup (the seat
/// set is fixed) and cloned by reference (the `Arc`s) on every dispatch.
#[derive(Clone)]
pub(crate) struct SeatTarget {
    /// Seat label (`None` for the default/pool seat, `Some(label)` for a
    /// labeled seat). Retained for tracing and `state_key` derivation.
    pub label: Option<String>,
    /// Key into `Router.state` for this seat's breaker + RPM bucket.
    /// Stable across a Router rebuild so `carry_over_runtime_state_from`
    /// matches a surviving seat and preserves its counters / park.
    pub state_key: String,
    /// Seat-pinned provider instance (built from a labeled `SecretRef`).
    pub provider: Arc<dyn Provider>,
    /// Source `SecretRef` for this specific seat. Retained for diagnostics
    /// (Debug / tracing of seat identity); the 401 self-heal does NOT read
    /// this field -- it works through the seat-pinned `provider`'s own
    /// `ManagedToken`, which already refreshes the correct seat.
    pub auth_secret_ref: Option<SecretRef>,
}

impl std::fmt::Debug for SeatTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SeatTarget")
            .field("label", &self.label)
            .field("state_key", &self.state_key)
            .field("provider_id", &self.provider.id())
            .field(
                "auth_secret_ref",
                &self.auth_secret_ref.as_ref().map(|sr| sr.to_string()),
            )
            .finish()
    }
}

/// Derive the runtime-state key for one seat of a pooled model.
///
/// The DEFAULT seat (label `None`) keys as the bare `nickname` -- so a
/// single-seat pool is byte-for-byte identical to a non-pooled model
/// (`state_key == nickname`). A LABELED seat keys as `"{nickname}#{label}"`,
/// mirroring the established `provider#label` convention used by
/// `oauth::seat_key` and `SecretRef`'s `Display`, which keeps the key
/// operator-readable in logs.
///
/// Collision boundary: a labeled-seat key collides with a real model
/// nickname only if an operator declares a SEPARATE `[models.X]` whose
/// nickname is literally `"{nickname}#{label}"` AND that label exists as a
/// seat of the pooled `nickname`. Since labeled-seat keys are only minted
/// for genuinely multi-seat oauth pools, this requires a deliberately
/// adversarial config; the bare-nickname default-seat key (the common
/// single-seat case) can never collide.
pub(crate) fn seat_state_key(nickname: &str, label: Option<&str>) -> String {
    match label {
        Some(label) => format!("{nickname}#{label}"),
        None => nickname.to_string(),
    }
}

/// Per-pool round-robin cursor set. Holds one [`AtomicUsize`] per pooled
/// model nickname; `RoundRobin` selection advances the cursor by one per
/// request via `fetch_add`. Lives on the `Router` alongside `state`.
///
/// The cursor is deliberately NOT carried over on a Router rebuild -- a
/// reset to seat 0 on hot-reload is benign at single-operator scale (the
/// only cost is one request landing on the default seat instead of the
/// next-in-rotation seat). `FillFirst` pools need no cursor and are never
/// inserted here.
#[derive(Debug, Default)]
pub(crate) struct RoundRobinCursors {
    cursors: BTreeMap<String, AtomicUsize>,
}

impl RoundRobinCursors {
    /// Register a round-robin cursor for a pooled nickname. Idempotent:
    /// re-registering keeps the existing cursor. Call at install time for
    /// each pooled model whose `seat_selection` is `RoundRobin`.
    pub fn register(&mut self, nickname: &str) {
        self.cursors
            .entry(nickname.to_string())
            .or_insert_with(|| AtomicUsize::new(0));
    }

    /// Return the starting offset for this request and advance the cursor.
    /// `None` when no cursor is registered for `nickname` (the pool is
    /// `FillFirst`, or the nickname is not pooled) -- callers treat that as
    /// "start at seat 0, fixed order".
    fn next_start(&self, nickname: &str) -> Option<usize> {
        self.cursors
            .get(nickname)
            .map(|c| c.fetch_add(1, Ordering::Relaxed))
    }
}

/// Maximum number of session->seat pins held at once. A bounded LRU keeps
/// the map at a few-thousand entries so memory stays flat under churn; the
/// least-recently-used pin is evicted when a new conversation arrives at
/// capacity. An evicted conversation simply re-pins (one cold miss) on its
/// next turn, so the bound is safe.
const STICKY_PIN_CAPACITY: usize = 4096;

/// Bounded LRU map of inbound conversation session key -> pinned seat's
/// STABLE `state_key` (see [`SeatTarget::state_key`] / [`seat_state_key`]).
/// A positional seat index is deliberately NOT stored: indices can shift on
/// a Router rebuild, whereas `state_key` is stable across reloads.
///
/// Wraps a `parking_lot::Mutex<LruCache<..>>` for interior mutability so the
/// map is read/written on the `&self` dispatch path.
///
/// UNLIKE [`RoundRobinCursors`], this map is CARRIED OVER on a Router
/// rebuild (see `Router::carry_over_sticky_from`). Dropping pins mid-incident
/// would scatter every live conversation off its warm-cache seat -- a mass
/// cold-miss across all in-flight conversations -- so the carry-over is
/// mandatory, not benign.
pub(crate) struct StickyPins {
    pins: Mutex<LruCache<String, String>>,
    /// Deterministic anti-herd tiebreak counter. When a birth pick finds
    /// several equally-least-loaded seats, the chooser rotates across them
    /// by `tiebreak % tied.len()` so concurrent fan-out misses reading the
    /// same capacity snapshot spread over distinct seats instead of herding
    /// onto one. Deliberately NOT carried over on a Router rebuild: a reset
    /// to 0 is benign -- it only re-seeds tie rotation, never a pin.
    tiebreak: AtomicUsize,
}

impl Default for StickyPins {
    fn default() -> Self {
        Self::new()
    }
}

impl StickyPins {
    /// Construct an empty pin map bounded at [`STICKY_PIN_CAPACITY`].
    pub(crate) fn new() -> Self {
        let cap = NonZeroUsize::new(STICKY_PIN_CAPACITY).expect("STICKY_PIN_CAPACITY > 0");
        Self {
            pins: Mutex::new(LruCache::new(cap)),
            tiebreak: AtomicUsize::new(0),
        }
    }

    /// Read the pinned seat `state_key` for `session_key`, marking it
    /// most-recently used under the lock. Marking MRU on read keeps an
    /// active conversation's pin hot so it survives LRU eviction while it
    /// is still being served. `None` when the session has no pin.
    pub(crate) fn get(&self, session_key: &str) -> Option<String> {
        self.pins.lock().get(session_key).cloned()
    }

    /// Return the next deterministic tiebreak value and advance the counter.
    /// Seeds the anti-herd rotation in [`sticky_least_loaded_order`].
    pub(crate) fn next_tiebreak(&self) -> usize {
        self.tiebreak.fetch_add(1, Ordering::Relaxed)
    }

    /// Insert or update the pin for `session_key`, marking it most-recently
    /// used. `state_key` is the pinned seat's stable runtime-state key.
    pub(crate) fn put(&self, session_key: &str, state_key: String) {
        self.pins.lock().put(session_key.to_string(), state_key);
    }

    /// Snapshot all entries in LRU order: least-recently-used FIRST,
    /// most-recently-used LAST. Used for carry-over on a Router rebuild so
    /// the destination map can re-`put` in the same recency order. (`iter`
    /// yields MRU->LRU, so the collected order is reversed.)
    pub(crate) fn export_entries(&self) -> Vec<(String, String)> {
        let guard = self.pins.lock();
        guard
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .rev()
            .collect()
    }
}

/// Resolve the per-request seat walk order for a pooled model.
///
/// `FillFirst` (or any non-pooled model, where `cursors` has no entry):
/// returns the fixed seat order as built -- default seat first, then sorted
/// labels -- so the chain walk drains one seat until it cools/parks before
/// falling to the next, maximizing prompt-cache locality.
///
/// `RoundRobin`: rotates the STARTING seat by one per request (the cursor's
/// `fetch_add` modulo seat count), then walks the remaining seats in order.
/// The relative order after the start offset is preserved so cooled seats
/// still fall through predictably.
///
/// Returns indices into the model's seat slice; the caller maps them back
/// to [`SeatTarget`]s. An empty or single-element seat set yields the
/// trivial order with no cursor traffic.
pub(crate) fn seat_order_for_request(
    nickname: &str,
    seat_count: usize,
    selection: SeatSelection,
    cursors: &RoundRobinCursors,
) -> Vec<usize> {
    if seat_count <= 1 {
        return (0..seat_count).collect();
    }
    let start = match selection {
        SeatSelection::RoundRobin => cursors.next_start(nickname).unwrap_or(0) % seat_count,
        SeatSelection::FillFirst => 0,
        // The real sticky-least-loaded ordering (session-key pin lookup +
        // least-loaded-at-birth) lands in the follow-up task. Until then
        // this intentionally mirrors FillFirst (start seat 0, fixed order)
        // so routing is byte-for-byte unchanged.
        SeatSelection::StickyLeastLoaded => 0,
    };
    (0..seat_count).map(|i| (start + i) % seat_count).collect()
}

/// Build a walk order with `home` first, then every other seat in ascending
/// index order. The home seat is a best-effort cache-locality hint; the
/// ascending tail preserves the fill-first fallback so the existing
/// sequential gate + chain fallback walk stays authoritative.
fn order_home_first(home: usize, seat_count: usize) -> Vec<usize> {
    let mut order = Vec::with_capacity(seat_count);
    order.push(home);
    order.extend((0..seat_count).filter(|&i| i != home));
    order
}

/// Pure seat-selection math for `StickyLeastLoaded`. Decides the per-request
/// walk order and whether this request should pin a freshly-chosen home seat.
///
/// `snapshots` is index-aligned with the seat slice (`len == seat_count`).
/// `pinned_index` is the seat this session is already pinned to (`Some`), or
/// `None` for a birth pick / pin miss. `tiebreak` seeds the anti-herd
/// rotation among equally-least-loaded candidates.
///
/// Returns `(walk_order, to_pin)`: `to_pin == Some(home)` ONLY on a successful
/// birth pick (the caller must pin it); `None` on a sticky-stay, the trivial
/// single-seat case, or an all-parked birth (no seat worth pinning yet).
pub(crate) fn sticky_least_loaded_order(
    seat_count: usize,
    pinned_index: Option<usize>,
    snapshots: &[CapacitySnapshot],
    tiebreak: usize,
) -> (Vec<usize>, Option<usize>) {
    if seat_count <= 1 {
        return ((0..seat_count).collect(), None);
    }
    // Sticky-stay: keep serving the pinned seat as the home hint; no new pin.
    if let Some(home) = pinned_index {
        return (order_home_first(home, seat_count), None);
    }

    // Birth pick: candidate set = dispatchable seats.
    let dispatchable: Vec<usize> = (0..seat_count)
        .filter(|&i| snapshots[i].is_dispatchable())
        .collect();
    if dispatchable.is_empty() {
        // All parked/exhausted: home 0, fill-first order, no pin. The gate +
        // fallback walk handles the all-parked case; a later turn re-picks
        // once a seat is healthy.
        return (order_home_first(0, seat_count), None);
    }

    // Health preference: if any candidate is fully Closed, do NOT birth a new
    // session onto a HalfOpenReady seat -- restrict to the Closed ones.
    let has_closed = dispatchable
        .iter()
        .any(|&i| snapshots[i].circuit == CircuitPhase::Closed);
    let candidates: Vec<usize> = if has_closed {
        dispatchable
            .into_iter()
            .filter(|&i| snapshots[i].circuit == CircuitPhase::Closed)
            .collect()
    } else {
        dispatchable
    };

    // Least loaded = most available RPM headroom. Treat unlimited (`None`) as
    // +infinity so an unlimited seat always wins the headroom comparison.
    let headroom = |idx: usize| -> f64 { snapshots[idx].rpm_available.unwrap_or(f64::INFINITY) };
    let max_headroom = candidates
        .iter()
        .map(|&i| headroom(i))
        .fold(f64::NEG_INFINITY, f64::max);
    let tied: Vec<usize> = candidates
        .into_iter()
        .filter(|&i| headroom(i) == max_headroom)
        .collect();

    // Anti-herd tiebreak: rotate deterministically across the tied seats.
    let home = tied[tiebreak % tied.len()];
    (order_home_first(home, seat_count), Some(home))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seat_state_key_default_seat_is_bare_nickname() {
        // Back-compat pin: the default seat (label None) keys as the bare
        // nickname, so a single-seat pool is identical to a non-pooled
        // model (state_key == nickname).
        assert_eq!(seat_state_key("opus", None), "opus");
    }

    #[test]
    fn seat_state_key_labeled_seat_is_hash_joined() {
        assert_eq!(seat_state_key("opus", Some("seat-b")), "opus#seat-b");
    }

    #[test]
    fn fill_first_order_is_fixed_zero_start() {
        // Arrange
        let cursors = RoundRobinCursors::default();

        // Act: three FillFirst calls.
        let a = seat_order_for_request("opus", 3, SeatSelection::FillFirst, &cursors);
        let b = seat_order_for_request("opus", 3, SeatSelection::FillFirst, &cursors);

        // Assert: stable, default-first order across requests.
        assert_eq!(a, vec![0, 1, 2]);
        assert_eq!(b, vec![0, 1, 2]);
    }

    #[test]
    fn round_robin_advances_start_seat_per_request() {
        // Arrange
        let mut cursors = RoundRobinCursors::default();
        cursors.register("opus");

        // Act: four requests over a 3-seat pool.
        let r0 = seat_order_for_request("opus", 3, SeatSelection::RoundRobin, &cursors);
        let r1 = seat_order_for_request("opus", 3, SeatSelection::RoundRobin, &cursors);
        let r2 = seat_order_for_request("opus", 3, SeatSelection::RoundRobin, &cursors);
        let r3 = seat_order_for_request("opus", 3, SeatSelection::RoundRobin, &cursors);

        // Assert: the starting seat advances by one each request and wraps.
        assert_eq!(r0, vec![0, 1, 2]);
        assert_eq!(r1, vec![1, 2, 0]);
        assert_eq!(r2, vec![2, 0, 1]);
        assert_eq!(r3, vec![0, 1, 2]);
    }

    #[test]
    fn round_robin_without_registered_cursor_falls_back_to_fixed() {
        // A RoundRobin model with no registered cursor (defensive) walks
        // the fixed order rather than panicking.
        let cursors = RoundRobinCursors::default();
        let order = seat_order_for_request("opus", 3, SeatSelection::RoundRobin, &cursors);
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[test]
    fn single_seat_order_is_trivial() {
        let cursors = RoundRobinCursors::default();
        assert_eq!(
            seat_order_for_request("opus", 1, SeatSelection::RoundRobin, &cursors),
            vec![0]
        );
        assert_eq!(
            seat_order_for_request("opus", 0, SeatSelection::FillFirst, &cursors),
            Vec::<usize>::new()
        );
    }

    #[test]
    fn stickyleastloaded_order_matches_fillfirst_for_now() {
        // Placeholder contract: until the follow-up adds real sticky
        // ordering, StickyLeastLoaded walks the fixed fill-first order.
        let cursors = RoundRobinCursors::default();
        let order = seat_order_for_request("m", 3, SeatSelection::StickyLeastLoaded, &cursors);
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[test]
    fn sticky_pins_put_then_export_round_trips() {
        // Arrange
        let pins = StickyPins::new();

        // Act
        pins.put("sess-1", "opus#seat-b".to_string());

        // Assert
        let entries = pins.export_entries();
        assert!(entries.contains(&("sess-1".to_string(), "opus#seat-b".to_string())));
    }

    #[test]
    fn sticky_pins_evicts_beyond_capacity() {
        // Arrange
        let pins = StickyPins::new();
        let overflow = 8;

        // Act: fill past capacity with distinct, never-re-touched keys.
        for i in 0..(STICKY_PIN_CAPACITY + overflow) {
            pins.put(&format!("sess-{i}"), format!("seat-{i}"));
        }

        // Assert: bounded at capacity.
        let entries = pins.export_entries();
        assert_eq!(entries.len(), STICKY_PIN_CAPACITY);

        // Assert: the earliest-inserted keys were evicted (LRU).
        let keys: std::collections::HashSet<&str> =
            entries.iter().map(|(k, _)| k.as_str()).collect();
        for i in 0..overflow {
            assert!(
                !keys.contains(format!("sess-{i}").as_str()),
                "earliest-inserted key sess-{i} should have been evicted",
            );
        }
        // And the most-recently-inserted survives.
        assert!(keys.contains(format!("sess-{}", STICKY_PIN_CAPACITY + overflow - 1).as_str()));
    }

    // ---- sticky_least_loaded_order pure-fn tests ----

    /// A Closed, unlimited (max headroom) snapshot -- the most-dispatchable
    /// possible seat. Used to build all-equal candidate sets.
    fn closed_unlimited() -> CapacitySnapshot {
        CapacitySnapshot {
            rpm_available: None,
            circuit: CircuitPhase::Closed,
        }
    }

    fn closed_with(rpm: f64) -> CapacitySnapshot {
        CapacitySnapshot {
            rpm_available: Some(rpm),
            circuit: CircuitPhase::Closed,
        }
    }

    #[test]
    fn sticky_birth_keyless_equal_snapshots_picks_index_zero() {
        // A birth pick (pinned_index=None) over all-equal candidates with
        // tiebreak=0 chooses seat 0 and yields the fill-first order, pinning 0.
        let snaps = vec![closed_unlimited(), closed_unlimited(), closed_unlimited()];
        let (order, to_pin) = sticky_least_loaded_order(3, None, &snaps, 0);
        assert_eq!(order, vec![0, 1, 2]);
        assert_eq!(to_pin, Some(0));
    }

    #[test]
    fn sticky_stay_orders_home_first_no_pin() {
        // An existing pin (Some(2)) stays: order leads with 2, the rest
        // ascending, and NO new pin is minted.
        let snaps: Vec<CapacitySnapshot> = Vec::new();
        let (order, to_pin) = sticky_least_loaded_order(3, Some(2), &snaps, 7);
        assert_eq!(order, vec![2, 0, 1]);
        assert_eq!(to_pin, None);
    }

    #[test]
    fn sticky_birth_picks_least_loaded_seat() {
        // Seat 1 has the most RPM headroom -> chosen as home and pinned.
        let snaps = vec![closed_with(2.0), closed_with(9.0), closed_with(5.0)];
        let (order, to_pin) = sticky_least_loaded_order(3, None, &snaps, 0);
        assert_eq!(order, vec![1, 0, 2]);
        assert_eq!(to_pin, Some(1));
    }

    #[test]
    fn sticky_birth_prefers_closed_over_half_open_ready() {
        // Seat 0 is HalfOpenReady with high headroom; seat 1 is Closed with
        // lower headroom. Health preference picks the Closed seat 1.
        let snaps = vec![
            CapacitySnapshot {
                rpm_available: Some(100.0),
                circuit: CircuitPhase::HalfOpenReady,
            },
            closed_with(3.0),
        ];
        let (order, to_pin) = sticky_least_loaded_order(2, None, &snaps, 0);
        assert_eq!(to_pin, Some(1));
        assert_eq!(order, vec![1, 0]);
    }

    #[test]
    fn sticky_birth_tiebreak_rotates_across_tied_seats() {
        // All three seats tied (equal snapshots): tiebreak rotates the home
        // deterministically and wraps -- the anti-herd spread.
        let snaps = vec![closed_with(5.0), closed_with(5.0), closed_with(5.0)];
        assert_eq!(sticky_least_loaded_order(3, None, &snaps, 0).1, Some(0));
        assert_eq!(sticky_least_loaded_order(3, None, &snaps, 1).1, Some(1));
        assert_eq!(sticky_least_loaded_order(3, None, &snaps, 2).1, Some(2));
        assert_eq!(sticky_least_loaded_order(3, None, &snaps, 3).1, Some(0));
    }

    #[test]
    fn sticky_birth_all_parked_yields_fill_first_no_pin() {
        // Every seat Open / not dispatchable: home 0, fill-first order, no
        // pin (a later turn re-picks once a seat is healthy).
        let open = CapacitySnapshot {
            rpm_available: None,
            circuit: CircuitPhase::Open,
        };
        let snaps = vec![open, open, open];
        let (order, to_pin) = sticky_least_loaded_order(3, None, &snaps, 0);
        assert_eq!(order, vec![0, 1, 2]);
        assert_eq!(to_pin, None);
    }

    #[test]
    fn sticky_order_home_first_in_the_middle() {
        // Home in the middle -> [home, then 0..n excluding home ascending].
        assert_eq!(order_home_first(2, 5), vec![2, 0, 1, 3, 4]);
    }

    #[test]
    fn sticky_single_seat_is_trivial() {
        let snaps = vec![closed_unlimited()];
        assert_eq!(
            sticky_least_loaded_order(1, None, &snaps, 0),
            (vec![0], None)
        );
        assert_eq!(
            sticky_least_loaded_order(0, None, &[], 0),
            (Vec::<usize>::new(), None)
        );
    }

    #[test]
    fn sticky_next_tiebreak_advances_monotonically() {
        let pins = StickyPins::new();
        assert_eq!(pins.next_tiebreak(), 0);
        assert_eq!(pins.next_tiebreak(), 1);
        assert_eq!(pins.next_tiebreak(), 2);
    }

    #[test]
    fn sticky_pins_get_returns_pinned_state_key() {
        let pins = StickyPins::new();
        assert_eq!(pins.get("sess-x"), None);
        pins.put("sess-x", "opus#seat-b".to_string());
        assert_eq!(pins.get("sess-x"), Some("opus#seat-b".to_string()));
    }
}
