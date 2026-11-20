//! Tests for the stale rule of the placement-safe view, and for how the pace
//! rule reads the same windows.

use std::time::{Instant, SystemTime};

use super::*;

use crate::quota::key::{SeatKey, seat_key_for_secret_ref};
use crate::quota::placement::{Reason, Standing, Tier, standing};
use crate::quota::reading::{Scope, WindowKey, WindowSet};
use crate::quota::reduce::QuotaSnapshot;
use crate::quota::store::QuotaStore;
use crate::quota::window::{Billing, QuotaWindow};

const FIVE_HOURS: u64 = 5 * 3_600;
const SEVEN_DAYS: u64 = 7 * 86_400;
const ONE_MS: Duration = Duration::from_millis(1);

fn base_wall() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1_781_000_000)
}

/// Stamps past one base on both clocks. The base sits an hour past a fresh
/// `Instant`, so a test can stamp a read instant BEFORE an observation.
struct Clock {
    monotonic: Instant,
}

impl Clock {
    fn new() -> Self {
        Self {
            monotonic: Instant::now() + Duration::from_hours(1),
        }
    }

    fn at(&self, offset: Duration) -> ObservationStamp {
        self.split(offset, offset)
    }

    /// A stamp whose wall and monotonic clocks advanced by different amounts.
    fn split(&self, wall: Duration, monotonic: Duration) -> ObservationStamp {
        ObservationStamp::from_parts(base_wall() + wall, self.monotonic + monotonic)
    }

    fn before_base(&self, monotonic: Duration) -> ObservationStamp {
        ObservationStamp::from_parts(
            base_wall(),
            self.monotonic
                .checked_sub(monotonic)
                .expect("the base sits past the offset"),
        )
    }
}

fn seat() -> SeatKey {
    let secret_ref = routectl_auth::SecretRef::OAuth {
        provider: "anthropic".to_string(),
        label: Some("a".to_string()),
    };
    seat_key_for_secret_ref(Some(&secret_ref)).expect("an oauth ref yields a key")
}

fn store() -> QuotaStore {
    let store = QuotaStore::default();
    store.admit_seats([seat()]);
    store
}

fn key(length_secs: u64, scope: Option<&str>) -> WindowKey {
    let scope = scope.map(|raw| Scope::new(raw).expect("a canonical scope"));
    WindowKey::new(length_secs, scope).expect("a classifiable length")
}

/// Store one complete poll observation of `windows` taken at `at`.
fn poll(store: &QuotaStore, at: ObservationStamp, windows: Vec<WindowReading>) {
    let observation = AccountObservation::new(
        ReadingSource::Poll,
        at,
        ObservationReport::Complete(WindowSet::new(windows).expect("a valid set")),
    )
    .expect("one stamp throughout");
    store
        .observe_account(&seat(), &observation)
        .expect("accepted");
}

fn running(key: WindowKey, used: f64, reset: Duration, at: &ObservationStamp) -> WindowReading {
    WindowReading::new(key, used, Some(base_wall() + reset), Started::Yes, at)
        .expect("a valid running window")
}

fn unstarted(key: WindowKey, at: &ObservationStamp) -> WindowReading {
    WindowReading::new(key, 0.0, None, Started::No, at).expect("a valid unstarted window")
}

/// The one window the view reports for the seat at `now`.
fn only_window(store: &QuotaStore, now: &ObservationStamp) -> AccountWindow {
    let readings = store.readings(now);
    let [account] = readings.accounts.as_slice() else {
        panic!("one configured account");
    };
    let [window] = account.windows.as_slice() else {
        panic!("one window held");
    };
    window.clone()
}

/// The seat's pace-rule standing at `now` for an Opus conversation.
fn standing_at(store: &QuotaStore, now: &ObservationStamp) -> Standing {
    let readings = store.readings(now);
    standing(readings.accounts.first(), "claude-opus-4-7", now)
}

// ---- The stale rule ----

