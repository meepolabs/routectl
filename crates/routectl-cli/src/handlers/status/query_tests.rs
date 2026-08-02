use super::*;

use crate::server::AppState;
use arc_swap::ArcSwap;
use axum::body::{Body, to_bytes};
use axum::http::Request as HttpRequest;
use chrono::Local;
use routectl_router::{Config, Router};
use routectl_usage::{CostStatus, QueryGroup, QueryMetrics, open};
use serde_json::Value;
use std::path::PathBuf;
use tempfile::TempDir;
use tower::ServiceExt;

/// A `StatusState` whose usage-ledger path is `db_path`.
fn state_with_ledger(db_path: PathBuf) -> Arc<StatusState> {
    let router = Router::new(Arc::new(Config::default()));
    let (app, _writer_dir) = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
    let mut status = StatusState::from_app(&app, None);
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
    let all = spec_from_body(br#"{"window":"all","group_by":"model"}"#, now).unwrap();
    assert_eq!(all.from_ms, 0, "all-time starts at the epoch");

    // The three calendar windows all start at or before the current instant and
    // share one upper bound; their relative order is NOT asserted, because a
    // month start can fall after a week start (the 1st mid-week).
    for token in ["today", "week", "month"] {
        let body = format!(r#"{{"window":"{token}","group_by":"model"}}"#);
        let spec = spec_from_body(body.as_bytes(), now).unwrap();
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
        let spec = spec_from_body(body.as_bytes(), now).unwrap();
        assert_eq!(spec.group_by, dim, "group_by={token}");
    }
}

#[test]
fn absent_filters_match_everything_and_present_ones_bind() {
    let now = Local::now();
    let unfiltered = spec_from_body(br#"{"window":"all","group_by":"model"}"#, now).unwrap();
    assert!(unfiltered.alias_filter.is_none());
    assert!(unfiltered.provider_filter.is_none());

    let filtered = spec_from_body(
        br#"{"window":"all","group_by":"model","alias":"fast","provider":"anthropic"}"#,
        now,
    )
    .unwrap();
    assert_eq!(filtered.alias_filter.as_deref(), Some("fast"));
    assert_eq!(filtered.provider_filter.as_deref(), Some("anthropic"));
}

#[test]
fn a_fired_deadline_sheds_under_its_own_code() {
    // The interrupt is NOT a broken database: it gets query_timeout, while a
    // lock gets db_busy and anything else db_unavailable.
    assert_eq!(
        query_error_code(&QueryError::Interrupted),
        codes::QUERY_TIMEOUT
    );
    let locked = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED),
        None,
    );
    assert_eq!(
        query_error_code(&QueryError::Sqlite(locked)),
        codes::DB_BUSY
    );
    let corrupt = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
        None,
    );
    assert_eq!(
        query_error_code(&QueryError::Sqlite(corrupt)),
        codes::DB_UNAVAILABLE
    );
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
    let spec = spec_from_body(VALID_BODY.as_bytes(), Local::now()).unwrap();
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
            }
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
    let spec = spec_from_body(VALID_BODY.as_bytes(), Local::now()).unwrap();

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
