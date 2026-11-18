use std::collections::BTreeSet;
use std::time::Duration;

use routectl_router::UsageVendor;

use super::{JitterSource, PollOutcome, PollSchedule, RandomJitter, SeatCandidate, SeatState};
use crate::server::seat_driver::SeatVerdict;

const SEC: i64 = 1_000;
const MIN: i64 = 60 * SEC;
/// An arbitrary wall-clock origin, far from zero so subtraction stays positive.
const START_MS: i64 = 1_800_000_000_000;

/// Draws the same value every time (clamped to the requested maximum).
struct FixedJitter(u64);

impl JitterSource for FixedJitter {
    fn draw_ms(&mut self, max_ms: u64) -> u64 {
        self.0.min(max_ms)
    }
}

/// Draws the maximum the schedule asks for, so first-sight due times land
/// at the end of the boot spread and jitter at its 60 s ceiling.
struct MaxJitter;

impl JitterSource for MaxJitter {
    fn draw_ms(&mut self, max_ms: u64) -> u64 {
        max_ms
    }
}

/// Returns more than asked, as a faulty source might.
struct OversizedJitter;

impl JitterSource for OversizedJitter {
    fn draw_ms(&mut self, _max_ms: u64) -> u64 {
        u64::MAX
    }
}

fn seat(account: &str, vendor: UsageVendor, last_accepted_ms: Option<i64>) -> SeatCandidate<'_> {
    SeatCandidate {
        account,
        vendor,
        last_accepted_ms,
    }
}

fn picked<'a>(seats: &[SeatCandidate<'a>], indices: &[usize]) -> Vec<&'a str> {
    indices.iter().map(|&index| seats[index].account).collect()
}

const ALL_OUTCOMES: [PollOutcome; 13] = [
    PollOutcome::Accepted,
    PollOutcome::NoWindows,
    PollOutcome::TokenNotReady,
    PollOutcome::Paused(None),
    PollOutcome::Paused(Some(Duration::MAX)),
    PollOutcome::Redirect,
    PollOutcome::Auth,
    PollOutcome::Http(500),
    PollOutcome::Timeout,
    PollOutcome::Transport,
    PollOutcome::TooLarge,
    PollOutcome::Malformed,
    PollOutcome::Superseded,
];

#[test]
fn seat_with_reading_is_due_only_after_five_minutes_plus_its_jitter() {
    let jitter_ms = 37 * SEC;
    let mut schedule = PollSchedule::new(FixedJitter(u64::try_from(jitter_ms).unwrap()));
    let reading_at = START_MS;
    let seats = [seat("anthropic", UsageVendor::Anthropic, Some(reading_at))];

    let at_four_min = schedule.sweep(reading_at + 4 * MIN, &seats);
    let just_before = schedule.sweep(reading_at + 5 * MIN + jitter_ms - 1, &seats);
    let at_due = schedule.sweep(reading_at + 5 * MIN + jitter_ms, &seats);

    assert!(at_four_min.is_empty());
    assert!(just_before.is_empty());
    assert_eq!(at_due, vec![0]);
}

#[test]
fn a_fresher_reading_from_any_source_pushes_the_due_time_out() {
    let mut schedule = PollSchedule::new(FixedJitter(0));
    let old = [seat("anthropic", UsageVendor::Anthropic, Some(START_MS))];
    let fresh = [seat(
        "anthropic",
        UsageVendor::Anthropic,
        Some(START_MS + 4 * MIN),
    )];

    assert_eq!(schedule.sweep(START_MS + 5 * MIN, &old), vec![0]);
    assert!(schedule.sweep(START_MS + 5 * MIN, &fresh).is_empty());
    assert_eq!(schedule.sweep(START_MS + 9 * MIN, &fresh), vec![0]);
}

