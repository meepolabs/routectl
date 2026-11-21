//! Coverage for the attention memory: recovery notes, first-seen ages, the
//! records-dropped rise window, doctor coverage, and the storage bounds. Every
//! read is driven by a fake clock through `AttentionInputs::now_ms`.

use std::sync::{Arc, Barrier};
use std::thread;

use routectl_core::failure_class::LastOutcome;
use routectl_router::{Finding, Status};

use super::*;
use routectl_router::Billing;
use routectl_router::runtime_state::UnusableCause;

use crate::commands::doctor::attention::{
    AccountSnapshot, AttentionItem, CredentialState, ItemState, PoolSnapshot,
    RECORDS_DROPPED_HOLD_MS, RefreshFailure, RouteSnapshot,
};

const START: i64 = 1_800_000_000_000;
const SEC: i64 = 1_000;
const MIN: i64 = 60 * SEC;
const READ_EVERY: i64 = 5 * SEC;

const POOL: &str = "claude-pool";
const POOL_KEY: &str = "pool_empty:claude-pool";
const ROUTE_KEY: &str = "route_circuit_open:claude-a";
const RECORDS_KEY: &str = "records_dropped:ledger";
const DOCTOR_KEY: &str = "doctor_failure:config/served-config";

fn at(now_ms: i64) -> AttentionInputs {
    AttentionInputs {
        now_ms,
        ..AttentionInputs::default()
    }
}

fn empty_pool(name: &str, configured: usize) -> PoolSnapshot {
    PoolSnapshot {
        pool: name.to_string(),
        configured,
        eligible: 0,
    }
}

fn with_empty_pool(now_ms: i64) -> AttentionInputs {
    AttentionInputs {
        pools: vec![empty_pool(POOL, 2)],
        ..at(now_ms)
    }
}

fn with_open_route(now_ms: i64, open_since_ms: i64) -> AttentionInputs {
    AttentionInputs {
        routes: vec![RouteSnapshot {
            state_key: "claude-a".to_string(),
            provider_entry: "anthropic-a".to_string(),
            account: None,
            last_outcome: Some(LastOutcome::Http5xx),
            circuit_open: true,
            open_since_ms: Some(open_since_ms),
        }],
        ..at(now_ms)
    }
}

fn with_loss(
    now_ms: i64,
    dropped_full: u64,
    dropped_closed: u64,
    write_errors: u64,
) -> AttentionInputs {
    AttentionInputs {
        record_loss: RecordLoss {
            dropped_full,
            dropped_closed,
            write_errors,
            last_rise_ms: None,
        },
        ..at(now_ms)
    }
}

fn with_doctor_failure(now_ms: i64) -> AttentionInputs {
    AttentionInputs {
        doctor_findings: vec![Finding {
            section: "config",
            name: "served-config".to_string(),
            status: Status::Fail,
            detail: "served config is unreadable".to_string(),
            remediation: None,
        }],
        ..at(now_ms)
    }
}

fn read(memory: &AttentionMemory, inputs: AttentionInputs) -> Vec<AttentionItem> {
    memory.build(inputs, DoctorCoverage::Current).items
}

fn find<'a>(items: &'a [AttentionItem], key: &str) -> Option<&'a AttentionItem> {
    items.iter().find(|item| item.key == key)
}

fn expect_item<'a>(items: &'a [AttentionItem], key: &str) -> &'a AttentionItem {
    find(items, key).unwrap_or_else(|| panic!("{key} is on the list: {items:#?}"))
}

#[test]
fn a_cleared_item_is_recovered_from_the_first_read_without_it_through_sixty_minutes() {
    let memory = AttentionMemory::new();
    read(&memory, with_empty_pool(START));
    let cleared_at = START + READ_EVERY;

    let first_clear = read(&memory, at(cleared_at));
    let at_sixty = read(&memory, at(cleared_at + FORGET_AFTER_MS));
    let past_sixty = read(&memory, at(cleared_at + FORGET_AFTER_MS + 1));

    let recovered = expect_item(&first_clear, POOL_KEY);
    assert_eq!(recovered.state, ItemState::Recovered);
    assert_eq!(recovered.recovered_at_ms, Some(cleared_at));
    let held = expect_item(&at_sixty, POOL_KEY);
    assert_eq!(held.state, ItemState::Recovered);
    assert_eq!(held.recovered_at_ms, Some(cleared_at));
    assert!(find(&past_sixty, POOL_KEY).is_none(), "{past_sixty:#?}");
}

