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

/// The one statement shape: the display label first, then the bucket index,
/// the TTFT of an eligible row (NULL otherwise) and the latency, for every
/// inference row of the window. Every column it reads is in the covering
/// aggregate index.
macro_rules! timings_sql {
    ($label:literal, $bucket:literal) => {
        concat!(
            "SELECT ",
            $label,
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
/// series aggregate groups on (`?6` the width, `?7` the anchor).
macro_rules! timings_sql_pair {
    ($label:literal) => {
        (
            timings_sql!($label, "0"),
            timings_sql!($label, "(ts_start - ?7) / ?6"),
        )
    };
}

/// The `(unbucketed, bucketed)` statements of `group_by`. The label column is
/// the column [`GroupDim::label`] reads from the fine key.
pub(super) const fn statements(group_by: GroupDim) -> (&'static str, &'static str) {
    match group_by {
        GroupDim::Model => timings_sql_pair!("COALESCE(model, requested_model)"),
        GroupDim::Alias => timings_sql_pair!("alias"),
        GroupDim::Provider => timings_sql_pair!("provider"),
        GroupDim::Seat => timings_sql_pair!("seat"),
    }
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
/// bucket index when `bucket_width_ms` is set.
pub(super) fn read_timings(
    tx: &Transaction<'_>,
    window: &ReadWindow<'_>,
    group_by: GroupDim,
    bucket_width_ms: Option<i64>,
    limit: &ReadLimit,
) -> Result<TimingSamples, QueryError> {
    let (unbucketed, bucketed) = statements(group_by);
    let mut out = TimingSamples::default();
    let mut stmt;
    let mut rows = match bucket_width_ms {
        None => {
            stmt = tx.prepare(unbucketed)?;
            stmt.query(rusqlite::params![
                window.from_ms,
                window.to_ms,
                window.alias_filter,
                window.provider_filter,
                window.watermark,
            ])?
        }
        Some(width_ms) => {
            stmt = tx.prepare(bucketed)?;
            stmt.query(rusqlite::params![
                window.from_ms,
                window.to_ms,
                window.alias_filter,
                window.provider_filter,
                window.watermark,
                width_ms,
                window.anchor_ms,
            ])?
        }
    };
    let series = bucket_width_ms.is_some();
    while let Some(row) = rows.next()? {
        limit.check()?;
        let label = match row.get_ref(0)? {
            ValueRef::Text(text) => std::str::from_utf8(text).map_err(rusqlite::Error::from)?,
            _ => UNATTRIBUTED,
        };
        let ttft: Option<i64> = row.get(2)?;
        let latency: i64 = row.get(3)?;
        let samples = match out.by_label.get_mut(label) {
            Some(samples) => samples,
            None => out.by_label.entry(label.to_string()).or_default(),
        };
        samples.push(ttft, latency);
        if series {
            let bucket: i64 = row.get(1)?;
            out.by_bucket.entry(bucket).or_default().push(ttft, latency);
        }
    }
    Ok(out)
}

#[cfg(test)]
#[path = "timings_tests.rs"]
mod tests;
