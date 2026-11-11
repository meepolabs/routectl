//! The refreshed table file: reparse only on a changed stat, pickup of a
//! renamed-in copy, last-good retention and its WARN, and the skipped-row
//! INFO.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use routectl_testkit::{CapturedEvent, capture_events};
use serde_json::json;
use tempfile::TempDir;

use super::super::prices_test_support::{bytes, priced_model, table};
use super::*;

/// A canonical table dated `date` whose one provider id carries `marker`,
/// so a test can tell copies apart and prove the marker never reaches a log.
fn table_bytes(date: &str, marker: &str) -> Vec<u8> {
    let mut fixture = table(json!([{ "id": marker, "models": [priced_model("m")] }]));
    fixture["generated_at"] = json!(date);
    bytes(&fixture)
}

const MALFORMED: &[u8] = br#"{"schema_version":1,"generated_at":"secret-marker-text""#;

struct Dir {
    _tmp: TempDir,
    file: PathBuf,
}

impl Dir {
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let file = tmp.path().join("prices.json");
        Self { _tmp: tmp, file }
    }

    /// Writes `contents` beside the table and renames it into place, the way
    /// the refresh replaces the file, then stamps a distinct mtime so the
    /// change is visible on filesystems with coarse timestamps.
    fn rename_in(&self, contents: &[u8], mtime_secs: u64) {
        let staged = self.file.with_extension("tmp");
        fs::write(&staged, contents).expect("write staged copy");
        set_mtime(&staged, mtime_secs);
        fs::rename(&staged, &self.file).expect("rename into place");
    }
}

fn set_mtime(path: &Path, secs: u64) {
    let when = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
    File::options()
        .write(true)
        .open(path)
        .expect("open for mtime")
        .set_modified(when)
        .expect("set mtime");
}

/// The date of the table `book` serves now.
fn current_date(book: &PriceBook) -> Option<String> {
    book.current().map(|t| t.generated_at().to_string())
}

fn warns(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN)
        .collect()
}

#[test]
fn an_unchanged_stat_does_not_reparse() {
    let dir = Dir::new();
    dir.rename_in(&table_bytes("2026-11-01", "acme"), 1_000);
    let book = PriceBook::new(&dir.file);

    let first = book.current();
    let second = book.current();
    let third = book.current();

    assert_eq!(
        first
            .as_ref()
            .map(|t| t.generated_at().to_string())
            .as_deref(),
        Some("2026-11-01")
    );
    assert_eq!(
        third
            .as_ref()
            .map(|t| t.generated_at().to_string())
            .as_deref(),
        Some("2026-11-01")
    );
    assert!(Arc::ptr_eq(
        first.as_ref().expect("first"),
        second.as_ref().expect("second")
    ));
    assert_eq!(book.parse_count(), 1);
}

#[test]
fn a_renamed_in_copy_is_visible_on_the_next_read() {
    let dir = Dir::new();
    dir.rename_in(&table_bytes("2026-11-01", "acme"), 1_000);
    let book = PriceBook::new(&dir.file);
    assert_eq!(current_date(&book).as_deref(), Some("2026-11-01"));

    dir.rename_in(&table_bytes("2026-11-05", "acme"), 2_000);

    assert_eq!(current_date(&book).as_deref(), Some("2026-11-05"));
    assert_eq!(book.parse_count(), 2);
}

#[test]
fn a_same_mtime_copy_of_another_length_is_reparsed() {
    let dir = Dir::new();
    dir.rename_in(&table_bytes("2026-11-01", "acme"), 1_000);
    let book = PriceBook::new(&dir.file);
    assert_eq!(current_date(&book).as_deref(), Some("2026-11-01"));

    dir.rename_in(&table_bytes("2026-11-05", "acme-longer"), 1_000);

    assert_eq!(current_date(&book).as_deref(), Some("2026-11-05"));
}

#[test]
fn a_malformed_copy_keeps_the_last_good_table_and_warns_once_with_its_date() {
    let dir = Dir::new();
    dir.rename_in(&table_bytes("2026-11-01", "secret-marker-text"), 1_000);
    let book = PriceBook::new(&dir.file);
    let good = book.current().expect("good table");

    dir.rename_in(MALFORMED, 2_000);
    let events = capture_events(|| {
        for _ in 0..3 {
            let kept = book.current().expect("last good kept");
            assert!(Arc::ptr_eq(&kept, &good));
        }
    });

    let warned = warns(&events);
    assert_eq!(warned.len(), 1, "{events:?}");
    assert_eq!(warned[0].field("retained_table"), Some("2026-11-01"));
    let rendered = format!("{events:?}");
    assert!(!rendered.contains("secret-marker-text"), "{rendered}");
}

#[test]
fn each_distinct_bad_state_warns_once() {
    let dir = Dir::new();
    dir.rename_in(&table_bytes("2026-11-01", "acme"), 1_000);
    let book = PriceBook::new(&dir.file);
    book.current().expect("good table");

    let events = capture_events(|| {
        dir.rename_in(MALFORMED, 2_000);
        book.current();
        book.current();
        dir.rename_in(b"[]", 3_000);
        book.current();
        book.current();
    });

    let warned = warns(&events);
    assert_eq!(warned.len(), 2, "{events:?}");
    assert!(
        warned
            .iter()
            .all(|e| e.field("retained_table") == Some("2026-11-01"))
    );
}

#[test]
fn a_good_copy_after_a_bad_one_replaces_the_retained_table() {
    let dir = Dir::new();
    dir.rename_in(&table_bytes("2026-11-01", "acme"), 1_000);
    let book = PriceBook::new(&dir.file);
    book.current().expect("good table");
    dir.rename_in(MALFORMED, 2_000);
    book.current();

    dir.rename_in(&table_bytes("2026-11-07", "acme"), 3_000);

    assert_eq!(current_date(&book).as_deref(), Some("2026-11-07"));
}

