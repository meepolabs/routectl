//! Grouped, priced, deadline-bounded aggregate query over the usage ledger.
//!
//! One SQL statement reads the ledger at the finest cost-relevant grain
//! (`model, provider, upstream, alias, provider_kind` -- the kind is part of the
//! grain because reasoning pricing depends on the structure the row was written
//! under, not on current config); the fold in this module prices each
//! fine row through a caller-supplied closure BEFORE the upstream dimension is
//! discarded, rolls the priced fine rows up to the requested coarse dimension,
//! and derives the display metrics. Totals are folded from the SAME per-group
//! accumulators rather than from a second SQL pass: a grouped SELECT over an
//! empty window returns zero ROWS, while an ungrouped one returns a single
//! all-NULL row, so accumulating locally removes that failure class entirely.
//!
//! Cost never enters this crate as a dependency -- only as the closure's
//! [`RowCost`] verdict, which keeps `routectl-usage` a leaf.

use std::collections::BTreeMap;
use std::time::Instant;

use serde::Serialize;

use crate::db::UsageDb;

use super::aggregate::{QUERY_AGG_SQL, SERIES_AGG_SQL, map_fine_row, map_fine_row_bucketed};
use super::deadline::DeadlineGuard;
use super::{AggRow, GroupKey, QueryError};

/// The most buckets a series may carry. The caller resolves the bucket width so
/// its count fits under this cap; this crate re-checks it as its own trust
/// boundary, since an unbounded count would densify an unbounded vector.
const SERIES_BUCKET_CAP: usize = 1000;

/// Label for a group whose grouping column is NULL (no target was dispatched).
const UNATTRIBUTED: &str = "(unattributed)";

/// A resolved time-bucket grid: uniform-width buckets anchored at
/// [`QuerySpec::from_ms`].
///
/// The caller owns granularity resolution (calendar edges, widening under the
/// bucket cap) and passes the ALREADY-RESOLVED grid in. Both fields are
/// re-validated on the query path -- `width_ms` must be strictly positive, since
/// SQLite's integer division by zero is a silent NULL rather than an error, and
/// `count` must fall in `1..=1000`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BucketSpec {
    /// The bucket width in milliseconds. Strictly positive.
    pub width_ms: i64,
    /// How many consecutive buckets the series covers, starting at the window's
    /// lower bound. Every one of them is emitted, traffic or not.
    pub count: usize,
}

/// A dense time series over the same rows the groups were folded from.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QuerySeries {
    /// The resolved bucket width in milliseconds. A reader draws each bucket at
    /// its own `start_ms` rather than assuming this stride.
    pub bucket_ms: i64,
    /// One entry per bucket in ascending time order, including the buckets that
    /// saw no traffic.
    pub buckets: Vec<SeriesBucket>,
}

/// One time bucket: when it starts, and the metrics of the rows inside it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SeriesBucket {
    /// Inclusive lower edge of the bucket, epoch-millis UTC.
    pub start_ms: i64,
    /// The bucket's display figures, in the same shape a group carries. A
    /// zero-traffic bucket reports `requests: 0`, every derived metric absent,
    /// and `cost_status: unpriced`.
    pub metrics: QueryMetrics,
}

/// What to read, how to group it, and how to narrow it.
///
/// The window is half-open: `[from_ms, to_ms)`. Both filters are matched as
/// BIND VALUES against a fixed statement -- no identifier or predicate is ever
/// interpolated into SQL.
#[derive(Debug, Clone)]
pub struct QuerySpec {
    /// Inclusive lower bound of the window, epoch-millis UTC.
    pub from_ms: i64,
    /// Exclusive upper bound of the window, epoch-millis UTC.
    pub to_ms: i64,
    /// The dimension the fine rows are rolled up to.
    pub group_by: GroupDim,
    /// Restrict to one routing alias. `None` matches every alias.
    pub alias_filter: Option<String>,
    /// Restrict to one served provider. `None` matches every provider.
    pub provider_filter: Option<String>,
    /// Also fold the same rows into a time series over this bucket grid. `None`
    /// asks for no series at all, and reads the ledger exactly as it would
    /// without this field.
    pub bucket: Option<BucketSpec>,
}