#[test]
fn reads_every_five_seconds_do_not_extend_a_recovered_items_lifetime() {
    let memory = AttentionMemory::new();
    read(&memory, with_empty_pool(START));
    let cleared_at = START + READ_EVERY;

    let mut now = cleared_at;
    while now <= cleared_at + FORGET_AFTER_MS {
        let items = read(&memory, at(now));
        let held = expect_item(&items, POOL_KEY);
        assert_eq!(
            held.recovered_at_ms,
            Some(cleared_at),
            "read at +{} ms",
            now - cleared_at
        );
        now += READ_EVERY;
    }
    let after = read(&memory, at(cleared_at + FORGET_AFTER_MS + 1));

    assert!(find(&after, POOL_KEY).is_none(), "{after:#?}");
}

#[test]
fn a_recovered_item_that_recurs_is_active_again_with_a_fresh_first_seen() {
    let memory = AttentionMemory::new();
    read(&memory, with_empty_pool(START));
    read(&memory, at(START + MIN));
    let recurs_at = START + 10 * MIN;

    let items = read(&memory, with_empty_pool(recurs_at));

    let item = expect_item(&items, POOL_KEY);
    assert_eq!(item.state, ItemState::Active);
    assert_eq!(item.recovered_at_ms, None);
    assert_eq!(item.since_ms, Some(recurs_at));
    assert_eq!(items.iter().filter(|i| i.key == POOL_KEY).count(), 1);
}

/// A kind with no native since takes the first read that saw it, so its age
/// grows across reads; every other field follows the latest read, so the
/// stored item never freezes.
#[test]
fn an_item_without_a_native_since_keeps_its_first_seen_while_its_details_follow_each_read() {
    let memory = AttentionMemory::new();
    read(&memory, with_empty_pool(START));
    let later = AttentionInputs {
        pools: vec![empty_pool(POOL, 5)],
        ..at(START + 7 * MIN)
    };

    let items = read(&memory, later);

    let item = expect_item(&items, POOL_KEY);
    assert_eq!(item.since_ms, Some(START));
    assert!(
        item.raw_token.contains("configured=5"),
        "{}",
        item.raw_token
    );
    assert!(item.cause.contains("0 of 5"), "{}", item.cause);
}

#[test]
fn an_item_with_a_native_since_keeps_it() {
    let memory = AttentionMemory::new();
    let opened = START - 30 * MIN;

    read(&memory, with_open_route(START, opened));
    let items = read(&memory, with_open_route(START + MIN, opened));

    assert_eq!(expect_item(&items, ROUTE_KEY).since_ms, Some(opened));
}

#[test]
fn a_counter_rise_raises_records_dropped_until_five_minutes_after_the_last_rise() {
    let memory = AttentionMemory::new();
    read(&memory, with_loss(START, 0, 0, 0));
    let first_rise = START + READ_EVERY;
    let last_rise = first_rise + 2 * MIN;

    let at_first = read(&memory, with_loss(first_rise, 1, 0, 0));
    read(&memory, with_loss(last_rise, 3, 0, 0));
    let at_hold_end = read(
        &memory,
        with_loss(last_rise + RECORDS_DROPPED_HOLD_MS, 3, 0, 0),
    );
    let past_hold = last_rise + RECORDS_DROPPED_HOLD_MS + 1;
    let after_hold = read(&memory, with_loss(past_hold, 3, 0, 0));

    assert!(find(&read(&AttentionMemory::new(), at(START)), RECORDS_KEY).is_none());
    assert_eq!(expect_item(&at_first, RECORDS_KEY).state, ItemState::Active);
    let held = expect_item(&at_hold_end, RECORDS_KEY);
    assert_eq!(held.state, ItemState::Active);
    assert_eq!(held.since_ms, Some(first_rise));
    let recovered = expect_item(&after_hold, RECORDS_KEY);
    assert_eq!(recovered.state, ItemState::Recovered);
    assert_eq!(recovered.recovered_at_ms, Some(past_hold));
}

