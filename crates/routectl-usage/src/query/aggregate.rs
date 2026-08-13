//! Aggregate + breakdown queries.

use rusqlite::Row;

use crate::db::UsageDb;

use super::{AggRow, GroupKey, QueryError};

/// The most recent quota-bearing snapshot for one seat. Mirrors the `quota_*`
/// columns the daemon stamps on a row when an upstream reports quota data.
///
/// The `quota_*` columns are SHARED across vendors, so `provider_kind` is the
/// discriminator a consumer MUST read to interpret them: `utilization` is a
/// fraction of a PER-PROVIDER window (Anthropic's rolling 5h vs codex's
/// weekly), and a codex row populates `claim` / `utilization` / `reset` only,
/// leaving `status` and both overage fields `None`.
#[derive(Debug, Clone)]
pub struct QuotaSnapshot {
    /// Credential identity this snapshot belongs to (`seat_key(provider,
    /// label)`, e.g. `codex`, `anthropic#a`). `None` for rows written before
    /// the seat column was populated, and for forwarded client credentials.
    pub seat: Option<String>,
    /// Provider kind of the row this snapshot came from -- the cross-vendor
    /// discriminator for the shared `quota_*` columns. `None` when absent.
    pub provider_kind: Option<String>,
    /// Start time of the row this snapshot came from, epoch-millis UTC.
    pub ts_start: i64,
    /// Quota claim token. `None` when absent.
    pub claim: Option<String>,
    /// Quota status. `None` when absent.
    pub status: Option<String>,
    /// Overage-quota status. `None` when absent.
    pub overage_status: Option<String>,
    /// Primary-quota utilization ratio. `None` when absent.
    pub utilization: Option<f64>,
    /// Overage-quota utilization ratio. `None` when absent.
    pub overage_utilization: Option<f64>,
    /// Quota reset time, epoch-SECONDS UTC (stored verbatim as the upstream
    /// reports it). `None` when absent.
    pub reset: Option<i64>,
}

/// The group-key + base aggregate columns, shared VERBATIM by [`AGG_SQL`] and
/// [`QUERY_AGG_SQL`].
///
/// This is a macro expanding to a string literal (rather than a `const`) so
/// both statements can be assembled with `concat!` at compile time: the two
/// queries MUST agree on the fine grain and on every base column, because the
/// same `map_agg_row` mapper reads both by ordinal position. Duplicating the
/// column list as two literals would let one drift silently past the other.
///
/// `provider_kind` is a group-key column, not a display dimension: reasoning
/// tokens are DISJOINT from output for some kinds and subsumed for others, so a
/// row's cost depends on the kind PERSISTED with it. Grouping on it keeps each
/// era of a renamed-kind provider in its own partition, so each is priced by
/// what it actually was. Callers that report at the documented external grain
/// coalesce these partitions back together after pricing.
macro_rules! agg_base_columns {
    () => {
        "    COALESCE(model, requested_model) AS model, provider, upstream, alias,
    provider_kind,
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
        THEN 1 ELSE 0 END)                              AS client_disconnect_pre_dispatch"
    };
}

/// The fine grain both aggregate statements group at. Shared so the two can
/// never drift apart.
macro_rules! agg_group_by {
    () => {
        "GROUP BY COALESCE(model, requested_model), provider, upstream, alias,
         provider_kind"
    };
}

const AGG_SQL: &str = concat!(
    "SELECT\n",
    agg_base_columns!(),
    "\nFROM requests AS r
WHERE ts_start >= ?1 AND ts_start < ?2\n",
    agg_group_by!()
);

/// Row-eligibility predicate for every time-to-first-token figure: a streaming
/// success that recorded a first-byte time. Shared by the p50 numerator /
/// denominator AND the p95 MAX so the two can never describe different
/// populations -- a mid-stream failure stamps a `ttfb_ms` too, and counting it
/// in one but not the other lets p50 exceed p95.
macro_rules! ttft_eligible {
    () => {
        "stream = 1 AND outcome = 'ok' AND ttfb_ms IS NOT NULL"
    };
}

/// Row-eligibility predicate for the request-weighted throughput estimate:
/// a streaming success with a usable TTFB, a STRICTLY positive generation
/// window, and a reported output-token count. The `>` is strict on purpose --
/// a zero-length window would divide by zero.
macro_rules! tok_s_eligible {
    () => {
        "stream = 1 AND outcome = 'ok' AND ttfb_ms IS NOT NULL
            AND latency_ms > ttfb_ms AND output_tokens IS NOT NULL"
    };
}

/// The cache-INCLUSIVE prompt total for one row: the disjoint prompt
/// dimensions summed, NULL counters contributing 0. Denominator of the
/// per-row cache-hit fraction.
macro_rules! cache_prompt_total {
    () => {
        "(COALESCE(input_tokens, 0) + cache_read
            + COALESCE(cache_write_5m, 0) + COALESCE(cache_write_1h, 0))"
    };
}

