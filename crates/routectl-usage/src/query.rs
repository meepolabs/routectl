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
    SUM(CASE WHEN outcome NOT IN ('ok', 'client_disconnect')
        THEN 1 ELSE 0 END)                              AS errors,
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
    COALESCE(SUM(stream), 0)                            AS stream_count,
    SUM(CASE WHEN outcome = 'client_disconnect' THEN 1 ELSE 0 END)
                                                         AS client_disconnect_total,
    SUM(CASE WHEN outcome = 'client_disconnect' AND r.model IS NULL
        THEN 1 ELSE 0 END)                              AS client_disconnect_pre_dispatch
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
        client_disconnect_total: row.get(26)?,
        client_disconnect_pre_dispatch: row.get(27)?,
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

/// Windowed M1 near-lossless attribution: per-heuristic freed-token sums,
/// the path-extractability count-pair, and the context-fraction count-pair,
/// RESTRICTED to rows where `would_trim_recorder_version IS NOT NULL` (the
/// M1 near-lossless pass ran). This filter is load-bearing: pre-M1 rows never
/// carry these columns, so without it a mixed-history window would silently
/// blend baseline and M1 semantics. Count-pairs (`path_units`/
/// `path_extractable`, `context_fraction_present`/`context_fraction_sum`) are
/// summed as raw counters here -- divide AFTER summing; never average a
/// per-row rate. Plain data; the caller decides how to display it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct M1AttributionSummary {
    /// Count of requests where the M1 recorder ran
    /// (`would_trim_recorder_version IS NOT NULL`), regardless of whether it
    /// found any marks. The M1-recorder candidate count.
    pub recorder_requests: i64,
    /// Summed dedup-heuristic freed-token count over `recorder_requests`.
    pub dedup_tokens: i64,
    /// Summed supersession-heuristic freed-token count over
    /// `recorder_requests`.
    pub supersession_tokens: i64,
    /// Summed path units considered for supersession-key extraction.
    pub path_units: i64,
    /// Summed path units that were extractable. Paired with `path_units`:
    /// the rate is `path_extractable as f64 / path_units as f64`.
    pub path_extractable: i64,
    /// Count of `recorder_requests` with a known `would_trim_context_fraction`
    /// (fail-closed `NULL` when the model's context window was unknown).
    pub context_fraction_present: i64,
    /// Summed `would_trim_context_fraction` over `context_fraction_present`
    /// rows. Paired with `context_fraction_present`: the mean is
    /// `context_fraction_sum / context_fraction_present as f64`.
    pub context_fraction_sum: f64,
}

const M1_ATTRIBUTION_SQL: &str = "\
SELECT
    COUNT(would_trim_recorder_version)                      AS recorder_requests,
    COALESCE(SUM(would_trim_dedup_tokens), 0)                AS dedup_tokens,
    COALESCE(SUM(would_trim_supersession_tokens), 0)         AS supersession_tokens,
    COALESCE(SUM(would_trim_path_units), 0)                  AS path_units,
    COALESCE(SUM(would_trim_path_extractable), 0)             AS path_extractable,
    COUNT(would_trim_context_fraction)                        AS context_fraction_present,
    COALESCE(SUM(would_trim_context_fraction), 0.0)           AS context_fraction_sum
FROM requests
WHERE ts_start >= ?1 AND ts_start < ?2
  AND would_trim_recorder_version IS NOT NULL";

/// The window's M1 near-lossless attribution. Restricted to
/// `would_trim_recorder_version IS NOT NULL` so baseline (pre-M1) rows never
/// mix into these totals. All fields are 0 when no row in the window carried
/// an M1 recording.
pub fn m1_attribution_summary(
    db: &UsageDb,
    from_ms: i64,
    to_ms: i64,
) -> Result<M1AttributionSummary, QueryError> {
    let mut stmt = db.conn().prepare(M1_ATTRIBUTION_SQL)?;
    let summary = stmt.query_row([from_ms, to_ms], |row| {
        Ok(M1AttributionSummary {
            recorder_requests: row.get(0)?,
            dedup_tokens: row.get(1)?,
            supersession_tokens: row.get(2)?,
            path_units: row.get(3)?,
            path_extractable: row.get(4)?,
            context_fraction_present: row.get(5)?,
            context_fraction_sum: row.get(6)?,
        })
    })?;
    Ok(summary)
}

