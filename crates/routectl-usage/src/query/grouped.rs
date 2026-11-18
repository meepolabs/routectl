//! Grouped, priced, deadline-bounded aggregate query over the usage ledger.
//!
//! One SQL statement reads the ledger at the finest cost-relevant grain
//! (`model, provider, upstream, alias, provider_kind, seat` -- the kind is part
//! of the grain because reasoning pricing depends on the structure the row was
//! written under, not on current config, and the seat so a seat grouping reads
//! the persisted column); the fold in this module prices each
//! fine row through a caller-supplied closure BEFORE the upstream dimension is
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

use super::aggregate::{
    ClassesByRow, QUERY_AGG_SQL, SERIES_AGG_SQL, SideReadWindow, map_fine_row,
    map_fine_row_bucketed, read_error_classes,
};
use super::cache_lifetimes::{attach, read_lifetimes};
use super::deadline::{CancelFlag, DeadlineGuard, ReadLimit};
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
    /// The credential identity the row was served under, read from the
    /// persisted `seat` column.
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

    /// The label a fine row rolls up under for this grouping; a NULL grouping
    /// column reads as `(unattributed)`.
    pub fn label(self, key: &GroupKey) -> String {
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
/// pricing closure. The three states are exclusive and drive [`CostStatus`].
///
/// The label lists name the dimensions the row used that had no rate, in the
/// caller's vocabulary; they feed [`QueryMetrics::missing_price_dimensions`].
#[derive(Debug, Clone, PartialEq)]
pub enum RowCost {
    /// A managed-subscription row: real usage, no per-token dollar cost, plus
    /// the API-equivalent value of that usage when the caller could resolve
    /// EVERY dimension the row used, and the used dimensions it could not. The
    /// value feeds [`QueryMetrics::equivalent_cost_usd`] only -- never
    /// `cost_usd`, which stays real spend.
    Subscription(Option<f64>, Vec<String>),
    /// A priced row and its dollar cost.
    Priced(f64),
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
/// zero". The `*_avg_ms` figures are request-weighted means and the
/// `*_max_ms` figures observed maxima; no field is a percentile.
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
    /// Mean end-to-end latency over every request, ms.
    pub latency_avg_ms: Option<i64>,
    /// Observed maximum end-to-end latency, ms.
    pub latency_max_ms: Option<i64>,
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
    /// rates a priced row would have used. Absent when no subscription row
    /// resolved a complete rate set -- never `0`, and never summed with
    /// `cost_usd`: this is notional replacement value, not spend.
    pub equivalent_cost_usd: Option<f64>,
    /// How completely the cost could be resolved.
    pub cost_status: CostStatus,
    /// The dimensions this metric set's rows used that had no rate, sorted
    /// and deduplicated: the reason a cost or an API-equivalent value is
    /// absent or partial. Empty when every used dimension was priced.
    pub missing_price_dimensions: Vec<String>,
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
/// comes from one snapshot. `limit` is a [`ReadLimit`] or a bare deadline
/// [`Instant`](std::time::Instant). A progress handler checks it every few
/// thousand VM instructions and interrupts the statement once the deadline has
/// passed or the cancel flag is set, surfacing as [`QueryError::Interrupted`];
/// a [`DeadlineGuard`] removes the handler on every exit path -- return, error,
/// and unwind alike -- so the connection is left as it was found. The Rust
/// loops over each statement's rows also check the cancel flag once per row,
/// since rows already computed (a grouped result, the `price` closure's own
/// work) run no VM instructions for the handler to count.
///
/// `price` is called once per FINE row -- while `upstream` is still known --
/// and its verdict is folded into the coarse group the row belongs to. Every
/// row it sees carries its per-lifetime cache writes
/// ([`AggRow::cache_write_lifetimes`] is `Some`); on the series path those are
/// the writes of that row's bucket alone.
pub fn query(
    db: &mut UsageDb,
    spec: &QuerySpec,
    price: impl Fn(&AggRow) -> RowCost,
    limit: impl Into<ReadLimit>,
) -> Result<QueryResult, QueryError> {
    let limit = limit.into();
    let cancel = limit.cancel.clone();
    cancel.check()?;
    // Held across the read + fold so the handler is detached even when the
    // `price` closure panics: the connection outlives this call, and a stale
    // expired deadline left on it would spuriously interrupt the next statement
    // run on it.
    let guard = DeadlineGuard::install(db, limit)?;
    read_and_fold(&guard, spec, price, &cancel)
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
    cancel: &CancelFlag,
) -> Result<QueryResult, QueryError> {
    if let Some(bucket) = spec.bucket {
        check_bucket(bucket, spec.from_ms)?;
    }
    let mut groups: BTreeMap<String, GroupAcc> = BTreeMap::new();
    let mut buckets: BTreeMap<i64, GroupAcc> = BTreeMap::new();

    let tx = db.conn().unchecked_transaction()?;
    match spec.bucket {
        None => fold_groups(&tx, spec, &price, cancel, &mut groups)?,
        Some(bucket) => {
            fold_groups_and_buckets(&tx, spec, bucket, &price, cancel, &mut groups, &mut buckets)?
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
    cancel: &CancelFlag,
    groups: &mut BTreeMap<String, GroupAcc>,
) -> Result<(), QueryError> {
    let window = side_read_window(spec);
    let mut lifetimes = read_lifetimes(tx, &window, None, cancel)?;
    let mut classes = read_error_classes(tx, &window, None, cancel)?;
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
        cancel.check()?;
        let mut fine = row?;
        attach(&mut fine.agg, 0, &mut lifetimes);
        let row_classes = take_classes(&fine.agg.key, 0, &mut classes);
        let cost = price(&fine.agg);
        let label = spec.group_by.label(&fine.agg.key);
        groups
            .entry(label)
            .or_default()
            .add(&fine, &cost, &row_classes);
    }
    debug_assert_classes_attached(&classes);
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
    cancel: &CancelFlag,
    groups: &mut BTreeMap<String, GroupAcc>,
    buckets: &mut BTreeMap<i64, GroupAcc>,
) -> Result<(), QueryError> {
    let window = side_read_window(spec);
    let mut lifetimes = read_lifetimes(tx, &window, Some(bucket.width_ms), cancel)?;
    let mut classes = read_error_classes(tx, &window, Some(bucket.width_ms), cancel)?;
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
        cancel.check()?;
        let (mut fine, bucket_ix) = row?;
        if bucket_ix < 0 || bucket_ix >= bucket.count as i64 {
            return Err(QueryError::InvalidBucket);
        }
        attach(&mut fine.agg, bucket_ix, &mut lifetimes);
        let row_classes = take_classes(&fine.agg.key, bucket_ix, &mut classes);
        let cost = price(&fine.agg);
        let label = spec.group_by.label(&fine.agg.key);
        groups
            .entry(label)
            .or_default()
            .add(&fine, &cost, &row_classes);
        buckets
            .entry(bucket_ix)
            .or_default()
            .add(&fine, &cost, &row_classes);
    }
    debug_assert_classes_attached(&classes);
    Ok(())
}

/// The error-class counts of one fine row in one bucket, taken out of
/// `classes`; empty when the row has no error.
fn take_classes(key: &GroupKey, bucket: i64, classes: &mut ClassesByRow) -> BTreeMap<String, i64> {
    classes.remove(&(key.clone(), bucket)).unwrap_or_default()
}

/// Every error row belongs to a fine row read under the same predicate in the
/// same snapshot, so a class count left unattached means the two statements
/// disagree on the window, a filter or the grain: a reader bug, caught in tests.
fn debug_assert_classes_attached(classes: &ClassesByRow) {
    debug_assert!(
        classes.is_empty(),
        "error-class rows matched no fine row: {classes:?}"
    );
}

/// The window and filters of `spec`, as the side reads bind them.
fn side_read_window(spec: &QuerySpec) -> SideReadWindow<'_> {
    SideReadWindow {
        from_ms: spec.from_ms,
        to_ms: spec.to_ms,
        alias_filter: spec.alias_filter.as_deref(),
        provider_filter: spec.provider_filter.as_deref(),
    }
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
    any_subscription: bool,
    any_unpriced: bool,
    missing_price_dimensions: BTreeSet<String>,
}

impl GroupAcc {
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
                self.missing_price_dimensions
                    .extend(missing.iter().cloned());
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
            RowCost::Unpriced(missing) => {
                self.any_unpriced = true;
                self.missing_price_dimensions
                    .extend(missing.iter().cloned());
            }
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
        self.any_subscription |= other.any_subscription;
        self.any_unpriced |= other.any_unpriced;
        self.missing_price_dimensions
            .extend(other.missing_price_dimensions.iter().cloned());
    }

    /// Resolve the derived metrics and the cost tri-state. Every division is
    /// guarded by its own eligible-row count, so a zero denominator yields
    /// `None` instead of a divide or a misleading zero.
    fn finish(self) -> QueryMetrics {
        let (cost_usd, cost_status) = self.cost();
        let equivalent_cost_usd = self.equivalent_cost();
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
            latency_avg_ms: mean_i64(self.latency_sum, self.requests),
            latency_max_ms: self.latency_max,
            throughput_tok_s: ratio(self.gen_output_tokens as f64 * 1000.0, self.gen_window_ms),
            ctx_avg: mean_i64(self.ctx_sum, self.ctx_count),
            ctx_peak: self.ctx_max,
            cache_hit_pct: ratio(self.cache_hit_num as f64 * 100.0, self.cache_hit_den),
            cost_usd,
            equivalent_cost_usd,
            cost_status,
            missing_price_dimensions: self.missing_price_dimensions.into_iter().collect(),
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
