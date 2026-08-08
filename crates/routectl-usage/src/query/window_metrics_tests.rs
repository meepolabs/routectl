// The per-window aggregate metrics (generation window, presence counts,
// stream counts, TTFBs) and the reuse-sample reader. Split from `tests.rs` to
// keep each file under the size ceiling; `include!`d into the same `tests`
// module so the helpers there stay in scope. All imports come from the host
// `tests.rs` -- do not add `use` lines here.

#[test]
fn aggregate_gen_window_only_counts_streaming_ok_rows_with_ttfb() {
    // Arrange: one qualifying streaming-ok row, plus rows that the
    // predicate must exclude (non-stream, error, NULL ttfb, latency<=ttfb).
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    // Qualifying: stream, ok, ttfb=100, latency=500 -> gen window 400.
    insert_full_row(
        &db,
        "g1",
        100,
        1,
        "ok",
        Some(100),
        500,
        Some(40),
        None,
        None,
    );
    // Non-stream row excluded.
    insert_full_row(
        &db,
        "g2",
        110,
        0,
        "ok",
        Some(100),
        500,
        Some(40),
        None,
        None,
    );
    // Error row excluded.
    insert_full_row(
        &db,
        "g3",
        120,
        1,
        "upstream_error",
        Some(100),
        500,
        Some(40),
        None,
        None,
    );
    // NULL ttfb excluded.
    insert_full_row(&db, "g4", 130, 1, "ok", None, 500, Some(40), None, None);
    // latency <= ttfb excluded.
    insert_full_row(
        &db,
        "g5",
        140,
        1,
        "ok",
        Some(500),
        500,
        Some(40),
        None,
        None,
    );

    // Act
    let rows = aggregate(&db, 0, 1000).expect("aggregate");

    // Assert: only the first row contributes to the generation window.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].gen_window_ms, 400);
    assert_eq!(rows[0].gen_output_tokens, 40);
}

#[test]
fn aggregate_presence_counts_distinguish_null_from_reported_zero() {
    // Arrange: one row reasoning=0 (reported), one row reasoning=NULL.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_full_row(
        &db,
        "p1",
        100,
        1,
        "ok",
        Some(10),
        50,
        Some(1),
        Some(0),
        Some(5),
    );
    insert_full_row(&db, "p2", 110, 1, "ok", Some(10), 50, Some(1), None, None);

    // Act
    let rows = aggregate(&db, 0, 1000).expect("aggregate");

    // Assert: COUNT(col) ignores the NULL row -> 1, not 2.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].reasoning_present, 1);
    assert_eq!(rows[0].cache_read_present, 1);
    assert_eq!(rows[0].reasoning_tokens, 0);
}

#[test]
fn aggregate_stream_count_sums_streaming_flag() {
    // Arrange: two streaming rows, one non-stream row.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_full_row(&db, "c1", 100, 1, "ok", Some(10), 50, Some(1), None, None);
    insert_full_row(&db, "c2", 110, 1, "ok", Some(10), 50, Some(1), None, None);
    insert_full_row(&db, "c3", 120, 0, "ok", Some(10), 50, Some(1), None, None);

    // Act
    let rows = aggregate(&db, 0, 1000).expect("aggregate");

    // Assert
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].stream_count, 2);
    assert_eq!(rows[0].ttfb_count, 3);
    assert_eq!(rows[0].sum_ttfb_ms, 30);
}

