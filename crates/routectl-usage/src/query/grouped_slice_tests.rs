// The sliced read: a window read in parallel ts_start slices under one rowid
// watermark answers exactly as one slice does, cuts its span where the
// contract says, ignores rows appended after its watermark, and stops every
// slice together. `include!`d into the `tests` module of `grouped_tests.rs` so
// the fixtures there stay in scope. All imports come from the host file -- do
// not add `use` lines here.

const SLICE_WIDTH_MS: i64 = 10_000;
const SLICED_TO_MS: i64 = 4 * SLICE_WIDTH_MS;
const WEEK_MS: i64 = 7 * 86_400_000;
const DAY_MS: i64 = 86_400_000;

/// One fine lane of the dense fixture: its group-key columns.
struct Lane {
    model: &'static str,
    provider: &'static str,
    upstream: &'static str,
    alias: &'static str,
    seat: Option<&'static str>,
}

/// Lanes over three providers (one priced, one subscription, one unpriced by
/// [`fingerprint_price`]), two aliases, and a NULL seat.
const LANES: [Lane; 4] = [
    Lane {
        model: "m1",
        provider: "pa",
        upstream: "u1",
        alias: "al",
        seat: Some("pa#a"),
    },
    Lane {
        model: "m1",
        provider: "pa",
        upstream: "u2",
        alias: "bl",
        seat: None,
    },
    Lane {
        model: "m2",
        provider: "pb",
        upstream: "u1",
        alias: "bl",
        seat: Some("pb"),
    },
    Lane {
        model: "m3",
        provider: "pc",
        upstream: "u3",
        alias: "al",
        seat: None,
    },
];

/// Every column a dense row sets; the rest stay NULL.
struct DenseRow<'a> {
    request_id: String,
    ts_start: i64,
    lane: &'a Lane,
    /// `None` writes a NULL served model (a pre-dispatch row); the group key
    /// still reads the lane's model through `requested_model`.
    served: bool,
    outcome: &'static str,
    resolved_class: Option<&'static str>,
    stream: i64,
    latency_ms: i64,
    ttfb_ms: Option<i64>,
    fallback_count: i64,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    reasoning_tokens: Option<i64>,
    cache_read: Option<i64>,
    cache_write_5m: Option<i64>,
    cache_write_1h: Option<i64>,
    cache_write_tokens: Option<i64>,
    cache_write_lifetimes: Option<&'static str>,
    server_tool_use: Option<&'static str>,
}

impl<'a> DenseRow<'a> {
    fn new(request_id: String, ts_start: i64, lane: &'a Lane) -> Self {
        Self {
            request_id,
            ts_start,
            lane,
            served: true,
            outcome: "ok",
            resolved_class: None,
            stream: 0,
            latency_ms: 10,
            ttfb_ms: None,
            fallback_count: 0,
            input_tokens: None,
            output_tokens: None,
            reasoning_tokens: None,
            cache_read: None,
            cache_write_5m: None,
            cache_write_1h: None,
            cache_write_tokens: None,
            cache_write_lifetimes: None,
            server_tool_use: None,
        }
    }
}

fn insert_dense(conn: &rusqlite::Connection, row: &DenseRow<'_>) {
    conn.execute(
        "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
         requested_model, alias, model, provider, upstream, seat, stream, outcome, \
         resolved_class, latency_ms, ttfb_ms, tool_count, msg_count, attempt_count, \
         fallback_count, input_tokens, output_tokens, reasoning_tokens, cache_read, \
         cache_write_5m, cache_write_1h, cache_write_tokens, cache_write_lifetimes, \
         server_tool_use) \
         VALUES (?1, ?1, ?2, 'anthropic', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, \
         ?13, 0, 1, 1, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23)",
        rusqlite::params![
            row.ts_start,
            row.request_id,
            row.lane.model,
            row.lane.alias,
            row.served.then_some(row.lane.model),
            row.lane.provider,
            row.lane.upstream,
            row.lane.seat,
            row.stream,
            row.outcome,
            row.resolved_class,
            row.latency_ms,
            row.ttfb_ms,
            row.fallback_count,
            row.input_tokens,
            row.output_tokens,
            row.reasoning_tokens,
            row.cache_read,
            row.cache_write_5m,
            row.cache_write_1h,
            row.cache_write_tokens,
            row.cache_write_lifetimes,
            row.server_tool_use,
        ],
    )
    .expect("insert dense row");
}

