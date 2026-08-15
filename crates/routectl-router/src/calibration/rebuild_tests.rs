//! Tests for the ledger-driven lane rebuild: replay order, the shared
//! admission rule, the unknown-nickname drop, current-clock freshness, and the
//! failed-read posture.

use super::*;

use std::time::Duration;

use crate::calibration::factor::{Factor, IDENTITY_PERMILLE};
use crate::calibration::store::SAMPLES_PER_LANE;
/// A fixed "now" far enough past the epoch that a stale offset can be
/// subtracted without underflowing.
fn now() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(10_000_000)
}

/// Rows the fake reader hands back, plus the window/limit it was asked for so
/// a test can assert what the rebuild requested.
struct FakeReader {
    rows: Vec<CalibrationLedgerRow>,
}

impl FakeReader {
    fn new(rows: Vec<CalibrationLedgerRow>) -> Self {
        Self { rows }
    }
}

impl CalibrationLedgerReader for FakeReader {
    fn read_calibration_samples(
        &self,
        window_start: SystemTime,
        _limit: usize,
    ) -> Vec<CalibrationLedgerRow> {
        self.rows
            .iter()
            .filter(|row| row.ts >= window_start)
            .cloned()
            .collect()
    }
}

/// A reader whose read failed: it yields nothing at all, which is the
/// posture a partial read must never take.
struct FailedReader;

impl CalibrationLedgerReader for FailedReader {
    fn read_calibration_samples(&self, _: SystemTime, _: usize) -> Vec<CalibrationLedgerRow> {
        Vec::new()
    }
}

fn row(
    nickname: &str,
    session: Option<&str>,
    estimated: u64,
    prompt: u64,
    ts: SystemTime,
) -> CalibrationLedgerRow {
    CalibrationLedgerRow::new(
        ts,
        "anthropic-api".to_string(),
        nickname.to_string(),
        session.map(str::to_string),
        estimated,
        prompt,
    )
}

/// Enough balanced evidence for one lane to clear the reduction's sample and
/// cohort floors, all at `ts`, all carrying the same ratio.
fn balanced_rows(nickname: &str, permille: u64, ts: SystemTime) -> Vec<CalibrationLedgerRow> {
    (0..9)
        .map(|i| {
            row(
                nickname,
                Some(&format!("caller-{}", i % 3)),
                10_000,
                10 * permille,
                ts,
            )
        })
        .collect()
}

fn accept_all(_: &str) -> bool {
    true
}

fn lane(nickname: &str) -> LaneKey {
    LaneKey {
        provider_kind: "anthropic-api".to_string(),
        nickname: nickname.to_string(),
    }
}

#[test]
fn replayed_evidence_reproduces_the_factor_live_traffic_had_learned() {
    // Arrange: the evidence a lane accumulated before a restart.
    let store = CalibrationStore::default();
    let reader = FakeReader::new(balanced_rows("opus", 1_200, now()));

    // Act
    let summary = rebuild_into(&reader, &store, &accept_all, now(), 100);

    // Assert: the lane comes back with the factor its evidence implies, not
    // merely with some factor.
    assert_eq!(summary.rows_loaded, 9);
    assert_eq!(summary.accepted, 9);
    assert_eq!(summary.lanes_calibrated, 1);
    assert_eq!(
        store.factor_for(&lane("opus"), now()).map(Factor::permille),
        Some(1_200)
    );
}

#[test]
fn rows_replay_oldest_first_so_the_ring_retains_what_arrival_order_would_have() {
    // Arrange: more rows than the lane's ring holds, handed to the reader
    // NEWEST-first so a rebuild that skipped the ordering step would retain
    // the wrong end of the history. Each row carries a distinct ratio, so
    // which samples survived is observable.
    let store = CalibrationStore::default();
    let total = SAMPLES_PER_LANE + 5;
    let newest_first: Vec<CalibrationLedgerRow> = (0..total)
        .rev()
        .map(|i| {
            row(
                "opus",
                Some("caller"),
                1_000,
                1_000 + i as u64,
                now() - Duration::from_secs(total as u64 - i as u64),
            )
        })
        .collect();
    let reader = FakeReader::new(newest_first);

    // Act
    rebuild_into(&reader, &store, &accept_all, now(), 1_000);

    // Assert: the ring dropped the five OLDEST samples, exactly as it would
    // have under live arrival order.
    let held: Vec<u32> = store
        .export_entries()
        .into_iter()
        .find(|(k, _)| *k == lane("opus"))
        .map(|(_, samples)| samples.iter().map(|s| s.permille).collect())
        .expect("the lane exists");
    assert_eq!(held.len(), SAMPLES_PER_LANE);
    assert_eq!(
        held.first(),
        Some(&1_005),
        "the five oldest samples were dropped, not the five newest"
    );
}

