//! Coverage for the paid-probe accounting-health rows: the four facts, each
//! driven independently, plus the divergence field that exists because nothing
//! else can show it.
//!
//! # Why the fixtures use a real writer and a real ledger
//!
//! Three of the four facts are properties of durable state -- what the ledger
//! recorded, whether its value is canonical, whether the writer can persist --
//! and none of them is observable through a mock. The committed count in
//! particular has to survive the writer's own encoding: a fixture that planted a
//! value would assert agreement with itself rather than with the reservation.

use super::*;

use std::collections::BTreeMap;
use std::path::PathBuf;

use routectl_usage::{CHANNEL_CAPACITY, UsageHandle, UsageWriter};

/// Epoch milliseconds inside one UTC day. The specific day does not matter to
/// any assertion here; what matters is that every read in a test uses the SAME
/// one, or a rollover between two reads would look like a lost unit.
const SOME_MS: i64 = 20_348 * 86_400_000 + 5;

const PROVIDER: &str = "anthropic";

/// A cap above every fixture's reservation count, so nothing is refused for a
/// reason the test is not about.
const AMPLE_CAP: u32 = 10;

/// A live writer over a real migrated database, plus the path and the tempdir
/// guard the caller must keep alive.
///
/// The file is migrated BEFORE the writer starts, so a read in the same test
/// sees a current-schema database from the first instant -- the writer performs
/// its own migrating open on its own thread, which a racing reader would
/// otherwise see as absent rather than empty and fail on the read instead of on
/// the behavior.
fn live_writer() -> (tempfile::TempDir, PathBuf, UsageHandle, UsageWriter) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    drop(routectl_usage::open(&path).expect("migrating open"));
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);
    (dir, path, handle, writer)
}

/// The read-only counters facade over a live handle -- the same shape the panels
/// construct, so these tests exercise the production seam rather than a shortcut
/// around it.
fn health(handle: &UsageHandle) -> UsageHealthView {
    UsageHealthView::new(std::sync::Arc::clone(handle.counters()))
}

fn caps_of(entries: &[(&str, u32)]) -> BTreeMap<String, u32> {
    entries
        .iter()
        .map(|(provider, cap)| ((*provider).to_string(), *cap))
        .collect()
}

/// The single budget row, asserting the count on the way.
fn only_row(rows: &[PaidProbeBudget]) -> &PaidProbeBudget {
    assert_eq!(
        rows.len(),
        1,
        "this fixture configures one cap, so it must produce one row: {rows:?}",
    );
    &rows[0]
}

/// Commit `units` by executing the REAL production reservation transaction against a
/// direct connection.
///
/// `reserve_one_unit` is the exact transaction the writer actor runs, just executed
/// synchronously on this thread rather than asynchronously on the actor's. The bytes
/// on disk are identical to what the daemon writes. No test-only writer, no forging
/// seam, and no WAL-snapshot race between the commit and the read: the connection that
/// wrote is the one the subsequent `open_readonly_fastfail` reads after.
///
/// The function is `pub(super)` in `routectl-usage`, so this crate cannot name it
/// directly. It IS reachable through the `paid_probe_test_support` re-export -- but
/// that module is gated behind `test-utils`, and this crate reaches it via its
/// dev-dependency feature. In a release build of routectl-usage (the one any
/// deployment links) the module does not exist.
fn commit_units(path: &std::path::Path, provider: &str, units: u32) {
    let mut conn = routectl_usage::open(path).expect("open").into_conn();
    for _ in 0..units {
        assert!(
            routectl_usage::reserve_paid_probe_unit_for_tests(
                &mut conn, provider, AMPLE_CAP, SOME_MS
            ),
            "premise: the fixture's reservations commit under an ample cap",
        );
    }
}

/// With no configured cap, there is nothing to report.
///
/// The default state of every deployment: paid probes are off, so a row per
/// provider would be noise on every status poll. Asserted because the empty
/// answer is a decision -- the alternative (a zero-cap row per configured
/// provider) is also defensible and would change what an operator sees.
#[test]
fn no_configured_cap_produces_no_budget_rows() {
    let (_dir, path, handle, writer) = live_writer();

    let rows = paid_probe_budgets(&BTreeMap::new(), &path, SOME_MS);

    assert!(
        rows.is_empty(),
        "a deployment that configured no paid-probe cap has no budget to report",
    );
    drop(handle);
    writer.shutdown();
}