/// The rows of one lane inside slice `slice` of `[0, SLICED_TO_MS)`: a legacy-era
/// streaming success on the slice's lower edge, a new-era success with a
/// lifetime split, an error, a pre-dispatch hangup, a cancellation, and a last
/// success just under the next edge. `tag` keeps request ids unique across
/// calls.
fn lane_rows<'a>(tag: &str, slice: i64, lane_ix: usize, lane: &'a Lane) -> Vec<DenseRow<'a>> {
    let base = slice * SLICE_WIDTH_MS;
    let n = lane_ix as i64;
    let id = |kind: &str| format!("{tag}-s{slice}-l{lane_ix}-{kind}");
    let classes = ["http-5xx", "timeout", "rate-limited"];
    vec![
        DenseRow {
            stream: 1,
            ttfb_ms: Some(40 + slice + n),
            latency_ms: 400 + 10 * n + slice,
            fallback_count: 1,
            input_tokens: Some(100 + slice),
            output_tokens: Some(50 + slice + n),
            reasoning_tokens: Some(7 + n),
            cache_read: Some(1_000 + 3 * slice + n),
            cache_write_5m: Some(30 + slice),
            cache_write_1h: Some(10 + n),
            server_tool_use: Some(r#"{"web_search":2,"code":1}"#),
            ..DenseRow::new(id("legacy"), base, lane)
        },
        DenseRow {
            latency_ms: 90 + slice,
            input_tokens: Some(20 + n),
            output_tokens: Some(5),
            cache_read: Some(501 + slice),
            cache_write_tokens: Some(40 + slice),
            cache_write_lifetimes: Some(if slice == 1 && lane_ix == 3 {
                r#"{"300":99}"#
            } else {
                r#"{"300":25,"3600":10}"#
            }),
            ..DenseRow::new(id("split"), base + 2_500 + n, lane)
        },
        DenseRow {
            outcome: "upstream_error",
            resolved_class: Some(classes[(slice as usize + lane_ix) % classes.len()]),
            latency_ms: 30,
            ..DenseRow::new(id("error"), base + 5_000 + n, lane)
        },
        DenseRow {
            served: false,
            outcome: "client_disconnect",
            latency_ms: 3,
            ..DenseRow::new(id("hangup"), base + 6_000 + n, lane)
        },
        DenseRow {
            outcome: "cancelled",
            latency_ms: 4,
            input_tokens: Some(9),
            ..DenseRow::new(id("cancel"), base + 7_000 + n, lane)
        },
        DenseRow {
            stream: 1,
            ttfb_ms: Some(20),
            latency_ms: 200 + slice,
            output_tokens: Some(70),
            cache_write_tokens: Some(5),
            cache_write_lifetimes: Some(if slice == 2 && lane_ix == 0 {
                r#"{"300":9}"#
            } else {
                r#"{"300":5}"#
            }),
            ..DenseRow::new(id("tail"), base + SLICE_WIDTH_MS - 1, lane)
        },
    ]
}

/// Every lane in every one of the four slices of `[0, SLICED_TO_MS)`.
fn seed_dense(db: &UsageDb, tag: &str) {
    db.conn().execute_batch("BEGIN").expect("begin");
    for slice in 0..4 {
        for (lane_ix, lane) in LANES.iter().enumerate() {
            for row in lane_rows(tag, slice, lane_ix, lane) {
                insert_dense(db.conn(), &row);
            }
        }
    }
    db.conn().execute_batch("COMMIT").expect("commit");
}

/// A cost that moves with every field of the row it prices, so a field the
/// merge loses or doubles shows up in the result. `pa` rows are priced, `pb`
/// rows are subscription rows valued the same way, `pc` rows are unpriced.
fn fingerprint_price(row: &AggRow) -> RowCost {
    let value = fingerprint(row);
    match row.key.provider.as_deref() {
        Some("pa") => RowCost::Priced(value),
        Some("pb") => RowCost::Subscription(Some(value), vec!["cache_write_1h".into()]),
        _ => RowCost::Unpriced(vec!["input".into()]),
    }
}

