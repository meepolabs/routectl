// Tests for the atomic paid-probe reservation against the control-storage
// table. Every case drives a REAL file-backed database, and the cases that
// assert visibility or contention use separate connections to that file --
// an in-memory or single-connection fixture cannot represent either.

/// Epoch milliseconds inside UTC day 20348, used wherever the specific day
/// does not matter.
const SOME_MS: i64 = 20_348 * MS_PER_UTC_DAY + 5;

/// Open a migrated database in a fresh directory and hand back the owned
/// connection plus the path, so a second connection can be opened to it.
fn reserve_db() -> (tempfile::TempDir, std::path::PathBuf, Connection) {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    let conn = crate::db::open(&path).expect("open").into_conn();
    (dir, path, conn)
}

/// The stored value for one provider-day, read through a connection the
/// reservation does not own.
fn stored_value(path: &std::path::Path, utc_day: i64, provider: &str) -> Option<String> {
    let reader = crate::db::open_readonly(path).expect("read-only open");
    let key = reservation_key(utc_day, provider);
    reader
        .conn()
        .query_row("SELECT value FROM meta WHERE key = ?1", [&key], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .expect("read stored value")
}

/// How many control rows exist for one provider-day.
fn stored_row_count(conn: &Connection, utc_day: i64, provider: &str) -> i64 {
    let key = reservation_key(utc_day, provider);
    conn.query_row("SELECT COUNT(*) FROM meta WHERE key = ?1", [&key], |row| {
        row.get(0)
    })
    .expect("count rows")
}

/// Write a raw value for one provider-day, bypassing the reservation path,
/// so a test can plant the state it wants read back.
fn plant_value(conn: &Connection, utc_day: i64, provider: &str, raw: &str) {
    let key = reservation_key(utc_day, provider);
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = ?2",
        rusqlite::params![&key, raw],
    )
    .expect("plant value");
}

#[test]
fn a_first_reservation_commits_one_unit_visible_from_another_connection() {
    // Arrange
    let (_dir, path, mut conn) = reserve_db();
    let day = utc_day_from_epoch_ms(SOME_MS);

    // Act
    let outcome = reserve_one_unit(&mut conn, "anthropic", 3, SOME_MS);

    // Assert
    assert_eq!(
        outcome,
        StoredReservation::Committed {
            utc_day: day,
            used: 1,
            cap: 3,
        }
    );
    assert_eq!(
        stored_value(&path, day, "anthropic").as_deref(),
        Some("1"),
        "a committed unit must be durable for a later reader"
    );
}

#[test]
fn reservations_commit_up_to_the_cap_and_then_report_exhaustion() {
    // Arrange
    let (_dir, path, mut conn) = reserve_db();
    let day = utc_day_from_epoch_ms(SOME_MS);
    let cap = 4_u32;

    // Act
    let committed: Vec<StoredReservation> = (0..cap)
        .map(|_| reserve_one_unit(&mut conn, "anthropic", cap, SOME_MS))
        .collect();
    let refused = reserve_one_unit(&mut conn, "anthropic", cap, SOME_MS);
    let refused_again = reserve_one_unit(&mut conn, "anthropic", cap, SOME_MS);

    // Assert
    for (index, outcome) in committed.iter().enumerate() {
        let used = u32::try_from(index + 1).expect("small index");
        assert_eq!(
            *outcome,
            StoredReservation::Committed {
                utc_day: day,
                used,
                cap,
            },
            "reservation {index} of a cap-{cap} day"
        );
    }
    assert_eq!(refused, StoredReservation::CapExhausted);
    assert_eq!(refused_again, StoredReservation::CapExhausted);
    assert_eq!(
        stored_value(&path, day, "anthropic").as_deref(),
        Some("4"),
        "a refused reservation must not move the committed count"
    );
}

