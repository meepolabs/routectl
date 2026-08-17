//! Cache-breakpoint decision read queries (v16 columns).

use crate::db::UsageDb;
use crate::record::{PREFIX_EPOCH_RESEEDED, PREFIX_EPOCH_REWRITTEN, PREFIX_EPOCH_STABLE};

use super::QueryError;
use super::session_ref::session_ref;

/// Windowed cache-breakpoint decision summary: how often each placement region
/// carried a decision at all, how often that decision was an actual injection
/// (`auto_emitted`), and the prefix-epoch event breakdown.
///
/// The per-region counts are deliberately a decided/emitted PAIR rather than a
/// pre-computed rate: rows written before the columns existed carry NULL and
/// must not be counted as declines, so the denominator has to travel with the
/// numerator. The remaining tokens (`caller_supplied`, `volatile_vetoed`, every
/// `auto_skipped:*` reason) are not partitioned here -- the aggregate is a
/// monitor, and the reason vocabulary lives in the `cache_auto_decision` log
/// where it is not summed over a window.
///
/// Plain data; the caller decides how to display it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheDecisionSummary {
    /// Requests carrying a front-region decision (`cache_front_decision IS NOT
    /// NULL`).
    pub front_decided: i64,
    /// Subset of `front_decided` where routectl actually injected a front
    /// breakpoint (`auto_emitted`).
    pub front_emitted: i64,
    /// Requests carrying a terminal-region decision
    /// (`cache_terminal_decision IS NOT NULL`).
    pub terminal_decided: i64,
    /// Subset of `terminal_decided` where routectl actually injected a terminal
    /// breakpoint (`auto_emitted`).
    pub terminal_emitted: i64,
    /// Requests carrying a prefix-epoch classification
    /// (`prefix_epoch_event IS NOT NULL`). NULL -- and so excluded here --
    /// whenever there was no comparable prior prefix to classify against: no
    /// session key, the session's first turn, the first turn after a process
    /// restart, or the first turn after the session was evicted from the
    /// detector's bounded store.
    pub epoch_classified: i64,
    /// Subset of `epoch_classified` whose prefix was byte-identical to the
    /// prior turn's.
    pub epoch_stable: i64,
    /// Subset of `epoch_classified` whose prefix shifted turn-to-turn -- the
    /// cache-invalidating case.
    pub epoch_rewritten: i64,
    /// Subset of `epoch_classified` where a new epoch was seeded.
    pub epoch_reseeded: i64,
}

/// The `auto_emitted` token: the one decision in the `cache_auto_decision`
/// vocabulary that means a breakpoint was actually injected.
const EMITTED_TOKEN: &str = "auto_emitted";

/// `COUNT(col)` ignores NULLs, so each `*_decided` count is the number of rows
/// that carried a decision at all -- pre-v16 rows are excluded rather than
/// counted as declines. The emitted token and the three epoch values are BOUND
/// (`?3..?6`) rather than inlined, so a renumbered `PREFIX_EPOCH_*` constant or
/// a renamed token cannot silently desync from this query.
const CACHE_DECISION_SQL: &str = "\
SELECT
    COUNT(cache_front_decision)                                          AS front_decided,
    COALESCE(SUM(CASE WHEN cache_front_decision = ?3 THEN 1 ELSE 0 END), 0)
                                                                         AS front_emitted,
    COUNT(cache_terminal_decision)                                       AS terminal_decided,
    COALESCE(SUM(CASE WHEN cache_terminal_decision = ?3 THEN 1 ELSE 0 END), 0)
                                                                         AS terminal_emitted,
    COUNT(prefix_epoch_event)                                            AS epoch_classified,
    COALESCE(SUM(CASE WHEN prefix_epoch_event = ?4 THEN 1 ELSE 0 END), 0) AS epoch_stable,
    COALESCE(SUM(CASE WHEN prefix_epoch_event = ?5 THEN 1 ELSE 0 END), 0) AS epoch_rewritten,
    COALESCE(SUM(CASE WHEN prefix_epoch_event = ?6 THEN 1 ELSE 0 END), 0) AS epoch_reseeded
