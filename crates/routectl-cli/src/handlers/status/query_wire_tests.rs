// The contracts sec-15 wire-shape pins for `/status/query`: the aggregate and
// bucketed exact-JSON drift tests, the null-not-skipped / cost-token rule, and
// the pricer snapshot pin. Split from `query_tests.rs` to keep each file under
// the size ceiling; `include!`d into the same `tests` module so the helpers
// there stay in scope. All imports come from the host `query_tests.rs` (its
// `use super::*`); do not add `use` lines here.

/// The DRIFT TEST. A fully-populated response is serialized and compared to
/// exact JSON, so every metric name, every `cost_status` token, and the
/// envelope shape are pinned: renaming or dropping any of them fails here.
#[test]
fn wire_shape_pins_every_metric_token() {
    fn metrics(
        offset: i64,
        status: CostStatus,
        cost: Option<f64>,
        equivalent: Option<f64>,
    ) -> QueryMetrics {
        QueryMetrics {
            requests: 100 + offset,
            ok: 90 + offset,
            errors: 9 + offset,
            input_tokens: 1_000 + offset,
            output_tokens: 2_000 + offset,
            reasoning_tokens: 300 + offset,
            cache_read_billed: 4_000 + offset,
            cache_write_5m: 50 + offset,
            cache_write_1h: 60 + offset,
            cache_write_tokens: 1060 + offset,
            cache_write_present: 6 + offset,
            cache_read_present: 20 + offset,
            server_tool_calls: 7 + offset,
            server_tool_present: 30 + offset,
            stream_count: 80 + offset,
            client_disconnect_total: 1 + offset,
            client_disconnect_pre_dispatch: offset,
            cancelled_total: 3 + offset,
            fallback_served: 5 + offset,
            errors_by_class: std::collections::BTreeMap::from([
                ("http-5xx".to_string(), 5 + offset),
                ("unclassified".to_string(), 4),
            ]),
            ttft_avg_ms: Some(120 + offset),
            ttft_max_ms: Some(450 + offset),
            ttft_p50_ms: Some(100 + offset),
            ttft_p90_ms: Some(300 + offset),
            ttft_p99_ms: Some(440 + offset),
            latency_avg_ms: Some(900 + offset),
            latency_max_ms: Some(3_100 + offset),
            latency_p50_ms: Some(700 + offset),
            latency_p90_ms: Some(2_500 + offset),
            latency_p99_ms: Some(3_000 + offset),
            throughput_tok_s: Some(42.5),
            ctx_avg: Some(1_200 + offset),
            ctx_peak: Some(8_400 + offset),
            cache_hit_pct: Some(63.25),
            cost_usd: cost,
            equivalent_cost_usd: equivalent,
            cost_status: status,
            missing_price_dimensions: if status == CostStatus::Partial {
                vec!["cache write 30m".to_string()]
            } else {
                Vec::new()
            },
        }
    }

    let result = QueryResult {
        groups: vec![QueryGroup {
            label: "sonnet".to_string(),
            metrics: metrics(0, CostStatus::Priced, Some(1.25), None),
        }],
        totals: metrics(1, CostStatus::Partial, Some(1.25), Some(3.5)),
        series: None,
    };
    let panel = Panel::available(
        SCHEMA_VERSION,
        "2026-08-02T00:00:00Z".to_string(),
        name_groups(result, GroupDim::Model, &AccountIdentities::default()),
    );

    // Built in parts: one literal for the whole envelope exceeds the
    // `json!` macro recursion limit.
    let group_metrics = serde_json::json!({
        "requests": 100,
        "ok": 90,
        "errors": 9,
        "input_tokens": 1000,
        "output_tokens": 2000,
        "reasoning_tokens": 300,
        "cache_read_billed": 4000,
        "cache_write_5m": 50,
        "cache_write_1h": 60,
        "cache_write_tokens": 1060,
        "cache_write_present": 6,
        "cache_read_present": 20,
        "server_tool_calls": 7,
        "server_tool_present": 30,
        "stream_count": 80,
        "client_disconnect_total": 1,
        "client_disconnect_pre_dispatch": 0,
        "cancelled_total": 3,
        "fallback_served": 5,
        "errors_by_class": {"http-5xx": 5, "unclassified": 4},
        "ttft_avg_ms": 120,
        "ttft_max_ms": 450,
        "ttft_p50_ms": 100,
        "ttft_p90_ms": 300,
        "ttft_p99_ms": 440,
        "latency_avg_ms": 900,
        "latency_max_ms": 3100,
        "latency_p50_ms": 700,
        "latency_p90_ms": 2500,
        "latency_p99_ms": 3000,
        "throughput_tok_s": 42.5,
        "ctx_avg": 1200,
        "ctx_peak": 8400,
        "cache_hit_pct": 63.25,
        "cost_usd": 1.25,
        "equivalent_cost_usd": null,
        "cost_status": "priced",
        "missing_price_dimensions": [],
    });
    let totals = serde_json::json!({
        "requests": 101,
        "ok": 91,
        "errors": 10,
        "input_tokens": 1001,
        "output_tokens": 2001,
        "reasoning_tokens": 301,
        "cache_read_billed": 4001,
        "cache_write_5m": 51,
        "cache_write_1h": 61,
        "cache_write_tokens": 1061,
        "cache_write_present": 7,
        "cache_read_present": 21,
        "server_tool_calls": 8,
        "server_tool_present": 31,
        "stream_count": 81,
        "client_disconnect_total": 2,
        "client_disconnect_pre_dispatch": 1,
        "cancelled_total": 4,
        "fallback_served": 6,
        "errors_by_class": {"http-5xx": 6, "unclassified": 4},
        "ttft_avg_ms": 121,
        "ttft_max_ms": 451,
        "ttft_p50_ms": 101,
        "ttft_p90_ms": 301,
        "ttft_p99_ms": 441,
        "latency_avg_ms": 901,
        "latency_max_ms": 3101,
        "latency_p50_ms": 701,
        "latency_p90_ms": 2501,
        "latency_p99_ms": 3001,
        "throughput_tok_s": 42.5,
        "ctx_avg": 1201,
        "ctx_peak": 8401,
        "cache_hit_pct": 63.25,
        "cost_usd": 1.25,
        "equivalent_cost_usd": 3.5,
        "cost_status": "partial",
        "missing_price_dimensions": ["cache write 30m"],
    });
    let expected = serde_json::json!({
        "schema_version": 5,
        "as_of": "2026-08-02T00:00:00Z",
        "unavailable": null,
        "data": {
            "groups": [{
                "label": "sonnet",
                "metrics": group_metrics
            }],
            "totals": totals,
            "series": null,
        }
    });

    assert_eq!(serde_json::to_value(&panel).unwrap(), expected);
}

