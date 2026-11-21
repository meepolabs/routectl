//! Exact time-to-first-token and latency percentiles per display group, per
//! series bucket and over the window.
//!
//! A percentile of a group is not a function of its fine rows' percentiles,
//! so this read does not go through the fine aggregate: it reads every
//! in-window inference row's display label, bucket index, TTFT (when the row
//! is TTFT-eligible) and latency, inside the same slice transaction, window,
//! filters and watermark as the aggregate, and keeps the raw values. The
//! slices' values are concatenated per label and bucket and each set is
//! reduced by nearest rank; the grouped fold stores the figures on the metric
//! set they describe.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};

use rusqlite::Transaction;
use rusqlite::types::ValueRef;

use super::QueryError;
use super::aggregate::ReadWindow;
use super::deadline::ReadLimit;
use super::grouped::{GroupDim, UNATTRIBUTED};

/// The one statement shape: the display label first, then the served
/// provider entry when the label falls back to that entry's account (NULL
/// otherwise), the bucket index, the TTFT of an eligible row (NULL otherwise)
/// and the latency, for every inference row of the window. Every column it
/// reads is in the covering aggregate index.
macro_rules! timings_sql {
    ($label:literal, $fallback:literal, $bucket:literal) => {
        concat!(
            "SELECT ",
            $label,
            ", ",
            $fallback,
            ", ",
            $bucket,
            ",
    CASE WHEN ",
            ttft_eligible!(),
            " THEN ttfb_ms END, latency_ms
FROM requests
WHERE ",
            read_filters!()
        )
    };
}

/// The bucket expression of the series read: the same global grid index the
/// series aggregate groups on (`?7` the width, `?8` the anchor).
macro_rules! timings_sql_pair {
    ($label:literal, $fallback:literal) => {
        (
            timings_sql!($label, $fallback, "0"),
            timings_sql!($label, $fallback, "(ts_start - ?8) / ?7"),
        )
    };
}

/// The `(unbucketed, bucketed)` statements of `group_by`. The label columns
/// are the columns [`QuerySpec::group_label`] reads from the fine key.
///
/// [`QuerySpec::group_label`]: super::grouped::QuerySpec::group_label
pub(super) const fn statements(group_by: GroupDim) -> (&'static str, &'static str) {
    match group_by {
        GroupDim::Model => timings_sql_pair!("COALESCE(model, requested_model)", "NULL"),
        GroupDim::Alias => timings_sql_pair!("alias", "NULL"),
        GroupDim::Provider => timings_sql_pair!("provider", "NULL"),
        GroupDim::Seat => timings_sql_pair!("seat", "provider"),
    }
}

/// The display label of one timing row, by the rule
/// [`QuerySpec::group_label`] applies to a fine row: the label column when it
/// is set, else the account `entry_accounts` maps the fallback entry to, else
/// `(unattributed)`.
///
/// [`QuerySpec::group_label`]: super::grouped::QuerySpec::group_label
fn row_label<'r>(
    label: ValueRef<'r>,
    fallback: ValueRef<'r>,
    entry_accounts: &'r BTreeMap<String, String>,
) -> Result<&'r str, rusqlite::Error> {
    if let ValueRef::Text(text) = label {
        return Ok(std::str::from_utf8(text)?);
    }
    if let ValueRef::Text(entry) = fallback
        && let Some(account) = entry_accounts.get(std::str::from_utf8(entry)?)
    {
        return Ok(account);
    }
    Ok(UNATTRIBUTED)
}

/// The raw values of one metric set: every eligible TTFT and every latency.
#[derive(Debug, Default)]
pub(super) struct Samples {
    ttft: Vec<i64>,
    latency: Vec<i64>,
}

impl Samples {
    fn push(&mut self, ttft: Option<i64>, latency: i64) {
        if let Some(ttft) = ttft {
            self.ttft.push(ttft);
        }
        self.latency.push(latency);
    }

    fn extend(&mut self, other: Self) {
        self.ttft.extend(other.ttft);
        self.latency.extend(other.latency);
    }

    fn percentiles(mut self) -> Percentiles {
        let [ttft_p50_ms, ttft_p90_ms, ttft_p99_ms] = nearest_ranks(&mut self.ttft);
        let [latency_p50_ms, latency_p90_ms, latency_p99_ms] = nearest_ranks(&mut self.latency);
        Percentiles {
            ttft_p50_ms,
            ttft_p90_ms,
            ttft_p99_ms,
            latency_p50_ms,
            latency_p90_ms,
            latency_p99_ms,
        }
    }
}

/// One slice's raw timing values, by display label and by bucket index.
#[derive(Debug, Default)]
pub(super) struct TimingSamples {
    pub(super) by_label: HashMap<String, Samples>,
    pub(super) by_bucket: BTreeMap<i64, Samples>,
}

impl TimingSamples {
    /// Add another slice's values. Checks `limit` once per label and per
    /// bucket.
    pub(super) fn merge(&mut self, other: Self, limit: &ReadLimit) -> Result<(), QueryError> {
        for (label, samples) in other.by_label {
            limit.check()?;
            match self.by_label.entry(label) {
                Entry::Occupied(mut into) => into.get_mut().extend(samples),
                Entry::Vacant(slot) => {
                    slot.insert(samples);
                }
            }
        }
        for (bucket, samples) in other.by_bucket {
            limit.check()?;
            self.by_bucket.entry(bucket).or_default().extend(samples);
        }
        Ok(())
    }

