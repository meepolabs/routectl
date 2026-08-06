use super::*;

use crate::commands::usage::MAX_BUCKETS;
use crate::handlers::status::DaemonMeta;
use crate::server::AppState;
use arc_swap::ArcSwap;
use axum::body::{Body, to_bytes};
use axum::http::Request as HttpRequest;
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use routectl_router::{Config, Router};
#[cfg(not(debug_assertions))]
use routectl_usage::{BucketSpec, RowCost};
use routectl_usage::{CostStatus, QueryGroup, QueryMetrics, QuerySeries, SeriesBucket, open};
use serde_json::Value;
use std::path::PathBuf;
use tempfile::TempDir;
use tower::ServiceExt;

/// A `StatusState` whose usage-ledger path is `db_path`.
fn state_with_ledger(db_path: PathBuf) -> Arc<StatusState> {
    let router = Router::new(Arc::new(Config::default()));
    let (app, _writer_dir) = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
    let mut status = StatusState::from_app(&app, None, DaemonMeta::for_test());
    status.usage_db_path = db_path;
    Arc::new(status)
}

/// Seed a WAL ledger with one `ok` streaming row per timestamp, all sharing one
/// group key.
fn seed_ledger(path: &Path, timestamps: &[i64]) {
    let db = open(path).expect("open ledger");
    for (i, ts) in timestamps.iter().enumerate() {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, ttfb_ms, tool_count, msg_count, attempt_count, fallback_count, \
                 input_tokens, output_tokens) \
                 VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 'm', 'p', 'u', 1, 'ok', \
                 50, 10, 0, 0, 1, 0, 100, 20)",
                rusqlite::params![ts, format!("r{i}")],
            )
            .expect("seed row");
    }
}

/// Drive one request through the real status router.
async fn send(
    state: Arc<StatusState>,
    method: &str,
    body: &str,
) -> (axum::http::StatusCode, Value) {
    let app = super::super::status_router().with_state(state);
    let resp = app
        .oneshot(
            HttpRequest::builder()
                .method(method)
                .uri("/status/query")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

const VALID_BODY: &str = r#"{"window":"all","group_by":"model"}"#;

/// The spec half of a parsed body, for the tests that only assert on the window
/// and filters.
fn parse_spec(body: &[u8], now: DateTime<Local>) -> Result<QuerySpec, ()> {
    spec_from_body(body, now).map(|(spec, _)| spec)
}

#[tokio::test]
async fn query_method_with_valid_body_returns_available_panel() {
    // Arrange: a seeded ledger and an in-vocabulary body.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    seed_ledger(&path, &[Local::now().timestamp_millis()]);

    // Act
    let (status, json) = send(state_with_ledger(path), QUERY_METHOD, VALID_BODY).await;

    // Assert
    assert_eq!(status, StatusCode::OK);
    assert!(json["unavailable"].is_null(), "seeded ledger: {json}");
    assert_eq!(json["schema_version"], SCHEMA_VERSION);
    assert_eq!(json["data"]["totals"]["requests"], 1);
    assert_eq!(json["data"]["groups"][0]["label"], "m");
}

#[tokio::test]
async fn every_method_other_than_query_returns_405() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    seed_ledger(&path, &[Local::now().timestamp_millis()]);

    for method in ["GET", "POST", "PUT", "DELETE", "PATCH"] {
        let (status, _) = send(state_with_ledger(path.clone()), method, VALID_BODY).await;
        assert_eq!(
            status,
            StatusCode::METHOD_NOT_ALLOWED,
            "{method} /status/query must be 405"
        );
    }
}

#[tokio::test]
async fn out_of_vocabulary_bodies_return_a_leak_free_400() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    seed_ledger(&path, &[Local::now().timestamp_millis()]);

    // Each body fails closed for a DIFFERENT reason: unknown token, unknown
    // field, missing required field, wrong type, blank filter, not JSON at all,
    // and a nested marker that must never be echoed back.
    let bodies = [
        r#"{"window":"decade","group_by":"model"}"#,
        r#"{"window":"all","group_by":"upstream"}"#,
        r#"{"window":"all","group_by":"model","seat":"LEAKMARKER"}"#,
        r#"{"group_by":"model"}"#,
        r#"{"window":"all"}"#,
        r#"{"window":1,"group_by":"model"}"#,
        r#"{"window":"all","group_by":"model","alias":"   "}"#,
        r#"{"window":"all","group_by":"model","alias":42}"#,
        "not json at all LEAKMARKER",
        "",
    ];

    for body in bodies {
        let (status, json) = send(state_with_ledger(path.clone()), QUERY_METHOD, body).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "body must be refused: {body}"
        );
        assert_eq!(json["schema_version"], SCHEMA_VERSION);
        assert_eq!(json["error"]["code"], INVALID_QUERY);
        assert!(json["error"]["message"].is_string());
        // The envelope is FIXED: no panel keys, no serde text, no body echo.
        let rendered = json.to_string();
        for forbidden in [
            "LEAKMARKER",
            "expected",
            "unknown",
            "line",
            "column",
            "seat",
            "decade",
            "upstream",
        ] {
            assert!(
                !rendered.contains(forbidden),
                "400 envelope leaked `{forbidden}` for body {body}: {rendered}"
            );
        }
        assert!(json.get("data").is_none(), "400 is not a panel: {rendered}");
        assert!(json.get("unavailable").is_none());
    }
}

