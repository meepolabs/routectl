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
//!     aggregate runs under the same isolation as the other panels
//!     (`guard_panel`'s blocking worker + `catch_unwind`, a per-request
//!     read-only fast-fail open), so a missing, busy, mismatched, or
//!     over-budget ledger degrades to an unavailable `Panel`, never a 500 --
//!     and the payload is serialized explicitly so a render failure degrades
//!     the same way instead of escaping as a fourth regime.
//!
//! Cost is resolved through the read-only router facade's pricer, which pins
//! ONE config snapshot for the whole request: a hot-swap mid-query can never
//! make two rows of one result price against different rate tables.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Local};
use serde::Deserialize;
use serde_json::json;

use routectl_usage::{
    GroupDim, QueryResult, QuerySeries, QuerySpec, earliest_ts_start, open_readonly_fastfail, query,
};

use crate::commands::usage::{BucketUnit, WindowFlag, resolve_bucket, window_bounds};
use crate::server::status_gate::QUERY_BUDGET_MS;

use super::router_view::QueryPricer;
use super::usage::{open_error_code, query_error_code};
use super::vocabulary::codes;
use super::{Panel, StatusState, guard_panel, now_utc_rfc3339};

/// Wire-shape version of the query payload. Additive changes (a new metric, a
/// new `group_by` or `window` token) do NOT bump it; a semantic change to an
/// existing field, or a removal, does.
pub const SCHEMA_VERSION: u32 = 1;

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
const BODY_READ_TIMEOUT: Duration = Duration::from_secs(1);

/// The closed request vocabulary. `deny_unknown_fields` plus the two closed
/// enums make every out-of-vocabulary body -- an unknown key, an unknown token,
/// a wrong type -- fail closed at parse time, before the ledger is touched.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryRequest {
    /// The calendar window to aggregate.
    window: QueryWindow,
    /// The dimension to roll the fine rows up to.
    group_by: QueryGroupBy,
    /// Restrict to one routing alias. Absent matches every alias.
    #[serde(default)]
    alias: Option<String>,
    /// Restrict to one served provider. Absent matches every provider.
    #[serde(default)]
    provider: Option<String>,
    /// Also return a time series at this granularity. ABSENT means no series at
    /// all -- the ledger is read exactly as it would be without this field.
    #[serde(default)]
    bucket: Option<QueryBucket>,
}

/// The closed `window` vocabulary. Bucketed / time-series shapes are reserved
/// for a later increment and are deliberately absent here rather than accepted
/// and ignored.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum QueryWindow {
    Today,
    Week,
    Month,
    All,
}

/// The closed `group_by` vocabulary. `seat`, `upstream`, `outcome`, and `class`
/// are reserved for later increments.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum QueryGroupBy {
    Model,
    Provider,
    Alias,
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
            Self::All => WindowFlag::All,
        }
    }
}

impl QueryGroupBy {
    const fn dim(self) -> GroupDim {
        match self {
            Self::Model => GroupDim::Model,
            Self::Provider => GroupDim::Provider,
            Self::Alias => GroupDim::Alias,
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

/// Parse a request body into the query spec plus the requested series
/// granularity, resolving the window against `now`. Every failure is the SAME
/// opaque unit error: the caller emits one fixed code, so no parse detail can
/// reach the wire.
///
/// The bucket GRID is not resolved here: an all-time series anchors on the
/// ledger's earliest row, which is a read, so the grid is resolved on the
/// blocking worker that already owns the connection.
///
/// Visible to the rest of the status module so the dashboard's drift test can
/// validate the request shapes the page declares against THIS parser rather
/// than a second copy of the vocabulary.
pub(super) fn spec_from_body(
    body: &[u8],
    now: DateTime<Local>,
) -> Result<(QuerySpec, Option<BucketUnit>), ()> {
    let request: QueryRequest = serde_json::from_slice(body).map_err(|_| ())?;
    let bounds = window_bounds(request.window.flag(), now);
    let spec = QuerySpec {
        from_ms: bounds.from_ms,
        to_ms: bounds.to_ms,
        group_by: request.group_by.dim(),
        alias_filter: filter_value(request.alias)?,
        provider_filter: filter_value(request.provider)?,
        bucket: None,
    };
    Ok((spec, request.bucket.map(QueryBucket::unit)))
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
    let now = Local::now();
    let Ok((spec, bucket)) = spec_from_body(&body, now) else {
        return invalid_query();
    };
    render(build(&state, spec, bucket, now).await)
}

/// Serialize a finished panel. Rendering is EXPLICIT rather than through the
/// `Json` responder because the responder's serialization-failure arm is a bare
/// 500 -- a fourth response regime this route does not declare. Any payload
/// that will not render degrades to the same unavailable panel a data-source
/// failure produces; the fallback carries no `data`, so it cannot fail in turn.
fn render(panel: Panel<QueryResult>) -> Response {
    match serde_json::to_vec(&panel) {
        Ok(bytes) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            bytes,
        )
            .into_response(),
        Err(_) => Json(Panel::<QueryResult>::unavailable(
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

/// Run the aggregate under panel isolation and record its availability edge.
///
/// The two request modes have their own edge detectors: a series read has a
/// distinct failure profile (a wider GROUP BY over a larger temp b-tree), so
/// sharing one detector would let a healthy aggregate poll mask a consistently
/// failing series poll. Exactly ONE of them sees each request.
async fn build(
    state: &StatusState,
    spec: QuerySpec,
    bucket: Option<BucketUnit>,
    now: DateTime<Local>,
) -> Panel<QueryResult> {
    let db_path = state.usage_db_path.clone();
    // ONE pinned config snapshot for the whole request's pricing.
    let pricer = state.router.pricer();
    // Stamped before the blocking read so a lock wait cannot skew the
    // freshness marker.
    let as_of = now_utc_rfc3339();
    let panel = guard_panel(
        &state.builder_capacity,
        SCHEMA_VERSION,
        codes::DB_UNAVAILABLE,
        move || build_panel(&db_path, spec, bucket, &pricer, as_of, now),
    )
    .await;
    if bucket.is_some() {
        state.observability.query_series.record(&panel);
    } else {
        state.observability.query.record(&panel);
    }
    panel
}

/// Open the ledger read-only, run the deadline-bounded grouped aggregate, and
/// map the outcome to a panel. Runs on a blocking worker (SQLite is
/// synchronous); the connection is opened and dropped within this call.
///
/// The deadline is anchored here rather than at request arrival so the budget
/// measures the QUERY, not the time the request spent waiting for a
/// concurrency permit or a blocking worker.
///
/// A requested series resolves its grid here, since an all-time window anchors
/// on the ledger's earliest row -- an O(log n) index probe, run before the
/// aggregate and only for that window. The probe carries the window's own lower
/// bound and the resolved anchor never falls below it, so replacing
/// `spec.from_ms` with that anchor selects the SAME rows and leaves the groups
/// and totals identical to the non-series path.
fn build_panel(
    db_path: &Path,
    mut spec: QuerySpec,
    bucket: Option<BucketUnit>,
    pricer: &QueryPricer,
    as_of: String,
    now: DateTime<Local>,
) -> Panel<QueryResult> {
    let deadline = Instant::now() + Duration::from_millis(QUERY_BUDGET_MS);
    let db = match open_readonly_fastfail(db_path) {
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
            match resolve_bucket(unit, spec.from_ms, spec.to_ms, first_row_ms, now) {
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
    match query(&db, &spec, |row| pricer.price(row), deadline) {
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
#[path = "query_tests.rs"]
mod tests;
