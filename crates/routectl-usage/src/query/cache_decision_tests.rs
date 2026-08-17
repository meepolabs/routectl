//! Tests for the cache-breakpoint decision read query.

use super::*;
use crate::db::open;
use tempfile::TempDir;

fn open_db() -> (TempDir, UsageDb) {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("usage.db");
    let db = open(&path).expect("open");
    (dir, db)
}

/// Insert a row with explicit v16 decision columns. Each is `Option` so the
/// NULL-vs-decided accounting is exercisable.
fn insert_decision_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    front: Option<&str>,
    terminal: Option<&str>,
    epoch: Option<i64>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, stream, outcome, latency_ms, tool_count, \
             msg_count, attempt_count, fallback_count, cache_front_decision, \
             cache_terminal_decision, prefix_epoch_event) \
             VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 0, 'ok', 0, 0, 0, 1, 0, ?3, ?4, ?5)",
            rusqlite::params![ts_start, request_id, front, terminal, epoch],
        )
        .expect("insert decision row");
}

#[test]
fn counts_decided_and_emitted_per_region_ignoring_undecided_rows() {
    // Arrange: an emitted pair, a declined pair, and an undecided (pre-v16
    // shaped) row that must land in neither count.
    let (_dir, db) = open_db();
    insert_decision_row(
        &db,
        "d1",
        100,
        Some("auto_emitted"),
        Some("auto_emitted"),
        None,
    );
    insert_decision_row(
        &db,
        "d2",
        110,
        Some("auto_emitted"),
        Some("auto_skipped:breakpoint_cap"),
        None,
    );
    insert_decision_row(&db, "d3", 120, None, None, None);

    // Act
    let s = cache_decision_summary(&db, 0, 1000).expect("summary");

    // Assert: COUNT ignores the NULL row; the two regions are counted apart.
    assert_eq!(s.front_decided, 2);
    assert_eq!(s.front_emitted, 2);
    assert_eq!(s.terminal_decided, 2);
    assert_eq!(s.terminal_emitted, 1);
}

#[test]
fn partitions_the_prefix_epoch_events_and_ignores_unclassified_rows() {
    // Arrange: one of each event value plus an unclassified (NULL) row.
    let (_dir, db) = open_db();
    insert_decision_row(&db, "e0", 100, None, None, Some(PREFIX_EPOCH_STABLE));
    insert_decision_row(&db, "e1", 110, None, None, Some(PREFIX_EPOCH_REWRITTEN));
    insert_decision_row(&db, "e2", 120, None, None, Some(PREFIX_EPOCH_RESEEDED));
    insert_decision_row(&db, "e3", 130, None, None, Some(PREFIX_EPOCH_STABLE));
    insert_decision_row(&db, "unclassified", 140, None, None, None);

    // Act
    let s = cache_decision_summary(&db, 0, 1000).expect("summary");

    // Assert
    assert_eq!(
        s.epoch_classified, 4,
        "the row with no comparable prior prefix is excluded"
    );
    assert_eq!(s.epoch_stable, 2);
    assert_eq!(s.epoch_rewritten, 1);
    assert_eq!(s.epoch_reseeded, 1);
}

#[test]
fn restricts_to_the_requested_window() {
    // Arrange: one in-window row and one before it.
    let (_dir, db) = open_db();
    insert_decision_row(
        &db,
        "inside",
        100,
        Some("auto_emitted"),
        Some("auto_emitted"),
        Some(PREFIX_EPOCH_REWRITTEN),
    );
    insert_decision_row(
        &db,
        "before",
        5,
        Some("auto_emitted"),
        Some("auto_emitted"),
        Some(PREFIX_EPOCH_REWRITTEN),
    );

    // Act
    let s = cache_decision_summary(&db, 100, 1000).expect("summary");

    // Assert
    assert_eq!(s.front_decided, 1);
    assert_eq!(s.terminal_decided, 1);
    assert_eq!(s.epoch_rewritten, 1);
}

