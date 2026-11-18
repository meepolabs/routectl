//! Read-budget evidence for the two ledger-backed status reads, on a 5M-row
//! live-mix ledger and on a file copy of a real ledger.
//!
//! Every cell times the `/status/query` build path a request runs after its
//! body is parsed: resolve the window, open the ledger read-only, probe the
//! earliest row for an all-time series grid, run the grouped query, fold it,
//! and price every fine row through the real pricer (a pinned router config
//! plus the shipped price table). The matrix is every window token, without
//! and with a series, cold and warm:
//!
//! - cold: the ledger file and its WAL are synced and evicted from the OS page
//!   cache with `posix_fadvise(DONTNEED)` before each repetition;
//! - warm: one untimed read first, then the timed repetitions.
//!
//! Each repetition opens a FRESH connection, as a request does, so SQLite's
//! own page cache is always empty; only the OS cache differs between the two.
//! The `/status/usage` collection is timed the same way for `today` and `all`.
//!
//! Reads run with NO usable deadline, so an over-budget read reports its real
//! elapsed time instead of interrupting; the budget is asserted afterwards.
//! The fixture shape and its asserted bands live in
//! `read_budget_fixture_tests.rs`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use routectl_router::prices::PriceBook;
use routectl_router::{Config, ProviderEntry, Router};
use routectl_usage::{ReadLimit, open, open_readonly};
use tempfile::TempDir;
use time::OffsetDateTime;

use crate::commands::usage::{WindowFlag, window_bounds};
use crate::local_time::{LocalZone, unix_ms};
use crate::server::status_gate::{QUERY_BUDGET_MS, USAGE_BUDGET_MS};

use super::super::query::{build_panel, parse_body};
use super::super::router_view::{QueryPricer, StatusRouterHandle};
use super::*;

include!("read_budget_fixture_tests.rs");

/// Timed repetitions per cell and cache condition.
const REPS: usize = 5;

/// Ceiling every window must stay under on the real-ledger copy, ms.
const SNAPSHOT_BUDGET_MS: u64 = 2_000;

/// Environment variable naming the ledger file to copy for the snapshot run.
const SNAPSHOT_ENV: &str = "ROUTECTL_BENCH_LEDGER_SNAPSHOT";

/// Every `/status/query` window token, with the series granularity the
/// dashboard requests for it.
const WINDOWS: [(&str, &str); 6] = [
    ("today", "hour"),
    ("week", "day"),
    ("month", "day"),
    ("all", "day"),
    ("last24h", "hour"),
    ("last7d", "hour"),
];

/// The `/status/usage` windows timed alongside.
const USAGE_WINDOWS: [(&str, WindowFlag); 2] =
    [("today", WindowFlag::Today), ("all", WindowFlag::All)];

/// The two OS page-cache conditions a cell is timed under.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cache {
    Cold,
    Warm,
}

impl Cache {
    const BOTH: [Self; 2] = [Self::Cold, Self::Warm];

    const fn label(self) -> &'static str {
        match self {
            Self::Cold => "cold",
            Self::Warm => "warm",
        }
    }
}

/// A deadline no read here can reach.
fn no_deadline() -> Instant {
    Instant::now() + Duration::from_hours(1)
}

/// `path` with `suffix` appended to its file name.
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut joined = path.as_os_str().to_owned();
    joined.push(suffix);
    PathBuf::from(joined)
}

/// Flush the ledger and its WAL, then drop their pages from the OS cache. A
/// missing WAL is skipped; a missing ledger fails the run.
#[cfg(target_os = "linux")]
fn evict_from_page_cache(ledger: &Path) {
    use std::os::fd::AsRawFd;
    for (path, required) in [
        (ledger.to_path_buf(), true),
        (with_suffix(ledger, "-wal"), false),
    ] {
        let file = match std::fs::File::open(&path) {
            Ok(file) => file,
            Err(_) if !required => continue,
            Err(err) => panic!("open {} for eviction: {err}", path.display()),
        };
        // DONTNEED skips dirty pages, so they are written back first.
        file.sync_all().expect("sync before eviction");
        // SAFETY: the descriptor is owned by `file` and open for the whole
        // call; posix_fadvise only reads its integer arguments.
        let rc = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        assert_eq!(rc, 0, "posix_fadvise on {}", path.display());
    }
}