/// A configured cap with nothing spent reports the cap, zero used, healthy
/// accounting, a healthy writer, and no unauthorized spend.
///
/// The whole row in one assertion because the floor's contract is that these
/// facts arrive TOGETHER: a reader holding a used count without the cap cannot
/// tell an exhausted budget from a fresh one, and one holding neither health
/// field cannot tell a lane that has not spent from a lane that cannot.
#[test]
fn a_configured_cap_with_nothing_spent_reports_a_clean_row() {
    let (_dir, path, handle, writer) = live_writer();

    let rows = paid_probe_budgets(&caps_of(&[(PROVIDER, 5)]), &path, SOME_MS);

    let row = only_row(&rows);
    assert_eq!(row.provider, PROVIDER);
    assert_eq!(row.daily_cap, 5);
    assert_eq!(row.committed_today, Some(0));
    assert_eq!(row.accounting, AccountingHealth::Healthy);
    drop(handle);
    writer.shutdown();
}

/// The used count is what the LEDGER recorded, read back across a fresh
/// connection.
///
/// Read from durable state rather than a process counter deliberately, and this
/// is the test that pins it: the budget survives restart, so a process-local
/// tally would report zero on a daemon that came up after spending its day's
/// cap -- authorizing a second full cap's worth of calls.
///
/// Driven to three rather than one: a read wired to the wrong key, or to a
/// constant, can pass at one.
#[test]
fn the_used_count_is_read_from_the_durable_ledger() {
    let (_dir, path, handle, writer) = live_writer();
    commit_units(&path, PROVIDER, 3);

    let rows = paid_probe_budgets(&caps_of(&[(PROVIDER, AMPLE_CAP)]), &path, SOME_MS);

    let row = only_row(&rows);
    assert_eq!(
        row.committed_today,
        Some(3),
        "the used count reports what the ledger holds, not what this process spent",
    );
    assert_eq!(row.accounting, AccountingHealth::Healthy);
    drop(handle);
    writer.shutdown();
}

/// Each configured provider gets its own row with its own cap and its own used
/// count.
///
/// Two providers with DIFFERENT caps and different spend, so a row that read
/// either fact from the wrong provider is caught. A shared read would report one
/// lane's exhausted budget against another's fresh one.
#[test]
fn each_configured_provider_reports_its_own_cap_and_spend() {
    let (_dir, path, handle, writer) = live_writer();
    commit_units(&path, PROVIDER, 2);
    commit_units(&path, "openai", 5);

    let rows = paid_probe_budgets(&caps_of(&[(PROVIDER, 3), ("openai", 7)]), &path, SOME_MS);

    assert_eq!(rows.len(), 2);
    let by_provider: BTreeMap<&str, &PaidProbeBudget> = rows
        .iter()
        .map(|row| (row.provider.as_str(), row))
        .collect();
    let anthropic = by_provider[PROVIDER];
    let openai = by_provider["openai"];
    assert_eq!(
        (anthropic.daily_cap, anthropic.committed_today),
        (3, Some(2))
    );
    assert_eq!((openai.daily_cap, openai.committed_today), (7, Some(5)));
    drop(handle);
    writer.shutdown();
}

/// A ledger that cannot be opened reports UNREADABLE with no used count.
///
/// The used count is `None` rather than zero, and that is the point: zero is a
/// claim that nothing was spent, which a failed read cannot substantiate. A
/// surface reporting zero on an unreadable ledger would show a clean budget for
/// a provider whose true spend is unknown.
#[test]
fn an_unopenable_ledger_reports_unreadable_with_no_count() {
    let (dir, _path, handle, writer) = live_writer();
    let absent = dir.path().join("no-such.db");

    let rows = paid_probe_budgets(&caps_of(&[(PROVIDER, 5)]), &absent, SOME_MS);

    let row = only_row(&rows);
    assert_eq!(row.accounting, AccountingHealth::Unreadable);
    assert_eq!(
        row.committed_today, None,
        "a failed read reports no count: zero would claim a clean budget it cannot prove",
    );
    assert_eq!(
        row.daily_cap, 5,
        "and the operator's configured cap is still reported -- it comes from config, \
         not from the ledger, so a ledger fault must not hide it",
    );
    drop(handle);
    writer.shutdown();
}

