//! Per-lifetime cache-write sums at the aggregate's fine grain, for pricing.
//!
//! Pricing a cache write needs to know how long each written token was cached
//! for, and that lives in the `cache_write_lifetimes` JSON split. Reading it
//! takes a `json_each` pass, so it runs as its own statement that only the
//! priced readers issue: [`super::query`] and
//! [`aggregate_with_cache_lifetimes`]. The unpriced [`super::aggregate`] read
//! never parses JSON.
//!
//! The statement reads only rows that carry a write total. Legacy rows
//! (written before the total and split columns existed) carry their writes in
//! the 5-minute and 1-hour columns, which the aggregate already sums per fine
//! row; [`attach`] merges those sums as the 300 s and 3600 s lifetimes.

use std::collections::{BTreeMap, HashMap};

use rusqlite::{Row, Transaction};

use crate::cache_writes::{LEGACY_1H_SECONDS, LEGACY_5M_SECONDS};
use crate::db::UsageDb;

use super::{AggRow, GroupKey, QueryError};

/// The cache writes of one fine aggregate row, split by how long the written
/// tokens were cached for.
///
/// `unknown_tokens` is what the per-row totals report beyond their own splits:
/// a total reported with no split is wholly unknown. A row whose split sums
/// above its total sets `invalid_split` and contributes no unknown tokens; the
/// row's writes cannot be priced as recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CacheWriteLifetimes {
    /// Summed tokens per lifetime in seconds. Holds an entry for every
    /// lifetime some row reported, a reported zero included.
    pub tokens_by_lifetime: BTreeMap<u32, i64>,
    /// Summed write tokens with no reported lifetime.
    pub unknown_tokens: i64,
    /// Whether any row's split sums above its own total.
    pub invalid_split: bool,
}

impl CacheWriteLifetimes {
    fn add_lifetime(&mut self, seconds: u32, tokens: i64) {
        *self.tokens_by_lifetime.entry(seconds).or_insert(0) += tokens;
    }
}

/// A split entry that counts: a JSON integer token value (a boolean, a float
/// or an integer too large for `i64` does not count) that is not negative,
/// under a key that is a positive decimal lifetime fitting `u32` seconds. Any
/// other entry counts toward neither a lifetime nor the split sum, so its
/// tokens stay unknown.
macro_rules! split_entry_counts {
    () => {
        "je.type = 'integer' AND typeof(je.value) = 'integer' AND je.value >= 0
            AND je.key GLOB '[1-9]*' AND je.key NOT GLOB '*[^0-9]*'
            AND length(je.key) <= 10 AND CAST(je.key AS INTEGER) <= 4294967295"
    };
}

/// The rows the per-lifetime read visits: those carrying a write total.
/// Word for word the WHERE of
/// [`crate::schema::CREATE_CACHE_WRITE_TS_START_INDEX`]: SQLite uses a
/// partial index only when the query's WHERE implies the index's.
macro_rules! write_total_rows {
    () => {
        "cache_write_tokens IS NOT NULL"
    };
}

/// The fine-group columns every relation below carries.
macro_rules! fine_key {
    () => {
        "k_model, k_provider, k_upstream, k_alias, k_kind, bucket"
    };
}

/// The statement body, with the bucket expression as its one parameter.
///
/// `writes` is the window's rows carrying a write total, at the aggregate's
/// fine grain, read once. A split is kept only when it is a JSON object (a
/// split beside no total is not a shape the ledger writer produces).
/// `entries` expands each kept split once into its counting entries, keyed by
/// the ledger row, and both consumers read that one expansion. The first
/// emits one summary row per fine group: the unknown remainder and the
/// invalid flag, each derived per ledger row from that row's own entry sum
/// (`lifetime` NULL). The second emits one row per (fine group, lifetime).
macro_rules! cache_lifetimes_sql {
    ($bucket:literal) => {
        concat!(
            "WITH writes AS MATERIALIZED (
    SELECT rowid AS rid,
        COALESCE(model, requested_model) AS k_model, provider AS k_provider,
        upstream AS k_upstream, alias AS k_alias, provider_kind AS k_kind,
        ",
            $bucket,
            " AS bucket,
        cache_write_tokens AS total,
        CASE WHEN json_valid(cache_write_lifetimes)
            THEN CASE WHEN json_type(cache_write_lifetimes) = 'object'
                THEN cache_write_lifetimes END END AS split
    FROM requests
    WHERE ts_start >= ?1 AND ts_start < ?2
      AND ",
            inference_rows!(),
            "
      AND (?3 IS NULL OR alias = ?3)
      AND (?4 IS NULL OR provider = ?4)
      AND ",
            write_total_rows!(),
            "
),
entries AS MATERIALIZED (
    SELECT writes.rid, ",
            fine_key!(),
            ",
        CAST(je.key AS INTEGER) AS lifetime, je.value AS tokens
    FROM writes, json_each(writes.split) AS je
    WHERE ",
            split_entry_counts!(),
            "
),
split_sums AS MATERIALIZED (
    SELECT rid, SUM(tokens) AS split_sum FROM entries GROUP BY rid
)
SELECT ",
            fine_key!(),
            ",
    NULL AS lifetime, 0 AS tokens,
    COALESCE(SUM(CASE WHEN COALESCE(split_sum, 0) <= total
        THEN total - COALESCE(split_sum, 0) ELSE 0 END), 0) AS unknown,
    MAX(CASE WHEN split_sum > total THEN 1 ELSE 0 END) AS invalid
FROM writes LEFT JOIN split_sums USING (rid)
GROUP BY ",
            fine_key!(),
            "
UNION ALL
SELECT ",
            fine_key!(),
            ",
    lifetime, SUM(tokens) AS tokens, 0, 0
FROM entries
GROUP BY ",
            fine_key!(),
            ", lifetime"
        )
    };
}