#[test]
fn on_an_empty_ledger_returns_all_zeros() {
    // Arrange
    let (_dir, db) = open_db();

    // Act
    let s = cache_decision_summary(&db, 0, 1000).expect("summary");

    // Assert
    assert_eq!(s, CacheDecisionSummary::default());
}

// --- the bounded suppressed-session finder ------------------------------

/// Insert a row with an explicit `(session_id, provider_kind, model)` triple
/// and explicit v16 decision columns, so the finder's grouping, its
/// count-once-per-request rule, and its triple-completeness filter are all
/// exercisable.
#[allow(clippy::too_many_arguments)]
fn insert_triple_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    session_id: Option<&str>,
    provider_kind: Option<&str>,
    model: Option<&str>,
    front: Option<&str>,
    terminal: Option<&str>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, stream, outcome, latency_ms, tool_count, \
             msg_count, attempt_count, fallback_count, session_id, provider_kind, \
             model, cache_front_decision, cache_terminal_decision) \
             VALUES (?1, ?1, ?2, 'openai', 'm', 'a', 0, 'ok', 0, 0, 0, 1, 0, ?3, \
             ?4, ?5, ?6, ?7)",
            rusqlite::params![
                ts_start,
                request_id,
                session_id,
                provider_kind,
                model,
                front,
                terminal
            ],
        )
        .expect("insert triple row");
}

/// Insert a suppressed request under the canonical triple used by the
/// finder tests, with the token on the front column only.
fn suppressed_row(db: &UsageDb, request_id: &str, ts_start: i64, session_id: &str, model: &str) {
    insert_triple_row(
        db,
        request_id,
        ts_start,
        Some(session_id),
        Some("anthropic-api"),
        Some(model),
        Some(K_SUPPRESSION_TOKEN),
        None,
    );
}

fn find_triple<'a>(
    found: &'a SuppressedSessions,
    session_id: &str,
    model: &str,
) -> &'a SuppressedSessionRow {
    let want = session_ref(session_id);
    found
        .rows
        .iter()
        .find(|r| r.session_ref == want && r.model == model)
        .expect("triple present")
}

#[test]
fn groups_suppressed_requests_by_the_session_provider_kind_model_triple() {
    // Arrange: one session across two models, plus a second session on the
    // first model -- three distinct triples, five suppressed requests.
    let (_dir, db) = open_db();
    suppressed_row(&db, "s1", 100, "sess-a", "haiku");
    suppressed_row(&db, "s2", 110, "sess-a", "haiku");
    suppressed_row(&db, "s3", 120, "sess-a", "sonnet");
    suppressed_row(&db, "s4", 130, "sess-b", "haiku");
    suppressed_row(&db, "s5", 140, "sess-b", "haiku");

    // Act
    let found = suppressed_sessions(&db, 0, 1000).expect("finder");

    // Assert
    assert_eq!(found.rows.len(), 3, "one row per triple: {found:?}");
    assert_eq!(
        find_triple(&found, "sess-a", "haiku").suppressed_requests,
        2
    );
    assert_eq!(
        find_triple(&found, "sess-a", "sonnet").suppressed_requests,
        1
    );
    assert_eq!(
        find_triple(&found, "sess-b", "haiku").suppressed_requests,
        2
    );
}

#[test]
fn reports_the_first_and_last_suppression_instant_per_triple() {
    // Arrange: three suppressions of one triple, inserted out of time order.
    let (_dir, db) = open_db();
    suppressed_row(&db, "t2", 250, "sess-a", "haiku");
    suppressed_row(&db, "t0", 100, "sess-a", "haiku");
    suppressed_row(&db, "t1", 180, "sess-a", "haiku");

    // Act
    let found = suppressed_sessions(&db, 0, 1000).expect("finder");

    // Assert
    let row = find_triple(&found, "sess-a", "haiku");
    assert_eq!(row.first_suppressed_ms, 100);
    assert_eq!(row.last_suppressed_ms, 250);
}