#[test]
fn a_row_whose_nickname_left_the_resolved_table_is_dropped() {
    // A history of renamed models must not grow the lane map with lanes that
    // can never serve a request.
    let store = CalibrationStore::default();
    let mut rows = balanced_rows("opus", 1_200, now());
    rows.extend(balanced_rows("retired-nickname", 1_300, now()));
    let reader = FakeReader::new(rows);

    let summary = rebuild_into(&reader, &store, &|nickname| nickname == "opus", now(), 100);

    assert_eq!(summary.accepted, 9);
    assert_eq!(summary.rejected_unknown_nickname, 9);
    assert_eq!(store.len(), 1, "only the still-resolvable lane exists");
    assert_eq!(store.factor_for(&lane("retired-nickname"), now()), None);
}

#[test]
fn a_degenerate_pair_is_refused_by_the_same_rule_the_live_write_applies() {
    // The rebuild owns no admission rule of its own: it stores through the
    // same call the live path uses, so a pair the live write refuses is
    // refused here with no second decision to drift.
    let store = CalibrationStore::default();
    let reader = FakeReader::new(vec![
        row("opus", Some("caller"), 0, 10_000, now()),
        row("opus", Some("caller"), 10_000, 0, now()),
    ]);

    let summary = rebuild_into(&reader, &store, &accept_all, now(), 100);

    assert_eq!(summary.rows_loaded, 2);
    assert_eq!(summary.accepted, 0);
    assert_eq!(summary.rejected_pair, 2);
    assert!(
        store.is_empty(),
        "neither degenerate pair may create a lane"
    );
}

#[test]
fn evidence_older_than_the_age_bound_comes_back_uncorrected() {
    // Freshness is judged against the CURRENT clock: a lane whose newest
    // evidence predates the bound must read as not-yet-calibrated, not as
    // calibrated on history.
    let store = CalibrationStore::default();
    let stale_ts = now() - MAX_SAMPLE_AGE - Duration::from_mins(1);
    let reader = FakeReader::new(balanced_rows("opus", 1_200, stale_ts));

    let summary = rebuild_into(&reader, &store, &accept_all, now(), 100);

    assert_eq!(
        summary.rows_loaded, 0,
        "the read window itself excludes evidence the reducer could not use"
    );
    assert_eq!(summary.lanes_calibrated, 0);
    assert_eq!(store.factor_for(&lane("opus"), now()), None);
}

#[test]
fn a_lane_below_the_cohort_floor_is_not_reported_as_calibrated() {
    // The summary's calibrated count runs the SAME reduction the gate's
    // lookup runs, so it cannot report a lane the gate would refuse. Nine
    // samples from ONE caller clear the sample floor and miss the cohort one.
    let store = CalibrationStore::default();
    let rows: Vec<CalibrationLedgerRow> = (0..9)
        .map(|_| row("opus", Some("one-caller"), 10_000, 12_000, now()))
        .collect();
    let reader = FakeReader::new(rows);

    let summary = rebuild_into(&reader, &store, &accept_all, now(), 100);

    assert_eq!(summary.accepted, 9);
    assert_eq!(summary.lanes_calibrated, 0);
    assert_eq!(store.factor_for(&lane("opus"), now()), None);
}

#[test]
fn a_failed_read_leaves_every_lane_uncorrected() {
    // A read that fails yields NOTHING rather than what it managed to read:
    // a factor reduced from a partial slice is a factor the full evidence
    // never supported.
    let store = CalibrationStore::default();

    let summary = rebuild_into(&FailedReader, &store, &accept_all, now(), 100);

    assert_eq!(summary, CalibrationRebuildSummary::default());
    assert!(store.is_empty());
}

#[test]
fn a_rebuilt_lane_corrects_the_estimate_in_the_direction_its_evidence_implies() {
    // End-to-end direction check on the rebuilt state: an under-counting
    // estimator must yield a LARGER corrected estimate after a restart, the
    // same way it did before one.
    let store = CalibrationStore::default();
    let reader = FakeReader::new(balanced_rows("opus", 1_500, now()));

    rebuild_into(&reader, &store, &accept_all, now(), 100);

    let factor = store
        .factor_for(&lane("opus"), now())
        .expect("the replayed evidence clears the floors");
    assert!(factor.apply(10_000) > 10_000);
    assert_eq!(factor.apply(10_000), 15_000);
    assert!(IDENTITY_PERMILLE < factor.permille());
}
