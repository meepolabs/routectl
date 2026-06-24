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

/// Pinned-seat record for one inbound conversation. Carries the seat's
/// stable `state_key` plus a one-time overflow-repin marker.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SeatPin {
    pub(crate) state_key: String,
    /// True once this session has been migrated off its birth seat by a
    /// one-time overflow-repin. Caps migration at one and prevents an
    /// A->B->A flap when the original seat recovers.
    pub(crate) repinned: bool,
}

/// Maximum number of session->seat pins held at once. A bounded LRU keeps
/// the map at a few-thousand entries so memory stays flat under churn; the
/// least-recently-used pin is evicted when a new conversation arrives at
/// capacity. An evicted conversation simply re-pins (one cold miss) on its
/// next turn, so the bound is safe.
const STICKY_PIN_CAPACITY: usize = 4096;

/// Bounded LRU map of inbound conversation session key -> pinned [`SeatPin`]
/// (the seat's STABLE `state_key` plus the one-time overflow-repin marker;
/// see [`SeatTarget::state_key`] / [`seat_state_key`]). A positional seat
/// index is deliberately NOT stored: indices can shift on a Router rebuild,
/// whereas `state_key` is stable across reloads.
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
    pins: Mutex<LruCache<String, SeatPin>>,
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

    /// Read the [`SeatPin`] for `session_key`, marking it most-recently used
    /// under the lock. Marking MRU on read keeps an active conversation's pin
    /// hot so it survives LRU eviction while it is still being served. `None`
    /// when the session has no pin.
    pub(crate) fn get(&self, session_key: &str) -> Option<SeatPin> {
        self.pins.lock().get(session_key).cloned()
    }

    /// Return the next deterministic tiebreak value and advance the counter.
    /// Seeds the anti-herd rotation in [`sticky_least_loaded_order`].
    pub(crate) fn next_tiebreak(&self) -> usize {
        self.tiebreak.fetch_add(1, Ordering::Relaxed)
    }

    /// Insert or update the pin for `session_key`, marking it most-recently
    /// used. Single setter: a birth pick passes `repinned: false`, a one-time
    /// overflow-repin passes `repinned: true`.
    pub(crate) fn put(&self, session_key: &str, pin: SeatPin) {
        self.pins.lock().put(session_key.to_string(), pin);
    }

    /// Snapshot all entries in LRU order: least-recently-used FIRST,
    /// most-recently-used LAST. Used for carry-over on a Router rebuild so
    /// the destination map can re-`put` in the same recency order (carrying
    /// the `repinned` flag, so the one-time cap survives the reload). (`iter`
    /// yields MRU->LRU, so the collected order is reversed.)
    pub(crate) fn export_entries(&self) -> Vec<(String, SeatPin)> {
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
        // Keyless / single-seat StickyLeastLoaded resolves here and walks the
        // fixed fill-first order (start seat 0). The keyed sticky-least-loaded
        // ordering lives in `Router::sticky_seat_order`, which needs the
        // inbound session key and per-seat capacity that this pure fn does not
        // receive.
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

/// Outcome of a single sticky-least-loaded selection. The walk-order hint is
/// returned alongside; this enum tells the caller whether (and how) to update
/// the pin. It also maps to the fixed-vocabulary `selection_decision` token
/// recorded in the usage ledger, so each variant is an observable decision.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SelectionOutcome {
    /// Pin miss (birth): caller pins `home` with `repinned: false`.
    Birth { home: usize },
    /// Home healthy, OR already-repinned, OR no healthy sibling: no pin write.
    Stay { home: usize },
    /// First migration off a non-dispatchable home: caller pins the new
    /// `home` with `repinned: true`.
    OverflowRepin { home: usize },
    /// Pin miss with no dispatchable seat: fill-first order, no pin.
    DeferNoHealthy,
}

