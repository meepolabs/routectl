// The per-class error breakdown on groups, totals and series buckets. Split
// from `grouped_tests.rs` to keep each file under the size ceiling;
// `include!`d into the same `tests` module so the fixtures there stay in
// scope. All imports come from the host file -- do not add `use` lines here.

/// Error rows over two aliases, two providers and three buckets of 1000 ms,
/// plus a success, a client hangup and a cancellation that each carry a class
/// a breakdown must not count.
fn seed_error_classes(db: &UsageDb) {
    let rows = [
        ("e1", 100, "al", "pa", "upstream_error", Some("http-5xx")),
        ("e2", 1500, "al", "pa", "upstream_error", None),
        ("ok", 1600, "al", "pa", "ok", None),
        ("e3", 200, "bl", "pb", "upstream_error", Some("timeout")),
        ("e4", 2500, "bl", "pa", "gate_blocked", Some("future-token")),
        (
            "e5",
            2600,
            "al",
            "pb",
            "upstream_error",
            Some("rate-limited"),
        ),
        ("cd", 300, "al", "pa", "client_disconnect", Some("timeout")),
        ("cx", 400, "bl", "pb", "cancelled", Some("timeout")),
    ];
    for (request_id, ts_start, alias, provider, outcome, resolved_class) in rows {
        insert(
            db,
            &Fixture {
                request_id,
                ts_start,
                alias,
                provider: Some(provider),
                outcome,
                resolved_class,
                ..Fixture::default()
            },
        );
    }
}

fn classes(pairs: &[(&str, i64)]) -> BTreeMap<String, i64> {
    pairs
        .iter()
        .map(|&(class, count)| (class.to_string(), count))
        .collect()
}

fn assert_classes_sum_to_errors(case: &str, at: &str, m: &QueryMetrics) {
    assert_eq!(
        m.errors_by_class.values().sum::<i64>(),
        m.errors,
        "{case}: {at} errors_by_class {:?} does not sum to errors",
        m.errors_by_class
    );
}

/// One filter case: its name, the alias and provider filters, the bucket grid,
/// and the totals breakdown it must report.
type FilterCase = (
    &'static str,
    Option<&'static str>,
    Option<&'static str>,
    Option<BucketSpec>,
    &'static [(&'static str, i64)],
);

#[test]
fn errors_by_class_sums_to_errors_on_groups_totals_and_buckets_under_each_filter() {
    let cases: [FilterCase; 4] = [
        (
            "unfiltered",
            None,
            None,
            None,
            &[
                ("future-token", 1),
                ("http-5xx", 1),
                ("rate-limited", 1),
                ("timeout", 1),
                ("unclassified", 1),
            ],
        ),
        (
            "alias filter",
            Some("al"),
            None,
            None,
            &[("http-5xx", 1), ("rate-limited", 1), ("unclassified", 1)],
        ),
        (
            "provider filter",
            None,
            Some("pa"),
            None,
            &[("future-token", 1), ("http-5xx", 1), ("unclassified", 1)],
        ),
        (
            "bucketed",
            None,
            None,
            Some(BucketSpec {
                width_ms: 1000,
                count: 3,
            }),
            &[
                ("future-token", 1),
                ("http-5xx", 1),
                ("rate-limited", 1),
                ("timeout", 1),
                ("unclassified", 1),
            ],
        ),
    ];
    for (case, alias_filter, provider_filter, bucket, expected_totals) in cases {
        // Arrange
        let (_dir, mut db) = open_db();
        seed_error_classes(&db);
        let spec = QuerySpec {
            alias_filter: alias_filter.map(str::to_string),
            provider_filter: provider_filter.map(str::to_string),
            bucket,
            ..spec(GroupDim::Alias)
        };

        // Act
        let result = query(&mut db, &spec, unpriced, no_deadline()).expect("query");

        // Assert
        assert!(!result.groups.is_empty(), "{case}: no groups");
        for group in &result.groups {
            assert_classes_sum_to_errors(case, &group.label, &group.metrics);
        }
        assert_classes_sum_to_errors(case, "totals", &result.totals);
        assert_eq!(
            result.totals.errors_by_class,
            classes(expected_totals),
            "{case}: totals"
        );
        if let Some(series) = &result.series {
            for bucket in &series.buckets {
                let at = format!("bucket {}", bucket.start_ms);
                assert_classes_sum_to_errors(case, &at, &bucket.metrics);
            }
        }
    }
}

