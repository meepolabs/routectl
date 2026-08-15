//! Tests for the calibration startup warm: the migrating-open ordering, the
//! served-nickname key (with the wire-id negative control), agreement with
//! live admission, the row-cap warning, and the failed-read posture.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use routectl_auth::{MemoryStore, SecretStore};
use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry};
use routectl_usage::{CHANNEL_CAPACITY, UsageWriter, open};
use tempfile::TempDir;

use super::*;
use crate::server::build_router_from_config;

/// The nickname the tests' resolved-model table declares, and the DIFFERENT
/// upstream wire id it maps to. Two distinct strings is the whole point: a
/// query keyed on the wire id must find nothing the gate can look up.
const NICKNAME: &str = "opus-lane";
const WIRE_ID: &str = "claude-opus-4-5-20251101";
const PROVIDER_KIND: &str = "openai-compat";

/// A router whose resolved-model table holds exactly `NICKNAME`, mapped to a
/// different upstream wire id.
async fn router_with_one_lane(tmp: &TempDir) -> Router {
    // A file:// credential ref rather than an inline one: the model builder
    // refuses `literal:` outright, and an env:// ref would force every test
    // here to serialize on the process environment.
    let key_path = tmp.path().join("api-key");
    std::fs::write(&key_path, b"test-key").expect("write key file");
    // The auth layer refuses a group- or world-readable secret file.
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
        .expect("restrict key file");
    let mut providers = BTreeMap::new();
    providers.insert(
        "p1".to_string(),
        ProviderEntry::openai_compat(
            "https://example.invalid/v1",
            format!("file://{}", key_path.display()),
        ),
    );
    let mut models = BTreeMap::new();
    models.insert(
        NICKNAME.to_string(),
        ModelEntry::new("p1".to_string(), WIRE_ID.to_string()),
    );
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".to_string(), AliasValue::Single(NICKNAME.to_string()));
    let mut config = Config {
        providers,
        models,
        aliases,
        ..Config::default()
    };
    config.usage.db_path = tmp.path().join("router-usage.db");
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    build_router_from_config(Arc::new(config), secrets)
        .await
        .expect("build router")
}

fn temp_db_path() -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    (dir, path)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after epoch")
        .as_millis() as i64
}

/// Insert one evidence row directly, so the reader's mapping and the query's
/// admission filters can be exercised against a real read-only open.
#[allow(clippy::too_many_arguments)]
fn insert_evidence_row(
    db: &routectl_usage::UsageDb,
    request_id: &str,
    ts_start: i64,
    session_id: Option<&str>,
    provider_kind: &str,
    model: &str,
    estimated: Option<i64>,
    prompt: Option<i64>,
    outcome: &str,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider_kind, session_id, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             calib_estimated_tokens, calib_prompt_tokens) \
             VALUES (?1, ?1, ?2, 'openai', ?3, 'fast', ?4, ?5, ?6, 0, ?7, 5, 0, 0, 1, 0, ?8, ?9)",
            rusqlite::params![
                ts_start,
                request_id,
                WIRE_ID,
                model,
                provider_kind,
                session_id,
                outcome,
                estimated,
                prompt,
            ],
        )
        .expect("insert evidence row");
}

/// Seed enough balanced evidence under `model` for one lane to clear the
/// reduction's sample and cohort floors, all carrying the same ratio.
fn seed_balanced_lane(db: &routectl_usage::UsageDb, model: &str, prompt: i64) {
    let base = now_ms();
    for i in 0..9 {
        insert_evidence_row(
            db,
            &format!("{model}-r{i}"),
            base - i64::from(9 - i),
            Some(&format!("caller-{}", i % 3)),
            PROVIDER_KIND,
            model,
            Some(10_000),
            Some(prompt),
            "ok",
        );
    }
}

/// How many request rows a ledger holds, so a filter test can distinguish
/// "the row was never written" from "the query refused it".
fn row_count(path: &Path) -> i64 {
    let db = open(path).expect("open ledger");
    db.conn()
        .query_row("SELECT COUNT(*) FROM requests", [], |row| row.get(0))
        .expect("count rows")
}

#[tokio::test]
async fn a_restart_recovers_the_factor_the_previous_process_had_learned() {
    // Arrange: a ledger holding one lane's worth of evidence, written under
    // the SERVED NICKNAME as the live path writes it.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    seed_balanced_lane(&db, NICKNAME, 12_000);
    drop(db);
    let tmp = TempDir::new().expect("tempdir");
    let router = router_with_one_lane(&tmp).await;

    // Act
    let summary = warm_calibration_from_ledger(&path, &router);

    // Assert: every seeded row was admitted and the lane came back
    // calibrated. The exact factor the same evidence reduces to is pinned in
    // the router crate, where the reduction lives.
    assert_eq!(summary.rows_loaded, 9);
    assert_eq!(summary.accepted, 9);
    assert_eq!(summary.lanes_calibrated, 1);
}

