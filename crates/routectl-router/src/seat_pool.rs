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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use routectl_auth::SecretRef;
use routectl_core::Provider;

use crate::config::SeatSelection;

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
    };
    (0..seat_count).map(|i| (start + i) % seat_count).collect()
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
}