/// Each counter rises on its own; the other two stay at their old totals, and
/// a later read at the same totals must not count as a second rise. Totals
/// already nonzero at the first read are a rise from the process-start zeros,
/// so each row lets that settle before its own rise.
#[test]
fn an_unchanged_nonzero_counter_never_raises_records_dropped_again() {
    let rows: [(&str, (u64, u64, u64)); 3] = [
        ("dropped_full", (4, 1, 1)),
        ("dropped_closed", (1, 4, 1)),
        ("write_errors", (1, 1, 4)),
    ];
    for (name, (full, closed, errors)) in rows {
        let memory = AttentionMemory::new();
        read(&memory, with_loss(START, 1, 1, 1));
        let settled = START + RECORDS_DROPPED_HOLD_MS + 1;
        let before_rise = read(&memory, with_loss(settled, 1, 1, 1));
        let rise = settled + MIN;
        let quiet = rise + RECORDS_DROPPED_HOLD_MS + 1;

        let at_rise = read(&memory, with_loss(rise, full, closed, errors));
        let mut later = Vec::new();
        for step in 0..=3 {
            later = read(
                &memory,
                with_loss(quiet + step * 30 * MIN, full, closed, errors),
            );
        }

        assert_eq!(
            expect_item(&before_rise, RECORDS_KEY).state,
            ItemState::Recovered,
            "{name}: control, settled before its rise"
        );
        assert_eq!(
            expect_item(&at_rise, RECORDS_KEY).state,
            ItemState::Active,
            "{name}: its rise raises the item"
        );
        assert!(
            find(&later, RECORDS_KEY).is_none_or(|item| item.state == ItemState::Recovered),
            "{name}: unchanged totals never re-raise: {later:#?}"
        );
    }
}

#[test]
fn a_lower_total_from_an_older_read_is_not_a_rise() {
    let memory = AttentionMemory::new();
    read(&memory, with_loss(START, 5, 0, 0));
    let quiet = START + RECORDS_DROPPED_HOLD_MS + 1;
    read(&memory, with_loss(quiet, 5, 0, 0));

    read(&memory, with_loss(quiet + READ_EVERY, 4, 0, 0));
    let items = read(&memory, with_loss(quiet + 2 * READ_EVERY, 5, 0, 0));

    assert_eq!(expect_item(&items, RECORDS_KEY).state, ItemState::Recovered);
}

const ACCOUNT: &str = "anthropic#a";

fn seat(credential: CredentialState, refresh_failing: bool) -> AccountSnapshot {
    AccountSnapshot {
        account: ACCOUNT.to_string(),
        display: "claude-a (a)".to_string(),
        credential,
        fix: Some("routectl login anthropic --label a".to_string()),
        remediation: None,
        refresh_failure: refresh_failing.then_some(RefreshFailure {
            first_failure_ms: START - 10 * MIN,
            token: "transient",
        }),
        poll_failure: None,
        windows: Vec::new(),
        billing: Billing::Unknown,
        billing_fresh: false,
    }
}

const fn expired(since_ms: i64) -> CredentialState {
    CredentialState::Unusable {
        cause: UnusableCause::Expired,
        since_ms,
    }
}

const fn parked() -> CredentialState {
    CredentialState::Parked {
        reason: routectl_core::UpstreamReason::CredentialInvalid,
        until_ms: START + 30 * MIN,
    }
}

fn with_seat(now_ms: i64, account: AccountSnapshot, route_open: bool) -> AttentionInputs {
    AttentionInputs {
        accounts: vec![account],
        routes: vec![RouteSnapshot {
            state_key: "fast".to_string(),
            provider_entry: "anthropic-seat".to_string(),
            account: Some(ACCOUNT.to_string()),
            last_outcome: Some(LastOutcome::Http5xx),
            circuit_open: route_open,
            open_since_ms: route_open.then_some(START - MIN),
        }],
        ..at(now_ms)
    }
}