#[test]
fn the_last_attempt_also_starts_the_interval() {
    let mut schedule = PollSchedule::new(FixedJitter(0));
    let seats = [seat("anthropic", UsageVendor::Anthropic, Some(START_MS))];
    let attempt_at = START_MS + 6 * MIN;
    schedule.sweep(attempt_at, &seats);

    schedule.record(
        "anthropic",
        UsageVendor::Anthropic,
        attempt_at,
        PollOutcome::TokenNotReady,
    );

    assert!(schedule.sweep(attempt_at + 5 * MIN - 1, &seats).is_empty());
    assert_eq!(schedule.sweep(attempt_at + 5 * MIN, &seats), vec![0]);
}

#[test]
fn jitter_is_redrawn_after_each_attempt() {
    let mut schedule = PollSchedule::new(RandomJitter::seeded(7));
    let seats = [seat("anthropic", UsageVendor::Anthropic, None)];
    let mut gaps = BTreeSet::new();
    let mut now = START_MS + 10 * MIN;
    for _ in 0..8 {
        schedule.sweep(now, &seats);
        schedule.record(
            "anthropic",
            UsageVendor::Anthropic,
            now,
            PollOutcome::Accepted,
        );
        schedule.sweep(now, &seats);
        let next_due = schedule
            .seat("anthropic")
            .map(SeatState::next_due_ms)
            .unwrap();
        let gap = next_due - now;
        assert!(
            (5 * MIN..=6 * MIN).contains(&gap),
            "gap {gap} outside 5-6 min"
        );
        gaps.insert(gap);
        now = next_due;
    }

    assert!(gaps.len() > 1, "jitter never changed: {gaps:?}");
}

#[test]
fn boot_due_times_are_pairwise_distinct_and_inside_five_minutes() {
    let accounts: Vec<String> = (0..12).map(|n| format!("anthropic:seat-{n}")).collect();
    let seats: Vec<SeatCandidate<'_>> = accounts
        .iter()
        .map(|account| seat(account, UsageVendor::Anthropic, None))
        .collect();
    let mut schedule = PollSchedule::new(RandomJitter::seeded(42));

    schedule.sweep(START_MS, &seats);

    let due: Vec<i64> = accounts
        .iter()
        .map(|account| schedule.seat(account).map(SeatState::next_due_ms).unwrap())
        .collect();
    let distinct: BTreeSet<i64> = due.iter().copied().collect();
    assert_eq!(distinct.len(), due.len(), "boot due times collide: {due:?}");
    assert!(
        due.iter()
            .all(|&at| (START_MS..=START_MS + 5 * MIN).contains(&at)),
        "{due:?}"
    );
}

#[test]
fn a_boot_seat_is_not_due_before_its_drawn_time() {
    let mut schedule = PollSchedule::new(MaxJitter);
    let seats = [seat("anthropic", UsageVendor::Anthropic, None)];

    assert!(schedule.sweep(START_MS, &seats).is_empty());
    assert!(schedule.sweep(START_MS + 5 * MIN - 1, &seats).is_empty());
    assert_eq!(schedule.sweep(START_MS + 5 * MIN, &seats), vec![0]);
}

#[test]
fn after_a_long_gap_one_sweep_yields_one_seat_per_vendor() {
    let mut schedule = PollSchedule::new(RandomJitter::seeded(3));
    let seats = [
        seat(
            "anthropic:a",
            UsageVendor::Anthropic,
            Some(START_MS + 3 * SEC),
        ),
        seat("anthropic:b", UsageVendor::Anthropic, Some(START_MS)),
        seat(
            "anthropic:c",
            UsageVendor::Anthropic,
            Some(START_MS + 2 * SEC),
        ),
        seat("anthropic:d", UsageVendor::Anthropic, Some(START_MS + MIN)),
        seat("codex:a", UsageVendor::Codex, Some(START_MS)),
        seat("codex:b", UsageVendor::Codex, Some(START_MS + MIN)),
    ];

    let indices = schedule.sweep(START_MS + 2 * 60 * MIN, &seats);

    let vendors: Vec<UsageVendor> = indices.iter().map(|&index| seats[index].vendor).collect();
    assert_eq!(vendors, vec![UsageVendor::Anthropic, UsageVendor::Codex]);
}

