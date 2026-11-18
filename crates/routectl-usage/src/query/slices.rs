//! The read half of the grouped query: a rowid watermark, then the window's
//! fine rows and side maps, read in up to [`SLICES`] parallel `ts_start`
//! slices and merged by fine group and bucket.
//!
//! A long window is the expensive case: one statement walks every index entry
//! in it and sorts them all to group them, and on a ledger whose index pages
//! are scattered on disk the walk is a long run of small random reads. Equal
//! sub-ranges read on their own connections keep several of those reads in
//! flight at once and split the sort.
//!
//! Every slice runs the same three statements over its own sub-range, in its
//! own transaction, all bound to one watermark taken before any slice starts.
//! What that guarantees, given how `requests` changes (rows are appended, a
//! row is written when its request completes so a late row can carry an old
//! `ts_start`, and the only delete is the retention prune of rows older than
//! a cutoff):
//!
//! - A row appended after the watermark is counted by no slice and plays no
//!   part in how the window is cut.
//! - The slices partition the window by `ts_start`, so no row is ever counted
//!   twice.
//! - The slices do NOT share one snapshot. A retention prune that commits
//!   while the slices are starting is seen by the slices whose transactions
//!   begin after it and not by those that began before, so the answer can
//!   count a set between the pre-prune and post-prune ledgers. Only rows
//!   older than the prune's cutoff can differ, and those lie in the oldest
//!   slices of a window that reaches back past the retention horizon.
//!
//! Slices return raw, unpriced fine rows; the merge sums them per
//! `(fine group, bucket)` and the caller prices and folds the merged rows, so a
//! figure never depends on how the window was cut.

use std::collections::hash_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::thread::{Builder, Scope, ScopedJoinHandle};

use crate::db::{OpenError, UsageDb, open_readonly_fastfail};

use super::aggregate::{BucketedFineRow, ClassesByRow, FineRow, ReadWindow, read_error_classes};
use super::aggregate::{earliest_ts_start_below, read_fine_rows};
use super::cache_lifetimes::{LifetimesByRow, merge_lifetimes, read_lifetimes};
use super::deadline::{CancelFlag, DeadlineGuard, ReadLimit};
use super::grouped::QuerySpec;
use super::{AggRow, GroupKey, QueryError};

/// How many slices a long window is read in. Four keeps the 5M-row all-time
/// read well inside the query budget on one core each; more buys speed the
/// budget does not need at the cost of more cores and connections.
const SLICES: usize = 4;

/// The shortest effective span that is sliced: shorter windows answer well
/// inside the budget on one connection, where slicing would only multiply the
/// side reads.
const SLICE_MIN_SPAN_MS: i64 = 7 * 86_400_000;

/// Opens the connection one slice after the first reads on. Takes the slice's
/// index and the read's limit.
pub(super) type OpenSlice<'a> =
    &'a (dyn Fn(usize, &ReadLimit) -> Result<UsageDb, QueryError> + Sync);

/// How a read is cut and bounded. [`ReadPlan::AUTO`] is the only plan outside
/// tests: slices chosen from the effective span, the watermark read from the
/// ledger, slices opened read-only beside the caller's connection.
pub(super) struct ReadPlan<'a> {
    /// Read in exactly this many slices, whatever the span.
    pub slices: Option<usize>,
    /// Count only rows at or below this rowid.
    pub watermark: Option<i64>,
    /// Open the connections of slices after the first.
    pub open_slice: Option<OpenSlice<'a>>,
}

impl ReadPlan<'_> {
    pub(super) const AUTO: ReadPlan<'static> = ReadPlan {
        slices: None,
        watermark: None,
        open_slice: None,
    };
}

/// One slice's raw read.
pub(super) struct SliceRows {
    pub fine: Vec<BucketedFineRow>,
    pub lifetimes: LifetimesByRow,
    pub classes: ClassesByRow,
}

/// The whole window's rows, merged across slices: one fine row per
/// `(fine group, bucket)` in a fixed order, and the side maps keyed the same
/// way.
pub(super) struct MergedRead {
    pub rows: Vec<BucketedFineRow>,
    pub lifetimes: LifetimesByRow,
    pub classes: ClassesByRow,
}