fn with_pool_failure(now_ms: i64, pool_counted: bool) -> AttentionInputs {
    AttentionInputs {
        pools: if pool_counted {
            vec![empty_pool(POOL, 2)]
        } else {
            Vec::new()
        },
        doctor_findings: vec![Finding {
            section: "pools",
            name: POOL.to_string(),
            status: Status::Fail,
            detail: "no member has a credential".to_string(),
            remediation: None,
        }],
        ..at(now_ms)
    }
}

/// An item the builder stops listing because a worse item now names the same
/// problem has not cleared, so it is never shown recovered while folded: a
/// refresh still failing once the token expires, a route still open once its
/// account is marked unusable or parked, a pools Fail once the pool's live
/// counts are read. What the read after the folded one shows depends on
/// what it carries: still folded, the item stays hidden; the worse item gone
/// and the folded condition still holding, it is active again; both gone, it
/// is recovered from that read. The `control` outcome, the same key cleared
/// with no folded read between, shows the key is recoverable at all.
#[test]
fn an_item_folded_into_a_worse_one_is_never_stamped_recovered() {
    let refresh_key = format!("account_refresh_failing:{ACCOUNT}");
    let route_key = "route_circuit_open:fast".to_string();
    type Read = fn(i64) -> AttentionInputs;
    // Each case: the folded condition listed on its own (also the read where
    // the worse item clears and it still holds), the read where a worse item
    // names it, and the read where both are gone.
    let cases: Vec<(&str, String, Read, Read, Read)> = vec![
        (
            "refresh under an expired credential",
            refresh_key.clone(),
            |t| with_seat(t, seat(CredentialState::Usable, true), false),
            |t| with_seat(t, seat(expired(START), true), false),
            |t| with_seat(t, seat(CredentialState::Usable, false), false),
        ),
        (
            "refresh under a parked credential",
            refresh_key,
            |t| with_seat(t, seat(CredentialState::Usable, true), false),
            |t| with_seat(t, seat(parked(), true), false),
            |t| with_seat(t, seat(CredentialState::Usable, false), false),
        ),
        (
            "open route under an unusable account",
            route_key.clone(),
            |t| with_seat(t, seat(CredentialState::Usable, false), true),
            |t| with_seat(t, seat(expired(START), false), true),
            |t| with_seat(t, seat(CredentialState::Usable, false), false),
        ),
        (
            "open route under a parked account",
            route_key,
            |t| with_seat(t, seat(CredentialState::Usable, false), true),
            |t| with_seat(t, seat(parked(), false), true),
            |t| with_seat(t, seat(CredentialState::Usable, false), false),
        ),
        (
            "pools Fail under pool_empty",
            "doctor_failure:pools/claude-pool".to_string(),
            |t| with_pool_failure(t, false),
            |t| with_pool_failure(t, true),
            at,
        ),
    ];
    let then = START + 2 * READ_EVERY;

    for (case, key, listed, folded, cleared) in cases {
        let outcomes: [(&str, bool, Read, Option<ItemState>); 4] = [
            ("stays folded", true, folded, None),
            ("worse item clears", true, listed, Some(ItemState::Active)),
            ("both clear", true, cleared, Some(ItemState::Recovered)),
            ("control", false, cleared, Some(ItemState::Recovered)),
        ];
        for (outcome, folds_first, next, expected) in outcomes {
            let name = format!("{case}, {outcome}");
            let memory = AttentionMemory::new();
            let first = read(&memory, listed(START));
            let during = folds_first.then(|| read(&memory, folded(START + READ_EVERY)));

            let after = read(&memory, next(then));

            assert_eq!(
                expect_item(&first, &key).state,
                ItemState::Active,
                "{name}: premise, listed on its own"
            );
            if let Some(during) = &during {
                assert!(
                    find(during, &key).is_none(),
                    "{name}: folded, not recovered: {during:#?}"
                );
            }
            let shown = find(&after, &key);
            assert_eq!(shown.map(|item| item.state), expected, "{name}: {after:#?}");
            if expected == Some(ItemState::Recovered) {
                assert_eq!(
                    shown.and_then(|item| item.recovered_at_ms),
                    Some(then),
                    "{name}: recovered from the first read without it"
                );
            }
        }
    }
}