#[cfg(not(target_os = "linux"))]
fn evict_from_page_cache(_ledger: &Path) {
    panic!("cold timing needs posix_fadvise page-cache eviction (Linux)");
}

/// The pricer source a request pins: one router snapshot over a config naming
/// the fixture's providers, and a price book whose refreshed table is absent,
/// so rows price against the shipped table.
struct BenchPricing {
    router: StatusRouterHandle,
    book: Arc<PriceBook>,
}

impl BenchPricing {
    fn new(dir: &Path) -> Self {
        let mut config = Config::default();
        let api = ["anthropic-one", "anthropic-other", "passthrough"];
        for name in api {
            let entry = ProviderEntry::anthropic_api("env://BENCH_API_KEY");
            config.providers.insert(name.to_string(), entry);
        }
        for name in ["anthropic-oauth", "anthropic-oauth-2"] {
            let entry = ProviderEntry::anthropic_api("oauth://anthropic");
            config.providers.insert(name.to_string(), entry);
        }
        let codex = ProviderEntry::openai_responses("oauth://codex");
        config.providers.insert("codex".to_string(), codex);
        for name in ["compat-a", "compat-b"] {
            let entry =
                ProviderEntry::openai_compat("https://compat.example/v1", "env://BENCH_API_KEY");
            config.providers.insert(name.to_string(), entry);
        }
        let router = Router::new(Arc::new(config));
        Self {
            router: StatusRouterHandle::new(Arc::new(ArcSwap::from_pointee(router))),
            book: Arc::new(PriceBook::new(dir.join("prices.json"))),
        }
    }

    fn pin(&self) -> QueryPricer {
        self.router.pricer(Arc::clone(&self.book)).pin()
    }
}

/// What every cell of one ledger is read against.
struct Bench<'a> {
    ledger: &'a Path,
    pricing: &'a BenchPricing,
    zone: LocalZone,
    now: OffsetDateTime,
}

/// One timed cell: its samples and the requests the read covered.
struct Cell {
    samples: Vec<Duration>,
    requests: i64,
}

impl Cell {
    fn p50(&self) -> Duration {
        let mut sorted = self.samples.clone();
        sorted.sort_unstable();
        sorted[(sorted.len() - 1) / 2]
    }

    fn max(&self) -> Duration {
        self.samples
            .iter()
            .copied()
            .max()
            .expect("at least one sample")
    }
}