/// The dimension a result set is grouped by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupDim {
    /// Served model nickname (coalesced to the requested model).
    Model,
    /// Served provider name.
    Provider,
    /// Resolved routing alias.
    Alias,
}

/// The cost verdict for one fine-grained [`AggRow`], as decided by the caller's
/// pricing closure. The three states are exclusive and drive [`CostStatus`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RowCost {
    /// A managed-subscription row: real usage, no per-token dollar cost, plus
    /// the API-equivalent value of that usage when the caller could resolve
    /// EVERY dimension the row used. The payload feeds
    /// [`QueryMetrics::equivalent_cost_usd`] only -- never `cost_usd`, which
    /// stays real spend.
    Subscription(Option<f64>),
    /// A priced row and its dollar cost.
    Priced(f64),
    /// A row with no usable price: counts toward usage, contributes no cost.
    Unpriced,
}

/// How completely a group's cost could be resolved.
///
/// The serialized form is the lowercase token [`CostStatus::as_str`] returns;
/// those four tokens are a wire contract, so the rename and `as_str` must stay
/// in lockstep.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CostStatus {
    /// Every row was priced; `cost_usd` is the group's full cost.
    Priced,
    /// Every row was unpriced; `cost_usd` is absent (never a misleading 0). A
    /// metric set with no rows at all also reads as unpriced, since nothing was
    /// priced -- claiming a cost of zero would be a stronger statement than the
    /// data supports.
    #[default]
    Unpriced,
    /// Every row was a subscription row; `cost_usd` is absent.
    Subscription,
    /// The group mixes cost kinds; `cost_usd` is the PRICED SUBTOTAL only, and
    /// is absent when the mix carries no priced row at all.
    Partial,
}

impl CostStatus {
    /// The wire token for this status.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Priced => "priced",
            Self::Unpriced => "unpriced",
            Self::Subscription => "subscription",
            Self::Partial => "partial",
        }
    }
}

/// Every display figure for one group (or for the window totals).
///
/// The additive fields are cross-row sums or counts. The derived fields are
/// `Option` and are `None` -- never `0` -- when the group holds no row eligible
/// to contribute to them, so "no data" is always distinguishable from "measured
/// zero". `ttft_p50_ms` and `latency_p50_ms` are request-weighted MEANS, an
/// approximation of the median; `*_p95_ms` are the observed maxima.
///
/// Every field name here IS the wire vocabulary: an absent `Option` serializes
/// as an explicit `null` (never skipped) so a reader can tell "no eligible rows"
/// from a measured `0`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct QueryMetrics {
    /// Total requests.
    pub requests: i64,
    /// Requests whose outcome was `ok`.
    pub ok: i64,
    /// Requests whose outcome was an error, excluding `client_disconnect`.
    pub errors: i64,
    /// Summed input tokens.
    pub input_tokens: i64,
    /// Summed output tokens.
    pub output_tokens: i64,
    /// Summed reasoning tokens.
    pub reasoning_tokens: i64,
    /// Summed billed cache-read volume (a per-turn flow, hence a sum).
    pub cache_read_billed: i64,
    /// Summed 5-minute cache-write tokens.
    pub cache_write_5m: i64,
    /// Summed 1-hour cache-write tokens.
    pub cache_write_1h: i64,
    /// Total server-tool invocations.
    pub server_tool_calls: i64,
    /// Count of streaming requests.
    pub stream_count: i64,
    /// Count of rows whose terminal outcome was `client_disconnect`.
    pub client_disconnect_total: i64,
    /// Count of requests served only after a fallback (`fallback_count > 0`).
    pub fallback_served: i64,
    /// Approximate p50 time-to-first-token (request-weighted mean) over
    /// streaming successes, ms. Same population as `ttft_p95_ms`.
    pub ttft_p50_ms: Option<i64>,
    /// Observed maximum time-to-first-token over streaming successes, ms.
    pub ttft_p95_ms: Option<i64>,
    /// Approximate p50 end-to-end latency (request-weighted mean), ms.
    pub latency_p50_ms: Option<i64>,
    /// Observed maximum end-to-end latency, ms.
    pub latency_p95_ms: Option<i64>,
    /// Request-weighted mean generation throughput, output tokens/second.
    pub throughput_tok_s: Option<f64>,
    /// Mean reported input-context size, tokens.
    pub ctx_avg: Option<i64>,
    /// Peak reported input-context size, tokens.
    pub ctx_peak: Option<i64>,
    /// Request-weighted mean share of prompt tokens served from cache, percent.
    pub cache_hit_pct: Option<f64>,
    /// Dollar cost, per [`CostStatus`]. Absent whenever no priced row
    /// contributed.
    pub cost_usd: Option<f64>,
    /// API-equivalent USD value of this metric set's SUBSCRIPTION rows, at the
    /// rates a priced row would have used. Absent when no subscription row
    /// resolved a complete rate set -- never `0`, and never summed with
    /// `cost_usd`: this is notional replacement value, not spend.
    pub equivalent_cost_usd: Option<f64>,
    /// How completely the cost could be resolved.
    pub cost_status: CostStatus,
}

