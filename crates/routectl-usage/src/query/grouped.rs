//! Grouped, priced, deadline-bounded aggregate query over the usage ledger.
//!
//! The read (see the `slices` module) returns the window's rows at the finest
//! cost-relevant grain (`model, provider, upstream, alias, provider_kind,
//! seat` -- the kind is part of the grain because reasoning pricing depends on
//! the structure the row was written under, not on current config, and the
//! seat so a seat grouping reads the persisted column), with each fine row's
//! cache-write lifetimes and error classes beside it; the fold in this module
//! prices each fine row through a caller-supplied closure BEFORE the upstream dimension is
//! discarded, rolls the priced fine rows up to the requested coarse dimension,
//! and derives the display metrics. Totals are folded from the SAME per-group
//! accumulators rather than from a second SQL pass: a grouped SELECT over an
//! empty window returns zero ROWS, while an ungrouped one returns a single
//! all-NULL row, so accumulating locally removes that failure class entirely.
//!
//! Cost never enters this crate as a dependency -- only as the closure's
//! [`RowCost`] verdict, which keeps `routectl-usage` a leaf.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::db::UsageDb;

use super::aggregate::ClassesByRow;
use super::cache_lifetimes::attach;
use super::deadline::{DeadlineGuard, ReadLimit};
use super::slices::{MergedRead, ReadPlan, WindowRows, read_merged};
use super::timings::{Percentiles, TimingRanks};
use super::{AggRow, GroupKey, QueryError};

/// The most buckets a series may carry. The caller resolves the bucket width so
/// its count fits under this cap; this crate re-checks it as its own trust
/// boundary, since an unbounded count would densify an unbounded vector.
const SERIES_BUCKET_CAP: usize = 1000;

/// Label for a group whose grouping column is NULL (no target was dispatched).
pub const UNATTRIBUTED: &str = "(unattributed)";

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
/// The window is half-open: `[from_ms, to_ms)`. Every filter is matched as
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
    /// Restrict to one fine model label: the served model, or the requested
    /// model on a row never dispatched. `None` matches every model.
    pub model_filter: Option<String>,
    /// Also fold the same rows into a time series over this bucket grid. `None`
    /// asks for no series at all, and reads the ledger exactly as it would
    /// without this field.
    pub bucket: Option<BucketSpec>,
    /// Provider entry -> credential account, for the entries whose account is
    /// the entry itself (an API key or a cloud credential). Read only by the
    /// seat grouping, and only for a row with no persisted seat; empty leaves
    /// every such row `(unattributed)`.
    pub entry_accounts: BTreeMap<String, String>,
}

impl QuerySpec {
    /// The label a fine row rolls up under for this spec's grouping: the one
    /// rule every reader of a grouped result labels its rows by.
    ///
    /// Under the seat grouping a persisted seat always wins; a row with none
    /// takes the account its served provider entry maps to in
    /// [`Self::entry_accounts`]; anything else reads as `(unattributed)`.
    /// Every other grouping reads its own column and ignores the map.
    #[must_use]
    pub fn group_label(&self, key: &GroupKey) -> String {
        if self.group_by == GroupDim::Seat
            && key.seat.is_none()
            && let Some(account) = key
                .provider
                .as_ref()
                .and_then(|entry| self.entry_accounts.get(entry))
        {
            return account.clone();
        }
        self.group_by.label(key)
    }
}

/// The dimension a result set is grouped by: the one list of groupings every
/// usage reader offers.
///
/// The serialized form is the lowercase token [`GroupDim::as_str`] returns;
/// the serde rename and `as_str` must stay in lockstep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupDim {
    /// Served model nickname (coalesced to the requested model).
    Model,
    /// Resolved routing alias.
    Alias,
    /// Served provider name.
    Provider,
    /// The credential account the row was served under: the persisted
    /// `seat` column, or for a row without one, the account of its served
    /// provider entry (see [`QuerySpec::group_label`]).
    Seat,
}

impl GroupDim {
    /// Every grouping, in display order.
    pub const ALL: [Self; 4] = [Self::Model, Self::Alias, Self::Provider, Self::Seat];