const CACHE_LIFETIMES_SQL: &str = cache_lifetimes_sql!("0");

/// [`CACHE_LIFETIMES_SQL`] split further by the same bucket index the series
/// aggregate groups on (`?5` is the bucket width, `?1` its anchor).
const CACHE_LIFETIMES_BUCKETED_SQL: &str = cache_lifetimes_sql!("(ts_start - ?1) / ?5");

/// Per-lifetime cache writes of the rows carrying a write total, keyed by fine
/// group and bucket index. The unbucketed read puts every row in bucket 0.
pub(super) type LifetimesByRow = HashMap<(GroupKey, i64), CacheWriteLifetimes>;

/// The window and filters a lifetime read applies; the same bind values the
/// aggregate it is merged into used.
pub(super) struct LifetimeWindow<'a> {
    pub from_ms: i64,
    pub to_ms: i64,
    pub alias_filter: Option<&'a str>,
    pub provider_filter: Option<&'a str>,
}

/// Read the per-lifetime writes of the rows carrying a write total, for every
/// fine group in the window. With `bucket_width_ms`, each group is further
/// split by bucket index. Legacy writes are not read here; see [`attach`].
pub(super) fn read_lifetimes(
    tx: &Transaction<'_>,
    window: &LifetimeWindow<'_>,
    bucket_width_ms: Option<i64>,
) -> Result<LifetimesByRow, QueryError> {
    let mut out = LifetimesByRow::new();
    let mut fold = |row: &Row<'_>| -> rusqlite::Result<()> {
        let (key, bucket, part) = map_part(row)?;
        merge_part(out.entry((key, bucket)).or_default(), part);
        Ok(())
    };
    match bucket_width_ms {
        None => {
            let mut stmt = tx.prepare(CACHE_LIFETIMES_SQL)?;
            let mut rows = stmt.query(rusqlite::params![
                window.from_ms,
                window.to_ms,
                window.alias_filter,
                window.provider_filter,
            ])?;
            while let Some(row) = rows.next()? {
                fold(row)?;
            }
        }
        Some(width_ms) => {
            let mut stmt = tx.prepare(CACHE_LIFETIMES_BUCKETED_SQL)?;
            let mut rows = stmt.query(rusqlite::params![
                window.from_ms,
                window.to_ms,
                window.alias_filter,
                window.provider_filter,
                width_ms,
            ])?;
            while let Some(row) = rows.next()? {
                fold(row)?;
            }
        }
    }
    Ok(out)
}

/// One result row: either a fine group's summary (`lifetime` NULL) or one
/// lifetime's split sum.
enum Part {
    Summary { unknown: i64, invalid: bool },
    Lifetime { seconds: u32, tokens: i64 },
}

fn map_part(row: &Row<'_>) -> rusqlite::Result<(GroupKey, i64, Part)> {
    let key = GroupKey {
        model: row.get(0)?,
        provider: row.get(1)?,
        upstream: row.get(2)?,
        alias: row.get(3)?,
        provider_kind: row.get(4)?,
    };
    let bucket = row.get(5)?;
    let part = match row.get::<_, Option<u32>>(6)? {
        Some(seconds) => Part::Lifetime {
            seconds,
            tokens: row.get(7)?,
        },
        None => Part::Summary {
            unknown: row.get(8)?,
            invalid: row.get::<_, i64>(9)? != 0,
        },
    };
    Ok((key, bucket, part))
}

fn merge_part(acc: &mut CacheWriteLifetimes, part: Part) {
    match part {
        Part::Lifetime { seconds, tokens } => acc.add_lifetime(seconds, tokens),
        Part::Summary { unknown, invalid } => {
            acc.unknown_tokens += unknown;
            acc.invalid_split |= invalid;
        }
    }
}

/// Attach each row's per-lifetime writes, taking them out of `lifetimes` and
/// merging the row's own legacy column sums in as the 300 s and 3600 s
/// lifetimes. A legacy column adds its lifetime only when some ledger row in
/// the group reported it, a reported zero included. A row with no reported
/// write gets an empty set.
pub(super) fn attach(row: &mut AggRow, bucket: i64, lifetimes: &mut LifetimesByRow) {
    let mut found = lifetimes
        .remove(&(row.key.clone(), bucket))
        .unwrap_or_default();
    if row.cache_write_5m_present > 0 {
        found.add_lifetime(LEGACY_5M_SECONDS, row.cache_write_5m);
    }
    if row.cache_write_1h_present > 0 {
        found.add_lifetime(LEGACY_1H_SECONDS, row.cache_write_1h);
    }
    row.cache_write_lifetimes = Some(found);
}

/// [`super::aggregate`] with each row's per-lifetime cache writes attached, for
/// a caller that prices the rows. Both statements read one snapshot.
pub fn aggregate_with_cache_lifetimes(
    db: &UsageDb,
    from_ms: i64,
    to_ms: i64,
) -> Result<Vec<AggRow>, QueryError> {
    let tx = db.conn().unchecked_transaction()?;
    let mut rows = super::aggregate::aggregate_in(&tx, from_ms, to_ms)?;
    let window = LifetimeWindow {
        from_ms,
        to_ms,
        alias_filter: None,
        provider_filter: None,
    };
    let mut lifetimes = read_lifetimes(&tx, &window, None)?;
    // A read-only transaction has nothing to commit; finishing releases the
    // snapshot.
    tx.finish()?;
    for row in &mut rows {
        attach(row, 0, &mut lifetimes);
    }
    Ok(rows)
}

#[cfg(test)]
#[path = "cache_lifetimes_tests.rs"]
mod tests;
