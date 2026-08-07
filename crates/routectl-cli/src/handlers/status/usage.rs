//! `/status/usage` panel: read-only windowed aggregates over the usage
//! ledger.
//!
//! The panel opens the ledger read-only PER REQUEST inside the
//! panic-isolation guard (which runs the builder on a blocking worker,
//! since SQLite is synchronous), aggregates the requested calendar window,
//! and maps the result to an aggregates-only DTO. It never caches a
//! connection, never touches the writer handle, and never exposes request
//! rows, ids, bodies, or prompts -- only rollups, windowed totals, the
//! per-seat quota snapshots, and the would-trim summary.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use chrono::Local;
use rusqlite::ErrorCode;
use serde::{Deserialize, Serialize};

use routectl_usage::{
    AggRow, GroupKey, OpenError, QueryError, QuotaSnapshot, UsageDb, WouldTrimSummary, aggregate,
    errors_by_class, latest_quota_by_seat, open_readonly_fastfail, would_trim_summary,
};

use crate::commands::usage::{WindowBounds, WindowFlag, window_bounds};

use super::vocabulary::codes;
use super::{Panel, StatusState, guard_panel, now_utc_rfc3339};

/// Wire-shape version of the usage panel payload.
pub const SCHEMA_VERSION: u32 = 3;

/// The `window` query parameter for `GET /status/usage`.
#[derive(Debug, Deserialize)]
pub(super) struct WindowQuery {
    window: Option<String>,
}

/// Aggregates-only usage payload. Carries per-`(alias, provider, model,
/// upstream)` rollups, windowed totals, one latest quota snapshot per seat,
/// and the would-trim summary. It never carries request rows, ids, bodies, or
/// prompts.
#[derive(Debug, Clone, Serialize)]
pub(super) struct UsagePanel {
    /// The resolved calendar window token: `today`, `week`, `month`, `all`.
    window: &'static str,
    /// Inclusive lower bound of the window, epoch-ms.
    from_ms: i64,
    /// Exclusive upper bound of the window, epoch-ms.
    to_ms: i64,
    totals: UsageTotals,
    groups: Vec<UsageGroup>,
    quota: Vec<UsageQuota>,
    would_trim: UsageWouldTrim,
}

/// Window-wide totals, summed across every rollup group.
#[derive(Debug, Clone, Default, Serialize)]
struct UsageTotals {
    requests: i64,
    ok: i64,
    errors: i64,
    input_tokens: i64,
    output_tokens: i64,
    reasoning_tokens: i64,
    cache_read_billed: i64,
    server_tool_calls: i64,
    /// Window-wide error breakdown by resolved failure class, merged across
    /// every group. Sums to `errors` by construction (same predicate). Empty
    /// (`{}`) when the window has zero errors. NULL-class rows bucket under
    /// `unclassified`; a forward-compat token an older reader does not know
    /// appears as its own key rather than folding into `unclassified`.
    errors_by_class: BTreeMap<String, i64>,
    /// Window-wide count of client-disconnect rows. Surfaced so
    /// `requests == ok + errors + client_disconnect_total` reconciles at
    /// totals (it already reconciles per group).
    client_disconnect_total: i64,
    /// Window-wide count of rows that REPORTED a `cache_read` value
    /// (`COUNT(cache_read)`, NULLs excluded) -- a reporting-only denominator,
    /// not a request count. Hit-rate figures MUST divide by this, NOT by raw
    /// `requests`: dividing by requests dilutes the rate with rows that never
    /// reported a cache read.
    cache_read_present: i64,
}

/// One rollup group at `(alias, provider, model, upstream)` granularity.
/// Every field is an aggregate count or token sum.
#[derive(Debug, Clone, Serialize)]
struct UsageGroup {
    alias: String,
    provider: Option<String>,
    model: Option<String>,
    upstream: Option<String>,
    requests: i64,
    ok: i64,
    errors: i64,
    input_tokens: i64,
    output_tokens: i64,
    reasoning_tokens: i64,
    cache_read_peak: i64,
    cache_read_billed: i64,
    cache_write_5m: i64,
    cache_write_1h: i64,
    server_tool_calls: i64,
    stream_count: i64,
    client_disconnect_total: i64,
    /// Per-group error breakdown by resolved failure class. Sums to `errors`
    /// by construction (same predicate). Empty (`{}`) when the group has zero
    /// errors. NULL-class rows bucket under `unclassified`; forward-compat
    /// tokens appear as their own keys.
    errors_by_class: BTreeMap<String, i64>,
    /// Count of rows in the group that REPORTED a `cache_read` value
    /// (`COUNT(cache_read)`, NULLs excluded) -- a reporting-only denominator,
    /// not a request count. Hit-rate figures MUST divide by this, NOT by raw
    /// `requests`: dividing by requests dilutes the rate with rows that never
    /// reported a cache read.
    cache_read_present: i64,
}