fn fingerprint(row: &AggRow) -> f64 {
    let AggRow {
        key: _,
        requests,
        ok,
        errors,
        input_tokens,
        output_tokens,
        reasoning_tokens,
        cache_read_peak,
        cache_read_avg,
        cache_read_billed,
        cache_write_5m,
        cache_write_1h,
        server_tool_calls,
        sum_ttfb_ms,
        ttfb_count,
        gen_window_ms,
        gen_output_tokens,
        reasoning_present,
        cache_read_present,
        cache_write_5m_present,
        cache_write_1h_present,
        server_tool_present,
        stream_count,
        client_disconnect_total,
        client_disconnect_pre_dispatch,
        cancelled_total,
        cache_write_tokens,
        cache_write_present,
        cache_write_lifetimes,
    } = row;
    let columns = [
        *requests,
        *ok,
        *errors,
        *input_tokens,
        *output_tokens,
        *reasoning_tokens,
        *cache_read_peak,
        *cache_read_avg,
        *cache_read_billed,
        *cache_write_5m,
        *cache_write_1h,
        *server_tool_calls,
        *sum_ttfb_ms,
        *ttfb_count,
        *gen_window_ms,
        *gen_output_tokens,
        *reasoning_present,
        *cache_read_present,
        *cache_write_5m_present,
        *cache_write_1h_present,
        *server_tool_present,
        *stream_count,
        *client_disconnect_total,
        *client_disconnect_pre_dispatch,
        *cancelled_total,
        *cache_write_tokens,
        *cache_write_present,
    ];
    // Distinct power-of-two weights: a lost or doubled column changes the
    // value, and every product stays an exact f64.
    let mut value: f64 = columns
        .iter()
        .enumerate()
        .map(|(i, &v)| v as f64 * (1_u64 << i) as f64 / 1024.0)
        .sum();
    let lifetimes = cache_write_lifetimes.as_ref().expect("lifetimes attached");
    for (seconds, tokens) in &lifetimes.tokens_by_lifetime {
        value += f64::from(*seconds) * *tokens as f64 / 1e6;
    }
    value += lifetimes.unknown_tokens as f64 / 7.0;
    if lifetimes.invalid_split {
        value += 0.5;
    }
    value
}

/// A plan that reads in exactly `slices` slices.
const fn forced(slices: usize) -> ReadPlan<'static> {
    ReadPlan {
        slices: Some(slices),
        watermark: None,
        open_slice: None,
    }
}

fn sliced_spec(group_by: GroupDim) -> QuerySpec {
    QuerySpec {
        from_ms: 0,
        to_ms: SLICED_TO_MS,
        group_by,
        alias_filter: None,
        provider_filter: None,
        model_filter: None,
        bucket: None,
        entry_accounts: Default::default(),
    }
}

fn read(db: &mut UsageDb, spec: &QuerySpec, plan: &ReadPlan<'_>) -> QueryResult {
    query_with(
        db,
        spec,
        fingerprint_price,
        ReadLimit::from(no_deadline()),
        plan,
    )
    .expect("query")
}

/// Every metric of `m`, each present or non-zero: the positive control that
/// the fixture can show a field the merge mishandles.
fn assert_every_metric_present(at: &str, m: &QueryMetrics) {
    let counts = [
        ("requests", m.requests),
        ("ok", m.ok),
        ("errors", m.errors),
        ("input_tokens", m.input_tokens),
        ("output_tokens", m.output_tokens),
        ("reasoning_tokens", m.reasoning_tokens),
        ("cache_read_billed", m.cache_read_billed),
        ("cache_write_5m", m.cache_write_5m),
        ("cache_write_1h", m.cache_write_1h),
        ("cache_write_tokens", m.cache_write_tokens),
        ("cache_write_present", m.cache_write_present),
        ("cache_read_present", m.cache_read_present),
        ("server_tool_calls", m.server_tool_calls),
        ("server_tool_present", m.server_tool_present),
        ("stream_count", m.stream_count),
        ("client_disconnect_total", m.client_disconnect_total),
        (
            "client_disconnect_pre_dispatch",
            m.client_disconnect_pre_dispatch,
        ),
        ("cancelled_total", m.cancelled_total),
        ("fallback_served", m.fallback_served),
    ];
    for (field, value) in counts {
        assert!(value > 0, "{at}: {field} is {value}");
    }
    let derived = [
        ("ttft_avg_ms", m.ttft_avg_ms.is_some()),
        ("ttft_max_ms", m.ttft_max_ms.is_some()),
        ("latency_avg_ms", m.latency_avg_ms.is_some()),
        ("latency_max_ms", m.latency_max_ms.is_some()),
        ("throughput_tok_s", m.throughput_tok_s.is_some()),
        ("ctx_avg", m.ctx_avg.is_some()),
        ("ctx_peak", m.ctx_peak.is_some()),
        ("cache_hit_pct", m.cache_hit_pct.is_some()),
        ("cost_usd", m.cost_usd.is_some()),
        ("equivalent_cost_usd", m.equivalent_cost_usd.is_some()),
        ("errors_by_class", m.errors_by_class.len() > 1),
        (
            "missing_price_dimensions",
            !m.missing_price_dimensions.is_empty(),
        ),
    ];
    for (field, present) in derived {
        assert!(present, "{at}: {field} is absent");
    }
}