    /// The wire token for this grouping.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Alias => "alias",
            Self::Provider => "provider",
            Self::Seat => "seat",
        }
    }

    /// The grouping whose token is `token`, or `None` for an unknown token.
    pub fn parse(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|dim| dim.as_str() == token)
    }

    /// The grouping column of `key`, or `(unattributed)` when it is NULL.
    /// Readers label rows through [`QuerySpec::group_label`].
    pub(crate) fn label(self, key: &GroupKey) -> String {
        let column = match self {
            Self::Model => key.model.as_deref(),
            Self::Alias => Some(key.alias.as_str()),
            Self::Provider => key.provider.as_deref(),
            Self::Seat => key.seat.as_deref(),
        };
        column.unwrap_or(UNATTRIBUTED).to_string()
    }
}

/// The cost verdict for one fine-grained [`AggRow`], as decided by the caller's
/// pricing closure. The four states are exclusive and drive [`CostStatus`].
///
/// The label lists name the dimensions the row used that had no rate, in the
/// caller's vocabulary; they feed [`QueryMetrics::missing_prices`] and
/// [`QueryMetrics::missing_price_dimensions`].
#[derive(Debug, Clone, PartialEq)]
pub enum RowCost {
    /// A managed-subscription row: real usage, no per-token dollar cost, plus
    /// the API-equivalent value of that usage, and the used dimensions with no
    /// rate. The value is either the whole row's or, when the labels are not
    /// empty, a covered subtotal of the dimensions that had a rate; a row with
    /// labels marks its set's [`QueryMetrics::equivalent_status`] partial,
    /// so a reader never needs to infer completeness from the value being
    /// present. The value feeds [`QueryMetrics::equivalent_cost_usd`] only --
    /// never `cost_usd`, which stays real spend.
    Subscription(Option<f64>, Vec<String>),
    /// A priced row and its dollar cost.
    Priced(f64),
    /// A row priced only in part: the dollar cost of the dimensions that had
    /// a rate, and the used dimensions that did not. The covered cost feeds
    /// `cost_usd` like a priced row; the row also counts as unpriced, so its
    /// metric set reads [`CostStatus::Partial`].
    PricedPart(f64, Vec<String>),
    /// A row with no usable price, and the used dimensions that had no rate:
    /// counts toward usage, contributes no cost.
    Unpriced(Vec<String>),
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
    /// The group mixes cost kinds, or holds a row priced only in part;
    /// `cost_usd` is the PRICED SUBTOTAL only (a part-priced row adds its
    /// covered cost), and is absent when nothing in the group was priced.
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

/// How completely a metric set's API-equivalent value covers its subscription
/// rows.
///
/// The serialized form is the lowercase token [`EquivalentStatus::as_str`]
/// returns; those three tokens are a wire contract, so the renames and
/// `as_str` must stay in lockstep.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub enum EquivalentStatus {
    /// The set holds no subscription row, so there is nothing to value.
    #[default]
    #[serde(rename = "none")]
    NoSubscription,
    /// No subscription row named a dimension with no rate;
    /// `equivalent_cost_usd` is the whole value (absent only when no row used
    /// a dimension, or the sum overflowed).
    #[serde(rename = "complete")]
    Complete,
    /// Some subscription row named a dimension with no rate:
    /// `equivalent_cost_usd` is a subtotal of the rows and dimensions that had
    /// one, and absent when none did.
    #[serde(rename = "partial")]
    Partial,
}

impl EquivalentStatus {
    /// The wire token for this status.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoSubscription => "none",
            Self::Complete => "complete",
            Self::Partial => "partial",
        }
    }
}

