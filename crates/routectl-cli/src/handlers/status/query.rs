//! `/status/query` -- the QUERY-method grouped aggregate over the usage
//! ledger.
//!
//! Three response regimes, mutually exclusive by construction:
//!   - **405** -- the method is not `QUERY`. The route is registered with
//!     `any()` because axum's `MethodFilter` cannot express a non-standard
//!     method, so the guard here IS the method router for this path.
//!   - **400** -- the body is not in the closed request vocabulary, is larger
//!     than this route answers, or never finished arriving. A fixed
//!     transport-level envelope, mirroring the status gate's 403/503: it never
//!     echoes the serde error text or any byte of the body, and the ledger is
//!     never opened on this path.
//!   - **200** -- everything else, including every data-source failure. The
//!     aggregate runs under the same panic isolation as the other panels (a
//!     blocking worker + `catch_unwind`, a per-request read-only fast-fail
//!     open), so a missing, busy, mismatched, or over-budget ledger degrades
//!     to an unavailable `Panel`, never a 500 -- and the payload is serialized
//!     explicitly so a render failure degrades the same way instead of
//!     escaping as a fourth regime.
//!
//! The read does NOT take a GET panel's builder capacity. It runs under its
//! own one-permit [`LedgerReadGate`], taken without waiting: a second query
//! while one is reading answers `db_busy` at once, and a long read never holds
//! a permit the live panels need. The read's deadline is fixed before the gate
//! and the blocking-worker queue, so both count against it, and a request
//! dropped mid-read sets the read's [`CancelFlag`], which stops it at the next
//! check and frees the gate.
//!
//! Cost is resolved through the read-only router facade's pricer, which pins
//! ONE config snapshot and ONE price-table snapshot for the whole request: a
//! hot-swap or a new price table mid-query can never make two rows of one
//! result price against different rate tables. A `seat` grouping names each
//! group's account from that same snapshot.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use time::OffsetDateTime;

use crate::local_time::LocalZone;
use serde::{Deserialize, Serialize};
use serde_json::json;

use routectl_router::AccountIdentities;
use routectl_usage::{
    CancelFlag, GroupDim, QueryMetrics, QueryResult, QuerySeries, QuerySpec, QueryTotals,
    ReadLimit, UNATTRIBUTED, earliest_ts_start, open_readonly_fastfail, query,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};

use crate::commands::usage::{BucketUnit, WindowFlag, resolve_bucket, window_bounds};
use crate::server::status_gate::QUERY_BUDGET_MS;

use super::router_view::QueryPricer;
use super::usage::{open_error_code, query_error_code};
use super::vocabulary::codes;
use super::{Panel, StatusState, now_utc_rfc3339, run_blocking_panel};

/// Wire-shape version of the query payload. Additive changes (a new metric, a
/// new `group_by` or `window` token) do NOT bump it; a semantic change to an
/// existing field, or a removal, does. At 3 every metric set gained
/// `cache_write_tokens` and `cache_write_present`, the cache writes of both
/// ledger eras, and `cache_write_5m` / `cache_write_1h` narrowed to rows written
/// before the ledger recorded a write total.
///
/// At 4 three figures changed meaning and four fields were renamed:
/// `throughput_tok_s` is summed generation output over summed generation time
/// (was a mean of per-request rates); `ctx_avg` / `ctx_peak` count fresh
/// input plus cache read plus cache write (were fresh input only);
/// `cache_hit_pct` is token-weighted over the rows reporting cache reads (was a
/// mean of per-request shares); the TTFT and latency p50 / p95 pairs, a mean
/// and a maximum, are now
/// `ttft_avg_ms` / `ttft_max_ms` / `latency_avg_ms` / `latency_max_ms`.
/// Added alongside, without a bump of their own: the `seat` grouping, the
/// `last24h` / `last7d` windows, per metric set `errors_by_class`,
/// `cache_read_present`, `server_tool_present` and
/// `client_disconnect_pre_dispatch`, and on every seat group `account`,
/// `provider_entry` and `login_label`.
///
/// At 5 the `seat` grouping changed meaning: a row with no recorded seat that
/// was served by an entry whose account is the entry itself (an API key or a
/// cloud credential) groups under that `#<entry>` account, as the served
/// config names it, rather than under `(unattributed)`.
pub const SCHEMA_VERSION: u32 = 5;