#[test]
fn an_absent_file_is_no_table_and_no_warn() {
    let dir = Dir::new();
    let book = PriceBook::new(&dir.file);

    let events = capture_events(|| {
        assert!(book.current().is_none());
        assert!(book.current().is_none());
    });

    assert!(warns(&events).is_empty(), "{events:?}");
    assert_eq!(book.parse_count(), 0);
}

#[test]
fn a_removed_file_drops_the_refreshed_table() {
    let dir = Dir::new();
    dir.rename_in(&table_bytes("2026-11-01", "acme"), 1_000);
    let book = PriceBook::new(&dir.file);
    book.current().expect("good table");

    fs::remove_file(&dir.file).expect("remove");

    assert!(book.current().is_none());
}

#[test]
fn a_bad_first_read_warns_with_no_retained_table() {
    let dir = Dir::new();
    dir.rename_in(MALFORMED, 1_000);
    let book = PriceBook::new(&dir.file);

    let events = capture_events(|| assert!(book.current().is_none()));

    let warned = warns(&events);
    assert_eq!(warned.len(), 1, "{events:?}");
    assert_eq!(warned[0].field("retained_table"), Some("none"));
}

#[test]
fn a_file_over_the_input_bound_is_refused_before_it_is_read() {
    let dir = Dir::new();
    dir.rename_in(&table_bytes("2026-11-01", "acme"), 1_000);
    let book = PriceBook::new(&dir.file);
    book.current().expect("good table");

    let file = File::options().write(true).open(&dir.file).expect("open");
    file.set_len(MAX_INPUT_BYTES as u64 + 1).expect("extend");
    drop(file);

    let events = capture_events(|| {
        let kept = book.current();
        assert_eq!(
            kept.as_ref()
                .map(|t| t.generated_at().to_string())
                .as_deref(),
            Some("2026-11-01")
        );
    });
    let warned = warns(&events);
    assert_eq!(warned.len(), 1, "{events:?}");
    assert_eq!(
        warned[0].field("error"),
        Some(PriceTableError::TooLarge.to_string().as_str())
    );
}

#[test]
fn concurrent_readers_share_one_table() {
    let dir = Dir::new();
    dir.rename_in(&table_bytes("2026-11-01", "acme"), 1_000);
    let book = Arc::new(PriceBook::new(&dir.file));

    let dates: Vec<Option<String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..8)
            .map(|_| scope.spawn(|| current_date(&book)))
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("join"))
            .collect()
    });

    assert!(dates.iter().all(|d| d.as_deref() == Some("2026-11-01")));
    assert!(book.parse_count() <= 8);
}

#[test]
fn the_book_is_shareable_across_tasks() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<PriceBook>();
}

fn skip_notes(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|e| e.level == tracing::Level::INFO && e.field("skipped_rows").is_some())
        .collect()
}

/// An accepted table that skipped rows logs the count once per file state,
/// never a skipped id; a table that skipped none logs nothing.
#[test]
fn skipped_rows_log_their_count_once_per_file_state() {
    let dir = Dir::new();
    let unusable_model = "secret marker model";
    let mut skipping = table(json!([{
        "id": "acme",
        "models": [priced_model("m"), priced_model(unusable_model), priced_model("")],
    }]));
    skipping["generated_at"] = json!("2026-11-02");
    dir.rename_in(&table_bytes("2026-11-01", "acme"), 1_000);
    let book = PriceBook::new(&dir.file);

    let clean = capture_events(|| {
        book.current().expect("clean table");
    });
    let events = capture_events(|| {
        dir.rename_in(&bytes(&skipping), 2_000);
        for _ in 0..3 {
            book.current().expect("skipping table");
        }
    });

    assert!(skip_notes(&clean).is_empty(), "{clean:?}");
    let noted = skip_notes(&events);
    assert_eq!(noted.len(), 1, "{events:?}");
    assert_eq!(noted[0].field("skipped_rows"), Some("2"));
    assert_eq!(noted[0].field("table"), Some("2026-11-02"));
    let rendered = format!("{events:?}");
    assert!(!rendered.contains(unusable_model), "{rendered}");
}

/// A refreshed copy whose legacy write key and `cache_write` object price one
/// lifetime differently is refused like any structural fault: the last good
/// table stays and one WARN names the refusal.
#[test]
fn a_copy_with_disagreeing_cache_write_spellings_keeps_the_last_good_table() {
    let dir = Dir::new();
    dir.rename_in(&table_bytes("2026-11-01", "acme"), 1_000);
    let book = PriceBook::new(&dir.file);
    let good = book.current().expect("good table");
    let mut conflicting = table(json!([{ "id": "acme", "models": [
        priced_model("m"),
        { "id": "n", "cache_write_5m": 1.25, "cache_write": { "300": 1.5 } },
    ] }]));
    conflicting["generated_at"] = json!("2026-11-05");

    let events = capture_events(|| {
        dir.rename_in(&bytes(&conflicting), 2_000);
        for _ in 0..3 {
            let kept = book.current().expect("last good kept");
            assert!(Arc::ptr_eq(&kept, &good));
        }
    });

    let warned = warns(&events);
    assert_eq!(warned.len(), 1, "{events:?}");
    assert_eq!(warned[0].field("retained_table"), Some("2026-11-01"));
    assert_eq!(
        warned[0].field("error"),
        Some(
            PriceTableError::CacheWriteAliasConflict
                .to_string()
                .as_str()
        )
    );
}
