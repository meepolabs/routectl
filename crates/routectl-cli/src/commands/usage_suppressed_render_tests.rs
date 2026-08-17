// Render-layer tests for the `--detail` K-suppressed-session finder block.
// Split from `usage_render_tests.rs` to keep each file under the size ceiling;
// `include!`d into the same `tests` module so all helpers there (`temp_db`,
// `cost_config`, `report_all`, `paid_row`, ...) are in scope. All imports come
// from the enclosing `usage_tests.rs`; do not add `use` lines here.

/// The canonical suppression token. The vocabulary's own spelling lives on
/// `CacheInjection::strategy_str` in the router crate and is not exported, so
/// it is restated here; the query layer holds the other copy.
const K_SUPPRESSION_TOKEN: &str = "auto_skipped:k_below_break_even";

/// Insert a row with an explicit `(session_id, provider_kind, model)` triple
/// and explicit v16 decision columns, so the finder block is testable from the
/// ledger up.
#[allow(clippy::too_many_arguments)]
fn decision_triple_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    session_id: &str,
    provider_kind: &str,
    model: &str,
    front: Option<&str>,
    terminal: Option<&str>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, provider, upstream, stream, outcome, latency_ms, \
             tool_count, msg_count, attempt_count, fallback_count, session_id, \
             provider_kind, model, cache_front_decision, cache_terminal_decision) \
             VALUES (?1, ?1, ?2, 'openai', 'req-model', 'al', 'paid', 'up-paid', 0, \
             'ok', 5, 0, 0, 1, 0, ?3, ?4, ?5, ?6, ?7)",
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
        .expect("insert decision triple row");
}

/// A K-suppressed request under the given session and model, token on the
/// front column.
fn k_suppressed_row(db: &UsageDb, request_id: &str, ts_start: i64, session: &str, model: &str) {
    decision_triple_row(
        db,
        request_id,
        ts_start,
        session,
        "anthropic-api",
        model,
        Some(K_SUPPRESSION_TOKEN),
        None,
    );
}