/// The one method this route answers. Not expressible as an axum
/// `MethodFilter`, hence the manual guard.
const QUERY_METHOD: &str = "QUERY";

/// Fixed code for a body outside the request vocabulary. The only 400 this
/// route emits.
const INVALID_QUERY: &str = "invalid_query";

/// Ceiling on the request body, bytes. A query body is a handful of short
/// tokens; anything larger is not a vocabulary this route can answer, so it is
/// refused as an invalid query rather than buffered.
const MAX_BODY_BYTES: usize = 8 * 1024;

/// How long the handler waits for the client to finish sending its body.
///
/// This route is the only `/status` path that awaits a client-controlled body
/// while holding a concurrency permit, and the byte ceiling above only trips on
/// bytes that actually ARRIVE. Without a deadline, a handful of connections that
/// announce a body and then stall mid-send would hold every permit for as long
/// as they keep the socket open and wedge the whole status surface, so a stalled
/// send is refused as an invalid query and gives its permit back.
///
/// One second, not two, and that STRENGTHENS the guard rather than weakening
/// it: `MAX_BODY_BYTES` is 8 KiB, so a full-size body arriving over loopback
/// within this deadline needs only 8 KB/s, and a sender slower than that is
/// exactly the stalled sender this timeout exists to shed. A legitimate query
/// body is a handful of short tokens and arrives in one segment.
///
/// It is also one term of a serial sum -- the build below runs under
/// `QUERY_BUDGET_MS` only AFTER this read completes -- so it cannot be changed
/// on its own. The derivation lives in `crate::server::status_gate`'s module
/// docs.
pub(super) const BODY_READ_TIMEOUT: Duration = Duration::from_secs(1);

/// The one-permit gate every `/status/query` ledger read runs under.
///
/// Taken WITHOUT waiting: a held gate answers `db_busy` at once. A queue would
/// let client retries pile up behind an abandoned read and spend their budgets
/// waiting. The permit is moved into the blocking closure, so it is released
/// when the read ends, never by the request future dropping; what makes an
/// abandoned read end early is the cancel its request sets on drop.
///
/// One instance per [`StatusState`], i.e. one per serve process.
pub(super) struct LedgerReadGate(Arc<Semaphore>);

impl Default for LedgerReadGate {
    fn default() -> Self {
        Self(Arc::new(Semaphore::new(1)))
    }
}

impl LedgerReadGate {
    /// Take the permit, or the unavailable code to answer with: `db_busy`
    /// while another read holds it. Nothing closes the semaphore; a closed
    /// one is matched by value rather than unwrapped (`panic = "abort"`).
    pub(super) fn try_acquire(&self) -> Result<OwnedSemaphorePermit, &'static str> {
        Arc::clone(&self.0)
            .try_acquire_owned()
            .map_err(|err| match err {
                TryAcquireError::NoPermits => codes::DB_BUSY,
                TryAcquireError::Closed => codes::DB_UNAVAILABLE,
            })
    }

    #[cfg(test)]
    pub(super) fn available_permits(&self) -> usize {
        self.0.available_permits()
    }
}

/// Sets its flag when dropped. Held by the request future, so a client that
/// drops the request stops the blocking read it started. It also fires after a
/// read that already finished, where there is nothing left to stop.
struct CancelOnDrop(CancelFlag);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// The read limit for a request whose body has just been parsed: its budget
/// starts now, before the gate and the blocking-worker queue.
fn request_read_limit() -> ReadLimit {
    ReadLimit::from(Instant::now() + Duration::from_millis(QUERY_BUDGET_MS))
}

/// Whether `limit` has already stopped the read: its flag is set or its
/// deadline has passed.
fn read_stopped(limit: &ReadLimit) -> bool {
    limit.cancel.is_cancelled() || Instant::now() >= limit.deadline
}