/// The trap this repo has already paid for once: the rebuild can only key on
/// what the ledger STORES, and it must produce the same lane key the live
/// write and the gate lookup use -- the served nickname. Keyed on the upstream
/// wire id, nothing ever matches, no lane ever calibrates, and there is no
/// error at all: just permanent silence that reads as health.
#[tokio::test]
async fn evidence_stored_under_the_wire_id_calibrates_zero_lanes() {
    // Arrange: the SAME evidence, differing only in which label the `model`
    // column carries -- the upstream wire id instead of the served nickname.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    seed_balanced_lane(&db, WIRE_ID, 12_000);
    drop(db);
    let tmp = TempDir::new().expect("tempdir");
    let router = router_with_one_lane(&tmp).await;

    // Act
    let summary = warm_calibration_from_ledger(&path, &router);

    // Assert: nine rows load and ZERO lanes calibrate. The wire id is not in
    // the resolved-model table, so it is dropped rather than becoming a lane
    // that can never serve -- and nothing errors, which is exactly why this
    // trap reads as health.
    assert_eq!(summary.rows_loaded, 9);
    assert_eq!(summary.accepted, 0);
    assert_eq!(summary.rejected_unknown_nickname, 9);
    assert_eq!(
        summary.lanes_calibrated, 0,
        "keying on the upstream wire id must calibrate no lane at all"
    );
}

/// Live admission and rebuild admission must agree row for row. A mid-stream
/// failure and a zero-total success are both refused by the live finalize
/// (which nulls the pair as a UNIT), so the rebuild must refuse them too --
/// otherwise a restart admits rows live traffic never would.
#[tokio::test]
async fn rebuild_rejects_exactly_what_live_admission_rejects() {
    // Arrange: nine admissible rows plus three the live path would refuse --
    // a non-success carrying a full pair, a success whose estimate half is
    // NULL, and a success whose prompt half is NULL.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    seed_balanced_lane(&db, NICKNAME, 12_000);
    let base = now_ms();
    insert_evidence_row(
        &db,
        "failed-with-pair",
        base,
        Some("caller-0"),
        PROVIDER_KIND,
        NICKNAME,
        Some(10_000),
        Some(90_000),
        "upstream_error",
    );
    insert_evidence_row(
        &db,
        "ok-null-estimate",
        base,
        Some("caller-1"),
        PROVIDER_KIND,
        NICKNAME,
        None,
        Some(90_000),
        "ok",
    );
    insert_evidence_row(
        &db,
        "ok-null-prompt",
        base,
        Some("caller-2"),
        PROVIDER_KIND,
        NICKNAME,
        Some(10_000),
        None,
        "ok",
    );
    drop(db);
    let tmp = TempDir::new().expect("tempdir");
    let router = router_with_one_lane(&tmp).await;

    // Act
    let summary = warm_calibration_from_ledger(&path, &router);

    // Assert: all twelve rows are in the ledger, and only the nine
    // admissible ones are handed to the store at all -- the three the live
    // finalize would have refused are filtered at the query, so they cannot
    // reach the reduction to move the lane's factor.
    assert_eq!(row_count(&path), 12, "sanity: every row was persisted");
    assert_eq!(
        summary.rows_loaded, 9,
        "a row live traffic would reject must not even be loaded"
    );
    assert_eq!(summary.accepted, 9);
    assert_eq!(summary.lanes_calibrated, 1);
}

#[tokio::test]
async fn an_unreadable_ledger_leaves_every_lane_uncorrected() {
    // Arrange: a non-DB file at the path. The migrating open fails on it, so
    // the rebuild never runs a query.
    let (_dir, path) = temp_db_path();
    std::fs::write(&path, b"this is not a sqlite database").expect("write junk");
    let tmp = TempDir::new().expect("tempdir");
    let router = router_with_one_lane(&tmp).await;

    // Act
    let summary = warm_calibration_from_ledger(&path, &router);

    // Assert: not one row read, so not one factor produced -- a partial read
    // must never become a correction.
    assert_eq!(summary, CalibrationRebuildSummary::default());
}

#[tokio::test]
async fn an_absent_ledger_warms_nothing_and_does_not_warn() {
    // A missing DB is the cold start, not a failure: the migrating open
    // creates it empty and the query finds no rows.
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("absent.db");
    let tmp = TempDir::new().expect("tempdir");
    let router = router_with_one_lane(&tmp).await;

    let mut summary = CalibrationRebuildSummary::default();
    let events = routectl_testkit::capture_events(|| {
        summary = warm_calibration_from_ledger(&path, &router);
    });

    assert_eq!(summary.rows_loaded, 0);
    assert_eq!(summary.lanes_calibrated, 0);
    assert!(
        !events.iter().any(|e| e.level == tracing::Level::WARN),
        "a cold start must not emit a read-failure warning"
    );
}

