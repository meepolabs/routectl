//! Token-count rows stay out of every inference reader, legacy NULL-operation
//! rows stay in, and no `requests` reader can skip the shared predicate
//! without a stated reason.

use std::path::Path;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::*;
use crate::Operation;
use crate::db::{UsageDb, open};

const WINDOW_FROM: i64 = 0;
const WINDOW_TO: i64 = 10_000;
const SAMPLE_LIMIT: usize = 100;

/// Insert one row that every reader in this module would admit when it is
/// inference traffic: a streamed request carrying every column a reader
/// filters or sums on. `operation` is bound verbatim so NULL (a pre-column
/// row) is expressible.
fn insert_eligible_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    outcome: &str,
    cache_read: i64,
    operation: Option<&str>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, provider_kind, upstream, seat, \
             session_id, stream, outcome, latency_ms, ttfb_ms, tool_count, msg_count, \
             attempt_count, fallback_count, input_tokens, output_tokens, cache_read, \
             resolved_class, quota_status, quota_utilization, would_trim_tokens, \
             would_trim_break_even_k, would_trim_k_floor, would_trim_shadow_misfire, \
             would_trim_recorder_version, would_trim_dedup_tokens, \
             calib_estimated_tokens, calib_prompt_tokens, reduction_decision, \
             reduction_bytes_saved, cache_front_decision, operation) \
             VALUES (?1, ?1, ?2, 'openai', 'req', 'al', 'm', 'p', 'anthropic-api', 'u', \
             's', 'sess', 1, ?3, 900, 100, 0, 1, 1, 0, 10, 20, ?5, 'rate_limited', \
             'allowed', 0.5, 7, 1.0, 2.0, 1, 1, 3, 10, 12, 'applied', 40, \
             'auto_skipped:k_below_break_even', ?4)",
            rusqlite::params![ts_start, request_id, outcome, operation, cache_read],
        )
        .expect("insert eligible row");
}

/// A ledger holding two inference rows of one session: a cache hit at ts 1000
/// and a miss at ts 1200.
fn base_ledger() -> (TempDir, UsageDb) {
    let dir = TempDir::new().expect("tempdir");
    let db = open(dir.path().join("usage.db")).expect("open");
    let inference = Some(Operation::Inference.as_str());
    insert_eligible_row(&db, "base-hit", 1000, "ok", 5, inference);
    insert_eligible_row(&db, "base-miss", 1200, "ok", 0, inference);
    (dir, db)
}

/// [`base_ledger`] plus an older success and a newer failure, both cache
/// hits, tagged with `operation`. The older one is what an earliest-row
/// reader sees; the newer one is what a latest-row reader sees; the failure
/// is what an error reader sees. Together the session reaches the four turns
/// the hazard-decay split needs, which the base ledger alone does not.
fn ledger_with(operation: Option<&str>) -> (TempDir, UsageDb) {
    let (dir, db) = base_ledger();
    insert_eligible_row(&db, "older-ok", 500, "ok", 5, operation);
    insert_eligible_row(&db, "newer-err", 1500, "upstream_error", 5, operation);
    (dir, db)
}

fn grouped_spec(bucket: Option<BucketSpec>) -> QuerySpec {
    QuerySpec {
        from_ms: WINDOW_FROM,
        to_ms: WINDOW_TO,
        group_by: GroupDim::Model,
        alias_filter: None,
        provider_filter: None,
        bucket,
    }
}

fn run_grouped(db: &UsageDb, bucket: Option<BucketSpec>) -> String {
    let deadline = Instant::now() + Duration::from_mins(10);
    let result = query(
        db,
        &grouped_spec(bucket),
        |_| RowCost::Priced(1.0),
        deadline,
    )
    .expect("grouped query");
    format!("{result:?}")
}

type Reader = fn(&UsageDb) -> String;

