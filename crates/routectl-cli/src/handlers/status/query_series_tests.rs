// The series-mode tests for `/status/query`: bucket-token vocabulary, grid
// widening, the all-time re-anchor, and the release-only 100k-row cost check.
// Split from `query_tests.rs` to keep each file under the size ceiling;
// `include!`d into the same `tests` module so the helpers there stay in scope.
// Only the release-only perf test needs imports of its own, so its
// `#[cfg(not(debug_assertions))]`-gated `use` travels with it below; every
// other import comes from the host `query_tests.rs`.

#[cfg(not(debug_assertions))]
use routectl_usage::{BucketSpec, RowCost};

// --- the series mode ----------------------------------------------------

/// A pinned local instant, mid-afternoon on an ordinary day: 2026-06-11
/// (Thursday) 14:30.
///
/// The `today` window runs from local midnight to `now`, so seeding rows at
/// offsets from the REAL clock and asserting they share that window only holds
/// for some hours of the day: `now - 1h` falls into yesterday whenever the suite
/// runs before 01:00 local. Anchoring the arrangement to a fixed instant instead
/// makes seed, window, and expected bucket count agree at every wall-clock hour.
fn fixed_now() -> DateTime<Local> {
    Local
        .from_local_datetime(
            &NaiveDate::from_ymd_opt(2026, 6, 11)
                .unwrap()
                .and_hms_opt(14, 30, 0)
                .unwrap(),
        )
        .earliest()
        .unwrap()
}

#[test]
fn a_bucket_token_returns_a_series_at_the_resolved_width() {
    // Arrange: two rows an hour apart on the pinned day, requested at hour
    // granularity. Both sit after that day's local midnight and at or before the
    // pinned instant, so both are inside `today` whatever the real clock reads.
    // The handler stamps `now` itself, so the window is resolved and the panel
    // built directly -- the same parse -> grid -> fold -> render path, with the
    // clock supplied rather than read.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    let now = fixed_now();
    let now_ms = now.timestamp_millis();
    seed_ledger(&path, &[now_ms - 3_600_000, now_ms]);
    let body = r#"{"window":"today","group_by":"model","bucket":"hour"}"#;
    let (spec, bucket) = spec_from_body(body.as_bytes(), now).expect("body is in vocabulary");
    let pricer = state_with_ledger(path.clone()).router.pricer();

    // Act
    let panel = build_panel(&path, spec, bucket, &pricer, now.to_utc().to_rfc3339(), now);
    let json = serde_json::to_value(&panel).expect("the panel renders");

    // Assert: a populated series whose width is the requested hour, and whose
    // buckets are dense and ascending.
    assert!(json["unavailable"].is_null(), "seeded ledger: {json}");
    assert_eq!(json["data"]["series"]["bucket_ms"], 3_600_000);
    let buckets = json["data"]["series"]["buckets"].as_array().unwrap();
    assert!(!buckets.is_empty(), "a same-day series has buckets: {json}");
    assert!(buckets.len() <= 25, "today+hour never exceeds 25 buckets");
    let served: i64 = buckets
        .iter()
        .map(|b| b["metrics"]["requests"].as_i64().unwrap())
        .sum();
    assert_eq!(served, 2, "every counted row lands in a bucket");
    assert_eq!(json["data"]["totals"]["requests"], 2);
}

#[tokio::test]
async fn an_absent_bucket_token_leaves_the_series_null() {
    // The non-series shape must stay byte-identical to the pre-series one: no
    // series object, an explicit null.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    seed_ledger(&path, &[Local::now().timestamp_millis()]);

    let (status, json) = send(state_with_ledger(path), QUERY_METHOD, VALID_BODY).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        json["data"]["series"],
        Value::Null,
        "no bucket asked, no series served: {json}"
    );
}

#[tokio::test]
async fn an_out_of_vocabulary_bucket_token_is_a_leak_free_400() {
    // Arrange: tokens outside the closed `hour|day` vocabulary. None of them is
    // accepted, so each must return the fixed `invalid_query` 400 envelope --
    // including the plausible-looking granularities and the alternate spellings.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    seed_ledger(&path, &[Local::now().timestamp_millis()]);
    let bodies = [
        r#"{"window":"all","group_by":"model","bucket":"week"}"#,
        r#"{"window":"all","group_by":"model","bucket":"minute"}"#,
        r#"{"window":"all","group_by":"model","bucket":"HOUR"}"#,
        r#"{"window":"all","group_by":"model","bucket":3600000}"#,
        r#"{"window":"all","group_by":"model","bucket_ms":3600000}"#,
    ];

    for body in bodies {
        // Act
        let (status, json) = send(state_with_ledger(path.clone()), QUERY_METHOD, body).await;

        // Assert
        assert_eq!(status, StatusCode::BAD_REQUEST, "must be refused: {body}");
        assert_eq!(json["error"]["code"], INVALID_QUERY);
        let rendered = json.to_string();
        for forbidden in ["week", "minute", "HOUR", "bucket", "expected", "unknown"] {
            assert!(
                !rendered.contains(forbidden),
                "400 envelope leaked `{forbidden}` for {body}: {rendered}"
            );
        }
        assert!(json.get("data").is_none());
    }
}