#[test]
fn a_request_suppressed_on_both_markers_counts_as_one_suppressed_request() {
    // Arrange: suppression withholds BOTH markers under one shared verdict,
    // so the row carries the token twice but is ONE suppressed request.
    let (_dir, db) = open_db();
    insert_triple_row(
        &db,
        "both",
        100,
        Some("sess-a"),
        Some("anthropic-api"),
        Some("haiku"),
        Some(K_SUPPRESSION_TOKEN),
        Some(K_SUPPRESSION_TOKEN),
    );

    // Act
    let found = suppressed_sessions(&db, 0, 1000).expect("finder");

    // Assert
    assert_eq!(
        find_triple(&found, "sess-a", "haiku").suppressed_requests,
        1,
        "front + terminal suppression is one withheld request, not two"
    );
}

#[test]
fn a_terminal_only_suppression_is_found() {
    // Arrange: only the terminal column carries the token.
    let (_dir, db) = open_db();
    insert_triple_row(
        &db,
        "term",
        100,
        Some("sess-a"),
        Some("anthropic-api"),
        Some("haiku"),
        None,
        Some(K_SUPPRESSION_TOKEN),
    );

    // Act
    let found = suppressed_sessions(&db, 0, 1000).expect("finder");

    // Assert
    assert_eq!(
        find_triple(&found, "sess-a", "haiku").suppressed_requests,
        1
    );
}

#[test]
fn excludes_rows_carrying_a_different_decision_token() {
    // Arrange: an emitted row and a differently-skipped row under the same
    // triple as one genuine suppression.
    let (_dir, db) = open_db();
    suppressed_row(&db, "yes", 100, "sess-a", "haiku");
    insert_triple_row(
        &db,
        "emitted",
        110,
        Some("sess-a"),
        Some("anthropic-api"),
        Some("haiku"),
        Some("auto_emitted"),
        Some("auto_emitted"),
    );
    insert_triple_row(
        &db,
        "other-skip",
        120,
        Some("sess-a"),
        Some("anthropic-api"),
        Some("haiku"),
        Some("auto_skipped:breakpoint_cap"),
        None,
    );

    // Act
    let found = suppressed_sessions(&db, 0, 1000).expect("finder");

    // Assert
    assert_eq!(found.rows.len(), 1);
    assert_eq!(
        find_triple(&found, "sess-a", "haiku").suppressed_requests,
        1
    );
}

#[test]
fn excludes_a_suppressed_row_missing_part_of_the_triple() {
    // Arrange: the K estimator keys on the full triple, so a row missing any
    // part of it cannot be attributed to a suppressible triple.
    let (_dir, db) = open_db();
    insert_triple_row(
        &db,
        "no-session",
        100,
        None,
        Some("anthropic-api"),
        Some("haiku"),
        Some(K_SUPPRESSION_TOKEN),
        None,
    );
    insert_triple_row(
        &db,
        "no-kind",
        110,
        Some("sess-a"),
        None,
        Some("haiku"),
        Some(K_SUPPRESSION_TOKEN),
        None,
    );
    insert_triple_row(
        &db,
        "no-model",
        120,
        Some("sess-a"),
        Some("anthropic-api"),
        None,
        Some(K_SUPPRESSION_TOKEN),
        None,
    );

    // Act
    let found = suppressed_sessions(&db, 0, 1000).expect("finder");

    // Assert
    assert!(found.rows.is_empty(), "{found:?}");
}

#[test]
fn orders_triples_newest_suppression_first_deterministically() {
    // Arrange: three triples whose last suppression is strictly ordered, plus
    // a pair sharing an identical first/last instant so the key tiebreak is
    // the thing under test.
    let (_dir, db) = open_db();
    suppressed_row(&db, "old", 100, "sess-old", "haiku");
    suppressed_row(&db, "mid", 200, "sess-mid", "haiku");
    suppressed_row(&db, "new", 300, "sess-new", "haiku");
    suppressed_row(&db, "tie-a", 250, "sess-tie", "aaa");
    suppressed_row(&db, "tie-b", 250, "sess-tie", "bbb");

    // Act: two independent reads of the same ledger.
    let first = suppressed_sessions(&db, 0, 1000).expect("finder");
    let second = suppressed_sessions(&db, 0, 1000).expect("finder");

    // Assert: newest-first, and byte-identical across reads.
    let order: Vec<(u64, &str)> = first
        .rows
        .iter()
        .map(|r| (r.session_ref, r.model.as_str()))
        .collect();
    assert_eq!(
        order,
        vec![
            (session_ref("sess-new"), "haiku"),
            (session_ref("sess-tie"), "aaa"),
            (session_ref("sess-tie"), "bbb"),
            (session_ref("sess-mid"), "haiku"),
            (session_ref("sess-old"), "haiku"),
        ],
    );
    assert_eq!(first, second, "repeat reads must agree");
}

