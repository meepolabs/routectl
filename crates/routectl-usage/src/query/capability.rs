//! Capability-ledger read queries for the warm-rebuild replayer.

/// Verdict token that marks a tombstone row. Mirrors the literal stamped by
/// `capability_event::CapabilityEvent::tombstone`; the two are pinned in
/// agreement by the tombstone round-trip test (a mismatch would make
/// `latest_tombstone` blind to written tombstones).
const TOMBSTONE_VERDICT: &str = "tombstone";

/// One `capability_events` row as read back for replay, carrying the
/// implicit `rowid` (the ledger's insertion-order boundary key). Every
/// column except `rowid` / `ts` is nullable in the schema, so the
/// nullable-by-DDL columns surface as `Option` and the replayer parses the
/// open-set tokens tolerantly. Plain data; the router maps these to its
/// admission calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityEventRow {
    /// The implicit SQLite rowid -- the ledger's insertion-order key.
    pub rowid: i64,
    /// Capture time (epoch milliseconds).
    pub ts: i64,
    /// The NORMALIZED lane key (empty on a tombstone row).
    pub lane_key: Option<String>,
    /// The NORMALIZED capability key (empty on a tombstone row).
    pub capability: Option<String>,
    /// Open-set admission verdict token.
    pub verdict: Option<String>,
    /// Open-set phase token.
    pub phase: Option<String>,
    /// Open-set source token.
    pub source: Option<String>,
    /// Open-set signal-tier token.
    pub tier: Option<String>,
    /// The pinned observation-evidence token, or `None`.
    pub evidence_class: Option<String>,
    /// The raw upstream wire token, forensic / display only, or `None`.
    pub upstream_token: Option<String>,
    /// Catalog version the row was stamped under.
    pub catalog_version: Option<i64>,
    /// Overlay revision the row was stamped under.
    pub overlay_revision: Option<i64>,
}

/// The latest tombstone's boundary key and the revision it was stamped
/// under. The boot replayer compares these revisions against the current
/// catalog / overlay to decide fail-closed vs replay, and reads only rows
/// after `rowid`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TombstoneRow {
    /// The tombstone's rowid -- the correctness boundary.
    pub rowid: i64,
    /// Catalog version stamped on the tombstone, or `None`.
    pub catalog_version: Option<i64>,
    /// Overlay revision stamped on the tombstone, or `None`.
    pub overlay_revision: Option<i64>,
}

/// Read up to `limit` capability events whose rowid is strictly greater
/// than `after_rowid`, oldest-first.
///
/// Ordered `(ts ASC, rowid ASC)` so the replayer sees events in the order
/// they were captured; `rowid` breaks ties among rows sharing an identical
/// `ts` (millisecond collisions) by insertion order, keeping replay
/// deterministic. Capped at `limit` rows (the warm-rebuild row cap): the
/// caller treats a full result as "possibly truncated". `after_rowid = 0`
/// (or any value below the first rowid) reads from the ledger's start.
pub fn read_capability_events_after(
    conn: &rusqlite::Connection,
    after_rowid: i64,
    limit: usize,
) -> rusqlite::Result<Vec<CapabilityEventRow>> {
    let mut stmt = conn.prepare(READ_AFTER_SQL)?;
    let rows = stmt
        .query_map(rusqlite::params![after_rowid, limit as i64], |row| {
            Ok(CapabilityEventRow {
                rowid: row.get(0)?,
                ts: row.get(1)?,
                lane_key: row.get(2)?,
                capability: row.get(3)?,
                verdict: row.get(4)?,
                phase: row.get(5)?,
                source: row.get(6)?,
                tier: row.get(7)?,
                evidence_class: row.get(8)?,
                upstream_token: row.get(9)?,
                catalog_version: row.get(10)?,
                overlay_revision: row.get(11)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The highest-rowid tombstone row, or `None` when the ledger holds none.
///
/// The tombstone is the correctness boundary: the replayer trusts only
/// rows after it, and only when its stamped revision matches the current
/// boot revision. A ledger with no tombstone yields `None`, which the
/// boot path treats as "fail closed and write a fresh tombstone".
pub fn latest_tombstone(conn: &rusqlite::Connection) -> rusqlite::Result<Option<TombstoneRow>> {
    let mut stmt = conn.prepare(&latest_tombstone_sql())?;
    let mut rows = stmt.query([])?;
    match rows.next()? {
        Some(row) => Ok(Some(TombstoneRow {
            rowid: row.get(0)?,
            catalog_version: row.get(1)?,
            overlay_revision: row.get(2)?,
        })),
        None => Ok(None),
    }
}

/// Build the latest-tombstone query. Single-sources the tombstone verdict
/// token from `capability_event` so the boundary read and the row
/// constructor never drift.
fn latest_tombstone_sql() -> String {
    format!(
        "SELECT rowid, catalog_version, overlay_revision \
         FROM capability_events \
         WHERE verdict = '{TOMBSTONE_VERDICT}' \
         ORDER BY rowid DESC LIMIT 1"
    )
}

/// The bound read-after query. Column order matches `CapabilityEventRow`'s
/// `get` positions above.
const READ_AFTER_SQL: &str = "\
SELECT rowid, ts, lane_key, capability, verdict, phase, source, tier,
       evidence_class, upstream_token, catalog_version, overlay_revision
FROM capability_events
WHERE rowid > ?1
ORDER BY ts ASC, rowid ASC
LIMIT ?2";

#[cfg(test)]
#[path = "capability_tests.rs"]
mod tests;
