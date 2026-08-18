//! Tests for the per-seat store and its merge rule.
//!
//! The merge rule is tested in every direction deliberately. Each direction
//! fails differently and three of them fail SILENTLY: a preserved-past-expiry
//! window hands a drained seat its headroom back, an erased sibling window
//! loses a signal nobody reported missing, and an out-of-order overwrite
//! quietly installs an older truth. None of those surfaces as an error, so a
//! test per direction is the only guard.

use super::*;

use std::time::Instant;

use crate::quota::freshness::accept_reset;
use crate::quota::key::seat_key_for_secret_ref;
use crate::quota::window::{Billing, Utilization};

const SEAT_PROVIDER: &str = "anthropic";
const WINDOW: Duration = Duration::from_hours(5);
const TOLERANCE: Duration = Duration::from_mins(5);

/// A stamp `offset` past a fixed base on both clocks, so two stamps in one test
/// are ordered on the monotonic clock exactly as their wall clocks are.
fn stamp_at(offset: Duration) -> ObservationStamp {
    ObservationStamp::from_parts(base_wall() + offset, base_monotonic() + offset)
}

fn base_wall() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_781_000_000)
}

/// A monotonic base far enough past process start that a test can subtract from
/// it without underflowing.
fn base_monotonic() -> Instant {
    Instant::now()
}

fn seat(label: Option<&str>) -> SeatKey {
    let secret_ref = routectl_auth::SecretRef::OAuth {
        provider: SEAT_PROVIDER.to_string(),
        label: label.map(str::to_string),
    };
    seat_key_for_secret_ref(Some(&secret_ref)).expect("an oauth ref yields a key")
}

/// A `Known` window at `fraction`, resetting `until` after the observation.
fn known(fraction: f64, observed: &ObservationStamp, until: Duration) -> QuotaWindow {
    let reset_at = observed.wall() + until;
    QuotaWindow::Known {
        utilization: Utilization::new(fraction).expect("a valid fraction"),
        reset_at: accept_reset(reset_at, observed, WINDOW, TOLERANCE)
            .expect("a reset inside the window is accepted"),
    }
}

/// A snapshot carrying the given windows, observed at `observed`.
fn snapshot(observed: ObservationStamp, fast: QuotaWindow, slow: QuotaWindow) -> QuotaSnapshot {
    QuotaSnapshot {
        observed,
        fast,
        slow,
        billing: Billing::Unknown,
    }
}

/// A store admitting the default and one labeled seat.
fn store() -> QuotaStore {
    let store = QuotaStore::default();
    store.admit_seats([seat(None), seat(Some("seat-b"))]);
    store
}

/// The fraction of a known window, or a panic naming what came back unknown.
fn fraction_of(window: &QuotaWindow, role: &str) -> f64 {
    match window {
        QuotaWindow::Known { utilization, .. } => utilization.fraction(),
        QuotaWindow::Unknown => panic!("the {role} window was expected to be known"),
    }
}

#[test]
fn a_fresh_store_holds_nothing() {
    let store = store();

    assert!(store.is_empty());
    assert_eq!(store.len(), 0);
    assert!(
        store
            .reading_for(&seat(None), &stamp_at(Duration::ZERO))
            .is_none()
    );
}

#[test]
fn a_first_observation_is_stored_for_an_admitted_seat() {
    let store = store();
    let observed = stamp_at(Duration::ZERO);

    let stored = store.observe(
        &seat(None),
        snapshot(
            observed,
            known(0.25, &observed, Duration::from_mins(30)),
            QuotaWindow::Unknown,
        ),
    );

    assert!(stored);
    let reading = store
        .reading_for(&seat(None), &observed)
        .expect("the seat now holds a reading");
    assert_eq!(fraction_of(&reading.fast, "fast"), 0.25);
    assert_eq!(reading.slow, QuotaWindow::Unknown);
}

/// Membership is enforced at INSERTION, not filtered at read: an identity
/// outside the configured seat set must not create an entry at all.
#[test]
fn an_unadmitted_seat_is_refused_and_creates_no_entry() {
    let store = QuotaStore::default();
    let observed = stamp_at(Duration::ZERO);

    let stored = store.observe(
        &seat(None),
        snapshot(
            observed,
            known(0.25, &observed, Duration::from_mins(30)),
            QuotaWindow::Unknown,
        ),
    );

    assert!(!stored, "an undeclared seat must not be admitted");
    assert!(store.is_empty());
}

// ---- The merge rule, one test per direction ----