#[test]
fn four_slices_answer_exactly_as_one_for_every_grouping_filter_and_grid() {
    // Arrange: a dense fixture, and a positive control that each slice alone
    // carries every metric.
    let (_dir, mut db) = open_db();
    seed_dense(&db, "dense");
    for slice in 0..4 {
        let alone = QuerySpec {
            from_ms: slice * SLICE_WIDTH_MS,
            to_ms: (slice + 1) * SLICE_WIDTH_MS,
            ..sliced_spec(GroupDim::Model)
        };
        let totals = read(&mut db, &alone, &forced(1)).totals;
        assert_every_metric_present(&format!("slice {slice}"), &totals);
    }
    let filters = [
        ("unfiltered", None, None, None),
        ("alias al", Some("al"), None, None),
        ("provider pa", None, Some("pa"), None),
        ("model m1", None, None, Some("m1")),
    ];
    let grids = [
        ("no series", None),
        (
            "grid on the slice edges",
            Some(BucketSpec {
                width_ms: 1_000,
                count: 40,
            }),
        ),
        (
            "grid cut by the slice edges",
            Some(BucketSpec {
                width_ms: 3_000,
                count: 14,
            }),
        ),
    ];
    for group_by in GroupDim::ALL {
        for (filter, alias_filter, provider_filter, model_filter) in filters {
            for (grid, bucket) in grids {
                let case = format!("{} / {filter} / {grid}", group_by.as_str());
                let spec = QuerySpec {
                    alias_filter: alias_filter.map(String::from),
                    provider_filter: provider_filter.map(String::from),
                    model_filter: model_filter.map(String::from),
                    bucket,
                    ..sliced_spec(group_by)
                };

                // Act
                let one = read(&mut db, &spec, &forced(1));
                let four = read(&mut db, &spec, &forced(4));

                // Assert
                assert!(one.totals.requests > 0, "{case}: the filter matched rows");
                assert_eq!(four, one, "{case}");
            }
        }
    }
}

#[test]
fn the_merged_mean_cache_read_truncates_like_the_ledgers_average() {
    // A mean whose fraction is under, exactly at and over one half, the rows
    // in different slices; the ledger's own AVG over the same rows is the
    // oracle.
    let cases: [(&str, &[i64]); 4] = [
        ("whole", &[88_000, 89_000, 91_000]),
        ("fraction under a half", &[1, 1, 2]),
        ("fraction of exactly a half", &[1, 2]),
        ("fraction over a half", &[1, 2, 2]),
    ];
    for (case, reads) in cases {
        // Arrange
        let (_dir, mut db) = open_db();
        for (i, (&cache_read, ts_start)) in reads.iter().zip([0, 15_000, 35_000]).enumerate() {
            let row = DenseRow {
                cache_read: Some(cache_read),
                ..DenseRow::new(format!("avg-{i}"), ts_start, &LANES[0])
            };
            insert_dense(db.conn(), &row);
        }
        let ledger_avg: Vec<i64> = crate::query::aggregate(&db, 0, SLICED_TO_MS)
            .expect("aggregate")
            .iter()
            .map(|row| row.cache_read_avg)
            .collect();
        let seen = std::cell::RefCell::new(Vec::new());
        let recording = |row: &AggRow| {
            seen.borrow_mut().push(row.cache_read_avg);
            RowCost::Unpriced(Vec::new())
        };

        // Act
        query_with(
            &mut db,
            &sliced_spec(GroupDim::Model),
            recording,
            ReadLimit::from(no_deadline()),
            &forced(4),
        )
        .expect("query");

        // Assert
        assert_eq!(ledger_avg.len(), 1, "{case}: one fine row");
        assert_eq!(seen.into_inner(), ledger_avg, "{case}");
    }
}

/// One `slice_ranges` case: its name, the window's lower bound, its earliest
/// row, its upper bound, a forced count, and the ranges expected.
type RangeCase = (
    &'static str,
    i64,
    Option<i64>,
    i64,
    Option<usize>,
    Vec<(i64, i64)>,
);

