//! Would-trim + K-calibration read queries.

use crate::db::UsageDb;

use super::QueryError;

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
    COALESCE(SUM(CASE WHEN would_trim_tokens IS NOT NULL
              AND would_trim_k_floor IS NOT NULL
              AND would_trim_break_even_k IS NOT NULL
              AND would_trim_k_floor >= would_trim_break_even_k
         THEN 1 ELSE 0 END), 0)                                        AS verdict_met,
    COALESCE(SUM(CASE WHEN would_trim_tokens IS NOT NULL
              AND would_trim_k_floor IS NOT NULL
              AND would_trim_break_even_k IS NOT NULL
              AND would_trim_k_floor < would_trim_break_even_k
         THEN 1 ELSE 0 END), 0)                                        AS verdict_unmet,
    COALESCE(SUM(CASE WHEN would_trim_tokens IS NOT NULL
              AND would_trim_break_even_k IS NOT NULL
              AND would_trim_k_floor IS NULL
         THEN 1 ELSE 0 END), 0)                                        AS verdict_cold,
    COALESCE(SUM(CASE WHEN would_trim_tokens IS NOT NULL
              AND would_trim_break_even_k IS NULL
         THEN 1 ELSE 0 END), 0)                                        AS verdict_unpriced
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

/// Windowed near-lossless attribution: per-heuristic freed-token sums,
/// the path-extractability count-pair, and the context-fraction count-pair,
/// RESTRICTED to rows where `would_trim_recorder_version IS NOT NULL` (the
/// near-lossless recorder pass ran). This filter is load-bearing:
/// pre-recorder rows never carry these columns, so without it a mixed-history
/// window would silently blend baseline and near-lossless semantics.
/// Count-pairs (`path_units`/
/// `path_extractable`, `context_fraction_present`/`context_fraction_sum`) are
/// summed as raw counters here -- divide AFTER summing; never average a
/// per-row rate. Plain data; the caller decides how to display it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct NearLosslessAttributionSummary {
    /// Count of requests where the near-lossless recorder ran
    /// (`would_trim_recorder_version IS NOT NULL`), regardless of whether it
    /// found any marks. The recorder candidate count.
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

const NEAR_LOSSLESS_ATTRIBUTION_SQL: &str = "\
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

/// The window's near-lossless attribution. Restricted to
/// `would_trim_recorder_version IS NOT NULL` so baseline (pre-recorder) rows
/// never mix into these totals. All fields are 0 when no row in the window
/// carried a near-lossless recording.
pub fn near_lossless_attribution_summary(
    db: &UsageDb,
    from_ms: i64,
    to_ms: i64,
) -> Result<NearLosslessAttributionSummary, QueryError> {
    let mut stmt = db.conn().prepare(NEAR_LOSSLESS_ATTRIBUTION_SQL)?;
    let summary = stmt.query_row([from_ms, to_ms], |row| {
        Ok(NearLosslessAttributionSummary {
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
    /// pooled hazard would over-predict E\[K\] late). DIAGNOSTIC, never a gate.
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
SELECT ts_start, session_id, provider_kind, model, cache_read
FROM (
  SELECT rowid AS rid, ts_start, session_id, provider_kind, model, COALESCE(cache_read, 0) AS cache_read
  FROM requests
  WHERE ts_start >= ?1
    AND session_id IS NOT NULL
    AND provider_kind IS NOT NULL
    AND model IS NOT NULL
    AND outcome = 'ok'
  ORDER BY ts_start DESC, rowid DESC
  LIMIT ?2
)
ORDER BY ts_start ASC, rid ASC";

/// Raw reuse samples whose request start time is at or after `window_start_ms`.
/// Selects the most recent `limit` qualifying rows in the window (newest-N,
/// via an inner `ORDER BY ts_start DESC, rowid DESC LIMIT`), then returns them
/// oldest-first (the outer `ORDER BY ts_start ASC, rowid ASC`). When qualifying
/// rows exceed `limit` the oldest are dropped, not the newest. `rowid` breaks
/// ties: it tracks insertion order, so among rows sharing an identical
/// `ts_start` the most recently inserted win the cap and survivors emit in
/// stable insertion order -- selection at the boundary is deterministic.
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