#[test]
fn a_newer_known_replaces_an_older_known() {
    let store = store();
    let first = stamp_at(Duration::ZERO);
    let second = stamp_at(Duration::from_mins(10));
    store.observe(
        &seat(None),
        snapshot(
            first,
            known(0.20, &first, Duration::from_mins(30)),
            QuotaWindow::Unknown,
        ),
    );

    store.observe(
        &seat(None),
        snapshot(
            second,
            known(0.40, &second, Duration::from_mins(30)),
            QuotaWindow::Unknown,
        ),
    );

    let reading = store.reading_for(&seat(None), &second).expect("a reading");
    assert_eq!(fraction_of(&reading.fast, "fast"), 0.40);
}

/// A window RESETTING is the case a smoothing merge would get wrong: a drop in
/// utilization is a real reading, not an anomaly.
#[test]
fn a_newer_known_replaces_an_older_one_even_when_utilization_decreases() {
    let store = store();
    let first = stamp_at(Duration::ZERO);
    let second = stamp_at(Duration::from_mins(10));
    store.observe(
        &seat(None),
        snapshot(
            first,
            known(0.90, &first, Duration::from_mins(30)),
            QuotaWindow::Unknown,
        ),
    );

    store.observe(
        &seat(None),
        snapshot(
            second,
            known(0.05, &second, Duration::from_mins(30)),
            QuotaWindow::Unknown,
        ),
    );

    let reading = store.reading_for(&seat(None), &second).expect("a reading");
    assert_eq!(
        fraction_of(&reading.fast, "fast"),
        0.05,
        "a window that reset must report its new value, not its high-water mark"
    );
}

#[test]
fn a_newer_known_replaces_a_stored_unknown() {
    let store = store();
    let first = stamp_at(Duration::ZERO);
    let second = stamp_at(Duration::from_mins(10));
    store.observe(
        &seat(None),
        snapshot(first, QuotaWindow::Unknown, QuotaWindow::Unknown),
    );

    store.observe(
        &seat(None),
        snapshot(
            second,
            known(0.30, &second, Duration::from_mins(30)),
            QuotaWindow::Unknown,
        ),
    );

    let reading = store.reading_for(&seat(None), &second).expect("a reading");
    assert_eq!(fraction_of(&reading.fast, "fast"), 0.30);
}

#[test]
fn an_incoming_unknown_preserves_a_still_fresh_known() {
    let store = store();
    let first = stamp_at(Duration::ZERO);
    let second = stamp_at(Duration::from_mins(10));
    store.observe(
        &seat(None),
        snapshot(
            first,
            known(0.30, &first, Duration::from_hours(2)),
            QuotaWindow::Unknown,
        ),
    );

    store.observe(
        &seat(None),
        snapshot(second, QuotaWindow::Unknown, QuotaWindow::Unknown),
    );

    let reading = store.reading_for(&seat(None), &second).expect("a reading");
    assert_eq!(
        fraction_of(&reading.fast, "fast"),
        0.30,
        "one response that carried no header is not evidence the budget changed"
    );
}

/// A preserved window must keep aging on the observation it CAME FROM. If the
/// merge re-stamped it, a seat under steady traffic whose responses stopped
/// carrying the header would hold the same reading forever.
#[test]
fn an_incoming_unknown_leaves_unknown_once_the_stored_reading_expired() {
    let store = store();
    let first = stamp_at(Duration::ZERO);
    let after_reset = stamp_at(Duration::from_hours(2));
    store.observe(
        &seat(None),
        snapshot(
            first,
            known(0.30, &first, Duration::from_hours(1)),
            QuotaWindow::Unknown,
        ),
    );

    store.observe(
        &seat(None),
        snapshot(after_reset, QuotaWindow::Unknown, QuotaWindow::Unknown),
    );

    let reading = store
        .reading_for(&seat(None), &after_reset)
        .expect("a reading");
    assert_eq!(
        reading.fast,
        QuotaWindow::Unknown,
        "a window past its own reset must never be revived by a merge"
    );
}

#[test]
fn an_incoming_unknown_leaves_unknown_when_no_prior_reading_exists() {
    let store = store();
    let observed = stamp_at(Duration::ZERO);

    store.observe(
        &seat(None),
        snapshot(observed, QuotaWindow::Unknown, QuotaWindow::Unknown),
    );

    let reading = store
        .reading_for(&seat(None), &observed)
        .expect("a reading");
    assert_eq!(reading.fast, QuotaWindow::Unknown);
    assert_eq!(reading.slow, QuotaWindow::Unknown);
}