#[test]
fn the_vendor_pick_is_the_seat_that_has_waited_longest() {
    let mut schedule = PollSchedule::new(FixedJitter(0));
    let seats = [
        seat(
            "anthropic:a",
            UsageVendor::Anthropic,
            Some(START_MS + 3 * SEC),
        ),
        seat("anthropic:b", UsageVendor::Anthropic, Some(START_MS)),
        seat(
            "anthropic:c",
            UsageVendor::Anthropic,
            Some(START_MS + 2 * SEC),
        ),
    ];

    let indices = schedule.sweep(START_MS + 2 * 60 * MIN, &seats);

    assert_eq!(picked(&seats, &indices), vec!["anthropic:b"]);
}

/// Each row: the outcome that ends the attempt, then the gap from that
/// attempt to the seat's next due time (jitter fixed at zero).
#[test]
fn error_backoff_reads_5_10_15_15_minutes_and_success_clears_it() {
    let mut schedule = PollSchedule::new(FixedJitter(0));
    let seats = [seat("codex", UsageVendor::Codex, Some(START_MS))];
    let rows = [
        (PollOutcome::Timeout, 5 * MIN, 1),
        (PollOutcome::Http(500), 10 * MIN, 2),
        (PollOutcome::Transport, 15 * MIN, 3),
        (PollOutcome::Malformed, 15 * MIN, 4),
        (PollOutcome::Accepted, 5 * MIN, 0),
        (PollOutcome::Auth, 5 * MIN, 1),
    ];
    let mut now = START_MS + 5 * MIN;
    for (outcome, expected_gap, expected_streak) in rows {
        assert_eq!(schedule.sweep(now, &seats), vec![0], "{outcome:?}: not due");

        schedule.record("codex", UsageVendor::Codex, now, outcome);
        schedule.sweep(now, &seats);

        let state = schedule.seat("codex").unwrap();
        assert_eq!(state.next_due_ms() - now, expected_gap, "{outcome:?}");
        assert_eq!(state.error_streak(), expected_streak, "{outcome:?}");
        assert!(
            schedule.sweep(now + expected_gap - 1, &seats).is_empty(),
            "{outcome:?}"
        );
        now += expected_gap;
    }
}

#[test]
fn backoff_holds_even_when_a_header_reading_is_old() {
    let mut schedule = PollSchedule::new(FixedJitter(0));
    let seats = [seat("codex", UsageVendor::Codex, Some(START_MS))];
    let mut now = START_MS + 5 * MIN;
    for _ in 0..3 {
        schedule.sweep(now, &seats);
        schedule.record("codex", UsageVendor::Codex, now, PollOutcome::Timeout);
        now += 5 * MIN;
    }
    let attempt_at = now - 5 * MIN;

    assert!(schedule.sweep(attempt_at + 15 * MIN - 1, &seats).is_empty());
    assert_eq!(schedule.sweep(attempt_at + 15 * MIN, &seats), vec![0]);
}