#[test]
fn a_zero_cap_refuses_and_creates_no_row() {
    // Arrange
    let (_dir, _path, mut conn) = reserve_db();
    let day = utc_day_from_epoch_ms(SOME_MS);

    // Act
    let refused = reserve_one_unit(&mut conn, "anthropic", 0, SOME_MS);

    // Assert
    assert_eq!(refused, StoredReservation::CapExhausted);
    assert_eq!(
        stored_row_count(&conn, day, "anthropic"),
        0,
        "a zero cap must leave the control storage untouched"
    );

    // Control: the same call at a positive cap DOES create the row, so the
    // absence above is the zero cap and not an inert fixture.
    let committed = reserve_one_unit(&mut conn, "anthropic", 1, SOME_MS);
    assert_eq!(
        committed,
        StoredReservation::Committed {
            utc_day: day,
            used: 1,
            cap: 1,
        }
    );
    assert_eq!(stored_row_count(&conn, day, "anthropic"), 1);
}

#[test]
fn a_cap_lowered_below_the_committed_count_refuses_without_changing_it() {
    // Arrange
    let (_dir, path, mut conn) = reserve_db();
    let day = utc_day_from_epoch_ms(SOME_MS);
    plant_value(&conn, day, "anthropic", "5");

    // Act
    let refused = reserve_one_unit(&mut conn, "anthropic", 2, SOME_MS);

    // Assert
    assert_eq!(refused, StoredReservation::CapExhausted);
    assert_eq!(
        stored_value(&path, day, "anthropic").as_deref(),
        Some("5"),
        "lowering a cap must neither cancel nor rewrite committed units"
    );
}

#[test]
fn a_cap_equal_to_the_committed_count_refuses() {
    // Arrange
    let (_dir, path, mut conn) = reserve_db();
    let day = utc_day_from_epoch_ms(SOME_MS);
    plant_value(&conn, day, "anthropic", "2");

    // Act
    let refused = reserve_one_unit(&mut conn, "anthropic", 2, SOME_MS);

    // Assert
    assert_eq!(refused, StoredReservation::CapExhausted);
    assert_eq!(stored_value(&path, day, "anthropic").as_deref(), Some("2"));
}

#[test]
fn a_cap_one_above_the_committed_count_commits_exactly_one_more() {
    // Arrange
    let (_dir, path, mut conn) = reserve_db();
    let day = utc_day_from_epoch_ms(SOME_MS);
    plant_value(&conn, day, "anthropic", "2");

    // Act
    let outcome = reserve_one_unit(&mut conn, "anthropic", 3, SOME_MS);

    // Assert
    assert_eq!(
        outcome,
        StoredReservation::Committed {
            utc_day: day,
            used: 3,
            cap: 3,
        }
    );
    assert_eq!(stored_value(&path, day, "anthropic").as_deref(), Some("3"));
}

#[test]
fn every_malformed_stored_class_refuses_and_leaves_the_bytes_identical() {
    // Arrange: one planted value per refusal class the parser names, plus
    // the shapes a permissive parser would read as zero.
    let malformed = [
        "",
        "00",
        "01",
        "0000000001",
        "-1",
        "+1",
        " 1",
        "1 ",
        "1.0",
        "1e3",
        "0x2",
        "nan",
        "one",
        "4294967296",
        "99999999999999999999",
    ];
    let day = utc_day_from_epoch_ms(SOME_MS);

    for raw in malformed {
        let (_dir, path, mut conn) = reserve_db();
        plant_value(&conn, day, "anthropic", raw);

        // Act
        let refused = reserve_one_unit(&mut conn, "anthropic", 5, SOME_MS);

        // Assert
        assert_eq!(
            refused,
            StoredReservation::MalformedState,
            "malformed state {raw:?} must refuse rather than read as a count"
        );
        assert_eq!(
            stored_value(&path, day, "anthropic").as_deref(),
            Some(raw),
            "a refused reservation must leave {raw:?} byte-identical"
        );
    }
}

