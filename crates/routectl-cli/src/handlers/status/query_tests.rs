use super::*;

use super::super::Availability;
use super::super::test_hooks::StatusTestHooks;
use crate::commands::usage::MAX_BUCKETS;
use crate::handlers::status::DaemonMeta;
use crate::handlers::status::accounts::tests::{anthropic_block, codex_block, router_of};
use crate::server::AppState;
use arc_swap::ArcSwap;
use axum::body::{Body, to_bytes};
use axum::http::Request as HttpRequest;
use routectl_router::{
    AccountIdentities, Config, ModelEntry, PricingConfig, ProviderEntry, Router,
};
use routectl_usage::{CostStatus, QueryGroup, QueryMetrics, QuerySeries, SeriesBucket, open};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
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
    seed_ledger(
        &path,
        &[crate::local_time::unix_ms(time::OffsetDateTime::now_utc())],
    );

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
    seed_ledger(
        &path,
        &[crate::local_time::unix_ms(time::OffsetDateTime::now_utc())],
    );

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
    seed_ledger(
        &path,
        &[crate::local_time::unix_ms(time::OffsetDateTime::now_utc())],
    );

    // Each body fails closed for a DIFFERENT reason: unknown token, unknown
    // field, missing required field, wrong type, blank filter, not JSON at all,
    // and a nested marker that must never be echoed back.
    let bodies = [
        r#"{"window":"decade","group_by":"model"}"#,
        r#"{"window":"24h","group_by":"model"}"#,
        r#"{"window":"trailing_24h","group_by":"model"}"#,
        r#"{"window":"all","group_by":"upstream"}"#,
        r#"{"window":"all","group_by":"outcome"}"#,
        r#"{"window":"all","group_by":"class"}"#,
        r#"{"window":"all","group_by":"client"}"#,
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
            "outcome",
            "class",
            "client",
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
    assert!(json["data"]["totals"]["ttft_avg_ms"].is_null());
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
    seed_ledger(
        &path,
        &[crate::local_time::unix_ms(time::OffsetDateTime::now_utc())],
    );
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
    seed_ledger(
        &path,
        &[crate::local_time::unix_ms(time::OffsetDateTime::now_utc())],
    );
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
    let now = OffsetDateTime::now_utc();
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
fn trailing_window_tokens_parse_to_exact_durations_ending_now() {
    let now = OffsetDateTime::now_utc();
    let all = parse_spec(br#"{"window":"all","group_by":"model"}"#, now).unwrap();
    for (token, span_ms) in [("last24h", 86_400_000), ("last7d", 604_800_000)] {
        let body = format!(r#"{{"window":"{token}","group_by":"model"}}"#);
        let spec = parse_spec(body.as_bytes(), now).expect("token is in vocabulary");
        assert_eq!(spec.to_ms, all.to_ms, "window={token} ends at now");
        assert_eq!(spec.to_ms - spec.from_ms, span_ms, "window={token} span");
    }
}

#[test]
fn each_group_by_token_maps_to_its_dimension() {
    let now = OffsetDateTime::now_utc();
    for dim in GroupDim::ALL {
        let token = dim.as_str();
        let body = format!(r#"{{"window":"all","group_by":"{token}"}}"#);
        let spec = parse_spec(body.as_bytes(), now).unwrap();
        assert_eq!(spec.group_by, dim, "group_by={token}");
    }
}

#[tokio::test]
async fn group_by_seat_rolls_rows_up_by_their_recorded_seat() {
    // Arrange: two rows under one seat, one under another, and one with no
    // recorded seat, all sharing one model so only a seat grouping splits them.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    let now = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());
    seed_seated_ledger(
        &path,
        now,
        &[Some("work"), Some("work"), Some("home"), None],
    );

    // Act
    let (status, json) = send(
        state_with_ledger(path),
        QUERY_METHOD,
        r#"{"window":"all","group_by":"seat"}"#,
    )
    .await;

    // Assert
    assert_eq!(status, StatusCode::OK);
    assert!(json["unavailable"].is_null(), "seeded ledger: {json}");
    let mut groups: Vec<(String, i64)> = json["data"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            let label = g["label"].as_str().unwrap().to_string();
            (label, g["metrics"]["requests"].as_i64().unwrap())
        })
        .collect();
    groups.sort();
    assert_eq!(
        groups,
        [
            ("(unattributed)".to_string(), 1),
            ("home".to_string(), 1),
            ("work".to_string(), 2),
        ]
    );
    assert_eq!(json["data"]["totals"]["requests"], 4);
}

/// A router serving `anthropic#work` through two entries (`zeta`, `alpha`)
/// and the unlabeled `codex` login.
fn work_and_codex_router() -> Router {
    router_of(&[
        (
            "a-model",
            "zeta",
            anthropic_block(),
            "oauth://anthropic#work".into(),
        ),
        (
            "b-model",
            "alpha",
            anthropic_block(),
            "oauth://anthropic#work".into(),
        ),
        (
            "c-model",
            "codex-one",
            codex_block(),
            "oauth://codex".into(),
        ),
    ])
}

fn state_over_router(db_path: PathBuf, router: Router) -> Arc<StatusState> {
    let app = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
    let mut status = StatusState::from_app(&app, None, DaemonMeta::for_test());
    status.usage_db_path = db_path;
    Arc::new(status)
}

fn group_by_label<'a>(json: &'a Value, label: &str) -> &'a Value {
    json["data"]["groups"]
        .as_array()
        .expect("groups array")
        .iter()
        .find(|group| group["label"] == label)
        .unwrap_or_else(|| panic!("no group {label}: {json}"))
}

/// Each seat group names its account with the identity `/status/accounts`
/// gives it: a configured labeled login carries all three fields (under the
/// smaller of the two entries naming it), an unlabeled login a null label, a
/// ledger seat the config no longer names its account and two nulls, and
/// `(unattributed)` three nulls.
#[tokio::test]
async fn seat_groups_carry_the_account_identity_of_the_served_config() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    let now = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());
    seed_seated_ledger(
        &path,
        now,
        &[
            Some("anthropic#work"),
            Some("codex"),
            Some("anthropic#gone"),
            None,
        ],
    );
    let state = state_over_router(path, work_and_codex_router());

    // Act
    let (status, json) = send(state, QUERY_METHOD, r#"{"window":"all","group_by":"seat"}"#).await;

    // Assert
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["schema_version"], SCHEMA_VERSION, "{json}");
    let rows: [(&str, Value, Value, Value); 4] = [
        (
            "anthropic#work",
            Value::from("anthropic#work"),
            Value::from("alpha"),
            Value::from("work"),
        ),
        (
            "codex",
            Value::from("codex"),
            Value::from("codex-one"),
            Value::Null,
        ),
        (
            "anthropic#gone",
            Value::from("anthropic#gone"),
            Value::Null,
            Value::Null,
        ),
        ("(unattributed)", Value::Null, Value::Null, Value::Null),
    ];
    for (label, account, entry, login) in rows {
        let group = group_by_label(&json, label).as_object().expect("group");
        assert_eq!(group["account"], account, "{label}");
        assert!(group.contains_key("provider_entry"), "{label}");
        assert_eq!(group["provider_entry"], entry, "{label}");
        assert!(group.contains_key("login_label"), "{label}");
        assert_eq!(group["login_label"], login, "{label}");
        assert_eq!(group["metrics"]["requests"], 1, "{label}");
    }
}