/// Pick the least-loaded HEALTHY index among `candidates`: dispatchable only,
/// Closed preferred over HalfOpenReady, then max rpm_available (None=+inf),
/// ties broken deterministically by `tiebreak`. None if no candidate is
/// dispatchable. `candidates` are indices into `snapshots`.
fn pick_least_loaded(
    candidates: &[usize],
    snapshots: &[CapacitySnapshot],
    tiebreak: usize,
) -> Option<usize> {
    let dispatchable: Vec<usize> = candidates
        .iter()
        .copied()
        .filter(|&i| snapshots[i].is_dispatchable())
        .collect();
    if dispatchable.is_empty() {
        return None;
    }

    // Health preference: if any candidate is fully Closed, do NOT pick a
    // HalfOpenReady seat -- restrict to the Closed ones.
    let has_closed = dispatchable
        .iter()
        .any(|&i| snapshots[i].circuit == CircuitPhase::Closed);
    let preferred: Vec<usize> = if has_closed {
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
    let max_headroom = preferred
        .iter()
        .map(|&i| headroom(i))
        .fold(f64::NEG_INFINITY, f64::max);
    let tied: Vec<usize> = preferred
        .into_iter()
        .filter(|&i| headroom(i) == max_headroom)
        .collect();

    // Anti-herd tiebreak: rotate deterministically across the tied seats.
    Some(tied[tiebreak % tied.len()])
}

/// Pure seat-selection math for `StickyLeastLoaded`. Decides the per-request
/// walk order and the [`SelectionOutcome`] (whether/how to update the pin).
///
/// `snapshots` is index-aligned with the seat slice (`len == seat_count`),
/// gathered for the hit AND miss path now (the overflow check reads the
/// pinned home's snapshot). `pinned_index` is the seat this session is pinned
/// to (`Some`), or `None` for a birth pick / pin miss. `already_repinned` is
/// the pin's one-time overflow marker. `tiebreak` seeds the anti-herd
/// rotation among equally-least-loaded candidates.
///
/// One-time overflow-repin with hysteresis: a pinned home that goes
/// non-dispatchable is migrated ONCE to the least-loaded healthy sibling and
/// never chased further -- we never compare against or return to the original
/// seat, so a recovered original cannot pull the session back (no A->B->A
/// flap).
pub(crate) fn sticky_least_loaded_order(
    seat_count: usize,
    pinned_index: Option<usize>,
    already_repinned: bool,
    snapshots: &[CapacitySnapshot],
    tiebreak: usize,
) -> (Vec<usize>, SelectionOutcome) {
    if seat_count <= 1 {
        return (
            (0..seat_count).collect(),
            SelectionOutcome::Stay { home: 0 },
        );
    }

    let n = seat_count;
    match pinned_index {
        Some(home) => {
            // Healthy home: keep serving it; no pin write.
            if snapshots[home].is_dispatchable() {
                return (order_home_first(home, n), SelectionOutcome::Stay { home });
            }
            // Already migrated once: do NOT chase further. The gate + fallback
            // walk handles the dead home for this request. One-time cap.
            if already_repinned {
                return (order_home_first(home, n), SelectionOutcome::Stay { home });
            }
            // First migration: pick the least-loaded healthy SIBLING.
            let siblings: Vec<usize> = (0..n).filter(|&i| i != home).collect();
            match pick_least_loaded(&siblings, snapshots, tiebreak) {
                Some(new_home) => (
                    order_home_first(new_home, n),
                    SelectionOutcome::OverflowRepin { home: new_home },
                ),
                // No healthy sibling: nowhere better. Stay (no flap, no pin);
                // the gate handles the dead home.
                None => (order_home_first(home, n), SelectionOutcome::Stay { home }),
            }
        }
        None => {
            // Birth pick: candidate set = all seats.
            let all: Vec<usize> = (0..n).collect();
            match pick_least_loaded(&all, snapshots, tiebreak) {
                Some(home) => (order_home_first(home, n), SelectionOutcome::Birth { home }),
                // All parked/exhausted: home 0, fill-first order, no pin. A
                // later turn re-picks once a seat is healthy.
                None => (order_home_first(0, n), SelectionOutcome::DeferNoHealthy),
            }
        }
    }
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
    fn stickyleastloaded_keyless_order_matches_fillfirst() {
        // Keyless / single-seat StickyLeastLoaded resolves through this pure
        // fn and walks the fixed fill-first order; the keyed sticky ordering
        // lives in Router::sticky_seat_order.
        let cursors = RoundRobinCursors::default();
        let order = seat_order_for_request("m", 3, SeatSelection::StickyLeastLoaded, &cursors);
        assert_eq!(order, vec![0, 1, 2]);
    }

    fn pin(state_key: &str) -> SeatPin {
        SeatPin {
            state_key: state_key.to_string(),
            repinned: false,
        }
    }

    #[test]
    fn sticky_pins_put_then_export_round_trips() {
        // Arrange
        let pins = StickyPins::new();

        // Act
        pins.put("sess-1", pin("opus#seat-b"));

        // Assert
        let entries = pins.export_entries();
        assert!(entries.contains(&("sess-1".to_string(), pin("opus#seat-b"))));
    }

    #[test]
    fn sticky_pins_evicts_beyond_capacity() {
        // Arrange
        let pins = StickyPins::new();
        let overflow = 8;

        // Act: fill past capacity with distinct, never-re-touched keys.
        for i in 0..(STICKY_PIN_CAPACITY + overflow) {
            pins.put(&format!("sess-{i}"), pin(&format!("seat-{i}")));
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

    /// An Open (non-dispatchable) snapshot -- a parked seat.
    fn open() -> CapacitySnapshot {
        CapacitySnapshot {
            rpm_available: None,
            circuit: CircuitPhase::Open,
        }
    }

    #[test]
    fn sticky_birth_keyless_equal_snapshots_picks_index_zero() {
        // A birth pick (pinned_index=None) over all-equal candidates with
        // tiebreak=0 chooses seat 0 and yields the fill-first order, pinning 0.
        let snaps = vec![closed_unlimited(), closed_unlimited(), closed_unlimited()];
        let (order, outcome) = sticky_least_loaded_order(3, None, false, &snaps, 0);
        assert_eq!(order, vec![0, 1, 2]);
        assert_eq!(outcome, SelectionOutcome::Birth { home: 0 });
    }

    #[test]
    fn healthy_home_stays() {
        // An existing pin whose home is dispatchable stays: order leads with
        // the home, the rest ascending, and NO new pin is minted.
        let snaps = vec![closed_unlimited(), closed_unlimited(), closed_unlimited()];
        let (order, outcome) = sticky_least_loaded_order(3, Some(2), false, &snaps, 7);
        assert_eq!(order, vec![2, 0, 1]);
        assert_eq!(outcome, SelectionOutcome::Stay { home: 2 });
    }

    #[test]
    fn overflow_repin_migrates_to_healthy_sibling_once() {
        // Home (index 0) is Open / non-dispatchable; sibling 1 is Closed. Not
        // yet repinned -> migrate ONCE to sibling 1, order leads with 1.
        let snaps = vec![open(), closed_unlimited(), open()];
        let (order, outcome) = sticky_least_loaded_order(3, Some(0), false, &snaps, 0);
        assert_eq!(outcome, SelectionOutcome::OverflowRepin { home: 1 });
        assert_eq!(order, vec![1, 0, 2]);
    }

    #[test]
    fn already_repinned_unhealthy_home_stays() {
        // Pinned home non-dispatchable but already_repinned=true: the one-time
        // cap holds -> Stay, no second migration even though sibling 1 is
        // healthy.
        let snaps = vec![open(), closed_unlimited(), open()];
        let (order, outcome) = sticky_least_loaded_order(3, Some(0), true, &snaps, 0);
        assert_eq!(outcome, SelectionOutcome::Stay { home: 0 });
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[test]
    fn overflow_no_healthy_sibling_stays() {
        // Pinned home non-dispatchable, every sibling also non-dispatchable,
        // not repinned -> Stay (no flap, no pin: nowhere better).
        let snaps = vec![open(), open(), open()];
        let (order, outcome) = sticky_least_loaded_order(3, Some(0), false, &snaps, 0);
        assert_eq!(outcome, SelectionOutcome::Stay { home: 0 });
        assert_eq!(order, vec![0, 1, 2]);
    }

    #[test]
    fn no_flap_repinned_sibling_stays_when_original_recovers() {
        // Hysteresis: after a session repins onto sibling 1 (repinned=true),
        // a later call where the ORIGINAL seat 0 has recovered must NOT pull
        // the session back. The pin now points at sibling 1, which is
        // dispatchable -> Stay on 1.
        let recovered = vec![closed_unlimited(), closed_unlimited(), open()];
        let (order, outcome) = sticky_least_loaded_order(3, Some(1), true, &recovered, 0);
        assert_eq!(outcome, SelectionOutcome::Stay { home: 1 });
        assert_eq!(order, vec![1, 0, 2]);
    }

    #[test]
    fn birth_unchanged() {
        // A miss still yields Birth on a healthy pool and DeferNoHealthy when
        // all seats are parked.
        let healthy = vec![closed_with(2.0), closed_with(9.0), closed_with(5.0)];
        let (order, outcome) = sticky_least_loaded_order(3, None, false, &healthy, 0);
        assert_eq!(order, vec![1, 0, 2]);
        assert_eq!(outcome, SelectionOutcome::Birth { home: 1 });

        let parked = vec![open(), open(), open()];
        let (order, outcome) = sticky_least_loaded_order(3, None, false, &parked, 0);
        assert_eq!(order, vec![0, 1, 2]);
        assert_eq!(outcome, SelectionOutcome::DeferNoHealthy);
    }

    #[test]
    fn sticky_birth_picks_least_loaded_seat() {
        // Seat 1 has the most RPM headroom -> chosen as home and pinned.
        let snaps = vec![closed_with(2.0), closed_with(9.0), closed_with(5.0)];
        let (order, outcome) = sticky_least_loaded_order(3, None, false, &snaps, 0);
        assert_eq!(order, vec![1, 0, 2]);
        assert_eq!(outcome, SelectionOutcome::Birth { home: 1 });
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
        let (order, outcome) = sticky_least_loaded_order(2, None, false, &snaps, 0);
        assert_eq!(outcome, SelectionOutcome::Birth { home: 1 });
        assert_eq!(order, vec![1, 0]);
    }

    #[test]
    fn sticky_birth_tiebreak_rotates_across_tied_seats() {
        // All three seats tied (equal snapshots): tiebreak rotates the home
        // deterministically and wraps -- the anti-herd spread.
        let snaps = vec![closed_with(5.0), closed_with(5.0), closed_with(5.0)];
        assert_eq!(
            sticky_least_loaded_order(3, None, false, &snaps, 0).1,
            SelectionOutcome::Birth { home: 0 }
        );
        assert_eq!(
            sticky_least_loaded_order(3, None, false, &snaps, 1).1,
            SelectionOutcome::Birth { home: 1 }
        );
        assert_eq!(
            sticky_least_loaded_order(3, None, false, &snaps, 2).1,
            SelectionOutcome::Birth { home: 2 }
        );
        assert_eq!(
            sticky_least_loaded_order(3, None, false, &snaps, 3).1,
            SelectionOutcome::Birth { home: 0 }
        );
    }

    #[test]
    fn sticky_birth_all_parked_yields_fill_first_no_pin() {
        // Every seat Open / not dispatchable: home 0, fill-first order, no
        // pin (a later turn re-picks once a seat is healthy).
        let snaps = vec![open(), open(), open()];
        let (order, outcome) = sticky_least_loaded_order(3, None, false, &snaps, 0);
        assert_eq!(order, vec![0, 1, 2]);
        assert_eq!(outcome, SelectionOutcome::DeferNoHealthy);
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
            sticky_least_loaded_order(1, None, false, &snaps, 0),
            (vec![0], SelectionOutcome::Stay { home: 0 })
        );
        assert_eq!(
            sticky_least_loaded_order(0, None, false, &[], 0),
            (Vec::<usize>::new(), SelectionOutcome::Stay { home: 0 })
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
        pins.put("sess-x", pin("opus#seat-b"));
        assert_eq!(pins.get("sess-x"), Some(pin("opus#seat-b")));
    }
}
