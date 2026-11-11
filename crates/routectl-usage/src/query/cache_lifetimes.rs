//! Per-lifetime cache-write sums at the aggregate's fine grain, for pricing.
//!
//! Pricing a cache write needs to know how long each written token was cached
//! for, and that lives in the `cache_write_lifetimes` JSON split. Reading it
//! means parsing every split, so it runs as its own statement that only the
//! priced readers issue: [`super::query`] and
//! [`aggregate_with_cache_lifetimes`]. The unpriced [`super::aggregate`] read
//! never parses JSON.
//!
//! The statement reads only rows that carry a positive write total, one
//! result row per ledger row, and each split is parsed and folded into its
//! fine group here rather than grouped in SQL. Legacy rows
//! (written before the total and split columns existed) carry their writes in
//! the 5-minute and 1-hour columns, which the aggregate already sums per fine
//! row; [`attach`] merges those sums as the 300 s and 3600 s lifetimes.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use rusqlite::types::ValueRef;
use rusqlite::{Row, Transaction};
use serde::de::{Deserialize, Deserializer, IgnoredAny, MapAccess, SeqAccess, Visitor};

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
    /// lifetime a row with a positive write total reported, a reported zero
    /// included; a row whose total is zero adds nothing.
    pub tokens_by_lifetime: BTreeMap<u32, i64>,
    /// Summed write tokens with no reported lifetime.
    pub unknown_tokens: i64,
    /// Whether any row's split sums above its own total.
    pub invalid_split: bool,
}

impl CacheWriteLifetimes {
    fn add_lifetime(&mut self, seconds: u32, tokens: i64) {
        let sum = self.tokens_by_lifetime.entry(seconds).or_insert(0);
        *sum = sum.saturating_add(tokens);
    }
}

/// The rows the per-lifetime read visits: those carrying a positive write
/// total. A zero total contributes no tokens to any lifetime, so skipping it
/// changes no price. Word for word the WHERE of
/// [`crate::schema::CREATE_CACHE_WRITE_TS_START_INDEX`]: SQLite uses a
/// partial index only when the query's WHERE implies the index's.
macro_rules! write_total_rows {
    () => {
        "cache_write_tokens > 0"
    };
}

/// The statement, with the bucket expression as its one parameter: one
/// result row per ledger row carrying a positive write total, with its fine
/// group key, bucket, total and raw split. No grouping in SQL: each split is
/// parsed and folded per row in Rust (see [`fold_row`]).
macro_rules! cache_lifetimes_sql {
    ($bucket:literal) => {
        concat!(
            "SELECT COALESCE(model, requested_model), provider, upstream, alias,
        provider_kind, ",
            $bucket,
            ", cache_write_tokens, cache_write_lifetimes
    FROM requests
    WHERE ts_start >= ?1 AND ts_start < ?2
      AND ",
            inference_rows!(),
            "
      AND (?3 IS NULL OR alias = ?3)
      AND (?4 IS NULL OR provider = ?4)
      AND ",
            write_total_rows!()
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

/// Read the per-lifetime writes of the rows carrying a positive write total,
/// for every fine group in the window. With `bucket_width_ms`, each group is
/// further split by bucket index. Legacy writes are not read here; see
/// [`attach`].
pub(super) fn read_lifetimes(
    tx: &Transaction<'_>,
    window: &LifetimeWindow<'_>,
    bucket_width_ms: Option<i64>,
) -> Result<LifetimesByRow, QueryError> {
    let mut out = LifetimesByRow::new();
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
                fold_row(row, &mut out)?;
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
                fold_row(row, &mut out)?;
            }
        }
    }
    Ok(out)
}

fn fold_row(row: &Row<'_>, out: &mut LifetimesByRow) -> rusqlite::Result<()> {
    let key = GroupKey {
        model: row.get(0)?,
        provider: row.get(1)?,
        upstream: row.get(2)?,
        alias: row.get(3)?,
        provider_kind: row.get(4)?,
    };
    let bucket: i64 = row.get(5)?;
    let total: i64 = row.get(6)?;
    let split = match row.get_ref(7)? {
        ValueRef::Text(text) => parse_split(text),
        _ => Vec::new(),
    };
    add_row(out.entry((key, bucket)).or_default(), total, &split);
    Ok(())
}