/// A condition shown recovered that returns folded into a worse item has not
/// stayed cleared: it leaves the list, and when both clear later it is
/// recovered from that later read, not from its first recovery.
#[test]
fn a_recovered_item_that_returns_folded_recovers_from_its_new_clearing() {
    let memory = AttentionMemory::new();
    let key = format!("account_refresh_failing:{ACCOUNT}");
    let failing = |t| with_seat(t, seat(CredentialState::Usable, true), false);
    let folded = |t| with_seat(t, seat(expired(START), true), false);
    let clear = |t| with_seat(t, seat(CredentialState::Usable, false), false);
    read(&memory, failing(START));
    let first_clear = read(&memory, clear(START + READ_EVERY));
    let returned = read(&memory, folded(START + 2 * READ_EVERY));

    let after = read(&memory, clear(START + 3 * READ_EVERY));

    assert_eq!(
        expect_item(&first_clear, &key).recovered_at_ms,
        Some(START + READ_EVERY),
        "premise: recovered once"
    );
    assert!(find(&returned, &key).is_none(), "{returned:#?}");
    let item = expect_item(&after, &key);
    assert_eq!(item.state, ItemState::Recovered);
    assert_eq!(item.recovered_at_ms, Some(START + 3 * READ_EVERY));
}

/// Once the worse item clears while the folded condition still holds, the
/// folded item is listed active again with its own start, never recovered in
/// between.
#[test]
fn a_folded_refresh_is_active_again_when_the_credential_item_clears() {
    let memory = AttentionMemory::new();
    let key = format!("account_refresh_failing:{ACCOUNT}");
    read(
        &memory,
        with_seat(START, seat(CredentialState::Usable, true), false),
    );
    read(
        &memory,
        with_seat(START + READ_EVERY, seat(expired(START), true), false),
    );

    let after = read(
        &memory,
        with_seat(
            START + 2 * READ_EVERY,
            seat(CredentialState::Usable, true),
            false,
        ),
    );

    let item = expect_item(&after, &key);
    assert_eq!(item.state, ItemState::Active);
    assert_eq!(item.since_ms, Some(START - 10 * MIN));
}

#[test]
fn doctor_coverage_unavailable_leaves_earlier_doctor_items_neither_cleared_nor_recovered() {
    let memory = AttentionMemory::new();
    read(&memory, with_doctor_failure(START));
    let unavailable_read = AttentionInputs {
        pools: vec![empty_pool(POOL, 2)],
        ..at(START + MIN)
    };

    let held = memory
        .build(unavailable_read, DoctorCoverage::Unavailable)
        .items;
    let current = read(&memory, at(START + 2 * MIN));

    let doctor = expect_item(&held, DOCTOR_KEY);
    assert_eq!(doctor.state, ItemState::Active);
    assert_eq!(doctor.recovered_at_ms, None);
    let recovered = expect_item(&current, DOCTOR_KEY);
    assert_eq!(
        recovered.state,
        ItemState::Recovered,
        "control: a current report clears it"
    );
    assert_eq!(recovered.recovered_at_ms, Some(START + 2 * MIN));
}

#[test]
fn doctor_coverage_unavailable_still_recovers_other_kinds() {
    let memory = AttentionMemory::new();
    read(&memory, with_empty_pool(START));

    let items = memory
        .build(at(START + MIN), DoctorCoverage::Unavailable)
        .items;

    assert_eq!(expect_item(&items, POOL_KEY).state, ItemState::Recovered);
}

#[test]
fn doctor_coverage_unavailable_keeps_a_recovered_doctor_items_first_recovery() {
    let memory = AttentionMemory::new();
    read(&memory, with_doctor_failure(START));
    read(&memory, at(START + MIN));

    let items = memory
        .build(at(START + 2 * MIN), DoctorCoverage::Unavailable)
        .items;

    let doctor = expect_item(&items, DOCTOR_KEY);
    assert_eq!(doctor.state, ItemState::Recovered);
    assert_eq!(doctor.recovered_at_ms, Some(START + MIN));
}