#[tokio::test]
async fn empty_ledger_is_available_with_zeros_never_400() {
    // A valid body against a healthy but EMPTY ledger is data, not a client
    // error: HTTP 200, available, zeroed totals, no groups.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    drop(open(&path).expect("open empty ledger"));

    let (status, json) = send(state_with_ledger(path), QUERY_METHOD, VALID_BODY).await;

    assert_eq!(status, StatusCode::OK);
    assert!(json["unavailable"].is_null(), "empty ledger: {json}");
    assert_eq!(json["data"]["totals"]["requests"], 0);
    assert_eq!(json["data"]["groups"].as_array().unwrap().len(), 0);
    // An absent derived metric is an explicit null, never a misleading 0.
    assert!(json["data"]["totals"]["ttft_p50_ms"].is_null());
    assert!(json["data"]["totals"]["cost_usd"].is_null());
    assert_eq!(json["data"]["totals"]["cost_status"], "unpriced");
}

#[tokio::test]
async fn missing_ledger_is_unavailable_no_data_never_400() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("absent.db");

    let (status, json) = send(state_with_ledger(path), QUERY_METHOD, VALID_BODY).await;

    assert_eq!(status, StatusCode::OK, "a missing ledger never 400s");
    assert_eq!(json["unavailable"], codes::NO_DATA);
    assert!(json["data"].is_null());
    assert!(json["as_of"].is_null());
    assert_eq!(json["schema_version"], SCHEMA_VERSION);
}