/// Read `spec`'s window on `db` (whose read limit is already installed) and on
/// up to [`SLICES`] - 1 further connections, and merge the slices.
///
/// The first slice that fails or panics cancels its siblings; every slice is
/// joined before this returns, and the first error that is not an interrupt
/// is the answer (an interrupt when every failure was one). A slice's panic
/// is re-raised on the calling thread once every slice has ended. No partial
/// result is ever returned.
pub(super) fn read_merged(
    db: &UsageDb,
    spec: &QuerySpec,
    limit: &ReadLimit,
    plan: &ReadPlan<'_>,
) -> Result<MergedRead, QueryError> {
    let watermark = match plan.watermark {
        Some(watermark) => watermark,
        None => read_watermark(db)?,
    };
    let ranges = plan_ranges(db, spec, watermark, plan.slices)?;
    let path = db.path().to_path_buf();
    let open_fastfail = |_: usize, _: &ReadLimit| {
        open_readonly_fastfail(&path).map_err(|err| slice_open_error(&err))
    };
    let slices = SliceJob {
        spec,
        watermark,
        limit,
        open: plan.open_slice.unwrap_or(&open_fastfail),
    };
    let results = slices.run(db, &ranges);
    merge(first_failure(results)?, limit)
}

/// The ledger's highest rowid, 0 when it is empty. Over every row, not only
/// inference rows: it bounds which rows exist, and the statements it is bound
/// into apply their own population predicate.
const WATERMARK_SQL: &str = "SELECT COALESCE(MAX(rowid), 0) FROM requests";

/// The highest rowid in the ledger, 0 when it is empty.
fn read_watermark(db: &UsageDb) -> Result<i64, QueryError> {
    let watermark = db.conn().query_row(WATERMARK_SQL, [], |row| row.get(0))?;
    Ok(watermark)
}

/// The `[from, to)` ranges `spec`'s window is read in. The earliest-row probe
/// runs only when the window is long enough for its answer to matter, and
/// sees only the rows at or below `watermark`, the rows the slices count.
fn plan_ranges(
    db: &UsageDb,
    spec: &QuerySpec,
    watermark: i64,
    forced: Option<usize>,
) -> Result<Vec<(i64, i64)>, QueryError> {
    let span = i128::from(spec.to_ms) - i128::from(spec.from_ms);
    if forced.is_none() && span < i128::from(SLICE_MIN_SPAN_MS) {
        return Ok(vec![(spec.from_ms, spec.to_ms)]);
    }
    let earliest = earliest_ts_start_below(db, spec.from_ms, watermark)?;
    Ok(slice_ranges(spec.from_ms, earliest, spec.to_ms, forced))
}

/// Cut `[from_ms, to_ms)` into contiguous half-open ranges: `forced` many, or
/// [`SLICES`] when the effective span is at least [`SLICE_MIN_SPAN_MS`] and
/// one otherwise.
///
/// The effective span starts at the window's earliest row (`earliest`, `None`
/// for a window with no row), since an all-time window starts at the epoch
/// and an even cut of that would put every row in the last range. The range
/// edges are equal cuts of the effective span; the first range still starts
/// at `from_ms` and the last ends at `to_ms`, so together they cover the
/// window exactly.
pub(super) fn slice_ranges(
    from_ms: i64,
    earliest: Option<i64>,
    to_ms: i64,
    forced: Option<usize>,
) -> Vec<(i64, i64)> {
    let low = earliest.map_or(from_ms, |e| e.max(from_ms)).min(to_ms);
    let span = i128::from(to_ms) - i128::from(low);
    let count = forced.unwrap_or_else(|| {
        if earliest.is_some() && span >= i128::from(SLICE_MIN_SPAN_MS) {
            SLICES
        } else {
            1
        }
    });
    let count = count.max(1);
    let edges = (1..count).map(|i| {
        // In range: `span` is non-negative and `i < count`, so the edge lies
        // in `[low, to_ms]`.
        (i128::from(low) + span * i as i128 / count as i128) as i64
    });
    let bounds: Vec<i64> = std::iter::once(from_ms)
        .chain(edges)
        .chain(std::iter::once(to_ms))
        .collect();
    bounds.windows(2).map(|pair| (pair[0], pair[1])).collect()
}

/// What every slice of one read shares.
struct SliceJob<'a> {
    spec: &'a QuerySpec,
    watermark: i64,
    limit: &'a ReadLimit,
    open: OpenSlice<'a>,
}

/// A slice after the first: running on its own thread, or waiting to run on
/// the caller's thread because its thread could not be started.
enum Pending<'scope> {
    Thread(ScopedJoinHandle<'scope, Result<SliceRows, QueryError>>),
    Inline(usize, (i64, i64)),
}