    /// Reduce every value set to its percentiles: per label, per bucket of
    /// the `bucket_count`-long grid (an empty bucket has none), and over the
    /// whole window. A bucket index outside the grid is refused, as the fold
    /// refuses one.
    pub(super) fn finish(
        self,
        bucket_count: Option<usize>,
        limit: &ReadLimit,
    ) -> Result<TimingRanks, QueryError> {
        let mut totals = Samples::default();
        let mut groups = HashMap::with_capacity(self.by_label.len());
        for (label, samples) in self.by_label {
            limit.check()?;
            totals.ttft.extend_from_slice(&samples.ttft);
            totals.latency.extend_from_slice(&samples.latency);
            groups.insert(label, samples.percentiles());
        }
        limit.check()?;
        let totals = totals.percentiles();
        let mut by_bucket = self.by_bucket;
        let mut buckets = Vec::with_capacity(bucket_count.unwrap_or(0));
        for index in 0..bucket_count.unwrap_or(0) {
            limit.check()?;
            let samples = by_bucket.remove(&(index as i64)).unwrap_or_default();
            buckets.push(samples.percentiles());
        }
        if !by_bucket.is_empty() {
            return Err(QueryError::InvalidBucket);
        }
        Ok(TimingRanks {
            groups,
            totals,
            buckets,
        })
    }
}

/// The six timing percentiles of one metric set, ms. Each is absent when the
/// set holds no eligible value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Percentiles {
    pub(super) ttft_p50_ms: Option<i64>,
    pub(super) ttft_p90_ms: Option<i64>,
    pub(super) ttft_p99_ms: Option<i64>,
    pub(super) latency_p50_ms: Option<i64>,
    pub(super) latency_p90_ms: Option<i64>,
    pub(super) latency_p99_ms: Option<i64>,
}

/// The timing percentiles of a grouped read: per group label, over the
/// window, and per series bucket in grid order (empty without a series).
#[derive(Debug, Default)]
pub(super) struct TimingRanks {
    pub(super) groups: HashMap<String, Percentiles>,
    pub(super) totals: Percentiles,
    pub(super) buckets: Vec<Percentiles>,
}

/// Nearest-rank p50, p90 and p99 of `values` (rank `ceil(q * n)`,
/// 1-indexed), reordering it in place; all absent when it is empty. Selects
/// the highest rank first, so each lower rank is selected within the part
/// already known to hold it.
fn nearest_ranks(values: &mut [i64]) -> [Option<i64>; 3] {
    let n = values.len();
    if n == 0 {
        return [None; 3];
    }
    let index = |percent: usize| (percent * n).div_ceil(100).max(1) - 1;
    let (i50, i90, i99) = (index(50), index(90), index(99));
    let p99 = *values.select_nth_unstable(i99).1;
    let p90 = *values[..=i99].select_nth_unstable(i90).1;
    let p50 = *values[..=i90].select_nth_unstable(i50).1;
    [Some(p50), Some(p90), Some(p99)]
}

/// Read the timing values of `window` for `group_by`, each split further by
/// bucket index when `bucket_width_ms` is set. `entry_accounts` labels a
/// seat-grouped row with no persisted seat, as [`QuerySpec::entry_accounts`]
/// does for the aggregate.
///
/// [`QuerySpec::entry_accounts`]: super::grouped::QuerySpec::entry_accounts
pub(super) fn read_timings(
    tx: &Transaction<'_>,
    window: &ReadWindow<'_>,
    group_by: GroupDim,
    entry_accounts: &BTreeMap<String, String>,
    bucket_width_ms: Option<i64>,
    limit: &ReadLimit,
) -> Result<TimingSamples, QueryError> {
    let (unbucketed, bucketed) = statements(group_by);
    let mut out = TimingSamples::default();
    let mut stmt;
    let mut rows = match bucket_width_ms {
        None => {
            stmt = tx.prepare(unbucketed)?;
            stmt.query(window.params())?
        }
        Some(width_ms) => {
            stmt = tx.prepare(bucketed)?;
            stmt.query(window.bucketed_params(&width_ms))?
        }
    };
    let series = bucket_width_ms.is_some();
    while let Some(row) = rows.next()? {
        limit.check()?;
        let label = row_label(row.get_ref(0)?, row.get_ref(1)?, entry_accounts)?;
        let ttft: Option<i64> = row.get(3)?;
        let latency: i64 = row.get(4)?;
        let samples = match out.by_label.get_mut(label) {
            Some(samples) => samples,
            None => out.by_label.entry(label.to_string()).or_default(),
        };
        samples.push(ttft, latency);
        if series {
            let bucket: i64 = row.get(2)?;
            out.by_bucket.entry(bucket).or_default().push(ttft, latency);
        }
    }
    Ok(out)
}

#[cfg(test)]
#[path = "timings_tests.rs"]
mod tests;