/// Each row: the 429's `Retry-After`, then how long the vendor stays paused.
#[test]
fn a_429_pauses_only_its_vendor_for_the_capped_wait() {
    let rows: [(&str, Option<Duration>, i64); 6] = [
        ("120 s", Some(Duration::from_mins(2)), 120 * SEC),
        ("7200 s capped", Some(Duration::from_hours(2)), 60 * MIN),
        ("absent", None, 15 * MIN),
        ("zero", Some(Duration::ZERO), 0),
        ("u64::MAX s", Some(Duration::from_secs(u64::MAX)), 60 * MIN),
        ("Duration::MAX", Some(Duration::MAX), 60 * MIN),
    ];
    for (name, retry_after, expected_pause) in rows {
        let mut schedule = PollSchedule::new(FixedJitter(0));
        let seats = [
            seat("anthropic:a", UsageVendor::Anthropic, Some(START_MS)),
            seat("anthropic:b", UsageVendor::Anthropic, Some(START_MS)),
            seat("codex", UsageVendor::Codex, Some(START_MS)),
        ];
        let now = START_MS + 5 * MIN;
        assert_eq!(schedule.sweep(now, &seats), vec![0, 2], "{name}");

        schedule.record(
            "anthropic:a",
            UsageVendor::Anthropic,
            now,
            PollOutcome::Paused(retry_after),
        );

        assert_eq!(
            schedule.pause_until(UsageVendor::Anthropic),
            Some(now + expected_pause),
            "{name}"
        );
        assert_eq!(schedule.pause_until(UsageVendor::Codex), None, "{name}");
        if expected_pause > 0 {
            assert_eq!(
                picked(&seats, &schedule.sweep(now + expected_pause - 1, &seats)),
                vec!["codex"],
                "{name}"
            );
        }
        assert_eq!(
            picked(&seats, &schedule.sweep(now + expected_pause, &seats)),
            vec!["anthropic:b", "codex"],
            "{name}"
        );
        assert_eq!(schedule.pause_until(UsageVendor::Anthropic), None, "{name}");
    }
}

#[test]
fn a_429_does_not_extend_the_seat_error_backoff() {
    let mut schedule = PollSchedule::new(FixedJitter(0));
    let seats = [seat("anthropic", UsageVendor::Anthropic, Some(START_MS))];
    let now = START_MS + 5 * MIN;
    schedule.sweep(now, &seats);

    schedule.record(
        "anthropic",
        UsageVendor::Anthropic,
        now,
        PollOutcome::Paused(None),
    );

    assert_eq!(schedule.seat("anthropic").unwrap().error_streak(), 0);
}

#[test]
fn time_arithmetic_saturates_at_the_extremes() {
    let mut schedule = PollSchedule::new(OversizedJitter);
    let seats = [
        seat("anthropic", UsageVendor::Anthropic, Some(i64::MAX)),
        seat("codex", UsageVendor::Codex, None),
    ];

    assert!(schedule.sweep(i64::MAX - 1, &seats).is_empty());
    schedule.record(
        "codex",
        UsageVendor::Codex,
        i64::MAX,
        PollOutcome::Paused(Some(Duration::MAX)),
    );
    for _ in 0..5 {
        schedule.record(
            "anthropic",
            UsageVendor::Anthropic,
            i64::MAX,
            PollOutcome::Timeout,
        );
    }
    assert_eq!(schedule.pause_until(UsageVendor::Codex), Some(i64::MAX));
    assert!(schedule.sweep(i64::MIN, &seats).is_empty());
    assert_eq!(
        schedule.seat("codex").unwrap().next_due_ms(),
        i64::MIN + 60 * MIN + MIN
    );
}

#[test]
fn an_oversized_jitter_draw_is_clamped_to_the_spread() {
    let mut schedule = PollSchedule::new(OversizedJitter);
    let seats = [seat("anthropic", UsageVendor::Anthropic, Some(START_MS))];

    schedule.sweep(START_MS, &seats);

    assert_eq!(
        schedule.seat("anthropic").unwrap().next_due_ms(),
        START_MS + 6 * MIN
    );
}

#[test]
fn state_for_a_seat_no_longer_managed_is_dropped_on_the_next_sweep() {
    let mut schedule = PollSchedule::new(FixedJitter(0));
    let both = [
        seat("anthropic", UsageVendor::Anthropic, Some(START_MS)),
        seat("codex", UsageVendor::Codex, Some(START_MS)),
    ];
    let now = START_MS + 5 * MIN;
    schedule.sweep(now, &both);
    schedule.record("codex", UsageVendor::Codex, now, PollOutcome::Timeout);
    assert!(schedule.seat("codex").is_some());

    schedule.sweep(now + MIN, &both[..1]);

    assert!(schedule.seat("codex").is_none());
    assert!(schedule.seat("anthropic").is_some());
}