#[test]
fn each_series_bucket_carries_the_classes_of_its_own_rows() {
    // Arrange
    let (_dir, mut db) = open_db();
    seed_error_classes(&db);
    let spec = QuerySpec {
        bucket: Some(BucketSpec {
            width_ms: 1000,
            count: 3,
        }),
        ..spec(GroupDim::Alias)
    };

    // Act
    let result = query(&mut db, &spec, unpriced, no_deadline()).expect("query");

    // Assert: a bucket holds the classes of the error rows inside it only.
    let per_bucket: Vec<BTreeMap<String, i64>> = series(&result)
        .buckets
        .iter()
        .map(|b| b.metrics.errors_by_class.clone())
        .collect();
    assert_eq!(
        per_bucket,
        vec![
            classes(&[("http-5xx", 1), ("timeout", 1)]),
            classes(&[("unclassified", 1)]),
            classes(&[("future-token", 1), ("rate-limited", 1)]),
        ]
    );
}

#[test]
fn a_null_class_counts_as_unclassified_and_an_unknown_token_keeps_its_own_key() {
    // Arrange: one group, two errors with no recorded class and one carrying a
    // token this build has never seen.
    let (_dir, mut db) = open_db();
    for (request_id, resolved_class) in [
        ("n1", None),
        ("n2", None),
        ("u1", Some("not-a-known-class")),
    ] {
        insert(
            &db,
            &Fixture {
                request_id,
                outcome: "upstream_error",
                resolved_class,
                ..Fixture::default()
            },
        );
    }

    // Act
    let result = query(&mut db, &spec(GroupDim::Model), unpriced, no_deadline()).expect("query");

    // Assert
    let m = &group(&result, "m1").metrics;
    assert_eq!(m.errors, 3);
    assert_eq!(
        m.errors_by_class,
        classes(&[("not-a-known-class", 1), ("unclassified", 2)])
    );
}

#[test]
fn a_metric_set_without_errors_serializes_an_empty_class_object() {
    // Arrange
    let (_dir, mut db) = open_db();
    insert(&db, &Fixture::default());

    // Act
    let result = query(&mut db, &spec(GroupDim::Model), unpriced, no_deadline()).expect("query");
    let json = serde_json::to_value(&result).expect("serialize");

    // Assert: an object, never null or absent, on groups and totals alike.
    let empty = serde_json::json!({});
    assert_eq!(json["groups"][0]["metrics"]["errors_by_class"], empty);
    assert_eq!(json["totals"]["errors_by_class"], empty);
}

#[test]
fn an_expired_deadline_interrupts_the_error_class_read() {
    // Arrange: enough error rows that the class read alone runs past one
    // progress-callback interval, read inside a guarded transaction as the
    // query fold reads it.
    let (_dir, mut db) = open_db();
    db.conn().execute_batch("BEGIN").expect("begin");
    for i in 0..4000 {
        let request_id = format!("err-{i}");
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
                 resolved_class) \
                 VALUES (?1, ?1, ?2, 'openai', 'req-model', 'al', 'm1', 'pa', 'u1', 0, \
                 'upstream_error', 10, 0, 0, 1, 0, 'http-5xx')",
                rusqlite::params![100 + i, request_id],
            )
            .expect("insert error row");
    }
    db.conn().execute_batch("COMMIT").expect("commit");
    let expired = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("instant predates the process start");
    let window = ReadWindow {
        from_ms: 0,
        to_ms: 10_000,
        alias_filter: None,
        provider_filter: None,
        watermark: i64::MAX,
        anchor_ms: 0,
    };

    // Act
    let guard = DeadlineGuard::install(&mut db, expired).expect("install deadline");
    let tx = guard.conn().unchecked_transaction().expect("begin read");
    let interrupted = read_error_classes(&tx, &window, None, &CancelFlag::default());
    drop(tx);
    drop(guard);
    let after = read_error_classes(
        &db.conn().unchecked_transaction().expect("begin read"),
        &window,
        None,
        &CancelFlag::default(),
    );

    // Assert: the class read sheds as an interrupt, and runs to completion
    // once the guard is gone.
    assert!(
        matches!(interrupted, Err(QueryError::Interrupted)),
        "expected an interrupt, got {interrupted:?}"
    );
    let counts: i64 = after
        .expect("read after the guard")
        .values()
        .flat_map(BTreeMap::values)
        .sum();
    assert_eq!(counts, 4000);
}