#[test]
fn a_committed_count_at_the_unit_ceiling_refuses_without_overflowing() {
    // Arrange
    let (_dir, path, mut conn) = reserve_db();
    let day = utc_day_from_epoch_ms(SOME_MS);
    plant_value(&conn, day, "anthropic", &format_units(u32::MAX));

    // Act
    let refused = reserve_one_unit(&mut conn, "anthropic", u32::MAX, SOME_MS);

    // Assert
    assert_eq!(refused, StoredReservation::CapExhausted);
    assert_eq!(
        stored_value(&path, day, "anthropic").as_deref(),
        Some("4294967295")
    );
}

#[test]
fn the_last_unit_below_the_ceiling_commits_to_the_ceiling() {
    // Arrange
    let (_dir, path, mut conn) = reserve_db();
    let day = utc_day_from_epoch_ms(SOME_MS);
    plant_value(&conn, day, "anthropic", &format_units(u32::MAX - 1));

    // Act
    let outcome = reserve_one_unit(&mut conn, "anthropic", u32::MAX, SOME_MS);

    // Assert
    assert_eq!(
        outcome,
        StoredReservation::Committed {
            utc_day: day,
            used: u32::MAX,
            cap: u32::MAX,
        }
    );
    assert_eq!(
        stored_value(&path, day, "anthropic").as_deref(),
        Some("4294967295")
    );
}

#[test]
fn distinct_providers_and_days_hold_independent_budgets() {
    // Arrange
    let (_dir, path, mut conn) = reserve_db();
    let today = utc_day_from_epoch_ms(SOME_MS);
    let tomorrow_ms = SOME_MS + MS_PER_UTC_DAY;
    let tomorrow = utc_day_from_epoch_ms(tomorrow_ms);

    // Act
    let first = reserve_one_unit(&mut conn, "anthropic", 1, SOME_MS);
    let exhausted = reserve_one_unit(&mut conn, "anthropic", 1, SOME_MS);
    let other_provider = reserve_one_unit(&mut conn, "openai", 1, SOME_MS);
    let next_day = reserve_one_unit(&mut conn, "anthropic", 1, tomorrow_ms);

    // Assert
    assert_eq!(
        first,
        StoredReservation::Committed {
            utc_day: today,
            used: 1,
            cap: 1,
        }
    );
    assert_eq!(exhausted, StoredReservation::CapExhausted);
    assert_eq!(
        other_provider,
        StoredReservation::Committed {
            utc_day: today,
            used: 1,
            cap: 1,
        },
        "one provider's spent day must not spend another's"
    );
    assert_eq!(
        next_day,
        StoredReservation::Committed {
            utc_day: tomorrow,
            used: 1,
            cap: 1,
        },
        "a day rollover must start the provider's budget over"
    );
    assert_eq!(
        stored_value(&path, today, "anthropic").as_deref(),
        Some("1")
    );
    assert_eq!(stored_value(&path, today, "openai").as_deref(), Some("1"));
    assert_eq!(
        stored_value(&path, tomorrow, "anthropic").as_deref(),
        Some("1")
    );
}

#[test]
fn a_provider_whose_name_carries_the_key_separator_keeps_its_own_budget() {
    // Arrange
    let (_dir, _path, mut conn) = reserve_db();

    // Act
    let colon_name = reserve_one_unit(&mut conn, "a:b", 1, SOME_MS);
    let plain_name = reserve_one_unit(&mut conn, "ab", 1, SOME_MS);

    // Assert
    assert!(matches!(
        colon_name,
        StoredReservation::Committed { used: 1, .. }
    ));
    assert!(
        matches!(plain_name, StoredReservation::Committed { used: 1, .. }),
        "a separator-carrying name must not consume another provider's day"
    );
}