/// The `/status/query` SELECT clause: [`AGG_SQL`]'s base columns at the SAME
/// fine grain, plus the thirteen expressions the derived display metrics need.
///
/// A macro rather than a `const` for the same reason [`agg_base_columns`] is:
/// both [`QUERY_AGG_SQL`] and [`SERIES_AGG_SQL`] assemble it with `concat!` at
/// compile time, so the two can never present different columns to the shared
/// [`map_fine_row`] ordinals.
///
/// Every zero-row-able SUM is COALESCEd (an empty group set returns NO rows,
/// but a group whose rows are all NULL in a column returns SQL NULL, which
/// would fail an `i64` column read); every MAX is read as `Option`, since a MAX
/// over no non-NULL values is legitimately absent.
macro_rules! query_agg_select {
    () => {
        concat!(
            "SELECT\n",
            agg_base_columns!(),
            ",
    MAX(CASE WHEN ",
            ttft_eligible!(),
            " THEN ttfb_ms END)                                 AS ttfb_max,
    COALESCE(SUM(CASE WHEN ",
            ttft_eligible!(),
            " THEN ttfb_ms ELSE 0 END), 0)                      AS ttft_sum,
    COALESCE(SUM(CASE WHEN ",
            ttft_eligible!(),
            " THEN 1 ELSE 0 END), 0)                            AS ttft_count,
    COALESCE(SUM(latency_ms), 0)                        AS latency_sum,
    MAX(latency_ms)                                     AS latency_max,
    MAX(input_tokens)                                   AS input_tokens_max,
    COUNT(input_tokens)                                 AS input_tokens_present,
    COALESCE(SUM(CASE WHEN ",
            tok_s_eligible!(),
            "
        THEN CAST(output_tokens AS REAL) * 1000.0
             / CAST(latency_ms - ttfb_ms AS REAL)
        ELSE 0.0 END), 0.0)                             AS tok_s_sum,
    COALESCE(SUM(CASE WHEN ",
            tok_s_eligible!(),
            "
        THEN 1 ELSE 0 END), 0)                          AS tok_s_count,
    COALESCE(SUM(CASE WHEN cache_read IS NOT NULL AND ",
            cache_prompt_total!(),
            " > 0
        THEN CAST(cache_read AS REAL) / CAST(",
            cache_prompt_total!(),
            " AS REAL)
        ELSE 0.0 END), 0.0)                             AS cache_hit_sum,
    COALESCE(SUM(CASE WHEN cache_read IS NOT NULL AND ",
            cache_prompt_total!(),
            " > 0
        THEN 1 ELSE 0 END), 0)                          AS cache_hit_count,
    COALESCE(SUM(CASE WHEN fallback_count > 0
        THEN 1 ELSE 0 END), 0)                          AS fallback_served"
        )
    };
}

/// The window predicate and the two optional filters, as BIND PARAMS (never
/// interpolated identifiers). Shared by both query statements so a filter can
/// never apply to one and not the other.
macro_rules! query_agg_from_where {
    () => {
        "
FROM requests AS r
WHERE ts_start >= ?1 AND ts_start < ?2
  AND (?3 IS NULL OR alias = ?3)
  AND (?4 IS NULL OR provider = ?4)\n"
    };
}

/// The `/status/query` aggregate.
///
/// A SEPARATE statement rather than more columns on `AGG_SQL`: the existing
/// panel binds exactly two params and maps exactly the base columns, so adding
/// the filter placeholders and the extra columns there would change every
/// existing caller's call shape for no benefit. The base column list and the
/// GROUP BY are shared verbatim through macros, so the fine grain and the
/// `map_agg_row` ordinals stay identical by construction.
pub(super) const QUERY_AGG_SQL: &str = concat!(
    query_agg_select!(),
    query_agg_from_where!(),
    agg_group_by!()
);

/// [`QUERY_AGG_SQL`]'s statement at the SAME fine grain, plus a bucket index as
/// one further GROUP BY dimension, so one scan feeds both the coarse groups and
/// the time series.
///
/// `?5` is the bucket WIDTH in milliseconds and `?1` doubles as the bucket
/// ANCHOR. The numerator `ts_start - ?1` is never negative, because the same
/// `?1` is the window's inclusive lower bound in the WHERE. A zero `?5` would
/// make every bucket index a silent SQL NULL rather than an error, which is why
/// the caller's `width_ms > 0` invariant is re-checked before this statement
/// runs.
pub(super) const SERIES_AGG_SQL: &str = concat!(
    query_agg_select!(),
    ",\n    (ts_start - ?1) / ?5                                AS bucket",
    query_agg_from_where!(),
    agg_group_by!(),
    ", bucket"
);

/// One [`QUERY_AGG_SQL`] row: the shared base aggregate plus the raw
/// numerators / denominators / maxima the fold turns into display metrics.
/// Deliberately NOT folded into `AggRow`: that struct is the existing panel's
/// contract and gains nothing from these columns.
pub(super) struct FineRow {
    /// The base aggregate, identical in shape to what [`aggregate`] returns.
    pub agg: AggRow,
    /// `MAX(ttfb_ms)` over streaming successes with a TTFB; `None` when the
    /// group has no such row.
    pub ttfb_max: Option<i64>,
    /// Summed `ttfb_ms` over TTFT-eligible rows -- the p50 numerator. Filtered
    /// to the SAME population as `ttfb_max`, unlike `AggRow::sum_ttfb_ms`.
    pub ttft_sum: i64,
    /// Count of TTFT-eligible rows (the `ttft_sum` divisor).
    pub ttft_count: i64,
    /// Summed end-to-end latency, milliseconds.
    pub latency_sum: i64,
    /// `MAX(latency_ms)`. `latency_ms` is NOT NULL in the schema, so this is
    /// `None` only for a group with no rows -- which cannot occur in a grouped
    /// result -- but it is still read as `Option` rather than assumed.
    pub latency_max: Option<i64>,
    /// `MAX(input_tokens)` -- the group's context high-water mark. `None` when
    /// no row reported an input-token count.
    pub input_tokens_max: Option<i64>,
    /// Rows that REPORTED an input-token count (`COUNT` ignores NULL). The
    /// mean-context denominator.
    pub input_tokens_present: i64,
    /// Summed per-request tokens/second over throughput-eligible rows.
    pub tok_s_sum: f64,
    /// Count of throughput-eligible rows (the `tok_s_sum` divisor).
    pub tok_s_count: i64,
    /// Summed per-request cache-hit FRACTION over cache-reporting rows.
    pub cache_hit_sum: f64,
    /// Count of cache-reporting rows with a positive prompt total (the
    /// `cache_hit_sum` divisor).
    pub cache_hit_count: i64,
    /// Rows served only after a fallback (`fallback_count > 0`). Additive, not
    /// a ratio: `fallback_count` is NOT NULL in the schema, so this is a plain
    /// count of rows where the router had to try more than its first choice.
    pub fallback_served: i64,
}

/// Windowed aggregate grouped by
/// `(model, provider, upstream, alias, provider_kind)`. Rows outside
/// `[from_ms, to_ms)` are excluded. The caller prices each row from its
/// PERSISTED `provider_kind` and then rolls the partitions up for display.
pub fn aggregate(db: &UsageDb, from_ms: i64, to_ms: i64) -> Result<Vec<AggRow>, QueryError> {
    let mut stmt = db.conn().prepare(AGG_SQL)?;
    let rows = stmt
        .query_map([from_ms, to_ms], map_agg_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Build a [`FineRow`] from a [`QUERY_AGG_SQL`] result row: the base columns
/// through the shared mapper, then the twelve extra columns by ordinal.
pub(super) fn map_fine_row(row: &Row) -> rusqlite::Result<FineRow> {
    Ok(FineRow {
        agg: map_agg_row(row)?,
        ttfb_max: row.get(29)?,
        ttft_sum: row.get(30)?,
        ttft_count: row.get(31)?,
        latency_sum: row.get(32)?,
        latency_max: row.get(33)?,
        input_tokens_max: row.get(34)?,
        input_tokens_present: row.get(35)?,
        tok_s_sum: row.get(36)?,
        tok_s_count: row.get(37)?,
        cache_hit_sum: row.get(38)?,
        cache_hit_count: row.get(39)?,
        fallback_served: row.get(40)?,
    })
}

/// Build a [`FineRow`] plus its bucket index from a [`SERIES_AGG_SQL`] result
/// row. That statement is [`QUERY_AGG_SQL`] with ONE trailing column appended,
/// so the shared mapper reads every column it already knows and only the bucket
/// index is read here.
pub(super) fn map_fine_row_bucketed(row: &Row) -> rusqlite::Result<(FineRow, i64)> {
    let fine = map_fine_row(row)?;
    let bucket = row.get(41)?;
    Ok((fine, bucket))
}

const EARLIEST_TS_START_SQL: &str = "SELECT MIN(ts_start) FROM requests WHERE ts_start >= ?1";

/// The `ts_start` of the oldest row at or after `from_ms`, or `None` when no row
/// qualifies. A `MIN` over a range of an indexed column, so this is an index seek
/// rather than a scan.
///
/// The caller uses it to anchor an unbounded window: bucketing an all-history
/// window from the epoch would emit tens of thousands of empty leading buckets.
/// The lower bound matters even for that window: `from_ms` is the same inclusive
/// bound the aggregate applies, so an anchor derived from this can never pull in
/// a row the unbucketed query excludes.
pub fn earliest_ts_start(db: &UsageDb, from_ms: i64) -> Result<Option<i64>, QueryError> {
    let mut stmt = db.conn().prepare(EARLIEST_TS_START_SQL)?;
    // `MIN` over zero rows is a single SQL NULL row, not an absent row, so the
    // empty ledger arrives as `Ok(None)` rather than `QueryReturnedNoRows`.
    let earliest = stmt.query_row([from_ms], |row| row.get::<_, Option<i64>>(0))?;
    Ok(earliest)
}

/// Build an `AggRow` from a result row. The column order matches `AGG_SQL`.
fn map_agg_row(row: &Row) -> rusqlite::Result<AggRow> {
    Ok(AggRow {
        key: GroupKey {
            model: row.get(0)?,
            provider: row.get(1)?,
            upstream: row.get(2)?,
            alias: row.get(3)?,
            provider_kind: row.get(4)?,
        },
        requests: row.get(5)?,
        ok: row.get(6)?,
        errors: row.get(7)?,
        input_tokens: row.get(8)?,
        output_tokens: row.get(9)?,
        reasoning_tokens: row.get(10)?,
        cache_read_peak: row.get(11)?,
        cache_read_avg: row.get(12)?,
        cache_read_billed: row.get(13)?,
        cache_write_5m: row.get(14)?,
        cache_write_1h: row.get(15)?,
        server_tool_calls: row.get(16)?,
        sum_ttfb_ms: row.get(17)?,
        ttfb_count: row.get(18)?,
        gen_window_ms: row.get(19)?,
        gen_output_tokens: row.get(20)?,
        reasoning_present: row.get(21)?,
        cache_read_present: row.get(22)?,
        cache_write_5m_present: row.get(23)?,
        cache_write_1h_present: row.get(24)?,
        server_tool_present: row.get(25)?,
        stream_count: row.get(26)?,
        client_disconnect_total: row.get(27)?,
        client_disconnect_pre_dispatch: row.get(28)?,
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
    provider_kind,
    COALESCE(resolved_class, 'unclassified')            AS class,
    COUNT(*)                                            AS count
FROM requests
WHERE ts_start >= ?1 AND ts_start < ?2
  AND outcome NOT IN ('ok', 'client_disconnect')
GROUP BY COALESCE(model, requested_model), provider, upstream, alias,
         provider_kind,
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
                provider_kind: row.get(4)?,
            };
            Ok((key, row.get::<_, String>(5)?, row.get::<_, i64>(6)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

const LATEST_QUOTA_BY_SEAT_SQL: &str = "\
SELECT seat, provider_kind, ts_start, quota_claim, quota_status,
       quota_overage_status, quota_utilization, quota_overage_utilization,
       quota_reset
FROM (
    SELECT seat, provider_kind, ts_start, quota_claim, quota_status,
           quota_overage_status, quota_utilization, quota_overage_utilization,
           quota_reset,
           ROW_NUMBER() OVER (
               PARTITION BY seat ORDER BY ts_start DESC, rowid DESC) AS rn
    FROM requests
    WHERE quota_status IS NOT NULL OR quota_utilization IS NOT NULL
)
WHERE rn = 1
ORDER BY seat";

/// The most recent quota-bearing row PER SEAT -- one snapshot per credential
/// identity, newest first within each seat (`ts_start DESC, rowid DESC`).
/// Empty when no row carries quota data.
///
/// Eligibility is `quota_status IS NOT NULL OR quota_utilization IS NOT NULL`:
/// not every vendor reports a status token (codex reports utilization only), so
/// a status-only predicate would make those rows invisible. Rows whose `seat`
/// is NULL are NOT filtered out -- they form their own bucket, so pre-seat
/// history stays visible rather than being dropped or given a synthetic seat.
///
/// Not windowed: a quota snapshot is the latest known state of a seat,
/// independent of any report window.
pub fn latest_quota_by_seat(db: &UsageDb) -> Result<Vec<QuotaSnapshot>, QueryError> {
    let mut stmt = db.conn().prepare(LATEST_QUOTA_BY_SEAT_SQL)?;
    let snapshots = stmt
        .query_map([], |row| {
            Ok(QuotaSnapshot {
                seat: row.get(0)?,
                provider_kind: row.get(1)?,
                ts_start: row.get(2)?,
                claim: row.get(3)?,
                status: row.get(4)?,
                overage_status: row.get(5)?,
                utilization: row.get(6)?,
                overage_utilization: row.get(7)?,
                reset: row.get(8)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(snapshots)
}

const TTFBS_SQL: &str = "\
SELECT model, provider, upstream, alias, provider_kind, ttfb_ms
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
                provider_kind: row.get(4)?,
            };
            Ok((key, row.get::<_, i64>(5)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}
