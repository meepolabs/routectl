//! Large-ledger read-cost measurement for the two budgeted status reads.
//!
//! ONE seeded ledger, many reads. Seeding dominates the cost of an experiment
//! at this scale, so a single multi-million-row fixture is built once and then
//! read through nested `from_ms` windows selecting roughly a quarter, a half,
//! three quarters, and all of it. Four points rather than one: a single point
//! cannot tell a linear cost from a superlinear one, and the shape here is a
//! testable claim -- the temp-b-tree term saturates (series output rows are
//! capped at 1000 buckets x the fine-key count) while the row-scan term stays
//! linear.
//!
//! Three reads are timed per window:
//!
//! - the `/status/query` grouped aggregate WITH a 1000-bucket series grid,
//! - the same aggregate with `bucket: None` (the unbucketed path), and
//! - the whole `/status/usage` panel collection over the same window.
//!
//! Every read runs with NO usable deadline (a far-future `Instant`), so a read
//! that would blow its budget reports its real elapsed time instead of
//! interrupting. This measures the read curve, not the guard; the budget
//! consts are printed alongside so a reader can see where each curve crosses.
//!
//! What this fixture is NOT: a windowed read over a 4M-row table selecting 1M
//! rows is not byte-identical to a read of a 1M-row table. The series and
//! aggregate statements ride `idx_requests_ts_start` (pinned by
//! `the_series_statement_uses_the_ts_start_index` in the leaf crate), so the
//! difference between the two is index depth, not the linear scan term. That is
//! adequate for a "start paying attention at N rows" threshold and inadequate
//! for anything finer.
//!
//! Cache condition: each timed read opens a FRESH read-only connection, so no
//! page of the ledger sits in that connection's own cache -- but nothing here
//! evicts the OS page cache, and the fixture was just written by this same
//! process. Results are SQLite-cold and OS-warm. They are not cold-cache
//! numbers and must not be recorded as such.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use routectl_usage::{BucketSpec, GroupDim, QuerySpec, RowCost, open, query};
use tempfile::TempDir;

use crate::server::status_gate::QUERY_BUDGET_MS;

use super::*;

/// Rows in the fixture. The windows below select 1/4, 2/4, 3/4 and 4/4 of them.
const TOTAL_ROWS: usize = 4_000_000;

/// Calendar span the rows are spread evenly over.
const SPAN_DAYS: i64 = 400;

/// Buckets per series read -- the crate's `SERIES_BUCKET_CAP`, which is what a
/// real all-time `bucket: day` request resolves to on a long ledger. The bucket
/// WIDTH is derived per window so the count stays pinned here.
const BUCKET_COUNT: usize = 1000;

/// Timed repetitions per data point.
const REPS: usize = 5;

/// Fine-grain key components. Indexed INDEPENDENTLY when seeding, so the fine
/// grouping key `(model, provider, upstream, alias)` takes all 100 combinations
/// rather than the 10 correlated pairs a shared index would produce.
const MODELS: [&str; 10] = [
    "m-00", "m-01", "m-02", "m-03", "m-04", "m-05", "m-06", "m-07", "m-08", "m-09",
];
const ALIASES: [&str; 10] = [
    "a-00", "a-01", "a-02", "a-03", "a-04", "a-05", "a-06", "a-07", "a-08", "a-09",
];

/// How often the seeder reports progress, in rows. Seeding this fixture takes
/// minutes; a human watching the run needs to see it is alive.
const SEED_PROGRESS_EVERY: usize = 250_000;

/// Concurrent readers to contend at the 1M-row point -- `STATUS_MAX_INFLIGHT`,
/// the number of `/status*` requests the gate admits at once, and so the number
/// of panel builders that can scan the ledger simultaneously.
const CONCURRENT_READERS: usize = 4;

/// Sorted-sample summary of one data point.
struct Stats {
    p50: Duration,
    p95: Duration,
    max: Duration,
}

/// Summarize timed repetitions. At `REPS` = 5 the p95 index coincides with the
/// maximum; both are printed anyway so a future change to `REPS` needs no
/// reinterpretation of the output.
fn stats(samples: &[Duration]) -> Stats {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let pick = |q: f64| {
        let rank = (sorted.len() as f64 * q).ceil().max(1.0) as usize;
        sorted[rank - 1]
    };
    Stats {
        p50: pick(0.5),
        p95: pick(0.95),
        max: *sorted.last().expect("at least one sample"),
    }
}