/// Every display figure for one group (or for the window totals).
///
/// The additive fields are cross-row sums or counts. The derived fields are
/// `Option` and are `None` -- never `0` -- when the group holds no row eligible
/// to contribute to them, so "no data" is always distinguishable from "measured
/// zero". The `*_avg_ms` figures are request-weighted means, the `*_max_ms`
/// figures observed maxima, and the `*_p50_ms` / `*_p90_ms` / `*_p99_ms`
/// figures exact nearest-rank percentiles over the same populations as the
/// mean beside them.
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
    /// Requests whose outcome was an error, excluding `client_disconnect`
    /// and `cancelled`.
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
    /// Summed cache-write tokens across both ledger eras (see
    /// [`AggRow::cache_write_tokens`]).
    pub cache_write_tokens: i64,
    /// Count of requests reporting a cache-write value, a reported zero
    /// included.
    pub cache_write_present: i64,
    /// Count of requests reporting a cache-read value, a reported zero
    /// included.
    pub cache_read_present: i64,
    /// Total server-tool invocations.
    pub server_tool_calls: i64,
    /// Count of requests reporting a server-tool usage map.
    pub server_tool_present: i64,
    /// Count of streaming requests.
    pub stream_count: i64,
    /// Count of rows whose terminal outcome was `client_disconnect`.
    pub client_disconnect_total: i64,
    /// The part of `client_disconnect_total` that hung up before a served
    /// model was recorded.
    pub client_disconnect_pre_dispatch: i64,
    /// Count of rows whose terminal outcome was `cancelled`.
    pub cancelled_total: i64,
    /// Count of requests served only after a fallback (`fallback_count > 0`).
    pub fallback_served: i64,
    /// `errors` split by failure class: class token -> count, summing exactly
    /// to `errors`. An error row with no recorded class counts as
    /// `unclassified`; a token this build does not know stays its own key.
    /// Empty (`{}`) when there are no errors.
    pub errors_by_class: BTreeMap<String, i64>,
    /// Mean time-to-first-token over streaming successes, ms. Same
    /// population as `ttft_max_ms`.
    pub ttft_avg_ms: Option<i64>,
    /// Observed maximum time-to-first-token over streaming successes, ms.
    pub ttft_max_ms: Option<i64>,
    /// Median time-to-first-token over streaming successes, ms (nearest
    /// rank). Same population as `ttft_avg_ms`.
    pub ttft_p50_ms: Option<i64>,
    /// 90th-percentile time-to-first-token over streaming successes, ms.
    pub ttft_p90_ms: Option<i64>,
    /// 99th-percentile time-to-first-token over streaming successes, ms.
    pub ttft_p99_ms: Option<i64>,
    /// Mean end-to-end latency over every request, ms.
    pub latency_avg_ms: Option<i64>,
    /// Observed maximum end-to-end latency, ms.
    pub latency_max_ms: Option<i64>,
    /// Median end-to-end latency over every request, ms (nearest rank). Same
    /// population as `latency_avg_ms`.
    pub latency_p50_ms: Option<i64>,
    /// 90th-percentile end-to-end latency over every request, ms.
    pub latency_p90_ms: Option<i64>,
    /// 99th-percentile end-to-end latency over every request, ms.
    pub latency_p99_ms: Option<i64>,
    /// Generation throughput, output tokens/second: the output tokens of
    /// streaming successes over their summed generation time (latency after
    /// the first token). Absent when no generation time was measured.
    pub throughput_tok_s: Option<f64>,
    /// Mean per-request context, tokens: fresh input plus cache read plus
    /// cache write, over requests reporting any of the three.
    pub ctx_avg: Option<i64>,
    /// Largest per-request context, tokens, under the same rule as `ctx_avg`.
    pub ctx_peak: Option<i64>,
    /// Share of prompt tokens served from cache, percent, token-weighted:
    /// summed cache reads over summed input + cache read + cache write, both
    /// taken over the fine rows that report cache reads. Absent when that
    /// prompt total is zero.
    pub cache_hit_pct: Option<f64>,
    /// Dollar cost, per [`CostStatus`]. Absent whenever no priced row
    /// contributed.
    pub cost_usd: Option<f64>,
    /// API-equivalent USD value of this metric set's SUBSCRIPTION rows, at the
    /// rates a priced row would have used. A present value may be a subtotal:
    /// read [`Self::equivalent_status`] for whether it covers every
    /// subscription row, never the value's presence. Absent when no
    /// subscription row was valued -- never `0`, and never summed with
    /// `cost_usd`: this is notional replacement value, not spend.
    pub equivalent_cost_usd: Option<f64>,
    /// How completely `equivalent_cost_usd` covers the set's subscription
    /// rows.
    pub equivalent_status: EquivalentStatus,
    /// How completely the cost could be resolved.
    pub cost_status: CostStatus,
    /// The dimensions this metric set's rows used that had no rate, sorted
    /// and deduplicated: the reason a cost or an API-equivalent value is
    /// absent or partial. Empty when every used dimension was priced. Always
    /// the distinct `dimension` values of [`Self::missing_prices`].
    pub missing_price_dimensions: Vec<String>,
    /// The distinct (fine model, dimension) pairs behind
    /// [`Self::missing_price_dimensions`], sorted by model then dimension.
    pub missing_prices: Vec<MissingPrice>,
    /// Summed cache-write tokens with no reported lifetime (see
    /// [`super::CacheWriteLifetimes::unknown_tokens`]). A ledger row whose
    /// lifetime split sums above its total adds nothing.
    pub cache_write_unknown_tokens: i64,
}

