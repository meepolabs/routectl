//! Lossless-minifier (context-reduction) outcome read queries.

use crate::db::UsageDb;

use super::QueryError;

/// Bytes per estimated token. Mirrors `routectl_core::context_reduction`'s
/// `BYTES_PER_TOKEN_ESTIMATE`; duplicated rather than imported because this
/// crate is a leaf and must not depend on the reducer.
const BYTES_PER_TOKEN_ESTIMATE: i64 = 4;

/// Windowed lossless-minifier outcome summary: the per-decision request
/// histogram plus the summed raw effect counters over the window. Restricted
/// to rows where `reduction_decision IS NOT NULL` -- rows written before the
/// column existed, and rows that never dispatched a target, carry no outcome
/// and must not dilute the histogram.
///
/// `est_tokens_saved` is NOT a field: the token estimate is derived from
/// `bytes_saved` at read time (see [`ReductionSummary::est_tokens_saved`]) so
/// `reduction_bytes_saved` stays the single source of truth.
///
/// VALIDITY: ratios and cost claims drawn from this summary hold only for
/// windows in which the usage channel dropped nothing -- the service-side
/// `dropped_full` / `dropped_disabled` / `write_errors` counters must be flat
/// across the window. A lossy window under-reports every counter here by an
/// unknown amount, so it supports counts-seen statements only.
///
/// Plain data; the caller decides how to display it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReductionSummary {
    /// Requests in the window carrying a minifier outcome
    /// (`reduction_decision IS NOT NULL`). The sum of `decisions`' counts.
    pub decided_requests: i64,
    /// Request count per outcome token, ascending by token. Read as an OPEN
    /// vocabulary rather than matched against a fixed set: the token set
    /// (`applied`, `skipped:disabled`, `skipped:no-tail`,
    /// `skipped:nothing-to-strip`, `skipped:unknown`) is documented
    /// additive-forever, so a token this build has never seen still reports.
    pub decisions: Vec<(String, i64)>,
    /// Summed strings the minifier actually rewrote.
    pub strings_compressed: i64,
    /// Summed candidate strings left untouched (non-JSON or already compact) --
    /// a permanent ceiling, not headroom.
    pub strings_skipped: i64,
    /// Summed candidate strings that parsed as JSON but whose re-parse
    /// equality guard declined.
    ///
    /// With the current minifier this count is structurally unreachable: every
    /// value the lexer rewrites re-parses equal. A NONZERO value therefore
    /// signals a minifier DEFECT (the guard caught a rewrite that changed
    /// meaning) -- it is not ordinary traffic headroom, and must be read as a
    /// bug report rather than as evidence for a more capable transform.
    pub strings_rejected: i64,
    /// Summed exact bytes removed from prepared outbound payloads. NOT billed
    /// tokens.
    pub bytes_saved: i64,
}

impl ReductionSummary {
    /// Token estimate for `bytes_saved`, derived at read time as
    /// `bytes_saved / 4` and deliberately never persisted.
    pub const fn est_tokens_saved(&self) -> i64 {
        self.bytes_saved / BYTES_PER_TOKEN_ESTIMATE
    }
}

/// One `GROUP BY reduction_decision` row: the token, its request count, and
/// the four raw counters summed within it. The window totals are folded from
/// these in Rust so the histogram and the totals cannot disagree.
const REDUCTION_SQL: &str = "\
SELECT
    reduction_decision                                        AS decision,
    COUNT(*)                                                  AS requests,
    COALESCE(SUM(reduction_strings_compressed), 0)             AS strings_compressed,
    COALESCE(SUM(reduction_strings_skipped), 0)                AS strings_skipped,
    COALESCE(SUM(reduction_strings_rejected), 0)               AS strings_rejected,
    COALESCE(SUM(reduction_bytes_saved), 0)                    AS bytes_saved
FROM requests
WHERE ts_start >= ?1 AND ts_start < ?2
  AND reduction_decision IS NOT NULL
GROUP BY reduction_decision
ORDER BY reduction_decision";

/// The window's lossless-minifier outcome summary. Rows with a NULL
/// `reduction_decision` (pre-column history, and requests that dispatched no
/// target) are excluded, so `decided_requests` is the population the counters
/// describe rather than the window's request count. A NULL counter inside a
/// decided row contributes 0. All fields are zero / empty when no row in the
/// window carried an outcome.
pub fn reduction_summary(
    db: &UsageDb,
    from_ms: i64,
    to_ms: i64,
) -> Result<ReductionSummary, QueryError> {
    let mut stmt = db.conn().prepare(REDUCTION_SQL)?;
    let rows = stmt
        .query_map([from_ms, to_ms], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    let mut summary = ReductionSummary::default();
    for (decision, requests, compressed, skipped, rejected, bytes) in rows {
        summary.decided_requests += requests;
        summary.strings_compressed += compressed;
        summary.strings_skipped += skipped;
        summary.strings_rejected += rejected;
        summary.bytes_saved += bytes;
        summary.decisions.push((decision, requests));
    }
    Ok(summary)
}