/// Seed `TOTAL_ROWS` rows spread evenly over `SPAN_DAYS`, one prepared
/// statement inside one transaction. Returns the window the rows occupy as
/// `(from_ms, span_ms)`.
fn seed_wide_grid(path: &Path) -> (i64, i64) {
    let db = open(path).expect("open ledger");
    let span_ms = SPAN_DAYS * 86_400_000;
    let from_ms = Local::now().timestamp_millis() - span_ms;
    let conn = db.conn();
    conn.execute_batch("BEGIN").expect("begin");
    {
        let mut stmt = conn
            .prepare(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, ttfb_ms, tool_count, msg_count, attempt_count, fallback_count, \
                 input_tokens, output_tokens) \
                 VALUES (?1, ?1, ?2, 'openai', ?3, ?4, ?3, 'p', 'u', 1, 'ok', \
                 50, 10, 0, 0, 1, 0, 100, 20)",
            )
            .expect("prepare seed");
        let started = Instant::now();
        for i in 0..TOTAL_ROWS {
            let ts = from_ms + (i as i64 * span_ms) / TOTAL_ROWS as i64;
            stmt.execute(rusqlite::params![
                ts,
                format!("r{i}"),
                MODELS[i % MODELS.len()],
                ALIASES[(i / MODELS.len()) % ALIASES.len()],
            ])
            .expect("seed row");
            if (i + 1) % SEED_PROGRESS_EVERY == 0 {
                println!(
                    "  seeded {} / {TOTAL_ROWS} rows ({:?})",
                    i + 1,
                    started.elapsed()
                );
            }
        }
    }
    conn.execute_batch("COMMIT").expect("commit");
    (from_ms, span_ms)
}

/// The query spec for a window, with or without a 1000-bucket series grid.
fn spec_for(from_ms: i64, to_ms: i64, bucketed: bool) -> QuerySpec {
    QuerySpec {
        from_ms,
        to_ms,
        group_by: GroupDim::Model,
        alias_filter: None,
        provider_filter: None,
        bucket: bucketed.then(|| BucketSpec {
            // Derived so the grid is exactly BUCKET_COUNT buckets wide whatever
            // the window: the bucket COUNT is what drives the temp b-tree.
            width_ms: (to_ms - from_ms) / BUCKET_COUNT as i64 + 1,
            count: BUCKET_COUNT,
        }),
    }
}

/// A deadline far enough out that no read here can reach it, so an over-budget
/// read reports its real elapsed time instead of interrupting.
fn no_deadline() -> Instant {
    Instant::now() + Duration::from_hours(1)
}

/// Time one grouped-aggregate read on a fresh read-only connection.
fn time_query(path: &Path, spec: &QuerySpec) -> (Duration, i64) {
    let db = open_readonly_fastfail(path).expect("open seeded ledger");
    let started = Instant::now();
    let result =
        query(&db, spec, |_row| RowCost::Unpriced, no_deadline()).expect("query completes");
    (started.elapsed(), result.totals.requests)
}

/// Time one whole usage-panel collection on a fresh read-only connection.
fn time_usage(path: &Path, bounds: WindowBounds) -> (Duration, i64) {
    let db = open_readonly_fastfail(path).expect("open seeded ledger");
    let started = Instant::now();
    let panel = collect(&db, "all", bounds, no_deadline()).expect("collection completes");
    (started.elapsed(), panel.totals.requests)
}

/// Run `REPS` timed repetitions and print the point.
fn report(label: &str, budget_ms: u64, rows: i64, samples: &[Duration]) {
    let s = stats(samples);
    println!(
        "{label}: rows={rows} reps={} p50={:?} p95={:?} max={:?} (budget {budget_ms}ms)",
        samples.len(),
        s.p50,
        s.p95,
        s.max
    );
}

/// Run `CONCURRENT_READERS` identical reads at once and print the per-reader
/// spread alongside the wall time of the whole batch.
fn report_concurrent(
    label: &str,
    budget_ms: u64,
    path: &Path,
    read: impl Fn(&Path) -> Duration + Copy + Send + Sync,
) {
    let owned = path.to_path_buf();
    let started = Instant::now();
    let samples: Vec<Duration> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..CONCURRENT_READERS)
            .map(|_| {
                let p: PathBuf = owned.clone();
                scope.spawn(move || read(&p))
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("reader thread"))
            .collect()
    });
    let wall = started.elapsed();
    let s = stats(&samples);
    println!(
        "{label}: readers={CONCURRENT_READERS} p50={:?} p95={:?} max={:?} wall={wall:?} \
         (budget {budget_ms}ms)",
        s.p50, s.p95, s.max
    );
}