#[test]
fn ttfbs_returns_in_window_streaming_ok_values() {
    // Arrange: two qualifying streaming-ok rows, plus excluded rows.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_full_row(&db, "t1", 100, 1, "ok", Some(11), 50, Some(1), None, None);
    insert_full_row(&db, "t2", 110, 1, "ok", Some(22), 50, Some(1), None, None);
    // Non-stream excluded.
    insert_full_row(&db, "t3", 120, 0, "ok", Some(33), 50, Some(1), None, None);
    // Error excluded.
    insert_full_row(
        &db,
        "t4",
        130,
        1,
        "timeout",
        Some(44),
        50,
        Some(1),
        None,
        None,
    );
    // Out of window excluded.
    insert_full_row(&db, "t5", 5, 1, "ok", Some(55), 50, Some(1), None, None);

    // Act
    let rows = ttfbs(&db, 100, 1000).expect("ttfbs");

    // Assert
    let values: Vec<i64> = rows.iter().map(|(_, ms)| *ms).collect();
    assert_eq!(values.len(), 2);
    assert!(values.contains(&11));
    assert!(values.contains(&22));
    assert!(!values.contains(&33));
    assert!(!values.contains(&44));
    assert!(!values.contains(&55));
}

/// Insert a row exercising the reuse-sample columns: nullable
/// `session_id`, `provider_kind`, `model`, `cache_read`, and an explicit
/// `outcome` so the admission-contract filter is exercisable.
#[allow(clippy::too_many_arguments)]
fn insert_reuse_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    session_id: Option<&str>,
    provider_kind: Option<&str>,
    model: Option<&str>,
    cache_read: Option<i64>,
    outcome: &str,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider_kind, session_id, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, cache_read) \
             VALUES (?1, ?1, ?2, 'anthropic', 'req-model', 'al', ?3, ?4, ?5, 1, ?6, \
             5, 0, 0, 1, 0, ?7)",
            rusqlite::params![
                ts_start,
                request_id,
                model,
                provider_kind,
                session_id,
                outcome,
                cache_read,
            ],
        )
        .expect("insert reuse row");
}

#[test]
fn read_reuse_samples_filters_nulls_coalesces_and_orders() {
    // Arrange: a mix of complete and partial rows, two triples, delivered
    // out of ts order, plus an out-of-window row.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    // Complete rows for triple A (one with NULL cache_read -> 0).
    insert_reuse_row(
        &db,
        "a2",
        200,
        Some("s1"),
        Some("anthropic-api"),
        Some("opus"),
        Some(42),
        "ok",
    );
    insert_reuse_row(
        &db,
        "a1",
        100,
        Some("s1"),
        Some("anthropic-api"),
        Some("opus"),
        None,
        "ok",
    );
    // A second triple (different provider_kind).
    insert_reuse_row(
        &db,
        "b1",
        150,
        Some("s1"),
        Some("bedrock"),
        Some("opus"),
        Some(7),
        "ok",
    );
    // NULL session_id -> filtered out (no usable triple identity).
    insert_reuse_row(
        &db,
        "n-sess",
        120,
        None,
        Some("anthropic-api"),
        Some("opus"),
        Some(9),
        "ok",
    );
    // NULL provider_kind -> filtered out.
    insert_reuse_row(
        &db,
        "n-pk",
        130,
        Some("s2"),
        None,
        Some("opus"),
        Some(9),
        "ok",
    );
    // NULL model -> filtered out.
    insert_reuse_row(
        &db,
        "n-model",
        140,
        Some("s2"),
        Some("anthropic-api"),
        None,
        Some(9),
        "ok",
    );
    // Out of window (ts < window_start).
    insert_reuse_row(
        &db,
        "old",
        50,
        Some("s1"),
        Some("anthropic-api"),
        Some("opus"),
        Some(99),
        "ok",
    );

    // Act: window starts at 100.
    let rows = read_reuse_samples_since(db.conn(), 100, 100).expect("read");

    // Assert: three rows survive (the three complete, in-window rows),
    // ordered ascending by ts.
    let ids: Vec<i64> = rows.iter().map(|r| r.ts_start_ms).collect();
    assert_eq!(ids, vec![100, 150, 200]);
    // NULL cache_read coalesced to 0 on a1.
    let a1 = rows.iter().find(|r| r.ts_start_ms == 100).expect("a1");
    assert_eq!(a1.cache_read, 0);
    assert_eq!(a1.session_id, "s1");
    assert_eq!(a1.provider_kind, "anthropic-api");
    assert_eq!(a1.model, "opus");
    // The cross-provider row maps its own provider_kind.
    let b1 = rows.iter().find(|r| r.ts_start_ms == 150).expect("b1");
    assert_eq!(b1.provider_kind, "bedrock");
    assert_eq!(b1.cache_read, 7);
}