/// Fold one ledger row into its fine group: every counting entry adds to its
/// lifetime; the row's total beyond its own split sum is unknown, and a split
/// summing above the total flags the group invalid instead.
fn add_row(acc: &mut CacheWriteLifetimes, total: i64, split: &[(u32, i64)]) {
    let mut split_sum: i64 = 0;
    for &(seconds, tokens) in split {
        acc.add_lifetime(seconds, tokens);
        split_sum = split_sum.saturating_add(tokens);
    }
    if split_sum > total {
        acc.invalid_split = true;
    } else {
        acc.unknown_tokens = acc.unknown_tokens.saturating_add(total - split_sum);
    }
}

/// The counting entries of a stored split, in document order (a repeated key
/// counts each time). Text that is not a JSON object yields none.
///
/// An entry counts when its value is a JSON integer (a boolean, a float or an
/// integer outside `i64` does not) that is not negative, under a key that is a
/// positive decimal lifetime fitting `u32` seconds with no leading zero. Any
/// other entry counts toward neither a lifetime nor the split sum, so its
/// tokens stay unknown.
fn parse_split(text: &[u8]) -> Vec<(u32, i64)> {
    serde_json::from_slice::<SplitEntries>(text)
        .map(|entries| entries.0)
        .unwrap_or_default()
}

fn lifetime_seconds(key: &str) -> Option<u32> {
    let bytes = key.as_bytes();
    let leading_digit = matches!(bytes.first(), Some(b'1'..=b'9'));
    if !leading_digit || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    key.parse().ok()
}

/// A split document's counting entries; deserializes only from an object.
struct SplitEntries(Vec<(u32, i64)>);

impl<'de> Deserialize<'de> for SplitEntries {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(SplitVisitor)
    }
}

struct SplitVisitor;

impl<'de> Visitor<'de> for SplitVisitor {
    type Value = SplitEntries;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON object of lifetime seconds to tokens")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<SplitEntries, A::Error> {
        let mut entries = Vec::new();
        while let Some((LifetimeKey(key), value)) = map.next_entry::<LifetimeKey, SplitTokens>()? {
            if let (Some(seconds), SplitTokens::Counted(tokens)) = (key, value) {
                entries.push((seconds, tokens));
            }
        }
        Ok(SplitEntries(entries))
    }
}

/// One split entry's key as lifetime seconds, `None` when it is not one.
struct LifetimeKey(Option<u32>);

impl<'de> Deserialize<'de> for LifetimeKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_str(KeyVisitor)
    }
}

struct KeyVisitor;

impl Visitor<'_> for KeyVisitor {
    type Value = LifetimeKey;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("an object key")
    }

    fn visit_str<E>(self, key: &str) -> Result<LifetimeKey, E> {
        Ok(LifetimeKey(lifetime_seconds(key)))
    }
}

/// One split entry's value: a non-negative `i64` integer counts; any other
/// JSON value is consumed and ignored.
enum SplitTokens {
    Counted(i64),
    Ignored,
}

impl<'de> Deserialize<'de> for SplitTokens {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(TokensVisitor)
    }
}

struct TokensVisitor;

impl<'de> Visitor<'de> for TokensVisitor {
    type Value = SplitTokens;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_i64<E>(self, v: i64) -> Result<SplitTokens, E> {
        Ok(if v >= 0 {
            SplitTokens::Counted(v)
        } else {
            SplitTokens::Ignored
        })
    }

    fn visit_u64<E>(self, v: u64) -> Result<SplitTokens, E> {
        Ok(i64::try_from(v).map_or(SplitTokens::Ignored, SplitTokens::Counted))
    }

    fn visit_f64<E>(self, _: f64) -> Result<SplitTokens, E> {
        Ok(SplitTokens::Ignored)
    }

    fn visit_bool<E>(self, _: bool) -> Result<SplitTokens, E> {
        Ok(SplitTokens::Ignored)
    }

    fn visit_str<E>(self, _: &str) -> Result<SplitTokens, E> {
        Ok(SplitTokens::Ignored)
    }

    fn visit_unit<E>(self) -> Result<SplitTokens, E> {
        Ok(SplitTokens::Ignored)
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<SplitTokens, A::Error> {
        while seq.next_element::<IgnoredAny>()?.is_some() {}
        Ok(SplitTokens::Ignored)
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<SplitTokens, A::Error> {
        while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
        Ok(SplitTokens::Ignored)
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