#[test]
fn slice_ranges_cut_the_effective_span_into_contiguous_ranges() {
    const T: i64 = 1_700_000_000_000;
    let quarter_week = WEEK_MS / 4;
    let cases: [RangeCase; 6] = [
        (
            "a span under a week is one range",
            0,
            Some(0),
            WEEK_MS - 1,
            None,
            vec![(0, WEEK_MS - 1)],
        ),
        (
            "a span of exactly a week is four",
            0,
            Some(0),
            WEEK_MS,
            None,
            vec![
                (0, quarter_week),
                (quarter_week, 2 * quarter_week),
                (2 * quarter_week, 3 * quarter_week),
                (3 * quarter_week, WEEK_MS),
            ],
        ),
        (
            "an all-time window is cut from its earliest row",
            0,
            Some(T),
            T + 4 * WEEK_MS,
            None,
            vec![
                (0, T + WEEK_MS),
                (T + WEEK_MS, T + 2 * WEEK_MS),
                (T + 2 * WEEK_MS, T + 3 * WEEK_MS),
                (T + 3 * WEEK_MS, T + 4 * WEEK_MS),
            ],
        ),
        (
            "a long window with only a recent row is one range",
            T,
            Some(T + 4 * WEEK_MS - DAY_MS),
            T + 4 * WEEK_MS,
            None,
            vec![(T, T + 4 * WEEK_MS)],
        ),
        (
            "a window with no row is one range",
            0,
            None,
            T,
            None,
            vec![(0, T)],
        ),
        (
            "a forced count cuts any span",
            0,
            Some(0),
            SLICED_TO_MS,
            Some(4),
            vec![
                (0, 10_000),
                (10_000, 20_000),
                (20_000, 30_000),
                (30_000, 40_000),
            ],
        ),
    ];
    for (case, from_ms, earliest, to_ms, count, expected) in cases {
        assert_eq!(
            crate::query::slices::slice_ranges(from_ms, earliest, to_ms, count),
            expected,
            "{case}"
        );
    }
}

#[test]
fn rows_on_slice_edges_and_in_empty_slices_merge_exactly() {
    let grid = Some(BucketSpec {
        width_ms: 3_000,
        count: 14,
    });
    let cases: [(&str, &[i64]); 3] = [
        (
            "rows on and around an edge",
            &[0, 9_999, 10_000, 10_001, 39_999],
        ),
        ("two empty middle slices", &[0, 35_000]),
        ("a one-row ledger", &[0]),
    ];
    for (case, timestamps) in cases {
        // Arrange
        let (_dir, mut db) = open_db();
        for (i, &ts_start) in timestamps.iter().enumerate() {
            let row = DenseRow {
                input_tokens: Some(10 + i as i64),
                ..DenseRow::new(format!("edge-{i}"), ts_start, &LANES[0])
            };
            insert_dense(db.conn(), &row);
        }
        let spec = QuerySpec {
            bucket: grid,
            ..sliced_spec(GroupDim::Model)
        };

        // Act
        let one = read(&mut db, &spec, &forced(1));
        let four = read(&mut db, &spec, &forced(4));

        // Assert: every row counted once, and the bucket the edge at 10 000
        // cuts ([9 000, 12 000)) sums its two halves.
        assert_eq!(four.totals.requests, timestamps.len() as i64, "{case}");
        let cut = timestamps
            .iter()
            .filter(|&&ts| (9_000..12_000).contains(&ts))
            .count() as i64;
        let series = four.series.as_ref().expect("series");
        assert_eq!(series.buckets[3].metrics.requests, cut, "{case}");
        assert_eq!(four, one, "{case}");
    }
}

#[test]
fn only_a_span_of_a_week_or_more_from_the_earliest_row_is_sliced() {
    const T: i64 = 1_700_000_000_000;
    let cases: [(&str, &[i64], i64, usize); 4] = [
        (
            "six days of rows, all time",
            &[T, T + 6 * DAY_MS],
            T + 6 * DAY_MS + 1,
            0,
        ),
        ("one row a week before the end", &[T], T + WEEK_MS, 3),
        (
            "two weeks of rows, all time",
            &[T, T + 13 * DAY_MS],
            T + 14 * DAY_MS,
            3,
        ),
        ("an empty window", &[], T, 0),
    ];
    for (case, timestamps, to_ms, expected_opens) in cases {
        // Arrange
        let (dir, mut db) = open_db();
        for (i, &ts_start) in timestamps.iter().enumerate() {
            insert_dense(
                db.conn(),
                &DenseRow::new(format!("span-{i}"), ts_start, &LANES[0]),
            );
        }
        let path = dir.path().join("usage.db");
        let opens = std::sync::atomic::AtomicUsize::new(0);
        let counting = |_: usize, _: &ReadLimit| -> Result<UsageDb, QueryError> {
            opens.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(crate::db::open_readonly_fastfail(&path).expect("open slice"))
        };
        let auto_counting = ReadPlan {
            slices: None,
            watermark: None,
            open_slice: Some(&counting),
        };
        let spec = QuerySpec {
            to_ms,
            ..sliced_spec(GroupDim::Model)
        };

        // Act
        let auto = read(&mut db, &spec, &auto_counting);
        let one = read(&mut db, &spec, &forced(1));

        // Assert
        assert_eq!(
            opens.load(std::sync::atomic::Ordering::Relaxed),
            expected_opens,
            "{case}: slice connections opened"
        );
        assert_eq!(auto.totals.requests, timestamps.len() as i64, "{case}");
        assert_eq!(auto, one, "{case}");
    }
}