#[test]
fn a_window_is_fresh_at_stale_after_and_stale_one_millisecond_later() {
    let clock = Clock::new();
    let store = store();
    let t0 = clock.at(Duration::ZERO);
    poll(
        &store,
        t0,
        vec![running(
            key(FIVE_HOURS, None),
            0.25,
            Duration::from_hours(2),
            &t0,
        )],
    );

    let at_bound = only_window(&store, &clock.at(STALE_AFTER));
    let past_bound = only_window(&store, &clock.at(STALE_AFTER + ONE_MS));

    assert!(!at_bound.stale);
    assert_eq!(at_bound.age, Some(STALE_AFTER));
    assert_eq!(at_bound.remaining(), Some(0.75));
    assert!(past_bound.stale);
    assert_eq!(past_bound.remaining(), None, "a stale window gives no room");
}

/// The one account the view lists for the seat at `now`.
fn only_account(store: &QuotaStore, now: &ObservationStamp) -> AccountReading {
    let readings = store.readings(now);
    let [account] = readings.accounts.as_slice() else {
        panic!("one configured account");
    };
    account.clone()
}

#[test]
fn the_last_reading_is_fresh_at_stale_after_and_stale_one_millisecond_later() {
    // Arrange
    let clock = Clock::new();
    let store = store();
    let t0 = clock.at(Duration::ZERO);
    poll(&store, t0, vec![unstarted(key(FIVE_HOURS, None), &t0)]);
    let fresh_at = |now: Duration| {
        only_account(&store, &clock.at(now))
            .last_accepted
            .expect("an accepted reading")
            .fresh
    };

    // Act
    let at_bound = fresh_at(STALE_AFTER);
    let past_bound = fresh_at(STALE_AFTER + ONE_MS);

    // Assert
    assert!(at_bound);
    assert!(!past_bound);
}

/// Billing keeps its last known value for display past the stale bound, but
/// reads as fresh only within it, and an unknown claim is never fresh.
#[test]
fn billing_is_fresh_only_when_known_and_observed_within_stale_after() {
    let rows = [
        ("overage at the bound", Billing::Overage, STALE_AFTER, true),
        (
            "overage past the bound",
            Billing::Overage,
            STALE_AFTER + ONE_MS,
            false,
        ),
        (
            "included at the bound",
            Billing::Included,
            STALE_AFTER,
            true,
        ),
        ("unknown at once", Billing::Unknown, Duration::ZERO, false),
    ];
    for (name, billing, read_after, fresh) in rows {
        // Arrange
        let clock = Clock::new();
        let store = store();
        let t0 = clock.at(Duration::ZERO);
        assert!(store.observe(
            &seat(),
            QuotaSnapshot {
                observed: t0,
                fast: QuotaWindow::Unknown,
                slow: QuotaWindow::Unknown,
                billing: billing.clone(),
            },
        ));

        // Act
        let account = only_account(&store, &clock.at(read_after));

        // Assert
        assert_eq!(account.billing, billing, "row: {name}");
        assert_eq!(account.billing_fresh, fresh, "row: {name}");
    }
}

#[test]
fn a_running_window_is_stale_once_its_reset_passes_inside_stale_after() {
    let clock = Clock::new();
    let store = store();
    let t0 = clock.at(Duration::ZERO);
    let reset = Duration::from_mins(3);
    poll(
        &store,
        t0,
        vec![running(key(FIVE_HOURS, None), 0.25, reset, &t0)],
    );

    let before = only_window(&store, &clock.at(Duration::from_millis(179_999)));
    let at_reset = only_window(&store, &clock.at(reset));

    assert!(!before.stale);
    assert!(at_reset.stale);
    assert_eq!(at_reset.remaining(), None);
}

#[test]
fn a_window_is_stale_when_the_wall_clock_advanced_without_the_monotonic_one() {
    let clock = Clock::new();
    let store = store();
    let t0 = clock.at(Duration::ZERO);
    poll(
        &store,
        t0,
        vec![running(
            key(FIVE_HOURS, None),
            0.25,
            Duration::from_hours(2),
            &t0,
        )],
    );
    let after_suspend = clock.split(STALE_AFTER + ONE_MS, Duration::from_mins(1));

    let window = only_window(&store, &after_suspend);

    assert!(window.stale);
    assert_eq!(window.age, Some(STALE_AFTER + ONE_MS));
}

