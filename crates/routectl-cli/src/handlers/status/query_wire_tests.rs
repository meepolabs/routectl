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
            server_tool_calls: 7 + offset,
            stream_count: 80 + offset,
            client_disconnect_total: 1 + offset,
            fallback_served: 5 + offset,
            ttft_p50_ms: Some(120 + offset),
            ttft_p95_ms: Some(450 + offset),
            latency_p50_ms: Some(900 + offset),
            latency_p95_ms: Some(3_100 + offset),
            throughput_tok_s: Some(42.5),
            ctx_avg: Some(1_200 + offset),
            ctx_peak: Some(8_400 + offset),
            cache_hit_pct: Some(63.25),
            cost_usd: cost,
            equivalent_cost_usd: equivalent,
            cost_status: status,
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
    let panel = Panel::available(SCHEMA_VERSION, "2026-08-02T00:00:00Z".to_string(), result);

    let expected = serde_json::json!({
        "schema_version": 1,
        "as_of": "2026-08-02T00:00:00Z",
        "unavailable": null,
        "data": {
            "groups": [{
                "label": "sonnet",
                "metrics": {
                    "requests": 100,
                    "ok": 90,
                    "errors": 9,
                    "input_tokens": 1000,
                    "output_tokens": 2000,
                    "reasoning_tokens": 300,
                    "cache_read_billed": 4000,
                    "cache_write_5m": 50,
                    "cache_write_1h": 60,
                    "server_tool_calls": 7,
                    "stream_count": 80,
                    "client_disconnect_total": 1,
                    "fallback_served": 5,
                    "ttft_p50_ms": 120,
                    "ttft_p95_ms": 450,
                    "latency_p50_ms": 900,
                    "latency_p95_ms": 3100,
                    "throughput_tok_s": 42.5,
                    "ctx_avg": 1200,
                    "ctx_peak": 8400,
                    "cache_hit_pct": 63.25,
                    "cost_usd": 1.25,
                    "equivalent_cost_usd": null,
                    "cost_status": "priced",
                }
            }],
            "totals": {
                "requests": 101,
                "ok": 91,
                "errors": 10,
                "input_tokens": 1001,
                "output_tokens": 2001,
                "reasoning_tokens": 301,
                "cache_read_billed": 4001,
                "cache_write_5m": 51,
                "cache_write_1h": 61,
                "server_tool_calls": 8,
                "stream_count": 81,
                "client_disconnect_total": 2,
                "fallback_served": 6,
                "ttft_p50_ms": 121,
                "ttft_p95_ms": 451,
                "latency_p50_ms": 901,
                "latency_p95_ms": 3101,
                "throughput_tok_s": 42.5,
                "ctx_avg": 1201,
                "ctx_peak": 8401,
                "cache_hit_pct": 63.25,
                "cost_usd": 1.25,
                "equivalent_cost_usd": 3.5,
                "cost_status": "partial",
            },
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
            server_tool_calls: 13 + offset,
            stream_count: 21 + offset,
            client_disconnect_total: 2 + offset,
            fallback_served: 3 + offset,
            ttft_p50_ms: Some(210 + offset),
            ttft_p95_ms: Some(640 + offset),
            latency_p50_ms: Some(1_500 + offset),
            latency_p95_ms: Some(4_700 + offset),
            throughput_tok_s: Some(31.5),
            ctx_avg: Some(2_400 + offset),
            ctx_peak: Some(9_600 + offset),
            cache_hit_pct: Some(17.75),
            cost_usd: cost,
            equivalent_cost_usd: equivalent,
            cost_status: status,
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
    let panel = Panel::available(SCHEMA_VERSION, "2026-08-02T00:00:00Z".to_string(), result);

    let expected = serde_json::json!({
        "schema_version": 1,
        "as_of": "2026-08-02T00:00:00Z",
        "unavailable": null,
        "data": {
            "groups": [{
                "label": "sonnet",
                "metrics": {
                    "requests": 40,
                    "ok": 31,
                    "errors": 8,
                    "input_tokens": 5000,
                    "output_tokens": 6000,
                    "reasoning_tokens": 700,
                    "cache_read_billed": 8000,
                    "cache_write_5m": 90,
                    "cache_write_1h": 110,
                    "server_tool_calls": 13,
                    "stream_count": 21,
                    "client_disconnect_total": 2,
                    "fallback_served": 3,
                    "ttft_p50_ms": 210,
                    "ttft_p95_ms": 640,
                    "latency_p50_ms": 1500,
                    "latency_p95_ms": 4700,
                    "throughput_tok_s": 31.5,
                    "ctx_avg": 2400,
                    "ctx_peak": 9600,
                    "cache_hit_pct": 17.75,
                    "cost_usd": 2.5,
                    "equivalent_cost_usd": null,
                    "cost_status": "priced",
                }
            }],
            "totals": {
                "requests": 41,
                "ok": 32,
                "errors": 9,
                "input_tokens": 5001,
                "output_tokens": 6001,
                "reasoning_tokens": 701,
                "cache_read_billed": 8001,
                "cache_write_5m": 91,
                "cache_write_1h": 111,
                "server_tool_calls": 14,
                "stream_count": 22,
                "client_disconnect_total": 3,
                "fallback_served": 4,
                "ttft_p50_ms": 211,
                "ttft_p95_ms": 641,
                "latency_p50_ms": 1501,
                "latency_p95_ms": 4701,
                "throughput_tok_s": 31.5,
                "ctx_avg": 2401,
                "ctx_peak": 9601,
                "cache_hit_pct": 17.75,
                "cost_usd": 2.5,
                "equivalent_cost_usd": 4.25,
                "cost_status": "partial",
            },
            "series": {
                "bucket_ms": 3600000,
                "buckets": [
                    {
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
                            "server_tool_calls": 15,
                            "stream_count": 23,
                            "client_disconnect_total": 4,
                            "fallback_served": 5,
                            "ttft_p50_ms": 212,
                            "ttft_p95_ms": 642,
                            "latency_p50_ms": 1502,
                            "latency_p95_ms": 4702,
                            "throughput_tok_s": 31.5,
                            "ctx_avg": 2402,
                            "ctx_peak": 9602,
                            "cache_hit_pct": 17.75,
                            "cost_usd": 0.75,
                            "equivalent_cost_usd": null,
                            "cost_status": "priced",
                        }
                    },
                    {
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
                            "server_tool_calls": 0,
                            "stream_count": 0,
                            "client_disconnect_total": 0,
                            "fallback_served": 0,
                            "ttft_p50_ms": null,
                            "ttft_p95_ms": null,
                            "latency_p50_ms": null,
                            "latency_p95_ms": null,
                            "throughput_tok_s": null,
                            "ctx_avg": null,
                            "ctx_peak": null,
                            "cache_hit_pct": null,
                            "cost_usd": null,
                            "equivalent_cost_usd": null,
                            "cost_status": "unpriced",
                        }
                    }
                ]
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
            "ttft_p50_ms",
            "ttft_p95_ms",
            "latency_p50_ms",
            "latency_p95_ms",
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
    seed_ledger(&path, &[Local::now().timestamp_millis()]);
    let state = state_with_ledger(path.clone());

    let pricer = state.router.pricer();
    let db = open_readonly_fastfail(&path).expect("open seeded ledger");
    let spec = parse_spec(VALID_BODY.as_bytes(), Local::now()).unwrap();

    let first = query(&db, &spec, |row| pricer.price(row), far_deadline()).unwrap();
    let second = query(&db, &spec, |row| pricer.price(row), far_deadline()).unwrap();
    assert_eq!(first, second);
    // The default config carries no `[registry]` pricing, so a real row is
    // honestly unpriced rather than costed at zero.
    assert_eq!(first.totals.cost_status, CostStatus::Unpriced);
    assert!(first.totals.cost_usd.is_none());
}
