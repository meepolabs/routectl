//! The two `requests` columns that record a request's prompt-cache writes.
//!
//! `cache_write_tokens` is the write total and `cache_write_lifetimes` the
//! per-lifetime split as a JSON object keyed by lifetime seconds. NULL in the
//! total means the upstream reported no write usage; 0 is an observed zero.
//! NULL in the split means no split was reported. Tokens the split does not
//! cover (total minus split sum) are an unknown lifetime, derived on read and
//! never stored. A split summing above the total is stored as reported, so a
//! reader can see it and leave the row unpriced.
//!
//! [`cache_write_columns`] is the only producer of both values, so the total
//! and the split written to one row can never disagree about where they came
//! from.

use std::collections::BTreeMap;
use std::fmt::Write as _;

/// The bound values of `cache_write_tokens` and `cache_write_lifetimes` for
/// one row. `None` binds SQL NULL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheWriteColumns {
    /// The write total in tokens.
    pub tokens: Option<u64>,
    /// The canonical JSON split, keyed by decimal lifetime seconds in
    /// ascending numeric order, e.g. `{"300":2523,"3600":0}`.
    pub lifetimes: Option<String>,
}

/// Build the two cache-write column values from a reported total and a
/// lifetime-seconds -> tokens split.
///
/// - With a total, the total is stored as given, whatever the split sums to.
/// - With a split and no total, the split's sum is the total.
/// - An empty split counts as no split.
/// - Zero entries in the split are kept as reported.
pub fn cache_write_columns(
    total: Option<u64>,
    lifetimes: Option<&BTreeMap<u32, u64>>,
) -> CacheWriteColumns {
    let split = lifetimes.filter(|split| !split.is_empty());
    let tokens = total.or_else(|| split.map(split_sum));
    CacheWriteColumns {
        tokens,
        lifetimes: split.map(canonical_json),
    }
}

fn split_sum(split: &BTreeMap<u32, u64>) -> u64 {
    split
        .values()
        .fold(0u64, |sum, tokens| sum.saturating_add(*tokens))
}

/// Render the split as a JSON object. Written by hand because a JSON map keyed
/// by strings sorts lexically ("1800" before "300"); iterating the integer keys
/// keeps the numeric order.
fn canonical_json(split: &BTreeMap<u32, u64>) -> String {
    let mut out = String::from("{");
    for (index, (seconds, tokens)) in split.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(out, "\"{seconds}\":{tokens}");
    }
    out.push('}');
    out
}

#[cfg(test)]
#[path = "cache_writes_tests.rs"]
mod tests;