FROM requests
WHERE ts_start >= ?1 AND ts_start < ?2";

/// The window's cache-breakpoint decision summary. All fields are 0 when no
/// row in the window carried a decision or a classified epoch.
pub fn cache_decision_summary(
    db: &UsageDb,
    from_ms: i64,
    to_ms: i64,
) -> Result<CacheDecisionSummary, QueryError> {
    let mut stmt = db.conn().prepare(CACHE_DECISION_SQL)?;
    let summary = stmt.query_row(
        rusqlite::params![
            from_ms,
            to_ms,
            EMITTED_TOKEN,
            PREFIX_EPOCH_STABLE,
            PREFIX_EPOCH_REWRITTEN,
            PREFIX_EPOCH_RESEEDED,
        ],
        |row| {
            Ok(CacheDecisionSummary {
                front_decided: row.get(0)?,
                front_emitted: row.get(1)?,
                terminal_decided: row.get(2)?,
                terminal_emitted: row.get(3)?,
                epoch_classified: row.get(4)?,
                epoch_stable: row.get(5)?,
                epoch_rewritten: row.get(6)?,
                epoch_reseeded: row.get(7)?,
            })
        },
    )?;
    Ok(summary)
}

/// The `auto_skipped:k_below_break_even` token: auto-emit ran, the K
/// estimator's calibrated per-turn reuse for this triple sat below the
/// marker's break-even, and BOTH markers were withheld on economic grounds.
/// Bound into [`SUPPRESSED_SESSIONS_SQL`] rather than inlined so the literal
/// appears once on this side of the vocabulary.
const K_SUPPRESSION_TOKEN: &str = "auto_skipped:k_below_break_even";

/// Most triples [`suppressed_sessions`] returns. A bound, not a page: the
/// finder answers "which sessions are currently latched off caching", which
/// is a short list to act on, and an operator who hits the cap gets
/// [`SuppressedSessions::truncated`] rather than a silently clipped answer.
pub const SUPPRESSED_SESSION_CAP: usize = 20;

/// One `(session, provider_kind, model)` triple that had at least one
/// K-suppressed request in the window.
///
/// `session_ref` is a per-process salted digest, never the raw session key:
/// the key is client-supplied and may be a durable personal identifier, so
/// this finder hashes it on read and never returns, logs, or renders the raw
/// ledger value. The write path already persists `requests.session_id`; this
/// reader does not change what is stored. Two rows of one report sharing
/// a `session_ref` are the same session; a `session_ref` from one run means
/// nothing in another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppressedSessionRow {
    /// Opaque within-run reference for the triple's session key.
    pub session_ref: u64,
    /// Provider kind persisted with the rows.
    pub provider_kind: String,
    /// Served model nickname the K estimator keyed on.
    pub model: String,
    /// REQUESTS in the triple that carried the suppression token, counted
    /// once per request. A request whose front AND terminal decision both
    /// carry the token is one suppressed request, not two -- the two markers
    /// are withheld by a single shared verdict.
    pub suppressed_requests: i64,
    /// `ts_start` of the triple's earliest suppressed request in the window.
    pub first_suppressed_ms: i64,
    /// `ts_start` of the triple's latest suppressed request in the window.
    /// The recency the ordering is by: a still-latched session sorts above
    /// one that stopped being suppressed earlier in the window.
    pub last_suppressed_ms: i64,
}

