// Storage-outcome mapping: every state the control storage can present must
// reach the caller as its own answer, and only one of them authorizes a paid
// call. Driven through a real writer wherever a state can be planted on disk,
// and through the mapping directly where it cannot.

/// Plant a raw value under one provider-day's reservation key, bypassing the
/// reservation path, so a test can drive the state it wants read back.
fn plant_units(path: &Path, utc_day: i64, provider: &str, raw: &str) {
    let db = crate::db::open(path).expect("open to plant");
    db.conn()
        .execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) \
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            rusqlite::params![&reservation_key(utc_day, provider), raw],
        )
        .expect("plant units");
}

/// A reservation command as the handle would build one, for the cases that
/// drive the writer-side body against a connection they control.
///
/// Its ack sender goes nowhere: `reserve` RETURNS the outcome and the writer
/// loop is what answers, so these cases read the return value directly.
fn command_for(provider: &str, cap: u32) -> PaidProbeCommand {
    crate::writer::paid_probe_command_for_tests(provider, cap).0
}

/// Epoch milliseconds inside `utc_day`, for the cases that pin a day rather
/// than sampling the wall clock.
const fn day_ms(utc_day: i64) -> i64 {
    utc_day * 86_400_000 + 5
}

/// A cap of zero blocks every reservation and writes nothing -- the operator's
/// way of turning a provider's paid probes off entirely.
#[tokio::test]
async fn a_zero_cap_refuses_and_creates_no_row() {
    // Arrange
    let (_dir, path) = temp_path();
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);

    // Act
    let outcome = reserve_through(&handle, "anthropic", 0).await;

    // Assert
    assert_eq!(outcome, PaidProbeCommit::CapExhausted);
    assert!(
        reservation_keys(&path).is_empty(),
        "a refused reservation must leave no accounting row",
    );

    // Control: the same handle commits under a nonzero cap, so the refusal is
    // the cap and not a broken fixture.
    assert_eq!(
        reserve_through(&handle, "anthropic", 1).await,
        PaidProbeCommit::Committed { used: 1, cap: 1 },
    );

    stop(handle, writer);
}

/// Accounting state the codec would never have written is refused rather than
/// read as zero -- reading corrupt state as "nothing spent" would license a
/// full cap's worth of paid calls every time it is read -- and the bytes are
/// left identical.
#[tokio::test]
async fn malformed_stored_state_refuses_and_leaves_the_bytes_identical() {
    for raw in ["", "-1", " 1", "01", "1.0", "1x", "99999999999999999999"] {
        // Arrange
        let (_dir, path) = temp_path();
        let day = utc_day_from_epoch_ms(wall_clock_ms());
        plant_units(&path, day, "anthropic", raw);
        let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);

        // Act
        let outcome = reserve_through(&handle, "anthropic", 3).await;

        // Assert
        assert_eq!(
            outcome,
            PaidProbeCommit::MalformedState,
            "stored state {raw:?} must be refused, never read as zero",
        );
        assert_eq!(
            stored_units(&path, day, "anthropic").as_deref(),
            Some(raw),
            "a refusal must leave stored state {raw:?} byte-identical",
        );

        stop(handle, writer);
    }
}

/// A control for the malformed case above: the SAME fixture shape carrying a
/// canonical count commits, so the refusals are the stored bytes and not the
/// planting helper.
#[tokio::test]
async fn a_canonical_planted_count_commits_on_top_of_itself() {
    // Arrange
    let (_dir, path) = temp_path();
    let day = utc_day_from_epoch_ms(wall_clock_ms());
    plant_units(&path, day, "anthropic", "2");
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);

    // Act
    let outcome = reserve_through(&handle, "anthropic", 4).await;

    // Assert
    assert_eq!(outcome, PaidProbeCommit::Committed { used: 3, cap: 4 });

    stop(handle, writer);
}

/// A database missing its control table cannot record a unit, so the
/// reservation reports a failed write and the writer degrades rather than
/// answering with a count it could not store.
#[tokio::test]
async fn a_database_without_the_control_table_reports_a_failed_write() {
    // Arrange: a real SQLite file the writer will attach to READ-WRITE without
    // migrating, carrying the requests table the open requires and a schema
    // version this build accepts, but no control table.
    let (_dir, path) = temp_path();
    {
        let db = crate::db::open(&path).expect("migrating open");
        db.conn()
            .execute_batch("DROP TABLE meta")
            .expect("drop the control table");
    }
    let mut conn = rusqlite::Connection::open(&path).expect("reopen");

    // Act: the mapping is driven directly, because the writer's own open would
    // recreate the table it needs.
    let command = command_for("anthropic", 3);
    let outcome = reserve(Some(&mut conn), &command, wall_clock_ms());

    // Assert
    assert_eq!(outcome, PaidProbeCommit::WriteFailed);

    // Control: with the table back the same connection commits, so the refusal
    // is the missing table and nothing else.
    conn.execute_batch(crate::schema::CREATE_META_TABLE)
        .expect("recreate the control table");
    assert!(matches!(
        reserve(
            Some(&mut conn),
            &command_for("anthropic", 3),
            wall_clock_ms()
        ),
        PaidProbeCommit::Committed { used: 1, cap: 3 },
    ));
}