/// One seat's latest quota-bearing snapshot. The `quota_*` ledger columns are
/// shared across vendors, so `provider_kind` is the discriminator a client
/// MUST read to interpret `utilization` (a fraction of a per-provider window)
/// and to know which of the remaining fields that vendor populates at all.
#[derive(Debug, Clone, Serialize)]
struct UsageQuota {
    /// Credential identity this snapshot belongs to. `null` for pre-seat
    /// history and forwarded client credentials.
    seat: Option<String>,
    /// Provider kind of the row this snapshot came from -- the cross-vendor
    /// discriminator for every other field here.
    provider_kind: Option<String>,
    /// Freshness of THIS snapshot: the `ts_start` of the row it came from,
    /// epoch-ms. Distinct from the panel envelope's query-time `as_of`.
    ts_start_ms: i64,
    claim: Option<String>,
    status: Option<String>,
    overage_status: Option<String>,
    utilization: Option<f64>,
    overage_utilization: Option<f64>,
    /// Quota-reset instant in epoch MILLISECONDS. The ledger stamps quota
    /// resets in epoch SECONDS (see the observer), so the seconds value is
    /// scaled by 1000 here to make the `_ms` name truthful and keep it
    /// consistent with `ts_start_ms`.
    reset_ms: Option<i64>,
}

/// Windowed steady-state would-trim opportunity summary.
#[derive(Debug, Clone, Default, Serialize)]
struct UsageWouldTrim {
    candidate_requests: i64,
    would_trim_tokens: i64,
    verdict_met: i64,
    verdict_unmet: i64,
    verdict_cold: i64,
    verdict_unpriced: i64,
}

/// Map the `window` query value to its flag and canonical token. An unknown
/// or absent value defaults to `today`.
fn parse_window(raw: Option<&str>) -> (WindowFlag, &'static str) {
    match raw {
        Some("week") => (WindowFlag::ThisWeek, "week"),
        Some("month") => (WindowFlag::ThisMonth, "month"),
        Some("all") => (WindowFlag::All, "all"),
        _ => (WindowFlag::Today, "today"),
    }
}

/// The aggregate composer (`GET /status`) has no window parameter and
/// reports today's window.
pub(super) async fn build(state: &StatusState) -> Panel<UsagePanel> {
    build_window(state, WindowFlag::Today, "today").await
}

async fn build_window(
    state: &StatusState,
    flag: WindowFlag,
    token: &'static str,
) -> Panel<UsagePanel> {
    let bounds = window_bounds(flag, Local::now());
    let db_path = state.usage_db_path.clone();
    // Stamp `as_of` at request time (before the blocking read), so a lock
    // wait or slow disk cannot skew the freshness marker.
    let as_of = now_utc_rfc3339();
    let panel = guard_panel(SCHEMA_VERSION, codes::DB_UNAVAILABLE, move || {
        build_panel(&db_path, token, bounds, as_of)
    })
    .await;
    state.observability.usage.record(&panel);
    panel
}

pub(super) async fn handler(
    State(state): State<Arc<StatusState>>,
    Query(query): Query<WindowQuery>,
) -> Json<Panel<UsagePanel>> {
    let (flag, token) = parse_window(query.window.as_deref());
    Json(build_window(&state, flag, token).await)
}

/// Open the ledger read-only, aggregate the window, and build the panel.
/// Runs on a blocking worker (via [`guard_panel`]); the connection is
/// opened and dropped within this call, so nothing is cached across
/// requests. Every recoverable failure degrades to an unavailable panel
/// carrying the mapped shed code -- never a stale payload.
fn build_panel(
    db_path: &Path,
    token: &'static str,
    bounds: WindowBounds,
    as_of: String,
) -> Panel<UsagePanel> {
    let db = match open_readonly_fastfail(db_path) {
        Ok(db) => db,
        Err(err) => return Panel::unavailable(SCHEMA_VERSION, open_error_code(&err)),
    };
    match collect(&db, token, bounds) {
        Ok(dto) => Panel::available(SCHEMA_VERSION, as_of, dto),
        Err(code) => Panel::unavailable(SCHEMA_VERSION, code),
    }
}

/// Run the three read-only aggregate queries and assemble the DTO. On a
/// query failure, returns the mapped shed code.
fn collect(
    db: &UsageDb,
    token: &'static str,
    bounds: WindowBounds,
) -> Result<UsagePanel, &'static str> {
    let rows = aggregate(db, bounds.from_ms, bounds.to_ms).map_err(|e| query_error_code(&e))?;
    let breakdown =
        errors_by_class(db, bounds.from_ms, bounds.to_ms).map_err(|e| query_error_code(&e))?;
    let quota = latest_quota_by_seat(db).map_err(|e| query_error_code(&e))?;
    let would_trim =
        would_trim_summary(db, bounds.from_ms, bounds.to_ms).map_err(|e| query_error_code(&e))?;
    Ok(assemble(token, bounds, rows, breakdown, quota, would_trim))
}

