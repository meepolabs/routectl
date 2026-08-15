//! Token-estimate calibration evidence read query, for the router's
//! boot-time per-lane warm rebuild.

/// One raw calibration-evidence pair for the per-lane warm rebuild.
/// Usage-LOCAL: epoch-ms timestamp and signed counts as stored, with no
/// router types. The caller (the router-side rebuild) owns the lane key, the
/// cohort derivation and the ratio arithmetic; this layer only hands back the
/// columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalibrationSampleRow {
    /// Request start time, epoch-millis UTC.
    pub ts_start_ms: i64,
    /// Stable provider-kind token of the served target.
    pub provider_kind: String,
    /// Served model NICKNAME, never the upstream wire id.
    pub model: String,
    /// Inbound session identifier the request was recorded under, absent when
    /// the request carried no recognized identity. Kept NULLABLE on purpose:
    /// the live path records a keyless request too (under a shared cohort), so
    /// filtering these out would admit fewer rows than live traffic did.
    pub session_id: Option<String>,
    /// routectl's own byte-heuristic estimate of the dispatched payload.
    pub estimated_tokens: i64,
    /// The upstream's own cache-INCLUSIVE prompt total for the same request.
    pub prompt_tokens: i64,
}

const CALIBRATION_SAMPLES_SQL: &str = "\
SELECT ts_start, provider_kind, model, session_id, calib_estimated_tokens, calib_prompt_tokens
FROM (
  SELECT rowid AS rid, ts_start, provider_kind, model, session_id,
         calib_estimated_tokens, calib_prompt_tokens
  FROM requests
  WHERE ts_start >= ?1
    AND provider_kind IS NOT NULL
    AND model IS NOT NULL
    AND calib_estimated_tokens IS NOT NULL
    AND calib_prompt_tokens IS NOT NULL
    AND outcome = 'ok'
  ORDER BY ts_start DESC, rowid DESC
  LIMIT ?2
)
ORDER BY ts_start ASC, rid ASC";

/// Raw calibration-evidence pairs whose request start time is at or after
/// `window_start_ms`. Selects the most recent `limit` qualifying rows in the
/// window (newest-N, via an inner `ORDER BY ts_start DESC, rowid DESC LIMIT`),
/// then returns them oldest-first (the outer `ORDER BY ts_start ASC, rid ASC`)
/// so a replay lands in the order live traffic produced. When qualifying rows
/// exceed `limit` the oldest are dropped, not the newest. `rowid` breaks ties:
/// it tracks insertion order, so among rows sharing an identical `ts_start`
/// the most recently inserted win the cap and survivors emit in stable
/// insertion order.
///
/// Admission contract, matching the live sample path row for row:
///
/// - `outcome = 'ok'` ONLY. The live write fires on the success finalize, and
///   the finalize refuses the evidence pair outright on any other outcome. A
///   mid-stream failure may have observed a partial prompt total, but it never
///   reaches the live store, so the rebuild must not replay it either --
///   otherwise a restart would admit rows live traffic never would, silently
///   diverging the two paths.
/// - both calibration columns NOT NULL. The pair is admitted or refused as a
///   unit, so a NULL in either half means the row was refused as evidence when
///   it was written.
/// - `provider_kind` and `model` NOT NULL. A lane is
///   `(provider_kind, served nickname)`; a NULL in either half has no usable
///   lane identity and forms no lane on the live path either.
///
/// `model` is the SERVED NICKNAME, which is what the live write and the gate's
/// lookup both key on. Keying this query on the upstream wire id instead would
/// silently never match a lane the gate can look up, holding every lane
/// uncorrected forever while reading as health.
///
/// `session_id` is deliberately NOT filtered: the live path records a keyless
/// request under a shared cohort rather than dropping it.
pub fn read_calibration_samples_since(
    conn: &rusqlite::Connection,
    window_start_ms: i64,
    limit: usize,
) -> rusqlite::Result<Vec<CalibrationSampleRow>> {
    let mut stmt = conn.prepare(CALIBRATION_SAMPLES_SQL)?;
    let rows = stmt
        .query_map(rusqlite::params![window_start_ms, limit as i64], |row| {
            Ok(CalibrationSampleRow {
                ts_start_ms: row.get(0)?,
                provider_kind: row.get(1)?,
                model: row.get(2)?,
                session_id: row.get(3)?,
                estimated_tokens: row.get(4)?,
                prompt_tokens: row.get(5)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
#[path = "calibration_tests.rs"]
mod tests;