/// The window's K-suppressed triples, newest-suppression first, bounded by
/// [`SUPPRESSED_SESSION_CAP`].
///
/// VALIDITY: this is a FINDER, not a measurement. The rows it returns are
/// the ones the ledger recorded; a window in which the service's usage
/// channel dropped records (`dropped_full` / `dropped_disabled` /
/// `write_errors` nonzero) under-reports both the triple list and each
/// count by an unknown amount, so no rate, share, or cost conclusion may be
/// drawn from it -- only "these sessions were seen suppressed".
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SuppressedSessions {
    /// The triples, at most [`SUPPRESSED_SESSION_CAP`] of them.
    pub rows: Vec<SuppressedSessionRow>,
    /// True when the window held MORE suppressed triples than the cap, so
    /// the list is a newest-first prefix rather than the whole set.
    pub truncated: bool,
}

/// One row per `(session_id, provider_kind, model)` triple with at least one
/// suppressed request.
///
/// `COUNT(*)` over rows matched by an `OR` across the two decision columns is
/// what makes a request with the token on BOTH columns count once: the row is
/// selected once, not once per column.
///
/// The three key columns must all be present -- the K estimator keys on the
/// full triple, so a row missing any part of it cannot be attributed to a
/// suppressible triple in the first place.
///
/// Ordered newest-suppression first with `session_id, provider_kind, model`
/// as the tiebreak, so the bounded prefix is deterministic for a given
/// ledger. The tiebreak is on the RAW session column rather than the
/// reported digest, which is process-salted and therefore unordered across
/// runs.
const SUPPRESSED_SESSIONS_SQL: &str = "\
SELECT
    session_id,
    provider_kind,
    model,
    COUNT(*)        AS suppressed_requests,
    MIN(ts_start)   AS first_suppressed,
    MAX(ts_start)   AS last_suppressed
FROM requests
WHERE ts_start >= ?1 AND ts_start < ?2
  AND session_id IS NOT NULL
  AND provider_kind IS NOT NULL
  AND model IS NOT NULL
  AND (cache_front_decision = ?3 OR cache_terminal_decision = ?3)
GROUP BY session_id, provider_kind, model
ORDER BY last_suppressed DESC, first_suppressed DESC, session_id, provider_kind, model
LIMIT ?4";

/// Find the window's K-suppressed sessions: the bounded, newest-first list of
/// `(session, provider_kind, model)` triples whose requests carried the
/// K-suppression token, with each triple's suppressed-request count and its
/// first / last suppression instant.
///
/// This is the detection surface for a wrongly-latched session. Suppression
/// withholds a cache marker on economic evidence, and a wrong verdict costs
/// money at HTTP 200 with every availability and error signal green -- so the
/// regret is bounded by how fast the session can be FOUND and the kill switch
/// thrown, which is what this query exists to serve.
///
/// One extra row beyond the cap is requested so the truncation indicator is a
/// fact about the ledger rather than an inference from a full page.
pub fn suppressed_sessions(
    db: &UsageDb,
    from_ms: i64,
    to_ms: i64,
) -> Result<SuppressedSessions, QueryError> {
    let probe_limit = i64::try_from(SUPPRESSED_SESSION_CAP).unwrap_or(i64::MAX - 1) + 1;
    let mut stmt = db.conn().prepare(SUPPRESSED_SESSIONS_SQL)?;
    let mut rows = stmt
        .query_map(
            rusqlite::params![from_ms, to_ms, K_SUPPRESSION_TOKEN, probe_limit],
            |row| {
                Ok(SuppressedSessionRow {
                    session_ref: session_ref(&row.get::<_, String>(0)?),
                    provider_kind: row.get(1)?,
                    model: row.get(2)?,
                    suppressed_requests: row.get(3)?,
                    first_suppressed_ms: row.get(4)?,
                    last_suppressed_ms: row.get(5)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;

    let truncated = rows.len() > SUPPRESSED_SESSION_CAP;
    rows.truncate(SUPPRESSED_SESSION_CAP);
    Ok(SuppressedSessions { rows, truncated })
}

#[cfg(test)]
#[path = "cache_decision_tests.rs"]
mod tests;