/// The closed request vocabulary. `deny_unknown_fields` plus the closed
/// enums make every out-of-vocabulary body -- an unknown key, an unknown token,
/// a wrong type -- fail closed at parse time, before the ledger is touched.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryRequest {
    /// The window to aggregate.
    window: QueryWindow,
    /// The dimension to roll the fine rows up to: any token of the shared
    /// grouping list, so a new grouping is accepted here without an edit.
    group_by: GroupDim,
    /// Restrict to one routing alias. Absent matches every alias.
    #[serde(default)]
    alias: Option<String>,
    /// Restrict to one served provider. Absent matches every provider.
    #[serde(default)]
    provider: Option<String>,
    /// Restrict to one fine model label: the served model, or the requested
    /// model on a request never dispatched. Absent matches every model.
    #[serde(default)]
    model: Option<String>,
    /// Also return a time series at this granularity. ABSENT means no series at
    /// all -- the ledger is read exactly as it would be without this field.
    #[serde(default)]
    bucket: Option<QueryBucket>,
}

/// The closed `window` vocabulary. Time-series granularity is NOT expressed
/// here -- it is carried by the separate `bucket` field ([`QueryBucket`]),
/// so a window token is only ever a range. `last24h` and `last7d` are exact
/// durations ending at the request's instant; the rest are local calendar
/// windows.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum QueryWindow {
    Today,
    Week,
    Month,
    Last24h,
    Last7d,
    All,
}

/// The closed `bucket` vocabulary: the granularity a series is REQUESTED at.
/// Coarser tokens are deliberately absent -- the server widens the grid itself
/// under the bucket cap and reports the resolved width, so a client never needs
/// to name one.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum QueryBucket {
    Hour,
    Day,
}

impl QueryWindow {
    const fn flag(self) -> WindowFlag {
        match self {
            Self::Today => WindowFlag::Today,
            Self::Week => WindowFlag::ThisWeek,
            Self::Month => WindowFlag::ThisMonth,
            Self::Last24h => WindowFlag::Last24h,
            Self::Last7d => WindowFlag::Last7d,
            Self::All => WindowFlag::All,
        }
    }
}

impl QueryBucket {
    const fn unit(self) -> BucketUnit {
        match self {
            Self::Hour => BucketUnit::Hour,
            Self::Day => BucketUnit::Day,
        }
    }
}

/// A body in the closed request vocabulary, with its filters validated but its
/// window not yet resolved: resolving needs the host zone, which is loaded on
/// the blocking worker that builds the panel.
#[derive(Debug)]
pub(super) struct ParsedQuery {
    flag: WindowFlag,
    group_by: GroupDim,
    alias_filter: Option<String>,
    provider_filter: Option<String>,
    model_filter: Option<String>,
    bucket: Option<BucketUnit>,
}

/// The query payload on the wire: the grouped result, with every group of a
/// `seat` grouping naming its account.
#[derive(Debug, Clone, Serialize)]
pub(super) struct QueryPayload {
    groups: Vec<GroupRow>,
    totals: QueryTotals,
    series: Option<QuerySeries>,
}

/// One group. A seat group also carries its account identity; a group of
/// any other grouping carries only its label and metrics.
#[derive(Debug, Clone, Serialize)]
struct GroupRow {
    label: String,
    #[serde(flatten)]
    seat: Option<SeatIdentity>,
    metrics: QueryMetrics,
}

/// A seat group's identity, resolved from the served config, never parsed
/// from the label.
#[derive(Debug, Clone, Serialize)]
struct SeatIdentity {
    /// The ledger's seat value, the key `/status/accounts` lists the account
    /// under; null for the `(unattributed)` group.
    account: Option<String>,
    /// The `[providers]` entry the account is reported under; null for an
    /// account the served config no longer names.
    provider_entry: Option<String>,
    /// The `<label>` of its `oauth://<provider>#<label>` login; null for an
    /// unlabeled login and an account the served config no longer names.
    login_label: Option<String>,
}

