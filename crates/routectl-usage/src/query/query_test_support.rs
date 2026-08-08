// The shared ledger-seeding helpers and row finders for the `query` unit
// tests. `include!`d into `tests.rs`, so these compile into THAT module and
// every fragment sees them; all imports live in the host file -- do not add
// `use` lines here.

fn temp_db_path() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    (dir, path)
}

/// Insert a row with explicit group keys, outcome, tokens, latency, and
/// optional server_tool_use JSON. Token args are Option to exercise the
/// NULL-contributes-0 path.
#[allow(clippy::too_many_arguments)]
fn insert_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    model: &str,
    provider: &str,
    upstream: &str,
    alias: &str,
    outcome: &str,
    input: Option<i64>,
    output: Option<i64>,
    latency_ms: i64,
    server_tool_use: Option<&str>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             input_tokens, output_tokens, server_tool_use) \
             VALUES (?1, ?1, ?2, 'openai', 'req-model', ?3, ?4, ?5, ?6, 0, ?7, \
             ?8, 0, 0, 1, 0, ?9, ?10, ?11)",
            rusqlite::params![
                ts_start,
                request_id,
                alias,
                model,
                provider,
                upstream,
                outcome,
                latency_ms,
                input,
                output,
                server_tool_use,
            ],
        )
        .expect("insert row");
}

/// Insert a row with explicit `stream`, `ttfb_ms`, `outcome`,
/// `reasoning_tokens`, and cache columns so the streaming /
/// presence-count paths can be exercised. `ttfb_ms`, `reasoning`, and the
/// cache args are `Option` so NULL-vs-reported-0 is testable.
#[allow(clippy::too_many_arguments)]
fn insert_full_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    stream: i64,
    outcome: &str,
    ttfb_ms: Option<i64>,
    latency_ms: i64,
    output: Option<i64>,
    reasoning: Option<i64>,
    cache_read: Option<i64>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, ttfb_ms, tool_count, msg_count, attempt_count, \
             fallback_count, output_tokens, reasoning_tokens, cache_read) \
             VALUES (?1, ?1, ?2, 'openai', 'req-model', 'al', 'm', 'pa', 'ua', \
             ?3, ?4, ?5, ?6, 0, 0, 1, 0, ?7, ?8, ?9)",
            rusqlite::params![
                ts_start, request_id, stream, outcome, latency_ms, ttfb_ms, output, reasoning,
                cache_read,
            ],
        )
        .expect("insert full row");
}

/// Insert a quota-bearing row with an explicit `seat` / `provider_kind` and
/// individually nullable quota columns, so the per-seat partition and the
/// widened `status OR utilization` eligibility predicate are both exercisable.
#[allow(clippy::too_many_arguments)]
fn insert_seat_quota_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    seat: Option<&str>,
    provider_kind: Option<&str>,
    status: Option<&str>,
    utilization: Option<f64>,
    reset: Option<i64>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, stream, outcome, latency_ms, tool_count, \
             msg_count, attempt_count, fallback_count, seat, provider_kind, \
             quota_status, quota_utilization, quota_reset) \
             VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 0, 'ok', 0, 0, 0, 1, 0, \
             ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                ts_start,
                request_id,
                seat,
                provider_kind,
                status,
                utilization,
                reset
            ],
        )
        .expect("insert quota row");
}

fn find_seat<'a>(snaps: &'a [QuotaSnapshot], seat: Option<&str>) -> &'a QuotaSnapshot {
    snaps
        .iter()
        .find(|s| s.seat.as_deref() == seat)
        .expect("seat bucket present")
}

fn find_row<'a>(rows: &'a [AggRow], provider: &str, upstream: &str) -> &'a AggRow {
    rows.iter()
        .find(|r| {
            r.key.provider.as_deref() == Some(provider)
                && r.key.upstream.as_deref() == Some(upstream)
        })
        .expect("group present")
}