/// One coarse group: its label under the requested [`GroupDim`], plus its
/// metrics.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueryGroup {
    /// The grouping column's value, or `(unattributed)` when it is NULL.
    pub label: String,
    /// The group's display figures.
    pub metrics: QueryMetrics,
}

/// Window-wide totals. Identical in shape to a group's metrics by
/// construction: they are folded from the same per-group accumulators, so
/// totals and groups can never disagree.
pub type QueryTotals = QueryMetrics;

/// A grouped query's result: the coarse groups in label order, plus the totals
/// folded from them.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueryResult {
    /// Coarse groups, ordered by label.
    pub groups: Vec<QueryGroup>,
    /// Window-wide totals across every group.
    pub totals: QueryTotals,
    /// The time series over the same rows, present exactly when
    /// [`QuerySpec::bucket`] asked for one. Serialized as an explicit `null`
    /// otherwise, never skipped.
    pub series: Option<QuerySeries>,
}

/// Run one windowed, filtered, priced, grouped aggregate.
///
/// The whole read happens inside a single deferred transaction, so every figure
/// comes from one snapshot. A progress handler checks `deadline` every few
/// thousand VM instructions and interrupts the statement once it has passed,
/// surfacing as [`QueryError::Interrupted`]; a [`DeadlineGuard`] removes the
/// handler on every exit path -- return, error, and unwind alike -- so the
/// connection is left as it was found.
///
/// `price` is called once per FINE row -- while `upstream` is still known --
/// and its verdict is folded into the coarse group the row belongs to.
pub fn query(
    db: &UsageDb,
    spec: &QuerySpec,
    price: impl Fn(&AggRow) -> RowCost,
    deadline: Instant,
) -> Result<QueryResult, QueryError> {
    // Held across the read + fold so the handler is detached even when the
    // `price` closure panics: the connection outlives this call, and a stale
    // expired deadline left on it would spuriously interrupt the next statement
    // run on it.
    let _guard = DeadlineGuard::install(db, deadline)?;
    read_and_fold(db, spec, price)
}

