//! Per-lane cache-ignore evidence: consecutive same-lane turns that carried
//! the caller's own cache markers inside the cache lifetime, over a stable
//! prefix, and still reported neither a cache read nor a cache write.
//!
//! Config-agnostic: the caller names the providers to evaluate and each one's
//! minimum cacheable prompt and cache lifetime; this module only counts.

use std::collections::BTreeSet;

use crate::db::UsageDb;
use crate::record::{Outcome, PREFIX_EPOCH_STABLE};

use super::QueryError;

/// The `caller_supplied` front-region decision token: the request already
/// carried the caller's own cache markers, so routectl placed none.
///
/// This and the two kind tokens below are deliberate MIRRORS of
/// `routectl_core::cache_decision::CALLER_SUPPLIED` and
/// `routectl_core::provider_kind::config::{ANTHROPIC_API, BEDROCK}`,
/// duplicated rather than imported because this crate is a leaf and must not
/// depend on core (the same posture as `cache_decision`'s mirrored tokens).
/// Agreement with the emitting spelling is pinned by a behavioral round-trip
/// test in routectl-cli, the only crate that sees both sides: it writes rows
/// carrying the core constants and asserts this query counts them.
const CALLER_SUPPLIED_TOKEN: &str = "caller_supplied";

/// Config kind of the direct Anthropic Messages API (mirror; see
/// [`CALLER_SUPPLIED_TOKEN`]).
const ANTHROPIC_API_KIND: &str = "anthropic-api";

/// Config kind of native AWS Bedrock (mirror; see [`CALLER_SUPPLIED_TOKEN`]).
const BEDROCK_KIND: &str = "bedrock";

/// Most lanes [`cache_ignore_lanes`] returns. A bound, not a page: the result
/// feeds a per-lane verdict, and a ledger with more lanes than this gets
/// [`CacheIgnoreLanes::truncated`] rather than a silently clipped answer.
pub const CACHE_IGNORE_LANE_CAP: usize = 256;

/// The evaluation rule for one provider the caller considers eligible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheIgnoreRule {
    /// Configured provider name, as persisted in the ledger's `provider`.
    pub provider: String,
    /// Smallest whole prompt (input + cache read + cache write tokens) that
    /// can be cached on this provider; smaller turns never count.
    pub min_prompt_tokens: i64,
    /// Longest gap, in milliseconds, between a turn and the lane's previous
    /// turn for the previous turn's cache entry to still be alive.
    pub lifetime_ms: i64,
}

/// Qualifying-turn counts for one `(provider, seat, model, provider_kind)`
/// lane. Carries no session identifier.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheIgnoreLane {
    /// Configured provider name.
    pub provider: String,
    /// Seat the turns were served on, when the provider has seats.
    pub seat: Option<String>,
    /// Served model.
    pub model: Option<String>,
    /// Config provider kind persisted with the rows.
    pub provider_kind: String,
    /// Qualifying turns that reported a zero cache read AND a zero cache
    /// write.
    pub evidence_turns: i64,
    /// Qualifying turns that reported any cache read or cache write.
    pub active_turns: i64,
    /// Distinct sessions among the lane's qualifying turns.
    pub sessions: i64,
}

/// The window's lanes, at most [`CACHE_IGNORE_LANE_CAP`] of them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CacheIgnoreLanes {
    /// Lanes with no active turn first, so the cap drops lanes that already
    /// showed cache activity before any that might be ignoring markers; then
    /// evidence turns, then active turns (both descending), then the lane
    /// keys.
    pub lanes: Vec<CacheIgnoreLane>,
    /// True when the window held more lanes than the cap. A lane absent from
    /// a truncated result may simply have been cut, so a consumer comparing
    /// two results must not read its absence as a change.
    pub truncated: bool,
}