/// Fold the query results into the wire DTO. Totals are summed over the
/// rollup groups. The flat error breakdown is merged into a per-group
/// `class -> count` map keyed by the shared group key, then accumulated into
/// the window-wide totals map.
fn assemble(
    token: &'static str,
    bounds: WindowBounds,
    rows: Vec<AggRow>,
    breakdown: Vec<(GroupKey, String, i64)>,
    quota: Vec<QuotaSnapshot>,
    would_trim: WouldTrimSummary,
) -> UsagePanel {
    let mut per_group: std::collections::HashMap<GroupKey, BTreeMap<String, i64>> =
        std::collections::HashMap::new();
    let mut totals_by_class: BTreeMap<String, i64> = BTreeMap::new();
    for (key, class, count) in breakdown {
        *totals_by_class.entry(class.clone()).or_default() += count;
        *per_group.entry(key).or_default().entry(class).or_default() += count;
    }

    let mut totals = UsageTotals::default();
    let mut groups = Vec::with_capacity(rows.len());
    for row in rows {
        totals.requests += row.requests;
        totals.ok += row.ok;
        totals.errors += row.errors;
        totals.input_tokens += row.input_tokens;
        totals.output_tokens += row.output_tokens;
        totals.reasoning_tokens += row.reasoning_tokens;
        totals.cache_read_billed += row.cache_read_billed;
        totals.server_tool_calls += row.server_tool_calls;
        totals.client_disconnect_total += row.client_disconnect_total;
        totals.cache_read_present += row.cache_read_present;
        let group_classes = per_group.remove(&row.key).unwrap_or_default();
        groups.push(map_group(row, group_classes));
    }
    totals.errors_by_class = totals_by_class;
    UsagePanel {
        window: token,
        from_ms: bounds.from_ms,
        to_ms: bounds.to_ms,
        totals,
        groups,
        quota: quota.into_iter().map(map_quota).collect(),
        would_trim: map_would_trim(would_trim),
    }
}

fn map_group(row: AggRow, errors_by_class: BTreeMap<String, i64>) -> UsageGroup {
    UsageGroup {
        alias: row.key.alias,
        provider: row.key.provider,
        model: row.key.model,
        upstream: row.key.upstream,
        requests: row.requests,
        ok: row.ok,
        errors: row.errors,
        input_tokens: row.input_tokens,
        output_tokens: row.output_tokens,
        reasoning_tokens: row.reasoning_tokens,
        cache_read_peak: row.cache_read_peak,
        cache_read_billed: row.cache_read_billed,
        cache_write_5m: row.cache_write_5m,
        cache_write_1h: row.cache_write_1h,
        server_tool_calls: row.server_tool_calls,
        stream_count: row.stream_count,
        client_disconnect_total: row.client_disconnect_total,
        errors_by_class,
        cache_read_present: row.cache_read_present,
    }
}

fn map_quota(snapshot: QuotaSnapshot) -> UsageQuota {
    UsageQuota {
        seat: snapshot.seat,
        provider_kind: snapshot.provider_kind,
        ts_start_ms: snapshot.ts_start,
        claim: snapshot.claim,
        status: snapshot.status,
        overage_status: snapshot.overage_status,
        utilization: snapshot.utilization,
        overage_utilization: snapshot.overage_utilization,
        // Ledger quota resets are epoch SECONDS; scale to ms so the `_ms`
        // field name is truthful and the client's ms formatter is correct.
        reset_ms: snapshot.reset.and_then(|s| s.checked_mul(1000)),
    }
}

const fn map_would_trim(summary: WouldTrimSummary) -> UsageWouldTrim {
    UsageWouldTrim {
        candidate_requests: summary.candidate_requests,
        would_trim_tokens: summary.would_trim_tokens,
        verdict_met: summary.verdict_met,
        verdict_unmet: summary.verdict_unmet,
        verdict_cold: summary.verdict_cold,
        verdict_unpriced: summary.verdict_unpriced,
    }
}

/// Map a ledger-open failure to its `Panel::unavailable` shed code. A busy
/// or locked DB under the fast-fail timeout is distinct from a missing or
/// otherwise unreadable file. Shared with the `/status/query` handler so the
/// two ledger-backed surfaces classify an open failure identically.
pub(super) fn open_error_code(err: &OpenError) -> &'static str {
    match err {
        OpenError::NoData { .. } => codes::NO_DATA,
        OpenError::VersionTooNew { .. } | OpenError::VersionTooOld { .. } => codes::SCHEMA_MISMATCH,
        OpenError::Open { source, .. } | OpenError::Pragma(source) => {
            busy_or_unavailable(source.sqlite_error_code())
        }
        _ => codes::DB_UNAVAILABLE,
    }
}

/// Map a query failure to its `Panel::unavailable` shed code. Shared with the
/// `/status/query` handler so the two ledger-backed surfaces classify a read
/// failure identically.
///
/// A fired deadline is its OWN code: the ledger is healthy and the window is
/// simply too large to answer inside the budget, which is a different operator
/// action than a busy or unreadable database. This panel installs no progress
/// handler and asks for no time series, so neither `Interrupted` nor
/// `InvalidBucket` is reachable from it; both are mapped rather than panicked so
/// the never-500 posture holds if that ever changes.
pub(super) fn query_error_code(err: &QueryError) -> &'static str {
    match err {
        QueryError::Sqlite(source) => busy_or_unavailable(source.sqlite_error_code()),
        QueryError::Interrupted => codes::QUERY_TIMEOUT,
        // The bucket grid is resolved caller-side, so an unusable one is a bug
        // rather than operator input.
        QueryError::InvalidBucket => codes::DB_UNAVAILABLE,
    }
}

/// Whether a SQLite error code names transient contention (shed as busy, worth
/// a retry) rather than an unusable source. Shared with the `/status/query`
/// handler so both classify contention identically.
pub(super) const fn busy_or_unavailable(code: Option<ErrorCode>) -> &'static str {
    if matches!(
        code,
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked)
    ) {
        codes::DB_BUSY
    } else {
        codes::DB_UNAVAILABLE
    }
}

