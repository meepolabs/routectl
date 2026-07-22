//! Aggregate + breakdown queries.

use rusqlite::Row;

use crate::db::UsageDb;

use super::{AggRow, GroupKey, QueryError};

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

/// A SECOND flat query beside [`AGG_SQL`], deliberately NOT folded into it:
/// the aggregate stays one row per group, while this breakdown fans a group
/// into one row per failure class. It shares `AGG_SQL`'s window predicate and
/// group-key expressions and adds the SAME `errors` filter (`outcome NOT IN
/// ('ok', 'client_disconnect')`), so the per-group class counts sum EXACTLY to
/// that group's `AggRow::errors`. NULL `resolved_class` (pre-dispatch or
/// never-classified rows) buckets under `unclassified` via the COALESCE; a
/// forward-compat token written by a newer binary passes through verbatim as
/// its own class rather than being folded into `unclassified`.
pub(super) const ERRORS_BY_CLASS_SQL: &str = "\
SELECT
    COALESCE(model, requested_model) AS model, provider, upstream, alias,
    COALESCE(resolved_class, 'unclassified')            AS class,
    COUNT(*)                                            AS count
FROM requests
WHERE ts_start >= ?1 AND ts_start < ?2
  AND outcome NOT IN ('ok', 'client_disconnect')
GROUP BY COALESCE(model, requested_model), provider, upstream, alias,
         COALESCE(resolved_class, 'unclassified')";

/// Windowed per-group error breakdown by resolved failure class. Returns one
/// `(group key, class, count)` triple per `(group, class)` pair; the caller
/// merges these into a per-group `class -> count` map (`GroupKey` derives
/// `Hash` for that merge). By construction the counts sum to `AggRow::errors`
/// per group and at totals (identical predicate). Rows outside `[from_ms,
/// to_ms)` are excluded.
pub fn errors_by_class(
    db: &UsageDb,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<(GroupKey, String, i64)>, QueryError> {
    let mut stmt = db.conn().prepare(ERRORS_BY_CLASS_SQL)?;
    let rows = stmt
        .query_map([from_ms, to_ms], |row| {
            let key = GroupKey {
                model: row.get(0)?,
                provider: row.get(1)?,
                upstream: row.get(2)?,
                alias: row.get(3)?,
            };
            Ok((key, row.get::<_, String>(4)?, row.get::<_, i64>(5)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
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