#[test]
fn a_row_above_the_watermark_does_not_change_how_the_window_is_cut() {
    // Arrange: six days of rows, too short a span to slice, then a late row
    // above the watermark whose old start time would stretch the span past a
    // week.
    const T: i64 = 1_700_000_000_000;
    let (dir, mut db) = open_db();
    for (i, ts_start) in [T, T + 6 * DAY_MS].into_iter().enumerate() {
        insert_dense(
            db.conn(),
            &DenseRow::new(format!("span-{i}"), ts_start, &LANES[0]),
        );
    }
    let spec = QuerySpec {
        to_ms: T + 6 * DAY_MS + 1,
        ..sliced_spec(GroupDim::Model)
    };
    let before = read(&mut db, &spec, &forced(1));
    let watermark = max_rowid(&db);
    insert_dense(
        db.conn(),
        &DenseRow::new("late-old".to_owned(), T - 30 * DAY_MS, &LANES[0]),
    );
    let path = dir.path().join("usage.db");
    let opens = std::sync::atomic::AtomicUsize::new(0);
    let counting = |_: usize, _: &ReadLimit| -> Result<UsageDb, QueryError> {
        opens.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(crate::db::open_readonly_fastfail(&path).expect("open slice"))
    };
    let plan_at = |watermark: Option<i64>| ReadPlan {
        slices: None,
        watermark,
        open_slice: Some(&counting),
    };

    // Act
    let cut = read(&mut db, &spec, &plan_at(Some(watermark)));
    let opened_at_watermark = opens.swap(0, std::sync::atomic::Ordering::Relaxed);
    let uncut = read(&mut db, &spec, &plan_at(None));
    let opened_above = opens.load(std::sync::atomic::Ordering::Relaxed);

    // Assert: the control shows the late row stretches the plan of a read
    // that counts it; the read below the watermark plans and answers as
    // before the row existed.
    assert_eq!(opened_above, 3, "control: a read counting the row");
    assert_eq!(uncut.totals.requests, 3, "control: the late row is visible");
    assert_eq!(opened_at_watermark, 0, "slice connections opened");
    assert_eq!(cut, before);
}