/// One dimension a model's rows used that had no rate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct MissingPrice {
    /// The fine model label the row was served under (the label the model
    /// grouping reads), whatever the metric set is grouped by.
    pub model: String,
    /// The missing dimension, in the pricer's vocabulary.
    pub dimension: String,
    /// The configuration key that would price the dimension. This crate does
    /// not own configuration and always leaves it `None`; a caller that does
    /// fills it in.
    pub config_key: Option<String>,
}

/// One coarse group: its label under the requested [`GroupDim`], plus its
/// metrics.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueryGroup {
    /// The grouping column's value, or `(unattributed)` when it is NULL.
    pub label: String,
    /// The group's display figures.
    pub metrics: QueryMetrics,
    /// Requests per served fine model (the model label the model grouping
    /// reads), over the rows that recorded a served provider. `Some` only
    /// under the alias grouping, where `requests` minus the map's sum is the
    /// group's requests that were never dispatched. Not part of the wire
    /// shape.
    #[serde(skip)]
    pub requests_by_model: Option<BTreeMap<String, i64>>,
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
    /// otherwise, never skipped. The groups and totals are the same with or
    /// without it.
    ///
    /// A bucket's figures are priced and derived over that bucket's rows
    /// alone. The additive counts of the buckets sum to the totals, but the
    /// costs and the cache-hit share need not: a fine row unpriced over the
    /// window (a cache write of unknown lifetime) can be priced in the buckets
    /// that hold none of it, and a row counts toward the hit share in a bucket
    /// only when it reported cache reads there.
    pub series: Option<QuerySeries>,
}

/// Run one windowed, filtered, priced, grouped aggregate.
///
/// The read first takes the ledger's highest rowid as a watermark and every
/// statement counts only rows at or below it, so a row appended during the
/// read is never counted. A window whose span from its earliest row is a week
/// or more is read in parallel `ts_start` slices, each on its own read-only
/// connection and in its own transaction; the slices partition the window, so
/// no row is counted twice. They do not share one snapshot: a retention prune
/// committing while they start can drop rows older than its cutoff from some
/// slices and not others (see the `slices` module).
/// A shorter window is one slice on `db` alone, one snapshot.
///
/// `limit` is a [`ReadLimit`] or a bare deadline
/// [`Instant`](std::time::Instant). A progress handler on every connection
/// checks it every few thousand VM instructions and interrupts the statement
/// once the deadline has passed or the cancel flag is set, surfacing as
/// [`QueryError::Interrupted`]; a [`DeadlineGuard`] removes the handler from
/// `db` on every exit path -- return, error, and unwind alike -- so the
/// connection is left as it was found. The Rust loops over each statement's
/// rows, the slice merge and the fold also check the whole limit (deadline
/// and flag) once per row, since rows already computed (a grouped result, the
/// `price` closure's own work) run no VM instructions for the handler to
/// count. A slice that fails or panics stops the others; every slice
/// has ended before this returns, the first failure that is not an interrupt
/// is the error, and no partial result is ever returned.
///
/// `price` is called once per FINE row over the whole window -- while
/// `upstream` is still known -- on the calling thread, after the slices are
/// merged, and its verdict is folded into the coarse group the row belongs to.
/// On the series path it is also called once per fine row and bucket, over
/// that bucket's rows alone, for the bucket's figures. Every row it sees
/// carries its per-lifetime cache writes ([`AggRow::cache_write_lifetimes`]
/// is `Some`) for the span it is priced over.
pub fn query(
    db: &mut UsageDb,
    spec: &QuerySpec,
    price: impl Fn(&AggRow) -> RowCost,
    limit: impl Into<ReadLimit>,
) -> Result<QueryResult, QueryError> {
    query_with(db, spec, price, limit.into(), &ReadPlan::AUTO)
}

