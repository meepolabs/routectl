// Render-layer tests for the `--detail` lossless-minifier (reduction) block.
// Split from `usage_render_tests.rs` to keep each file under the size ceiling;
// `include!`d into the same `tests` module so all helpers there (`temp_db`,
// `cost_config`, `report_all`, `paid_row`, ...) are in scope. All imports come
// from the enclosing `usage_tests.rs`; do not add `use` lines here.

/// Insert a row carrying the v15 reduction outcome columns, so the `--detail`
/// reduction block is testable. `decision` `None` writes a NULL outcome
/// (pre-column history / no dispatched target).
#[allow(clippy::too_many_arguments)]
fn reduction_row(
    db: &UsageDb,
    request_id: &str,
    decision: Option<&str>,
    compressed: i64,
    skipped: i64,
    rejected: i64,
    bytes_saved: i64,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             reduction_decision, reduction_strings_compressed, \
             reduction_strings_skipped, reduction_strings_rejected, \
             reduction_bytes_saved) \
             VALUES (1000, 1000, ?1, 'openai', 'req-model', 'al', 'm', 'paid', \
             'up-paid', 0, 'ok', 5, 0, 0, 1, 0, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                request_id,
                decision,
                compressed,
                skipped,
                rejected,
                bytes_saved
            ],
        )
        .expect("insert reduction row");
}

#[test]
fn render_detail_shows_reduction_block_with_histogram_and_derived_tokens() {
    // Arrange: two applied rows and one skip, 40_000 bytes saved in total.
    let (_dir, _path, db) = temp_db();
    reduction_row(&db, "rd1", Some("applied"), 4, 2, 0, 24_000);
    reduction_row(&db, "rd2", Some("applied"), 1, 1, 0, 16_000);
    reduction_row(&db, "rd3", Some("skipped:nothing-to-strip"), 0, 9, 0, 0);
    let report = report_all(&db, &cost_config(), None, true);

    // Act
    let out = render_report(&report);

    // Assert: decided count, per-token histogram, humanized bytes (40_000 ->
    // "40K"), and the read-time-derived token estimate (40_000 / 4 = 10_000 ->
    // "10K").
    assert!(
        out.contains("reduction: 3 reqs decided"),
        "detail output must surface the reduction block: {out}"
    );
    assert!(out.contains("applied=2"), "per-token histogram: {out}");
    assert!(
        out.contains("skipped:nothing-to-strip=1"),
        "per-token histogram: {out}"
    );
    assert!(out.contains("40K bytes"), "summed bytes saved: {out}");
    assert!(out.contains("~10K est tokens"), "derived tokens: {out}");
    assert!(
        out.contains("strings compressed=5"),
        "summed counters: {out}"
    );
    assert!(out.contains("skipped=12"), "summed counters: {out}");
}

#[test]
fn render_detail_reduction_block_carries_the_drop_counter_validity_caveat() {
    // Arrange
    let (_dir, _path, db) = temp_db();
    reduction_row(&db, "rd1", Some("applied"), 1, 0, 0, 800);
    let report = report_all(&db, &cost_config(), None, true);

    // Act
    let out = render_report(&report);

    // Assert: the block states it is counts-not-a-rate and names all three
    // usage-channel drop counters an operator must check flat.
    assert!(out.contains("counts observed, not a rate"), "caveat: {out}");
    for counter in ["dropped_full", "dropped_disabled", "write_errors"] {
        assert!(
            out.contains(counter),
            "validity caveat must name {counter}: {out}"
        );
    }
}

#[test]
fn render_detail_reduction_block_omits_a_percentage_or_cost_figure() {
    // Arrange: a window whose counters would invite a share or dollar claim.
    let (_dir, _path, db) = temp_db();
    reduction_row(&db, "rd1", Some("applied"), 10, 5, 0, 4_000);
    reduction_row(&db, "rd2", Some("skipped:no-tail"), 0, 0, 0, 0);
    let report = report_all(&db, &cost_config(), None, true);

    // Act
    let out = render_report(&report);
    let block = out
        .lines()
        .skip_while(|l| !l.starts_with("reduction:"))
        .take_while(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");

    // Assert: no ratio and no cost claim over a channel that can drop records.
    assert!(!block.contains('%'), "no ratio in the block: {block}");
    assert!(!block.contains('$'), "no cost claim in the block: {block}");
}

#[test]
fn render_detail_reduction_block_flags_a_nonzero_rejected_count_as_a_defect() {
    // Arrange: `rejected` is structurally unreachable with the current
    // minifier, so a nonzero count is a guard-caught rewrite -- a defect
    // signal, not headroom.
    let (_dir, _path, db) = temp_db();
    reduction_row(&db, "rd1", Some("applied"), 2, 0, 3, 100);
    let report = report_all(&db, &cost_config(), None, true);

    // Act
    let out = render_report(&report);

    // Assert
    assert!(out.contains("rejected=3"), "rejected count surfaced: {out}");
    assert!(
        out.contains("not traffic headroom"),
        "must read as a defect signal, not headroom: {out}"
    );
}

#[test]
fn render_detail_reduction_block_omits_the_rejected_line_when_zero() {
    // Arrange: the ordinary case -- nothing rejected.
    let (_dir, _path, db) = temp_db();
    reduction_row(&db, "rd1", Some("applied"), 2, 1, 0, 100);
    let report = report_all(&db, &cost_config(), None, true);

    // Act + Assert
    let out = render_report(&report);
    assert!(
        !out.contains("rejected="),
        "a zero rejected count adds no line: {out}"
    );
}

#[test]
fn render_non_detail_omits_reduction_block() {
    // Arrange: outcomes exist, but the default table must not surface them.
    let (_dir, _path, db) = temp_db();
    reduction_row(&db, "rd1", Some("applied"), 1, 0, 0, 500);
    let report = report_all(&db, &cost_config(), None, false);

    // Act + Assert
    let out = render_report(&report);
    assert!(
        !out.contains("reduction:"),
        "the default (non-detail) table must omit the reduction block: {out}"
    );
}

#[test]
fn render_detail_omits_reduction_block_when_no_row_carries_an_outcome() {
    // Arrange: a NULL-outcome row (pre-column history) plus a plain row.
    let (_dir, _path, db) = temp_db();
    reduction_row(&db, "old", None, 0, 0, 0, 0);
    paid_row(&db, "plain", Some(10), Some(20));
    let report = report_all(&db, &cost_config(), None, true);

    // Act + Assert: a window with no decided request stays uncluttered.
    let out = render_report(&report);
    assert!(
        !out.contains("reduction:"),
        "no decided requests -> no reduction block: {out}"
    );
}