/// The row population is filtered BEFORE the window function so a pair is
/// always two adjacent inference turns of one session on one lane; every
/// other predicate, outcome included, applies AFTER the pairing, so an
/// intervening failed turn breaks the pair instead of being skipped over.
/// The provider pre-filter is the one exception that cannot change a pair:
/// provider is in the partition key, so a dropped provider's rows were never
/// another provider's previous turn. Kind and decision are not partition
/// keys and must stay after the pairing.
/// `rowid` breaks `ts_start` ties so the pairing is deterministic.
const CACHE_IGNORE_SQL: &str = concat!(
    "\
WITH rules AS (
    SELECT json_extract(value, '$.provider')          AS provider,
           json_extract(value, '$.min_prompt_tokens') AS min_prompt_tokens,
           json_extract(value, '$.lifetime_ms')       AS lifetime_ms
    FROM json_each(?3)
),
paired AS (
    SELECT session_id, provider, seat, model, provider_kind, ts_start, outcome,
           cache_front_decision, prefix_epoch_event, input_tokens, cache_read,
           ",
    cache_write_total_expr!(),
    " AS write_total,
           LAG(ts_start) OVER lane AS prev_ts,
           LAG(outcome)  OVER lane AS prev_outcome
    FROM requests
    WHERE ts_start >= ?1 AND ts_start < ?2
      AND session_id IS NOT NULL
      AND ",
    inference_rows!(),
    "
      AND COALESCE(probe, 0) = 0
      AND provider IN (SELECT provider FROM rules)
    WINDOW lane AS (
        PARTITION BY session_id, provider, COALESCE(seat, ''), model
        ORDER BY ts_start, rowid
    )
),
qualifying AS (
    SELECT p.session_id, p.provider, p.seat, p.model, p.provider_kind,
           p.cache_read, p.write_total
    FROM paired p
    JOIN rules r ON r.provider = p.provider
    WHERE p.prev_ts IS NOT NULL
      AND p.prev_outcome = ?4
      AND p.outcome = ?4
      AND p.cache_front_decision = ?5
      AND p.provider_kind IN (?6, ?7)
      AND p.ts_start - p.prev_ts <= r.lifetime_ms
      AND p.prefix_epoch_event = ?8
      -- The absent-vs-zero guard: an unreported read or write never counts
      -- as zero. Absent read/write read as zero in the size sum below, so
      -- these two checks are the only thing that excludes such a turn.
      -- Absent input_tokens deliberately keeps the sum NULL: an unknown
      -- prompt size never qualifies, whatever the minimum.
      AND p.cache_read IS NOT NULL
      AND p.write_total IS NOT NULL
      AND p.input_tokens + COALESCE(p.cache_read, 0) + COALESCE(p.write_total, 0)
          >= r.min_prompt_tokens
)
SELECT provider, seat, model, provider_kind,
       SUM(CASE WHEN cache_read = 0 AND write_total = 0 THEN 1 ELSE 0 END) AS evidence_turns,
       SUM(CASE WHEN cache_read = 0 AND write_total = 0 THEN 0 ELSE 1 END) AS active_turns,
       COUNT(DISTINCT session_id)                                          AS sessions
FROM qualifying
GROUP BY provider, seat, model, provider_kind
ORDER BY (active_turns = 0) DESC, evidence_turns DESC, active_turns DESC,
         provider, seat, model, provider_kind
LIMIT ?9"
);

/// Count, per lane, the turns in `[from_ms, to_ms)` that sent the caller's
/// cache markers to a marker-carrying egress within the provider's cache
/// lifetime of an ok previous turn on the same lane, over a stable prefix at
/// or above the provider's minimum, and say whether each reported any cache
/// activity.
///
/// Only providers named in `rules` are counted. A provider named twice keeps
/// its first rule. Empty `rules` return an empty result without reading the
/// ledger.
pub fn cache_ignore_lanes(
    db: &UsageDb,
    from_ms: i64,
    to_ms: i64,
    rules: &[CacheIgnoreRule],
) -> Result<CacheIgnoreLanes, QueryError> {
    if rules.is_empty() {
        return Ok(CacheIgnoreLanes::default());
    }
    let probe_limit = i64::try_from(CACHE_IGNORE_LANE_CAP).unwrap_or(i64::MAX - 1) + 1;
    let mut stmt = db.conn().prepare(CACHE_IGNORE_SQL)?;
    let mut lanes = stmt
        .query_map(
            rusqlite::params![
                from_ms,
                to_ms,
                rules_json(rules),
                Outcome::Ok.as_str(),
                CALLER_SUPPLIED_TOKEN,
                ANTHROPIC_API_KIND,
                BEDROCK_KIND,
                PREFIX_EPOCH_STABLE,
                probe_limit,
            ],
            |row| {
                Ok(CacheIgnoreLane {
                    provider: row.get(0)?,
                    seat: row.get(1)?,
                    model: row.get(2)?,
                    provider_kind: row.get(3)?,
                    evidence_turns: row.get(4)?,
                    active_turns: row.get(5)?,
                    sessions: row.get(6)?,
                })
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;

    let truncated = lanes.len() > CACHE_IGNORE_LANE_CAP;
    lanes.truncate(CACHE_IGNORE_LANE_CAP);
    Ok(CacheIgnoreLanes { lanes, truncated })
}

/// The rules as one JSON array, first rule per provider only, so the SQL join
/// can never count a turn twice.
fn rules_json(rules: &[CacheIgnoreRule]) -> String {
    let mut seen = BTreeSet::new();
    let unique: Vec<serde_json::Value> = rules
        .iter()
        .filter(|rule| seen.insert(rule.provider.as_str()))
        .map(|rule| {
            serde_json::json!({
                "provider": rule.provider,
                "min_prompt_tokens": rule.min_prompt_tokens,
                "lifetime_ms": rule.lifetime_ms,
            })
        })
        .collect();
    serde_json::Value::Array(unique).to_string()
}

#[cfg(test)]
#[path = "cache_ignore_tests.rs"]
mod tests;
