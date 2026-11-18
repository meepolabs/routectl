// The cancel flag on the read limit: the progress handler, the up-front check,
// and the per-row checks in the Rust loops (fine-row fold, error-class read,
// cache-write lifetimes parse). Split from `grouped_tests.rs` to keep each file
// under the size ceiling; `include!`d into the same `tests` module so the
// fixtures there stay in scope. All imports come from the host file -- do not
// add `use` lines here.

/// A far-future limit carrying `cancel`.
fn limit_with(cancel: &CancelFlag) -> ReadLimit {
    ReadLimit {
        deadline: no_deadline(),
        cancel: cancel.clone(),
    }
}

/// A flag that is already set.
fn cancelled() -> CancelFlag {
    let flag = CancelFlag::default();
    flag.cancel();
    flag
}

/// The whole window, unfiltered, as the side reads bind it.
fn whole_window() -> SideReadWindow<'static> {
    SideReadWindow {
        from_ms: 0,
        to_ms: 10_000,
        alias_filter: None,
        provider_filter: None,
    }
}

/// Three fine rows (three models), each one error row carrying a class and a
/// cache-write split, so every Rust loop of the read sees more than one row.
fn seed_three_fine_rows(db: &UsageDb) {
    for (index, model) in ["m1", "m2", "m3"].into_iter().enumerate() {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
                 resolved_class, cache_write_tokens, cache_write_lifetimes) \
                 VALUES (?1, ?1, ?2, 'anthropic', 'req-model', 'al', ?3, 'pa', 'u1', 0, \
                 'upstream_error', 10, 0, 0, 1, 0, 'http-5xx', 10, '{\"300\":10}')",
                rusqlite::params![100 + index as i64, format!("c{index}"), model],
            )
            .expect("insert fine row");
    }
}

#[test]
fn a_default_flag_is_unset_and_a_clone_sees_cancel() {
    let flag = CancelFlag::default();
    let clone = flag.clone();

    let before = clone.is_cancelled();
    flag.cancel();

    assert!(!before, "a default flag must start unset");
    assert!(clone.is_cancelled(), "a clone shares the one flag");
}

#[test]
fn a_flag_set_before_the_query_interrupts_it_without_pricing_a_row() {
    // An empty ledger gives no row loop a chance to check, so only the check
    // before the read can shed it; the seeded ledger shows no row is priced.
    for (ledger, seed) in [("empty", false), ("seeded", true)] {
        // Arrange
        let (_dir, mut db) = open_db();
        if seed {
            seed_three_fine_rows(&db);
        }
        let priced = std::cell::Cell::new(0_usize);
        let counting = |_row: &AggRow| {
            priced.set(priced.get() + 1);
            RowCost::Unpriced
        };

        // Act
        let interrupted = query(
            &mut db,
            &spec(GroupDim::Model),
            counting,
            limit_with(&cancelled()),
        );
        let after = query(
            &mut db,
            &spec(GroupDim::Model),
            unpriced,
            limit_with(&CancelFlag::default()),
        );

        // Assert: shed as an interrupt before any row reached the pricer, and
        // an unset flag on the same connection reads to completion.
        assert!(
            matches!(interrupted, Err(QueryError::Interrupted)),
            "{ledger}: expected an interrupt, got {interrupted:?}"
        );
        assert_eq!(
            priced.get(),
            0,
            "{ledger}: no row may be priced once cancelled"
        );
        let expected = if seed { 3 } else { 0 };
        assert_eq!(after.expect("query").totals.requests, expected, "{ledger}");
    }
}

#[test]
fn a_flag_set_from_inside_the_price_closure_interrupts_at_the_next_fine_row() {
    // One row per fold path: the plain group fold and the group-and-bucket fold.
    let cases = [
        ("groups", None),
        (
            "groups and buckets",
            Some(BucketSpec {
                width_ms: 1000,
                count: 10,
            }),
        ),
    ];
    for (path, bucket) in cases {
        // Arrange: three fine rows, so the fold has a next row after the one
        // whose pricing sets the flag. The ledger is small enough that the
        // progress handler never runs, so only the fold's own check can stop it.
        let (_dir, mut db) = open_db();
        seed_three_fine_rows(&db);
        let query_spec = QuerySpec {
            bucket,
            ..spec(GroupDim::Model)
        };
        let flag = CancelFlag::default();
        let priced = std::cell::Cell::new(0_usize);
        let cancelling = |_row: &AggRow| {
            priced.set(priced.get() + 1);
            flag.cancel();
            RowCost::Unpriced
        };

        // Act
        let interrupted = query(&mut db, &query_spec, cancelling, limit_with(&flag));

        // Assert
        assert!(
            matches!(interrupted, Err(QueryError::Interrupted)),
            "{path}: expected an interrupt, got {interrupted:?}"
        );
        assert_eq!(
            priced.get(),
            1,
            "{path}: the fold must stop at the next fine row"
        );
    }
}

#[test]
fn a_set_flag_interrupts_each_side_read_row_loop() {
    // One row per side-read loop: each read runs with no progress handler
    // installed, so only its own per-row check can return the interrupt.
    type SideRead = fn(&rusqlite::Transaction<'_>, &CancelFlag) -> Result<usize, QueryError>;
    let cases: [(&str, SideRead); 4] = [
        ("error classes, unbucketed", |tx, cancel| {
            read_error_classes(tx, &whole_window(), None, cancel).map(|c| c.len())
        }),
        ("error classes, bucketed", |tx, cancel| {
            read_error_classes(tx, &whole_window(), Some(1000), cancel).map(|c| c.len())
        }),
        ("cache-write lifetimes, unbucketed", |tx, cancel| {
            read_lifetimes(tx, &whole_window(), None, cancel).map(|l| l.len())
        }),
        ("cache-write lifetimes, bucketed", |tx, cancel| {
            read_lifetimes(tx, &whole_window(), Some(1000), cancel).map(|l| l.len())
        }),
    ];
    let (_dir, db) = open_db();
    seed_three_fine_rows(&db);
    for (read, run) in cases {
        let tx = db.conn().unchecked_transaction().expect("begin read");

        // Act
        let completed = run(&tx, &CancelFlag::default());
        let interrupted = run(&tx, &cancelled());

        // Assert: the unset flag reads every fine row (the control), the set
        // one sheds.
        assert_eq!(
            completed.expect(read),
            3,
            "{read}: control read every fine row"
        );
        assert!(
            matches!(interrupted, Err(QueryError::Interrupted)),
            "{read}: expected an interrupt, got {interrupted:?}"
        );
    }
}

#[test]
fn a_set_flag_interrupts_a_statement_through_the_progress_handler() {
    // Arrange: enough rows that an unguarded aggregate scan runs past one
    // progress-callback interval; `aggregate` has no Rust check of its own, so
    // only the handler can stop it. The deadline is far off.
    let (_dir, mut db) = open_db();
    seed_bulk(&db, 4000);

    // Act
    let guard = DeadlineGuard::install(&mut db, limit_with(&cancelled())).expect("install limit");
    let interrupted = crate::query::aggregate(&guard, 0, 10_000);
    drop(guard);
    let after = crate::query::aggregate(&db, 0, 10_000);

    // Assert: the handler interrupts on the flag alone, and is detached with
    // the guard.
    assert!(
        matches!(interrupted, Err(QueryError::Interrupted)),
        "expected an interrupt, got {interrupted:?}"
    );
    let scanned = after.expect("aggregate after the guard");
    assert_eq!(scanned.iter().map(|r| r.requests).sum::<i64>(), 4000);
}