/// The SERIES DRIFT TEST. The bucketed shape gets its own exact-JSON snapshot
/// beside the aggregate one: `series`, `bucket_ms`, `buckets`, `start_ms`, and
/// every per-bucket metric token are pinned, so renaming or dropping any of
/// them fails here. The second bucket is a zero-traffic one, which also pins
/// that an untravelled bucket reports an honest `requests: 0` with explicit
/// nulls rather than being skipped or fabricated.
#[test]
fn bucketed_wire_shape_pins_the_series_tokens() {
    fn metrics(
        offset: i64,
        status: CostStatus,
        cost: Option<f64>,
        equivalent: Option<f64>,
    ) -> QueryMetrics {
        QueryMetrics {
            requests: 40 + offset,
            ok: 31 + offset,
            errors: 8 + offset,
            input_tokens: 5_000 + offset,
            output_tokens: 6_000 + offset,
            reasoning_tokens: 700 + offset,
            cache_read_billed: 8_000 + offset,
            cache_write_5m: 90 + offset,
            cache_write_1h: 110 + offset,
            cache_write_tokens: 1110 + offset,
            cache_write_present: 11 + offset,
            cache_read_present: 20 + offset,
            server_tool_calls: 13 + offset,
            server_tool_present: 30 + offset,
            stream_count: 21 + offset,
            client_disconnect_total: 2 + offset,
            client_disconnect_pre_dispatch: offset,
            cancelled_total: 4 + offset,
            fallback_served: 3 + offset,
            errors_by_class: std::collections::BTreeMap::from([
                ("timeout".to_string(), 6 + offset),
                ("unclassified".to_string(), 2),
            ]),
            ttft_avg_ms: Some(210 + offset),
            ttft_max_ms: Some(640 + offset),
            ttft_p50_ms: Some(180 + offset),
            ttft_p90_ms: Some(500 + offset),
            ttft_p99_ms: Some(630 + offset),
            latency_avg_ms: Some(1_500 + offset),
            latency_max_ms: Some(4_700 + offset),
            latency_p50_ms: Some(1_200 + offset),
            latency_p90_ms: Some(4_000 + offset),
            latency_p99_ms: Some(4_600 + offset),
            throughput_tok_s: Some(31.5),
            ctx_avg: Some(2_400 + offset),
            ctx_peak: Some(9_600 + offset),
            cache_hit_pct: Some(17.75),
            cost_usd: cost,
            equivalent_cost_usd: equivalent,
            cost_status: status,
            missing_price_dimensions: if status == CostStatus::Partial {
                vec!["cache write 30m".to_string()]
            } else {
                Vec::new()
            },
        }
    }

    let result = QueryResult {
        groups: vec![QueryGroup {
            label: "sonnet".to_string(),
            metrics: metrics(0, CostStatus::Priced, Some(2.5), None),
        }],
        totals: metrics(1, CostStatus::Partial, Some(2.5), Some(4.25)),
        series: Some(QuerySeries {
            bucket_ms: 3_600_000,
            buckets: vec![
                SeriesBucket {
                    start_ms: 1_000_000,
                    metrics: metrics(2, CostStatus::Priced, Some(0.75), None),
                },
                SeriesBucket {
                    start_ms: 4_600_000,
                    metrics: QueryMetrics::default(),
                },
            ],
        }),
    };
    let panel = Panel::available(
        SCHEMA_VERSION,
        "2026-08-02T00:00:00Z".to_string(),
        name_groups(result, GroupDim::Model, &AccountIdentities::default()),
    );

    let first_bucket = serde_json::json!({
        "start_ms": 1000000,
        "metrics": {
            "requests": 42,
            "ok": 33,
            "errors": 10,
            "input_tokens": 5002,
            "output_tokens": 6002,
            "reasoning_tokens": 702,
            "cache_read_billed": 8002,
            "cache_write_5m": 92,
            "cache_write_1h": 112,
            "cache_write_tokens": 1112,
            "cache_write_present": 13,
            "cache_read_present": 22,
            "server_tool_calls": 15,
            "server_tool_present": 32,
            "stream_count": 23,
            "client_disconnect_total": 4,
            "client_disconnect_pre_dispatch": 2,
            "cancelled_total": 6,
            "fallback_served": 5,
            "errors_by_class": {"timeout": 8, "unclassified": 2},
            "ttft_avg_ms": 212,
            "ttft_max_ms": 642,
            "ttft_p50_ms": 182,
            "ttft_p90_ms": 502,
            "ttft_p99_ms": 632,
            "latency_avg_ms": 1502,
            "latency_max_ms": 4702,
            "latency_p50_ms": 1202,
            "latency_p90_ms": 4002,
            "latency_p99_ms": 4602,
            "throughput_tok_s": 31.5,
            "ctx_avg": 2402,
            "ctx_peak": 9602,
            "cache_hit_pct": 17.75,
            "cost_usd": 0.75,
            "equivalent_cost_usd": null,
            "cost_status": "priced",
            "missing_price_dimensions": [],
        }
    });
    let empty_bucket = serde_json::json!({
        "start_ms": 4600000,
        "metrics": {
            "requests": 0,
            "ok": 0,
            "errors": 0,
            "input_tokens": 0,
            "output_tokens": 0,
            "reasoning_tokens": 0,
            "cache_read_billed": 0,
            "cache_write_5m": 0,
            "cache_write_1h": 0,
            "cache_write_tokens": 0,
            "cache_write_present": 0,
            "cache_read_present": 0,
            "server_tool_calls": 0,
            "server_tool_present": 0,
            "stream_count": 0,
            "client_disconnect_total": 0,
            "client_disconnect_pre_dispatch": 0,
            "cancelled_total": 0,
            "fallback_served": 0,
            "errors_by_class": {},
            "ttft_avg_ms": null,
            "ttft_max_ms": null,
            "ttft_p50_ms": null,
            "ttft_p90_ms": null,
            "ttft_p99_ms": null,
            "latency_avg_ms": null,
            "latency_max_ms": null,
            "latency_p50_ms": null,
            "latency_p90_ms": null,
            "latency_p99_ms": null,
            "throughput_tok_s": null,
            "ctx_avg": null,
            "ctx_peak": null,
            "cache_hit_pct": null,
            "cost_usd": null,
            "equivalent_cost_usd": null,
            "cost_status": "unpriced",
            "missing_price_dimensions": [],
        }
    });

    let group_metrics = serde_json::json!({
        "requests": 40,
        "ok": 31,
        "errors": 8,
        "input_tokens": 5000,
        "output_tokens": 6000,
        "reasoning_tokens": 700,
        "cache_read_billed": 8000,
        "cache_write_5m": 90,
        "cache_write_1h": 110,
        "cache_write_tokens": 1110,
        "cache_write_present": 11,
        "cache_read_present": 20,
        "server_tool_calls": 13,
        "server_tool_present": 30,
        "stream_count": 21,
        "client_disconnect_total": 2,
        "client_disconnect_pre_dispatch": 0,
        "cancelled_total": 4,
        "fallback_served": 3,
        "errors_by_class": {"timeout": 6, "unclassified": 2},
        "ttft_avg_ms": 210,
        "ttft_max_ms": 640,
        "ttft_p50_ms": 180,
        "ttft_p90_ms": 500,
        "ttft_p99_ms": 630,
        "latency_avg_ms": 1500,
        "latency_max_ms": 4700,
        "latency_p50_ms": 1200,
        "latency_p90_ms": 4000,
        "latency_p99_ms": 4600,
        "throughput_tok_s": 31.5,
        "ctx_avg": 2400,
        "ctx_peak": 9600,
        "cache_hit_pct": 17.75,
        "cost_usd": 2.5,
        "equivalent_cost_usd": null,
        "cost_status": "priced",
        "missing_price_dimensions": [],
    });
    let totals = serde_json::json!({
        "requests": 41,
        "ok": 32,
        "errors": 9,
        "input_tokens": 5001,
        "output_tokens": 6001,
        "reasoning_tokens": 701,
        "cache_read_billed": 8001,
        "cache_write_5m": 91,
        "cache_write_1h": 111,
        "cache_write_tokens": 1111,
        "cache_write_present": 12,
        "cache_read_present": 21,
        "server_tool_calls": 14,
        "server_tool_present": 31,
        "stream_count": 22,
        "client_disconnect_total": 3,
        "client_disconnect_pre_dispatch": 1,
        "cancelled_total": 5,
        "fallback_served": 4,
        "errors_by_class": {"timeout": 7, "unclassified": 2},
        "ttft_avg_ms": 211,
        "ttft_max_ms": 641,
        "ttft_p50_ms": 181,
        "ttft_p90_ms": 501,
        "ttft_p99_ms": 631,
        "latency_avg_ms": 1501,
        "latency_max_ms": 4701,
        "latency_p50_ms": 1201,
        "latency_p90_ms": 4001,
        "latency_p99_ms": 4601,
        "throughput_tok_s": 31.5,
        "ctx_avg": 2401,
        "ctx_peak": 9601,
        "cache_hit_pct": 17.75,
        "cost_usd": 2.5,
        "equivalent_cost_usd": 4.25,
        "cost_status": "partial",
        "missing_price_dimensions": ["cache write 30m"],
    });
    let expected = serde_json::json!({
        "schema_version": 5,
        "as_of": "2026-08-02T00:00:00Z",
        "unavailable": null,
        "data": {
            "groups": [{
                "label": "sonnet",
                "metrics": group_metrics
            }],
            "totals": totals,
            "series": {
                "bucket_ms": 3600000,
                "buckets": [first_bucket, empty_bucket]
            },
        }
    });

    assert_eq!(serde_json::to_value(&panel).unwrap(), expected);
}