#[tokio::test]
async fn a_window_too_wide_for_the_cap_widens_the_bucket_instead_of_exceeding_it() {
    // Arrange: a row a decade back, so all-time at hour granularity would want
    // ~90k buckets.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    let now_ms = Local::now().timestamp_millis();
    let decade_ms = 3_650 * 86_400_000_i64;
    seed_ledger(&path, &[now_ms - decade_ms, now_ms]);

    // Act
    let (status, json) = send(
        state_with_ledger(path),
        QUERY_METHOD,
        r#"{"window":"all","group_by":"model","bucket":"hour"}"#,
    )
    .await;

    // Assert: the grid widened to a whole multiple of the requested hour, the
    // count is capped, and coverage is still total.
    assert_eq!(status, StatusCode::OK);
    assert!(json["unavailable"].is_null(), "{json}");
    let bucket_ms = json["data"]["series"]["bucket_ms"].as_i64().unwrap();
    assert!(bucket_ms > 3_600_000, "the grid widened: {bucket_ms}");
    assert_eq!(bucket_ms % 3_600_000, 0, "a whole multiple of the unit");
    let buckets = json["data"]["series"]["buckets"].as_array().unwrap();
    assert!(
        buckets.len() <= MAX_BUCKETS,
        "capped at {MAX_BUCKETS}, got {}",
        buckets.len()
    );
    let served: i64 = buckets
        .iter()
        .map(|b| b["metrics"]["requests"].as_i64().unwrap())
        .sum();
    assert_eq!(served, 2, "widening never drops a row");
}

#[tokio::test]
async fn an_empty_ledger_serves_an_empty_series_never_an_error() {
    // A healthy but empty ledger has nothing to anchor a grid on; that is data,
    // not a client error and not a data-source failure.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    drop(open(&path).expect("open empty ledger"));

    let (status, json) = send(
        state_with_ledger(path),
        QUERY_METHOD,
        r#"{"window":"all","group_by":"model","bucket":"day"}"#,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(json["unavailable"].is_null(), "empty ledger: {json}");
    assert_eq!(
        json["data"]["series"]["buckets"].as_array().unwrap().len(),
        0,
        "an empty ledger is an empty series, not 1000 synthetic zeros"
    );
    assert_eq!(json["data"]["series"]["bucket_ms"], 86_400_000);
    assert_eq!(json["data"]["totals"]["requests"], 0);
}

#[tokio::test]
async fn the_all_time_re_anchor_leaves_the_groups_and_totals_unchanged() {
    // The series path rewrites `from_ms` from the epoch to the earliest row's
    // local midnight. No row predates that, so the row SET is identical and the
    // coarse fold must match the non-series path exactly.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    let now_ms = Local::now().timestamp_millis();
    seed_ledger(
        &path,
        &[now_ms - 30 * 86_400_000, now_ms - 86_400_000, now_ms],
    );
    let state = state_with_ledger(path);

    let (_, plain) = send(state.clone(), QUERY_METHOD, VALID_BODY).await;
    let (_, bucketed) = send(
        state,
        QUERY_METHOD,
        r#"{"window":"all","group_by":"model","bucket":"day"}"#,
    )
    .await;

    assert_eq!(
        plain["data"]["totals"], bucketed["data"]["totals"],
        "the re-anchor changed the totals"
    );
    assert_eq!(
        plain["data"]["groups"], bucketed["data"]["groups"],
        "the re-anchor changed the groups"
    );
    assert_eq!(plain["data"]["totals"]["requests"], 3);
}

#[tokio::test]
async fn a_pre_epoch_row_is_excluded_by_the_bucketed_and_plain_all_time_paths_alike() {
    // Arrange: a row stamped before the 1970 epoch (a skewed clock at write
    // time) beside normal ones. The all-time window's lower bound is the epoch,
    // so the plain path excludes it; the bucketed path re-anchors and must reach
    // the same row set rather than pulling it back in.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    let now_ms = Local::now().timestamp_millis();
    seed_ledger(&path, &[-86_400_000, now_ms - 86_400_000, now_ms]);
    let state = state_with_ledger(path);

    // Act
    let (_, plain) = send(state.clone(), QUERY_METHOD, VALID_BODY).await;
    let (_, bucketed) = send(
        state,
        QUERY_METHOD,
        r#"{"window":"all","group_by":"model","bucket":"day"}"#,
    )
    .await;

    // Assert
    assert_eq!(
        plain["data"]["totals"], bucketed["data"]["totals"],
        "the re-anchor changed the totals"
    );
    assert_eq!(
        plain["data"]["groups"], bucketed["data"]["groups"],
        "the re-anchor changed the groups"
    );
    assert_eq!(
        plain["data"]["totals"]["requests"], 2,
        "the pre-epoch row is outside the all-time window"
    );
    for bucket in bucketed["data"]["series"]["buckets"].as_array().unwrap() {
        assert!(
            bucket["start_ms"].as_i64().unwrap() >= 0,
            "the grid anchored below the window's lower bound: {bucket}"
        );
    }
}

/// Seed `rows` rows spread evenly over `days`, drawn from a small realistic set
/// of model/alias keys so the bucketed GROUP BY builds a real temp b-tree
/// without collapsing to a single group. One prepared statement inside one
/// transaction: the seed itself is not what the timing below measures.
#[cfg(not(debug_assertions))]
fn seed_deep_ledger(path: &Path, rows: usize, days: i64) -> (i64, i64) {
    const MODELS: [&str; 4] = ["m-a", "m-b", "m-c", "m-d"];
    const ALIASES: [&str; 8] = ["a0", "a1", "a2", "a3", "a4", "a5", "a6", "a7"];

    let db = open(path).expect("open ledger");
    let span_ms = days * 86_400_000;
    let from_ms = Local::now().timestamp_millis() - span_ms;
    let conn = db.conn();
    conn.execute_batch("BEGIN").expect("begin");
    {
        let mut stmt = conn
            .prepare(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, ttfb_ms, tool_count, msg_count, attempt_count, fallback_count, \
                 input_tokens, output_tokens) \
                 VALUES (?1, ?1, ?2, 'openai', ?3, ?4, ?3, 'p', 'u', 1, 'ok', \
                 50, 10, 0, 0, 1, 0, 100, 20)",
            )
            .expect("prepare seed");
        for i in 0..rows {
            let ts = from_ms + (i as i64 * span_ms) / rows as i64;
            stmt.execute(rusqlite::params![
                ts,
                format!("r{i}"),
                MODELS[i % MODELS.len()],
                ALIASES[i % ALIASES.len()],
            ])
            .expect("seed row");
        }
    }
    conn.execute_batch("COMMIT").expect("commit");
    (from_ms, span_ms)
}