#[tokio::test]
async fn a_wrong_method_is_refused_before_the_body_is_read() {
    // The method guard is the FIRST thing the handler does, so an
    // out-of-vocabulary body under a wrong method is 405 (method regime), not
    // 400 (vocab regime) -- the three regimes are exclusive.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("absent.db");
    let (status, _) = send(state_with_ledger(path), "POST", "not json at all").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn a_body_over_the_ceiling_is_refused_without_echoing_it() {
    // Arrange: an otherwise-valid body padded one byte past the ceiling, with a
    // marker inside the padding so an echo is detectable.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    seed_ledger(&path, &[Local::now().timestamp_millis()]);
    let prefix = r#"{"window":"all","group_by":"model","alias":"LEAKMARKER"#;
    let body = format!(
        "{prefix}{}\"}}",
        "P".repeat(MAX_BODY_BYTES + 1 - prefix.len() - 2)
    );
    assert_eq!(body.len(), MAX_BODY_BYTES + 1, "one byte past the ceiling");

    // Act
    let (status, json) = send(state_with_ledger(path), QUERY_METHOD, &body).await;

    // Assert: the ceiling is a vocabulary refusal, and the refusal carries
    // nothing from the payload it refused.
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(json["error"]["code"], INVALID_QUERY);
    let rendered = json.to_string();
    assert!(!rendered.contains("LEAKMARKER"), "echoed body: {rendered}");
    assert!(!rendered.contains("PP"), "echoed padding: {rendered}");
    assert!(json.get("data").is_none());
}

#[tokio::test(start_paused = true)]
async fn a_body_that_never_finishes_arriving_is_refused_on_the_read_deadline() {
    // Arrange: a body whose stream announces nothing and never yields -- the
    // byte ceiling cannot help, since no byte ever arrives. The handler holds a
    // concurrency permit while it waits, so the read must be bounded rather than
    // lasting as long as the client keeps the socket open.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    seed_ledger(&path, &[Local::now().timestamp_millis()]);
    let stalled = Body::from_stream(futures::stream::pending::<Result<Vec<u8>, std::io::Error>>());
    let app = super::super::status_router().with_state(state_with_ledger(path));

    // Act: paused time is auto-advanced while the handler is parked on the
    // deadline, so the wall clock is not actually waited out.
    let started = tokio::time::Instant::now();
    let resp = app
        .oneshot(
            HttpRequest::builder()
                .method(QUERY_METHOD)
                .uri("/status/query")
                .header("content-type", "application/json")
                .body(stalled)
                .unwrap(),
        )
        .await
        .unwrap();

    // Assert: refused as an out-of-vocabulary query the moment the deadline
    // fires, releasing the permit instead of holding it indefinitely.
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["code"], INVALID_QUERY);
    assert_eq!(
        started.elapsed(),
        BODY_READ_TIMEOUT,
        "the read is bounded by exactly the deadline"
    );
}

#[tokio::test]
async fn a_non_finite_metric_renders_as_null_and_never_as_a_500() {
    // Arrange: a panel whose float metric is not finite -- the shape the cost
    // and ratio guards exist to prevent, asserted here from the render side so
    // the route's three declared regimes stay exhaustive either way.
    let panel = Panel::available(
        SCHEMA_VERSION,
        "2026-08-02T00:00:00Z".to_string(),
        QueryResult {
            groups: vec![],
            totals: QueryMetrics {
                throughput_tok_s: Some(f64::NAN),
                cache_hit_pct: Some(f64::INFINITY),
                ..QueryMetrics::default()
            },
            series: None,
        },
    );

    // Act: rendering is explicit, so no responder can interpose its own
    // serialization-failure 500.
    let resp = render(panel);

    // Assert
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        resp.headers()
            .get(header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("application/json")
    );
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    assert!(json["data"]["totals"]["throughput_tok_s"].is_null());
    assert!(json["data"]["totals"]["cache_hit_pct"].is_null());
}