/// `result` as the wire payload, each group of a `seat` grouping named from
/// `identities`.
fn name_groups(
    result: QueryResult,
    group_by: GroupDim,
    identities: &AccountIdentities,
) -> QueryPayload {
    let groups = result
        .groups
        .into_iter()
        .map(|group| GroupRow {
            seat: (group_by == GroupDim::Seat).then(|| seat_identity(&group.label, identities)),
            label: group.label,
            metrics: group.metrics,
        })
        .collect();
    QueryPayload {
        groups,
        totals: result.totals,
        series: result.series,
    }
}

/// `panel` with its result, if any, as the wire payload named by
/// [`name_groups`].
fn name_panel_groups(
    panel: Panel<QueryResult>,
    group_by: GroupDim,
    identities: &AccountIdentities,
) -> Panel<QueryPayload> {
    Panel {
        schema_version: panel.schema_version,
        as_of: panel.as_of,
        data: panel
            .data
            .map(|result| name_groups(result, group_by, identities)),
        unavailable: panel.unavailable,
    }
}

fn seat_identity(label: &str, identities: &AccountIdentities) -> SeatIdentity {
    if label == UNATTRIBUTED {
        return SeatIdentity {
            account: None,
            provider_entry: None,
            login_label: None,
        };
    }
    let identity = identities.get(label);
    SeatIdentity {
        account: Some(label.to_owned()),
        provider_entry: identity.map(|id| id.provider_entry.clone()),
        login_label: identity.and_then(|id| id.login_label.clone()),
    }
}

/// Parse a request body against the closed vocabulary. Every failure is the
/// SAME opaque unit error: the caller emits one fixed code, so no parse detail
/// can reach the wire.
pub(super) fn parse_body(body: &[u8]) -> Result<ParsedQuery, ()> {
    let request: QueryRequest = serde_json::from_slice(body).map_err(|_| ())?;
    Ok(ParsedQuery {
        flag: request.window.flag(),
        group_by: request.group_by,
        alias_filter: filter_value(request.alias)?,
        provider_filter: filter_value(request.provider)?,
        model_filter: filter_value(request.model)?,
        bucket: request.bucket.map(QueryBucket::unit),
    })
}

impl ParsedQuery {
    /// The query spec plus the requested series granularity, with the window
    /// resolved in `zone` against `now`.
    ///
    /// The bucket GRID is not resolved here: an all-time series anchors on the
    /// ledger's earliest row, which is a read, so the grid is resolved on the
    /// blocking worker that already owns the connection.
    pub(super) fn resolve(
        self,
        zone: &LocalZone,
        now: OffsetDateTime,
    ) -> (QuerySpec, Option<BucketUnit>) {
        let bounds = window_bounds(zone, self.flag, now);
        let spec = QuerySpec {
            from_ms: bounds.from_ms,
            to_ms: bounds.to_ms,
            group_by: self.group_by,
            alias_filter: self.alias_filter,
            provider_filter: self.provider_filter,
            model_filter: self.model_filter,
            bucket: None,
            entry_accounts: BTreeMap::new(),
        };
        (spec, self.bucket)
    }
}

/// [`parse_body`] then [`ParsedQuery::resolve`], for tests that hold a zone.
///
/// Visible to the rest of the status module so the dashboard's drift test can
/// validate the request shapes the page declares against THIS parser rather
/// than a second copy of the vocabulary.
#[cfg(test)]
pub(super) fn spec_from_body(
    body: &[u8],
    zone: &LocalZone,
    now: OffsetDateTime,
) -> Result<(QuerySpec, Option<BucketUnit>), ()> {
    Ok(parse_body(body)?.resolve(zone, now))
}

/// Validate one optional filter. A present-but-blank filter matches nothing and
/// would render an all-zero result that reads like real data, so it is refused
/// as out-of-vocabulary rather than silently answered.
fn filter_value(raw: Option<String>) -> Result<Option<String>, ()> {
    match raw {
        Some(value) if value.trim().is_empty() => Err(()),
        other => Ok(other),
    }
}