#[test]
fn caps_the_result_and_reports_truncation_when_more_triples_exist() {
    // Arrange: one more triple than the cap, each with a distinct recency.
    let (_dir, db) = open_db();
    let over = SUPPRESSED_SESSION_CAP + 1;
    for i in 0..over {
        let session = format!("sess-{i:03}");
        suppressed_row(
            &db,
            &format!("r{i:03}"),
            100 + i64::try_from(i).expect("small index"),
            &session,
            "haiku",
        );
    }

    // Act
    let found = suppressed_sessions(&db, 0, 10_000).expect("finder");

    // Assert: a newest-first prefix, flagged as incomplete.
    assert_eq!(found.rows.len(), SUPPRESSED_SESSION_CAP);
    assert!(found.truncated, "more matches than the cap");
    assert_eq!(
        found.rows[0].session_ref,
        session_ref(&format!("sess-{:03}", over - 1)),
        "the prefix starts at the newest suppression"
    );
}

#[test]
fn a_result_exactly_at_the_cap_is_not_reported_as_truncated() {
    // Arrange
    let (_dir, db) = open_db();
    for i in 0..SUPPRESSED_SESSION_CAP {
        suppressed_row(
            &db,
            &format!("r{i:03}"),
            100 + i64::try_from(i).expect("small index"),
            &format!("sess-{i:03}"),
            "haiku",
        );
    }

    // Act
    let found = suppressed_sessions(&db, 0, 10_000).expect("finder");

    // Assert
    assert_eq!(found.rows.len(), SUPPRESSED_SESSION_CAP);
    assert!(!found.truncated);
}

#[test]
fn the_finder_restricts_to_the_requested_window() {
    // Arrange: one suppression inside the window and one before it.
    let (_dir, db) = open_db();
    suppressed_row(&db, "inside", 100, "sess-in", "haiku");
    suppressed_row(&db, "before", 5, "sess-out", "haiku");

    // Act
    let found = suppressed_sessions(&db, 100, 1000).expect("finder");

    // Assert
    assert_eq!(found.rows.len(), 1);
    assert_eq!(found.rows[0].session_ref, session_ref("sess-in"));
}

#[test]
fn on_a_ledger_with_no_suppression_the_finder_returns_nothing() {
    // Arrange
    let (_dir, db) = open_db();
    insert_triple_row(
        &db,
        "emitted",
        100,
        Some("sess-a"),
        Some("anthropic-api"),
        Some("haiku"),
        Some("auto_emitted"),
        Some("auto_emitted"),
    );

    // Act + Assert
    let found = suppressed_sessions(&db, 0, 1000).expect("finder");
    assert_eq!(found, SuppressedSessions::default());
}

#[test]
fn the_raw_session_key_is_never_returned() {
    // Arrange: a session key that would be a durable personal identifier if
    // it reached an operator-facing surface.
    let (_dir, db) = open_db();
    let raw = "user@example.test";
    suppressed_row(&db, "s1", 100, raw, "haiku");

    // Act
    let found = suppressed_sessions(&db, 0, 1000).expect("finder");

    // Assert: the row carries an opaque reference, and nothing renders the key.
    assert_eq!(found.rows.len(), 1);
    assert_eq!(found.rows[0].session_ref, session_ref(raw));
    assert!(
        !format!("{found:?}").contains(raw),
        "the raw session key must not travel with the row: {found:?}"
    );
}