#[test]
fn a_window_is_stale_when_the_read_instant_cannot_be_ordered_after_it() {
    let clock = Clock::new();
    let rows: [(&str, ObservationStamp); 2] = [
        ("monotonic before the stamp", clock.before_base(ONE_MS)),
        (
            "wall before the stamp",
            ObservationStamp::from_parts(base_wall() - ONE_MS, clock.at(ONE_MS).monotonic()),
        ),
    ];
    for (name, now) in rows {
        let store = store();
        let t0 = clock.at(Duration::ZERO);
        poll(
            &store,
            t0,
            vec![running(
                key(FIVE_HOURS, None),
                0.25,
                Duration::from_hours(2),
                &t0,
            )],
        );

        let window = only_window(&store, &now);

        assert!(window.stale, "row: {name}");
        assert_eq!(window.age, None, "row: {name}");
        assert_eq!(window.remaining(), None, "row: {name}");
    }
}

/// Placement reads the view's stale rule: a running window older than the
/// stale bound no longer ranks the seat, however far its reset.
#[test]
fn placement_stops_ranking_on_a_running_window_once_the_view_marks_it_stale() {
    let clock = Clock::new();
    let store = store();
    let t0 = clock.at(Duration::ZERO);
    poll(
        &store,
        t0,
        vec![running(
            key(FIVE_HOURS, None),
            0.25,
            Duration::from_hours(2),
            &t0,
        )],
    );
    let fresh = clock.at(STALE_AFTER);
    let stale = clock.at(STALE_AFTER + ONE_MS);

    assert!(!only_window(&store, &fresh).stale);
    assert_eq!(standing_at(&store, &fresh).tier, Tier::Roomy);
    assert!(only_window(&store, &stale).stale);
    assert_eq!(standing_at(&store, &stale).reason, Reason::StaleReading);
}

// ---- What the view reports ----

#[test]
fn the_view_lists_every_configured_account_with_its_windows_and_last_reading() {
    let clock = Clock::new();
    let store = QuotaStore::default();
    let silent = seat_key_for_secret_ref(Some(&routectl_auth::SecretRef::OAuth {
        provider: "anthropic".to_string(),
        label: Some("b".to_string()),
    }))
    .expect("a key");
    store.admit_seats([seat(), silent]);
    let t0 = clock.at(Duration::ZERO);
    poll(
        &store,
        t0,
        vec![
            running(key(SEVEN_DAYS, None), 0.40, Duration::from_hours(48), &t0),
            unstarted(key(FIVE_HOURS, None), &t0),
            running(
                key(SEVEN_DAYS, Some("opus")),
                0.10,
                Duration::from_hours(48),
                &t0,
            ),
        ],
    );
    let now = clock.at(Duration::from_mins(2));

    let readings = store.readings(&now);

    let accounts: Vec<(&str, ReportState)> = readings
        .accounts
        .iter()
        .map(|a| (a.account.as_str(), a.report))
        .collect();
    assert_eq!(
        accounts,
        [
            ("anthropic#a", ReportState::Windows),
            ("anthropic#b", ReportState::NoReading)
        ]
    );
    let held = &readings.accounts[0];
    assert_eq!(
        held.last_accepted,
        Some(LastReading {
            observed_at: base_wall(),
            source: ReadingSource::Poll,
            fresh: true,
        })
    );
    let shapes: Vec<(u64, LengthClass, Option<&str>, f64, Started)> = held
        .windows
        .iter()
        .map(|w| {
            (
                w.window.length_secs,
                w.window.class,
                w.window.scope.as_deref(),
                w.window.used,
                w.window.started,
            )
        })
        .collect();
    assert_eq!(
        shapes,
        [
            (FIVE_HOURS, LengthClass::Short, None, 0.0, Started::No),
            (SEVEN_DAYS, LengthClass::Long, None, 0.40, Started::Yes),
            (
                SEVEN_DAYS,
                LengthClass::Long,
                Some("opus"),
                0.10,
                Started::Yes
            ),
        ]
    );
    assert!(
        held.windows
            .iter()
            .all(|w| !w.stale && w.observed_at == base_wall())
    );
    assert_eq!(held.windows[0].age, Some(Duration::from_mins(2)));
}

