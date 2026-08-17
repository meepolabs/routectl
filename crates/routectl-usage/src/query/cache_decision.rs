//! Cache-breakpoint decision read queries (v16 columns).

use crate::db::UsageDb;
use crate::record::{PREFIX_EPOCH_RESEEDED, PREFIX_EPOCH_REWRITTEN, PREFIX_EPOCH_STABLE};

use super::QueryError;

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

#[cfg(test)]
#[path = "cache_decision_tests.rs"]
mod tests;