/// Responses complete out of order, and a late arrival describes an EARLIER
/// moment. Accepting it would install an older truth over a newer one.
#[test]
fn an_older_observation_never_overwrites_a_newer_one() {
    let store = store();
    let earlier = stamp_at(Duration::ZERO);
    let later = stamp_at(Duration::from_mins(10));
    store.observe(
        &seat(None),
        snapshot(
            later,
            known(0.40, &later, Duration::from_mins(30)),
            QuotaWindow::Unknown,
        ),
    );

    store.observe(
        &seat(None),
        snapshot(
            earlier,
            known(0.05, &earlier, Duration::from_mins(30)),
            QuotaWindow::Unknown,
        ),
    );

    // Asserted on the STORED VALUE, not on a rejection flag: ordering is
    // enforced per window, so an out-of-order arrival is dropped for the
    // window it describes rather than by refusing the whole snapshot. A
    // snapshot-level refusal would also throw away this arrival's OTHER
    // windows, which may be the newest readings there are for them.
    let reading = store.reading_for(&seat(None), &later).expect("a reading");
    assert_eq!(
        fraction_of(&reading.fast, "fast"),
        0.40,
        "the newer reading stands; the late arrival describing an earlier moment did not \
         install itself over it"
    );
}

/// The windows merge INDEPENDENTLY. An upstream that reports one window and
/// omits the other must not erase the one it omitted.
#[test]
fn omitting_one_window_does_not_erase_the_other() {
    let store = store();
    let first = stamp_at(Duration::ZERO);
    let second = stamp_at(Duration::from_mins(10));
    store.observe(
        &seat(None),
        snapshot(
            first,
            known(0.20, &first, Duration::from_hours(2)),
            known(0.60, &first, Duration::from_hours(2)),
        ),
    );

    store.observe(
        &seat(None),
        snapshot(
            second,
            known(0.35, &second, Duration::from_hours(2)),
            QuotaWindow::Unknown,
        ),
    );

    let reading = store.reading_for(&seat(None), &second).expect("a reading");
    assert_eq!(fraction_of(&reading.fast, "fast"), 0.35);
    assert_eq!(
        fraction_of(&reading.slow, "slow"),
        0.60,
        "the omitted window keeps its own last reading"
    );
}

/// Read-time expiry, independent of any merge: a window stale beyond its own
/// reset reads Unknown rather than at its last value.
#[test]
fn a_window_past_its_own_reset_reads_unknown_at_read_time() {
    let store = store();
    let observed = stamp_at(Duration::ZERO);
    store.observe(
        &seat(None),
        snapshot(
            observed,
            known(0.10, &observed, Duration::from_hours(1)),
            QuotaWindow::Unknown,
        ),
    );

    let reading = store
        .reading_for(&seat(None), &stamp_at(Duration::from_hours(2)))
        .expect("the entry still exists");

    assert_eq!(
        reading.fast,
        QuotaWindow::Unknown,
        "an expired window must read as no evidence, not as its last value -- \
         a low stale reading would attract every new session"
    );
}

/// The monotonic backstop, independent of the reset: a reading whose seat
/// stopped receiving traffic must not stay authoritative forever just because
/// its wall-clock reset has not arrived.
#[test]
fn a_reading_older_than_the_age_ceiling_reads_unknown_however_far_its_reset() {
    let store = store();
    let observed = stamp_at(Duration::ZERO);
    store.observe(
        &seat(None),
        snapshot(
            observed,
            known(0.10, &observed, Duration::from_hours(5)),
            QuotaWindow::Unknown,
        ),
    );

    let reading = store
        .reading_for(&seat(None), &stamp_at(MAX_OBSERVATION_AGE + WINDOW))
        .expect("the entry still exists");

    assert_eq!(reading.fast, QuotaWindow::Unknown);
}

#[test]
fn two_seats_hold_independent_readings() {
    let store = store();
    let observed = stamp_at(Duration::ZERO);
    store.observe(
        &seat(None),
        snapshot(
            observed,
            known(0.10, &observed, Duration::from_hours(2)),
            QuotaWindow::Unknown,
        ),
    );
    store.observe(
        &seat(Some("seat-b")),
        snapshot(
            observed,
            known(0.80, &observed, Duration::from_hours(2)),
            QuotaWindow::Unknown,
        ),
    );

    assert_eq!(store.len(), 2);
    let default_seat = store
        .reading_for(&seat(None), &observed)
        .expect("a reading");
    let labeled_seat = store
        .reading_for(&seat(Some("seat-b")), &observed)
        .expect("a reading");
    assert_eq!(fraction_of(&default_seat.fast, "fast"), 0.10);
    assert_eq!(fraction_of(&labeled_seat.fast, "fast"), 0.80);
}