/// [`query`] under an explicit [`ReadPlan`], so a test can force the slice
/// count, the watermark or a slice's connection.
pub(super) fn query_with(
    db: &mut UsageDb,
    spec: &QuerySpec,
    price: impl Fn(&AggRow) -> RowCost,
    limit: ReadLimit,
    plan: &ReadPlan<'_>,
) -> Result<QueryResult, QueryError> {
    limit.check()?;
    // The read stops its own slices through a child flag, leaving the
    // caller's flag as the caller set it.
    let limit = ReadLimit {
        deadline: limit.deadline,
        cancel: limit.cancel.child(),
    };
    // Held across the read + fold so the handler is detached even when the
    // `price` closure panics: the connection outlives this call, and a stale
    // expired deadline left on it would spuriously interrupt the next statement
    // run on it.
    let guard = DeadlineGuard::install(db, limit.clone())?;
    read_and_fold(&guard, spec, price, &limit, plan)
}

/// The read + fold, split out so [`query_with`] can scope its progress guard
/// around the whole of it.
///
/// The groups and totals are priced over the whole window and each series
/// bucket over itself, so the additive counts reconcile by construction and
/// the priced and ratio figures need not (see [`QueryResult::series`]).
fn read_and_fold(
    db: &UsageDb,
    spec: &QuerySpec,
    price: impl Fn(&AggRow) -> RowCost,
    limit: &ReadLimit,
    plan: &ReadPlan<'_>,
) -> Result<QueryResult, QueryError> {
    if let Some(bucket) = spec.bucket {
        check_bucket(bucket, spec.from_ms)?;
    }
    let mut read = read_merged(db, spec, limit, plan)?;
    let ranks =
        std::mem::take(&mut read.timings).finish(spec.bucket.map(|bucket| bucket.count), limit)?;
    let (groups, buckets) = fold(spec, read, &price, limit)?;

    let mut totals = GroupAcc::default();
    for acc in groups.values() {
        limit.check()?;
        totals.merge(acc);
    }
    let TimingRanks {
        groups: group_ranks,
        totals: total_ranks,
        buckets: bucket_ranks,
    } = ranks;
    let series = spec
        .bucket
        .map(|bucket| densify(bucket, spec.from_ms, buckets, &bucket_ranks));
    let groups = groups
        .into_iter()
        .map(|(label, mut acc)| {
            let ranks = group_ranks.get(&label).copied().unwrap_or_default();
            let requests_by_model = acc.requests_by_model.take();
            QueryGroup {
                label,
                metrics: acc.finish(ranks),
                requests_by_model,
            }
        })
        .collect();
    Ok(QueryResult {
        groups,
        totals: totals.finish(total_ranks),
        series,
    })
}

/// The coarse groups by label and, on the series path, the buckets by index.
type Folded = (BTreeMap<String, GroupAcc>, BTreeMap<i64, GroupAcc>);

