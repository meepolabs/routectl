// Shared helpers for the `/status/query` test modules: the state builder, the
// ledger seeders, the request driver, and the deadline helper. Split from
// `query_tests.rs` to keep each file under the size ceiling; `include!`d into
// the same `tests` module, so every helper here is in scope for all four
// fragments without duplication and without re-exporting across modules. All
// imports come from the host `query_tests.rs` (its `use super::*`); do not add
// `use` lines here.

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

fn far_deadline() -> Instant {
    Instant::now() + Duration::from_secs(30)
}