#[test]
fn each_window_token_maps_to_its_window_flag() {
    let now = Local::now();
    let all = parse_spec(br#"{"window":"all","group_by":"model"}"#, now).unwrap();
    assert_eq!(all.from_ms, 0, "all-time starts at the epoch");

    // The three calendar windows all start at or before the current instant and
    // share one upper bound; their relative order is NOT asserted, because a
    // month start can fall after a week start (the 1st mid-week).
    for token in ["today", "week", "month"] {
        let body = format!(r#"{{"window":"{token}","group_by":"model"}}"#);
        let spec = parse_spec(body.as_bytes(), now).unwrap();
        assert!(spec.from_ms > 0, "window={token} is not all-time");
        assert!(spec.from_ms < spec.to_ms, "window={token} is non-empty");
        assert_eq!(spec.to_ms, all.to_ms, "every window shares the upper bound");
    }
}

#[test]
fn each_group_by_token_maps_to_its_dimension() {
    let now = Local::now();
    for (token, dim) in [
        ("model", GroupDim::Model),
        ("provider", GroupDim::Provider),
        ("alias", GroupDim::Alias),
    ] {
        let body = format!(r#"{{"window":"all","group_by":"{token}"}}"#);
        let spec = parse_spec(body.as_bytes(), now).unwrap();
        assert_eq!(spec.group_by, dim, "group_by={token}");
    }
}

#[test]
fn absent_filters_match_everything_and_present_ones_bind() {
    let now = Local::now();
    let unfiltered = parse_spec(br#"{"window":"all","group_by":"model"}"#, now).unwrap();
    assert!(unfiltered.alias_filter.is_none());
    assert!(unfiltered.provider_filter.is_none());

    let filtered = parse_spec(
        br#"{"window":"all","group_by":"model","alias":"fast","provider":"anthropic"}"#,
        now,
    )
    .unwrap();
    assert_eq!(filtered.alias_filter.as_deref(), Some("fast"));
    assert_eq!(filtered.provider_filter.as_deref(), Some("anthropic"));
}

#[tokio::test]
async fn an_expired_deadline_sheds_query_timeout_end_to_end() {
    // The progress handler checks the deadline every few thousand VM
    // instructions, so the interrupt is only observable on a query that
    // actually runs that long -- a handful of rows completes before the first
    // check. Seeded wide enough (distinct group keys force a real temp B-tree)
    // that an already-expired deadline is cut short on the real query path,
    // not just in the error mapping.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    seed_wide_ledger(&path, 4_000);
    let db = open_readonly_fastfail(&path).expect("open seeded ledger");
    let spec = parse_spec(VALID_BODY.as_bytes(), Local::now()).unwrap();
    let pricer = state_with_ledger(path).router.pricer();

    let err = query(
        &db,
        &spec,
        |row| pricer.price(row),
        Instant::now()
            .checked_sub(Duration::from_millis(1))
            .expect("the process has been up for at least a millisecond"),
    )
    .expect_err("an already-expired deadline must interrupt");

    assert_eq!(query_error_code(&err), codes::QUERY_TIMEOUT);
}

/// Seed `rows` rows, each under its OWN model/alias so the grouped aggregate
/// builds a real temp B-tree rather than collapsing to a single group.
fn seed_wide_ledger(path: &Path, rows: usize) {
    let db = open(path).expect("open ledger");
    let now = Local::now().timestamp_millis();
    let conn = db.conn();
    conn.execute_batch("BEGIN").expect("begin");
    for i in 0..rows {
        conn.execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, ttfb_ms, tool_count, msg_count, attempt_count, fallback_count, \
             input_tokens, output_tokens) \
             VALUES (?1, ?1, ?2, 'openai', ?3, ?4, ?3, 'p', 'u', 1, 'ok', \
             50, 10, 0, 0, 1, 0, 100, 20)",
            rusqlite::params![now, format!("r{i}"), format!("m{i}"), format!("a{i}")],
        )
        .expect("seed row");
    }
    conn.execute_batch("COMMIT").expect("commit");
}

/// The DRIFT TEST. A fully-populated response is serialized and compared to
/// exact JSON, so every metric name, every `cost_status` token, and the
/// envelope shape are pinned: renaming or dropping any of them fails here.
#[test]
fn wire_shape_pins_every_metric_token() {
    fn metrics(offset: i64, status: CostStatus, cost: Option<f64>) -> QueryMetrics {
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
            cost_status: status,
        }
    }

    let result = QueryResult {
        groups: vec![QueryGroup {
            label: "sonnet".to_string(),
            metrics: metrics(0, CostStatus::Priced, Some(1.25)),
        }],
        totals: metrics(1, CostStatus::Partial, Some(1.25)),
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
    fn metrics(offset: i64, status: CostStatus, cost: Option<f64>) -> QueryMetrics {
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
            cost_status: status,
        }
    }

    let result = QueryResult {
        groups: vec![QueryGroup {
            label: "sonnet".to_string(),
            metrics: metrics(0, CostStatus::Priced, Some(2.5)),
        }],
        totals: metrics(1, CostStatus::Partial, Some(2.5)),
        series: Some(QuerySeries {
            bucket_ms: 3_600_000,
            buckets: vec![
                SeriesBucket {
                    start_ms: 1_000_000,
                    metrics: metrics(2, CostStatus::Priced, Some(0.75)),
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

fn far_deadline() -> Instant {
    Instant::now() + Duration::from_secs(30)
}

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