// ---- Billing ----

#[test]
fn an_incoming_unknown_billing_preserves_a_known_one() {
    let store = store();
    let first = stamp_at(Duration::ZERO);
    let second = stamp_at(Duration::from_mins(10));
    store.observe(
        &seat(None),
        QuotaSnapshot {
            observed: first,
            fast: known(0.10, &first, Duration::from_hours(2)),
            slow: QuotaWindow::Unknown,
            billing: Billing::Overage,
        },
    );

    store.observe(
        &seat(None),
        snapshot(second, QuotaWindow::Unknown, QuotaWindow::Unknown),
    );

    let reading = store.reading_for(&seat(None), &second).expect("a reading");
    assert_eq!(
        reading.billing,
        Billing::Overage,
        "a missing claim is not evidence a seat became cheaper"
    );
}

// ---- Rejection counters ----

#[test]
fn rejections_are_counted_by_reason() {
    let store = store();

    store.record_rejection(&seat(None), RejectionReason::InvalidUtilization);
    store.record_rejection(&seat(None), RejectionReason::ExpiredReset);
    store.record_rejection(&seat(None), RejectionReason::ImplausibleReset);
    store.record_rejection(&seat(None), RejectionReason::ImplausibleReset);
    store.record_rejection(&seat(None), RejectionReason::Overflow);

    let totals = store.rejection_totals();
    assert_eq!(totals.invalid_utilization, 1);
    assert_eq!(totals.expired_reset, 1);
    assert_eq!(totals.implausible_reset, 2);
    assert_eq!(totals.overflow, 1);
}

/// The counters are EXACT while the WARN is throttled -- a seat producing
/// malformed metadata on every response must not turn a diagnostic into an
/// unbounded log stream, and the count must not be what pays for that.
#[test]
fn the_rejection_count_stays_exact_under_a_flood() {
    let store = store();

    for _ in 0..500 {
        store.record_rejection(&seat(None), RejectionReason::ImplausibleReset);
    }

    assert_eq!(store.rejection_totals().implausible_reset, 500);
}

// ---- Carry-over (Router shares the store `Arc`; `admit_seats` prunes) ----

/// `admit_seats` is the ONE prune point, called both at install time and
/// again by `Router::carry_over_quota_from` after the store is shared: a
/// seat the new call no longer names loses its reading immediately, so a
/// retired seat cannot keep answering from a past config's traffic.
#[test]
fn admit_seats_prunes_a_retired_seats_reading() {
    let store = store();
    let observed = stamp_at(Duration::ZERO);
    store.observe(
        &seat(Some("seat-b")),
        snapshot(
            observed,
            known(0.45, &observed, Duration::from_hours(2)),
            QuotaWindow::Unknown,
        ),
    );
    assert_eq!(store.len(), 1);

    store.admit_seats([seat(None)]);

    assert!(
        store.is_empty(),
        "a seat the new admit_seats call no longer names must not keep its reading",
    );
}

/// Pins the ordering guarantee that closes the admission TOCTOU (a write
/// racing a concurrent `admit_seats` prune): the interleaving itself is
/// closed by lock discipline (`observe` holds `seats` across its admission
/// check and insert, nesting a brief `admitted` read inside it; this is the
/// only method that nests the two locks, always in this one direction), not
/// by anything a sequential test can force a race into. What IS
/// sequentially verifiable, and what this test pins: once `admit_seats`
/// completes, a retired seat has no reading left AND a subsequent write for
/// it is refused and counted -- there is no post-prune window where a
/// stale insert could still land.
#[test]
fn after_admit_seats_a_retired_seats_write_is_refused_and_counted() {
    let store = store();
    let observed = stamp_at(Duration::ZERO);
    store.observe(
        &seat(Some("seat-b")),
        snapshot(
            observed,
            known(0.45, &observed, Duration::from_hours(2)),
            QuotaWindow::Unknown,
        ),
    );
    assert_eq!(store.len(), 1);

    store.admit_seats([seat(None)]);
    assert!(
        store.is_empty(),
        "the retired seat's pre-existing reading must be gone after the prune",
    );

    let wrote = store.observe(
        &seat(Some("seat-b")),
        snapshot(
            observed,
            known(0.9, &observed, Duration::from_hours(2)),
            QuotaWindow::Unknown,
        ),
    );
    assert!(
        !wrote,
        "a write for a seat retired by admit_seats must be refused"
    );
    assert_eq!(
        store.refused_by_admission_total(),
        1,
        "the refused write must be counted"
    );
    assert!(
        store.is_empty(),
        "the refused write must not have inserted a reading"
    );
}