impl Bench<'_> {
    /// One `/status/query` read, from the parsed body to the priced panel.
    fn query_once(&self, body: &str) -> (Duration, i64) {
        let started = Instant::now();
        let parsed = parse_body(body.as_bytes()).expect("bench body is in the vocabulary");
        let (spec, bucket) = parsed.resolve(&self.zone, self.now);
        let wants_series = bucket.is_some();
        let panel = build_panel(
            self.ledger,
            spec,
            bucket,
            &self.pricing.pin(),
            String::new(),
            &self.zone,
            self.now,
            &ReadLimit::from(no_deadline()),
        );
        let elapsed = started.elapsed();
        let data = panel
            .data
            .unwrap_or_else(|| panic!("{body}: unavailable {:?}", panel.unavailable));
        assert_eq!(data.series.is_some(), wants_series, "{body}: series shape");
        (elapsed, data.totals.requests)
    }

    /// One `/status/usage` collection over `flag`'s window.
    fn usage_once(&self, token: &'static str, flag: WindowFlag) -> (Duration, i64) {
        let bounds = window_bounds(&self.zone, flag, self.now);
        let started = Instant::now();
        let mut db = open_readonly_fastfail(self.ledger).expect("open ledger");
        let panel = collect(
            &mut db,
            token,
            bounds,
            LedgerHealth::default(),
            no_deadline(),
        )
        .expect("collection completes");
        (started.elapsed(), panel.totals.requests)
    }

    /// Time `read` `REPS` times under `cache`.
    fn time_cell(&self, cache: Cache, read: impl Fn() -> (Duration, i64)) -> Cell {
        if cache == Cache::Warm {
            read();
        }
        let mut cell = Cell {
            samples: Vec::with_capacity(REPS),
            requests: 0,
        };
        for _ in 0..REPS {
            if cache == Cache::Cold {
                evict_from_page_cache(self.ledger);
            }
            let (elapsed, requests) = read();
            cell.samples.push(elapsed);
            cell.requests = requests;
        }
        cell
    }

    /// Every query cell, printed; the cells over `budget_ms` are returned.
    fn query_matrix(&self, budget_ms: u64) -> Vec<String> {
        let mut over = Vec::new();
        for (window, unit) in WINDOWS {
            for series in [None, Some(unit)] {
                let body = series.map_or_else(
                    || format!(r#"{{"window":"{window}","group_by":"provider"}}"#),
                    |unit| {
                        format!(
                            r#"{{"window":"{window}","group_by":"provider","bucket":"{unit}"}}"#
                        )
                    },
                );
                for cache in Cache::BOTH {
                    let cell = self.time_cell(cache, || self.query_once(&body));
                    let label = format!(
                        "query window={window} series={} {}",
                        series.unwrap_or("none"),
                        cache.label()
                    );
                    print_cell(&label, &cell, budget_ms);
                    if cell.max() >= Duration::from_millis(budget_ms) {
                        over.push(format!("{label}: max {:?}", cell.max()));
                    }
                }
            }
        }
        over
    }

    /// Every usage-panel cell, printed against its own budget.
    fn usage_matrix(&self) {
        for (token, flag) in USAGE_WINDOWS {
            for cache in Cache::BOTH {
                let cell = self.time_cell(cache, || self.usage_once(token, flag));
                let label = format!("usage window={token} {}", cache.label());
                print_cell(&label, &cell, USAGE_BUDGET_MS);
            }
        }
    }
}

fn print_cell(label: &str, cell: &Cell, budget_ms: u64) {
    println!(
        "{label}: requests={} reps={} p50_ms={} max_ms={} budget_ms={budget_ms}",
        cell.requests,
        cell.samples.len(),
        cell.p50().as_millis(),
        cell.max().as_millis()
    );
}

/// The machine context every number is recorded with.
fn print_environment(ledger: &Path) {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            info.lines()
                .find(|line| line.starts_with("model name"))
                .and_then(|line| line.split(':').nth(1))
                .map(|name| name.trim().to_string())
        })
        .unwrap_or_else(|| "unknown".to_string());
    let threads = std::thread::available_parallelism().map_or(0, std::num::NonZero::get);
    println!("cpu: {cpu} ({threads} threads)");
    println!("ledger: {}", ledger.display());
    for suffix in ["", "-wal"] {
        let path = with_suffix(ledger, suffix);
        if let Ok(meta) = std::fs::metadata(&path) {
            println!("  {} bytes: {}", path.display(), meta.len());
        }
    }
}

