use std::collections::BTreeMap;
use std::path::Path;
use std::time::{Duration, Instant};

use rusqlite::Connection;

use super::{CacheWriteColumns, cache_write_columns};
use crate::record::UsageRecord;
use crate::writer::UsageWriter;

/// A reported write: (name, total, lifetime-seconds -> tokens split).
type ReportedWrite = (&'static str, Option<u64>, Option<BTreeMap<u32, u64>>);

fn split(pairs: &[(u32, u64)]) -> BTreeMap<u32, u64> {
    pairs.iter().copied().collect()
}

fn columns(tokens: Option<u64>, lifetimes: Option<&str>) -> CacheWriteColumns {
    CacheWriteColumns {
        tokens,
        lifetimes: lifetimes.map(str::to_string),
    }
}

#[test]
fn builds_the_total_and_canonical_split_for_every_reported_shape() {
    // Arrange: (row name, reported total, reported split, expected columns).
    let cases: [(ReportedWrite, CacheWriteColumns); 9] = [
        (
            (
                "split with total keeps the total and zero entries",
                Some(2523),
                Some(split(&[(300, 2523), (3600, 0)])),
            ),
            columns(Some(2523), Some(r#"{"300":2523,"3600":0}"#)),
        ),
        (
            ("total only has no split", Some(4096), None),
            columns(Some(4096), None),
        ),
        (
            ("observed zero stays zero, not absent", Some(0), None),
            columns(Some(0), None),
        ),
        (("absent stays absent", None, None), columns(None, None)),
        (
            (
                "split only takes its sum as the total",
                None,
                Some(split(&[(300, 100), (3600, 23)])),
            ),
            columns(Some(123), Some(r#"{"300":100,"3600":23}"#)),
        ),
        (
            (
                "keys render in numeric order, not text order",
                Some(6),
                Some(split(&[(3600, 3), (300, 1), (1800, 2)])),
            ),
            columns(Some(6), Some(r#"{"300":1,"1800":2,"3600":3}"#)),
        ),
        (
            (
                "split above total is stored as reported",
                Some(10),
                Some(split(&[(300, 8), (3600, 7)])),
            ),
            columns(Some(10), Some(r#"{"300":8,"3600":7}"#)),
        ),
        (
            (
                "split below total leaves the remainder unstored",
                Some(50),
                Some(split(&[(300, 20)])),
            ),
            columns(Some(50), Some(r#"{"300":20}"#)),
        ),
        (
            (
                "empty split counts as no split",
                None,
                Some(BTreeMap::new()),
            ),
            columns(None, None),
        ),
    ];

    for ((name, total, lifetimes), expected) in cases {
        // Act
        let built = cache_write_columns(total, lifetimes.as_ref());

        // Assert
        assert_eq!(built, expected, "{name}");
    }
}

#[test]
fn the_split_json_parses_back_to_the_reported_pairs() {
    // Arrange
    let reported = split(&[
        (45, 1),
        (300, 2),
        (1800, 3),
        (3600, 4),
        (86_400, u64::from(u32::MAX)),
    ]);

    // Act
    let built = cache_write_columns(None, Some(&reported));

    // Assert: the text is valid JSON whose members are exactly the pairs.
    let text = built.lifetimes.expect("a split was reported");
    let parsed: BTreeMap<String, u64> = serde_json::from_str(&text).expect("valid JSON object");
    let round_trip: BTreeMap<u32, u64> = parsed
        .into_iter()
        .map(|(k, v)| (k.parse().expect("decimal seconds key"), v))
        .collect();
    assert_eq!(round_trip, reported);
}

#[test]
fn a_split_sum_past_u64_saturates_instead_of_wrapping() {
    // Arrange
    let reported = split(&[(300, u64::MAX), (3600, 5)]);

    // Act
    let built = cache_write_columns(None, Some(&reported));

    // Assert
    assert_eq!(built.tokens, Some(u64::MAX));
}

/// One record per builder shape: (request id, record total, record split).
fn written_shapes() -> Vec<ReportedWrite> {
    vec![
        (
            "split-and-total",
            Some(2523),
            Some(split(&[(300, 2523), (3600, 0)])),
        ),
        ("total-only", Some(4096), None),
        ("observed-zero", Some(0), None),
        ("absent", None, None),
        ("split-only", None, Some(split(&[(300, 100), (3600, 23)]))),
        (
            "numeric-order",
            Some(6),
            Some(split(&[(3600, 3), (300, 1), (1800, 2)])),
        ),
        (
            "split-above-total",
            Some(10),
            Some(split(&[(300, 8), (3600, 7)])),
        ),
    ]
}

/// The persisted `(cache_write_tokens, cache_write_lifetimes,
/// cache_write_5m, cache_write_1h)` of one row.
type PersistedCacheWrites = (Option<i64>, Option<String>, Option<i64>, Option<i64>);

fn persisted(conn: &Connection, request_id: &str) -> PersistedCacheWrites {
    conn.query_row(
        "SELECT cache_write_tokens, cache_write_lifetimes, cache_write_5m, cache_write_1h \
         FROM requests WHERE request_id = ?1",
        [request_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .expect("persisted row")
}

fn write_through_the_writer(path: &Path, records: Vec<UsageRecord>) {
    let want = records.len() as u64;
    let (handle, writer) = UsageWriter::start(path.to_path_buf(), 16, 0, true);
    for record in records {
        handle.try_send(record);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while writer.counters().persisted() < want && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(writer.counters().persisted(), want, "rows persisted");
    drop(handle);
    writer.shutdown();
}

#[test]
fn each_written_row_carries_exactly_the_builder_output_and_no_legacy_write() {
    // Arrange
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    let records = written_shapes()
        .into_iter()
        .map(|(request_id, total, lifetimes)| UsageRecord {
            request_id: request_id.to_string(),
            cache_write_tokens: total,
            cache_write_lifetimes: lifetimes,
            ..UsageRecord::default()
        })
        .collect();

    // Act
    write_through_the_writer(&path, records);

    // Assert: the two columns equal the builder's values for the same input,
    // and the legacy per-lifetime columns stay NULL on every new row.
    let conn = Connection::open(&path).expect("read open");
    for (request_id, total, lifetimes) in written_shapes() {
        let built = cache_write_columns(total, lifetimes.as_ref());
        let expected = (
            built.tokens.map(|t| i64::try_from(t).expect("fits")),
            built.lifetimes,
            None,
            None,
        );
        assert_eq!(persisted(&conn, request_id), expected, "{request_id}");
    }
}
