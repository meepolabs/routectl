//! Grouped, priced, deadline-bounded aggregate query over the usage ledger.
//!
//! One SQL statement reads the ledger at the finest cost-relevant grain
//! (`model, provider, upstream, alias`); the fold in this module prices each
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

use super::aggregate::{QUERY_AGG_SQL, map_fine_row};
use super::{AggRow, GroupKey, QueryError};

/// How often (in SQLite VM instructions) the deadline is re-checked. Small
/// enough that a runaway scan is cut short promptly, large enough that the
/// callback is not a measurable share of the query's own work.
const PROGRESS_OPS: i32 = 10_000;

/// Label for a group whose grouping column is NULL (no target was dispatched).
const UNATTRIBUTED: &str = "(unattributed)";

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
    /// A managed-subscription row: real usage, no per-token dollar cost.
    Subscription,
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
}

/// Run one windowed, filtered, priced, grouped aggregate.
///
/// The whole read happens inside a single deferred transaction, so every figure
/// comes from one snapshot. A progress handler checks `deadline` every few
/// thousand VM instructions and interrupts the statement once it has passed,
/// surfacing as [`QueryError::Interrupted`]; a [`ProgressGuard`] removes the
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
    let conn = db.conn();
    conn.progress_handler(PROGRESS_OPS, Some(move || Instant::now() > deadline))?;
    // Held across the read + fold so the handler is detached even when the
    // `price` closure panics: the connection outlives this call, and a stale
    // expired deadline left on it would spuriously interrupt the next statement
    // run on it.
    let _guard = ProgressGuard { conn };
    read_and_fold(db, spec, price)
}

/// Detaches [`query`]'s progress handler on every exit path, unwinding
/// included.
struct ProgressGuard<'a> {
    conn: &'a rusqlite::Connection,
}

impl Drop for ProgressGuard<'_> {
    fn drop(&mut self) {
        // A detach failure never masks the read's outcome -- the read is what
        // the caller asked for, and a handler that could not be removed is not
        // the caller's problem to interpret.
        let _ = self
            .conn
            .progress_handler(PROGRESS_OPS, None::<fn() -> bool>);
    }
}

/// The read + fold, split out so [`query`] can scope its progress guard around
/// the whole of it.
fn read_and_fold(
    db: &UsageDb,
    spec: &QuerySpec,
    price: impl Fn(&AggRow) -> RowCost,
) -> Result<QueryResult, QueryError> {
    let tx = db.conn().unchecked_transaction().map_err(classify)?;
    let mut groups: BTreeMap<String, GroupAcc> = BTreeMap::new();
    {
        let mut stmt = tx.prepare(QUERY_AGG_SQL).map_err(classify)?;
        let rows = stmt
            .query_map(
                rusqlite::params![
                    spec.from_ms,
                    spec.to_ms,
                    spec.alias_filter.as_deref(),
                    spec.provider_filter.as_deref(),
                ],
                map_fine_row,
            )
            .map_err(classify)?;
        for row in rows {
            let fine = row.map_err(classify)?;
            let cost = price(&fine.agg);
            let label = group_label(&fine.agg.key, spec.group_by);
            groups.entry(label).or_default().add(&fine, cost);
        }
    }
    // A read-only transaction has nothing to commit; rolling back releases the
    // snapshot without touching the DB.
    tx.finish().map_err(classify)?;

    let mut totals = GroupAcc::default();
    for acc in groups.values() {
        totals.merge(acc);
    }
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
    })
}

/// Map a SQLite failure to [`QueryError`], separating a deadline interrupt from
/// every other cause so the shell can shed it under its own code.
fn classify(err: rusqlite::Error) -> QueryError {
    if err.sqlite_error_code() == Some(rusqlite::ErrorCode::OperationInterrupted) {
        return QueryError::Interrupted;
    }
    QueryError::Sqlite(err)
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
            RowCost::Subscription => self.any_subscription = true,
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
            ttft_p50_ms: mean_i64(self.ttft_sum, self.ttft_count),
            ttft_p95_ms: self.ttfb_max,
            latency_p50_ms: mean_i64(self.latency_sum, self.requests),
            latency_p95_ms: self.latency_max,
            throughput_tok_s: mean_f64(self.tok_s_sum, self.tok_s_count),
            ctx_avg: mean_i64(self.input_tokens, self.input_tokens_present),
            ctx_peak: self.input_tokens_max,
            cache_hit_pct: mean_f64(self.cache_hit_sum, self.cache_hit_count).map(|r| r * 100.0),
            cost_usd,
            cost_status,
        }
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