/// The large-ledger cost curve for both budgeted status reads.
///
/// `#[ignore]`d and never part of the default suite: seeding several million
/// rows costs minutes, and the pre-commit gate already runs a full release
/// build and test pass on every commit. Run it explicitly, in release (a debug
/// SQLite fold is several times slower for reasons the shipped binary never
/// pays), and with `TMPDIR` pointed at real storage -- the default `/tmp` is a
/// tmpfs on many systems, which would measure a RAM-backed ledger and quietly
/// flatter every number:
///
/// ```text
/// TMPDIR=/var/tmp cargo test -p routectl-cli --release \
///   handlers::status::usage::read_budget_bench_tests::the_large_ledger_read_cost_curve \
///   -- --ignored --nocapture
/// ```
#[test]
#[ignore = "seeds a multi-million-row ledger; run explicitly with --ignored"]
fn the_large_ledger_read_cost_curve() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("usage.db");
    println!(
        "seeding {TOTAL_ROWS} rows over {SPAN_DAYS} days at {}",
        path.display()
    );
    let seed_started = Instant::now();
    let (from_ms, span_ms) = seed_wide_grid(&path);
    println!("seeded in {:?}", seed_started.elapsed());
    for (suffix, what) in [("", "db"), ("-wal", "wal"), ("-shm", "shm")] {
        let mut p = path.clone().into_os_string();
        p.push(suffix);
        if let Ok(meta) = std::fs::metadata(&p) {
            println!("  {what} file: {} bytes", meta.len());
        }
    }
    let to_ms = from_ms + span_ms + 1;

    // Nested windows, each anchored at the SAME upper bound and reaching
    // further back, so the four points differ only in how many rows they
    // select.
    for quarter in 1..=4_i64 {
        let selected = TOTAL_ROWS as i64 * quarter / 4;
        let window_from = to_ms - (span_ms * quarter) / 4 - 1;
        let label = format!("{selected} rows");

        let bucketed = spec_for(window_from, to_ms, true);
        let mut samples = Vec::with_capacity(REPS);
        let mut counted = 0;
        for _ in 0..REPS {
            let (elapsed, rows) = time_query(&path, &bucketed);
            samples.push(elapsed);
            counted = rows;
        }
        report(
            &format!("query bucketed  {label}"),
            QUERY_BUDGET_MS,
            counted,
            &samples,
        );

        let plain = spec_for(window_from, to_ms, false);
        let mut samples = Vec::with_capacity(REPS);
        for _ in 0..REPS {
            let (elapsed, rows) = time_query(&path, &plain);
            samples.push(elapsed);
            counted = rows;
        }
        report(
            &format!("query unbucketed {label}"),
            QUERY_BUDGET_MS,
            counted,
            &samples,
        );

        let bounds = WindowBounds {
            from_ms: window_from,
            to_ms,
        };
        let mut samples = Vec::with_capacity(REPS);
        for _ in 0..REPS {
            let (elapsed, rows) = time_usage(&path, bounds);
            samples.push(elapsed);
            counted = rows;
        }
        report(
            &format!("usage panel     {label}"),
            USAGE_BUDGET_MS,
            counted,
            &samples,
        );

        // Every point above is single-request, while the gate admits
        // STATUS_MAX_INFLIGHT concurrent scans that contend for CPU, memory
        // bandwidth and temp b-trees. Measured at the smallest window only:
        // this bounds nothing (the contention predates any budget change), but
        // an unmeasured 4x factor must not be written down as if measured.
        if quarter == 1 {
            let bucketed_at_quarter = bucketed.clone();
            report_concurrent(
                &format!("query bucketed  {label} CONCURRENT"),
                QUERY_BUDGET_MS,
                &path,
                |p| time_query(p, &bucketed_at_quarter).0,
            );
            report_concurrent(
                &format!("usage panel     {label} CONCURRENT"),
                USAGE_BUDGET_MS,
                &path,
                |p| time_usage(p, bounds).0,
            );
        }

        assert_eq!(
            counted, selected,
            "the {quarter}/4 window must select {selected} rows"
        );
    }
}