/// Every read renames the pool, as a config edited on every reload would: each
/// old name becomes a recovered entry. The store stays at the bound and keeps
/// the newest, and the current read's item is always kept.
#[test]
fn config_churn_cannot_grow_the_stored_entries_past_the_count_bound() {
    let memory = AttentionMemory::new();
    let reads = 3 * MAX_ENTRIES;

    let mut last = Vec::new();
    let mut peak = 0;
    for n in 0..reads {
        let inputs = AttentionInputs {
            pools: vec![empty_pool(&format!("pool-{n:04}"), 1)],
            ..at(START + i64::try_from(n).expect("small") * SEC)
        };
        last = read(&memory, inputs);
        peak = peak.max(memory.stored_len());
    }

    assert_eq!(peak, MAX_ENTRIES, "the churn fills the store to its bound");
    assert_eq!(last.len(), MAX_ENTRIES);
    let newest = format!("pool_empty:pool-{:04}", reads - 1);
    assert_eq!(expect_item(&last, &newest).state, ItemState::Active);
    let oldest_kept = format!("pool_empty:pool-{:04}", reads - MAX_ENTRIES);
    assert_eq!(expect_item(&last, &oldest_kept).state, ItemState::Recovered);
    assert!(find(&last, "pool_empty:pool-0000").is_none());
}

#[test]
fn entries_older_than_the_age_bound_are_dropped_from_the_store() {
    let memory = AttentionMemory::new();
    read(&memory, with_empty_pool(START));
    read(&memory, at(START + MIN));
    assert_eq!(
        memory.stored_len(),
        1,
        "control: the recovered entry is stored"
    );

    read(&memory, at(START + MIN + FORGET_AFTER_MS + 1));

    assert_eq!(memory.stored_len(), 0);
}

/// A doctor item held through an outage is not evidence forever: once it has
/// gone unobserved past the age bound it is dropped, never shown recovered.
#[test]
fn a_doctor_item_held_past_the_age_bound_is_dropped_not_recovered() {
    let memory = AttentionMemory::new();
    read(&memory, with_doctor_failure(START));
    let unavailable = |now_ms| memory.build(at(now_ms), DoctorCoverage::Unavailable).items;

    let at_bound = unavailable(START + FORGET_AFTER_MS);
    let past_bound = unavailable(START + FORGET_AFTER_MS + 1);
    let next_current = read(&memory, at(START + FORGET_AFTER_MS + MIN));

    assert_eq!(expect_item(&at_bound, DOCTOR_KEY).state, ItemState::Active);
    assert!(find(&past_bound, DOCTOR_KEY).is_none(), "{past_bound:#?}");
    assert!(
        find(&next_current, DOCTOR_KEY).is_none(),
        "{next_current:#?}"
    );
}

#[test]
fn the_fold_counts_recovered_items_apart_from_active_ones() {
    let memory = AttentionMemory::new();
    read(&memory, with_empty_pool(START));

    let attention = memory.build(with_open_route(START + MIN, START), DoctorCoverage::Current);

    assert_eq!(attention.counts.recovered, 1);
    assert_eq!(attention.counts.degraded, 1);
    assert_eq!(attention.counts.broken, 0);
}

/// Two requests reconciling at once must agree on one first-seen time and one
/// rise: a reconcile that let go of the store between its read and its write
/// would let the second request stamp its own time over the first.
#[test]
fn concurrent_reads_agree_on_one_first_seen_and_one_rise() {
    for round in 0..200 {
        let memory = Arc::new(AttentionMemory::new());
        let start = Arc::new(Barrier::new(2));
        let workers: Vec<_> = (0..2_i64)
            .map(|n| {
                let memory = Arc::clone(&memory);
                let start = Arc::clone(&start);
                thread::spawn(move || {
                    let inputs = AttentionInputs {
                        pools: vec![empty_pool(POOL, 2)],
                        ..with_loss(START + n * SEC, 1, 0, 0)
                    };
                    start.wait();
                    memory.build(inputs, DoctorCoverage::Current).items
                })
            })
            .collect();

        let results: Vec<Vec<AttentionItem>> = workers
            .into_iter()
            .map(|worker| worker.join().expect("worker"))
            .collect();

        for key in [POOL_KEY, RECORDS_KEY] {
            let since: Vec<_> = results
                .iter()
                .map(|items| expect_item(items, key).since_ms)
                .collect();
            assert_eq!(since[0], since[1], "round {round}: {key}");
        }
    }
}