#[test]
fn a_database_without_the_control_table_reports_a_failed_write() {
    // Arrange: a real file SQLite will open, carrying no control table.
    let dir = tempfile::TempDir::new().expect("tempdir");
    let path = dir.path().join("bare.db");
    let mut conn = Connection::open(&path).expect("open bare db");

    // Act
    let outcome = reserve_one_unit(&mut conn, "anthropic", 3, SOME_MS);

    // Assert
    assert_eq!(outcome, StoredReservation::WriteFailed);

    // Control: with the table present the same call on the same connection
    // commits, so the refusal above is the missing table and nothing else.
    conn.execute_batch(crate::schema::CREATE_META_TABLE)
        .expect("create control table");
    assert!(matches!(
        reserve_one_unit(&mut conn, "anthropic", 3, SOME_MS),
        StoredReservation::Committed { used: 1, .. }
    ));
}

#[test]
fn a_read_only_connection_reports_a_failed_write_and_commits_nothing() {
    // Arrange
    let (_dir, path, _conn) = reserve_db();
    let day = utc_day_from_epoch_ms(SOME_MS);
    let readonly = crate::db::open_readonly(&path).expect("read-only open");
    let mut readonly_conn = readonly.into_conn();

    // Act
    let outcome = reserve_one_unit(&mut readonly_conn, "anthropic", 3, SOME_MS);

    // Assert
    assert_eq!(outcome, StoredReservation::WriteFailed);
    assert_eq!(
        stored_value(&path, day, "anthropic"),
        None,
        "a failed write must leave no reservation behind"
    );
}

#[test]
fn a_connection_locked_out_by_another_writer_reports_a_failed_write() {
    // Arrange: a sibling connection holds the write lock, and ours sheds
    // immediately rather than waiting for it.
    let (_dir, path, mut conn) = reserve_db();
    let day = utc_day_from_epoch_ms(SOME_MS);
    conn.busy_timeout(std::time::Duration::from_millis(0))
        .expect("drop the busy timeout");
    let blocker = crate::db::open(&path).expect("sibling open").into_conn();
    blocker
        .execute_batch("BEGIN EXCLUSIVE")
        .expect("take the write lock");

    // Act
    let outcome = reserve_one_unit(&mut conn, "anthropic", 3, SOME_MS);

    // Assert
    assert_eq!(outcome, StoredReservation::WriteFailed);
    assert_eq!(stored_value(&path, day, "anthropic"), None);

    // Control: once the lock is released the same connection commits, so
    // the refusal above is the contention and not a broken fixture.
    blocker.execute_batch("ROLLBACK").expect("release the lock");
    assert!(matches!(
        reserve_one_unit(&mut conn, "anthropic", 3, SOME_MS),
        StoredReservation::Committed { used: 1, .. }
    ));
}

#[test]
fn the_reservation_holds_the_write_lock_across_its_own_read() {
    // Arrange: a sibling connection tries to commit a write at the exact
    // point between the reservation's read and its own write, and sheds
    // immediately instead of waiting. A transaction that took the write lock
    // BEFORE reading locks the sibling out and still commits; one that only
    // read would let the sibling in and then lose its own upgrade -- so the
    // two behaviours differ in BOTH assertions below, deterministically and
    // with no timing.
    let (_dir, path, mut conn) = reserve_db();
    let day = utc_day_from_epoch_ms(SOME_MS);
    let sibling = crate::db::open(&path).expect("sibling open").into_conn();
    sibling
        .busy_timeout(std::time::Duration::from_millis(0))
        .expect("drop the busy timeout");

    // Control: the sibling's write succeeds while no reservation is running,
    // so a refusal below is the reservation's lock and not a bad statement.
    sibling
        .execute(
            "INSERT INTO meta (key, value) VALUES ('paid_probe_lock_probe_control', 'x')",
            [],
        )
        .expect("sibling writes freely outside a reservation");

    // Act
    let sibling_result = std::cell::RefCell::new(None);
    let outcome = reserve_one_unit_instrumented(&mut conn, "anthropic", 3, SOME_MS, &|| {
        let attempt = sibling.execute(
            "INSERT INTO meta (key, value) VALUES ('paid_probe_lock_probe', 'x')",
            [],
        );
        *sibling_result.borrow_mut() = Some(attempt.is_err());
    });

    // Assert
    assert_eq!(
        sibling_result.into_inner(),
        Some(true),
        "a sibling write must be locked out from the read onward"
    );
    assert_eq!(
        outcome,
        StoredReservation::Committed {
            utc_day: day,
            used: 1,
            cap: 3,
        },
        "the reservation must not lose its own write to a sibling"
    );
    assert_eq!(stored_value(&path, day, "anthropic").as_deref(), Some("1"));
}

