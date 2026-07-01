//! Read-only windowed aggregation over the usage `requests` table.
//!
//! Every query takes an inclusive-exclusive epoch-ms window
//! `[from_ms, to_ms)` and applies it as `ts_start >= ? AND ts_start < ?`
//! with BOUND parameters only. This layer is pure data access: it sums and
//! groups at the finest cost-relevant granularity and hands plain structs
//! back. All rollup, percentile math, cost computation, and formatting
//! belong to the caller (the `routectl usage` CLI), not here.

use rusqlite::Row;

use crate::db::UsageDb;

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
#[derive(Debug, Clone, PartialEq, Eq, Default)]
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
}

/// The most recent quota-bearing snapshot in the DB. Mirrors the `quota_*`
/// columns the daemon stamps on a row when an upstream reports quota data.
#[derive(Debug, Clone)]
pub struct QuotaSnapshot {
    pub ts_start: i64,
    pub claim: Option<String>,
    pub status: Option<String>,
    pub overage_status: Option<String>,
    pub utilization: Option<f64>,
    pub overage_utilization: Option<f64>,
    pub reset: Option<i64>,
}

const AGG_SQL: &str = "\
SELECT
    COALESCE(model, requested_model) AS model, provider, upstream, alias,
    COUNT(*)                                            AS requests,
    SUM(CASE WHEN outcome = 'ok' THEN 1 ELSE 0 END)     AS ok,
    SUM(CASE WHEN outcome != 'ok' THEN 1 ELSE 0 END)    AS errors,
    COALESCE(SUM(input_tokens), 0)                      AS input_tokens,
    COALESCE(SUM(output_tokens), 0)                     AS output_tokens,
    COALESCE(SUM(reasoning_tokens), 0)                  AS reasoning_tokens,
    COALESCE(MAX(cache_read), 0)                        AS cache_read_peak,
    CAST(COALESCE(AVG(cache_read), 0) AS INTEGER)       AS cache_read_avg,
    COALESCE(SUM(cache_read), 0)                        AS cache_read_billed,
    COALESCE(SUM(cache_write_5m), 0)                    AS cache_write_5m,
    COALESCE(SUM(cache_write_1h), 0)                    AS cache_write_1h,
    COALESCE(SUM(
        CASE WHEN r.server_tool_use IS NOT NULL AND json_valid(r.server_tool_use)
        THEN (
            SELECT COALESCE(SUM(je.value), 0)
            FROM json_each(r.server_tool_use) AS je
            WHERE typeof(je.value) = 'integer'
        )
        ELSE 0 END
    ), 0)                                               AS server_tool_calls,
    COALESCE(SUM(ttfb_ms), 0)                           AS sum_ttfb_ms,
    COUNT(ttfb_ms)                                      AS ttfb_count,
    COALESCE(SUM(CASE WHEN stream = 1 AND outcome = 'ok' AND ttfb_ms IS NOT NULL
        AND latency_ms > ttfb_ms THEN latency_ms - ttfb_ms ELSE 0 END), 0) AS gen_window_ms,
    COALESCE(SUM(CASE WHEN stream = 1 AND outcome = 'ok' AND ttfb_ms IS NOT NULL
        AND latency_ms > ttfb_ms THEN output_tokens ELSE 0 END), 0)        AS gen_output_tokens,
    COUNT(reasoning_tokens)                             AS reasoning_present,
    COUNT(cache_read)                                   AS cache_read_present,
    COUNT(cache_write_5m)                               AS cache_write_5m_present,
    COUNT(cache_write_1h)                               AS cache_write_1h_present,
    COUNT(server_tool_use)                              AS server_tool_present,
    COALESCE(SUM(stream), 0)                            AS stream_count
FROM requests AS r
WHERE ts_start >= ?1 AND ts_start < ?2
GROUP BY COALESCE(model, requested_model), provider, upstream, alias";