/// The finder block's lines, from its header to the first blank line.
fn suppressed_block(out: &str) -> String {
    out.lines()
        .skip_while(|l| !l.starts_with("k-suppressed sessions:"))
        .take_while(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn render_detail_shows_one_finder_line_per_suppressed_triple() {
    // Arrange: one session suppressed on two models plus a second session,
    // three triples in total.
    let (_dir, _path, db) = temp_db();
    k_suppressed_row(&db, "k1", 1_000, "sess-a", "haiku");
    k_suppressed_row(&db, "k2", 1_100, "sess-a", "haiku");
    k_suppressed_row(&db, "k3", 1_200, "sess-a", "sonnet");
    k_suppressed_row(&db, "k4", 1_300, "sess-b", "haiku");
    let report = report_all(&db, &cost_config(), None, true);

    // Act
    let out = render_report(&report);
    let block = suppressed_block(&out);

    // Assert: the header counts triples, and each triple carries its key, its
    // per-triple request count, and both suppression instants.
    assert!(
        block.contains("k-suppressed sessions: 3 (session, provider_kind, model) triples"),
        "finder header: {block}"
    );
    assert_eq!(
        block.matches("session=").count(),
        3,
        "one line per triple: {block}"
    );
    assert!(block.contains("kind=anthropic-api"), "triple key: {block}");
    assert!(block.contains("model=haiku"), "triple key: {block}");
    assert!(block.contains("model=sonnet"), "triple key: {block}");
    assert!(block.contains("reqs=2"), "per-triple count: {block}");
    assert!(block.contains("first="), "first suppression: {block}");
    assert!(block.contains("last="), "last suppression: {block}");
}

#[test]
fn render_detail_finder_orders_triples_newest_suppression_first() {
    // Arrange: three triples with strictly ordered last-suppression instants.
    let (_dir, _path, db) = temp_db();
    k_suppressed_row(&db, "old", 1_000, "sess-old", "m-old");
    k_suppressed_row(&db, "mid", 2_000, "sess-mid", "m-mid");
    k_suppressed_row(&db, "new", 3_000, "sess-new", "m-new");
    let report = report_all(&db, &cost_config(), None, true);

    // Act
    let out = render_report(&report);
    let block = suppressed_block(&out);

    // Assert: the model column is a proxy for triple identity here, so the
    // order of its appearances IS the row order.
    let order: Vec<&str> = ["m-new", "m-mid", "m-old"]
        .into_iter()
        .filter(|m| block.contains(&format!("model={m}")))
        .collect();
    assert_eq!(order.len(), 3, "all three triples rendered: {block}");
    let positions: Vec<usize> = order
        .iter()
        .map(|m| block.find(&format!("model={m}")).expect("rendered"))
        .collect();
    assert!(
        positions[0] < positions[1] && positions[1] < positions[2],
        "newest suppression first: {block}"
    );
}

#[test]
fn render_detail_finder_counts_a_both_marker_suppression_once() {
    // Arrange: suppression withholds BOTH markers under one shared verdict, so
    // the row carries the token twice and is ONE suppressed request.
    let (_dir, _path, db) = temp_db();
    decision_triple_row(
        &db,
        "both",
        1_000,
        "sess-a",
        "anthropic-api",
        "haiku",
        Some(K_SUPPRESSION_TOKEN),
        Some(K_SUPPRESSION_TOKEN),
    );
    let report = report_all(&db, &cost_config(), None, true);

    // Act
    let out = render_report(&report);
    let block = suppressed_block(&out);

    // Assert
    assert!(
        block.contains("reqs=1"),
        "front + terminal suppression is one withheld request: {block}"
    );
}

#[test]
fn render_detail_finder_caps_the_list_and_says_so() {
    // Arrange: one more triple than the cap, each with a distinct recency.
    let (_dir, _path, db) = temp_db();
    for i in 0..=SUPPRESSED_SESSION_CAP {
        let idx = i64::try_from(i).expect("small index");
        k_suppressed_row(
            &db,
            &format!("k{i:03}"),
            1_000 + idx,
            &format!("sess-{i:03}"),
            "haiku",
        );
    }
    let report = report_all(&db, &cost_config(), None, true);

    // Act
    let out = render_report(&report);
    let block = suppressed_block(&out);

    // Assert: exactly the cap is listed, and the operator is told the list is
    // a newest-first prefix rather than the whole set.
    assert_eq!(
        block.matches("session=").count(),
        SUPPRESSED_SESSION_CAP,
        "capped at {SUPPRESSED_SESSION_CAP}: {block}"
    );
    assert!(
        block.contains(&format!(
            "more triples than the {SUPPRESSED_SESSION_CAP}-row cap"
        )),
        "truncation indicator: {block}"
    );
}

#[test]
fn render_detail_finder_omits_the_truncation_line_when_nothing_was_dropped() {
    // Arrange
    let (_dir, _path, db) = temp_db();
    k_suppressed_row(&db, "k1", 1_000, "sess-a", "haiku");
    let report = report_all(&db, &cost_config(), None, true);

    // Act + Assert
    let block = suppressed_block(&render_report(&report));
    assert!(
        !block.contains("more triples than"),
        "a complete list adds no truncation line: {block}"
    );
}

#[test]
fn render_detail_finder_carries_the_drop_counter_validity_caveat() {
    // Arrange: a window whose counts an operator might otherwise price. The
    // usage channel can drop records, so the block must refuse a rate or cost
    // reading and name the three counters that prove the window lossless.
    let (_dir, _path, db) = temp_db();
    k_suppressed_row(&db, "k1", 1_000, "sess-a", "haiku");
    k_suppressed_row(&db, "k2", 1_100, "sess-a", "haiku");
    let report = report_all(&db, &cost_config(), None, true);

    // Act
    let out = render_report(&report);
    let block = suppressed_block(&out);

    // Assert
    assert!(
        block.contains("sessions seen, not a rate"),
        "caveat: {block}"
    );
    for counter in ["dropped_full", "dropped_disabled", "write_errors"] {
        assert!(
            block.contains(counter),
            "validity caveat must name {counter}: {block}"
        );
    }
    assert!(!block.contains('%'), "no ratio in the block: {block}");
    assert!(!block.contains('$'), "no cost claim in the block: {block}");
}

#[test]
fn render_detail_finder_never_prints_the_raw_session_key() {
    // Arrange: a session key that would be a durable personal identifier if it
    // reached an operator-facing surface.
    let (_dir, _path, db) = temp_db();
    let raw = "user@example.test";
    k_suppressed_row(&db, "k1", 1_000, raw, "haiku");
    let report = report_all(&db, &cost_config(), None, true);

    // Act
    let out = render_report(&report);

    // Assert: an opaque fixed-width reference stands in for the key.
    assert!(
        !out.contains(raw),
        "the raw session key must never render: {out}"
    );
    let block = suppressed_block(&out);
    let reference = block
        .split("session=")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("a session reference is rendered");
    assert_eq!(reference.len(), 16, "fixed-width hex reference: {block}");
    assert!(
        reference.chars().all(|c| c.is_ascii_hexdigit()),
        "opaque hex reference: {block}"
    );
}

#[test]
fn render_detail_omits_the_finder_when_nothing_was_suppressed() {
    // Arrange: an emitted decision and a differently-skipped one.
    let (_dir, _path, db) = temp_db();
    decision_triple_row(
        &db,
        "emitted",
        1_000,
        "sess-a",
        "anthropic-api",
        "haiku",
        Some("auto_emitted"),
        Some("auto_emitted"),
    );
    decision_triple_row(
        &db,
        "capped",
        1_100,
        "sess-a",
        "anthropic-api",
        "haiku",
        Some("auto_skipped:breakpoint_cap"),
        None,
    );
    let report = report_all(&db, &cost_config(), None, true);

    // Act + Assert: an ordinary window stays uncluttered.
    let out = render_report(&report);
    assert!(
        !out.contains("k-suppressed sessions:"),
        "no suppression -> no finder block: {out}"
    );
}

#[test]
fn render_non_detail_omits_the_finder() {
    // Arrange: suppression exists, but the default table must not surface it.
    let (_dir, _path, db) = temp_db();
    k_suppressed_row(&db, "k1", 1_000, "sess-a", "haiku");
    let report = report_all(&db, &cost_config(), None, false);

    // Act + Assert
    let out = render_report(&report);
    assert!(
        !out.contains("k-suppressed sessions:"),
        "the default (non-detail) table must omit the finder: {out}"
    );
}