/// The route's single handler. `method` is the FIRST extractor so a wrong
/// method is refused before the body is buffered at all.
pub(super) async fn handler(
    method: Method,
    State(state): State<Arc<StatusState>>,
    request: Request,
) -> Response {
    if method.as_str() != QUERY_METHOD {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    // Buffered as raw bytes, not through a `Json` extractor: the extractor's
    // own rejection would emit a shape this route does not own (and can carry
    // parse detail), so the body is parsed here under the fixed envelope. Both
    // an oversize body and a body that never finishes arriving are the same
    // out-of-vocabulary answer.
    let read = tokio::time::timeout(
        BODY_READ_TIMEOUT,
        to_bytes(request.into_body(), MAX_BODY_BYTES),
    )
    .await;
    let Ok(Ok(body)) = read else {
        return invalid_query();
    };
    let Ok(parsed) = parse_body(&body) else {
        return invalid_query();
    };
    let limit = request_read_limit();
    render(build(&state, parsed, OffsetDateTime::now_utc(), limit).await)
}

/// Serialize a finished panel. Rendering is EXPLICIT rather than through the
/// `Json` responder because the responder's serialization-failure arm is a bare
/// 500 -- a fourth response regime this route does not declare. Any payload
/// that will not render degrades to the same unavailable panel a data-source
/// failure produces; the fallback carries no `data`, so it cannot fail in turn.
fn render<T: Serialize>(panel: Panel<T>) -> Response {
    match serde_json::to_vec(&panel) {
        Ok(bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response(),
        Err(_) => Json(Panel::<T>::unavailable(
            SCHEMA_VERSION,
            codes::DB_UNAVAILABLE,
        ))
        .into_response(),
    }
}

/// The fixed 400 envelope. Same shape as the status gate's transport-level
/// 403/503 (a `schema_version` plus a code/message pair) and carrying nothing
/// request-specific -- no serde text, no body bytes, no filter values.
fn invalid_query() -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({
            "schema_version": SCHEMA_VERSION,
            "error": {
                "code": INVALID_QUERY,
                "message": "query body is not in the supported query vocabulary",
            },
        })),
    )
        .into_response()
}

/// Run the aggregate under the ledger-read gate and panel isolation, and record
/// its availability edge.
///
/// `limit` is fixed by the caller before this is entered, so the gate and the
/// blocking-worker queue spend the same budget the read does. Dropping this
/// future sets `limit`'s flag.
///
/// The two request modes have their own edge detectors: a series read has a
/// distinct failure profile (a wider GROUP BY over a larger temp b-tree), so
/// sharing one detector would let a healthy aggregate poll mask a consistently
/// failing series poll. Exactly ONE of them sees each request, busy and
/// timed-out answers included.
async fn build(
    state: &StatusState,
    parsed: ParsedQuery,
    now: OffsetDateTime,
    limit: ReadLimit,
) -> Panel<QueryPayload> {
    let _cancel_on_drop = CancelOnDrop(limit.cancel.clone());
    let is_series = parsed.bucket.is_some();
    let panel = match state.ledger_read.try_acquire() {
        Ok(permit) => read_under_permit(state, permit, parsed, now, limit).await,
        Err(code) => Panel::unavailable(SCHEMA_VERSION, code),
    };
    if is_series {
        state.observability.query_series.record(&panel);
    } else {
        state.observability.query.record(&panel);
    }
    panel
}

/// Hand the read to a blocking worker that owns `permit` until it ends.
async fn read_under_permit(
    state: &StatusState,
    permit: OwnedSemaphorePermit,
    parsed: ParsedQuery,
    now: OffsetDateTime,
    limit: ReadLimit,
) -> Panel<QueryPayload> {
    let db_path = state.usage_db_path.clone();
    // ONE pinned config snapshot for the whole request; its price tables are
    // pinned once on the blocking worker.
    let pricer = state.pricer();
    let identities = pricer.account_identities();
    // Stamped before the blocking read so a lock wait cannot skew the
    // freshness marker.
    let as_of = now_utc_rfc3339();
    #[cfg(test)]
    let stall = state.test_hooks.stall_query_read;
    run_blocking_panel(permit, SCHEMA_VERSION, codes::DB_UNAVAILABLE, move || {
        #[cfg(test)]
        if stall {
            stall_until_stopped(&limit);
        }
        let zone = LocalZone::load();
        let (spec, bucket) = parsed.resolve(&zone, now);
        let group_by = spec.group_by;
        let panel = build_panel(
            &db_path,
            spec,
            bucket,
            &pricer.pin(),
            as_of,
            &zone,
            now,
            &limit,
        );
        name_panel_groups(panel, group_by, &identities)
    })
    .await
}

