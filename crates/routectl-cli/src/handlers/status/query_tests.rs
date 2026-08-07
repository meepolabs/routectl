use super::*;

use crate::commands::usage::MAX_BUCKETS;
use crate::handlers::status::DaemonMeta;
use crate::server::AppState;
use arc_swap::ArcSwap;
use axum::body::{Body, to_bytes};
use axum::http::Request as HttpRequest;
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use routectl_router::{Config, Router};
use routectl_usage::{CostStatus, QueryGroup, QueryMetrics, QuerySeries, SeriesBucket, open};
use serde_json::Value;
use std::path::PathBuf;
use tempfile::TempDir;
use tower::ServiceExt;

// The shared harness and the wire/series test groups live in sibling files to
// keep each file under the size ceiling. They compile into THIS module via
// `include!`, so the helpers stay in scope and no test's module path changes.
include!("query_test_support.rs");

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

include!("query_wire_tests.rs");
include!("query_series_tests.rs");