impl SliceJob<'_> {
    /// Read every range: the first on `db` on this thread, the rest each on
    /// its own connection and thread. Returns once every slice has ended, in
    /// range order.
    fn run(&self, db: &UsageDb, ranges: &[(i64, i64)]) -> Vec<Result<SliceRows, QueryError>> {
        let Some((&first, rest)) = ranges.split_first() else {
            return Vec::new();
        };
        std::thread::scope(|scope| {
            let pending: Vec<Pending<'_>> = rest
                .iter()
                .enumerate()
                .map(|(offset, &range)| self.spawn(scope, offset + 1, range))
                .collect();
            let mut results = Vec::with_capacity(ranges.len());
            let first_slice = CancelOnUnwind(&self.limit.cancel);
            results.push(self.stop_on_error(self.read_on(db, first)));
            drop(first_slice);
            for slice in pending {
                results.push(match slice {
                    Pending::Thread(handle) => handle
                        .join()
                        .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
                    Pending::Inline(index, range) => self.read_other(index, range),
                });
            }
            results
        })
    }

    /// Start slice `index` on its own thread; a thread that cannot be started
    /// leaves the slice to run on the caller's thread instead.
    fn spawn<'scope, 'env>(
        &'env self,
        scope: &'scope Scope<'scope, 'env>,
        index: usize,
        range: (i64, i64),
    ) -> Pending<'scope> {
        let spawned = Builder::new()
            .name(format!("routectl-usage-slice-{index}"))
            .spawn_scoped(scope, move || self.read_other(index, range));
        match spawned {
            Ok(handle) => Pending::Thread(handle),
            Err(err) => {
                tracing::warn!(
                    target: "routectl_usage::query",
                    slice = index,
                    error = %err,
                    "usage slice thread could not start; reading the slice on the calling thread"
                );
                Pending::Inline(index, range)
            }
        }
    }

    /// Read a slice after the first on its own connection, under its own copy
    /// of the read limit.
    fn read_other(&self, index: usize, range: (i64, i64)) -> Result<SliceRows, QueryError> {
        let _stop_siblings_on_panic = CancelOnUnwind(&self.limit.cancel);
        let read = || {
            self.limit.check()?;
            let mut db = (self.open)(index, self.limit)?;
            let guard = DeadlineGuard::install(&mut db, self.limit.clone())?;
            self.read_on(&guard, range)
        };
        self.stop_on_error(read())
    }

    /// Read one range on `db`: both side reads and the aggregate, in one
    /// transaction.
    fn read_on(&self, db: &UsageDb, (from_ms, to_ms): (i64, i64)) -> Result<SliceRows, QueryError> {
        let limit = self.limit;
        limit.check()?;
        let window = ReadWindow {
            from_ms,
            to_ms,
            alias_filter: self.spec.alias_filter.as_deref(),
            provider_filter: self.spec.provider_filter.as_deref(),
            watermark: self.watermark,
            anchor_ms: self.spec.from_ms,
        };
        let width = self.spec.bucket.map(|bucket| bucket.width_ms);
        let tx = db.conn().unchecked_transaction()?;
        let lifetimes = read_lifetimes(&tx, &window, width, limit)?;
        let classes = read_error_classes(&tx, &window, width, limit)?;
        let fine = read_fine_rows(&tx, &window, width, limit)?;
        // A read-only transaction has nothing to commit; finishing releases
        // the snapshot.
        tx.finish()?;
        Ok(SliceRows {
            fine,
            lifetimes,
            classes,
        })
    }

    /// Cancel the sibling slices when this one failed.
    fn stop_on_error(
        &self,
        result: Result<SliceRows, QueryError>,
    ) -> Result<SliceRows, QueryError> {
        if result.is_err() {
            self.limit.cancel.cancel();
        }
        result
    }
}

/// Sets the flag when dropped by an unwinding panic. A slice holds one for
/// its whole body, so a slice that panics stops its siblings at once rather
/// than when the caller reaches its handle: the caller joins in range order
/// and may be waiting on an earlier slice, and the scope joins every thread
/// before the panic leaves it.
struct CancelOnUnwind<'a>(&'a CancelFlag);

impl Drop for CancelOnUnwind<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.cancel();
        }
    }
}