/// Windowed aggregate grouped by `(model, provider, upstream, alias)`.
/// Rows outside `[from_ms, to_ms)` are excluded. The caller rolls these up
/// for display and prices them per upstream.
pub fn aggregate(db: &UsageDb, from_ms: i64, to_ms: i64) -> Result<Vec<AggRow>, QueryError> {
    let mut stmt = db.conn().prepare(AGG_SQL)?;
    let rows = stmt
        .query_map([from_ms, to_ms], map_agg_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Build an `AggRow` from a result row. The column order matches `AGG_SQL`.
fn map_agg_row(row: &Row) -> rusqlite::Result<AggRow> {
    Ok(AggRow {
        key: GroupKey {
            model: row.get(0)?,
            provider: row.get(1)?,
            upstream: row.get(2)?,
            alias: row.get(3)?,
        },
        requests: row.get(4)?,
        ok: row.get(5)?,
        errors: row.get(6)?,
        input_tokens: row.get(7)?,
        output_tokens: row.get(8)?,
        reasoning_tokens: row.get(9)?,
        cache_read_peak: row.get(10)?,
        cache_read_avg: row.get(11)?,
        cache_read_billed: row.get(12)?,
        cache_write_5m: row.get(13)?,
        cache_write_1h: row.get(14)?,
        server_tool_calls: row.get(15)?,
        sum_ttfb_ms: row.get(16)?,
        ttfb_count: row.get(17)?,
        gen_window_ms: row.get(18)?,
        gen_output_tokens: row.get(19)?,
        reasoning_present: row.get(20)?,
        cache_read_present: row.get(21)?,
        cache_write_5m_present: row.get(22)?,
        cache_write_1h_present: row.get(23)?,
        server_tool_present: row.get(24)?,
        stream_count: row.get(25)?,
    })
}

const LATEST_QUOTA_SQL: &str = "\
SELECT ts_start, quota_claim, quota_status, quota_overage_status,
       quota_utilization, quota_overage_utilization, quota_reset
FROM requests
WHERE quota_status IS NOT NULL
ORDER BY ts_start DESC, rowid DESC
LIMIT 1";

/// The most recent row carrying quota data (`quota_status IS NOT NULL`),
/// or `None` if no row has quota data. Not windowed: the CLI's quota line
/// reflects the latest known snapshot regardless of the report window.
pub fn latest_quota(db: &UsageDb) -> Result<Option<QuotaSnapshot>, QueryError> {
    let mut stmt = db.conn().prepare(LATEST_QUOTA_SQL)?;
    let snapshot = stmt
        .query_row([], |row| {
            Ok(QuotaSnapshot {
                ts_start: row.get(0)?,
                claim: row.get(1)?,
                status: row.get(2)?,
                overage_status: row.get(3)?,
                utilization: row.get(4)?,
                overage_utilization: row.get(5)?,
                reset: row.get(6)?,
            })
        })
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(snapshot)
}

/// Windowed steady-state would-trim opportunity: how many requests in the
/// window carried a non-mutating would-cut candidate, and the summed
/// `would_trim_tokens` (the candidate freed-token count `d`) over them. The
/// verdict counts (`met`/`unmet`/`cold`/`unpriced`) are derived at query time
/// from the numeric advisory columns -- never persisted as a token. Plain
/// data; the caller decides how to display it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WouldTrimSummary {
    /// Count of requests in the window with a would-cut candidate
    /// (`would_trim_tokens IS NOT NULL`).
    pub candidate_requests: i64,
    /// Summed `would_trim_tokens` over those requests.
    pub would_trim_tokens: i64,
    /// Priced + Calibrated + floor >= K*: the estimator predicted reuse
    /// was sufficient; a real cut would have been authorized.
    pub verdict_met: i64,
    /// Priced + Calibrated + floor < K*: estimator ran but predicted
    /// insufficient reuse to justify the cut.
    pub verdict_unmet: i64,
    /// Priced but not yet Calibrated (no floor stamped): estimator has
    /// not seen enough samples to make a confidence call.
    pub verdict_cold: i64,
    /// No verified pricing row: K* could not be computed.
    pub verdict_unpriced: i64,
}

/// The verdict classification logic mirrors router.rs `would_trim_k_floor_for_meta`:
///   unpriced : would_trim_break_even_k IS NULL
///   cold     : break_even NOT NULL AND k_floor IS NULL
///   met      : k_floor NOT NULL AND k_floor >= would_trim_break_even_k
///   unmet    : k_floor NOT NULL AND k_floor < would_trim_break_even_k
/// The WHERE gate restricts the verdict counts to candidate rows only.
const WOULD_TRIM_SQL: &str = "\
SELECT
    COUNT(would_trim_tokens)                                            AS candidate_requests,
    COALESCE(SUM(would_trim_tokens), 0)                                AS would_trim_tokens,
    SUM(CASE WHEN would_trim_tokens IS NOT NULL
              AND would_trim_k_floor IS NOT NULL
              AND would_trim_break_even_k IS NOT NULL
              AND would_trim_k_floor >= would_trim_break_even_k
         THEN 1 ELSE 0 END)                                            AS verdict_met,
    SUM(CASE WHEN would_trim_tokens IS NOT NULL
              AND would_trim_k_floor IS NOT NULL
              AND would_trim_break_even_k IS NOT NULL
              AND would_trim_k_floor < would_trim_break_even_k
         THEN 1 ELSE 0 END)                                            AS verdict_unmet,
    SUM(CASE WHEN would_trim_tokens IS NOT NULL
              AND would_trim_break_even_k IS NOT NULL
              AND would_trim_k_floor IS NULL
         THEN 1 ELSE 0 END)                                            AS verdict_cold,
    SUM(CASE WHEN would_trim_tokens IS NOT NULL
              AND would_trim_break_even_k IS NULL
         THEN 1 ELSE 0 END)                                            AS verdict_unpriced
FROM requests
WHERE ts_start >= ?1 AND ts_start < ?2";

/// The window's steady-state would-trim opportunity. `COUNT(col)` ignores
/// NULLs, so `candidate_requests` is the number of requests the trimmer
/// flagged; `would_trim_tokens` is the summed candidate freed-token count.
/// The verdict counts partition the candidate rows by the derived
/// met/unmet/cold/unpriced classification. All fields are 0 when no row in
/// the window carried a candidate.
pub fn would_trim_summary(
    db: &UsageDb,
    from_ms: i64,
    to_ms: i64,
) -> Result<WouldTrimSummary, QueryError> {
    let mut stmt = db.conn().prepare(WOULD_TRIM_SQL)?;
    let summary = stmt.query_row([from_ms, to_ms], |row| {
        Ok(WouldTrimSummary {
            candidate_requests: row.get(0)?,
            would_trim_tokens: row.get(1)?,
            verdict_met: row.get(2)?,
            verdict_unmet: row.get(3)?,
            verdict_cold: row.get(4)?,
            verdict_unpriced: row.get(5)?,
        })
    })?;
    Ok(summary)
}

/// Windowed shadow misfire monitor summary. Counts candidate turns compared
/// (rows where `would_trim_shadow_misfire IS NOT NULL`) and misfire turns
/// (`would_trim_shadow_misfire = 1`). A misfire means the trimmed cacheable
/// prefix fingerprint shifted turn-to-turn -- the canary that a live cut would
/// break the upstream cache. Plain data; the caller decides how to display it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ShadowMisfireSummary {
    /// Count of turns where a shadow comparison was made (NOT NULL).
    pub compared_turns: i64,
    /// Count of turns where the fingerprint differed (Misfire, value = 1).
    pub misfire_turns: i64,
}

