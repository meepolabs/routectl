//! Pins the per-seat latest-quota read to the window-function statement it
//! replaced: same rows, same order, with and without the partial quota index.

use rusqlite::types::Value;
use tempfile::TempDir;

use super::{LATEST_QUOTA_BY_SEAT_SQL, latest_quota_by_seat};
use crate::db::{UsageDb, open};

/// The window-function read the seek-per-seat statement replaced, kept as
/// the oracle for its results.
const WINDOW_ORACLE_SQL: &str = "\
SELECT seat, provider_kind, ts_start, quota_claim, quota_status,
       quota_overage_status, quota_utilization, quota_overage_utilization,
       quota_reset
FROM (
    SELECT seat, provider_kind, ts_start, quota_claim, quota_status,
           quota_overage_status, quota_utilization, quota_overage_utilization,
           quota_reset,
           ROW_NUMBER() OVER (
               PARTITION BY seat ORDER BY ts_start DESC, rowid DESC) AS rn
    FROM requests
    WHERE quota_status IS NOT NULL OR quota_utilization IS NOT NULL
)
WHERE rn = 1
ORDER BY seat";

/// One ledger row: `(seat, ts_start, quota_status, quota_utilization)`, the
/// other quota columns derived from the row's position so every returned
/// column distinguishes rows.
type Seed = (Option<&'static str>, i64, Option<&'static str>, Option<f64>);

/// Several seats, a NULL-seat bucket, `ts_start` ties at each bucket's
/// newest instant (later insert wins), rows carrying only one of the two
/// eligibility columns, newer rows with no quota data at all, and a seat
/// that never reported quota.
const FIXTURE: &[Seed] = &[
    (Some("seat-b"), 100, Some("active"), Some(0.10)),
    (Some("seat-a"), 120, Some("active"), None),
    (None, 130, None, Some(0.20)),
    (Some("codex"), 140, None, Some(0.30)),
    (Some("seat-a"), 300, Some("tie-first"), Some(0.40)),
    (Some("seat-a"), 300, Some("tie-second"), Some(0.41)),
    (Some("seat-a"), 300, Some("tie-third"), Some(0.42)),
    (None, 310, Some("tie-first"), None),
    (None, 310, Some("tie-second"), None),
    (Some("seat-b"), 320, None, Some(0.50)),
    (Some("seat-b"), 320, Some("tie-second"), None),
    (Some("codex"), 150, None, Some(0.31)),
    (Some("seat-a"), 900, None, None),
    (None, 900, None, None),
    (Some("seat-b"), 900, None, None),
    (Some("never-quota"), 950, None, None),
];

fn seeded_db() -> (TempDir, UsageDb) {
    let dir = TempDir::new().expect("tempdir");
    let db = open(dir.path().join("usage.db")).expect("open");
    for (i, (seat, ts_start, status, utilization)) in FIXTURE.iter().enumerate() {
        let n = i64::try_from(i).expect("small fixture");
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, stream, outcome, latency_ms, tool_count, \
                 msg_count, attempt_count, fallback_count, seat, provider_kind, \
                 quota_claim, quota_status, quota_overage_status, quota_utilization, \
                 quota_overage_utilization, quota_reset) \
                 VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 0, 'ok', 0, 0, 0, 1, 0, \
                 ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    ts_start,
                    format!("q{i}"),
                    seat,
                    format!("kind-{i}"),
                    format!("claim-{i}"),
                    status,
                    format!("overage-{i}"),
                    utilization,
                    0.01 * f64::from(u8::try_from(i).expect("small fixture")),
                    10_000 + n,
                ],
            )
            .expect("seed quota row");
    }
    (dir, db)
}

fn rows(db: &UsageDb, sql: &str) -> Vec<Vec<Value>> {
    let mut stmt = db.conn().prepare(sql).expect("prepare");
    let width = stmt.column_count();
    stmt.query_map([], |row| (0..width).map(|i| row.get(i)).collect())
        .expect("query")
        .collect::<Result<_, _>>()
        .expect("collect")
}

/// The quota index as shipped, and the same index with `ts_start DESC`: a
/// forward walk of the second meets tied rows in ascending rowid order, so
/// only the statement's own tie-break picks the right one.
const QUOTA_INDEX_SHAPES: [(&str, &str); 2] = [
    ("shipped quota index", ""),
    (
        "descending ts_start index",
        "DROP INDEX idx_requests_quota_seat;
         CREATE INDEX quota_seat_desc ON requests (seat, ts_start DESC)
         WHERE quota_status IS NOT NULL OR quota_utilization IS NOT NULL;",
    ),
];

#[test]
fn the_seek_per_seat_read_returns_the_window_function_rows_in_its_order() {
    for (shape, ddl) in QUOTA_INDEX_SHAPES {
        // Arrange
        let (_dir, db) = seeded_db();
        db.conn().execute_batch(ddl).expect("index shape");
        let expected = rows(&db, WINDOW_ORACLE_SQL);

        // Act
        let actual = rows(&db, LATEST_QUOTA_BY_SEAT_SQL);

        // Assert
        assert_eq!(actual, expected, "{shape}");
    }
}

#[test]
fn the_oracle_fixture_resolves_every_bucket_to_its_last_tied_quota_row() {
    // Arrange
    let (_dir, db) = seeded_db();

    // Act
    let snapshots = latest_quota_by_seat(&db).expect("read");

    // Assert: the fixture exercises what the oracle comparison relies on.
    let picked: Vec<(Option<&str>, i64, Option<&str>)> = snapshots
        .iter()
        .map(|s| (s.seat.as_deref(), s.ts_start, s.status.as_deref()))
        .collect();
    assert_eq!(
        picked,
        [
            (None, 310, Some("tie-second")),
            (Some("codex"), 150, None),
            (Some("seat-a"), 300, Some("tie-third")),
            (Some("seat-b"), 320, Some("tie-second")),
        ]
    );
}
