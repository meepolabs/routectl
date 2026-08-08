//! Usage read-query facade and shared row/error types.

mod aggregate;
mod capability;
mod deadline;
mod grouped;
mod would_trim;

pub use aggregate::{
    QuotaSnapshot, aggregate, earliest_ts_start, errors_by_class, latest_quota_by_seat, ttfbs,
};
pub use capability::{
    CapabilityEventRow, TombstoneRow, latest_tombstone, read_capability_events_after,
};
pub use deadline::DeadlineGuard;
pub use grouped::{
    BucketSpec, CostStatus, GroupDim, QueryGroup, QueryMetrics, QueryResult, QuerySeries,
    QuerySpec, QueryTotals, RowCost, SeriesBucket, query,
};
pub use would_trim::{
    KCalibration, NearLosslessAttributionSummary, ReuseSampleRow, ShadowMisfireSummary,
    WouldTrimSummary, k_calibration_summary, near_lossless_attribution_summary,
    read_reuse_samples_since, shadow_misfire_summary, would_trim_summary,
};

/// Errors raised while querying the usage DB.
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    /// A SQLite operation failed while reading.
    #[error("usage query failed: {0}")]
    Sqlite(#[source] rusqlite::Error),

    /// The query exceeded its deadline and was interrupted mid-statement. A
    /// distinct variant so the caller can shed it under its own code rather
    /// than reporting the DB as unavailable.
    #[error("usage query exceeded its deadline")]
    Interrupted,

    /// The requested time-bucket grid violated its invariants: a non-positive
    /// width, or a bucket count outside `1..=1000`. The caller resolves the grid
    /// and owns those bounds; this is the read path re-checking them rather than
    /// dividing by zero in SQL or densifying an unbounded vector.
    #[error("usage query received an unusable bucket grid")]
    InvalidBucket,
}

/// Separate a fired-deadline interrupt from every other SQLite failure, so a
/// caller can shed it under its own code rather than reporting the ledger as
/// unusable.
///
/// Deliberately hand-written rather than `#[from]`: the conversion is what
/// every `?` in this module tree goes through, so classifying HERE is what
/// makes [`QueryError::Interrupted`] reachable from every query function
/// without any of them taking a deadline parameter. A deadline installed by
/// [`DeadlineGuard`] can interrupt any statement on the connection, including
/// ones run by functions that know nothing about it.
impl From<rusqlite::Error> for QueryError {
    fn from(err: rusqlite::Error) -> Self {
        if err.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted) {
            return Self::Interrupted;
        }
        Self::Sqlite(err)
    }
}

/// The group-key columns shared by the aggregate and the raw-latency rows.
/// `alias` is `NOT NULL` in the schema so it is always present; the rest are
/// nullable. Plain data; the caller decides how to display or roll these up.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct GroupKey {
    /// Served model nickname. `None` in the raw column when no target was
    /// dispatched (aggregates coalesce it to `requested_model`).
    pub model: Option<String>,
    /// Served provider name. `None` when no target was dispatched.
    pub provider: Option<String>,
    /// Served upstream target id. `None` when no target was dispatched.
    pub upstream: Option<String>,
    /// Resolved routing alias. `NOT NULL` in the schema, so always present.
    pub alias: String,
}

