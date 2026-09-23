//! Tests for the paid-probe budget READ: one provider-day's committed count as
//! the accounting-health surface reads it.
//!
//! Every case drives a real file-backed database and reserves through the real
//! reservation path wherever a committed count is under test, rather than
//! planting a value: what the read has to agree with is the WRITER, and a fixture
//! that planted its own value would be asserting agreement with itself. The key
//! and value encodings are private to the writer's module, so a planted fixture
//! is also a second copy of an encoding that can drift.

use super::*;

use crate::paid_probe::{MS_PER_UTC_DAY, reserve_one_unit};

/// Epoch milliseconds inside UTC day 20348, where the specific day does not
/// matter.
const SOME_MS: i64 = 20_348 * MS_PER_UTC_DAY + 5;

/// A generous cap, so a fixture reserving several units is never refused for a
/// reason the test is not about.
const AMPLE_CAP: u32 = 10;

const PROVIDER: &str = "anthropic";

/// A migrated database in a fresh directory, with its owned connection.
fn read_db() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    let conn = crate::db::open(&path).expect("open").into_conn();
    (dir, conn)
}

/// Plant a RAW value for one provider-day, bypassing the reservation.
///
/// Used only by the malformed-state cases, where the point is a value the writer
/// would never have produced -- so there is nothing the writer could be driven to
/// do that would create it.
fn plant_raw(conn: &Connection, provider: &str, now_epoch_ms: i64, raw: &str) {
    let key = crate::paid_probe::reservation_key(
        crate::paid_probe::utc_day_from_epoch_ms(now_epoch_ms),
        provider,
    );
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = ?2",
        rusqlite::params![&key, raw],
    )
    .expect("plant raw value");
}

/// A provider-day nothing has been reserved against reads as zero committed.
///
/// Zero rather than an absence, because nothing spent IS a reading: an operator
/// reconciling a cap needs "0 of 5", and a surface that reported the day as
/// unavailable would be indistinguishable from a broken ledger.
#[test]
fn a_day_with_no_reservation_reads_as_zero_committed() {
    let (_dir, conn) = read_db();

    let usage = committed_units(&conn, PROVIDER, SOME_MS);

    assert_eq!(usage, PaidProbeUsage::Committed(0));
}

/// The read reports exactly what the reservation committed.
///
/// Driven through the real reservation three times, and asserted at three rather
/// than at one: a read wired to the wrong key, or to a hardcoded value, can pass
/// at one. The count is what an operator reconciles against the cap, so an
/// off-by-one here is a budget they cannot trust.
#[test]
fn the_read_reports_what_the_reservation_committed() {
    let (_dir, mut conn) = read_db();
    for _ in 0..3 {
        assert!(
            matches!(
                reserve_one_unit(&mut conn, PROVIDER, AMPLE_CAP, SOME_MS),
                crate::paid_probe::StoredReservation::Committed { .. }
            ),
            "premise: the fixture's reservations commit under an ample cap",
        );
    }

    let usage = committed_units(&conn, PROVIDER, SOME_MS);

    assert_eq!(usage, PaidProbeUsage::Committed(3));
}

/// A reservation for one provider is invisible to another provider's read.
///
/// The key's provider field is what separates the two budgets, so a read that
/// ignored it would report one provider's spend against every provider's cap --
/// showing an exhausted budget for a lane that has spent nothing.
#[test]
fn one_providers_reservation_does_not_appear_in_anothers_read() {
    let (_dir, mut conn) = read_db();
    let _ = reserve_one_unit(&mut conn, PROVIDER, AMPLE_CAP, SOME_MS);

    assert_eq!(
        committed_units(&conn, PROVIDER, SOME_MS),
        PaidProbeUsage::Committed(1),
        "premise: the reservation landed for its own provider",
    );
    assert_eq!(
        committed_units(&conn, "openai", SOME_MS),
        PaidProbeUsage::Committed(0),
        "a sibling provider's budget is untouched",
    );
}

/// A reservation on one UTC day is invisible to the next day's read.
///
/// The cap is per provider per UTC DAY, so a read that ignored the day would
/// report yesterday's exhausted budget as today's -- suppressing every paid probe
/// on a lane whose budget had actually rolled over.
#[test]
fn a_previous_days_reservation_does_not_appear_in_todays_read() {
    let (_dir, mut conn) = read_db();
    let _ = reserve_one_unit(&mut conn, PROVIDER, AMPLE_CAP, SOME_MS);

    assert_eq!(
        committed_units(&conn, PROVIDER, SOME_MS),
        PaidProbeUsage::Committed(1),
        "premise: the reservation landed on its own day",
    );
    assert_eq!(
        committed_units(&conn, PROVIDER, SOME_MS + MS_PER_UTC_DAY),
        PaidProbeUsage::Committed(0),
        "the next UTC day starts with a fresh budget",
    );
}

/// A stored value the writer would never have produced reads as MALFORMED, never
/// as zero.
///
/// This is the case the whole three-valued answer exists for. The reservation
/// REFUSES on unparseable state -- reading it as "nothing spent" would license a
/// full cap's worth of calls every time -- so a health surface reporting zero
/// here would show a clean budget for a provider whose paid probes are all
/// failing closed, which is the exact opposite of the truth.
///
/// Every form the codec rejects is driven, not just one: they fail for different
/// reasons inside the parser, and a reader that special-cased one would pass a
/// single-case test while reading another as zero.
#[test]
fn a_value_the_codec_would_not_have_written_reads_as_malformed() {
    for raw in ["", "01", "-1", " 1", "1.0", "x", "4294967296"] {
        let (_dir, conn) = read_db();
        plant_raw(&conn, PROVIDER, SOME_MS, raw);

        let usage = committed_units(&conn, PROVIDER, SOME_MS);

        assert_eq!(
            usage,
            PaidProbeUsage::Malformed,
            "a stored value of {raw:?} is unreadable accounting, never a zero spend",
        );
    }
}

/// The canonical control for the case above: a value the codec DID write reads
/// back as its count.
///
/// Without it, a reader that answered `Malformed` unconditionally would satisfy
/// every assertion in the malformed test -- and would report every provider's
/// accounting as broken.
#[test]
fn a_value_the_codec_did_write_reads_back_as_its_count() {
    let (_dir, mut conn) = read_db();
    let _ = reserve_one_unit(&mut conn, PROVIDER, AMPLE_CAP, SOME_MS);

    assert_eq!(
        committed_units(&conn, PROVIDER, SOME_MS),
        PaidProbeUsage::Committed(1),
        "the reader accepts exactly what the writer produces",
    );
}

/// A read whose query cannot run reports UNREADABLE, distinctly from malformed.
///
/// The two send an operator to different places: malformed state is corrupt
/// accounting to repair, while an unreadable query is a database access problem
/// whose stored state may be perfectly fine. Driven by dropping the table the
/// read names, which is the one way to break the query without touching a value.
#[test]
fn a_read_whose_query_cannot_run_reports_unreadable() {
    let (_dir, conn) = read_db();
    conn.execute("DROP TABLE meta", [])
        .expect("the fixture drops the table the read names");

    let usage = committed_units(&conn, PROVIDER, SOME_MS);

    assert_eq!(
        usage,
        PaidProbeUsage::Unreadable,
        "a failed query is not evidence about the stored value",
    );
}