const SHADOW_MISFIRE_SQL: &str = "\
SELECT
    COUNT(would_trim_shadow_misfire)                                      AS compared_turns,
    COALESCE(SUM(CASE WHEN would_trim_shadow_misfire = 1 THEN 1 ELSE 0 END), 0) AS misfire_turns
FROM requests
WHERE ts_start >= ?1 AND ts_start < ?2";

/// The window's shadow misfire monitor summary. `COUNT(col)` ignores NULLs,
/// so `compared_turns` is the number of turns the monitor compared. All fields
/// are 0 when no row in the window carried a shadow observation.
pub fn shadow_misfire_summary(
    db: &UsageDb,
    from_ms: i64,
    to_ms: i64,
) -> Result<ShadowMisfireSummary, QueryError> {
    let mut stmt = db.conn().prepare(SHADOW_MISFIRE_SQL)?;
    let summary = stmt.query_row([from_ms, to_ms], |row| {
        Ok(ShadowMisfireSummary {
            compared_turns: row.get(0)?,
            misfire_turns: row.get(1)?,
        })
    })?;
    Ok(summary)
}

/// K-estimator calibration triple over all history. Populated by
/// `k_calibration_summary`; zero-fields indicate no calibrated predictions.
///
/// The calibration measures the FLOOR (p10 of the K distribution), which is
/// the persisted gate-authorizing column. The spec's "p50" label refers to
/// the predicted typical reuse; only the floor is persisted, so v1 calibrates
/// the floor. The whole-session realized-reuse metric (`a`) is an optimistic
/// upper bound: it counts all cache-read hits within the session regardless of
/// TTL alignment. TTL-windowed refinement is a v2 concern; this v1 uses the
/// whole-session count as a conservative-on-the-denominator measure that
/// remains auditable without sub-second replay.
#[derive(Debug, Clone, PartialEq)]
pub struct KCalibration {
    /// Population size: rows with `would_trim_k_floor IS NOT NULL`.
    pub n: usize,
    /// Fraction of population where realized reuse >= predicted floor.
    /// PASS threshold: >= 0.90.
    pub coverage: f64,
    /// Median of |floor - realized| / max_realized_reuse over the population.
    /// PASS threshold: <= 0.40.
    pub accuracy: f64,
}

/// Per-row data pulled from the DB for the calibration computation.
struct CalibRow {
    floor: f64,
    /// Whole-session realized K: COUNT of rows in the same
    /// (session_id, provider_kind, model) group with cache_read > 0.
    realized: i64,
}

/// A single read-only GROUP-BY join pulls each calibrated row's floor plus
/// its whole-session realized reuse; coverage, sufficiency, and the median
/// accuracy are thin Rust reductions because SQLite lacks a native MEDIAN.
///
/// `session_reuse` computes whole-session realized K once per
/// (session_id, provider_kind, model) group. The JOIN then attaches each
/// calibrated row's floor to its group's realized count. NULL-session rows
/// and rows without provider_kind or model are excluded by the WHERE guards
/// in both the CTE and the outer query.
const K_CALIBRATION_SQL: &str = "\
WITH session_reuse AS (
    SELECT session_id, provider_kind, model,
           SUM(CASE WHEN cache_read > 0 THEN 1 ELSE 0 END) AS realized
    FROM requests
    WHERE session_id IS NOT NULL AND provider_kind IS NOT NULL AND model IS NOT NULL
    GROUP BY session_id, provider_kind, model
)
SELECT r.would_trim_k_floor AS floor, sr.realized AS realized
FROM requests r
JOIN session_reuse sr
  ON sr.session_id = r.session_id AND sr.provider_kind = r.provider_kind AND sr.model = r.model
WHERE r.would_trim_k_floor IS NOT NULL
  AND r.would_trim_k_floor >= 0.0
  AND r.session_id IS NOT NULL AND r.provider_kind IS NOT NULL AND r.model IS NOT NULL";