/// K-estimator calibration triple over all history. Populated by
/// `k_calibration_summary`; zero-fields indicate no calibrated predictions.
///
/// The calibration measures the persisted FLOOR (`would_trim_k_floor`, the
/// only gate-authorizing bound) against REMAINING-FUTURE realized reuse from
/// each row's point in time -- the count of later same-triple rows that
/// actually observed a cache read. This is deliberately NOT whole-session
/// realized reuse: a whole-session comparison counts reuse that happened
/// BEFORE the prediction as if it validated the prediction, which
/// systematically blesses late-session over-predictions (the money-losing
/// direction). Remaining-future is the honest question the floor is asked to
/// answer -- "will the prefix be re-read enough MORE times to justify cutting
/// it now?".
#[derive(Debug, Clone, PartialEq)]
pub struct KCalibration {
    /// Population size: rows with `would_trim_k_floor IS NOT NULL`.
    pub n: usize,
    /// Fraction of population where remaining-future reuse >= predicted
    /// floor. PRIMARY safety metric. PASS threshold: >= 0.90.
    pub coverage: f64,
    /// Median of `|floor - realized_remaining| / (realized_remaining + 1)`
    /// over the population -- per-row normalized so one high-reuse row can no
    /// longer compress everyone else's error toward zero. DIAGNOSTIC only,
    /// not a safety gate.
    pub accuracy: f64,
    /// Mean first-half -> second-half per-turn continuation-rate delta across
    /// qualifying (session, provider_kind, model) groups. A material NEGATIVE
    /// value means reuse decays late in a session; read before the live-cut
    /// go/no-go decision, it is
    /// the trigger to open the age-conditioned-hazard design (a constant
    /// pooled hazard would over-predict E[K] late). DIAGNOSTIC, never a gate.
    /// 0.0 when no group has enough rows to split into meaningful halves.
    pub hazard_decay: f64,
}

/// Per-row data pulled from the DB for the calibration computation.
struct CalibRow {
    floor: f64,
    /// Remaining-future realized K: COUNT of rows in the same
    /// (session_id, provider_kind, model) group with cache_read > 0 that
    /// occur STRICTLY AFTER this row, ordered by (ts_start, rowid).
    realized_remaining: i64,
}

/// A single read-only pass computes each row's REMAINING-FUTURE realized
/// reuse via a windowed running count over the same-triple rows that follow
/// it, then filters to the calibrated rows. Coverage, sufficiency, and the
/// median accuracy are thin Rust reductions because SQLite lacks a native
/// MEDIAN.
///
/// The window frame `ROWS BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING`
/// counts, per row, cache_read>0 rows strictly after it within its
/// (session_id, provider_kind, model) partition, ordered by (ts_start,
/// rowid). The COALESCE turns the empty frame of a group's last row (a NULL
/// SUM) into 0. The subquery runs the window over ALL valid-triple rows so
/// future reuse is counted even from uncalibrated rows; the outer WHERE then
/// restricts the population to calibrated rows.
const K_CALIBRATION_SQL: &str = "\
SELECT floor, realized_remaining FROM (
    SELECT r.would_trim_k_floor AS floor,
           COALESCE(SUM(CASE WHEN cache_read > 0 THEN 1 ELSE 0 END) OVER (
               PARTITION BY session_id, provider_kind, model
               ORDER BY ts_start, rowid
               ROWS BETWEEN 1 FOLLOWING AND UNBOUNDED FOLLOWING
           ), 0) AS realized_remaining
    FROM requests r
    WHERE session_id IS NOT NULL AND provider_kind IS NOT NULL AND model IS NOT NULL
)
WHERE floor IS NOT NULL AND floor >= 0.0";

/// Minimum rows in a (session, provider_kind, model) group before its
/// first-half/second-half continuation-rate delta is meaningful enough to
/// fold into `hazard_decay`. Below this a split is one-vs-one or one-vs-two,
/// too noisy to inform the age-conditioning decision.
const HAZARD_DECAY_MIN_GROUP_ROWS: usize = 4;

/// Ordered per-turn reuse outcomes for the hazard-decay reduction. Delivered
/// grouped by the triple and oldest-first WITHIN each group, so consecutive
/// rows sharing a triple form one session's turn sequence.
const K_HAZARD_DECAY_SQL: &str = "\
SELECT session_id, provider_kind, model,
       CASE WHEN cache_read > 0 THEN 1 ELSE 0 END AS hit