/// Every inference reader, rendered through `Debug` so one comparison covers
/// every field it returns.
const INFERENCE_READERS: &[(&str, Reader)] = &[
    ("aggregate", |db| {
        format!("{:?}", aggregate(db, WINDOW_FROM, WINDOW_TO).expect("read"))
    }),
    ("grouped", |db| run_grouped(db, None)),
    ("grouped_series", |db| {
        run_grouped(
            db,
            Some(BucketSpec {
                width_ms: 1000,
                count: 10,
            }),
        )
    }),
    ("earliest_ts_start", |db| {
        format!("{:?}", earliest_ts_start(db, WINDOW_FROM).expect("read"))
    }),
    ("errors_by_class", |db| {
        format!(
            "{:?}",
            errors_by_class(db, WINDOW_FROM, WINDOW_TO).expect("read")
        )
    }),
    ("ttfbs", |db| {
        format!("{:?}", ttfbs(db, WINDOW_FROM, WINDOW_TO).expect("read"))
    }),
    ("would_trim_summary", |db| {
        format!(
            "{:?}",
            would_trim_summary(db, WINDOW_FROM, WINDOW_TO).expect("read")
        )
    }),
    ("shadow_misfire_summary", |db| {
        format!(
            "{:?}",
            shadow_misfire_summary(db, WINDOW_FROM, WINDOW_TO).expect("read")
        )
    }),
    ("near_lossless_attribution_summary", |db| {
        format!(
            "{:?}",
            near_lossless_attribution_summary(db, WINDOW_FROM, WINDOW_TO).expect("read")
        )
    }),
    ("k_calibration_summary", |db| {
        format!("{:?}", k_calibration_summary(db).expect("read"))
    }),
    ("read_reuse_samples_since", |db| {
        format!(
            "{:?}",
            read_reuse_samples_since(db.conn(), WINDOW_FROM, SAMPLE_LIMIT).expect("read")
        )
    }),
    ("read_calibration_samples_since", |db| {
        format!(
            "{:?}",
            read_calibration_samples_since(db.conn(), WINDOW_FROM, SAMPLE_LIMIT).expect("read")
        )
    }),
    ("cache_decision_summary", |db| {
        format!(
            "{:?}",
            cache_decision_summary(db, WINDOW_FROM, WINDOW_TO).expect("read")
        )
    }),
    ("suppressed_sessions", |db| {
        format!(
            "{:?}",
            suppressed_sessions(db, WINDOW_FROM, WINDOW_TO).expect("read")
        )
    }),
    ("reduction_summary", |db| {
        format!(
            "{:?}",
            reduction_summary(db, WINDOW_FROM, WINDOW_TO).expect("read")
        )
    }),
    ("for_each_stream_extra", |db| {
        let mut rows = Vec::new();
        let visited = for_each_stream_extra(db, "openai", WINDOW_FROM, WINDOW_TO, |row| {
            rows.push(row);
        })
        .expect("read");
        format!("{visited} {rows:?}")
    }),
];

#[test]
fn inference_readers_ignore_count_tokens_rows() {
    for (name, read) in INFERENCE_READERS {
        // Arrange
        let (_a, inference_only) = base_ledger();
        let (_b, with_counts) = ledger_with(Some(Operation::CountTokens.as_str()));
        let (_c, with_inference) = ledger_with(Some(Operation::Inference.as_str()));

        // Act
        let baseline = read(&inference_only);
        let counted = read(&with_counts);
        let control = read(&with_inference);

        // Assert: the extra rows are visible to this reader as inference, so
        // their absence from `counted` is the predicate, not the fixture.
        assert_ne!(
            control, baseline,
            "{name}: fixture rows must be eligible for this reader"
        );
        assert_eq!(counted, baseline, "{name}: count_tokens rows leaked in");
    }
}

#[test]
fn inference_readers_count_legacy_null_operation_rows() {
    for (name, read) in INFERENCE_READERS {
        // Arrange
        let (_a, legacy) = ledger_with(None);
        let (_b, tagged) = ledger_with(Some(Operation::Inference.as_str()));

        // Act + Assert
        assert_eq!(
            read(&legacy),
            read(&tagged),
            "{name}: a NULL-operation row must read as inference"
        );
    }
}

#[test]
fn latest_quota_by_seat_reads_count_tokens_rows() {
    // Arrange: the newest quota-bearing row is a token-count request.
    let (_dir, db) = ledger_with(Some(Operation::CountTokens.as_str()));

    // Act
    let snapshots = latest_quota_by_seat(&db).expect("read");

    // Assert: seat state comes from whichever request saw it last.
    assert_eq!(snapshots.len(), 1);
    assert_eq!(snapshots[0].ts_start, 1500);
}

/// `requests` readers that deliberately keep token-count rows, keyed by the
/// file and the const or macro holding the statement.
const EXEMPT_SITES: &[(&str, &str, &str)] = &[
    (
        "aggregate.rs",
        "LATEST_QUOTA_BY_SEAT_SQL",
        "seat quota state, not an inference population: the freshest snapshot wins whatever the request kind",
    ),
    (
        "observation.rs",
        "OBSERVATION_TOTALS_SQL",
        "observation totals reconcile with process-wide counters that include count_tokens traffic",
    ),
];