#[test]
fn read_reuse_samples_selects_newest_within_window() {
    // Arrange: limit + 2 eligible rows with ascending distinct ts_start,
    // all inside the window.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    let limit = 3usize;
    let total = limit + 2;
    for i in 0..total as i64 {
        insert_reuse_row(
            &db,
            &format!("r{i}"),
            1000 + i,
            Some("s1"),
            Some("anthropic-api"),
            Some("opus"),
            Some(1),
            "ok",
        );
    }

    // Act: window admits all rows; cap below the eligible count.
    let rows = read_reuse_samples_since(db.conn(), 1000, limit).expect("read");

    // Assert: the returned set is the NEWEST `limit` rows (largest
    // timestamps), returned oldest-first. Under the oldest-first ASC
    // LIMIT this returned [1000, 1001, 1002] and fails.
    let ids: Vec<i64> = rows.iter().map(|r| r.ts_start_ms).collect();
    assert_eq!(ids, vec![1002, 1003, 1004]);
}

#[test]
fn read_reuse_samples_breaks_cap_boundary_ties_by_insertion_order() {
    // Arrange: more qualifying rows than the limit, where the rows
    // straddling the cap boundary share an identical ts_start. Insertion
    // order (rowid) is the only signal distinguishing them, so it must
    // decide which survive and how survivors are ordered. Each tied row
    // carries a distinct cache_read so the public row shape reveals both
    // which rows survived and in what order.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    let limit = 3usize;
    // Four rows at the SAME ts_start, inserted earliest-first with
    // ascending cache_read markers (10, 20, 30, 40).
    for (i, marker) in [10i64, 20, 30, 40].into_iter().enumerate() {
        insert_reuse_row(
            &db,
            &format!("tie{i}"),
            500,
            Some("s1"),
            Some("anthropic-api"),
            Some("opus"),
            Some(marker),
            "ok",
        );
    }

    // Act: cap below the count of tied rows.
    let rows = read_reuse_samples_since(db.conn(), 0, limit).expect("read");

    // Assert: the three most-recently-inserted tied rows survive (markers
    // 20, 30, 40 -- dropping the earliest-inserted 10), emitted in stable
    // insertion order (oldest-first, rowid ascending) despite the shared
    // ts_start.
    assert_eq!(rows.len(), limit);
    let ts: Vec<i64> = rows.iter().map(|r| r.ts_start_ms).collect();
    assert_eq!(ts, vec![500, 500, 500]);
    let markers: Vec<i64> = rows.iter().map(|r| r.cache_read).collect();
    assert_eq!(markers, vec![20, 30, 40]);
}

#[test]
fn read_reuse_samples_excludes_non_ok_outcome() {
    // Arrange: a mid-stream-failed row (upstream_error) that still
    // carries a full triple and a non-null cache_read -- the divergence
    // case where the live path never records it (record_k_sample only
    // fires on the success finalize / natural stream EOS) but a
    // filter-less rebuild would replay it after a restart. An ok row in
    // the same window must still be admitted.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    insert_reuse_row(
        &db,
        "failed",
        100,
        Some("s1"),
        Some("anthropic-api"),
        Some("opus"),
        Some(42),
        "upstream_error",
    );
    insert_reuse_row(
        &db,
        "succeeded",
        110,
        Some("s1"),
        Some("anthropic-api"),
        Some("opus"),
        Some(7),
        "ok",
    );

    // Act
    let rows = read_reuse_samples_since(db.conn(), 0, 100).expect("read");

    // Assert: only the ok row survives.
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].ts_start_ms, 110);
    assert_eq!(rows[0].cache_read, 7);
}
