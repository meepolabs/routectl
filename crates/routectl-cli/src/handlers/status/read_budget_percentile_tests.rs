// Exact timing percentiles on the read-budget fixtures: every query cell
// timed with the six percentile fields checked on every metric set, the peak
// resident memory of an all-time read, and the figures beside the mean and
// maximum they sit next to. `include!`d into `read_budget_bench_tests.rs`; all
// imports come from the host, so do not add `use` lines here.

/// The `/proc/self/status` field holding the process's peak resident set.
const PEAK_RSS_FIELD: &str = "VmHWM:";

/// Reset the process's peak resident set to its current one (Linux
/// `clear_refs` value 5), so the next reading is the peak of what ran since.
fn reset_peak_rss() {
    std::fs::write("/proc/self/clear_refs", "5").expect("reset the peak RSS");
}

/// The process's peak resident set since the last reset, KiB.
fn peak_rss_kib() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").expect("read process status");
    status
        .lines()
        .find_map(|line| line.strip_prefix(PEAK_RSS_FIELD))
        .and_then(|rest| rest.trim().trim_end_matches("kB").trim().parse().ok())
        .expect("peak RSS field")
}

/// The body of one matrix cell.
fn cell_body(window: &str, series: Option<&str>, group_by: &str) -> String {
    series.map_or_else(
        || format!(r#"{{"window":"{window}","group_by":"{group_by}"}}"#),
        |unit| format!(r#"{{"window":"{window}","group_by":"{group_by}","bucket":"{unit}"}}"#),
    )
}

/// The machine's one-minute load average, as `uptime` reports it.
fn load_average() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|text| text.split_whitespace().next().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

/// The six percentile fields every serialized metric set carries.
const PERCENTILE_FIELDS: [&str; 6] = [
    "ttft_p50_ms",
    "ttft_p90_ms",
    "ttft_p99_ms",
    "latency_p50_ms",
    "latency_p90_ms",
    "latency_p99_ms",
];

impl Bench<'_> {
    /// One read whose percentile fields are checked on every metric set.
    fn checked_once(&self, body: &str) -> (Duration, i64) {
        let (elapsed, data) = self.query_result(body);
        assert_percentiles_fit(body, &data);
        (elapsed, data.totals.requests)
    }

    /// Every query cell timed, cold and warm, printed with the load at the
    /// time; the cells over `budget_ms` are returned.
    fn percentile_matrix(&self, budget_ms: u64) -> Vec<String> {
        let mut over = Vec::new();
        for (window, unit) in WINDOWS {
            for series in [None, Some(unit)] {
                let body = cell_body(window, series, "provider");
                for cache in Cache::BOTH {
                    let label = format!(
                        "window={window} series={} {}",
                        series.unwrap_or("none"),
                        cache.label()
                    );
                    let load = load_average();
                    let cell = self.time_cell(cache, || self.checked_once(&body));
                    println!(
                        "pct {label}: requests={} p50_ms={} max_ms={} load1={load} \
                         budget_ms={budget_ms}",
                        cell.requests,
                        cell.p50().as_millis(),
                        cell.max().as_millis(),
                    );
                    if cell.max() >= Duration::from_millis(budget_ms) {
                        over.push(format!("{label}: max {:?}", cell.max()));
                    }
                }
            }
        }
        over
    }

    /// Peak resident memory of one warm all-time read, with and without a
    /// series.
    fn peak_rss_all(&self) {
        for series in [None, Some("day")] {
            let body = cell_body("all", series, "provider");
            self.query_once(&body);
            reset_peak_rss();
            let before = peak_rss_kib();
            let _ = self.query_result(&body);
            let peak = peak_rss_kib();
            println!(
                "peak rss window=all series={}: before_kib={before} peak_kib={peak} \
                 growth_kib={}",
                series.unwrap_or("none"),
                peak.saturating_sub(before)
            );
        }
    }
}

/// Every metric set of `data` serializes the six percentile fields; each
/// lies within the observed range the aggregate reports, and a set holds a
/// percentile exactly when it holds the mean beside it.
fn assert_percentiles_fit(body: &str, data: &QueryResult) {
    let check = |what: &str, m: &routectl_usage::QueryMetrics| {
        let json = serde_json::to_value(m).expect("metrics serialize");
        for field in PERCENTILE_FIELDS {
            assert!(json.get(field).is_some(), "{body} {what}: {field} missing");
        }
        assert_eq!(
            m.ttft_p50_ms.is_some(),
            m.ttft_avg_ms.is_some(),
            "{body} {what}: ttft"
        );
        assert_eq!(
            m.latency_p50_ms.is_some(),
            m.latency_avg_ms.is_some(),
            "{body} {what}: latency"
        );
        assert!(m.ttft_p50_ms <= m.ttft_p90_ms && m.ttft_p90_ms <= m.ttft_p99_ms);
        assert!(
            m.ttft_p99_ms <= m.ttft_max_ms,
            "{body} {what}: ttft p99 > max"
        );
        assert!(m.latency_p50_ms <= m.latency_p90_ms && m.latency_p90_ms <= m.latency_p99_ms);
        assert!(
            m.latency_p99_ms <= m.latency_max_ms,
            "{body} {what}: latency p99 > max"
        );
    };
    check("totals", &data.totals);
    if data.totals.requests > 0 {
        assert!(
            data.totals.latency_p50_ms.is_some(),
            "{body}: a read with requests carries a latency p50"
        );
    }
    for group in &data.groups {
        check(&group.label, &group.metrics);
    }
    if let Some(series) = &data.series {
        for bucket in &series.buckets {
            check("bucket", &bucket.metrics);
        }
    }
}

/// `ms` as seconds with one decimal, `-` when absent.
fn secs(ms: Option<i64>) -> String {
    ms.map_or_else(
        || "-".to_string(),
        |ms| format!("{:.1}", ms as f64 / 1_000.0),
    )
}

/// The last-7-days provider table: mean, p50, p90, p99 and max of TTFT and
/// latency per provider and in total.
fn print_sanity_table(bench: &Bench<'_>) {
    let body = cell_body("last7d", None, "provider");
    let (_, data) = bench.query_result(&body);
    println!(
        "sanity last7d by provider (s): label requests | ttft avg p50 p90 p99 max | \
         latency avg p50 p90 p99 max"
    );
    let rows = data
        .groups
        .iter()
        .map(|g| (g.label.as_str(), &g.metrics))
        .chain(std::iter::once(("TOTAL", &data.totals)));
    for (label, m) in rows {
        println!(
            "sanity {label} {} | {} {} {} {} {} | {} {} {} {} {}",
            m.requests,
            secs(m.ttft_avg_ms),
            secs(m.ttft_p50_ms),
            secs(m.ttft_p90_ms),
            secs(m.ttft_p99_ms),
            secs(m.ttft_max_ms),
            secs(m.latency_avg_ms),
            secs(m.latency_p50_ms),
            secs(m.latency_p90_ms),
            secs(m.latency_p99_ms),
            secs(m.latency_max_ms),
        );
    }
}

/// Exact percentiles on the 5M-row live-mix ledger: every cell cold and warm
/// with the percentile fields checked, and the peak memory of an all-time
/// read. Fails when a cell reaches `QUERY_BUDGET_MS`.
#[test]
#[ignore = "seeds a 5M-row ledger; see docs/DEVELOPMENT.md \"Explicit runs\""]
fn exact_timing_percentiles_on_five_million_rows() {
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
    let over = bench.percentile_matrix(QUERY_BUDGET_MS);
    bench.peak_rss_all();
    print_sanity_table(&bench);

    // Assert
    assert!(
        over.is_empty(),
        "cells at or over {QUERY_BUDGET_MS} ms: {over:#?}"
    );
}

/// Exact percentiles on a file copy of a real ledger: the same matrix, the
/// peak memory, and the last-7-days provider table.
#[test]
#[ignore = "reads an operator-supplied ledger copy; see docs/DEVELOPMENT.md \"Explicit runs\""]
fn exact_timing_percentiles_on_a_real_ledger_copy() {
    let Some(source) = std::env::var_os(SNAPSHOT_ENV).map(PathBuf::from) else {
        println!("skipped: set {SNAPSHOT_ENV} to a usage ledger file to copy");
        return;
    };

    // Arrange
    let dir = TempDir::new().unwrap();
    let ledger = dir.path().join("usage.db");
    std::fs::copy(&source, &ledger).expect("copy the snapshot");
    let source_wal = with_suffix(&source, "-wal");
    if source_wal.exists() {
        std::fs::copy(&source_wal, with_suffix(&ledger, "-wal")).expect("copy the snapshot WAL");
    }
    // The copy keeps the source's mode; a read-only source would leave a copy
    // the migrating open cannot write.
    for path in [ledger.clone(), with_suffix(&ledger, "-wal")] {
        if path.exists() {
            let mut perms = std::fs::metadata(&path).expect("stat copy").permissions();
            std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o600);
            std::fs::set_permissions(&path, perms).expect("make the copy writable");
        }
    }
    let migrate_started = Instant::now();
    drop(open(&ledger).expect("migrating open of the copy"));
    println!("migrated in {:?}", migrate_started.elapsed());
    print_environment(&ledger);
    let pricing = BenchPricing::new(dir.path());
    let bench = Bench {
        ledger: &ledger,
        pricing: &pricing,
        zone: LocalZone::load(),
        now: instant_after_last_row(&ledger),
    };

    // Act
    let over = bench.percentile_matrix(SNAPSHOT_BUDGET_MS);
    bench.peak_rss_all();
    print_sanity_table(&bench);

    // Assert
    assert!(
        over.is_empty(),
        "cells at or over {SNAPSHOT_BUDGET_MS} ms: {over:#?}"
    );
}