/// Every slice's rows, or the first failure that is not an interrupt (an
/// interrupt when every failure was one).
fn first_failure(
    results: Vec<Result<SliceRows, QueryError>>,
) -> Result<Vec<SliceRows>, QueryError> {
    let mut slices = Vec::with_capacity(results.len());
    let mut interrupted = false;
    for result in results {
        match result {
            Ok(rows) => slices.push(rows),
            Err(QueryError::Interrupted) => interrupted = true,
            Err(err) => return Err(err),
        }
    }
    if interrupted {
        return Err(QueryError::Interrupted);
    }
    Ok(slices)
}

/// A slice connection that failed to open, as a query error. The caller's own
/// open of the same file has already succeeded, so a refusal here (no data, a
/// version or journal-mode mismatch) means the file changed underneath the
/// read and is reported as a failed open.
fn slice_open_error(err: &OpenError) -> QueryError {
    match err {
        OpenError::Open { source, .. } | OpenError::Pragma(source) => {
            QueryError::from(clone_sqlite_error(source))
        }
        other => QueryError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
            Some(other.to_string()),
        )),
    }
}

/// `rusqlite::Error` is not `Clone`; its code and message carry what a caller
/// classifies on.
fn clone_sqlite_error(err: &rusqlite::Error) -> rusqlite::Error {
    match err {
        rusqlite::Error::SqliteFailure(code, message) => {
            rusqlite::Error::SqliteFailure(*code, message.clone())
        }
        other => rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
            Some(other.to_string()),
        ),
    }
}

/// Merge the slices by `(fine group, bucket)`, in a fixed order so the
/// caller's fold sums the same rows in the same order however the window was
/// cut. A key read by one slice only is taken as read, so a single-slice read
/// passes through no merge step. Checks `limit` once per fine row.
pub(super) fn merge(slices: Vec<SliceRows>, limit: &ReadLimit) -> Result<MergedRead, QueryError> {
    let mut fine: HashMap<_, FineRow> = HashMap::new();
    let mut lifetimes = LifetimesByRow::new();
    let mut classes = ClassesByRow::new();
    for slice in slices {
        for (row, bucket) in slice.fine {
            limit.check()?;
            match fine.entry((row.agg.key.clone(), bucket)) {
                Entry::Occupied(mut merged) => merge_fine(merged.get_mut(), row),
                Entry::Vacant(slot) => {
                    slot.insert(row);
                }
            }
        }
        for (key, from) in slice.lifetimes {
            match lifetimes.entry(key) {
                Entry::Occupied(mut merged) => merge_lifetimes(merged.get_mut(), from),
                Entry::Vacant(slot) => {
                    slot.insert(from);
                }
            }
        }
        for (key, from) in slice.classes {
            match classes.entry(key) {
                Entry::Occupied(mut merged) => merge_classes(merged.get_mut(), from),
                Entry::Vacant(slot) => {
                    slot.insert(from);
                }
            }
        }
    }
    let mut rows: Vec<BucketedFineRow> = fine
        .into_iter()
        .map(|((_, bucket), mut row)| {
            derive_cache_read_avg(&mut row.agg);
            (row, bucket)
        })
        .collect();
    rows.sort_unstable_by(|(a, a_bucket), (b, b_bucket)| {
        let (a, b) = (&a.agg.key, &b.agg.key);
        (
            &a.model,
            &a.provider,
            &a.upstream,
            &a.alias,
            &a.provider_kind,
            &a.seat,
            a_bucket,
        )
            .cmp(&(
                &b.model,
                &b.provider,
                &b.upstream,
                &b.alias,
                &b.provider_kind,
                &b.seat,
                b_bucket,
            ))
    });
    Ok(MergedRead {
        rows,
        lifetimes,
        classes,
    })
}

/// The fine rows of a read merged across their buckets: one row per fine
/// group over the whole window, in the order each group first appears, with
/// its lifetimes and error classes summed alongside. The groups and totals are
/// priced from these, so they never depend on whether a series was requested.
#[derive(Default)]
pub(super) struct WindowRows {
    rows: Vec<(FineRow, BTreeMap<String, i64>)>,
    index: HashMap<GroupKey, usize>,
}

impl WindowRows {
    /// Add one bucket's fine row, its lifetimes already attached, and its
    /// error classes.
    pub(super) fn add(&mut self, mut row: FineRow, classes: BTreeMap<String, i64>) {
        match self.index.entry(row.agg.key.clone()) {
            Entry::Occupied(at) => {
                let (into, into_classes) = &mut self.rows[*at.get()];
                let lifetimes = row.agg.cache_write_lifetimes.take().unwrap_or_default();
                merge_fine(into, row);
                merge_lifetimes(
                    into.agg.cache_write_lifetimes.get_or_insert_default(),
                    lifetimes,
                );
                merge_classes(into_classes, classes);
                derive_cache_read_avg(&mut into.agg);
            }
            Entry::Vacant(slot) => {
                slot.insert(self.rows.len());
                self.rows.push((row, classes));
            }
        }
    }