/// The 5M-row live-mix ledger answers every window, with and without a series,
/// cold and warm, under `QUERY_BUDGET_MS`.
///
/// `#[ignore]`d: seeding takes minutes and writes several GB. Run it in
/// release with `TMPDIR` on real storage (a tmpfs `/tmp` would measure a
/// RAM-backed ledger) under a memory cap; the exact command is in
/// docs/DEVELOPMENT.md "Explicit runs".
#[test]
#[ignore = "seeds a 5M-row ledger; see docs/DEVELOPMENT.md \"Explicit runs\""]
fn a_five_million_row_ledger_answers_every_window_inside_the_query_budget() {
    // Arrange
    let dir = TempDir::new().unwrap();
    let ledger = dir.path().join("usage.db");
    let now = OffsetDateTime::now_utc();
    let now_ms = unix_ms(now);
    let seed_started = Instant::now();
    seed_live_mix(&ledger, now_ms);
    println!(
        "seeded {LIVE_MIX_ROWS} rows in {:?}",
        seed_started.elapsed()
    );
    let stats = FixtureStats::read(&ledger);
    stats.print();
    stats.assert_live_mix(now_ms);
    print_environment(&ledger);
    let pricing = BenchPricing::new(dir.path());
    let bench = Bench {
        ledger: &ledger,
        pricing: &pricing,
        zone: LocalZone::load(),
        now,
    };

    // Act
    let over = bench.query_matrix(QUERY_BUDGET_MS);
    bench.usage_matrix();

    // Assert
    assert!(
        over.is_empty(),
        "cells at or over {QUERY_BUDGET_MS} ms: {over:#?}"
    );
}

/// The SQLite header's `user_version`, read from the file bytes (offset 60,
/// big-endian) so the snapshot is never opened by SQLite.
fn header_user_version(path: &Path) -> u32 {
    use std::io::Read;
    let mut header = [0_u8; 64];
    std::fs::File::open(path)
        .and_then(|mut file| file.read_exact(&mut header))
        .expect("read the SQLite header");
    u32::from_be_bytes([header[60], header[61], header[62], header[63]])
}

/// The last row's start plus one millisecond, so the windows are anchored at
/// the end of the copied history whenever the run happens.
fn instant_after_last_row(ledger: &Path) -> OffsetDateTime {
    let db = open_readonly(ledger).expect("open migrated copy");
    let last_ms: i64 = db
        .conn()
        .query_row("SELECT MAX(ts_start) FROM requests", [], |r| r.get(0))
        .expect("last row");
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(last_ms + 1) * 1_000_000)
        .expect("last row instant")
}

/// A file copy of a real ledger migrates to the current schema (timed) and
/// answers every window, cold and warm, under `SNAPSHOT_BUDGET_MS`.
///
/// The ledger named by `ROUTECTL_BENCH_LEDGER_SNAPSHOT` is only file-copied
/// into a scratch directory; SQLite opens the copy alone. Skips with a message
/// when the variable is unset.
#[test]
#[ignore = "reads an operator-supplied ledger copy; see docs/DEVELOPMENT.md \"Explicit runs\""]
fn a_real_ledger_copy_answers_every_window_inside_two_seconds() {
    let Some(source) = std::env::var_os(SNAPSHOT_ENV).map(PathBuf::from) else {
        println!("skipped: set {SNAPSHOT_ENV} to a usage ledger file to copy");
        return;
    };

    // Arrange: copy the file (and a WAL beside it, if any), then migrate the copy.
    let dir = TempDir::new().unwrap();
    let ledger = dir.path().join("usage.db");
    let copy_started = Instant::now();
    std::fs::copy(&source, &ledger).expect("copy the snapshot");
    let source_wal = with_suffix(&source, "-wal");
    if source_wal.exists() {
        std::fs::copy(&source_wal, with_suffix(&ledger, "-wal")).expect("copy the snapshot WAL");
    }
    println!("copied in {:?}", copy_started.elapsed());
    let from_version = header_user_version(&ledger);
    let migrate_started = Instant::now();
    drop(open(&ledger).expect("migrating open of the copy"));
    println!(
        "migrated v{from_version} -> v{} (index build) in {:?}",
        routectl_usage::SCHEMA_VERSION,
        migrate_started.elapsed()
    );
    print_environment(&ledger);
    let pricing = BenchPricing::new(dir.path());
    let bench = Bench {
        ledger: &ledger,
        pricing: &pricing,
        zone: LocalZone::load(),
        now: instant_after_last_row(&ledger),
    };

    // Act
    let over = bench.query_matrix(SNAPSHOT_BUDGET_MS);
    bench.usage_matrix();

    // Assert
    assert!(
        over.is_empty(),
        "cells at or over {SNAPSHOT_BUDGET_MS} ms: {over:#?}"
    );
}