#[test]
fn a_seat_that_returns_starts_fresh() {
    let mut schedule = PollSchedule::new(FixedJitter(0));
    let seats = [seat("codex", UsageVendor::Codex, Some(START_MS))];
    let now = START_MS + 5 * MIN;
    schedule.sweep(now, &seats);
    schedule.record("codex", UsageVendor::Codex, now, PollOutcome::Http(503));
    schedule.sweep(now, &[]);

    schedule.sweep(now + MIN, &seats);

    let state = schedule.seat("codex").unwrap();
    assert_eq!(state.error_streak(), 0);
    assert_eq!(state.last_attempt_ms(), None);
}

#[test]
fn a_late_outcome_for_a_dropped_seat_creates_no_state_but_still_pauses_its_vendor() {
    let mut schedule = PollSchedule::new(FixedJitter(0));

    schedule.record(
        "anthropic",
        UsageVendor::Anthropic,
        START_MS,
        PollOutcome::Paused(None),
    );

    assert!(schedule.seat("anthropic").is_none());
    assert_eq!(
        schedule.pause_until(UsageVendor::Anthropic),
        Some(START_MS + 15 * MIN)
    );
}

#[test]
fn record_keeps_the_last_attempt_and_its_outcome() {
    let mut schedule = PollSchedule::new(FixedJitter(0));
    let seats = [seat("codex", UsageVendor::Codex, None)];
    schedule.sweep(START_MS, &seats);

    schedule.record(
        "codex",
        UsageVendor::Codex,
        START_MS + SEC,
        PollOutcome::Redirect,
    );

    let state = schedule.seat("codex").unwrap();
    assert_eq!(state.last_attempt_ms(), Some(START_MS + SEC));
    assert_eq!(state.last_outcome(), Some(PollOutcome::Redirect));
}

#[test]
fn no_poll_outcome_maps_to_a_definitive_verdict() {
    for outcome in ALL_OUTCOMES {
        assert_ne!(outcome.verdict(), SeatVerdict::Definitive, "{outcome:?}");
    }
}

/// Each row: an outcome and the verdict the driver's health counts it as.
#[test]
fn poll_outcomes_map_to_quiet_or_transient() {
    let rows = [
        (PollOutcome::Accepted, SeatVerdict::Quiet),
        (PollOutcome::NoWindows, SeatVerdict::Quiet),
        (PollOutcome::TokenNotReady, SeatVerdict::Quiet),
        (PollOutcome::Superseded, SeatVerdict::Quiet),
        (PollOutcome::Paused(None), SeatVerdict::Transient),
        (PollOutcome::Redirect, SeatVerdict::Transient),
        (PollOutcome::Auth, SeatVerdict::Transient),
        (PollOutcome::Http(500), SeatVerdict::Transient),
        (PollOutcome::Timeout, SeatVerdict::Transient),
        (PollOutcome::Transport, SeatVerdict::Transient),
        (PollOutcome::TooLarge, SeatVerdict::Transient),
        (PollOutcome::Malformed, SeatVerdict::Transient),
    ];
    for (outcome, verdict) in rows {
        assert_eq!(outcome.verdict(), verdict, "{outcome:?}");
    }
}

#[test]
fn outcome_tokens_are_distinct_per_kind() {
    let tokens: BTreeSet<&str> = ALL_OUTCOMES
        .iter()
        .map(|outcome| outcome.as_str())
        .collect();

    assert_eq!(tokens.len(), 12);
}

#[test]
fn the_os_seeded_jitter_stays_inside_the_requested_range() {
    let mut jitter = RandomJitter::from_os();

    assert!((0..200).all(|_| jitter.draw_ms(60_000) <= 60_000));
}

const YEAR: i64 = 365 * 24 * 60 * MIN;