    /// The merged rows, each beside its error classes.
    pub(super) fn into_rows(self) -> Vec<(FineRow, BTreeMap<String, i64>)> {
        self.rows
    }
}

/// Add the class counts of `from` into `into`.
fn merge_classes(into: &mut BTreeMap<String, i64>, from: BTreeMap<String, i64>) {
    for (class, count) in from {
        *into.entry(class).or_default() += count;
    }
}

/// Add `from` into `into`, two reads of the same fine group and bucket.
///
/// Both structs are destructured in full, so a column added to either fails
/// to compile here until it is merged.
fn merge_fine(into: &mut FineRow, from: FineRow) {
    let FineRow {
        agg,
        ttfb_max,
        ttft_sum,
        ttft_count,
        latency_sum,
        latency_max,
        ctx_max,
        ctx_sum,
        ctx_count,
        fallback_served,
    } = from;
    merge_agg(&mut into.agg, agg);
    into.ttfb_max = max_opt(into.ttfb_max, ttfb_max);
    into.ttft_sum += ttft_sum;
    into.ttft_count += ttft_count;
    into.latency_sum += latency_sum;
    into.latency_max = max_opt(into.latency_max, latency_max);
    into.ctx_max = max_opt(into.ctx_max, ctx_max);
    into.ctx_sum += ctx_sum;
    into.ctx_count += ctx_count;
    into.fallback_served += fallback_served;
}

/// The [`AggRow`] half of [`merge_fine`]. The mean is re-derived after the
/// merge and the lifetimes are attached after it, so neither is merged here.
fn merge_agg(into: &mut AggRow, from: AggRow) {
    let AggRow {
        key: _,
        requests,
        ok,
        errors,
        input_tokens,
        output_tokens,
        reasoning_tokens,
        cache_read_peak,
        cache_read_avg: _,
        cache_read_billed,
        cache_write_5m,
        cache_write_1h,
        server_tool_calls,
        sum_ttfb_ms,
        ttfb_count,
        gen_window_ms,
        gen_output_tokens,
        reasoning_present,
        cache_read_present,
        cache_write_5m_present,
        cache_write_1h_present,
        server_tool_present,
        stream_count,
        client_disconnect_total,
        client_disconnect_pre_dispatch,
        cancelled_total,
        cache_write_tokens,
        cache_write_present,
        cache_write_lifetimes: _,
    } = from;
    into.requests += requests;
    into.ok += ok;
    into.errors += errors;
    into.input_tokens += input_tokens;
    into.output_tokens += output_tokens;
    into.reasoning_tokens += reasoning_tokens;
    into.cache_read_peak = into.cache_read_peak.max(cache_read_peak);
    into.cache_read_billed += cache_read_billed;
    into.cache_write_5m += cache_write_5m;
    into.cache_write_1h += cache_write_1h;
    into.server_tool_calls += server_tool_calls;
    into.sum_ttfb_ms += sum_ttfb_ms;
    into.ttfb_count += ttfb_count;
    into.gen_window_ms += gen_window_ms;
    into.gen_output_tokens += gen_output_tokens;
    into.reasoning_present += reasoning_present;
    into.cache_read_present += cache_read_present;
    into.cache_write_5m_present += cache_write_5m_present;
    into.cache_write_1h_present += cache_write_1h_present;
    into.server_tool_present += server_tool_present;
    into.stream_count += stream_count;
    into.client_disconnect_total += client_disconnect_total;
    into.client_disconnect_pre_dispatch += client_disconnect_pre_dispatch;
    into.cancelled_total += cancelled_total;
    into.cache_write_tokens += cache_write_tokens;
    into.cache_write_present += cache_write_present;
}

/// The mean cache-read size from the merged sum and count: SQL's
/// `CAST(AVG(..) AS INTEGER)` truncates, and so does integer division over the
/// non-negative counters.
const fn derive_cache_read_avg(row: &mut AggRow) {
    row.cache_read_avg = if row.cache_read_present > 0 {
        row.cache_read_billed / row.cache_read_present
    } else {
        0
    };
}

/// `MAX` across two optional maxima: present beats absent.
fn max_opt(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    a.max(b)
}