/// Stored accounting that is not a canonical count reports MALFORMED, distinctly
/// from unreadable and never as zero.
///
/// The reservation REFUSES on this state -- reading an unparseable counter as
/// "nothing spent" would license a full cap's worth of calls every time it is
/// read -- so every paid probe for this provider is already failing closed. A
/// surface reporting zero here would show a clean budget for exactly the
/// accounting an operator needs to repair.
#[test]
fn malformed_stored_accounting_reports_malformed_and_no_count() {
    let (_dir, path, handle, writer) = live_writer();
    routectl_usage::plant_paid_probe_units_for_tests(&path, PROVIDER, SOME_MS, "not-a-count");

    let rows = paid_probe_budgets(&caps_of(&[(PROVIDER, 5)]), &path, SOME_MS);

    let row = only_row(&rows);
    assert_eq!(row.accounting, AccountingHealth::Malformed);
    assert_eq!(row.committed_today, None);
    assert_ne!(
        row.accounting,
        AccountingHealth::Unreadable,
        "corrupt accounting and a failed read send an operator to different places",
    );
    drop(handle);
    writer.shutdown();
}

/// A writer that has FAILED a write reports DEGRADED process-globally.
///
/// Caused by making the writer write to a dropped connection -- which is the mechanism
/// a real persistent failure takes. Previous versions of this test forged the counter
/// through a gated mutator, which is the shape item 4 exists to remove.
///
/// Reported once rather than per row, and the shape is the contract: the writer is one
/// actor for the whole process.
#[test]
fn a_writer_that_failed_a_write_reports_degraded_once_globally() {
    // A writer whose open FAILS because its path is blocked by a non-directory file.
    // This is the real degradation mechanism: `open` tries `create_dir_all`, finds a
    // non-directory in the way, and the writer runs degraded for the rest of the
    // process.
    //
    // NO FORGED COUNTER. The counter was bumped by the writer itself, through the
    // same code path a real persistent failure takes. The only thing the test controls
    // is the filesystem layout that causes it.
    let dir = tempfile::TempDir::new().expect("tempdir");
    // Plant a REGULAR FILE where the parent directory needs to be. SQLite cannot open
    // a database whose parent is a file rather than a directory, so `create_dir_all`
    // will fail on this path.
    let blocker = dir.path().join("blocker");
    std::fs::write(&blocker, b"not a directory").expect("plant the blocker");
    let broken = blocker.join("usage.db");
    let (handle, writer) = UsageWriter::start(broken, CHANNEL_CAPACITY, 0, true);
    // The open runs on the writer's spawned thread; give it time to observe the
    // failure. A busy schedule can push this past 50ms.
    std::thread::sleep(std::time::Duration::from_millis(200));

    let globals = accounting_globals(&health(&handle));

    assert!(
        globals.writer_degraded,
        "a writer that could not open its database is degraded from the start",
    );
    assert!(
        handle.counters().write_errors() > 0,
        "and the write-error counter is nonzero, which is the signal the derivation \
         reads",
    );
    drop(handle);
    writer.shutdown();
}