/// Late rows: one per lane in every slice, each an error with a class and a
/// cache-write split, so every statement of the read would see them.
fn insert_late_rows(conn: &rusqlite::Connection, tag: &str) {
    for slice in 0..4 {
        for (lane_ix, lane) in LANES.iter().enumerate() {
            let row = DenseRow {
                outcome: "timeout",
                resolved_class: Some("late-class"),
                input_tokens: Some(1_000_000),
                cache_write_tokens: Some(77),
                cache_write_lifetimes: Some(r#"{"60":77}"#),
                ..DenseRow::new(
                    format!("{tag}-s{slice}-l{lane_ix}"),
                    slice * SLICE_WIDTH_MS + 100,
                    lane,
                )
            };
            insert_dense(conn, &row);
        }
    }
}

fn max_rowid(db: &UsageDb) -> i64 {
    db.conn()
        .query_row("SELECT MAX(rowid) FROM requests", [], |r| r.get(0))
        .expect("max rowid")
}

#[test]
fn rows_appended_above_the_watermark_are_excluded_from_every_statement() {
    let grids = [
        None,
        Some(BucketSpec {
            width_ms: 3_000,
            count: 14,
        }),
    ];
    for bucket in grids {
        // Arrange: the answer before the late rows exist, then the late rows
        // with older start times than rows already read.
        let (_dir, mut db) = open_db();
        seed_dense(&db, "base");
        let spec = QuerySpec {
            bucket,
            ..sliced_spec(GroupDim::Model)
        };
        let before = read(&mut db, &spec, &forced(1));
        let watermark = max_rowid(&db);
        insert_late_rows(db.conn(), "late");
        let at_watermark = ReadPlan {
            slices: Some(4),
            watermark: Some(watermark),
            open_slice: None,
        };

        // Act
        let cut = read(&mut db, &spec, &at_watermark);
        let uncut = read(&mut db, &spec, &forced(4));

        // Assert: the control shows the late rows are visible to an uncut
        // read, and the cut read equals the read made before they existed.
        assert_ne!(uncut.totals.requests, before.totals.requests, "{bucket:?}");
        assert_eq!(cut, before, "{bucket:?}");
    }
}

#[test]
fn the_read_takes_its_watermark_before_any_slice_reads() {
    // Arrange: the second slice appends rows into every slice's range just
    // before it opens its own connection, so its snapshot holds them.
    let (dir, mut db) = open_db();
    seed_dense(&db, "base");
    let path = dir.path().join("usage.db");
    let spec = sliced_spec(GroupDim::Model);
    let before = read(&mut db, &spec, &forced(1));
    let appending = |index: usize, _: &ReadLimit| -> Result<UsageDb, QueryError> {
        if index == 1 {
            let writer = rusqlite::Connection::open(&path).expect("open writer");
            writer
                .busy_timeout(Duration::from_secs(5))
                .expect("busy timeout");
            insert_late_rows(&writer, "late");
        }
        Ok(crate::db::open_readonly_fastfail(&path).expect("open slice"))
    };
    let plan = ReadPlan {
        slices: Some(4),
        watermark: None,
        open_slice: Some(&appending),
    };

    // Act
    let during = read(&mut db, &spec, &plan);
    let after = read(&mut db, &spec, &forced(4));

    // Assert
    assert_eq!(during, before);
    assert_eq!(
        after.totals.requests,
        before.totals.requests + 16,
        "the appended rows are there for the next read"
    );
}

/// Bulk rows spread over all four slices, enough that each slice's aggregate
/// runs past a progress-callback interval.
fn seed_bulk_sliced(db: &UsageDb, per_slice: i64) {
    db.conn().execute_batch("BEGIN").expect("begin");
    for slice in 0..4 {
        for i in 0..per_slice {
            let row = DenseRow {
                input_tokens: Some(i),
                ..DenseRow::new(
                    format!("bulk-{slice}-{i}"),
                    slice * SLICE_WIDTH_MS + i % SLICE_WIDTH_MS,
                    &LANES[(i % 4) as usize],
                )
            };
            insert_dense(db.conn(), &row);
        }
    }
    db.conn().execute_batch("COMMIT").expect("commit");
}

/// Block until `limit` is stopped (its flag set or its deadline passed),
/// bounded so a broken stop cannot hang the suite. Whether it was stopped.
fn wait_for_stop(limit: &ReadLimit) -> bool {
    let bound = Instant::now() + Duration::from_secs(10);
    while Instant::now() < bound {
        if limit.cancel.is_cancelled() || Instant::now() >= limit.deadline {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

#[test]
fn a_stopped_read_is_interrupted_only_after_every_slice_has_ended() {
    let cases = [
        ("cancelled mid-read", true),
        ("deadline expired mid-read", false),
    ];
    for (case, cancels) in cases {
        // Arrange: the last slice stops the read itself (by setting the
        // caller's flag, or by waiting out a short deadline), then keeps its
        // thread busy well past the stop before it reads.
        let (dir, mut db) = open_db();
        seed_bulk_sliced(&db, 3_000);
        let path = dir.path().join("usage.db");
        let caller = CancelFlag::default();
        let limit = ReadLimit {
            deadline: if cancels {
                no_deadline()
            } else {
                Instant::now() + Duration::from_millis(300)
            },
            cancel: caller.clone(),
        };
        let last_ended = std::sync::atomic::AtomicBool::new(false);
        let hook = |index: usize, slice_limit: &ReadLimit| -> Result<UsageDb, QueryError> {
            if index == 3 {
                if cancels {
                    caller.cancel();
                }
                assert!(
                    wait_for_stop(slice_limit),
                    "{case}: the last slice saw no stop"
                );
                std::thread::sleep(Duration::from_millis(50));
                last_ended.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(crate::db::open_readonly_fastfail(&path).expect("open slice"))
        };
        let plan = ReadPlan {
            slices: Some(4),
            watermark: None,
            open_slice: Some(&hook),
        };

        // Act
        let stopped = query_with(
            &mut db,
            &sliced_spec(GroupDim::Model),
            unpriced,
            limit,
            &plan,
        );
        let ended_before_return = last_ended.load(std::sync::atomic::Ordering::SeqCst);
        let unbounded = crate::query::aggregate(&db, 0, SLICED_TO_MS);

        // Assert: interrupted, joined, and the caller's connection carries no
        // handler afterwards (an expired one would interrupt this scan too).
        assert!(
            matches!(stopped, Err(QueryError::Interrupted)),
            "{case}: expected an interrupt, got {stopped:?}"
        );
        assert!(
            ended_before_return,
            "{case}: returned before the last slice ended"
        );
        let scanned = unbounded.expect("an unbounded read after the stopped one");
        assert_eq!(
            scanned.iter().map(|r| r.requests).sum::<i64>(),
            12_000,
            "{case}"
        );
    }
}

#[test]
fn a_real_slice_error_wins_over_its_siblings_interrupts() {
    for failing in [1, 3] {
        // Arrange: one slice fails to open with an I/O error; every other
        // slice after the first that gets as far as opening waits for the
        // stop first, so its own answer is an interrupt either way.
        let (dir, mut db) = open_db();
        seed_dense(&db, "base");
        let path = dir.path().join("usage.db");
        let siblings_opened = std::sync::atomic::AtomicUsize::new(0);
        let siblings_stopped = std::sync::atomic::AtomicUsize::new(0);
        let hook = |index: usize, slice_limit: &ReadLimit| -> Result<UsageDb, QueryError> {
            if index == failing {
                return Err(QueryError::Sqlite(rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
                    None,
                )));
            }
            siblings_opened.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if wait_for_stop(slice_limit) {
                siblings_stopped.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            Ok(crate::db::open_readonly_fastfail(&path).expect("open slice"))
        };
        let plan = ReadPlan {
            slices: Some(4),
            watermark: None,
            open_slice: Some(&hook),
        };
        let caller = CancelFlag::default();
        let limit = ReadLimit {
            deadline: no_deadline(),
            cancel: caller.clone(),
        };

        // Act
        let failed = query_with(
            &mut db,
            &sliced_spec(GroupDim::Model),
            unpriced,
            limit.clone(),
            &plan,
        );
        let again = query_with(
            &mut db,
            &sliced_spec(GroupDim::Model),
            unpriced,
            limit,
            &forced(4),
        );

        // Assert: the I/O error is the answer, both siblings were stopped, and
        // the caller's own flag and connection are left usable.
        let code = match &failed {
            Err(QueryError::Sqlite(err)) => err.sqlite_error_code(),
            other => panic!("slice {failing}: expected the I/O error, got {other:?}"),
        };
        assert_eq!(
            code,
            Some(rusqlite::ErrorCode::SystemIoFailure),
            "slice {failing}"
        );
        assert_eq!(
            siblings_stopped.load(std::sync::atomic::Ordering::SeqCst),
            siblings_opened.load(std::sync::atomic::Ordering::SeqCst),
            "slice {failing}: every sibling that opened was stopped"
        );
        assert!(
            !caller.is_cancelled(),
            "slice {failing}: the caller's flag was set"
        );
        assert_eq!(
            again.expect("a read after the failed one").totals.requests,
            96
        );
    }
}

#[test]
fn a_panicking_slice_stops_an_earlier_blocked_slice_at_once() {
    for (blocked, panicking) in [(1, 3), (1, 2)] {
        // Arrange: an earlier slice waits for the stop (bounded well past the
        // promptness bound below), and a later one panics opening its
        // connection once the earlier one is waiting. The caller joins in
        // range order, so it reaches the panicking slice only after the
        // blocked one has ended.
        let (dir, mut db) = open_db();
        seed_dense(&db, "base");
        let path = dir.path().join("usage.db");
        let blocked_waiting = std::sync::atomic::AtomicBool::new(false);
        let blocked_saw_stop = std::sync::atomic::AtomicBool::new(false);
        let hook = |index: usize, slice_limit: &ReadLimit| -> Result<UsageDb, QueryError> {
            if index == panicking {
                let bound = Instant::now() + Duration::from_secs(10);
                while !blocked_waiting.load(std::sync::atomic::Ordering::SeqCst)
                    && Instant::now() < bound
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                panic!("slice {index} failed");
            }
            if index == blocked {
                blocked_waiting.store(true, std::sync::atomic::Ordering::SeqCst);
                if wait_for_stop(slice_limit) {
                    blocked_saw_stop.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            Ok(crate::db::open_readonly_fastfail(&path).expect("open slice"))
        };
        let plan = ReadPlan {
            slices: Some(4),
            watermark: None,
            open_slice: Some(&hook),
        };
        let caller = CancelFlag::default();
        let limit = ReadLimit {
            deadline: no_deadline(),
            cancel: caller.clone(),
        };
        let started = Instant::now();

        // Act
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            query_with(
                &mut db,
                &sliced_spec(GroupDim::Model),
                unpriced,
                limit,
                &plan,
            )
        }));
        let elapsed = started.elapsed();
        let after = read(&mut db, &sliced_spec(GroupDim::Model), &forced(4));

        // Assert: the panic reaches the caller, the blocked slice was stopped
        // rather than timing out, the caller's flag is untouched, and its
        // connection reads on.
        let payload = outcome.expect_err("the slice's panic reaches the caller");
        assert_eq!(
            payload.downcast_ref::<String>().map(String::as_str),
            Some(format!("slice {panicking} failed").as_str()),
        );
        assert!(
            blocked_saw_stop.load(std::sync::atomic::Ordering::SeqCst),
            "slice {blocked} was never stopped"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "slice {panicking}: the read took {elapsed:?} to end"
        );
        assert!(!caller.is_cancelled(), "slice {panicking}");
        assert_eq!(after.totals.requests, 96, "slice {panicking}");
    }
}