/// A row with no recorded seat served by an entry whose account is its own
/// API key groups under that `#<entry>` account, named like any other
/// configured account: the entry, and no login label.
#[tokio::test]
async fn an_unseated_row_on_an_api_key_entry_groups_under_its_account() {
    // Arrange: every seeded row is served by entry `p`, configured here with
    // an `env://` key beside the `anthropic#work` login.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    let now = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());
    seed_seated_ledger(&path, now, &[Some("anthropic#work"), None, None]);
    let router = router_of(&[
        (
            "a-model",
            "alpha",
            anthropic_block(),
            "oauth://anthropic#work".into(),
        ),
        (
            "k-model",
            "p",
            anthropic_block(),
            "env://ROUTECTL_QUERY_TEST_KEY".into(),
        ),
    ]);
    let state = state_over_router(path, router);

    // Act
    let (status, json) = send(state, QUERY_METHOD, r#"{"window":"all","group_by":"seat"}"#).await;

    // Assert
    assert_eq!(status, StatusCode::OK);
    assert_eq!(json["schema_version"], SCHEMA_VERSION, "{json}");
    let labels: Vec<&str> = json["data"]["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .filter_map(|g| g["label"].as_str())
        .collect();
    assert_eq!(labels, ["#p", "anthropic#work"], "{json}");
    let keyed = group_by_label(&json, "#p");
    assert_eq!(keyed["account"], "#p", "{json}");
    assert_eq!(keyed["provider_entry"], "p", "{json}");
    assert!(
        keyed
            .as_object()
            .expect("group")
            .contains_key("login_label")
    );
    assert_eq!(keyed["login_label"], Value::Null, "{json}");
    assert_eq!(keyed["metrics"]["requests"], 2, "{json}");
}

/// Groups of every other grouping keep exactly their label and metrics: no
/// identity key appears, on the route, even over a config naming seats.
#[tokio::test]
async fn non_seat_groups_carry_no_identity_keys() {
    for group_by in ["model", "alias", "provider"] {
        // Arrange
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("usage.db");
        let now = crate::local_time::unix_ms(time::OffsetDateTime::now_utc());
        seed_seated_ledger(&path, now, &[Some("anthropic#work"), None]);
        let state = state_over_router(path, work_and_codex_router());

        // Act
        let body = format!(r#"{{"window":"all","group_by":"{group_by}"}}"#);
        let (status, json) = send(state, QUERY_METHOD, &body).await;

        // Assert
        assert_eq!(status, StatusCode::OK, "{group_by}");
        for group in json["data"]["groups"].as_array().expect("groups") {
            let mut keys: Vec<&str> = group
                .as_object()
                .expect("group")
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            assert_eq!(keys, ["label", "metrics"], "{group_by}: {group}");
        }
    }
}

/// A non-seat grouping's wire payload is the grouped result exactly as it
/// serialized before seat groups were named, plus the ledger's earliest row.
#[test]
fn non_seat_payloads_serialize_as_the_grouped_result() {
    for group_by in [GroupDim::Model, GroupDim::Alias, GroupDim::Provider] {
        // Arrange
        let result = QueryResult {
            groups: vec![QueryGroup {
                label: "anthropic#work".to_string(),
                metrics: QueryMetrics::default(),
                requests_by_model: None,
            }],
            totals: QueryMetrics::default(),
            series: Some(QuerySeries {
                bucket_ms: 3_600_000,
                buckets: vec![SeriesBucket {
                    start_ms: 0,
                    metrics: QueryMetrics::default(),
                }],
            }),
        };
        let identities = AccountIdentities::from_config(&work_and_codex_router().config);

        let read = QueryRead {
            result: result.clone(),
            earliest_ms: Some(1_000),
        };

        // Act
        let payload = name_groups(read, group_by, &identities);

        // Assert
        let mut expected = serde_json::to_value(&result).unwrap();
        expected["earliest_ms"] = Value::from(1_000);
        assert_eq!(
            serde_json::to_value(&payload).unwrap(),
            expected,
            "{group_by:?}"
        );
    }
}

/// Seed one `ok` row at `ts` per entry of `seats`, each recording that seat
/// (or none), all under one model/alias/provider.
fn seed_seated_ledger(path: &Path, ts: i64, seats: &[Option<&str>]) {
    let db = open(path).expect("open ledger");
    for (i, seat) in seats.iter().enumerate() {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, ttfb_ms, tool_count, msg_count, attempt_count, fallback_count, \
                 input_tokens, output_tokens, seat) \
                 VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 'm', 'p', 'u', 1, 'ok', \
                 50, 10, 0, 0, 1, 0, 100, 20, ?3)",
                rusqlite::params![ts, format!("r{i}"), seat],
            )
            .expect("seed row");
    }
}

#[test]
fn absent_filters_match_everything_and_present_ones_bind() {
    let now = OffsetDateTime::now_utc();
    let unfiltered = parse_spec(br#"{"window":"all","group_by":"model"}"#, now).unwrap();
    assert!(unfiltered.alias_filter.is_none());
    assert!(unfiltered.provider_filter.is_none());
    assert!(unfiltered.model_filter.is_none());

    let filtered = parse_spec(
        br#"{"window":"all","group_by":"model","alias":"fast","provider":"anthropic","model":"m1"}"#,
        now,
    )
    .unwrap();
    assert_eq!(filtered.alias_filter.as_deref(), Some("fast"));
    assert_eq!(filtered.provider_filter.as_deref(), Some("anthropic"));
    assert_eq!(filtered.model_filter.as_deref(), Some("m1"));
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
    let mut db = open_readonly_fastfail(&path).expect("open seeded ledger");
    let spec = parse_spec(VALID_BODY.as_bytes(), OffsetDateTime::now_utc()).unwrap();
    let pricer = state_with_ledger(path).pricer().pin();

    let err = query(
        &mut db,
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
include!("query_price_pickup_tests.rs");
include!("query_model_filter_tests.rs");
include!("query_gate_tests.rs");