/// Stand in for a read that runs until its limit stops it, the way the
/// progress handler stops a long statement. See
/// `StatusTestHooks::stall_query_read`.
#[cfg(test)]
fn stall_until_stopped(limit: &ReadLimit) {
    while !read_stopped(limit) {
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Open the ledger read-only, run the grouped aggregate under `limit`, and map
/// the outcome to a panel. Runs on a blocking worker (SQLite is synchronous);
/// the connection is opened and dropped within this call.
///
/// The seat grouping's entry accounts are taken from `pricer`, so a row's
/// label and its price read one config snapshot.
///
/// A limit that has already stopped the read (its request was dropped while
/// this waited for a worker, or the budget ran out in the queue) answers
/// `query_timeout` before the ledger is opened.
///
/// A requested series resolves its grid here, since an all-time window anchors
/// on the ledger's earliest row -- an O(log n) index probe, run before the
/// aggregate and only for that window. The probe carries the window's own lower
/// bound and the resolved anchor never falls below it, so replacing
/// `spec.from_ms` with that anchor selects the SAME rows and leaves the groups
/// and totals identical to the non-series path.
#[expect(
    clippy::too_many_arguments,
    reason = "the request's parts are resolved on the async side and the \
              blocking worker; bundling them would only rename the list"
)]
pub(super) fn build_panel(
    db_path: &Path,
    mut spec: QuerySpec,
    bucket: Option<BucketUnit>,
    pricer: &QueryPricer,
    as_of: String,
    zone: &LocalZone,
    now: OffsetDateTime,
    limit: &ReadLimit,
) -> Panel<QueryResult> {
    if read_stopped(limit) {
        return Panel::unavailable(SCHEMA_VERSION, codes::QUERY_TIMEOUT);
    }
    spec.entry_accounts = pricer.entry_accounts();
    let mut db = match open_readonly_fastfail(db_path) {
        Ok(db) => db,
        Err(err) => return Panel::unavailable(SCHEMA_VERSION, open_error_code(&err)),
    };
    let empty_series = match bucket {
        None => None,
        Some(unit) => {
            let first_row_ms = if spec.from_ms == 0 {
                match earliest_ts_start(&db, spec.from_ms) {
                    Ok(earliest) => earliest,
                    Err(err) => {
                        return Panel::unavailable(SCHEMA_VERSION, query_error_code(&err));
                    }
                }
            } else {
                None
            };
            match resolve_bucket(zone, unit, spec.from_ms, spec.to_ms, first_row_ms, now) {
                Some((anchor_ms, resolved)) => {
                    spec.from_ms = anchor_ms;
                    spec.bucket = Some(resolved);
                    None
                }
                // Nothing to bucket -- an all-time window over an empty ledger
                // has no earliest row to anchor a grid on. The series is still
                // REPORTED, as the empty one it is, so a series request always
                // answers in the series shape rather than looking like a
                // non-series request that dropped the field.
                None => Some(QuerySeries {
                    bucket_ms: unit.base_width_ms(),
                    buckets: Vec::new(),
                }),
            }
        }
    };
    match query(&mut db, &spec, |row| pricer.price(row), limit.clone()) {
        Ok(mut result) => {
            if let Some(series) = empty_series {
                result.series = Some(series);
            }
            Panel::available(SCHEMA_VERSION, as_of, result)
        }
        Err(err) => Panel::unavailable(SCHEMA_VERSION, query_error_code(&err)),
    }
}

#[cfg(test)]
pub(super) mod fixture;
#[cfg(test)]
#[path = "query_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "query_cli_parity_tests.rs"]
mod cli_parity_tests;