/// The counterpart: a seat that survives re-admission keeps its reading.
#[test]
fn admit_seats_keeps_a_still_admitted_seats_reading() {
    let store = store();
    let observed = stamp_at(Duration::ZERO);
    store.observe(
        &seat(None),
        snapshot(
            observed,
            known(0.45, &observed, Duration::from_hours(2)),
            QuotaWindow::Unknown,
        ),
    );

    store.admit_seats([seat(None), seat(Some("seat-c"))]);

    let reading = store.reading_for(&seat(None), &observed);
    assert!(
        reading.is_some(),
        "re-admitting a seat must not drop its existing reading",
    );
}

/// A write refused for admission is counted, distinctly from every other
/// rejection reason -- the signal that makes a missed re-admit visible in
/// the metrics snapshot instead of silently refusing every write for a
/// legitimately new seat.
#[test]
fn observe_on_an_unadmitted_seat_bumps_the_refusal_counter() {
    let store = QuotaStore::default();
    let observed = stamp_at(Duration::ZERO);

    store.observe(
        &seat(None),
        snapshot(
            observed,
            known(0.25, &observed, Duration::from_mins(30)),
            QuotaWindow::Unknown,
        ),
    );
    store.observe(
        &seat(None),
        snapshot(
            observed,
            known(0.25, &observed, Duration::from_mins(30)),
            QuotaWindow::Unknown,
        ),
    );

    assert_eq!(store.refused_by_admission_total(), 2);
}

/// A late FAST reading must be judged against the FAST window's own stamp, not
/// against whatever arrived most recently for the seat. Ordering per snapshot
/// discards a genuinely newer reading for one window whenever a DIFFERENT
/// window happened to be updated in between -- and the discarded value is the
/// newest evidence that window has.
#[test]
fn a_late_window_reading_is_ordered_against_its_own_window_not_the_seat() {
    let store = store();
    let t1 = stamp_at(Duration::ZERO);
    let t2 = stamp_at(Duration::from_mins(10));
    let t3 = stamp_at(Duration::from_mins(20));

    // T1: FAST known high.
    store.observe(
        &seat(None),
        snapshot(
            t1,
            known(0.80, &t1, Duration::from_hours(4)),
            QuotaWindow::Unknown,
        ),
    );
    // T3: a response carrying only SLOW. FAST is preserved, and the seat's
    // most recent arrival is now T3.
    store.observe(
        &seat(None),
        snapshot(
            t3,
            QuotaWindow::Unknown,
            known(0.10, &t3, Duration::from_hours(4)),
        ),
    );
    // T2 FAST arrives late -- older than the seat's last arrival, but NEWER
    // than the FAST reading actually stored.
    store.observe(
        &seat(None),
        snapshot(
            t2,
            known(0.20, &t2, Duration::from_hours(4)),
            QuotaWindow::Unknown,
        ),
    );

    let reading = store.reading_for(&seat(None), &t3).expect("a reading");
    assert_eq!(
        fraction_of(&reading.fast, "fast"),
        0.20,
        "the T2 FAST reading is the newest FAST observation and must stand; judging it \
         against the seat's T3 arrival would keep the stale T1 value"
    );
    assert_eq!(
        fraction_of(&reading.slow, "slow"),
        0.10,
        "the SLOW reading is untouched by the late FAST arrival"
    );
}

/// Billing ages on the same ceiling as a window. It has no reset of its own, so
/// without the check a seat that reported overage and then went quiet keeps
/// reporting overage forever while its windows correctly read Unknown -- a cost
/// signal outliving the capacity signal it arrived beside.
#[test]
fn a_billing_state_past_the_age_ceiling_reads_unknown() {
    let store = store();
    let observed = stamp_at(Duration::ZERO);
    store.observe(
        &seat(None),
        QuotaSnapshot {
            observed,
            fast: known(0.30, &observed, Duration::from_hours(4)),
            slow: QuotaWindow::Unknown,
            billing: Billing::Overage,
        },
    );

    let fresh = store
        .reading_for(&seat(None), &stamp_at(Duration::from_mins(5)))
        .expect("a reading");
    assert_eq!(fresh.billing, Billing::Overage, "still inside the ceiling");

    let stale = store
        .reading_for(
            &seat(None),
            &stamp_at(MAX_OBSERVATION_AGE + Duration::from_mins(1)),
        )
        .expect("a reading");
    assert_eq!(
        stale.billing,
        Billing::Unknown,
        "an aged-out billing state must not keep demoting a seat on evidence no longer \
         in evidence"
    );
}