/// The read + fold, split out so [`query`] can scope its progress guard around
/// the whole of it.
///
/// Both paths read ONE statement and fold it. On the series path that single
/// scan feeds two accumulator maps at once: a second bucket-only statement would
/// re-scan the ledger for a strict subset of rows already in hand, and each row
/// is priced exactly once for both folds, so groups, totals and series reconcile
/// by construction.
fn read_and_fold(
    db: &UsageDb,
    spec: &QuerySpec,
    price: impl Fn(&AggRow) -> RowCost,
) -> Result<QueryResult, QueryError> {
    if let Some(bucket) = spec.bucket {
        check_bucket(bucket, spec.from_ms)?;
    }
    let mut groups: BTreeMap<String, GroupAcc> = BTreeMap::new();
    let mut buckets: BTreeMap<i64, GroupAcc> = BTreeMap::new();

    let tx = db.conn().unchecked_transaction()?;
    match spec.bucket {
        None => fold_groups(&tx, spec, &price, &mut groups)?,
        Some(bucket) => {
            fold_groups_and_buckets(&tx, spec, bucket, &price, &mut groups, &mut buckets)?
        }
    }
    // A read-only transaction has nothing to commit; rolling back releases the
    // snapshot without touching the DB.
    tx.finish()?;

    let mut totals = GroupAcc::default();
    for acc in groups.values() {
        totals.merge(acc);
    }
    let series = spec
        .bucket
        .map(|bucket| densify(bucket, spec.from_ms, buckets));
    let groups = groups
        .into_iter()
        .map(|(label, acc)| QueryGroup {
            label,
            metrics: acc.finish(),
        })
        .collect();
    Ok(QueryResult {
        groups,
        totals: totals.finish(),
        series,
    })
}

/// Re-check the caller's bucket-grid invariants at the crate boundary.
///
/// A non-positive width would divide by zero in SQL, which SQLite answers with a
/// silent NULL bucket index rather than an error, and an out-of-range count
/// would densify an unbounded vector. The last bucket's start is computed in
/// i128 so a width-times-count product that would not fit an `i64` is refused
/// here rather than overflowing in [`densify`], whose arithmetic is then
/// provably in range. All three are the caller's contract to uphold, but a
/// violation is answered with an error rather than an assertion: this fold is
/// network-reachable and the release profile aborts on panic.
const fn check_bucket(bucket: BucketSpec, anchor_ms: i64) -> Result<(), QueryError> {
    if bucket.width_ms <= 0 || bucket.count == 0 || bucket.count > SERIES_BUCKET_CAP {
        return Err(QueryError::InvalidBucket);
    }
    let last_start = anchor_ms as i128 + (bucket.count as i128 - 1) * bucket.width_ms as i128;
    if last_start > i64::MAX as i128 {
        return Err(QueryError::InvalidBucket);
    }
    Ok(())
}

/// Fold the windowed fine rows into the coarse groups.
fn fold_groups(
    tx: &rusqlite::Transaction<'_>,
    spec: &QuerySpec,
    price: &impl Fn(&AggRow) -> RowCost,
    groups: &mut BTreeMap<String, GroupAcc>,
) -> Result<(), QueryError> {
    let mut stmt = tx.prepare(QUERY_AGG_SQL)?;
    let rows = stmt.query_map(
        rusqlite::params![
            spec.from_ms,
            spec.to_ms,
            spec.alias_filter.as_deref(),
            spec.provider_filter.as_deref(),
        ],
        map_fine_row,
    )?;
    for row in rows {
        let fine = row?;
        let cost = price(&fine.agg);
        let label = group_label(&fine.agg.key, spec.group_by);
        groups.entry(label).or_default().add(&fine, cost);
    }
    Ok(())
}