/// Stale evidence is judged against the CURRENT clock, not against the row
/// timestamps alone: a lane whose newest pair predates the reduction's age
/// bound must come back uncorrected rather than calibrated on history.
#[tokio::test]
async fn evidence_older_than_the_age_bound_does_not_come_back_calibrated() {
    // Arrange: a full lane's worth of evidence, all stamped well before the
    // reduction's age bound.
    let (_dir, path) = temp_db_path();
    let db = open(&path).expect("open");
    let stale = now_ms() - 1000 * 60 * 60 * 48;
    for i in 0..9 {
        insert_evidence_row(
            &db,
            &format!("stale-r{i}"),
            stale + i64::from(i),
            Some(&format!("caller-{}", i % 3)),
            PROVIDER_KIND,
            NICKNAME,
            Some(10_000),
            Some(12_000),
            "ok",
        );
    }
    drop(db);
    let tmp = TempDir::new().expect("tempdir");
    let router = router_with_one_lane(&tmp).await;

    // Act
    let summary = warm_calibration_from_ledger(&path, &router);

    // Assert: the read window itself excludes evidence the reducer could not
    // have used, so the lane comes back uncorrected.
    assert_eq!(summary.rows_loaded, 0);
    assert_eq!(summary.lanes_calibrated, 0);
}

#[test]
fn rebuild_log_reports_the_tally_and_stays_quiet_under_the_cap() {
    let summary = CalibrationRebuildSummary {
        rows_loaded: REBUILD_ROW_LIMIT - 1,
        accepted: 40,
        rejected_unknown_nickname: 2,
        rejected_pair: 1,
        lanes_calibrated: 3,
    };

    let events = routectl_testkit::capture_events(|| emit_rebuild_log(&summary));

    let info = events
        .iter()
        .find(|e| e.level == tracing::Level::INFO)
        .expect("info rebuild log emitted");
    assert_eq!(info.field("rows_loaded"), Some("4999"));
    assert_eq!(info.field("accepted"), Some("40"));
    assert_eq!(info.field("rejected_unknown_nickname"), Some("2"));
    assert_eq!(info.field("rejected_pair"), Some("1"));
    assert_eq!(info.field("lanes_calibrated"), Some("3"));
    assert!(
        !events.iter().any(|e| e.level == tracing::Level::WARN),
        "no cap-hit warning under the row cap"
    );
}

#[test]
fn rebuild_log_warns_when_the_row_cap_truncated_the_read() {
    // A silent truncation reads as "we loaded everything", so hitting the cap
    // must warn rather than pass on the info line alone.
    let summary = CalibrationRebuildSummary {
        rows_loaded: REBUILD_ROW_LIMIT,
        accepted: REBUILD_ROW_LIMIT,
        lanes_calibrated: 5,
        ..CalibrationRebuildSummary::default()
    };

    let events = routectl_testkit::capture_events(|| emit_rebuild_log(&summary));

    let warn = events
        .iter()
        .find(|e| e.level == tracing::Level::WARN)
        .expect("cap-hit warning emitted");
    assert!(warn.message.contains("hit the row cap"));
    assert_eq!(warn.field("rows_loaded"), Some("5000"));
    assert_eq!(warn.field("row_cap"), Some("5000"));
}

/// The evidence columns exist only after the newest migration, and a
/// read-only open rejects an older schema outright -- so a warm that merely
/// hoped the migration had already run would read nothing at all. Driving a
/// row through the REAL writer proves the migrating open the warm performs
/// leaves a database the writer then attaches to without re-migrating.
#[tokio::test]
async fn the_warm_migrates_before_reading_and_the_writer_still_attaches() {
    // Arrange: an absent ledger, warmed FIRST (bootstrap order) so the warm
    // itself performs the migrating open.
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    let tmp = TempDir::new().expect("tempdir");
    let router = router_with_one_lane(&tmp).await;
    let cold = warm_calibration_from_ledger(&path, &router);
    assert_eq!(cold.lanes_calibrated, 0, "an empty ledger warms no lane");

    // Act: start the real writer against the SAME file the warm just
    // migrated, push one row through it, then warm again as a fresh boot
    // would.
    let (handle, writer) = UsageWriter::start(path.clone(), CHANNEL_CAPACITY, 0, true);
    drop(handle);
    writer.shutdown();
    let db = open(&path).expect("reopen after writer");
    seed_balanced_lane(&db, NICKNAME, 12_000);
    drop(db);
    let restarted = router_with_one_lane(&tmp).await;
    let summary = warm_calibration_from_ledger(&path, &restarted);

    // Assert: the post-writer read found the evidence columns, so the schema
    // survived both opens.
    assert_eq!(summary.accepted, 9);
    assert_eq!(summary.lanes_calibrated, 1);
}

/// The warm is BOOTSTRAP ONLY. Re-running it on a hot reload would clobber
/// fresher live samples with older ledger history; a reload instead CARRIES
/// the live store over, which is a different mechanism entirely. Neither
/// property is observable from a unit call, so this is a structural guard on
/// the wiring: the reload coordinator must carry over and must not warm.
#[test]
fn the_reload_coordinator_carries_the_store_over_and_never_re_warms() {
    let reload_src = include_str!("reload.rs");
    assert!(
        !reload_src.contains("warm_calibration_from_ledger"),
        "a hot reload must not re-warm from the ledger -- it would replace \
         fresher live samples with older history"
    );
    assert_eq!(
        reload_src.matches("carry_over_calibration_from").count(),
        2,
        "both reload paths must carry the learned lanes onto the new router"
    );
}