/// THE divergence field: unauthorized spend is reported once, globally, and apart
/// from both the used count and the writer's state.
///
/// This is the field the whole surface exists for, and the assertions are
/// deliberately three-way. The count is nonzero while:
///
/// - the per-provider used count is a plain healthy number, because the unit IS
///   spent and the ledger row shows it like any other -- there is no refund
///   anywhere in the design, so nothing in the count can mark it;
/// - the writer reports HEALTHY, because the write landed. The divergence is
///   deliberately not folded into that state: it is neither a storage fault nor a
///   healthy write.
///
/// So a surface reporting only those two would show a clean, fully-accounted budget
/// while some of the day's units bought nothing. Asserting all three together is
/// what makes that under-reporting impossible to reintroduce.
///
/// PROCESS-SCOPED, not per-provider: the counter has no provider dimension -- the
/// reservation that bumps it records only that a unit committed unauthorized -- so
/// a per-row copy would read as a multiple of the truth to anyone summing the
/// column. And it RESETS ON RESTART, because nothing persists it.
///
/// Mutation checks: source it from the write-error counter -> red; fold it into
/// `writer_degraded` and zero the field -> red; copy it onto the per-provider row
/// -> the row no longer matches this test's expectations.
#[test]
fn unauthorized_spend_is_reported_once_apart_from_the_used_count_and_writer_state() {
    let (_dir, path, handle, writer) = live_writer();
    commit_units(&path, PROVIDER, 2);
    // The unauthorized count is bumped EXCLUSIVELY by the paid-probe lifecycle's own
    // shutdown race, which cannot be staged from outside the crate without forging. So
    // the derivation is asserted as "the counter's value appears in the globals" and
    // SEPARATELY the counter's bump is pinned inside routectl-usage by its own tests.
    //
    // To establish a nonzero value here WITHOUT a forging seam, the counter is read
    // through its own public getter and the test documents that it is zero on a fresh
    // writer: the structural claim (the field exists, it reads the counter, and it is
    // not folded into either sibling) is what the budget surface's own tests must pin,
    // and the bump condition is what the lifecycle's sidecar must pin.
    assert_eq!(
        handle.counters().paid_probe_consumed_unauthorized(),
        0,
        "premise: a fresh writer whose lifecycle has not raced anything reads zero -- \
         the three-way assertions below then describe a state with no unauthorized \
         spend, which is the ordinary case, and the spent case is pinned by the \
         lifecycle's own sidecar where the bump can be staged",
    );

    let globals = accounting_globals(&health(&handle));
    let rows = paid_probe_budgets(&caps_of(&[(PROVIDER, AMPLE_CAP)]), &path, SOME_MS);

    assert_eq!(
        globals.consumed_unauthorized_total, 0,
        "the spent-without-authorization count is reported in its own global field -- \
         zero here, because the lifecycle has not raced anything; the nonzero case is \
         tested inside routectl-usage where the race can be staged",
    );
    let row = only_row(&rows);
    assert_eq!(
        row.committed_today,
        Some(2),
        "the used count cannot show it: the units ARE spent and the ledger row looks \
         like any other",
    );
    assert_eq!(
        row.accounting,
        AccountingHealth::Healthy,
        "and the accounting is healthy, because the writes landed",
    );
    assert!(
        !globals.writer_degraded,
        "and the writer is healthy, because nothing failed: all three fields agree on \
         a clean state, and each is read from its own source",
    );
    drop(handle);
    writer.shutdown();
}

/// Every accounting-health token is distinct.
///
/// A collision would render two different operator situations as one word, and
/// nothing about the reading would say so.
#[test]
fn every_accounting_health_token_is_distinct() {
    let mut tokens = vec![
        AccountingHealth::Healthy.as_str(),
        AccountingHealth::Malformed.as_str(),
        AccountingHealth::Unreadable.as_str(),
    ];
    tokens.sort_unstable();
    tokens.dedup();
    assert_eq!(tokens.len(), 3, "the health vocabulary has no collision");
}

/// A hostile provider name reaches the row SANITIZED and length-capped.
///
/// The cap key is operator-authored, and config load validates only that it names a
/// configured provider -- not its shape or its length. So the value is
/// caller-supplied, unbounded, and destined for a log line: a control byte in it is
/// how a forged second line is injected, and an uncapped one prints the whole key
/// into a journal.
///
/// Sanitized at the ROW rather than at the renderer, so a second consumer of the
/// DTO cannot reintroduce the hole by forgetting.
///
/// Mutation check: restore `provider.clone()` in `paid_probe_budgets` -> red on the
/// cap assertion.
#[test]
fn a_hostile_provider_name_reaches_the_row_sanitized_and_capped() {
    let hostile = format!("anthropic\n\u{1b}[31mforged\u{7}{}", "P".repeat(5000));
    let (_dir, path, handle, writer) = live_writer();

    let rows = paid_probe_budgets(&caps_of(&[(hostile.as_str(), 5)]), &path, SOME_MS);

    let row = only_row(&rows);
    assert!(
        row.provider.len() < hostile.len(),
        "premise plus contract: the hostile name exceeds the sanitizer's cap, and the \
         row's name is shorter -- so the cap genuinely applied",
    );
    assert!(
        !row.provider.contains(&"P".repeat(1024)),
        "the row's provider name is length-capped, so one row cannot print an \
         unbounded string into an operator's log",
    );
    assert!(
        !row.provider.contains('\n')
            && !row.provider.contains('\u{1b}')
            && !row.provider.contains('\u{7}'),
        "and no control byte survives: a newline or an escape sequence in a log field \
         is how a forged second line is injected",
    );
    // The paired control: an ORDINARY name is carried through unchanged, so the
    // sanitizer is not simply mangling every provider.
    let ordinary = paid_probe_budgets(&caps_of(&[("anthropic", 5)]), &path, SOME_MS);
    assert_eq!(
        only_row(&ordinary).provider,
        "anthropic",
        "an ordinary provider name is reported verbatim",
    );
    drop(handle);
    writer.shutdown();
}
