// Robustness under interrupts, panics and arithmetic edges: deadline
// interrupts (aggregate and series), the panicking price closure, the cost
// overflow guard, and the interrupt error mapping. Split from
// `grouped_tests.rs` to keep each file under the size ceiling; `include!`d into
// the same `tests` module so the fixtures there stay in scope. All imports come
// from the host -- do not add `use` lines here.

#[test]
fn an_expired_deadline_during_the_series_fold_yields_no_partial_series() {
    // Arrange: enough rows that the bucketed scan runs past one progress
    // callback interval, plus an already-expired deadline.
    let (_dir, db) = open_db();
    seed_bulk(&db, 4000);
    let expired = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("instant predates the process start");

    // Act
    let interrupted = query(&db, &bucketed(GroupDim::Model, 1000, 10), unpriced, expired);
    let after = query(
        &db,
        &bucketed(GroupDim::Model, 1000, 10),
        unpriced,
        no_deadline(),
    );

    // Assert: the interrupt sheds as its own variant rather than densifying a
    // half-folded series, and the connection is left usable.
    assert!(
        matches!(interrupted, Err(QueryError::Interrupted)),
        "expected an interrupt, got {interrupted:?}"
    );
    let recovered = after.expect("query");
    assert_eq!(recovered.totals.requests, 4000);
    assert_eq!(series(&recovered).buckets.len(), 10);
}

/// Seed `count` minimal rows on consecutive `ts_start` values -- enough of them
/// that a full aggregate scan runs past at least one progress-callback interval.
fn seed_bulk(db: &UsageDb, count: i64) {
    db.conn().execute_batch("BEGIN").expect("begin");
    for i in 0..count {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, tool_count, msg_count, attempt_count, fallback_count) \
                 VALUES (?1, ?1, ?2, 'openai', 'req-model', 'al', 'm1', 'pa', 'u1', 0, \
                 'ok', 10, 0, 0, 1, 0)",
                rusqlite::params![100 + i, format!("bulk-{i}")],
            )
            .expect("insert bulk row");
    }
    db.conn().execute_batch("COMMIT").expect("commit");
}

#[test]
fn a_panicking_price_closure_still_detaches_the_progress_handler() {
    // Arrange: a ledger big enough that a full scan runs past a progress
    // callback interval, plus an ALREADY-EXPIRED deadline. The panicking query
    // is windowed down to a single row, so its own statement finishes inside
    // one interval and the handler never gets to fire on it.
    let (_dir, db) = open_db();
    seed_bulk(&db, 4000);
    let expired = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("instant predates the process start");
    let one_row = QuerySpec {
        from_ms: 100,
        to_ms: 101,
        ..spec(GroupDim::Model)
    };

    // Act: the pricing panic unwinds out of the fold the way a caught panel
    // panic would.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        query(&db, &one_row, |_row| panic!("pricing blew up"), expired)
    }));
    std::panic::set_hook(hook);

    // Assert: the connection is left clean, so a following full scan by a
    // caller that installs NO handler of its own still completes. A retained
    // expired handler would interrupt it -- `query` itself cannot show this,
    // since it overwrites the stale handler with its own on entry.
    assert!(panicked.is_err(), "expected the price panic to propagate");
    let scanned = crate::query::aggregate(&db, 0, 10_000).expect("aggregate after the panic");
    assert_eq!(scanned.iter().map(|r| r.requests).sum::<i64>(), 4000);
}

#[test]
fn a_cost_sum_that_overflows_to_infinity_reads_as_unpriced_rather_than_panicking() {
    // Arrange: one model over two upstreams, each priced at a magnitude an
    // operator could reach with an extreme `[registry.*.pricing]` rate, so the
    // group's SUM overflows f64 to infinity.
    let (_dir, db) = open_db();
    for (id, upstream) in [("a", "u1"), ("b", "u2")] {
        insert(
            &db,
            &Fixture {
                request_id: id,
                upstream: Some(upstream),
                ..Fixture::default()
            },
        );
    }

    // Act: the fold is network-reachable and the release profile aborts on
    // panic, so a non-finite total must be a VALUE, never an assertion.
    let result = query(
        &db,
        &spec(GroupDim::Model),
        |_row| RowCost::Priced(f64::MAX),
        no_deadline(),
    )
    .expect("query");

    // Assert: no cost claimed, and the status says so rather than reporting a
    // meaningless infinity.
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.cost_usd, None);
    assert_eq!(m.cost_status, CostStatus::Unpriced);
    assert_eq!(result.totals.cost_usd, None);
    assert_eq!(result.totals.cost_status, CostStatus::Unpriced);
    // The rest of the group is untouched: only the cost degraded.
    assert_eq!(m.requests, 2);
}

#[test]
fn a_finite_cost_still_prices_normally_beside_the_overflow_guard() {
    // Arrange: the guard must not swallow ordinary large-but-finite costs.
    let (_dir, db) = open_db();
    insert(&db, &Fixture::default());

    // Act
    let result = query(
        &db,
        &spec(GroupDim::Model),
        |_row| RowCost::Priced(1e300),
        no_deadline(),
    )
    .expect("query");

    // Assert
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.cost_status, CostStatus::Priced);
    assert_eq!(m.cost_usd, Some(1e300));
}

#[test]
fn interrupt_error_maps_to_its_own_variant() {
    // Arrange: the SQLite code a fired progress handler produces.
    let interrupted = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
        None,
    );
    let corrupt = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
        None,
    );

    // Act + Assert: a deadline interrupt is distinguishable from a real fault.
    assert!(matches!(
        QueryError::from(interrupted),
        QueryError::Interrupted
    ));
    assert!(matches!(QueryError::from(corrupt), QueryError::Sqlite(_)));
}

#[test]
fn expired_deadline_interrupts_the_query_and_leaves_the_connection_usable() {
    // Arrange: enough rows that the aggregate scan runs past one progress
    // callback interval.
    let (_dir, db) = open_db();
    seed_bulk(&db, 4000);
    let expired = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("instant predates the process start");

    // Act
    let interrupted = query(&db, &spec(GroupDim::Model), unpriced, expired);
    let after = query(&db, &spec(GroupDim::Model), unpriced, no_deadline());

    // Assert: the expired deadline sheds as its own variant, and the handler is
    // uninstalled afterwards so the next query on the same connection runs.
    assert!(
        matches!(interrupted, Err(QueryError::Interrupted)),
        "expected an interrupt, got {interrupted:?}"
    );
    assert_eq!(after.expect("query").totals.requests, 4000);
}