/// K-estimator calibration over all history. Measures how well the recorded
/// floor predictions track whole-session realized reuse. Returns a
/// `KCalibration` with n=0 when there are no calibrated predictions.
///
/// The median is computed in Rust over the pulled per-row error values for
/// auditability; SQLite's median extension is non-standard.
pub fn k_calibration_summary(db: &UsageDb) -> Result<KCalibration, QueryError> {
    let mut stmt = db.conn().prepare(K_CALIBRATION_SQL)?;
    let rows = stmt
        .query_map([], |row| {
            Ok(CalibRow {
                floor: row.get(0)?,
                realized: row.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let n = rows.len();
    if n == 0 {
        return Ok(KCalibration {
            n: 0,
            coverage: 0.0,
            accuracy: 0.0,
        });
    }

    let max_realized = rows
        .iter()
        .map(|r| r.realized)
        .max()
        .expect("non-empty: n==0 is guarded above");

    let coverage = rows.iter().filter(|r| r.realized as f64 >= r.floor).count() as f64 / n as f64;

    // Global-max normalizer: median(|floor - actual| / max_realized). A
    // high-reuse outlier session compresses other rows' relative error;
    // per-row normalization is a v2 consideration.
    let mut errors: Vec<f64> = rows
        .iter()
        .map(|r| {
            let diff = (r.floor - r.realized as f64).abs();
            if max_realized > 0 {
                diff / max_realized as f64
            } else {
                0.0
            }
        })
        .collect();
    errors.sort_by(|a, b| a.total_cmp(b));
    let accuracy = median_f64(&errors);

    Ok(KCalibration {
        n,
        coverage,
        accuracy,
    })
}

/// Nearest-rank median of a non-empty sorted slice.
fn median_f64(sorted: &[f64]) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let n = sorted.len();
    let mid = n / 2;
    if n.is_multiple_of(2) {
        (sorted[mid - 1] + sorted[mid]) / 2.0
    } else {
        sorted[mid]
    }
}

const TTFBS_SQL: &str = "\
SELECT model, provider, upstream, alias, ttfb_ms
FROM requests
WHERE ts_start >= ?1 AND ts_start < ?2
  AND ttfb_ms IS NOT NULL AND stream = 1 AND outcome = 'ok'";

/// Raw in-window `ttfb_ms` values with their group keys, so the CLI can
/// compute time-to-first-token percentiles per display group. Restricted to
/// streaming, successful rows with a recorded TTFB (the only rows where the
/// figure is meaningful).
pub fn ttfbs(db: &UsageDb, from_ms: i64, to_ms: i64) -> Result<Vec<(GroupKey, i64)>, QueryError> {
    let mut stmt = db.conn().prepare(TTFBS_SQL)?;
    let rows = stmt
        .query_map([from_ms, to_ms], |row| {
            let key = GroupKey {
                model: row.get(0)?,
                provider: row.get(1)?,
                upstream: row.get(2)?,
                alias: row.get(3)?,
            };
            Ok((key, row.get::<_, i64>(4)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// One raw reuse sample for the K estimator's rebuild path. Usage-LOCAL:
/// epoch-ms timestamp and signed counts as stored, with no router types. The
/// caller (the router-side rebuild) owns the reuse definition and the
/// SystemTime conversion; this layer only hands back the columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReuseSampleRow {
    /// Inbound session identifier the request was recorded under.
    pub session_id: String,
    /// Stable provider-kind token of the served target.
    pub provider_kind: String,
    /// Served model nickname.
    pub model: String,
    /// Request start time, epoch-millis UTC.
    pub ts_start_ms: i64,
    /// Cached prefix tokens re-read on the upstream response. NULL coalesces
    /// to 0.
    pub cache_read: i64,
}

const REUSE_SAMPLES_SQL: &str = "\
SELECT ts_start, session_id, provider_kind, model, COALESCE(cache_read, 0)
FROM requests
WHERE ts_start >= ?1
  AND session_id IS NOT NULL
  AND provider_kind IS NOT NULL
  AND model IS NOT NULL
ORDER BY ts_start ASC
LIMIT ?2";

/// Raw reuse samples whose request start time is at or after `window_start_ms`,
/// ordered oldest-first, capped at `limit`.
///
/// Rows without a `session_id`, `provider_kind`, or `model` are filtered out:
/// the K estimator keys on the full (session, provider_kind, model) triple, so
/// a NULL in any of the three has no usable identity and is dropped rather than
/// mapped to a sentinel. `cache_read` is COALESCEd to 0 (a NULL counter is a
/// no-reuse observation). Plain data; the router derives the reuse boolean and
/// the `SystemTime` from these columns.
pub fn read_reuse_samples_since(
    conn: &rusqlite::Connection,
    window_start_ms: i64,
    limit: usize,
) -> rusqlite::Result<Vec<ReuseSampleRow>> {
    let mut stmt = conn.prepare(REUSE_SAMPLES_SQL)?;
    let rows = stmt
        .query_map(rusqlite::params![window_start_ms, limit as i64], |row| {
            Ok(ReuseSampleRow {
                ts_start_ms: row.get(0)?,
                session_id: row.get(1)?,
                provider_kind: row.get(2)?,
                model: row.get(3)?,
                cache_read: row.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{open, open_readonly};
    use std::path::PathBuf;
    use tempfile::TempDir;

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

    fn insert_quota_row(
        db: &UsageDb,
        request_id: &str,
        ts_start: i64,
        status: &str,
        utilization: f64,
        reset: i64,
    ) {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, stream, outcome, latency_ms, tool_count, \
                 msg_count, attempt_count, fallback_count, quota_status, \
                 quota_utilization, quota_reset) \
                 VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 0, 'ok', 0, 0, 0, 1, 0, \
                 ?3, ?4, ?5)",
                rusqlite::params![ts_start, request_id, status, utilization, reset],
            )
            .expect("insert quota row");
    }

    fn find_row<'a>(rows: &'a [AggRow], provider: &str, upstream: &str) -> &'a AggRow {
        rows.iter()
            .find(|r| {
                r.key.provider.as_deref() == Some(provider)
                    && r.key.upstream.as_deref() == Some(upstream)
            })
            .expect("group present")
    }

    #[test]
    fn aggregate_groups_counts_and_sums_per_group() {
        // Arrange: two (provider, upstream) pairs, two outcomes, NULL tokens.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        // Group A: provider=pa upstream=ua -- 2 ok + 1 error.
        insert_row(
            &db,
            "a1",
            100,
            "m1",
            "pa",
            "ua",
            "al",
            "ok",
            Some(10),
            Some(20),
            5,
            None,
        );
        insert_row(
            &db,
            "a2",
            110,
            "m1",
            "pa",
            "ua",
            "al",
            "ok",
            Some(5),
            Some(7),
            15,
            None,
        );
        insert_row(
            &db,
            "a3",
            120,
            "m1",
            "pa",
            "ua",
            "al",
            "upstream_error",
            None,
            None,
            25,
            None,
        );
        // Group B: provider=pb upstream=ub -- 1 ok.
        insert_row(
            &db,
            "b1",
            130,
            "m2",
            "pb",
            "ub",
            "al",
            "ok",
            Some(3),
            None,
            9,
            None,
        );

        // Act
        let rows = aggregate(&db, 0, 1000).expect("aggregate");

        // Assert: two groups.
        assert_eq!(rows.len(), 2);
        let a = find_row(&rows, "pa", "ua");
        assert_eq!(a.requests, 3);
        assert_eq!(a.ok, 2);
        assert_eq!(a.errors, 1);
        // input: 10 + 5 + 0(NULL) = 15; output: 20 + 7 + 0 = 27.
        assert_eq!(a.input_tokens, 15);
        assert_eq!(a.output_tokens, 27);

        let b = find_row(&rows, "pb", "ub");
        assert_eq!(b.requests, 1);
        assert_eq!(b.ok, 1);
        assert_eq!(b.errors, 0);
        assert_eq!(b.input_tokens, 3);
        // output was NULL -> 0.
        assert_eq!(b.output_tokens, 0);
    }

    #[test]
    fn aggregate_excludes_rows_outside_window() {
        // Arrange
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_row(
            &db,
            "in",
            500,
            "m",
            "p",
            "u",
            "a",
            "ok",
            Some(1),
            Some(1),
            1,
            None,
        );
        insert_row(
            &db,
            "lo",
            99,
            "m",
            "p",
            "u",
            "a",
            "ok",
            Some(1),
            Some(1),
            1,
            None,
        );
        insert_row(
            &db,
            "hi",
            1000,
            "m",
            "p",
            "u",
            "a",
            "ok",
            Some(1),
            Some(1),
            1,
            None,
        );

        // Act: window [100, 1000) excludes ts 99 and ts 1000.
        let rows = aggregate(&db, 100, 1000).expect("aggregate");

        // Assert
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].requests, 1);
    }

    #[test]
    fn aggregate_sums_server_tool_calls_from_json() {
        // Arrange: two rows whose server_tool_use JSON maps carry int counts.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_row(
            &db,
            "s1",
            100,
            "m",
            "p",
            "u",
            "a",
            "ok",
            None,
            None,
            1,
            Some(r#"{"web_search": 2, "code_exec": 1}"#),
        );
        insert_row(
            &db,
            "s2",
            110,
            "m",
            "p",
            "u",
            "a",
            "ok",
            None,
            None,
            1,
            Some(r#"{"web_search": 3}"#),
        );
        // A row with no server tools contributes 0.
        insert_row(
            &db, "s3", 120, "m", "p", "u", "a", "ok", None, None, 1, None,
        );

        // Act
        let rows = aggregate(&db, 0, 1000).expect("aggregate");

        // Assert: 2 + 1 + 3 = 6 invocations across the group.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].server_tool_calls, 6);
    }

    #[test]
    fn aggregate_cache_read_reports_peak_avg_and_billed_with_distinct_semantics() {
        // Arrange: several rows in the SAME group with a CLIMBING cache_read.
        // cache_read is a per-turn SNAPSHOT of the cached prefix re-read that
        // turn. For DISPLAY (context SIZE) the group reports the peak (MAX) and
        // mean (AVG) -- summing those would repeat-count the same growing
        // prefix. For COST, cache reads are billed PER TURN, so the cumulative
        // cost basis IS the sum (`cache_read_billed`). All three must coexist
        // with the right semantics.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_full_row(
            &db,
            "k1",
            100,
            1,
            "ok",
            Some(10),
            50,
            Some(1),
            None,
            Some(88_000),
        );
        insert_full_row(
            &db,
            "k2",
            110,
            1,
            "ok",
            Some(10),
            50,
            Some(1),
            None,
            Some(89_000),
        );
        insert_full_row(
            &db,
            "k3",
            120,
            1,
            "ok",
            Some(10),
            50,
            Some(1),
            None,
            Some(91_000),
        );

        // Act
        let rows = aggregate(&db, 0, 1000).expect("aggregate");

        // Assert: one group; peak is the MAX, avg is the integer mean, and the
        // billed figure is the SUM (the cost basis), distinct from peak/avg.
        assert_eq!(rows.len(), 1);
        let r = &rows[0];
        assert_eq!(r.cache_read_peak, 91_000);
        assert_eq!(r.cache_read_avg, 89_333); // (88000+89000+91000)/3 truncated
        assert_eq!(r.cache_read_billed, 268_000); // SUM, the per-turn cost basis
                                                  // The display figures must NOT equal the billed sum.
        assert_ne!(r.cache_read_peak, r.cache_read_billed);
        assert_ne!(r.cache_read_avg, r.cache_read_billed);
        // cache_read_present still counts the reporting rows (all three).
        assert_eq!(r.cache_read_present, 3);
    }

    #[test]
    fn aggregate_null_model_attributes_to_requested_model() {
        // Arrange: a pre-dispatch abort has model=NULL but always carries a
        // requested_model. The aggregate must attribute it to requested_model
        // (the route asked for), not drop it into a NULL group key.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
                 input_tokens, output_tokens) \
                 VALUES (100, 100, 'abort', 'openai', 'asked-model', 'al', NULL, NULL, \
                 NULL, 0, 'client_disconnect', 5, 0, 0, 0, 0, 7, 0)",
                [],
            )
            .expect("insert null-model row");

        // Act
        let rows = aggregate(&db, 0, 1000).expect("aggregate");

        // Assert: the group key's model is the requested_model, never NULL.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].key.model.as_deref(), Some("asked-model"));
        assert!(
            rows[0].key.model.is_some(),
            "must not be a NULL model bucket"
        );
    }

    /// Insert a row with an optional `would_trim_tokens` value so the
    /// would-trim summary's NULL-vs-present accounting is testable.
    fn insert_would_trim_row(
        db: &UsageDb,
        request_id: &str,
        ts_start: i64,
        would_trim_tokens: Option<i64>,
    ) {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, stream, outcome, latency_ms, tool_count, \
                 msg_count, attempt_count, fallback_count, would_trim_tokens) \
                 VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 0, 'ok', 0, 0, 0, 1, 0, ?3)",
                rusqlite::params![ts_start, request_id, would_trim_tokens],
            )
            .expect("insert would-trim row");
    }

    #[test]
    fn would_trim_summary_counts_candidates_and_sums_tokens() {
        // Arrange: two rows with candidates, one without (NULL), plus an
        // out-of-window candidate row.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_would_trim_row(&db, "w1", 100, Some(40_000));
        insert_would_trim_row(&db, "w2", 110, Some(20_000));
        insert_would_trim_row(&db, "w3", 120, None);
        insert_would_trim_row(&db, "out", 5, Some(99_000));

        // Act
        let s = would_trim_summary(&db, 100, 1000).expect("summary");

        // Assert: COUNT ignores the NULL row and the out-of-window row.
        assert_eq!(s.candidate_requests, 2);
        assert_eq!(s.would_trim_tokens, 60_000);
    }

    #[test]
    fn would_trim_summary_is_zero_when_no_candidates() {
        // Arrange: only a plain row with no would-trim candidate.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_would_trim_row(&db, "plain", 100, None);

        // Act + Assert
        let s = would_trim_summary(&db, 0, 1000).expect("summary");
        assert_eq!(s.candidate_requests, 0);
        assert_eq!(s.would_trim_tokens, 0);
    }

    #[test]
    fn latest_quota_returns_most_recent_quota_row() {
        // Arrange: two quota rows at different ts_start.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_quota_row(&db, "q-old", 100, "active", 0.10, 5_000);
        insert_quota_row(&db, "q-new", 200, "throttled", 0.90, 9_000);
        // A non-quota row must be ignored.
        insert_row(
            &db, "plain", 300, "m", "p", "u", "a", "ok", None, None, 1, None,
        );

        // Act
        let snap = latest_quota(&db).expect("query").expect("some snapshot");

        // Assert: the newer quota row wins.
        assert_eq!(snap.ts_start, 200);
        assert_eq!(snap.status.as_deref(), Some("throttled"));
        assert_eq!(snap.utilization, Some(0.90));
        assert_eq!(snap.reset, Some(9_000));
    }

    #[test]
    fn latest_quota_returns_none_when_no_quota_rows() {
        // Arrange
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_row(
            &db, "plain", 100, "m", "p", "u", "a", "ok", None, None, 1, None,
        );

        // Act + Assert
        assert!(latest_quota(&db).expect("query").is_none());
    }

    #[test]
    fn aggregate_over_readonly_open_matches_seeded_results() {
        // Arrange: seed via the read-write open, then drop it so the file is
        // read through the real CLI path (open_readonly).
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_row(
            &db,
            "ro-a1",
            100,
            "m1",
            "pa",
            "ua",
            "al",
            "ok",
            Some(10),
            Some(20),
            5,
            None,
        );
        insert_row(
            &db,
            "ro-a2",
            110,
            "m1",
            "pa",
            "ua",
            "al",
            "upstream_error",
            None,
            None,
            15,
            None,
        );
        drop(db);

        // Act: read via the read-only open path.
        let ro = open_readonly(&path).expect("open readonly");
        let rows = aggregate(&ro, 0, 1000).expect("aggregate");

        // Assert
        assert_eq!(rows.len(), 1);
        let a = find_row(&rows, "pa", "ua");
        assert_eq!(a.requests, 2);
        assert_eq!(a.ok, 1);
        assert_eq!(a.errors, 1);
        assert_eq!(a.input_tokens, 10);
        assert_eq!(a.output_tokens, 20);
        assert_eq!(a.key.alias, "al");
    }

    #[test]
    fn latest_quota_over_readonly_open_matches_seeded_results() {
        // Arrange: seed quota rows, then drop the writer.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_quota_row(&db, "ro-q-old", 100, "active", 0.10, 5_000);
        insert_quota_row(&db, "ro-q-new", 200, "throttled", 0.90, 9_000);
        drop(db);

        // Act
        let ro = open_readonly(&path).expect("open readonly");
        let snap = latest_quota(&ro).expect("query").expect("some snapshot");

        // Assert: same most-recent row as the read-write path returns.
        assert_eq!(snap.ts_start, 200);
        assert_eq!(snap.status.as_deref(), Some("throttled"));
        assert_eq!(snap.utilization, Some(0.90));
        assert_eq!(snap.reset, Some(9_000));
    }

    #[test]
    fn aggregate_gen_window_only_counts_streaming_ok_rows_with_ttfb() {
        // Arrange: one qualifying streaming-ok row, plus rows that the
        // predicate must exclude (non-stream, error, NULL ttfb, latency<=ttfb).
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        // Qualifying: stream, ok, ttfb=100, latency=500 -> gen window 400.
        insert_full_row(
            &db,
            "g1",
            100,
            1,
            "ok",
            Some(100),
            500,
            Some(40),
            None,
            None,
        );
        // Non-stream row excluded.
        insert_full_row(
            &db,
            "g2",
            110,
            0,
            "ok",
            Some(100),
            500,
            Some(40),
            None,
            None,
        );
        // Error row excluded.
        insert_full_row(
            &db,
            "g3",
            120,
            1,
            "upstream_error",
            Some(100),
            500,
            Some(40),
            None,
            None,
        );
        // NULL ttfb excluded.
        insert_full_row(&db, "g4", 130, 1, "ok", None, 500, Some(40), None, None);
        // latency <= ttfb excluded.
        insert_full_row(
            &db,
            "g5",
            140,
            1,
            "ok",
            Some(500),
            500,
            Some(40),
            None,
            None,
        );

        // Act
        let rows = aggregate(&db, 0, 1000).expect("aggregate");

        // Assert: only the first row contributes to the generation window.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].gen_window_ms, 400);
        assert_eq!(rows[0].gen_output_tokens, 40);
    }

    #[test]
    fn aggregate_presence_counts_distinguish_null_from_reported_zero() {
        // Arrange: one row reasoning=0 (reported), one row reasoning=NULL.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_full_row(
            &db,
            "p1",
            100,
            1,
            "ok",
            Some(10),
            50,
            Some(1),
            Some(0),
            Some(5),
        );
        insert_full_row(&db, "p2", 110, 1, "ok", Some(10), 50, Some(1), None, None);

        // Act
        let rows = aggregate(&db, 0, 1000).expect("aggregate");

        // Assert: COUNT(col) ignores the NULL row -> 1, not 2.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].reasoning_present, 1);
        assert_eq!(rows[0].cache_read_present, 1);
        assert_eq!(rows[0].reasoning_tokens, 0);
    }

    #[test]
    fn aggregate_stream_count_sums_streaming_flag() {
        // Arrange: two streaming rows, one non-stream row.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_full_row(&db, "c1", 100, 1, "ok", Some(10), 50, Some(1), None, None);
        insert_full_row(&db, "c2", 110, 1, "ok", Some(10), 50, Some(1), None, None);
        insert_full_row(&db, "c3", 120, 0, "ok", Some(10), 50, Some(1), None, None);

        // Act
        let rows = aggregate(&db, 0, 1000).expect("aggregate");

        // Assert
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].stream_count, 2);
        assert_eq!(rows[0].ttfb_count, 3);
        assert_eq!(rows[0].sum_ttfb_ms, 30);
    }

    #[test]
    fn ttfbs_returns_in_window_streaming_ok_values() {
        // Arrange: two qualifying streaming-ok rows, plus excluded rows.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_full_row(&db, "t1", 100, 1, "ok", Some(11), 50, Some(1), None, None);
        insert_full_row(&db, "t2", 110, 1, "ok", Some(22), 50, Some(1), None, None);
        // Non-stream excluded.
        insert_full_row(&db, "t3", 120, 0, "ok", Some(33), 50, Some(1), None, None);
        // Error excluded.
        insert_full_row(
            &db,
            "t4",
            130,
            1,
            "timeout",
            Some(44),
            50,
            Some(1),
            None,
            None,
        );
        // Out of window excluded.
        insert_full_row(&db, "t5", 5, 1, "ok", Some(55), 50, Some(1), None, None);

        // Act
        let rows = ttfbs(&db, 100, 1000).expect("ttfbs");

        // Assert
        let values: Vec<i64> = rows.iter().map(|(_, ms)| *ms).collect();
        assert_eq!(values.len(), 2);
        assert!(values.contains(&11));
        assert!(values.contains(&22));
        assert!(!values.contains(&33));
        assert!(!values.contains(&44));
        assert!(!values.contains(&55));
    }

    /// Insert a row exercising the reuse-sample columns: nullable
    /// `session_id`, `provider_kind`, `model`, and `cache_read`.
    #[allow(clippy::too_many_arguments)]
    fn insert_reuse_row(
        db: &UsageDb,
        request_id: &str,
        ts_start: i64,
        session_id: Option<&str>,
        provider_kind: Option<&str>,
        model: Option<&str>,
        cache_read: Option<i64>,
    ) {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider_kind, session_id, stream, outcome, \
                 latency_ms, tool_count, msg_count, attempt_count, fallback_count, cache_read) \
                 VALUES (?1, ?1, ?2, 'anthropic', 'req-model', 'al', ?3, ?4, ?5, 1, 'ok', \
                 5, 0, 0, 1, 0, ?6)",
                rusqlite::params![
                    ts_start,
                    request_id,
                    model,
                    provider_kind,
                    session_id,
                    cache_read,
                ],
            )
            .expect("insert reuse row");
    }

    #[test]
    fn read_reuse_samples_filters_nulls_coalesces_and_orders() {
        // Arrange: a mix of complete and partial rows, two triples, delivered
        // out of ts order, plus an out-of-window row.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        // Complete rows for triple A (one with NULL cache_read -> 0).
        insert_reuse_row(
            &db,
            "a2",
            200,
            Some("s1"),
            Some("anthropic-api"),
            Some("opus"),
            Some(42),
        );
        insert_reuse_row(
            &db,
            "a1",
            100,
            Some("s1"),
            Some("anthropic-api"),
            Some("opus"),
            None,
        );
        // A second triple (different provider_kind).
        insert_reuse_row(
            &db,
            "b1",
            150,
            Some("s1"),
            Some("bedrock"),
            Some("opus"),
            Some(7),
        );
        // NULL session_id -> filtered out (no usable triple identity).
        insert_reuse_row(
            &db,
            "n-sess",
            120,
            None,
            Some("anthropic-api"),
            Some("opus"),
            Some(9),
        );
        // NULL provider_kind -> filtered out.
        insert_reuse_row(&db, "n-pk", 130, Some("s2"), None, Some("opus"), Some(9));
        // NULL model -> filtered out.
        insert_reuse_row(
            &db,
            "n-model",
            140,
            Some("s2"),
            Some("anthropic-api"),
            None,
            Some(9),
        );
        // Out of window (ts < window_start).
        insert_reuse_row(
            &db,
            "old",
            50,
            Some("s1"),
            Some("anthropic-api"),
            Some("opus"),
            Some(99),
        );

        // Act: window starts at 100.
        let rows = read_reuse_samples_since(db.conn(), 100, 100).expect("read");

        // Assert: three rows survive (the three complete, in-window rows),
        // ordered ascending by ts.
        let ids: Vec<i64> = rows.iter().map(|r| r.ts_start_ms).collect();
        assert_eq!(ids, vec![100, 150, 200]);
        // NULL cache_read coalesced to 0 on a1.
        let a1 = rows.iter().find(|r| r.ts_start_ms == 100).expect("a1");
        assert_eq!(a1.cache_read, 0);
        assert_eq!(a1.session_id, "s1");
        assert_eq!(a1.provider_kind, "anthropic-api");
        assert_eq!(a1.model, "opus");
        // The cross-provider row maps its own provider_kind.
        let b1 = rows.iter().find(|r| r.ts_start_ms == 150).expect("b1");
        assert_eq!(b1.provider_kind, "bedrock");
        assert_eq!(b1.cache_read, 7);
    }

    #[test]
    fn read_reuse_samples_respects_limit() {
        // Arrange: five eligible rows.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        for i in 0..5i64 {
            insert_reuse_row(
                &db,
                &format!("r{i}"),
                100 + i,
                Some("s1"),
                Some("anthropic-api"),
                Some("opus"),
                Some(1),
            );
        }

        // Act: cap at 3.
        let rows = read_reuse_samples_since(db.conn(), 0, 3).expect("read");

        // Assert: the three OLDEST (ascending order, then LIMIT).
        let ids: Vec<i64> = rows.iter().map(|r| r.ts_start_ms).collect();
        assert_eq!(ids, vec![100, 101, 102]);
    }
}