/// The memory reads no clock and touches no file: time arrives as a parameter.
/// The scan's tokens are proven live against a status module that does read
/// a clock.
#[test]
fn the_memory_reads_no_clock_and_touches_no_file() {
    let forbidden = [
        "SystemTime",
        "Instant",
        "UNIX_EPOCH",
        "chrono",
        "std::fs",
        "tokio::fs",
        "File::",
        "OpenOptions",
        "std::io",
    ];
    let own = include_str!("attention_memory.rs");
    let clocked = include_str!("usage.rs");

    let found: Vec<_> = forbidden.iter().filter(|t| own.contains(*t)).collect();

    assert!(found.is_empty(), "attention_memory.rs names {found:?}");
    assert!(
        forbidden.iter().any(|t| clocked.contains(*t)),
        "control: the scan finds a clock read in usage.rs"
    );
}

/// Open routes on the `relay` entry, `keys` after a server error.
fn with_relay_routes(now_ms: i64, keys: &[&str]) -> AttentionInputs {
    AttentionInputs {
        routes: keys
            .iter()
            .map(|key| RouteSnapshot {
                state_key: (*key).to_string(),
                provider_entry: "relay".to_string(),
                account: None,
                last_outcome: Some(LastOutcome::Http5xx),
                circuit_open: true,
                open_since_ms: Some(START - MIN),
            })
            .collect(),
        ..at(now_ms)
    }
}

fn active_keys(items: &[AttentionItem]) -> Vec<&str> {
    items
        .iter()
        .filter(|item| item.state == ItemState::Active)
        .map(|item| item.key.as_str())
        .collect()
}

/// A fold that drops to one open route: the next read lists that route's
/// own item active, the fold recovered, and no second active cause; the
/// routes that closed were named by the fold, so their own items never
/// show as recovered.
#[test]
fn a_fold_dropping_to_one_open_route_recovers_the_fold_and_lists_the_route() {
    let memory = AttentionMemory::new();
    let fold_key = "provider_failing:relay/http_5xx";
    let first = read(
        &memory,
        with_relay_routes(START, &["pt-a", "pt-b", "pt-c", "pt-d"]),
    );
    let then = START + READ_EVERY;

    let after = read(&memory, with_relay_routes(then, &["pt-a"]));

    assert_eq!(active_keys(&first), [fold_key], "premise: one fold");
    assert_eq!(active_keys(&after), ["route_circuit_open:pt-a"]);
    let fold = expect_item(&after, fold_key);
    assert_eq!(fold.state, ItemState::Recovered);
    assert_eq!(fold.recovered_at_ms, Some(then));
    for gone in ["pt-b", "pt-c", "pt-d"] {
        let key = format!("route_circuit_open:{gone}");
        assert!(find(&after, &key).is_none(), "{key}: {after:#?}");
    }
}

/// A route folded into a provider item is held, not recovered, while the
/// fold lists it, and its own item returns active with its own start when
/// the fold breaks up around it.
#[test]
fn a_route_listed_alone_then_folded_is_never_stamped_recovered() {
    let memory = AttentionMemory::new();
    let route_key = "route_circuit_open:pt-a";
    let alone = read(&memory, with_relay_routes(START, &["pt-a"]));
    let folded = read(
        &memory,
        with_relay_routes(START + READ_EVERY, &["pt-a", "pt-b"]),
    );

    let after = read(
        &memory,
        with_relay_routes(START + 2 * READ_EVERY, &["pt-a"]),
    );

    assert_eq!(expect_item(&alone, route_key).state, ItemState::Active);
    assert!(find(&folded, route_key).is_none(), "{folded:#?}");
    let item = expect_item(&after, route_key);
    assert_eq!(item.state, ItemState::Active);
    assert_eq!(item.since_ms, Some(START - MIN));
}