/// The 100k-row COST CHECK. Release-only: a debug build's SQLite fold is several
/// times slower for reasons the shipped binary never pays, so timing it there
/// would flake without telling us anything about production. The plan-shape half
/// of this guarantee -- that the series statement rides `idx_requests_ts_start`
/// rather than scanning -- is asserted unconditionally in the leaf crate's
/// `the_series_statement_uses_the_ts_start_index`.
#[cfg(not(debug_assertions))]
#[test]
fn a_day_series_over_a_hundred_thousand_rows_stays_inside_the_query_budget() {
    // Arrange: 100k rows over 400 days, read as an all-history day series. The
    // grid is built directly rather than through `resolve_bucket`, so this
    // measures the leaf fold and not the shell's calendar arithmetic.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    let (from_ms, span_ms) = seed_deep_ledger(&path, 100_000, 400);
    let db = open_readonly_fastfail(&path).expect("open seeded ledger");
    let spec = QuerySpec {
        from_ms,
        to_ms: from_ms + span_ms + 1,
        group_by: GroupDim::Model,
        alias_filter: None,
        provider_filter: None,
        bucket: Some(BucketSpec {
            width_ms: 86_400_000,
            count: 401,
        }),
    };
    let budget = Duration::from_millis(QUERY_BUDGET_MS);

    // Act: the deadline is the real one, so a run that blew the budget would
    // interrupt rather than report a misleading elapsed time.
    let started = Instant::now();
    let result = query(&db, &spec, |_row| RowCost::Unpriced, started + budget)
        .expect("a 100k-row day series must complete inside the budget");
    let elapsed = started.elapsed();

    // Assert: comfortably inside the budget, with every row folded into both the
    // groups and the series.
    assert!(
        elapsed * 2 < budget,
        "a 100k-row day series took {elapsed:?} against a {budget:?} budget"
    );
    assert_eq!(result.totals.requests, 100_000);
    let series = result.series.as_ref().expect("series present");
    assert_eq!(series.bucket_ms, 86_400_000);
    assert_eq!(series.buckets.len(), 401);
    let served: i64 = series.buckets.iter().map(|b| b.metrics.requests).sum();
    assert_eq!(served, 100_000, "every counted row lands in a bucket");
}