#[cfg(test)]
#[path = "usage_equivalence_tests.rs"]
mod equivalence_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::status::DaemonMeta;
    use crate::server::AppState;
    use arc_swap::ArcSwap;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use routectl_router::{Config, Router};
    use routectl_usage::{SCHEMA_VERSION as LEDGER_SCHEMA_VERSION, open};
    use serde_json::Value;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tower::ServiceExt;

    const DAY_MS: i64 = 86_400_000;

    /// Build a `StatusState` whose usage-ledger path is `db_path`.
    fn state_with_ledger(db_path: PathBuf) -> Arc<StatusState> {
        let router = Router::new(Arc::new(Config::default()));
        // The writer's tempdir is unused here (the status panel never touches
        // the writer handle); let it drop.
        let (app, _writer_dir) = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
        let mut status = StatusState::from_app(&app, None, DaemonMeta::for_test());
        status.usage_db_path = db_path;
        Arc::new(status)
    }

    /// Seed a WAL ledger with one `ok` row per timestamp, all sharing the
    /// same group key so windowing collapses them into a single group.
    fn seed_ledger(path: &Path, timestamps: &[i64]) {
        let db = open(path).expect("open ledger");
        for (i, ts) in timestamps.iter().enumerate() {
            db.conn()
                .execute(
                    "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                     requested_model, alias, model, provider, upstream, stream, outcome, \
                     latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
                     input_tokens, output_tokens) \
                     VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 'm', 'p', 'u', 0, 'ok', \
                     5, 0, 0, 1, 0, 10, 20)",
                    rusqlite::params![ts, format!("r{i}")],
                )
                .expect("seed row");
        }
    }

    async fn get_usage(state: Arc<StatusState>, uri: &str) -> (StatusCode, Value) {
        let app = super::super::status_router().with_state(state);
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        (status, json)
    }

    #[tokio::test]
    async fn windowed_aggregates_respect_window_and_default_is_today() {
        // Arrange: a row inside today's window and a row 40 days ago. The
        // old row falls outside today / this-week / this-month but inside
        // all-time; both share one group key.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("usage.db");
        let now_ms = Local::now().timestamp_millis();
        seed_ledger(&path, &[now_ms, now_ms - 40 * DAY_MS]);

        // Act + Assert: default (no query) == today == 1 request.
        for uri in [
            "/status/usage",
            "/status/usage?window=today",
            "/status/usage?window=week",
            "/status/usage?window=month",
        ] {
            let (status, json) = get_usage(state_with_ledger(path.clone()), uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            assert_eq!(json["data"]["totals"]["requests"], 1, "{uri}");
        }

        // all-time includes both rows in the single group.
        let (status, json) =
            get_usage(state_with_ledger(path.clone()), "/status/usage?window=all").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["data"]["window"], "all");
        assert_eq!(json["data"]["totals"]["requests"], 2);
        assert_eq!(json["data"]["groups"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn empty_healthy_ledger_renders_available_with_zeroed_data() {
        // A fresh, healthy WAL ledger with no rows must render an AVAILABLE
        // panel (data present, unavailable null) rather than shedding to
        // db_unavailable. Regression: the would-trim summary's verdict
        // aggregates returned SQL NULL over zero rows, which the row mapping
        // read as a non-nullable i64 and surfaced as a query error.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("usage.db");
        // Create the ledger (WAL, migrated) but insert no rows.
        drop(open(&path).expect("open empty ledger"));

        let (status, json) =
            get_usage(state_with_ledger(path.clone()), "/status/usage?window=all").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            json["unavailable"].is_null(),
            "empty healthy ledger must not be unavailable: {json}"
        );
        assert!(
            !json["data"].is_null(),
            "empty healthy ledger must carry a data payload: {json}"
        );
        assert_eq!(json["data"]["totals"]["requests"], 0);
        assert_eq!(json["data"]["would_trim"]["candidate_requests"], 0);
        assert_eq!(json["data"]["would_trim"]["verdict_met"], 0);
        assert_eq!(
            json["data"]["quota"].as_array().map(Vec::len),
            Some(0),
            "an empty ledger reports zero seats, not a missing field: {json}"
        );
    }

    /// One seeded quota row: `(request_id, seat, provider_kind, quota_status,
    /// quota_utilization, quota_reset)`. `quota_reset` is epoch SECONDS, the
    /// unit the ledger stores.
    type SeatQuotaSeed<'a> = (
        &'a str,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
        Option<f64>,
        Option<i64>,
    );

    /// Seed one quota-bearing row per [`SeatQuotaSeed`], each at a distinct
    /// `ts_start` so the per-seat newest-row resolution is deterministic.
    fn seed_seat_quota_ledger(path: &Path, rows: &[SeatQuotaSeed<'_>]) {
        let db = open(path).expect("open ledger");
        let now = Local::now().timestamp_millis();
        for (i, (id, seat, provider_kind, status, utilization, reset_s)) in rows.iter().enumerate()
        {
            db.conn()
                .execute(
                    "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                     requested_model, alias, model, provider, upstream, stream, outcome, \
                     latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
                     seat, provider_kind, quota_status, quota_utilization, quota_reset) \
                     VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 'm', 'p', 'u', 0, 'ok', \
                     5, 0, 0, 1, 0, ?3, ?4, ?5, ?6, ?7)",
                    rusqlite::params![
                        now + i64::try_from(i).unwrap_or(0),
                        id,
                        seat,
                        provider_kind,
                        status,
                        utilization,
                        reset_s
                    ],
                )
                .expect("seed seat quota row");
        }
    }

    #[tokio::test]
    async fn quota_renders_one_entry_per_seat_with_reset_scaled_from_seconds() {
        // Arrange: two seats reporting different vendor shapes -- an Anthropic
        // row with a status token and a codex row with utilization only -- plus
        // an older row for the Anthropic seat that the per-seat read supersedes.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("usage.db");
        seed_seat_quota_ledger(
            &path,
            &[
                (
                    "a-old",
                    Some("anthropic#a"),
                    Some("anthropic-api"),
                    Some("allowed"),
                    Some(0.1),
                    Some(1_000),
                ),
                (
                    "a-new",
                    Some("anthropic#a"),
                    Some("anthropic-api"),
                    Some("throttled"),
                    Some(0.9),
                    Some(9_000),
                ),
                (
                    "c-1",
                    Some("codex"),
                    Some("codex"),
                    None,
                    Some(0.16),
                    Some(1_786_210_114),
                ),
            ],
        );

        // Act
        let (status, json) =
            get_usage(state_with_ledger(path.clone()), "/status/usage?window=all").await;

        // Assert
        assert_eq!(status, StatusCode::OK);
        let quota = json["data"]["quota"].as_array().expect("quota array");
        assert_eq!(quota.len(), 2, "one entry per seat: {json}");
        let anthropic = quota
            .iter()
            .find(|q| q["seat"] == "anthropic#a")
            .expect("anthropic seat present");
        assert_eq!(anthropic["provider_kind"], "anthropic-api");
        assert_eq!(anthropic["status"], "throttled", "the newer row wins");
        assert_eq!(anthropic["reset_ms"], 9_000_000, "seconds scaled to ms");
        let codex = quota
            .iter()
            .find(|q| q["seat"] == "codex")
            .expect("codex seat present");
        assert_eq!(codex["provider_kind"], "codex");
        assert!(
            codex["status"].is_null(),
            "codex reports no status token: {codex}"
        );
        assert_eq!(codex["utilization"], 0.16);
        assert_eq!(codex["reset_ms"], 1_786_210_114_000_i64);
    }

    #[tokio::test]
    async fn quota_gives_a_pre_seat_row_a_null_seat_entry() {
        // Pre-seat history must stay visible under its own null-seat bucket
        // rather than being filtered or given a synthetic seat token.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("usage.db");
        seed_seat_quota_ledger(
            &path,
            &[("legacy", None, None, Some("allowed"), Some(0.2), None)],
        );

        let (status, json) =
            get_usage(state_with_ledger(path.clone()), "/status/usage?window=all").await;
        assert_eq!(status, StatusCode::OK);
        let quota = json["data"]["quota"].as_array().expect("quota array");
        assert_eq!(quota.len(), 1);
        assert!(quota[0]["seat"].is_null(), "{json}");
        assert!(quota[0]["reset_ms"].is_null());
    }

    #[tokio::test]
    async fn per_request_open_reflects_a_ledger_deleted_between_requests() {
        // A cached connection would keep reading a deleted file; a
        // per-request open sheds to no_data once the file is gone.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("usage.db");
        let now_ms = Local::now().timestamp_millis();
        seed_ledger(&path, &[now_ms]);

        let (status, json) = get_usage(state_with_ledger(path.clone()), "/status/usage").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["data"]["totals"]["requests"], 1);

        // Drop the WAL sidecars + main file, then re-request.
        for suffix in ["", "-wal", "-shm"] {
            let mut name = path.clone().into_os_string();
            name.push(suffix);
            let _ = std::fs::remove_file(PathBuf::from(name));
        }
        let (status, json) = get_usage(state_with_ledger(path.clone()), "/status/usage").await;
        assert_eq!(status, StatusCode::OK, "missing ledger is still HTTP 200");
        assert_eq!(json["unavailable"], "no_data");
        assert!(json["as_of"].is_null());
        assert!(json["data"].is_null());
    }

    #[tokio::test]
    async fn missing_ledger_renders_no_data_with_null_as_of() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("absent.db");
        let (status, json) = get_usage(state_with_ledger(path), "/status/usage").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["unavailable"], "no_data");
        assert!(json["as_of"].is_null());
        assert!(json["data"].is_null());
    }

    #[tokio::test]
    async fn schema_mismatch_renders_schema_mismatch() {
        // A DB whose user_version is beyond this binary is refused.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("future.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", LEDGER_SCHEMA_VERSION + 1)
            .unwrap();
        drop(conn);

        let (status, json) = get_usage(state_with_ledger(path), "/status/usage").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["unavailable"], "schema_mismatch");
        assert!(json["as_of"].is_null());
    }

    #[tokio::test]
    async fn non_wal_ledger_renders_db_unavailable() {
        // A version-matched DB with a non-WAL journal is refused closed.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("delete.db");
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "journal_mode", "DELETE").unwrap();
        conn.pragma_update(None, "user_version", LEDGER_SCHEMA_VERSION)
            .unwrap();
        conn.execute("CREATE TABLE requests (x INTEGER)", [])
            .unwrap();
        drop(conn);

        let (status, json) = get_usage(state_with_ledger(path), "/status/usage").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["unavailable"], "db_unavailable");
    }

    #[test]
    fn quota_reset_seconds_scaled_to_milliseconds() {
        // The ledger stamps quota resets in epoch SECONDS; the panel field is
        // named `reset_ms`, so the seconds value must be scaled by 1000. A
        // `None` reset stays `None`. Seat and provider_kind pass through as the
        // client's discriminator for the shared quota columns.
        let snapshot = QuotaSnapshot {
            seat: Some("anthropic#a".into()),
            provider_kind: Some("anthropic-api".into()),
            ts_start: 1_700_000_000_000,
            claim: None,
            status: Some("ok".into()),
            overage_status: None,
            utilization: Some(0.5),
            overage_utilization: None,
            reset: Some(9_000),
        };
        let mapped = map_quota(snapshot);
        assert_eq!(
            mapped.reset_ms,
            Some(9_000_000),
            "9000s must map to 9_000_000ms"
        );
        assert_eq!(
            mapped.ts_start_ms, 1_700_000_000_000,
            "ts_start passes through as ms"
        );
        assert_eq!(mapped.seat.as_deref(), Some("anthropic#a"));
        assert_eq!(mapped.provider_kind.as_deref(), Some("anthropic-api"));

        let no_reset = QuotaSnapshot {
            seat: None,
            provider_kind: None,
            ts_start: 0,
            claim: None,
            status: None,
            overage_status: None,
            utilization: None,
            overage_utilization: None,
            reset: None,
        };
        let mapped = map_quota(no_reset);
        assert_eq!(mapped.reset_ms, None, "absent reset stays absent");
        assert_eq!(mapped.seat, None, "a pre-seat row renders a null seat");
    }

    #[test]
    fn quota_reset_overflowing_millisecond_scale_maps_to_none() {
        let snapshot = QuotaSnapshot {
            seat: None,
            provider_kind: None,
            ts_start: 0,
            claim: None,
            status: None,
            overage_status: None,
            utilization: None,
            overage_utilization: None,
            reset: Some(i64::MAX / 1000 + 1),
        };
        let mapped = map_quota(snapshot);
        assert_eq!(
            mapped.reset_ms, None,
            "a reset that overflows the ms scale drops the field instead of panicking"
        );
    }

    #[test]
    fn parse_window_defaults_unknown_and_absent_to_today() {
        assert_eq!(parse_window(None).1, "today");
        assert_eq!(parse_window(Some("bogus")).1, "today");
        assert!(matches!(parse_window(Some("bogus")).0, WindowFlag::Today));
        assert_eq!(parse_window(Some("week")).1, "week");
        assert_eq!(parse_window(Some("month")).1, "month");
        assert_eq!(parse_window(Some("all")).1, "all");
    }

    #[test]
    fn open_error_code_maps_every_variant() {
        assert_eq!(
            open_error_code(&OpenError::NoData { path: "p".into() }),
            codes::NO_DATA
        );
        assert_eq!(
            open_error_code(&OpenError::VersionTooNew {
                found: 99,
                supported: 1
            }),
            codes::SCHEMA_MISMATCH
        );
        assert_eq!(
            open_error_code(&OpenError::VersionTooOld {
                found: 1,
                supported: 2
            }),
            codes::SCHEMA_MISMATCH
        );
        assert_eq!(
            open_error_code(&OpenError::NotWal {
                found: "delete".into()
            }),
            codes::DB_UNAVAILABLE
        );
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        );
        assert_eq!(
            open_error_code(&OpenError::Open {
                path: "p".into(),
                source: busy
            }),
            codes::DB_BUSY
        );
        let locked = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED),
            None,
        );
        assert_eq!(open_error_code(&OpenError::Pragma(locked)), codes::DB_BUSY);
    }

    #[test]
    fn query_error_code_maps_every_variant() {
        // The interrupt is NOT a broken database: a fired deadline gets
        // query_timeout, a lock gets db_busy, and anything else db_unavailable.
        assert_eq!(
            query_error_code(&QueryError::Interrupted),
            codes::QUERY_TIMEOUT
        );
        assert_eq!(
            query_error_code(&QueryError::InvalidBucket),
            codes::DB_UNAVAILABLE
        );
        let locked = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED),
            None,
        );
        assert_eq!(
            query_error_code(&QueryError::Sqlite(locked)),
            codes::DB_BUSY
        );
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        );
        assert_eq!(query_error_code(&QueryError::Sqlite(busy)), codes::DB_BUSY);
        let corrupt = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            None,
        );
        assert_eq!(
            query_error_code(&QueryError::Sqlite(corrupt)),
            codes::DB_UNAVAILABLE
        );
    }

    #[test]
    fn payload_carries_only_aggregates() {
        let bounds = WindowBounds {
            from_ms: 0,
            to_ms: 100,
        };
        let panel = assemble(
            "all",
            bounds,
            vec![AggRow {
                key: routectl_usage::GroupKey {
                    model: Some("m".into()),
                    provider: Some("p".into()),
                    upstream: Some("u".into()),
                    alias: "a".into(),
                },
                requests: 3,
                ok: 2,
                errors: 1,
                input_tokens: 10,
                output_tokens: 20,
                reasoning_tokens: 0,
                cache_read_peak: 5,
                cache_read_avg: 5,
                cache_read_billed: 15,
                cache_write_5m: 0,
                cache_write_1h: 0,
                server_tool_calls: 0,
                sum_ttfb_ms: 0,
                ttfb_count: 0,
                gen_window_ms: 0,
                gen_output_tokens: 0,
                reasoning_present: 0,
                cache_read_present: 0,
                cache_write_5m_present: 0,
                cache_write_1h_present: 0,
                server_tool_present: 0,
                stream_count: 0,
                client_disconnect_total: 0,
                client_disconnect_pre_dispatch: 0,
            }],
            vec![(
                GroupKey {
                    model: Some("m".into()),
                    provider: Some("p".into()),
                    upstream: Some("u".into()),
                    alias: "a".into(),
                },
                "http-5xx".into(),
                1,
            )],
            Vec::new(),
            WouldTrimSummary::default(),
        );
        let text = serde_json::to_string(&panel).unwrap();
        for forbidden in ["request_id", "prompt", "message", "\"body\"", "session_id"] {
            assert!(
                !text.contains(forbidden),
                "usage payload must not carry `{forbidden}`: {text}"
            );
        }
        // The aggregate shape IS present.
        for expected in [
            "totals",
            "groups",
            "would_trim",
            "requests",
            "errors_by_class",
        ] {
            assert!(
                text.contains(expected),
                "missing aggregate field {expected}"
            );
        }
    }

    /// Seed a WAL ledger with rows carrying an explicit outcome and (nullable)
    /// resolved_class so the error-breakdown paths are exercisable through the
    /// full HTTP panel. All rows share one group key.
    fn seed_class_ledger(path: &Path, rows: &[(&str, Option<&str>)]) {
        let db = open(path).expect("open ledger");
        let now = Local::now().timestamp_millis();
        for (i, (outcome, class)) in rows.iter().enumerate() {
            db.conn()
                .execute(
                    "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                     requested_model, alias, model, provider, upstream, stream, outcome, \
                     latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
                     input_tokens, output_tokens, cache_read, resolved_class) \
                     VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 'm', 'p', 'u', 0, ?3, \
                     5, 0, 0, 1, 0, 10, 20, ?4, ?5)",
                    rusqlite::params![now, format!("r{i}"), outcome, 100, class],
                )
                .expect("seed class row");
        }
    }

    #[tokio::test]
    async fn errors_by_class_sums_to_errors_per_group_and_totals() {
        // Arrange: one group, mixed outcomes incl. a NULL-class error row and
        // excluded ok / client_disconnect rows.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("usage.db");
        seed_class_ledger(
            &path,
            &[
                ("ok", None),
                ("client_disconnect", None),
                ("upstream_error", Some("http-5xx")),
                ("upstream_error", Some("http-5xx")),
                ("gate_blocked", None),
                ("upstream_error", Some("timeout")),
            ],
        );

        let (status, json) =
            get_usage(state_with_ledger(path.clone()), "/status/usage?window=all").await;
        assert_eq!(status, StatusCode::OK);

        // The single group's breakdown sums to its errors and to totals.
        let group = &json["data"]["groups"][0];
        assert_eq!(group["errors"], 4);
        assert_eq!(group["errors_by_class"]["http-5xx"], 2);
        assert_eq!(group["errors_by_class"]["unclassified"], 1);
        assert_eq!(group["errors_by_class"]["timeout"], 1);

        let totals = &json["data"]["totals"];
        assert_eq!(totals["errors"], 4);
        assert_eq!(totals["errors_by_class"]["http-5xx"], 2);
        assert_eq!(totals["errors_by_class"]["unclassified"], 1);
        assert_eq!(totals["errors_by_class"]["timeout"], 1);
        let breakdown_sum: i64 = totals["errors_by_class"]
            .as_object()
            .unwrap()
            .values()
            .map(|v| v.as_i64().unwrap())
            .sum();
        assert_eq!(breakdown_sum, 4);
    }

    #[tokio::test]
    async fn requests_reconcile_ok_errors_disconnect_at_totals() {
        // REQ = OK + ERR + DISC at totals, with client_disconnect_total and
        // cache_read_present surfaced.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("usage.db");
        seed_class_ledger(
            &path,
            &[
                ("ok", None),
                ("ok", None),
                ("upstream_error", Some("http-5xx")),
                ("client_disconnect", None),
            ],
        );

        let (status, json) =
            get_usage(state_with_ledger(path.clone()), "/status/usage?window=all").await;
        assert_eq!(status, StatusCode::OK);

        let totals = &json["data"]["totals"];
        let requests = totals["requests"].as_i64().unwrap();
        let ok = totals["ok"].as_i64().unwrap();
        let errors = totals["errors"].as_i64().unwrap();
        let disc = totals["client_disconnect_total"].as_i64().unwrap();
        assert_eq!(requests, 4);
        assert_eq!(ok, 2);
        assert_eq!(errors, 1);
        assert_eq!(disc, 1);
        assert_eq!(requests, ok + errors + disc);
        // Every seeded row reported a cache_read, so the reporting-only
        // denominator equals the request count here.
        assert_eq!(totals["cache_read_present"], 4);
    }

    /// The migration race at the panel boundary: a pre-migration v11 snapshot
    /// (the `resolved_class` column not yet added) must read back through
    /// `GET /status/usage` as an unavailable panel -- fail-closed, never a
    /// mixed-schema read or a 500 -- and become available once the writer has
    /// migrated it to v12. The read-only open's version guard is unit-covered
    /// in `routectl_usage::db`; this pins the HTTP-panel envelope on each side
    /// of the migration.
    #[tokio::test]
    async fn v11_ledger_fails_closed_at_panel_then_available_after_migration() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("usage.db");

        // Build a full current-schema ledger with rows, then roll it back to
        // the exact shape the prior binary wrote: drop the v12 column and stamp
        // the old user_version. This is a genuine pre-ALTER v11 file.
        seed_class_ledger(&path, &[("ok", None), ("upstream_error", Some("http-5xx"))]);
        {
            let db = open(&path).expect("reopen ledger");
            db.conn()
                .execute_batch(
                    "ALTER TABLE requests DROP COLUMN resolved_class; \
                     PRAGMA user_version = 11;",
                )
                .expect("roll back to v11 shape");
        }

        // Fail-closed: the read-only panel refuses the older schema.
        let (status, json) =
            get_usage(state_with_ledger(path.clone()), "/status/usage?window=all").await;
        assert_eq!(status, StatusCode::OK, "an older-schema DB never 500s");
        assert_eq!(
            json["unavailable"],
            codes::SCHEMA_MISMATCH,
            "a pre-resolved_class v11 DB must fail closed as schema_mismatch: {json}"
        );
        assert!(json["data"].is_null(), "unavailable panel carries no data");
        assert!(
            json["as_of"].is_null(),
            "unavailable panel carries no as_of"
        );

        // The writer migrates v11 -> v12 in place; the panel then reads it.
        open(&path).expect("migrate v11 -> v12");

        let (status, json) =
            get_usage(state_with_ledger(path.clone()), "/status/usage?window=all").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            json["unavailable"].is_null(),
            "a migrated v12 DB must read as available: {json}"
        );
        assert_eq!(json["schema_version"], 3);
        assert_eq!(json["data"]["totals"]["requests"], 2);
    }

    #[tokio::test]
    async fn empty_window_renders_empty_class_maps() {
        // An error row 40 days ago falls outside today's window: the panel's
        // error breakdown maps must serialize as empty objects, not absent.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("usage.db");
        let db = open(&path).expect("open ledger");
        let old = Local::now().timestamp_millis() - 40 * DAY_MS;
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
                 resolved_class) \
                 VALUES (?1, ?1, 'old', 'openai', 'm', 'a', 'm', 'p', 'u', 0, \
                 'upstream_error', 5, 0, 0, 1, 0, 'http-5xx')",
                rusqlite::params![old],
            )
            .expect("seed old error row");
        drop(db);

        let (status, json) = get_usage(state_with_ledger(path.clone()), "/status/usage").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["data"]["totals"]["requests"], 0);
        // The totals breakdown is present and empty (serializes as {}).
        assert!(json["data"]["totals"]["errors_by_class"].is_object());
        assert_eq!(
            json["data"]["totals"]["errors_by_class"]
                .as_object()
                .unwrap()
                .len(),
            0
        );
        // No groups in an empty window.
        assert_eq!(json["data"]["groups"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn usage_panel_reports_schema_version_three() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("usage.db");
        seed_ledger(&path, &[Local::now().timestamp_millis()]);
        let (status, json) = get_usage(state_with_ledger(path), "/status/usage").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(json["schema_version"], 3);
    }

    /// Poll-loop safety: a panel that stays unavailable across repeated polls
    /// logs at most ONE into-unavailable line (a fresh-install missing DB is a
    /// single DEBUG edge, never a per-poll warn), and that line carries only
    /// the fixed reason code -- no path, secret, or raw error.
    #[tokio::test]
    async fn missing_ledger_logs_one_transition_across_repeated_polls() {
        use routectl_testkit::with_capture;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("absent.db");
        let state = state_with_ledger(path.clone());

        let ((), events) = with_capture(async {
            for _ in 0..5 {
                let _ = build(&state).await;
            }
        })
        .await;

        let edges: Vec<_> = events
            .iter()
            .filter(|e| e.field("panel") == Some("usage"))
            .collect();
        assert_eq!(
            edges.len(),
            1,
            "5 polls of a persistently-unavailable panel must log one edge, got {}",
            edges.len()
        );
        assert_eq!(
            edges[0].level,
            tracing::Level::DEBUG,
            "a missing DB is a debug edge, not a warn"
        );
        assert_eq!(edges[0].field("code"), Some("no_data"));

        let rendered = format!("{} {:?}", edges[0].message, edges[0].fields);
        assert!(
            !rendered.contains(&path.display().to_string()) && !rendered.contains("absent.db"),
            "transition line leaked the DB path: {rendered}"
        );
    }

    /// An availability flip logs once in EACH direction: one into-unavailable
    /// edge while the ledger is absent, one back-to-available edge once it is
    /// seeded -- never a line per intervening poll.
    #[tokio::test]
    async fn availability_flip_logs_once_in_each_direction() {
        use routectl_testkit::with_capture;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("usage.db");
        let state = state_with_ledger(path.clone());

        let ((), events) = with_capture(async {
            for _ in 0..3 {
                let _ = build(&state).await;
            }
            seed_ledger(&path, &[Local::now().timestamp_millis()]);
            for _ in 0..3 {
                let _ = build(&state).await;
            }
        })
        .await;

        let edges: Vec<_> = events
            .iter()
            .filter(|e| e.field("panel") == Some("usage"))
            .collect();
        assert_eq!(
            edges.len(),
            2,
            "one edge each way, no per-poll spam, got {}",
            edges.len()
        );
        assert_eq!(edges[0].field("code"), Some("no_data"));
        assert!(
            edges[1].field("code").is_none(),
            "the back-to-available edge carries no reason code"
        );
    }
}