/// The two remaining `cost_status` tokens (the drift test above pins `priced`
/// and `partial`), plus the null-not-skipped rule for every absent metric.
#[test]
fn absent_metrics_serialize_as_null_and_the_cost_tokens_are_stable() {
    for (status, token) in [
        (CostStatus::Unpriced, "unpriced"),
        (CostStatus::Subscription, "subscription"),
    ] {
        let metrics = QueryMetrics {
            cost_status: status,
            ..QueryMetrics::default()
        };
        let json = serde_json::to_value(&metrics).unwrap();
        assert_eq!(json["cost_status"], token);
        for name in [
            "ttft_avg_ms",
            "ttft_max_ms",
            "ttft_p50_ms",
            "ttft_p90_ms",
            "ttft_p99_ms",
            "latency_avg_ms",
            "latency_max_ms",
            "latency_p50_ms",
            "latency_p90_ms",
            "latency_p99_ms",
            "throughput_tok_s",
            "ctx_avg",
            "ctx_peak",
            "cache_hit_pct",
            "cost_usd",
            "equivalent_cost_usd",
        ] {
            assert_eq!(
                json[name],
                Value::Null,
                "{name} must serialize as an explicit null, not be skipped"
            );
        }
    }
}

/// The pricer pins ONE config snapshot per request: the closure the query layer
/// calls resolves against the snapshot taken at `pricer()`, so a router
/// hot-swap mid-query cannot make two rows price differently.
#[tokio::test]
async fn the_pricer_resolves_against_one_pinned_snapshot() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    seed_ledger(
        &path,
        &[crate::local_time::unix_ms(time::OffsetDateTime::now_utc())],
    );
    let state = state_with_ledger(path.clone());

    let pricer = state.pricer().pin();
    let mut db = open_readonly_fastfail(&path).expect("open seeded ledger");
    let spec = parse_spec(VALID_BODY.as_bytes(), OffsetDateTime::now_utc()).unwrap();

    let first = query(&mut db, &spec, |row| pricer.price(row), far_deadline()).unwrap();
    let second = query(&mut db, &spec, |row| pricer.price(row), far_deadline()).unwrap();
    assert_eq!(first, second);
    // The default config carries no `[models.X.pricing]`, so a real row is
    // honestly unpriced rather than costed at zero.
    assert_eq!(first.totals.cost_status, CostStatus::Unpriced);
    assert!(first.totals.cost_usd.is_none());
}