/// Each row: the outcome that ends the attempt, then the backoff it sets
/// before the seat's jitter is added.
#[test]
fn every_backoff_deadline_carries_the_seat_jitter() {
    let jitter_ms = 37 * SEC;
    let mut schedule = PollSchedule::new(FixedJitter(u64::try_from(jitter_ms).unwrap()));
    let seats = [seat("codex", UsageVendor::Codex, Some(START_MS))];
    let rows = [
        ("streak 1", PollOutcome::Timeout, 5 * MIN),
        ("streak 2", PollOutcome::Http(500), 10 * MIN),
        ("streak 3", PollOutcome::Transport, 15 * MIN),
        ("capped", PollOutcome::Malformed, 15 * MIN),
    ];
    let mut now = START_MS + 5 * MIN + jitter_ms;
    for (name, outcome, backoff) in rows {
        assert_eq!(schedule.sweep(now, &seats), vec![0], "{name}: not due");

        schedule.record("codex", UsageVendor::Codex, now, outcome);

        let due = now + backoff + jitter_ms;
        assert!(schedule.sweep(due - 1, &seats).is_empty(), "{name}");
        assert_eq!(schedule.seat("codex").unwrap().next_due_ms(), due, "{name}");
        now = due;
    }
}

#[test]
fn a_vendor_pause_ends_at_each_seats_own_jitter() {
    let jitter_ms = 23 * SEC;
    let mut schedule = PollSchedule::new(FixedJitter(u64::try_from(jitter_ms).unwrap()));
    let seats = [
        seat("anthropic:a", UsageVendor::Anthropic, Some(START_MS)),
        seat("anthropic:b", UsageVendor::Anthropic, Some(START_MS)),
    ];
    let now = START_MS + 5 * MIN + jitter_ms;
    assert_eq!(schedule.sweep(now, &seats), vec![0]);
    schedule.record(
        "anthropic:a",
        UsageVendor::Anthropic,
        now,
        PollOutcome::Paused(Some(Duration::from_mins(2))),
    );
    let pause_end = now + 2 * MIN;

    assert!(schedule.sweep(pause_end, &seats).is_empty());
    assert!(schedule.sweep(pause_end + jitter_ms - 1, &seats).is_empty());
    assert_eq!(
        picked(&seats, &schedule.sweep(pause_end + jitter_ms, &seats)),
        vec!["anthropic:b"]
    );
}

#[test]
fn a_backward_clock_step_after_an_attempt_delays_the_seat_by_at_most_one_backoff() {
    let mut schedule = PollSchedule::new(MaxJitter);
    let seats = [seat("codex", UsageVendor::Codex, Some(START_MS))];
    let attempt_at = START_MS + 6 * MIN;
    assert_eq!(schedule.sweep(attempt_at, &seats), vec![0]);
    for _ in 0..3 {
        schedule.record(
            "codex",
            UsageVendor::Codex,
            attempt_at,
            PollOutcome::Timeout,
        );
    }
    let stepped_back = attempt_at - YEAR;

    assert!(schedule.sweep(stepped_back, &seats).is_empty());

    assert_eq!(
        schedule.sweep(stepped_back + 15 * MIN + MIN, &seats),
        vec![0]
    );
}

#[test]
fn a_backward_clock_step_after_a_429_pauses_the_vendor_at_most_an_hour() {
    let mut schedule = PollSchedule::new(MaxJitter);
    let seats = [
        seat("anthropic:a", UsageVendor::Anthropic, Some(START_MS)),
        seat("anthropic:b", UsageVendor::Anthropic, Some(START_MS)),
    ];
    let attempt_at = START_MS + 6 * MIN;
    assert_eq!(schedule.sweep(attempt_at, &seats), vec![0]);
    schedule.record(
        "anthropic:a",
        UsageVendor::Anthropic,
        attempt_at,
        PollOutcome::Paused(Some(Duration::from_hours(1))),
    );
    let stepped_back = attempt_at - YEAR;

    assert!(schedule.sweep(stepped_back, &seats).is_empty());
    assert!(
        schedule
            .pause_until(UsageVendor::Anthropic)
            .is_some_and(|until| until <= stepped_back + 60 * MIN)
    );

    assert_eq!(
        schedule.sweep(stepped_back + 60 * MIN + MIN, &seats).len(),
        1
    );
}

