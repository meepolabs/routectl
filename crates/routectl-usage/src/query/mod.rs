//! Usage read-query facade and shared row/error types.

mod aggregate;
mod would_trim;

pub use aggregate::{QuotaSnapshot, aggregate, errors_by_class, latest_quota, ttfbs};
pub use would_trim::{
    KCalibration, M1AttributionSummary, ReuseSampleRow, ShadowMisfireSummary, WouldTrimSummary,
    k_calibration_summary, m1_attribution_summary, read_reuse_samples_since,
    shadow_misfire_summary, would_trim_summary,
};

/// Errors raised while querying the usage DB.
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    /// A SQLite operation failed while reading.
    #[error("usage query failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// The group-key columns shared by the aggregate and the raw-latency rows.
/// `alias` is `NOT NULL` in the schema so it is always present; the rest are
/// nullable. Plain data; the caller decides how to display or roll these up.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct GroupKey {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub upstream: Option<String>,
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
    pub key: GroupKey,
    pub requests: i64,
    pub ok: i64,
    pub errors: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
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
    pub cache_write_5m: i64,
    pub cache_write_1h: i64,
    pub server_tool_calls: i64,
    pub sum_ttfb_ms: i64,
    pub ttfb_count: i64,
    pub gen_window_ms: i64,
    pub gen_output_tokens: i64,
    pub reasoning_present: i64,
    pub cache_read_present: i64,
    pub cache_write_5m_present: i64,
    pub cache_write_1h_present: i64,
    pub server_tool_present: i64,
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