/// Fold the windowed fine-and-bucketed rows into the coarse groups AND the
/// per-bucket accumulators, in one scan. Each map discards the dimension the
/// other keeps.
///
/// A row whose bucket index falls outside the grid is refused before it reaches
/// EITHER accumulator: [`densify`] emits only `0..count`, so counting such a row
/// in the groups and totals while dropping it from the series would make the two
/// folds disagree silently. Erroring keeps the guarantee absolute -- either every
/// counted row is representable in the series, or the query fails.
fn fold_groups_and_buckets(
    tx: &rusqlite::Transaction<'_>,
    spec: &QuerySpec,
    bucket: BucketSpec,
    price: &impl Fn(&AggRow) -> RowCost,
    groups: &mut BTreeMap<String, GroupAcc>,
    buckets: &mut BTreeMap<i64, GroupAcc>,
) -> Result<(), QueryError> {
    let mut stmt = tx.prepare(SERIES_AGG_SQL)?;
    let rows = stmt.query_map(
        rusqlite::params![
            spec.from_ms,
            spec.to_ms,
            spec.alias_filter.as_deref(),
            spec.provider_filter.as_deref(),
            bucket.width_ms,
        ],
        map_fine_row_bucketed,
    )?;
    for row in rows {
        let (fine, bucket_ix) = row?;
        if bucket_ix < 0 || bucket_ix >= bucket.count as i64 {
            return Err(QueryError::InvalidBucket);
        }
        let cost = price(&fine.agg);
        let label = group_label(&fine.agg.key, spec.group_by);
        groups.entry(label).or_default().add(&fine, cost);
        buckets.entry(bucket_ix).or_default().add(&fine, cost);
    }
    Ok(())
}

/// Resolve the per-bucket accumulators into a dense series: one entry per
/// bucket in the grid, in ascending time order, whether or not it saw traffic.
/// An absent bucket resolves through the SAME `finish` an empty group would, so
/// a zero-traffic bucket reports `requests: 0` with every derived metric absent
/// rather than a fabricated measurement.
///
/// A window that matched NO row at all short-circuits that fill and yields an
/// EMPTY series: it has nothing to plot, and up to a thousand zero buckets would
/// present an empty ledger as a measured flat line.
///
/// The `start_ms` arithmetic is plain i64 because [`check_bucket`] has already
/// rejected any grid whose last bucket start would not fit one.
fn densify(
    bucket: BucketSpec,
    anchor_ms: i64,
    mut buckets: BTreeMap<i64, GroupAcc>,
) -> QuerySeries {
    let filled = if buckets.is_empty() {
        Vec::new()
    } else {
        (0..bucket.count)
            .map(|i| SeriesBucket {
                start_ms: anchor_ms + i as i64 * bucket.width_ms,
                metrics: buckets.remove(&(i as i64)).unwrap_or_default().finish(),
            })
            .collect()
    };
    QuerySeries {
        bucket_ms: bucket.width_ms,
        buckets: filled,
    }
}

/// The label a fine row rolls up under for the requested dimension.
fn group_label(key: &GroupKey, dim: GroupDim) -> String {
    match dim {
        GroupDim::Model => key
            .model
            .clone()
            .unwrap_or_else(|| UNATTRIBUTED.to_string()),
        GroupDim::Provider => key
            .provider
            .clone()
            .unwrap_or_else(|| UNATTRIBUTED.to_string()),
        GroupDim::Alias => key.alias.clone(),
    }
}

/// Rollup state for one coarse group.
///
/// Sums and counts are additive; the maxima take a MAX ACROSS the fine rows'
/// maxima. Ratios are never accumulated as ratios -- each keeps its numerator
/// and denominator separately, so no mean is ever taken of a mean.
#[derive(Default)]
struct GroupAcc {
    requests: i64,
    ok: i64,
    errors: i64,
    input_tokens: i64,
    output_tokens: i64,
    reasoning_tokens: i64,
    cache_read_billed: i64,
    cache_write_5m: i64,
    cache_write_1h: i64,
    server_tool_calls: i64,
    stream_count: i64,
    client_disconnect_total: i64,
    fallback_served: i64,
    ttft_sum: i64,
    ttft_count: i64,
    ttfb_max: Option<i64>,
    latency_sum: i64,
    latency_max: Option<i64>,
    input_tokens_max: Option<i64>,
    input_tokens_present: i64,
    tok_s_sum: f64,
    tok_s_count: i64,
    cache_hit_sum: f64,
    cache_hit_count: i64,
    priced_usd: f64,
    any_priced: bool,
    /// Summed API-equivalent value of the subscription rows that resolved one.
    /// Kept strictly apart from `priced_usd` so a notional dollar can never
    /// reach the real-spend channel.
    equivalent_usd: f64,
    any_equivalent: bool,
    any_subscription: bool,
    any_unpriced: bool,
}