#[test]
fn a_backward_clock_step_before_the_first_poll_keeps_the_boot_spread() {
    let mut schedule = PollSchedule::new(MaxJitter);
    let seats = [seat("codex", UsageVendor::Codex, None)];
    assert!(schedule.sweep(START_MS, &seats).is_empty());
    let stepped_back = START_MS - YEAR;

    assert!(schedule.sweep(stepped_back, &seats).is_empty());

    assert_eq!(schedule.sweep(stepped_back + 5 * MIN, &seats), vec![0]);
}

#[test]
fn a_forward_clock_step_makes_seats_due_at_once_but_one_per_vendor() {
    let mut schedule = PollSchedule::new(RandomJitter::seeded(11));
    let seats = [
        seat("anthropic:a", UsageVendor::Anthropic, Some(START_MS)),
        seat("anthropic:b", UsageVendor::Anthropic, Some(START_MS + SEC)),
        seat(
            "anthropic:c",
            UsageVendor::Anthropic,
            Some(START_MS + 2 * SEC),
        ),
        seat("codex", UsageVendor::Codex, Some(START_MS)),
    ];
    assert!(schedule.sweep(START_MS + MIN, &seats).is_empty());

    let indices = schedule.sweep(START_MS + YEAR, &seats);

    let vendors: Vec<UsageVendor> = indices.iter().map(|&index| seats[index].vendor).collect();
    assert_eq!(vendors, vec![UsageVendor::Anthropic, UsageVendor::Codex]);
}

/// A reading stamped after the sweep's clock counts from the sweep that
/// first saw it, and does not slide forward on later sweeps.
#[test]
fn a_reading_stamped_in_the_future_counts_from_when_it_was_first_seen() {
    let jitter_ms = 11 * SEC;
    let mut schedule = PollSchedule::new(FixedJitter(u64::try_from(jitter_ms).unwrap()));
    let seats = [seat(
        "anthropic",
        UsageVendor::Anthropic,
        Some(START_MS + YEAR),
    )];

    assert!(schedule.sweep(START_MS, &seats).is_empty());
    assert!(schedule.sweep(START_MS + 3 * MIN, &seats).is_empty());
    assert!(
        schedule
            .sweep(START_MS + 5 * MIN + jitter_ms - 1, &seats)
            .is_empty()
    );

    assert_eq!(
        schedule.sweep(START_MS + 5 * MIN + jitter_ms, &seats),
        vec![0]
    );
}

#[test]
fn a_seat_recorded_at_the_clock_ceiling_is_not_due_on_every_sweep() {
    let mut schedule = PollSchedule::new(FixedJitter(0));
    let seats = [seat("anthropic", UsageVendor::Anthropic, Some(START_MS))];
    assert_eq!(schedule.sweep(i64::MAX, &seats), vec![0]);

    schedule.record(
        "anthropic",
        UsageVendor::Anthropic,
        i64::MAX,
        PollOutcome::Accepted,
    );

    for _ in 0..3 {
        assert!(schedule.sweep(i64::MAX, &seats).is_empty());
    }
}

#[test]
fn a_backward_clock_step_while_a_seat_waits_out_its_pause_jitter_is_bounded() {
    let jitter_ms = 23 * SEC;
    let mut schedule = PollSchedule::new(FixedJitter(u64::try_from(jitter_ms).unwrap()));
    let seats = [
        seat("anthropic:a", UsageVendor::Anthropic, Some(START_MS)),
        seat("anthropic:b", UsageVendor::Anthropic, Some(START_MS)),
    ];
    let now = START_MS + 5 * MIN + jitter_ms;
    assert_eq!(schedule.sweep(now, &seats), vec![0]);
    schedule.record(
        "anthropic:a",
        UsageVendor::Anthropic,
        now,
        PollOutcome::Paused(Some(Duration::from_mins(2))),
    );
    assert!(schedule.sweep(now + 2 * MIN, &seats).is_empty());
    assert_eq!(schedule.pause_until(UsageVendor::Anthropic), None);
    let stepped_back = now - YEAR;
    assert!(schedule.sweep(stepped_back, &seats).is_empty());

    let indices = schedule.sweep(stepped_back + 60 * MIN + jitter_ms, &seats);

    assert_eq!(indices.len(), 1);
}