FROM requests
WHERE session_id IS NOT NULL AND provider_kind IS NOT NULL AND model IS NOT NULL
ORDER BY session_id, provider_kind, model, ts_start, rowid";

/// One ordered per-turn reuse outcome for the hazard-decay reduction.
struct HitRow {
    session_id: String,
    provider_kind: String,
    model: String,
    /// True when the turn observed a cache read.
    hit: bool,
}

/// K-estimator calibration over all history. Measures how well the recorded
/// floor predictions track REMAINING-FUTURE realized reuse (see
/// [`KCalibration`]). Returns a `KCalibration` with n=0 when there are no
/// calibrated predictions.
///
/// The median is computed in Rust over the pulled per-row error values for
/// auditability; SQLite's median extension is non-standard. `hazard_decay`
/// is a second small reduction over the ordered per-turn reuse outcomes.
pub fn k_calibration_summary(db: &UsageDb) -> Result<KCalibration, QueryError> {
    let rows = {
        let mut stmt = db.conn().prepare(K_CALIBRATION_SQL)?;
        stmt.query_map([], |row| {
            Ok(CalibRow {
                floor: row.get(0)?,
                realized_remaining: row.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?
    };

    let n = rows.len();
    if n == 0 {
        return Ok(KCalibration {
            n: 0,
            coverage: 0.0,
            accuracy: 0.0,
            hazard_decay: 0.0,
        });
    }

    let coverage = rows
        .iter()
        .filter(|r| r.realized_remaining as f64 >= r.floor)
        .count() as f64
        / n as f64;

    // Per-row normalize the error (guard +1 so a 0-remaining row is finite).
    // Replaces the global-max normalizer, under which one high-reuse row
    // compressed every other row's relative error toward zero.
    let mut errors: Vec<f64> = rows
        .iter()
        .map(|r| {
            let realized = r.realized_remaining as f64;
            (r.floor - realized).abs() / (realized + 1.0)
        })
        .collect();
    errors.sort_by(f64::total_cmp);
    let accuracy = median_f64(&errors);

    let hazard_decay = {
        let mut stmt = db.conn().prepare(K_HAZARD_DECAY_SQL)?;
        let hit_rows = stmt
            .query_map([], |row| {
                Ok(HitRow {
                    session_id: row.get(0)?,
                    provider_kind: row.get(1)?,
                    model: row.get(2)?,
                    hit: row.get::<_, bool>(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        compute_hazard_decay(&hit_rows)
    };

    Ok(KCalibration {
        n,
        coverage,
        accuracy,
        hazard_decay,
    })
}

/// Mean first-half -> second-half per-turn continuation-rate delta across
/// (session, provider_kind, model) groups with at least
/// [`HAZARD_DECAY_MIN_GROUP_ROWS`] rows. For each qualifying group the rows
/// (already ordered oldest-first within the group) split at the midpoint;
/// `delta = second_half_rate - first_half_rate`, where each rate is the
/// fraction of that half's rows that observed a cache read. `hazard_decay` is
/// the mean delta across qualifying groups, or 0.0 when none qualify. A
/// material NEGATIVE mean means reuse decays late in a session.
///
/// `rows` MUST arrive grouped by the triple and oldest-first within each
/// group (as `K_HAZARD_DECAY_SQL` orders them), so consecutive equal-triple
/// rows form one session's turn sequence.
fn compute_hazard_decay(rows: &[HitRow]) -> f64 {
    let mut deltas: Vec<f64> = Vec::new();
    for group in rows.chunk_by(same_group) {
        if group.len() >= HAZARD_DECAY_MIN_GROUP_ROWS {
            let mid = group.len() / 2;
            deltas.push(hit_rate(&group[mid..]) - hit_rate(&group[..mid]));
        }
    }

    if deltas.is_empty() {
        0.0
    } else {
        deltas.iter().sum::<f64>() / deltas.len() as f64
    }
}

/// True when two hit rows belong to the same (session, provider_kind, model)
/// group.
fn same_group(a: &HitRow, b: &HitRow) -> bool {
    a.session_id == b.session_id && a.provider_kind == b.provider_kind && a.model == b.model
}

/// Fraction of a non-empty slice of turns that observed a cache read.
fn hit_rate(rows: &[HitRow]) -> f64 {
    let hits = rows.iter().filter(|r| r.hit).count();
    hits as f64 / rows.len() as f64
}

/// Nearest-rank median of a non-empty sorted slice.
fn median_f64(sorted: &[f64]) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let n = sorted.len();
    let mid = n / 2;
    if n.is_multiple_of(2) {
        f64::midpoint(sorted[mid - 1], sorted[mid])
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
  AND outcome = 'ok'
ORDER BY ts_start ASC
LIMIT ?2";

/// Raw reuse samples whose request start time is at or after `window_start_ms`,
/// ordered oldest-first, capped at `limit`.
///
/// Admission contract: `outcome = 'ok'` ONLY, matching the live sample path
/// (the live K-store write fires only on the non-streaming success finalize
/// and on natural stream EOS, both of which finalize as `Outcome::Ok`). A
/// mid-stream failure (e.g. `upstream_error`) may have observed partial
/// `cache_read`, but it never reaches the live K store, so the warm rebuild
/// must not replay it either -- otherwise a restart would admit rows live
/// traffic never would, silently diverging the two paths' K-store contents.
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

    #[test]
    fn aggregate_errors_excludes_client_disconnect_rows() {
        // Arrange: one ok row and one client_disconnect row in the same group.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_row(
            &db,
            "ok1",
            100,
            "m",
            "p",
            "u",
            "a",
            "ok",
            Some(1),
            Some(1),
            5,
            None,
        );
        insert_row(
            &db,
            "cd1",
            110,
            "m",
            "p",
            "u",
            "a",
            "client_disconnect",
            None,
            None,
            5,
            None,
        );

        // Act
        let rows = aggregate(&db, 0, 1000).expect("aggregate");

        // Assert: the disconnect row counts toward requests but not errors.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].requests, 2);
        assert_eq!(rows[0].ok, 1);
        assert_eq!(rows[0].errors, 0);
    }

    #[test]
    fn aggregate_errors_still_counts_gate_blocked_and_upstream_error() {
        // Arrange: a gate_blocked and an upstream_error row, plus one ok row.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_row(
            &db,
            "ok1",
            100,
            "m",
            "p",
            "u",
            "a",
            "ok",
            Some(1),
            Some(1),
            5,
            None,
        );
        insert_row(
            &db,
            "gb1",
            110,
            "m",
            "p",
            "u",
            "a",
            "gate_blocked",
            None,
            None,
            5,
            None,
        );
        insert_row(
            &db,
            "ue1",
            120,
            "m",
            "p",
            "u",
            "a",
            "upstream_error",
            None,
            None,
            5,
            None,
        );

        // Act
        let rows = aggregate(&db, 0, 1000).expect("aggregate");

        // Assert: both non-ok, non-disconnect outcomes count as errors.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].requests, 3);
        assert_eq!(rows[0].errors, 2);
    }

    #[test]
    fn aggregate_client_disconnect_pre_dispatch_counts_model_null_rows_only() {
        // Arrange: two client_disconnect rows -- one pre-dispatch (raw model
        // NULL, disconnected before a provider was ever stamped) and one
        // post-first-content-chunk (model stamped, then the client hung up
        // mid-stream) -- plus one ok row that must not be counted.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, tool_count, msg_count, attempt_count, fallback_count) \
                 VALUES (100, 100, 'pre', 'anthropic', 'asked', 'a', NULL, NULL, NULL, \
                 1, 'client_disconnect', 5, 0, 0, 0, 0)",
                [],
            )
            .expect("insert pre-dispatch disconnect");
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, tool_count, msg_count, attempt_count, fallback_count) \
                 VALUES (110, 110, 'post', 'anthropic', 'asked', 'a', 'm', 'p', 'u', \
                 1, 'client_disconnect', 5, 0, 0, 0, 0)",
                [],
            )
            .expect("insert post-dispatch disconnect");
        insert_row(
            &db,
            "ok1",
            120,
            "m",
            "p",
            "u",
            "a",
            "ok",
            Some(1),
            Some(1),
            5,
            None,
        );

        // Act
        let rows = aggregate(&db, 0, 1000).expect("aggregate");

        // Assert: both disconnects count toward the total; only the
        // NULL-raw-model one counts toward the pre-dispatch subset.
        let total_cd: i64 = rows.iter().map(|r| r.client_disconnect_total).sum();
        let total_pre: i64 = rows.iter().map(|r| r.client_disconnect_pre_dispatch).sum();
        assert_eq!(total_cd, 2);
        assert_eq!(total_pre, 1);
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

    /// Insert a row carrying the M1 attribution columns, or a baseline
    /// (pre-M1) row when `recorder_version` is `None` -- the latter must
    /// never contribute to `m1_attribution_summary` totals even when it
    /// carries a `would_trim_tokens` baseline candidate.
    #[allow(clippy::too_many_arguments)]
    fn insert_m1_attribution_row(
        db: &UsageDb,
        request_id: &str,
        ts_start: i64,
        recorder_version: Option<i64>,
        dedup_tokens: Option<i64>,
        supersession_tokens: Option<i64>,
        path_units: Option<i64>,
        path_extractable: Option<i64>,
        context_fraction: Option<f64>,
    ) {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, stream, outcome, latency_ms, tool_count, \
                 msg_count, attempt_count, fallback_count, would_trim_tokens, \
                 would_trim_recorder_version, would_trim_dedup_tokens, \
                 would_trim_supersession_tokens, would_trim_path_units, \
                 would_trim_path_extractable, would_trim_context_fraction) \
                 VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 0, 'ok', 0, 0, 0, 1, 0, 99999, \
                 ?3, ?4, ?5, ?6, ?7, ?8)",
                rusqlite::params![
                    ts_start,
                    request_id,
                    recorder_version,
                    dedup_tokens,
                    supersession_tokens,
                    path_units,
                    path_extractable,
                    context_fraction,
                ],
            )
            .expect("insert m1 attribution row");
    }

    #[test]
    fn m1_attribution_summary_excludes_baseline_rows_without_recorder_version() {
        // Arrange: two M1-recorded rows (recorder_version = 1) and one
        // baseline row (recorder_version = NULL) that also carries a
        // baseline would_trim_tokens candidate and would incorrectly inflate
        // the M1 totals if the recorder-version filter were dropped.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_m1_attribution_row(
            &db,
            "m1",
            100,
            Some(1),
            Some(500),
            Some(300),
            Some(4),
            Some(3),
            Some(0.10),
        );
        insert_m1_attribution_row(
            &db,
            "m2",
            110,
            Some(1),
            Some(200),
            Some(0),
            Some(2),
            Some(2),
            Some(0.20),
        );
        insert_m1_attribution_row(&db, "baseline", 120, None, None, None, None, None, None);

        // Act
        let s = m1_attribution_summary(&db, 100, 1000).expect("summary");

        // Assert: only the two recorder-version rows contribute.
        assert_eq!(s.recorder_requests, 2);
        assert_eq!(s.dedup_tokens, 700);
        assert_eq!(s.supersession_tokens, 300);
        assert_eq!(s.path_units, 6);
        assert_eq!(s.path_extractable, 5);
        assert_eq!(s.context_fraction_present, 2);
        assert!((s.context_fraction_sum - 0.30).abs() < 1e-9);
    }

    #[test]
    fn m1_attribution_summary_is_zero_when_no_recorder_rows() {
        // Arrange: only a baseline row with no M1 recording.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_m1_attribution_row(&db, "baseline", 100, None, None, None, None, None, None);

        // Act + Assert
        let s = m1_attribution_summary(&db, 0, 1000).expect("summary");
        assert_eq!(s.recorder_requests, 0);
        assert_eq!(s.dedup_tokens, 0);
        assert_eq!(s.supersession_tokens, 0);
        assert_eq!(s.path_units, 0);
        assert_eq!(s.path_extractable, 0);
        assert_eq!(s.context_fraction_present, 0);
        assert_eq!(s.context_fraction_sum, 0.0);
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
    /// `session_id`, `provider_kind`, `model`, `cache_read`, and an explicit
    /// `outcome` so the admission-contract filter is exercisable.
    #[allow(clippy::too_many_arguments)]
    fn insert_reuse_row(
        db: &UsageDb,
        request_id: &str,
        ts_start: i64,
        session_id: Option<&str>,
        provider_kind: Option<&str>,
        model: Option<&str>,
        cache_read: Option<i64>,
        outcome: &str,
    ) {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider_kind, session_id, stream, outcome, \
                 latency_ms, tool_count, msg_count, attempt_count, fallback_count, cache_read) \
                 VALUES (?1, ?1, ?2, 'anthropic', 'req-model', 'al', ?3, ?4, ?5, 1, ?6, \
                 5, 0, 0, 1, 0, ?7)",
                rusqlite::params![
                    ts_start,
                    request_id,
                    model,
                    provider_kind,
                    session_id,
                    outcome,
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
            "ok",
        );
        insert_reuse_row(
            &db,
            "a1",
            100,
            Some("s1"),
            Some("anthropic-api"),
            Some("opus"),
            None,
            "ok",
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
            "ok",
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
            "ok",
        );
        // NULL provider_kind -> filtered out.
        insert_reuse_row(
            &db,
            "n-pk",
            130,
            Some("s2"),
            None,
            Some("opus"),
            Some(9),
            "ok",
        );
        // NULL model -> filtered out.
        insert_reuse_row(
            &db,
            "n-model",
            140,
            Some("s2"),
            Some("anthropic-api"),
            None,
            Some(9),
            "ok",
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
            "ok",
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
                "ok",
            );
        }

        // Act: cap at 3.
        let rows = read_reuse_samples_since(db.conn(), 0, 3).expect("read");

        // Assert: the three OLDEST (ascending order, then LIMIT).
        let ids: Vec<i64> = rows.iter().map(|r| r.ts_start_ms).collect();
        assert_eq!(ids, vec![100, 101, 102]);
    }

    #[test]
    fn read_reuse_samples_excludes_non_ok_outcome() {
        // Arrange: a mid-stream-failed row (upstream_error) that still
        // carries a full triple and a non-null cache_read -- the divergence
        // case where the live path never records it (record_k_sample only
        // fires on the success finalize / natural stream EOS) but a
        // filter-less rebuild would replay it after a restart. An ok row in
        // the same window must still be admitted.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_reuse_row(
            &db,
            "failed",
            100,
            Some("s1"),
            Some("anthropic-api"),
            Some("opus"),
            Some(42),
            "upstream_error",
        );
        insert_reuse_row(
            &db,
            "succeeded",
            110,
            Some("s1"),
            Some("anthropic-api"),
            Some("opus"),
            Some(7),
            "ok",
        );

        // Act
        let rows = read_reuse_samples_since(db.conn(), 0, 100).expect("read");

        // Assert: only the ok row survives.
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ts_start_ms, 110);
        assert_eq!(rows[0].cache_read, 7);
    }

    /// Insert a calibration row: an optional `would_trim_k_floor` (None ->
    /// uncalibrated, still counts as future reuse) plus the (session_id,
    /// provider_kind, model) triple and a `cache_read` snapshot. `ts_start`
    /// drives the remaining-future ordering.
    #[allow(clippy::too_many_arguments)]
    fn insert_calib_row(
        db: &UsageDb,
        request_id: &str,
        ts_start: i64,
        session_id: &str,
        provider_kind: &str,
        model: &str,
        k_floor: Option<f64>,
        cache_read: i64,
    ) {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, provider_kind, session_id, \
                 stream, outcome, latency_ms, tool_count, msg_count, attempt_count, \
                 fallback_count, would_trim_k_floor, cache_read) \
                 VALUES (?1, ?1, ?2, 'anthropic', 'req-model', 'al', ?3, 'paid', ?4, ?5, \
                 1, 'ok', 5, 0, 0, 1, 0, ?6, ?7)",
                rusqlite::params![
                    ts_start,
                    request_id,
                    model,
                    provider_kind,
                    session_id,
                    k_floor,
                    cache_read,
                ],
            )
            .expect("insert calib row");
    }

    #[test]
    fn k_calibration_coverage_uses_remaining_future_not_whole_session() {
        // Arrange: ONE session whose reuse is concentrated EARLY. Under the
        // old whole-session comparison every calibrated row would see the
        // group's total of 2 hits and all three would be "covered". Under the
        // remaining-future comparison a LATE over-prediction is correctly a
        // miss, because no reuse remains after it.
        //   r1 ts=100 hit,  floor=1.0  -> 1 future hit (r2)  -> covered (1>=1)
        //   r2 ts=200 hit,  UNCALIBRATED (feeds future reuse, not population)
        //   r3 ts=300 miss, floor=2.0  -> 0 future hits      -> MISS (0<2)
        //   r4 ts=400 miss, floor=0.5  -> 0 future hits      -> MISS (0<0.5)
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_calib_row(&db, "r1", 100, "s1", "anth", "m1", Some(1.0), 5);
        insert_calib_row(&db, "r2", 200, "s1", "anth", "m1", None, 5);
        insert_calib_row(&db, "r3", 300, "s1", "anth", "m1", Some(2.0), 0);
        insert_calib_row(&db, "r4", 400, "s1", "anth", "m1", Some(0.5), 0);

        // Act
        let cal = k_calibration_summary(&db).expect("summary");

        // Assert: population is the 3 calibrated rows; remaining-future
        // coverage is 1/3 (whole-session would have been 3/3).
        assert_eq!(cal.n, 3, "only the calibrated rows form the population");
        assert!(
            (cal.coverage - 1.0 / 3.0).abs() < 1e-9,
            "remaining-future coverage must be 1/3, got {}",
            cal.coverage
        );
        // Per-row normalized errors: |1-1|/2=0, |2-0|/1=2, |0.5-0|/1=0.5;
        // sorted [0, 0.5, 2] -> median 0.5.
        assert!(
            (cal.accuracy - 0.5).abs() < 1e-9,
            "per-row-normalized median accuracy must be 0.5, got {}",
            cal.accuracy
        );
    }

    #[test]
    fn k_calibration_hazard_decay_is_negative_for_decaying_session() {
        // Arrange: a 4-turn session whose reuse decays -- both first-half
        // turns reused, neither second-half turn did. first_rate=1.0,
        // second_rate=0.0 -> delta = -1.0. All rows calibrated so n>0 and the
        // main path computes hazard_decay.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_calib_row(&db, "d0", 100, "sd", "anth", "m1", Some(1.0), 5);
        insert_calib_row(&db, "d1", 200, "sd", "anth", "m1", Some(1.0), 5);
        insert_calib_row(&db, "d2", 300, "sd", "anth", "m1", Some(1.0), 0);
        insert_calib_row(&db, "d3", 400, "sd", "anth", "m1", Some(1.0), 0);

        // Act
        let cal = k_calibration_summary(&db).expect("summary");

        // Assert: a material negative decay -- the age-conditioning trigger.
        assert!(
            (cal.hazard_decay + 1.0).abs() < 1e-9,
            "decaying session must yield hazard_decay = -1.0, got {}",
            cal.hazard_decay
        );
    }

    #[test]
    fn k_calibration_hazard_decay_is_zero_for_flat_session() {
        // Arrange: a 4-turn session with a CONSTANT (flat) reuse rate -- every
        // turn reused. Both halves rate 1.0 -> delta 0.0.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        for (i, ts) in [100, 200, 300, 400].into_iter().enumerate() {
            insert_calib_row(&db, &format!("f{i}"), ts, "sf", "anth", "m1", Some(1.0), 5);
        }

        // Act
        let cal = k_calibration_summary(&db).expect("summary");

        // Assert
        assert_eq!(cal.hazard_decay, 0.0, "flat reuse -> zero decay");
    }

    #[test]
    fn k_calibration_hazard_decay_is_zero_when_no_group_has_enough_rows() {
        // Arrange: a session with fewer than HAZARD_DECAY_MIN_GROUP_ROWS rows
        // -- no group qualifies, so the halves would be too noisy to inform
        // the age-conditioning decision.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");
        insert_calib_row(&db, "g0", 100, "sg", "anth", "m1", Some(1.0), 5);
        insert_calib_row(&db, "g1", 200, "sg", "anth", "m1", Some(1.0), 0);
        insert_calib_row(&db, "g2", 300, "sg", "anth", "m1", Some(1.0), 5);

        // Act
        let cal = k_calibration_summary(&db).expect("summary");

        // Assert: no qualifying group -> hazard_decay defaults to 0.0.
        assert_eq!(cal.hazard_decay, 0.0);
    }

    #[test]
    fn k_calibration_empty_db_is_all_zero_including_hazard_decay() {
        // Arrange: no rows at all.
        let (_dir, path) = temp_db_path();
        let db = open(&path).expect("open");

        // Act
        let cal = k_calibration_summary(&db).expect("summary");

        // Assert: the n==0 early return zeroes every field.
        assert_eq!(cal.n, 0);
        assert_eq!(cal.coverage, 0.0);
        assert_eq!(cal.accuracy, 0.0);
        assert_eq!(cal.hazard_decay, 0.0);
    }
}