#[test]
fn the_instrumented_body_is_the_production_body() {
    // Arrange: the seam the concurrency test drives must not be a second
    // implementation, so the public entry point's body is only a call into
    // it with a no-op hook.
    let source = include_str!("paid_probe.rs");
    let entry = source
        .split_once("pub(super) fn reserve_one_unit(")
        .expect("reservation entry point")
        .1;
    let body = entry
        .split_once("\n}\n")
        .expect("entry point body terminator")
        .0;

    // Assert
    assert!(
        body.contains("reserve_one_unit_instrumented("),
        "the entry point must delegate to the instrumented body: {body}"
    );
    for forbidden in ["BEGIN", "SELECT", "INSERT", "UPDATE", "commit("] {
        assert!(
            !body.contains(forbidden),
            "the entry point must carry no storage logic of its own ({forbidden}): {body}"
        );
    }
}

#[test]
fn the_module_exposes_no_way_to_give_a_committed_unit_back() {
    // Arrange: a committed unit is spent, so no release / refund / decrement
    // entry point may exist. Lexical, because the assertion is about an
    // ABSENT function -- a behavioural test cannot call what is not there.
    let source = include_str!("paid_probe.rs");

    // Assert: the premise -- the scan does see this module's own functions,
    // so a zero match below is an absence and not an unread file.
    assert!(
        source.contains("fn reserve_one_unit("),
        "the guard must be reading the reservation module"
    );
    for forbidden in [
        "fn release",
        "fn refund",
        "fn decrement",
        "fn cancel",
        "fn unreserve",
    ] {
        assert!(
            !source.contains(forbidden),
            "a committed unit is never given back, so `{forbidden}` must not exist"
        );
    }
}

#[test]
fn committed_counts_only_ever_rise_across_a_days_reservations() {
    // Arrange: the behavioural half of the no-refund contract -- whatever
    // sequence of outcomes a day sees, the stored count never falls.
    let (_dir, path, mut conn) = reserve_db();
    let day = utc_day_from_epoch_ms(SOME_MS);
    let caps = [3_u32, 3, 0, 1, 3, 3, 9, 2, 3];

    // Act
    let mut observed = Vec::new();
    for cap in caps {
        let outcome = reserve_one_unit(&mut conn, "anthropic", cap, SOME_MS);
        let stored = stored_value(&path, day, "anthropic")
            .map_or(0, |raw| parse_units(&raw).expect("canonical stored count"));
        observed.push((outcome, stored));
    }

    // Assert
    let mut previous = 0_u32;
    for (outcome, stored) in &observed {
        assert!(
            *stored >= previous,
            "stored count fell from {previous} to {stored} after {outcome:?}"
        );
        previous = *stored;
    }
    // The premise: this sequence really does include both a commit and a
    // refusal, so the monotonicity above is not read off an inert run.
    assert!(
        observed
            .iter()
            .any(|(outcome, _)| matches!(outcome, StoredReservation::Committed { .. }))
    );
    assert!(
        observed
            .iter()
            .any(|(outcome, _)| *outcome == StoredReservation::CapExhausted)
    );
    assert_eq!(previous, 4, "a cap raised later admits further units");
}