/// Every `FROM requests` / `JOIN requests` in `src`, as `(item, guarded)`:
/// the const or macro the statement lives in, and whether the statement up
/// to its terminating `;` applies the shared predicate. Comment lines are
/// skipped; SQL never lives in one.
fn requests_sites(src: &str) -> Vec<(String, bool)> {
    // A `\n` / `\t` escape inside a literal is whitespace to SQLite but an
    // identifier byte to a word-boundary check; blank it, keeping offsets.
    let lower = src
        .to_ascii_lowercase()
        .replace("\\n", "  ")
        .replace("\\t", "  ");
    let mut sites = Vec::new();
    let mut search_from = 0;
    while let Some(found) = find_requests_ref(&lower, search_from) {
        search_from = found + 1;
        let line_start = lower[..found].rfind('\n').map_or(0, |i| i + 1);
        if lower[line_start..].trim_start().starts_with("//") {
            continue;
        }
        let end = lower[found..].find(';').map_or(lower.len(), |i| found + i);
        let guarded = src[found..end].contains("inference_rows!()");
        sites.push((enclosing_item(&src[..found]), guarded));
    }
    sites
}

/// Byte offset of the next `from` / `join` keyword followed by whitespace and
/// the bare table name `requests`.
fn find_requests_ref(lower: &str, from: usize) -> Option<usize> {
    let bytes = lower.as_bytes();
    let is_ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    (from..bytes.len()).find(|&i| {
        ["from", "join"].iter().any(|kw| {
            let after = i + kw.len();
            if !lower[i..].starts_with(kw) || (i > 0 && is_ident(bytes[i - 1])) {
                return false;
            }
            let rest = &lower[after..];
            let table = rest.trim_start();
            table.len() < rest.len()
                && table.starts_with("requests")
                && !table
                    .as_bytes()
                    .get("requests".len())
                    .copied()
                    .is_some_and(is_ident)
        })
    })
}

/// The name of the last `const NAME` or `macro_rules! NAME` before a site.
fn enclosing_item(prefix: &str) -> String {
    let const_at = prefix.rfind("const ").map(|i| i + "const ".len());
    let macro_at = prefix
        .rfind("macro_rules! ")
        .map(|i| i + "macro_rules! ".len());
    let start = const_at.max(macro_at).unwrap_or(0);
    prefix[start..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect()
}

fn is_production_query_file(path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    path.extension().and_then(|e| e.to_str()) == Some("rs")
        && !name.ends_with("_tests.rs")
        && name != "tests.rs"
        && !name.contains("test_support")
}

#[test]
fn every_requests_reader_applies_predicate_or_is_exempt() {
    // Arrange
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/query");
    let mut unguarded = Vec::new();
    let mut guarded = 0;
    for entry in std::fs::read_dir(&dir).expect("read query dir") {
        let path = entry.expect("dir entry").path();
        if !is_production_query_file(&path) {
            continue;
        }
        let file = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let src = std::fs::read_to_string(&path).expect("read source");

        // Act
        for (item, is_guarded) in requests_sites(&src) {
            if is_guarded {
                guarded += 1;
            } else {
                unguarded.push((file.to_string(), item));
            }
        }
    }
    unguarded.sort();

    // Assert: the unguarded set IS the exemption list, so a new reader and a
    // stale exemption both fail.
    let mut exempt: Vec<(String, String)> = EXEMPT_SITES
        .iter()
        .map(|(file, item, _)| ((*file).to_string(), (*item).to_string()))
        .collect();
    exempt.sort();
    assert_eq!(unguarded, exempt);
    assert!(guarded > 0, "the scan must find guarded readers");
}

#[test]
fn site_scan_flags_a_reader_without_the_predicate() {
    // Arrange
    let src = "\
// A comment naming FROM requests is not a reader.
const BARE_SQL: &str = \"SELECT 1 FROM requests WHERE a\";
const ESCAPED_SQL: &str = \"SELECT 1\\nFROM requests\";
const LOWER_SQL: &str = \"select 1 from\n   requests r\";
const JOINED_SQL: &str = \"SELECT 1 FROM x JOIN requests ON 1\";
const GUARDED_SQL: &str = concat!(\"SELECT 1 FROM requests WHERE \", inference_rows!());
const OTHER_SQL: &str = \"SELECT 1 FROM requests_archive\";";

    // Act
    let sites = requests_sites(src);

    // Assert
    assert_eq!(
        sites,
        vec![
            ("BARE_SQL".to_string(), false),
            ("ESCAPED_SQL".to_string(), false),
            ("LOWER_SQL".to_string(), false),
            ("JOINED_SQL".to_string(), false),
            ("GUARDED_SQL".to_string(), true),
        ]
    );
}

#[test]
fn predicate_names_the_persisted_count_tokens_token() {
    assert_eq!(
        inference_rows!(),
        format!("operation IS NOT '{}'", Operation::CountTokens.as_str())
    );
}