impl GroupAcc {
    /// Fold one priced fine row in.
    fn add(&mut self, fine: &super::aggregate::FineRow, cost: RowCost) {
        let agg = &fine.agg;
        self.requests += agg.requests;
        self.ok += agg.ok;
        self.errors += agg.errors;
        self.input_tokens += agg.input_tokens;
        self.output_tokens += agg.output_tokens;
        self.reasoning_tokens += agg.reasoning_tokens;
        self.cache_read_billed += agg.cache_read_billed;
        self.cache_write_5m += agg.cache_write_5m;
        self.cache_write_1h += agg.cache_write_1h;
        self.server_tool_calls += agg.server_tool_calls;
        self.stream_count += agg.stream_count;
        self.client_disconnect_total += agg.client_disconnect_total;
        self.fallback_served += fine.fallback_served;
        self.ttft_sum += fine.ttft_sum;
        self.ttft_count += fine.ttft_count;
        self.ttfb_max = max_opt(self.ttfb_max, fine.ttfb_max);
        self.latency_sum += fine.latency_sum;
        self.latency_max = max_opt(self.latency_max, fine.latency_max);
        self.input_tokens_max = max_opt(self.input_tokens_max, fine.input_tokens_max);
        self.input_tokens_present += fine.input_tokens_present;
        self.tok_s_sum += fine.tok_s_sum;
        self.tok_s_count += fine.tok_s_count;
        self.cache_hit_sum += fine.cache_hit_sum;
        self.cache_hit_count += fine.cache_hit_count;
        match cost {
            RowCost::Subscription(equivalent) => {
                self.any_subscription = true;
                if let Some(usd) = equivalent {
                    // No finiteness assertion here: the equivalent channel
                    // degrades a non-finite sum to absent in
                    // `equivalent_cost`, and a debug panic on the way in would
                    // fire before that degrade could run.
                    self.equivalent_usd += usd;
                    self.any_equivalent = true;
                }
            }
            RowCost::Priced(usd) => {
                // A per-row price is finite by construction (rate validation
                // rejects non-finite rates), so a non-finite one is a pricer
                // bug, not operator input -- caught in tests, tolerated in
                // release, where the sum's own finiteness check absorbs it.
                debug_assert!(usd.is_finite(), "a resolved row price must be finite");
                self.priced_usd += usd;
                self.any_priced = true;
            }
            RowCost::Unpriced => self.any_unpriced = true,
        }
    }

    /// Fold a finished group's accumulator into the window totals. Operates on
    /// the RAW numerators, not on another group's derived metrics.
    fn merge(&mut self, other: &Self) {
        self.requests += other.requests;
        self.ok += other.ok;
        self.errors += other.errors;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_tokens += other.reasoning_tokens;
        self.cache_read_billed += other.cache_read_billed;
        self.cache_write_5m += other.cache_write_5m;
        self.cache_write_1h += other.cache_write_1h;
        self.server_tool_calls += other.server_tool_calls;
        self.stream_count += other.stream_count;
        self.client_disconnect_total += other.client_disconnect_total;
        self.fallback_served += other.fallback_served;
        self.ttft_sum += other.ttft_sum;
        self.ttft_count += other.ttft_count;
        self.ttfb_max = max_opt(self.ttfb_max, other.ttfb_max);
        self.latency_sum += other.latency_sum;
        self.latency_max = max_opt(self.latency_max, other.latency_max);
        self.input_tokens_max = max_opt(self.input_tokens_max, other.input_tokens_max);
        self.input_tokens_present += other.input_tokens_present;
        self.tok_s_sum += other.tok_s_sum;
        self.tok_s_count += other.tok_s_count;
        self.cache_hit_sum += other.cache_hit_sum;
        self.cache_hit_count += other.cache_hit_count;
        self.priced_usd += other.priced_usd;
        self.any_priced |= other.any_priced;
        self.equivalent_usd += other.equivalent_usd;
        self.any_equivalent |= other.any_equivalent;
        self.any_subscription |= other.any_subscription;
        self.any_unpriced |= other.any_unpriced;
    }