/// An unstarted window's reset is provisional, never an expiry: passing it
/// does not make the window stale; only its age does.
#[test]
fn an_unstarted_window_ages_out_but_its_provisional_reset_never_expires_it() {
    let clock = Clock::new();
    let store = store();
    let t0 = clock.at(Duration::ZERO);
    let provisional = WindowReading::new(
        key(FIVE_HOURS, None),
        0.0,
        Some(base_wall() + Duration::from_mins(2)),
        Started::No,
        &t0,
    )
    .expect("an unstarted window with a provisional reset");
    poll(&store, t0, vec![provisional]);

    let past_provisional = only_window(&store, &clock.at(Duration::from_mins(3)));
    let aged = only_window(&store, &clock.at(STALE_AFTER + ONE_MS));

    assert!(!past_provisional.stale);
    assert_eq!(past_provisional.remaining(), Some(1.0));
    assert!(aged.stale);
}

// ---- The pace standing of an unstarted window ----

#[test]
fn a_fresh_unstarted_window_ranks_until_stale_after_and_then_goes_unread() {
    let clock = Clock::new();
    let store = store();
    let t0 = clock.at(Duration::ZERO);
    poll(&store, t0, vec![unstarted(key(FIVE_HOURS, None), &t0)]);

    let at_bound = standing_at(&store, &clock.at(STALE_AFTER));
    let past_bound = standing_at(&store, &clock.at(STALE_AFTER + ONE_MS));

    assert_eq!(
        (at_bound.tier, at_bound.score, at_bound.reason),
        (Tier::Roomy, Some(1.0), Reason::Unstarted)
    );
    assert_eq!(
        (past_bound.tier, past_bound.reason),
        (Tier::Unread, Reason::StaleReading)
    );
}

#[test]
fn an_unstarted_window_read_across_a_suspend_is_unread() {
    let clock = Clock::new();
    let store = store();
    let t0 = clock.at(Duration::ZERO);
    poll(&store, t0, vec![unstarted(key(FIVE_HOURS, None), &t0)]);
    let after_suspend = clock.split(Duration::from_hours(1), Duration::from_mins(1));

    assert_eq!(standing_at(&store, &after_suspend).tier, Tier::Unread);
}

// ---- Billing ----

#[test]
fn an_account_with_no_meter_reads_billing_unknown() {
    let clock = Clock::new();
    let store = store();

    let readings = store.readings(&clock.at(Duration::ZERO));

    let [account] = readings.accounts.as_slice() else {
        panic!("one configured account");
    };
    assert_eq!(account.report, ReportState::NoReading);
    assert_eq!(account.billing, Billing::Unknown);
    assert!(!account.billing_fresh);
}

// ---- No wire shape ----

/// Resolves only when `T` does not implement `Serialize`: with an impl, both
/// blanket impls apply and the call below is ambiguous, which fails the build.
trait AmbiguousIfSerialize<A> {
    fn assert_not_serialize() {}
}

impl<T: ?Sized> AmbiguousIfSerialize<()> for T {}

struct ImplementsSerialize;

impl<T: ?Sized + serde::Serialize> AmbiguousIfSerialize<ImplementsSerialize> for T {}

#[test]
fn the_public_view_types_have_no_serialize_impl() {
    <AccountReadings as AmbiguousIfSerialize<_>>::assert_not_serialize();
    <AccountReading as AmbiguousIfSerialize<_>>::assert_not_serialize();
    <AccountWindow as AmbiguousIfSerialize<_>>::assert_not_serialize();
    <ReadingWindow as AmbiguousIfSerialize<_>>::assert_not_serialize();
    <LastReading as AmbiguousIfSerialize<_>>::assert_not_serialize();
    <AcceptedReading as AmbiguousIfSerialize<_>>::assert_not_serialize();
    <RejectionTotals as AmbiguousIfSerialize<_>>::assert_not_serialize();
    <ReportState as AmbiguousIfSerialize<_>>::assert_not_serialize();
    <LengthClass as AmbiguousIfSerialize<_>>::assert_not_serialize();
    <Started as AmbiguousIfSerialize<_>>::assert_not_serialize();
    <ReadingSource as AmbiguousIfSerialize<_>>::assert_not_serialize();
    <Billing as AmbiguousIfSerialize<_>>::assert_not_serialize();
    <crate::quota::usage_body::UsageVendor as AmbiguousIfSerialize<_>>::assert_not_serialize();
    <crate::router::PolledUsageRejection as AmbiguousIfSerialize<_>>::assert_not_serialize();
}