/// One aggregate row at the finest cost-relevant granularity:
/// `(model, provider, upstream, alias)`, where `model` coalesces to
/// `requested_model` so pre-dispatch aborts (NULL `model`) attribute to the
/// route the caller asked for rather than a NULL bucket. Token FLOW dims
/// (`input_tokens`, `output_tokens`, `cache_write_*`) are summed with
/// COALESCE so NULL counters contribute 0. `cache_read` is NOT summed: it is
/// a per-turn SNAPSHOT of cached-context size, so the group reports its peak
/// and mean instead (see `cache_read_peak` / `cache_read_avg`).
/// `server_tool_calls` is the sum of the integer values inside each row's
/// `server_tool_use` JSON map (via JSON1 `json_each`), i.e. the total number
/// of server-tool invocations -- not a count of rows that used a server tool.
///
/// The `*_present` fields are `COUNT(col)` (SQLite COUNT ignores NULLs), so
/// the caller can distinguish "metric reported as 0" from "metric not
/// reported". `gen_window_ms` / `gen_output_tokens` are summed only over
/// streaming, successful rows with a usable time-to-first-byte
/// (`stream=1 AND outcome='ok' AND ttfb_ms IS NOT NULL AND latency_ms >
/// ttfb_ms`); they feed a robust generation-throughput estimate.
///
/// `errors` counts `outcome NOT IN ('ok', 'client_disconnect')` -- a client
/// hangup before the first content chunk is not a routing failure, so it is
/// excluded and reported separately via `client_disconnect_total` /
/// `client_disconnect_pre_dispatch` instead. `gate_blocked` and
/// `upstream_error` remain inside `errors`.
#[derive(Debug, Clone)]
pub struct AggRow {
    /// The group's `(model, provider, upstream, alias)` key.
    pub key: GroupKey,
    /// Total requests in the group.
    pub requests: i64,
    /// Requests whose outcome was `ok`.
    pub ok: i64,
    /// Requests whose outcome was an error, excluding `client_disconnect`
    /// (see the struct note).
    pub errors: i64,
    /// Summed input tokens over the group (NULL counters count as 0).
    pub input_tokens: i64,
    /// Summed output tokens over the group (NULL counters count as 0).
    pub output_tokens: i64,
    /// Summed reasoning tokens over the group (NULL counters count as 0).
    pub reasoning_tokens: i64,
    /// Peak cached-context SIZE seen in the group (`MAX(cache_read)`). NOT a
    /// flow: each row's `cache_read` is a per-turn SNAPSHOT of the cached
    /// prefix re-read that turn, so summing it across a long session would
    /// repeat-count the same growing prefix. The peak is the true high-water
    /// mark of context served from cache.
    pub cache_read_peak: i64,
    /// Mean cached-context size across the group's rows (`AVG(cache_read)`,
    /// truncated to an integer). Same snapshot semantics as `cache_read_peak`.
    pub cache_read_avg: i64,
    /// Summed cache-read volume across the group (`SUM(cache_read)`), kept
    /// SOLELY as the cost basis: cache reads are billed PER TURN, so the
    /// cumulative dollar cost is the sum, not the peak. This is deliberately
    /// distinct from `cache_read_peak` / `cache_read_avg`, which are
    /// display-only context-SIZE figures -- do NOT "clean up" this SUM
    /// thinking it is the repeat-counting bug that was removed; pricing the
    /// peak instead understates cost by roughly the turn count.
    pub cache_read_billed: i64,
    /// Summed 5-minute cache-write tokens over the group.
    pub cache_write_5m: i64,
    /// Summed 1-hour cache-write tokens over the group.
    pub cache_write_1h: i64,
    /// Total server-tool invocations across the group (sum of the integer
    /// values inside each row's `server_tool_use` JSON map).
    pub server_tool_calls: i64,
    /// Summed time-to-first-byte, milliseconds, over rows with a TTFB.
    pub sum_ttfb_ms: i64,
    /// Count of rows with a non-NULL `ttfb_ms` (the `sum_ttfb_ms` divisor).
    pub ttfb_count: i64,
    /// Summed generation window (`latency_ms - ttfb_ms`) over streaming,
    /// successful rows with a usable TTFB. Feeds the throughput estimate.
    pub gen_window_ms: i64,
    /// Summed output tokens over the same rows as `gen_window_ms`. Feeds
    /// the throughput estimate.
    pub gen_output_tokens: i64,
    /// Count of rows reporting a reasoning-token value (COUNT ignores NULL),
    /// distinguishing "reported 0" from "not reported".
    pub reasoning_present: i64,
    /// Count of rows reporting a cache-read value (COUNT ignores NULL).
    pub cache_read_present: i64,
    /// Count of rows reporting a 5-minute cache-write value.
    pub cache_write_5m_present: i64,
    /// Count of rows reporting a 1-hour cache-write value.
    pub cache_write_1h_present: i64,
    /// Count of rows reporting a `server_tool_use` value.
    pub server_tool_present: i64,
    /// Count of streaming rows in the group.
    pub stream_count: i64,
    /// Rows in the group whose terminal outcome is `client_disconnect`
    /// (client hung up before `finalize`, per the `Drop` fallback in
    /// `usage_capture.rs`). Excluded from `errors` -- see that field's note.
    pub client_disconnect_total: i64,
    /// Subset of `client_disconnect_total` with a NULL raw `model` column,
    /// i.e. the client disconnected before dispatch stamped a served
    /// provider/model (pre-first-content-chunk). Computed on the raw
    /// column, not the `COALESCE(model, requested_model)` group key, so it
    /// reflects whether a provider was ever resolved for that row.
    pub client_disconnect_pre_dispatch: i64,
}

#[cfg(test)]
use crate::db::UsageDb;
#[cfg(test)]
use aggregate::ERRORS_BY_CLASS_SQL;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