    /// Resolve the derived metrics and the cost tri-state. Every division is
    /// guarded by its own eligible-row count, so a zero denominator yields
    /// `None` instead of a divide or a misleading zero.
    fn finish(self) -> QueryMetrics {
        let (cost_usd, cost_status) = self.cost();
        QueryMetrics {
            requests: self.requests,
            ok: self.ok,
            errors: self.errors,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            reasoning_tokens: self.reasoning_tokens,
            cache_read_billed: self.cache_read_billed,
            cache_write_5m: self.cache_write_5m,
            cache_write_1h: self.cache_write_1h,
            server_tool_calls: self.server_tool_calls,
            stream_count: self.stream_count,
            client_disconnect_total: self.client_disconnect_total,
            fallback_served: self.fallback_served,
            ttft_p50_ms: mean_i64(self.ttft_sum, self.ttft_count),
            ttft_p95_ms: self.ttfb_max,
            latency_p50_ms: mean_i64(self.latency_sum, self.requests),
            latency_p95_ms: self.latency_max,
            throughput_tok_s: mean_f64(self.tok_s_sum, self.tok_s_count),
            ctx_avg: mean_i64(self.input_tokens, self.input_tokens_present),
            ctx_peak: self.input_tokens_max,
            cache_hit_pct: mean_f64(self.cache_hit_sum, self.cache_hit_count).map(|r| r * 100.0),
            cost_usd,
            equivalent_cost_usd: self.equivalent_cost(),
            cost_status,
        }
    }

    /// The group's API-equivalent subscription value, absent when no
    /// subscription row resolved one.
    ///
    /// A non-finite sum degrades to absent for the same reason the priced sum
    /// does: this fold is network-reachable and the release profile aborts on
    /// panic. The degrade is scoped to THIS channel -- an overflowed equivalent
    /// leaves `cost_usd` and `cost_status` untouched.
    fn equivalent_cost(&self) -> Option<f64> {
        self.any_equivalent
            .then_some(self.equivalent_usd)
            .filter(|usd| usd.is_finite())
    }

    /// The group's cost and its status. A group with no cost signal at all
    /// (no rows) reads as `unpriced` with no amount.
    ///
    /// A non-finite total is a VALUE outcome, not a panic: this fold is
    /// reachable from the network and the release profile aborts on panic, so a
    /// cost sum that an extreme-magnitude configured rate overflowed to `inf`
    /// degrades to "no usable price" rather than taking the process down.
    fn cost(&self) -> (Option<f64>, CostStatus) {
        let priced = self.any_priced.then_some(self.priced_usd);
        if let Some(usd) = priced
            && !usd.is_finite()
        {
            return (None, CostStatus::Unpriced);
        }
        let kinds = i32::from(self.any_priced)
            + i32::from(self.any_subscription)
            + i32::from(self.any_unpriced);
        if kinds > 1 {
            return (priced, CostStatus::Partial);
        }
        if self.any_priced {
            return (priced, CostStatus::Priced);
        }
        if self.any_subscription {
            return (None, CostStatus::Subscription);
        }
        (None, CostStatus::Unpriced)
    }
}

/// `MAX` across two optional maxima: present beats absent.
fn max_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    }
}

/// `sum / count` as an integer mean, or `None` when nothing was eligible.
const fn mean_i64(sum: i64, count: i64) -> Option<i64> {
    if count > 0 { Some(sum / count) } else { None }
}

/// `sum / count` as a float mean, or `None` when nothing was eligible.
fn mean_f64(sum: f64, count: i64) -> Option<f64> {
    if count > 0 {
        Some(sum / count as f64)
    } else {
        None
    }
}

#[cfg(test)]
#[path = "grouped_tests.rs"]
mod tests;