/// Each row lands one outcome a minute after the last and reads the failure
/// run's start. The first failure stamps it, later failures keep it, an
/// accepted poll clears it, and an outcome that says nothing about the
/// endpoint leaves it as it was.
#[test]
fn failure_since_marks_the_first_failure_of_a_run_and_clears_on_acceptance() {
    let first_failure = START_MS + 3 * MIN;
    let second_run = START_MS + 9 * MIN;
    let rows: [(PollOutcome, Option<i64>); 12] = [
        (PollOutcome::TokenNotReady, None),
        (PollOutcome::Superseded, None),
        (PollOutcome::Timeout, Some(first_failure)),
        (PollOutcome::Http(503), Some(first_failure)),
        (PollOutcome::TokenNotReady, Some(first_failure)),
        (PollOutcome::Superseded, Some(first_failure)),
        (PollOutcome::Paused(None), Some(first_failure)),
        (PollOutcome::Accepted, None),
        (
            PollOutcome::Paused(Some(Duration::from_mins(2))),
            Some(second_run),
        ),
        (PollOutcome::Malformed, Some(second_run)),
        (PollOutcome::NoWindows, None),
        (PollOutcome::Auth, Some(START_MS + 12 * MIN)),
    ];
    let mut schedule = PollSchedule::new(FixedJitter(0));
    let seats = [seat("codex", UsageVendor::Codex, None)];
    schedule.sweep(START_MS, &seats);
    for (minute, (outcome, expected)) in (1_i64..).zip(rows) {
        let now = START_MS + minute * MIN;

        schedule.record("codex", UsageVendor::Codex, now, outcome);

        assert_eq!(
            schedule.seat("codex").unwrap().failure_since_ms(),
            expected,
            "minute {minute}: {outcome:?}"
        );
    }
}

/// Every failing kind starts a run on its own, and every accepting kind ends
/// one: a run started by any failure reads as failing until acceptance.
#[test]
fn each_failing_outcome_starts_a_run_and_each_accepting_outcome_ends_it() {
    for outcome in ALL_OUTCOMES {
        let mut schedule = PollSchedule::new(FixedJitter(0));
        let seats = [seat("codex", UsageVendor::Codex, None)];
        schedule.sweep(START_MS, &seats);
        schedule.record("codex", UsageVendor::Codex, START_MS, PollOutcome::Timeout);
        let failing = outcome.verdict() == SeatVerdict::Transient;

        schedule.record("codex", UsageVendor::Codex, START_MS + MIN, outcome);

        let since = schedule.seat("codex").unwrap().failure_since_ms();
        let expected = match outcome {
            PollOutcome::Accepted | PollOutcome::NoWindows => None,
            _ => Some(START_MS),
        };
        assert_eq!(since, expected, "{outcome:?} after a failure");
        let mut fresh = PollSchedule::new(FixedJitter(0));
        fresh.sweep(START_MS, &seats);
        fresh.record("codex", UsageVendor::Codex, START_MS, outcome);
        assert_eq!(
            fresh.seat("codex").unwrap().failure_since_ms(),
            failing.then_some(START_MS),
            "{outcome:?} from a clean seat"
        );
    }
}

#[test]
fn a_backward_clock_step_pulls_the_failure_start_back_to_now() {
    let mut schedule = PollSchedule::new(FixedJitter(0));
    let seats = [seat("codex", UsageVendor::Codex, None)];
    let failed_at = START_MS + 10 * MIN;
    schedule.sweep(START_MS, &seats);
    schedule.record("codex", UsageVendor::Codex, failed_at, PollOutcome::Timeout);
    let stepped_back = failed_at - YEAR;

    schedule.sweep(stepped_back, &seats);

    assert_eq!(
        schedule.seat("codex").unwrap().failure_since_ms(),
        Some(stepped_back)
    );
}