/// Fold the merged fine rows into the coarse groups and, on the series path,
/// the buckets.
///
/// A bucket is priced from its own fine rows. The groups are priced from the
/// same fine rows merged across their buckets first, so a group's figures are
/// the whole window's whether or not a series was requested: a row's price and
/// its cache-hit eligibility depend on what it holds over the span it is
/// priced over, not on the sum of its slices.
///
/// A row whose bucket index falls outside the grid is refused before it
/// reaches EITHER accumulator: [`densify`] emits only `0..count`, so counting
/// such a row in the groups and totals while dropping it from the series would
/// make the two folds disagree silently. Erroring keeps the guarantee absolute
/// -- either every counted row is representable in the series, or the query
/// fails.
fn fold(
    spec: &QuerySpec,
    read: MergedRead,
    price: &impl Fn(&AggRow) -> RowCost,
    limit: &ReadLimit,
) -> Result<Folded, QueryError> {
    let MergedRead {
        rows,
        mut lifetimes,
        mut classes,
        timings: _,
    } = read;
    let mut window = WindowRows::default();
    let mut buckets: BTreeMap<i64, GroupAcc> = BTreeMap::new();
    for (mut fine, bucket_ix) in rows {
        limit.check()?;
        if let Some(bucket) = spec.bucket
            && (bucket_ix < 0 || bucket_ix >= bucket.count as i64)
        {
            return Err(QueryError::InvalidBucket);
        }
        attach(&mut fine.agg, bucket_ix, &mut lifetimes);
        let row_classes = take_classes(&fine.agg.key, bucket_ix, &mut classes);
        if spec.bucket.is_some() {
            let cost = price(&fine.agg);
            buckets
                .entry(bucket_ix)
                .or_default()
                .add(&fine, &cost, &row_classes);
        }
        window.add(fine, row_classes);
    }
    debug_assert_classes_attached(&classes);
    let mut groups: BTreeMap<String, GroupAcc> = BTreeMap::new();
    for (fine, row_classes) in window.into_rows() {
        limit.check()?;
        let cost = price(&fine.agg);
        groups
            .entry(spec.group_label(&fine.agg.key))
            .or_insert_with(|| GroupAcc::for_group(spec.group_by))
            .add(&fine, &cost, &row_classes);
    }
    Ok((groups, buckets))
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

/// The error-class counts of one fine row in one bucket, taken out of
/// `classes`; empty when the row has no error.
fn take_classes(key: &GroupKey, bucket: i64, classes: &mut ClassesByRow) -> BTreeMap<String, i64> {
    classes.remove(&(key.clone(), bucket)).unwrap_or_default()
}

/// Every error row belongs to a fine row read under the same predicate, slice
/// and watermark, so a class count left unattached means the two statements
/// disagree on the window, a filter or the grain: a reader bug, caught in tests.
fn debug_assert_classes_attached(classes: &ClassesByRow) {
    debug_assert!(
        classes.is_empty(),
        "error-class rows matched no fine row: {classes:?}"
    );
}

/// Resolve the per-bucket accumulators into a dense series: one entry per
/// bucket in the grid, in ascending time order, whether or not it saw traffic.
/// An absent bucket resolves through the SAME `finish` an empty group would, so
/// a zero-traffic bucket reports `requests: 0` with every derived metric absent
/// rather than a fabricated measurement.
///
/// Each bucket takes its timing percentiles from `ranks`, indexed like the
/// grid.
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
    ranks: &[Percentiles],
) -> QuerySeries {
    let filled = if buckets.is_empty() {
        Vec::new()
    } else {
        (0..bucket.count)
            .map(|i| SeriesBucket {
                start_ms: anchor_ms + i as i64 * bucket.width_ms,
                metrics: buckets
                    .remove(&(i as i64))
                    .unwrap_or_default()
                    .finish(ranks.get(i).copied().unwrap_or_default()),
            })
            .collect()
    };
    QuerySeries {
        bucket_ms: bucket.width_ms,
        buckets: filled,
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
    cache_write_tokens: i64,
    cache_write_present: i64,
    cache_read_present: i64,
    server_tool_calls: i64,
    server_tool_present: i64,
    stream_count: i64,
    client_disconnect_total: i64,
    client_disconnect_pre_dispatch: i64,
    cancelled_total: i64,
    fallback_served: i64,
    errors_by_class: BTreeMap<String, i64>,
    ttft_sum: i64,
    ttft_count: i64,
    ttfb_max: Option<i64>,
    latency_sum: i64,
    latency_max: Option<i64>,
    ctx_max: Option<i64>,
    ctx_sum: i64,
    ctx_count: i64,
    gen_output_tokens: i64,
    gen_window_ms: i64,
    cache_hit_num: i64,
    cache_hit_den: i64,
    priced_usd: f64,
    any_priced: bool,
    /// Summed API-equivalent value of the subscription rows that resolved one.
    /// Kept strictly apart from `priced_usd` so a notional dollar can never
    /// reach the real-spend channel.
    equivalent_usd: f64,
    any_equivalent: bool,
    /// A subscription row named a dimension with no rate, so the summed
    /// equivalent leaves part of the set's usage out.
    any_equivalent_gap: bool,
    any_subscription: bool,
    any_unpriced: bool,
    /// (fine model, dimension) pairs that had no rate.
    missing_prices: BTreeSet<(String, String)>,
    cache_write_unknown_tokens: i64,
    /// Requests per served fine model; `Some` only on a coarse group under
    /// the alias grouping, so bucket and totals accumulators never carry it.
    requests_by_model: Option<BTreeMap<String, i64>>,
}

impl GroupAcc {
    /// The accumulator of one coarse group under `group_by`.
    fn for_group(group_by: GroupDim) -> Self {
        Self {
            requests_by_model: (group_by == GroupDim::Alias).then(BTreeMap::new),
            ..Self::default()
        }
    }

    /// Fold one priced fine row and its error-class counts in.
    fn add(
        &mut self,
        fine: &super::aggregate::FineRow,
        cost: &RowCost,
        classes: &BTreeMap<String, i64>,
    ) {
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
        self.cache_write_tokens += agg.cache_write_tokens;
        self.cache_write_present += agg.cache_write_present;
        self.cache_read_present += agg.cache_read_present;
        self.server_tool_calls += agg.server_tool_calls;
        self.server_tool_present += agg.server_tool_present;
        self.stream_count += agg.stream_count;
        self.client_disconnect_total += agg.client_disconnect_total;
        self.client_disconnect_pre_dispatch += agg.client_disconnect_pre_dispatch;
        self.cancelled_total += agg.cancelled_total;
        self.fallback_served += fine.fallback_served;
        if let Some(lifetimes) = &agg.cache_write_lifetimes {
            self.cache_write_unknown_tokens = self
                .cache_write_unknown_tokens
                .saturating_add(lifetimes.unknown_tokens);
        }
        if let Some(by_model) = &mut self.requests_by_model
            && agg.key.provider.is_some()
        {
            *by_model.entry(GroupDim::Model.label(&agg.key)).or_default() += agg.requests;
        }
        add_classes(&mut self.errors_by_class, classes);
        self.ttft_sum += fine.ttft_sum;
        self.ttft_count += fine.ttft_count;
        self.ttfb_max = max_opt(self.ttfb_max, fine.ttfb_max);
        self.latency_sum += fine.latency_sum;
        self.latency_max = max_opt(self.latency_max, fine.latency_max);
        self.ctx_max = max_opt(self.ctx_max, fine.ctx_max);
        self.ctx_sum += fine.ctx_sum;
        self.ctx_count += fine.ctx_count;
        self.gen_output_tokens += agg.gen_output_tokens;
        self.gen_window_ms += agg.gen_window_ms;
        // The CLI footer's rule: a fine row counts toward the hit share only
        // when it reports cache reads, and then with its whole prompt.
        if agg.cache_read_present > 0 {
            self.cache_hit_num += agg.cache_read_billed;
            self.cache_hit_den += agg.input_tokens + agg.cache_read_billed + agg.cache_write_tokens;
        }
        match cost {
            RowCost::Subscription(equivalent, missing) => {
                self.any_subscription = true;
                self.any_equivalent_gap |= !missing.is_empty();
                self.add_missing(&agg.key, missing);
                if let Some(usd) = *equivalent {
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
                self.priced_usd += *usd;
                self.any_priced = true;
            }
            RowCost::PricedPart(usd, missing) => {
                debug_assert!(usd.is_finite(), "a resolved row price must be finite");
                self.priced_usd += *usd;
                self.any_priced = true;
                self.any_unpriced = true;
                self.add_missing(&agg.key, missing);
            }
            RowCost::Unpriced(missing) => {
                self.any_unpriced = true;
                self.add_missing(&agg.key, missing);
            }
        }
    }

    /// Record the dimensions a fine row under `key` used with no rate.
    fn add_missing(&mut self, key: &GroupKey, missing: &[String]) {
        if missing.is_empty() {
            return;
        }
        let model = GroupDim::Model.label(key);
        self.missing_prices.extend(
            missing
                .iter()
                .map(|dimension| (model.clone(), dimension.clone())),
        );
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
        self.cache_write_tokens += other.cache_write_tokens;
        self.cache_write_present += other.cache_write_present;
        self.cache_read_present += other.cache_read_present;
        self.server_tool_calls += other.server_tool_calls;
        self.server_tool_present += other.server_tool_present;
        self.stream_count += other.stream_count;
        self.client_disconnect_total += other.client_disconnect_total;
        self.client_disconnect_pre_dispatch += other.client_disconnect_pre_dispatch;
        self.cancelled_total += other.cancelled_total;
        self.fallback_served += other.fallback_served;
        add_classes(&mut self.errors_by_class, &other.errors_by_class);
        self.ttft_sum += other.ttft_sum;
        self.ttft_count += other.ttft_count;
        self.ttfb_max = max_opt(self.ttfb_max, other.ttfb_max);
        self.latency_sum += other.latency_sum;
        self.latency_max = max_opt(self.latency_max, other.latency_max);
        self.ctx_max = max_opt(self.ctx_max, other.ctx_max);
        self.ctx_sum += other.ctx_sum;
        self.ctx_count += other.ctx_count;
        self.gen_output_tokens += other.gen_output_tokens;
        self.gen_window_ms += other.gen_window_ms;
        self.cache_hit_num += other.cache_hit_num;
        self.cache_hit_den += other.cache_hit_den;
        self.priced_usd += other.priced_usd;
        self.any_priced |= other.any_priced;
        self.equivalent_usd += other.equivalent_usd;
        self.any_equivalent |= other.any_equivalent;
        self.any_equivalent_gap |= other.any_equivalent_gap;
        self.any_subscription |= other.any_subscription;
        self.any_unpriced |= other.any_unpriced;
        self.missing_prices
            .extend(other.missing_prices.iter().cloned());
        self.cache_write_unknown_tokens = self
            .cache_write_unknown_tokens
            .saturating_add(other.cache_write_unknown_tokens);
    }

    /// Resolve the derived metrics and the cost tri-state, with the set's
    /// timing percentiles `ranks` read beside the aggregate. Every division is
    /// guarded by its own eligible-row count, so a zero denominator yields
    /// `None` instead of a divide or a misleading zero.
    fn finish(self, ranks: Percentiles) -> QueryMetrics {
        let (cost_usd, cost_status) = self.cost();
        let equivalent_cost_usd = self.equivalent_cost();
        let equivalent_status = self.equivalent_status();
        let (missing_price_dimensions, missing_prices) = missing_price_fields(self.missing_prices);
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
            cache_write_tokens: self.cache_write_tokens,
            cache_write_present: self.cache_write_present,
            cache_read_present: self.cache_read_present,
            server_tool_calls: self.server_tool_calls,
            server_tool_present: self.server_tool_present,
            stream_count: self.stream_count,
            client_disconnect_total: self.client_disconnect_total,
            client_disconnect_pre_dispatch: self.client_disconnect_pre_dispatch,
            cancelled_total: self.cancelled_total,
            fallback_served: self.fallback_served,
            errors_by_class: self.errors_by_class,
            ttft_avg_ms: mean_i64(self.ttft_sum, self.ttft_count),
            ttft_max_ms: self.ttfb_max,
            ttft_p50_ms: ranks.ttft_p50_ms,
            ttft_p90_ms: ranks.ttft_p90_ms,
            ttft_p99_ms: ranks.ttft_p99_ms,
            latency_avg_ms: mean_i64(self.latency_sum, self.requests),
            latency_max_ms: self.latency_max,
            latency_p50_ms: ranks.latency_p50_ms,
            latency_p90_ms: ranks.latency_p90_ms,
            latency_p99_ms: ranks.latency_p99_ms,
            throughput_tok_s: ratio(self.gen_output_tokens as f64 * 1000.0, self.gen_window_ms),
            ctx_avg: mean_i64(self.ctx_sum, self.ctx_count),
            ctx_peak: self.ctx_max,
            cache_hit_pct: ratio(self.cache_hit_num as f64 * 100.0, self.cache_hit_den),
            cost_usd,
            equivalent_cost_usd,
            equivalent_status,
            cost_status,
            missing_price_dimensions,
            missing_prices,
            cache_write_unknown_tokens: self.cache_write_unknown_tokens,
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

    /// How completely the group's equivalent covers its subscription rows.
    const fn equivalent_status(&self) -> EquivalentStatus {
        if !self.any_subscription {
            return EquivalentStatus::NoSubscription;
        }
        if self.any_equivalent_gap {
            return EquivalentStatus::Partial;
        }
        EquivalentStatus::Complete
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

/// The sorted distinct dimensions of `pairs`, and the pairs themselves as
/// [`MissingPrice`] entries in (model, dimension) order.
fn missing_price_fields(pairs: BTreeSet<(String, String)>) -> (Vec<String>, Vec<MissingPrice>) {
    let dimensions: BTreeSet<String> = pairs.iter().map(|(_, dim)| dim.clone()).collect();
    let prices = pairs
        .into_iter()
        .map(|(model, dimension)| MissingPrice {
            model,
            dimension,
            config_key: None,
        })
        .collect();
    (dimensions.into_iter().collect(), prices)
}

/// Add each class count of `from` into `into`.
fn add_classes(into: &mut BTreeMap<String, i64>, from: &BTreeMap<String, i64>) {
    for (class, count) in from {
        *into.entry(class.clone()).or_default() += count;
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

/// `num / den` as a float, or `None` when the denominator is not positive.
fn ratio(num: f64, den: i64) -> Option<f64> {
    if den > 0 {
        Some(num / den as f64)
    } else {
        None
    }
}

#[cfg(test)]
#[path = "grouped_tests.rs"]
mod tests;