/// A read-only connection cannot commit, so the reservation refuses and writes
/// nothing.
#[test]
fn a_read_only_connection_reports_a_failed_write_and_commits_nothing() {
    // Arrange
    let (_dir, path) = temp_path();
    drop(crate::db::open(&path).expect("migrating open"));
    let day = utc_day_from_epoch_ms(wall_clock_ms());
    let mut readonly = crate::db::open_readonly(&path)
        .expect("read-only open")
        .into_conn();

    // Act
    let outcome = reserve(
        Some(&mut readonly),
        &command_for("anthropic", 3),
        day_ms(day),
    );

    // Assert
    assert_eq!(outcome, PaidProbeCommit::WriteFailed);
    assert_eq!(
        stored_units(&path, day, "anthropic"),
        None,
        "a failed write must leave no reservation behind",
    );
}

/// A connection locked out by another writer refuses rather than claiming a
/// unit it could not store.
#[test]
fn a_connection_locked_out_by_another_writer_reports_a_failed_write() {
    // Arrange: a sibling holds the write lock, and ours sheds immediately
    // rather than waiting for it.
    let (_dir, path) = temp_path();
    let mut conn = crate::db::open(&path).expect("migrating open").into_conn();
    conn.busy_timeout(Duration::from_millis(0))
        .expect("drop the busy timeout");
    let day = utc_day_from_epoch_ms(wall_clock_ms());
    let blocker = crate::db::open(&path).expect("sibling open").into_conn();
    blocker
        .execute_batch("BEGIN EXCLUSIVE")
        .expect("take the write lock");

    // Act
    let outcome = reserve(Some(&mut conn), &command_for("anthropic", 3), day_ms(day));

    // Assert
    assert_eq!(outcome, PaidProbeCommit::WriteFailed);
    assert_eq!(stored_units(&path, day, "anthropic"), None);

    // Control: once the lock is released the same connection commits, so the
    // refusal is the contention and not a broken fixture.
    blocker.execute_batch("ROLLBACK").expect("release the lock");
    assert!(matches!(
        reserve(Some(&mut conn), &command_for("anthropic", 3), day_ms(day)),
        PaidProbeCommit::Committed { used: 1, cap: 3 },
    ));
}

/// A writer holding no connection at all cannot establish anything, so it
/// refuses instead of guessing a count.
#[test]
fn no_connection_reports_a_failed_write() {
    // Act
    let outcome = reserve(None, &command_for("anthropic", 3), day_ms(0));

    // Assert
    assert_eq!(outcome, PaidProbeCommit::WriteFailed);
}

/// The mapping from storage outcome to caller answer, stated case by case.
/// Exactly one storage outcome authorizes a paid call.
#[test]
fn every_storage_outcome_maps_to_its_own_answer() {
    // Arrange / Act / Assert
    assert_eq!(
        commit_for(crate::paid_probe::StoredReservation::Committed {
            utc_day: 20_348,
            used: 2,
            cap: 5,
        }),
        PaidProbeCommit::Committed { used: 2, cap: 5 },
    );
    assert_eq!(
        commit_for(crate::paid_probe::StoredReservation::CapExhausted),
        PaidProbeCommit::CapExhausted,
    );
    assert_eq!(
        commit_for(crate::paid_probe::StoredReservation::MalformedState),
        PaidProbeCommit::MalformedState,
    );
    assert_eq!(
        commit_for(crate::paid_probe::StoredReservation::WriteFailed),
        PaidProbeCommit::WriteFailed,
    );
}

/// The committed answer carries the storage layer's own count and cap, not a
/// number the mapping invented -- a distinct pair, so a swap or a constant
/// would be visible.
#[test]
fn a_committed_answer_carries_the_storage_layers_own_count_and_cap() {
    // Act
    let mapped = commit_for(crate::paid_probe::StoredReservation::Committed {
        utc_day: 1,
        used: 3,
        cap: 7,
    });

    // Assert
    assert_eq!(mapped, PaidProbeCommit::Committed { used: 3, cap: 7 });
}
