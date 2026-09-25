//! Raw read of one ingress dialect's streaming rows and their `extra` JSON,
//! for a caller that classifies the diagnostics a producer merged into it.
//!
//! This layer does not interpret `extra`: every row in the window is handed
//! back, whatever its `extra` holds, so a malformed or unexpected value is the
//! classifier's to count rather than this query's to drop.

use rusqlite::types::ValueRef;

use crate::db::UsageDb;

use super::QueryError;

/// One streaming row: its served lane as persisted, outcome and raw `extra`
/// text. The lane columns are raw client- and config-derived text; a caller
/// rendering them sanitizes them first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamExtraRow {
    /// Request start time, epoch-millis UTC.
    pub ts_start_ms: i64,
    /// Served provider name; `None` when no target was dispatched.
    pub provider: Option<String>,
    /// Provider kind persisted with the row; `None` before dispatch and on
    /// history written before the column was populated.
    pub provider_kind: Option<String>,
    /// Served model nickname; `None` when no target was dispatched.
    pub model: Option<String>,
    /// Served upstream model id; `None` when no target was dispatched.
    pub upstream: Option<String>,
    /// Terminal outcome token (`Outcome::as_str`).
    pub outcome: String,
    /// The `extra` column as text; `None` when NULL. A value stored with a
    /// non-text SQLite type is rendered as text rather than refused.
    pub extra: Option<String>,
}

const STREAM_EXTRA_SQL: &str = "\
SELECT ts_start, provider, provider_kind, model, upstream, outcome, extra
FROM requests
WHERE ingress_dialect = ?1
  AND stream = 1
  AND ts_start >= ?2 AND ts_start < ?3
ORDER BY ts_start ASC, rowid ASC";

/// Visit every streaming row of `ingress_dialect` whose start time falls in
/// the half-open window `[from_ms, to_ms)`, oldest first, without buffering
/// the window. Returns how many rows were visited.
pub fn for_each_stream_extra(
    db: &UsageDb,
    ingress_dialect: &str,
    from_ms: i64,
    to_ms: i64,
    mut visit: impl FnMut(StreamExtraRow),
) -> Result<u64, QueryError> {
    let mut stmt = db.conn().prepare(STREAM_EXTRA_SQL)?;
    let mut rows = stmt.query(rusqlite::params![ingress_dialect, from_ms, to_ms])?;
    let mut visited = 0_u64;
    while let Some(row) = rows.next()? {
        visit(StreamExtraRow {
            ts_start_ms: row.get(0)?,
            provider: row.get(1)?,
            provider_kind: row.get(2)?,
            model: row.get(3)?,
            upstream: row.get(4)?,
            outcome: row.get(5)?,
            extra: extra_text(row.get_ref(6)?),
        });
        visited += 1;
    }
    Ok(visited)
}

fn extra_text(value: ValueRef<'_>) -> Option<String> {
    match value {
        ValueRef::Null => None,
        ValueRef::Integer(n) => Some(n.to_string()),
        ValueRef::Real(x) => Some(x.to_string()),
        ValueRef::Text(bytes) | ValueRef::Blob(bytes) => {
            Some(String::from_utf8_lossy(bytes).into_owned())
        }
    }
}

#[cfg(test)]
#[path = "stream_extra_tests.rs"]
mod tests;
