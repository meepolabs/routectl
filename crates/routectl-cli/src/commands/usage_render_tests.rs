// Render-layer, quota-line, `--since` title, and `--by model` tests for the
// `routectl usage` read surface. Split from `usage_tests.rs` to keep each
// file under the size ceiling; `include!`d into the same `tests` module so
// all helpers there (`temp_db`, `insert_row`, `cost_config`, `find`,
// `fixed_now`, ...) are in scope without duplication. All imports come from
// the enclosing `usage_tests.rs` (its `use super::*`); do not add `use` lines
// here.

// --- render layer ---

/// Compact wrapper over `insert_row` for the common priced-row shape
/// (model "m", provider "paid", upstream "up-paid", alias "al", outcome
/// "ok"). Keeps the render-layer tests dense and readable.
fn paid_row(db: &UsageDb, request_id: &str, input: Option<i64>, output: Option<i64>) {
    insert_row(
        db, request_id, 1000, "m", "paid", "up-paid", "al", "ok", input, output, 5,
    );
}

fn report_all(db: &UsageDb, config: &Config, by: Option<GroupDim>, detail: bool) -> WindowReport {
    let bounds = window_bounds(WindowFlag::All, fixed_now());
    build_window_report(db, config, "t".into(), bounds, by, detail).unwrap()
}

/// A `paid` row with an explicit model and outcome, for grouping/error tests.
fn paid_model_row(db: &UsageDb, id: &str, model: &str, outcome: &str, input: i64, output: i64) {
    insert_row(
        db,
        id,
        1000,
        model,
        "paid",
        "up-paid",
        "al",
        outcome,
        Some(input),
        Some(output),
        5,
    );
}

#[test]
fn render_non_detail_header_has_expected_columns() {
    // Arrange
    let (_dir, _path, db) = temp_db();
    paid_row(&db, "h1", Some(10), Some(20));
    let report = report_all(&db, &cost_config(), None, false);

    // Act
    let out = render_report(&report);
    let header = out.lines().nth(1).expect("header line present");

    // Assert: default grouping is by model; standard columns follow. Dropped
    // and renamed columns must be absent.
    assert!(header.contains("model"));
    assert!(header.contains("reqs"));
    assert!(header.contains("err"));
    assert!(header.contains("input"));
    assert!(header.contains("output"));
    assert!(header.contains("cache_read"));
    assert!(header.contains("hit%"));
    // The normal view no longer carries reasoning, the context-size peak, or
    // cost -- ctx_peak and cost moved under --detail; reasoning is unrendered.
    for dropped in [
        "reasoning",
        "ctx_peak",
        "cost",
        "scope",
        "p95_ms",
        "max_ms",
        "wall_ms",
    ] {
        assert!(
            !header.contains(dropped),
            "header should not contain {dropped}"
        );
    }
}

/// Insert a row carrying a steady-state would-trim candidate, so the
/// --detail would-trim opportunity line is testable.
fn would_trim_row(db: &UsageDb, request_id: &str, would_trim_tokens: i64) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             would_trim_tokens) \
             VALUES (1000, 1000, ?1, 'openai', 'req-model', 'al', 'm', 'paid', \
             'up-paid', 0, 'ok', 5, 0, 0, 1, 0, ?2)",
            rusqlite::params![request_id, would_trim_tokens],
        )
        .expect("insert would-trim row");
}

#[test]
fn render_detail_shows_would_trim_opportunity_line() {
    // Arrange: two rows with would-cut candidates.
    let (_dir, _path, db) = temp_db();
    would_trim_row(&db, "wt1", 40_000);
    would_trim_row(&db, "wt2", 20_000);
    let report = report_all(&db, &cost_config(), None, true);

    // Act
    let out = render_report(&report);

    // Assert: the compact advisory line names the candidate count, the summed
    // tokens (humanized 60_000 -> "60K"), and flags that it is not applied.
    assert!(
        out.contains("would-trim: 2 reqs with a would-cut candidate"),
        "detail output must surface the would-trim opportunity: {out}"
    );
    assert!(out.contains("60K"), "summed candidate tokens: {out}");
    assert!(out.contains("not applied"), "advisory framing: {out}");
}

#[test]
fn render_non_detail_omits_would_trim_line() {
    // Arrange: a would-cut candidate exists, but the default table must not
    // surface it (only --detail does).
    let (_dir, _path, db) = temp_db();
    would_trim_row(&db, "wt1", 40_000);
    let report = report_all(&db, &cost_config(), None, false);

    // Act + Assert
    let out = render_report(&report);
    assert!(
        !out.contains("would-trim"),
        "the default (non-detail) table must omit the would-trim line: {out}"
    );
}

#[test]
fn render_detail_omits_would_trim_line_when_no_candidates() {
    // Arrange: a plain row, no would-cut candidate.
    let (_dir, _path, db) = temp_db();
    paid_row(&db, "plain", Some(10), Some(20));
    let report = report_all(&db, &cost_config(), None, true);

    // Act + Assert: a window with no candidates stays uncluttered.
    let out = render_report(&report);
    assert!(
        !out.contains("would-trim"),
        "no candidates -> no would-trim line: {out}"
    );
}

#[test]
fn render_by_provider_uses_provider_key_header() {
    // Arrange
    let (_dir, _path, db) = temp_db();
    paid_row(&db, "h2", Some(10), Some(20));
    let report = report_all(&db, &cost_config(), Some(GroupDim::Provider), false);

    // Act
    let out = render_report(&report);
    let header = out.lines().nth(1).expect("header line present");

    // Assert
    assert!(header.contains("provider"));
    assert!(!header.contains("scope"));
}

#[test]
fn render_data_row_shows_rolled_up_values_and_priced_cost() {
    // Arrange: 1_000_000 in @ $3 + 1_000_000 out @ $15 = $18.00. Cost renders
    // only under --detail, so request the detail view.
    let (_dir, _path, db) = temp_db();
    paid_row(&db, "r1", Some(1_000_000), Some(1_000_000));
    let report = report_all(&db, &cost_config(), Some(GroupDim::Provider), true);

    // Act
    let out = render_report(&report);
    let data_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("paid"))
        .expect("data row present");

    // Assert: tokens are humanized (1_000_000 -> "1M") and cost is priced.
    assert!(data_line.contains("1M"));
    assert!(data_line.contains("$18.00"));
}

#[test]
fn render_detail_adds_extra_columns_non_detail_omits_them() {
    // Arrange: a streaming row so the detail columns have data.
    let (_dir, _path, db) = temp_db();
    insert_stream_row(&db, "d1", 1000, "m", 120, 620, Some(50), Some(5), Some(7));
    let config = cost_config();
    let detail = report_all(&db, &config, None, true);
    let plain = report_all(&db, &config, None, false);

    // Act
    let detail_out = render_report(&detail);
    let plain_out = render_report(&plain);
    let detail_header = detail_out.lines().nth(1).expect("header");
    let plain_header = plain_out.lines().nth(1).expect("header");

    // Assert: detail header carries the new derived columns; non-detail omits
    // them, and the dropped latency columns appear nowhere.
    for col in [
        "cost",
        "ctx_peak",
        "ctx_avg",
        "cache_wr_5m",
        "cache_wr_1h",
        "ttft_p50",
        "ttft_p95",
        "tok/s",
        "srv_tools",
    ] {
        assert!(detail_header.contains(col), "detail header missing {col}");
        assert!(
            !plain_header.contains(col),
            "non-detail header should not contain {col}"
        );
    }
    // reasoning is no longer rendered in EITHER view.
    assert!(!detail_header.contains("reasoning"));
    assert!(!plain_header.contains("reasoning"));
    for dropped in ["max_ms", "wall_ms", "p95_ms"] {
        assert!(
            !detail_header.contains(dropped),
            "detail header should not contain {dropped}"
        );
    }
    // ttft is rendered via human_ms (120ms), tok/s is present, and the
    // detail-only latency summary line appears.
    assert!(detail_out.contains("120ms"));
    assert!(detail_out.contains("latency: TTFT"));
    assert!(detail_out.contains("tok/s"));
}

#[test]
fn render_humanizes_large_cache_read_and_honors_not_reported() {
    // Arrange: the cache_read column across all three presence/value cases --
    // a large reported volume (4_637_884 -> "4.6M"), a NULL (not reported), and a
    // reported-but-zero (present, sum 0) that must render "0", not "-".
    let (_dir, _path, db) = temp_db();
    insert_stream_row(
        &db,
        "c1",
        1000,
        "big",
        100,
        600,
        Some(10),
        Some(0),
        Some(4_637_884),
    );
    insert_stream_row(&db, "c2", 1100, "nulls", 100, 600, Some(10), None, None);
    insert_stream_row(&db, "c3", 1200, "zero", 100, 600, Some(10), None, Some(0));
    let report = report_all(&db, &cost_config(), Some(GroupDim::Model), false);

    // Act
    let out = render_report(&report);
    let big_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("big"))
        .expect("big row present");
    let null_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("nulls"))
        .expect("nulls row present");
    let zero_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("zero"))
        .expect("zero row present");

    // Assert: the cache_read column humanizes the billed sum; a provider that
    // does not report cache reads (NULL) shows "-", never "0"; and a provider
    // that reports a cache read of 0 (present, sum 0) shows "0", never "-".
    assert!(big_line.contains("4.6M"));
    assert!(
        null_line.contains(" - "),
        "NULL cache_read should show -: {null_line:?}"
    );
    // Header order is key|reqs|err|input|output|cache_read|hit%, so the cache_read
    // cell is field index 5 -- pin it by position so a stray "0" elsewhere on the
    // row cannot satisfy the assertion.
    let zero_cells: Vec<&str> = zero_line.split_whitespace().collect();
    assert_eq!(
        zero_cells.get(5).copied(),
        Some("0"),
        "reported-0 cache_read should render '0', not '-': {zero_line:?}"
    );
}

#[test]
fn render_detail_latency_summary_uses_window_total_not_alias_named_total() {
    // Arrange: under `--by alias`, an alias literally named "total" must not be
    // mistaken for the injected window-total row. Two streaming rows under that
    // alias (ttfb 100, 300) plus one under another alias (ttfb 500). The window
    // total p95 (nearest-rank over all three: 100,300,500 -> rank 3 -> 500) must
    // differ from the "total" alias group's own p95 (100,300 -> rank 2 -> 300).
    let (_dir, _path, db) = temp_db();
    let insert_stream_alias = |id: &str, alias: &str, ttfb: i64| {
        db.conn()
            .execute(
                "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
                 requested_model, alias, model, provider, upstream, stream, outcome, \
                 latency_ms, ttfb_ms, tool_count, msg_count, attempt_count, \
                 fallback_count, output_tokens) \
                 VALUES (1000, 1000, ?1, 'openai', 'req-model', ?2, 'm', 'paid', \
                 'up-paid', 1, 'ok', ?3, ?4, 0, 0, 1, 0, 1)",
                rusqlite::params![id, alias, ttfb + 1000, ttfb],
            )
            .expect("insert stream alias row");
    };
    insert_stream_alias("ta", "total", 100);
    insert_stream_alias("tb", "total", 300);
    insert_stream_alias("oc", "other", 500);

    let report = report_all(&db, &cost_config(), Some(GroupDim::Alias), true);

    // Act
    let out = render_report(&report);
    let summary = out
        .lines()
        .find(|l| l.starts_with("latency: TTFT"))
        .expect("latency summary line present");

    // Assert: the summary's p95 reflects the window-wide value (500ms), not the
    // "total" alias group's own p95 (300ms) -- the injected window total is
    // selected by position, not by the label colliding with the alias name.
    assert!(
        summary.contains("p95 500ms"),
        "summary p95 should be the window total, not the alias named total: {summary:?}"
    );
}

/// Config priced with a non-zero cache_read rate, used by the cost
/// regression test. The `paid` provider is API-key (not subscription) and
/// `up-paid` carries a cache_read price so cache reads contribute dollars.
fn cache_read_priced_config(cache_read_per_mtok: f64) -> Config {
    let mut config = Config::default();
    config.providers.insert(
        "paid".to_string(),
        ProviderEntry::anthropic_api("env://PAID_KEY"),
    );
    config.registry.insert(
        "up-paid".to_string(),
        RegistryEntry {
            pricing: Some(PricingConfig {
                cache_read_per_mtok: Some(cache_read_per_mtok),
                ..Default::default()
            }),
            provider: None,
        },
    );
    config
}

#[test]
fn cost_prices_summed_cache_read_not_peak() {
    // Arrange: one model group with a CLIMBING cache_read snapshot
    // (88000, 89000, 91000). Cache reads are billed PER TURN, so the cost
    // basis is the SUM (268000), not the peak (91000). Price only cache_read
    // at a known rate so the dollar figure is attributable to it alone.
    let (_dir, _path, db) = temp_db();
    insert_stream_row(&db, "cr1", 1000, "m", 100, 600, Some(1), None, Some(88_000));
    insert_stream_row(&db, "cr2", 1100, "m", 100, 600, Some(1), None, Some(89_000));
    insert_stream_row(&db, "cr3", 1200, "m", 100, 600, Some(1), None, Some(91_000));
    let rate = 6.0; // USD per million tokens
    let config = cache_read_priced_config(rate);
    let report = report_all(&db, &config, Some(GroupDim::Model), false);

    // Act
    let total = find(&report, "total");
    let cost = total.priced_total_usd.expect("priced cost present");

    // Assert: cost is the SUM-based figure (268000 * rate / 1e6), strictly
    // greater than the peak-based figure (91000 * rate / 1e6). This MUST fail
    // if cost is ever reverted to pricing the peak.
    let sum_cost = 268_000.0 * rate / 1_000_000.0;
    let peak_cost = 91_000.0 * rate / 1_000_000.0;
    assert!(
        (cost - sum_cost).abs() < 1e-9,
        "expected sum-based cost {sum_cost}, got {cost}"
    );
    assert!(
        cost > peak_cost,
        "sum-based cost {cost} must exceed peak-based cost {peak_cost}"
    );
}

#[test]
fn render_total_ctx_peak_is_max_not_sum_and_flows_stay_summed() {
    // Arrange: two streaming rows in the same model group with a climbing
    // cache_read snapshot (88000, 91000). The display total must report the
    // PEAK context (91000 -> "91K"), never the sum (179000 -> "179K"), while
    // input/output remain real summed flows.
    let (_dir, _path, db) = temp_db();
    insert_stream_row(
        &db,
        "ft1",
        1000,
        "m",
        100,
        600,
        Some(10),
        None,
        Some(88_000),
    );
    insert_stream_row(
        &db,
        "ft2",
        1100,
        "m",
        100,
        600,
        Some(20),
        None,
        Some(91_000),
    );
    let report = report_all(&db, &cost_config(), Some(GroupDim::Model), false);

    // Act
    let total = find(&report, "total");

    // Assert: ctx_peak is the MAX snapshot; the sum never appears as a field.
    assert_eq!(total.cache_read_peak, 91_000);
    assert_ne!(total.cache_read_peak, 179_000);
    // Output is a real summed flow (10 + 20).
    assert_eq!(total.output_tokens, 30);
}

#[test]
fn render_includes_total_row() {
    // Arrange
    let (_dir, _path, db) = temp_db();
    paid_model_row(&db, "t1", "m", "ok", 10, 20);
    let report = report_all(&db, &cost_config(), Some(GroupDim::Model), false);

    // Act
    let out = render_report(&report);

    // Assert: a total row is always present.
    assert!(out.lines().any(|l| l.trim_start().starts_with("total")));
}

#[test]
fn render_footer_shows_cache_hit_rate() {
    // Arrange: one cache-reporting stream row (cache_read=100, input=0) and a
    // fresh-input stream row (cache_read=0, input via reasoning unused). With the
    // token-weighted formula, a row reporting cache_read=100 and no fresh input /
    // cache-write has hit rate 100/100 = 100.0%.
    let (_dir, _path, db) = temp_db();
    insert_stream_row(&db, "r1", 1000, "m", 100, 600, Some(10), None, Some(100));
    paid_model_row(&db, "r2", "m", "upstream_error", 0, 0);
    let report = report_all(&db, &cost_config(), None, false);

    // Act
    let out = render_report(&report);

    // Assert: errors moved to a column; the footer is the cache-hit line only.
    assert!(out.contains("cache hit 100.0%"));
    assert!(!out.contains("errors:"));
}

#[test]
fn render_footer_na_rate_when_no_tokens() {
    // Arrange: NULL input, no cache => denominator 0 => rate None => "n/a".
    let (_dir, _path, db) = temp_db();
    paid_row(&db, "r3", None, None);
    let report = report_all(&db, &cost_config(), None, false);

    // Act
    let out = render_report(&report);

    // Assert
    assert!(out.contains("cache hit n/a"));
}

#[test]
fn render_footer_cache_hit_rate_is_token_weighted_over_reporting_rows() {
    // Arrange: two model groups that both report cache reads. The footer is the
    // token-weighted fraction of prompt tokens served from cache:
    //   num = sum(cache_read_billed) over reporting rows
    //   den = sum(input + cache_read_billed + cache_write_5m + cache_write_1h)
    // Recomputed from the old "per-group mean of peak/(peak+input)" figure, which
    // was wrong (it mixed a per-turn SNAPSHOT with summed input and dropped
    // cache-write). Group A: cache_read 900, input 100. Group B: cache_read 0,
    // input 100. num = 900 + 0 = 900; den = (100+900) + (100+0) = 1100;
    // 900/1100 = 0.81818 -> "81.8%".
    let (_dir, _path, db) = temp_db();
    // Group A "ga": a stream row carries the cache_read billed volume (900); a
    // paid row carries the fresh input (100). Both share (model, provider,
    // upstream, alias) so they roll into one aggregate group.
    insert_stream_row(&db, "ga_s", 1000, "ga", 100, 600, Some(5), None, Some(900));
    paid_model_row(&db, "ga_i", "ga", "ok", 100, 0);
    // Group B "gb": cache_read reported as 0 (present), input 100.
    insert_stream_row(&db, "gb_s", 1100, "gb", 100, 600, Some(5), None, Some(0));
    paid_model_row(&db, "gb_i", "gb", "ok", 100, 0);
    let report = report_all(&db, &cost_config(), Some(GroupDim::Model), false);

    // Act
    let out = render_report(&report);

    // Assert: token-weighted 900/1100 = 81.8%.
    assert!(
        out.contains("cache hit 81.8%"),
        "footer should be the token-weighted rate: {out:?}"
    );
}

#[test]
fn render_footer_excludes_non_cache_reporting_rows() {
    // Arrange: one cache-reporting group and one group that does NOT report cache
    // (NULL cache_read => cache_read_present == 0). The FOOTER's token-weighted
    // rate must exclude the non-reporting group's input from BOTH numerator and
    // denominator. Reporting group "rep": cache_read 600, input 0. Non-reporting
    // "norep": input 1000, NULL cache_read. Footer over reporting rows only =
    // 600/600 = 100.0%; if the non-reporting input leaked in it would be
    // 600/1600 = 37.5%.
    let (_dir, _path, db) = temp_db();
    insert_stream_row(
        &db,
        "rep_s",
        1000,
        "rep",
        100,
        600,
        Some(5),
        None,
        Some(600),
    );
    paid_model_row(&db, "norep_i", "norep", "ok", 1000, 0);
    let report = report_all(&db, &cost_config(), Some(GroupDim::Model), false);

    // Act
    let out = render_report(&report);
    let footer_line = out
        .lines()
        .find(|l| l.starts_with("cache hit"))
        .expect("footer line present");

    // Assert: the footer counts only the cache-reporting row -> 100.0%, never the
    // 37.5% it would show if the non-reporting input padded the denominator.
    assert!(
        footer_line.contains("cache hit 100.0%"),
        "non-reporting row must be excluded from footer num and den: {footer_line:?}"
    );
    assert!(
        !footer_line.contains("37.5%"),
        "non-reporting input must not pad the footer denominator: {footer_line:?}"
    );
}

#[test]
fn per_group_cache_hit_matches_footer_in_mixed_provider_group() {
    // Arrange: ONE display group (--by alias, shared alias "al") mixing a
    // cache-reporting aggregate (present>0, cache_read 900, input 100) with a
    // non-reporting aggregate (present==0, input 500). The per-group hit% must
    // count prompt tokens ONLY from the reporting aggregate -- 900/1000 = 90.0%
    // -- exactly the footer's rate. The pre-fix per-group rule summed input over
    // ALL rows in the group: 900/(600+900) = 60.0%, showing a diluted per-group
    // rate beside the footer's correct one in the same report.
    let (_dir, _path, db) = temp_db();
    // Reporting aggregate (model "rep"): a stream row carries the billed
    // cache-read volume (900); a paid row carries the fresh input (100). Both
    // share (provider "paid", upstream "up-paid", alias "al").
    insert_stream_row(
        &db,
        "rep_s",
        1000,
        "rep",
        100,
        600,
        Some(5),
        None,
        Some(900),
    );
    paid_model_row(&db, "rep_i", "rep", "ok", 100, 0);
    // Non-reporting aggregate (model "norep"): input 500, NULL cache_read.
    paid_model_row(&db, "norep_i", "norep", "ok", 500, 0);
    let report = report_all(&db, &cost_config(), Some(GroupDim::Alias), false);

    // Act
    let group = find(&report, "al").cache_hit_rate;
    let total = find(&report, "total").cache_hit_rate;
    let footer = report.cache_hit_rate;

    // Assert: per-group agrees with the footer (90.0%), never the diluted 60.0%.
    assert_eq!(footer, Some(0.9), "footer rate over reporting rows only");
    assert_eq!(
        group, footer,
        "per-group rate must equal the footer, not the input-diluted rate"
    );
    // By-construction pin: the total row mirrors the footer exactly.
    assert_eq!(total, footer, "total row must equal the footer");
}

#[test]
fn non_reporting_only_group_renders_no_data_marker() {
    // Arrange: a group whose only rows never report cache reads (present==0).
    // The shared denominator stays 0, so the rate is None and the hit% cell is
    // the "-" no-data marker -- never "0.0%".
    let (_dir, _path, db) = temp_db();
    paid_model_row(&db, "nr1", "norep", "ok", 500, 0);
    let report = report_all(&db, &cost_config(), Some(GroupDim::Model), false);

    // Act
    let group = find(&report, "norep");
    let out = render_report(&report);
    let row_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("norep"))
        .expect("norep row present");

    // Assert: no rate on the struct; the row renders the "-" marker, not 0.0%.
    assert_eq!(group.cache_hit_rate, None);
    assert!(
        row_line.contains(" - "),
        "hit% cell should be '-' for a non-reporting-only group: {row_line:?}"
    );
    assert!(
        !row_line.contains("0.0%"),
        "non-reporting-only group must not render 0.0%: {row_line:?}"
    );
}

#[test]
fn render_footer_not_equal_to_old_peak_based_value() {
    // Arrange: a regression guard for the footer-formula rewrite. The OLD footer
    // was a per-group mean of peak/(peak+input). For ONE group with two stream
    // rows (cache_read snapshots 100 then 900) plus input 100, the old value used
    // the PEAK (900): 900/(900+100) = 0.90 -> "90.0%". The NEW token-weighted
    // value sums the billed cache-read flow (100+900=1000): 1000/(100+1000) =
    // 0.90909 -> "90.9%". These differ, proving the footer no longer uses peak.
    let (_dir, _path, db) = temp_db();
    insert_stream_row(&db, "pk1", 1000, "pk", 100, 600, Some(5), None, Some(100));
    insert_stream_row(&db, "pk2", 1100, "pk", 100, 600, Some(5), None, Some(900));
    paid_model_row(&db, "pk_i", "pk", "ok", 100, 0);
    let report = report_all(&db, &cost_config(), Some(GroupDim::Model), false);

    // Act
    let out = render_report(&report);

    // Assert: the token-weighted billed figure (90.9%), NOT the old peak (90.0%).
    assert!(
        out.contains("cache hit 90.9%"),
        "footer should use summed billed cache-read, not peak: {out:?}"
    );
    assert!(
        !out.contains("90.0%"),
        "footer must not equal the old peak-based value: {out:?}"
    );
}

/// Insert a `client_disconnect` row with an explicit raw `model` value (or
/// `None` for a pre-dispatch hangup where dispatch never stamped a
/// provider). Mirrors the shape a `Drop`-without-`finalize` fallback writes.
fn client_disconnect_row(db: &UsageDb, request_id: &str, model: Option<&str>) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count) \
             VALUES (1000, 1000, ?1, 'openai', 'req-model', 'al', ?2, ?2, ?2, 0, \
             'client_disconnect', 5, 0, 0, 0, 0)",
            rusqlite::params![request_id, model],
        )
        .expect("insert client_disconnect row");
}

#[test]
fn render_footer_shows_client_disconnect_line() {
    // Arrange: one pre-dispatch disconnect (raw model NULL, never reached a
    // provider) and one post-dispatch disconnect (model stamped, so dispatch
    // had already resolved a provider before the client hung up).
    let (_dir, _path, db) = temp_db();
    client_disconnect_row(&db, "cd_pre", None);
    client_disconnect_row(&db, "cd_post", Some("m"));
    let report = report_all(&db, &cost_config(), None, false);

    // Act
    let out = render_report(&report);

    // Assert: both disconnects counted in the total; only the model-NULL one
    // counts toward the pre-dispatch subset.
    assert!(
        out.contains("client disconnects: 2 (1 pre-dispatch)"),
        "footer must report the disconnect total and pre-dispatch subset: {out:?}"
    );
}

#[test]
fn render_footer_client_disconnect_excluded_from_error_column() {
    // Arrange: a client_disconnect row alongside an ok row in the same group.
    // The disconnect must not inflate the `err` column (see AggRow::errors).
    let (_dir, _path, db) = temp_db();
    paid_model_row(&db, "ok1", "m", "ok", 10, 20);
    client_disconnect_row(&db, "cd1", Some("m"));
    let report = report_all(&db, &cost_config(), Some(GroupDim::Model), false);

    // Act
    let out = render_report(&report);
    let row_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("m "))
        .expect("model row present");

    // Assert: header order is key|reqs|err|input|..., so err is field index 2.
    // 2 requests total (1 ok + 1 disconnect), 0 errors.
    let cells: Vec<&str> = row_line.split_whitespace().collect();
    assert_eq!(cells.get(1).copied(), Some("2"), "reqs cell: {row_line:?}");
    assert_eq!(cells.get(2).copied(), Some("0"), "err cell: {row_line:?}");
}

#[test]
fn render_hit_pct_column_is_token_weighted_over_billed_not_peak() {
    // Arrange: one model group with two stream rows carrying a CLIMBING cache_read
    // snapshot (100 then 900) plus a paid row with fresh input 100. The hit%
    // column is token-weighted over the BILLED (summed) cache-read volume:
    //   billed = 100 + 900 = 1000; input = 100; cache_write = 0
    //   hit% = 1000 / (100 + 1000) = 0.90909 -> "90.9%"
    // If it ever used the PEAK (900) the cell would read "90.0%"
    // (900 / (100 + 900)). The group's own per-row cell must show the billed
    // figure, proving SUM not peak.
    let (_dir, _path, db) = temp_db();
    insert_stream_row(&db, "hp1", 1000, "hp", 100, 600, Some(5), None, Some(100));
    insert_stream_row(&db, "hp2", 1100, "hp", 100, 600, Some(5), None, Some(900));
    paid_model_row(&db, "hp_i", "hp", "ok", 100, 0);
    let report = report_all(&db, &cost_config(), Some(GroupDim::Model), false);

    // Act
    let out = render_report(&report);
    let row_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("hp"))
        .expect("hp row present");

    // Assert: the cell is the token-weighted billed figure (90.9%), not peak.
    assert!(
        row_line.contains("90.9%"),
        "hit% cell should use summed billed cache-read: {row_line:?}"
    );
    assert!(
        !row_line.contains("90.0%"),
        "hit% cell must not use the peak figure: {row_line:?}"
    );
}

#[test]
fn render_hit_pct_column_dash_when_provider_does_not_report_cache() {
    // Arrange: a paid row with input but NULL cache_read => cache_read_present
    // == 0. The hit% cell must render "-" (not "0.0%"): the provider does not
    // report cache reads, so the rate is "not reported", not "zero hits".
    let (_dir, _path, db) = temp_db();
    paid_model_row(&db, "nc1", "nocache", "ok", 500, 200);
    let report = report_all(&db, &cost_config(), Some(GroupDim::Model), false);

    // Act
    let out = render_report(&report);
    let row_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("nocache"))
        .expect("nocache row present");

    // Assert: dash, never a 0.0% rate, for a non-cache-reporting provider.
    assert!(
        row_line.contains(" - "),
        "hit% cell should be '-' when cache is not reported: {row_line:?}"
    );
    assert!(
        !row_line.contains("0.0%"),
        "hit% must not render 0.0% for a non-reporting provider: {row_line:?}"
    );
}

#[test]
fn render_table_left_aligns_key_and_right_aligns_numeric() {
    // Arrange: a short label ("paid") under a wider header ("provider") forces
    // padding so alignment direction is observable.
    let (_dir, _path, db) = temp_db();
    paid_row(&db, "a1", Some(7), Some(9));
    let report = report_all(&db, &cost_config(), Some(GroupDim::Provider), false);

    // Act
    let out = render_report(&report);
    let data_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("paid"))
        .expect("data row");

    // Assert: column 0 is left-aligned -- the short label sits at the line start
    // and is right-padded to the "provider" width (so trailing spaces follow).
    assert!(data_line.starts_with("paid"));
    assert!(
        data_line.starts_with("paid    "),
        "column 0 should be left-aligned and padded: {data_line:?}"
    );
    // Numeric column is right-aligned: the reqs value (1) renders flush against
    // the header width "reqs" (4) -- i.e. preceded by padding, not trailing.
    assert!(
        data_line.contains("   1 "),
        "reqs cell should be right-justified: {data_line:?}"
    );
}

#[test]
fn render_total_row_hit_pct_mirrors_footer_in_mixed_window() {
    // Arrange: a mixed-provider window with one cache-reporting group and one
    // that does NOT report cache. The TOTAL row's hit% must mirror the footer
    // (token-weighted over cache-reporting rows only), NOT a value diluted by
    // the non-reporting group's input.
    //   Reporting "rep": cache_read billed 900, input 100.
    //   Non-reporting "norep": input 1000, NULL cache_read (present == 0).
    // Footer / total = 900 / (100 + 900) = 90.0%.
    // The OLD total recomputed from its OWN summed fields, which fold in the
    // non-reporting input: 900 / (100 + 900 + 1000) = 900/2000 = 45.0%. The
    // total must read 90.0%, never 45.0%.
    let (_dir, _path, db) = temp_db();
    insert_stream_row(
        &db,
        "rep_s",
        1000,
        "rep",
        100,
        600,
        Some(5),
        None,
        Some(900),
    );
    paid_model_row(&db, "rep_i", "rep", "ok", 100, 0);
    paid_model_row(&db, "norep_i", "norep", "ok", 1000, 0);
    let report = report_all(&db, &cost_config(), Some(GroupDim::Model), false);

    // Act
    let out = render_report(&report);
    let total_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("total"))
        .expect("total row present");
    let footer_line = out
        .lines()
        .find(|l| l.starts_with("cache hit"))
        .expect("footer line present");

    // Assert: the total row's hit% equals the footer's reporting-only rate, and
    // is not the diluted all-input value.
    assert!(
        total_line.contains("90.0%"),
        "total hit% should mirror the footer rate: {total_line:?}"
    );
    assert!(
        !total_line.contains("45.0%"),
        "total hit% must not be diluted by non-reporting input: {total_line:?}"
    );
    assert!(
        footer_line.contains("cache hit 90.0%"),
        "footer should be the reporting-only token-weighted rate: {footer_line:?}"
    );
}

// --- quota line ---

#[allow(clippy::too_many_arguments)]
fn insert_quota_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    quota_status: &str,
    quota_utilization: f64,
    quota_overage_status: &str,
    quota_reset_s: i64,
) {
    insert_seat_quota_row(
        db,
        request_id,
        ts_start,
        None,
        Some(quota_status),
        Some(quota_utilization),
        Some(quota_overage_status),
        Some(quota_reset_s),
    );
}

/// Insert a quota-bearing row with an explicit `seat` and individually
/// nullable quota columns, so the per-seat render loop and a vendor that
/// reports utilization without a status token are both exercisable.
#[allow(clippy::too_many_arguments)]
fn insert_seat_quota_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    seat: Option<&str>,
    quota_status: Option<&str>,
    quota_utilization: Option<f64>,
    quota_overage_status: Option<&str>,
    quota_reset_s: Option<i64>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             input_tokens, output_tokens, seat, quota_status, quota_utilization, \
             quota_overage_status, quota_reset) \
             VALUES (?1, ?1, ?2, 'openai', 'req-model', 'al', 'm', 'sub', 'up-sub', 0, 'ok', \
             10, 0, 0, 1, 0, 100, 50, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                ts_start,
                request_id,
                seat,
                quota_status,
                quota_utilization,
                quota_overage_status,
                quota_reset_s,
            ],
        )
        .expect("insert quota row");
}

#[test]
fn build_report_surfaces_latest_quota_snapshot_per_seat() {
    // Arrange
    let (_dir, _path, db) = temp_db();
    insert_quota_row(&db, "q1", 1000, "allowed", 0.22, "none", 1_800_000_000);

    // Act
    let report = report_all(&db, &cost_config(), None, false);

    // Assert
    assert_eq!(report.quota.len(), 1);
    let quota = &report.quota[0];
    assert_eq!(quota.seat, None, "the seeded row carries no seat");
    assert_eq!(quota.status.as_deref(), Some("allowed"));
    assert_eq!(quota.utilization, Some(0.22));
    assert_eq!(quota.overage_status.as_deref(), Some("none"));
    assert_eq!(quota.reset, Some(1_800_000_000));
}

#[test]
fn render_report_includes_quota_line() {
    // Arrange
    let (_dir, _path, db) = temp_db();
    insert_quota_row(&db, "q2", 1000, "allowed", 0.22, "none", 1_800_000_000);
    let report = report_all(&db, &cost_config(), None, false);

    // Act
    let out = render_report(&report);
    let quota_line = out
        .lines()
        .find(|l| l.starts_with("quota["))
        .expect("quota line present");

    // Assert: a seatless row labels as "-"; 0.22 renders as 22%.
    assert!(quota_line.starts_with("quota[-]:"), "{quota_line}");
    assert!(quota_line.contains("status=allowed"));
    assert!(quota_line.contains("utilization=22%"));
    assert!(quota_line.contains("overage=none"));
    assert!(quota_line.contains("reset="));
}

#[test]
fn render_report_includes_one_quota_line_per_seat() {
    // Arrange: two seats, the second reporting utilization with no status
    // token or overage state (the codex shape), plus a superseded older row
    // for the first seat.
    let (_dir, _path, db) = temp_db();
    insert_seat_quota_row(
        &db,
        "a-old",
        1000,
        Some("anthropic#a"),
        Some("allowed"),
        Some(0.10),
        Some("none"),
        Some(1_800_000_000),
    );
    insert_seat_quota_row(
        &db,
        "a-new",
        2000,
        Some("anthropic#a"),
        Some("throttled"),
        Some(0.90),
        Some("none"),
        Some(1_800_000_000),
    );
    insert_seat_quota_row(
        &db,
        "c-1",
        1500,
        Some("codex"),
        None,
        Some(0.16),
        None,
        Some(1_786_210_114),
    );
    let report = report_all(&db, &cost_config(), None, false);

    // Act
    let out = render_report(&report);
    let lines: Vec<&str> = out.lines().filter(|l| l.starts_with("quota[")).collect();

    // Assert: one line per seat, each carrying that seat's newest snapshot.
    assert_eq!(lines.len(), 2, "{out}");
    let anthropic = lines
        .iter()
        .find(|l| l.starts_with("quota[anthropic#a]:"))
        .expect("anthropic seat line");
    assert!(anthropic.contains("status=throttled"), "{anthropic}");
    assert!(anthropic.contains("utilization=90%"), "{anthropic}");
    let codex = lines
        .iter()
        .find(|l| l.starts_with("quota[codex]:"))
        .expect("codex seat line");
    assert!(codex.contains("status=unknown"), "{codex}");
    assert!(codex.contains("utilization=16%"), "{codex}");
    assert!(codex.contains("overage=-"), "{codex}");
}

#[test]
fn render_report_omits_the_quota_line_when_nothing_reported() {
    // Arrange: a window with request rows but no quota-bearing row.
    let (_dir, _path, db) = temp_db();
    let report = report_all(&db, &cost_config(), None, false);

    // Act
    let out = render_report(&report);

    // Assert
    assert!(report.quota.is_empty());
    assert!(!out.contains("quota["), "{out}");
}

#[test]
fn format_reset_renders_local_timestamp_shape() {
    // Arrange: a fixed epoch-seconds value. The exact wall-clock depends on the
    // host TZ, so assert the canonical shape rather than a hardcoded string.
    let epoch_s = 1_800_000_000i64;

    // Act
    let rendered = format_reset(epoch_s);

    // Assert
    assert!(!rendered.is_empty());
    let bytes = rendered.as_bytes();
    // Shape: YYYY-MM-DD HH:MM (positions of digits and separators).
    assert_eq!(rendered.len(), 16, "unexpected reset format: {rendered:?}");
    assert!(bytes[4] == b'-' && bytes[7] == b'-');
    assert!(bytes[10] == b' ');
    assert!(bytes[13] == b':');
    assert!(rendered[0..4].chars().all(|c| c.is_ascii_digit()));
    assert!(rendered[5..7].chars().all(|c| c.is_ascii_digit()));
    assert!(rendered[8..10].chars().all(|c| c.is_ascii_digit()));
    assert!(rendered[11..13].chars().all(|c| c.is_ascii_digit()));
    assert!(rendered[14..16].chars().all(|c| c.is_ascii_digit()));
}

// --- --since title formatting ---

#[test]
fn since_with_until_builds_single_range_titled_block() {
    // Arrange
    let (_dir, _path, db) = temp_db();
    let config = cost_config();
    let args = UsageArgs {
        window: WindowFlag::None,
        since: Some("2026-06-01".to_string()),
        until: Some("2026-06-05".to_string()),
        by: None,
        detail: false,
        db: None,
        k_calibration: false,
    };

    // Act
    let blocks = build_blocks(&db, &config, &args, fixed_now()).unwrap();

    // Assert
    assert_eq!(blocks.len(), 1);
    assert!(blocks[0].contains("== 2026-06-01 .. 2026-06-05 =="));
}

#[test]
fn since_without_until_builds_single_open_ended_block() {
    // Arrange
    let (_dir, _path, db) = temp_db();
    let config = cost_config();
    let args = UsageArgs {
        window: WindowFlag::None,
        since: Some("2026-06-01".to_string()),
        until: None,
        by: None,
        detail: false,
        db: None,
        k_calibration: false,
    };

    // Act
    let blocks = build_blocks(&db, &config, &args, fixed_now()).unwrap();

    // Assert
    assert_eq!(blocks.len(), 1);
    assert!(blocks[0].contains("== since 2026-06-01 =="));
}

// --- --by model + (none) fallback ---

#[test]
fn by_model_groups_rows_sharing_a_model() {
    // Arrange: two rows with the same model "claude-x".
    let (_dir, _path, db) = temp_db();
    paid_model_row(&db, "m1", "claude-x", "ok", 10, 20);
    paid_model_row(&db, "m2", "claude-x", "ok", 5, 7);

    // Act
    let report = report_all(&db, &cost_config(), Some(GroupDim::Model), false);

    // Assert: one model group plus the always-appended total row.
    assert_eq!(report.rows.len(), 2);
    let row = find(&report, "claude-x");
    assert_eq!(row.requests, 2);
    assert_eq!(row.input_tokens, 15);
    assert_eq!(row.output_tokens, 27);
}

#[test]
fn by_model_null_model_attributes_to_requested_model() {
    // Arrange: a pre-dispatch abort has model=NULL but always carries a
    // requested_model. The aggregate must attribute it to requested_model so
    // it groups under the route the caller asked for, not a NULL bucket.
    let (_dir, _path, db) = temp_db();
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             input_tokens, output_tokens) \
             VALUES (1000, 1000, 'nm1', 'openai', 'gpt-asked', 'al', NULL, NULL, \
             NULL, 0, 'client_disconnect', 5, 0, 0, 0, 0, 8, 9)",
            [],
        )
        .expect("insert null-model row");

    // Act
    let report = report_all(&db, &cost_config(), Some(GroupDim::Model), false);

    // Assert: attributed to requested_model, never an "(unattributed)" bucket.
    assert!(
        report.rows.iter().all(|r| r.label != "(unattributed)"),
        "pre-dispatch abort must not land in the unattributed bucket"
    );
    let row = find(&report, "gpt-asked");
    assert_eq!(row.requests, 1);
    assert_eq!(row.input_tokens, 8);
}

/// Insert a successful row carrying explicit fresh input plus cache-write
/// buckets, so the displayed-input fold-in (input + cache_write_5m +
/// cache_write_1h) is testable. Model "m", provider "paid", upstream
/// "up-paid", alias "al".
fn insert_cache_write_row(
    db: &UsageDb,
    request_id: &str,
    input: i64,
    cache_write_5m: i64,
    cache_write_1h: i64,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             input_tokens, output_tokens, cache_write_5m, cache_write_1h) \
             VALUES (1000, 1000, ?1, 'openai', 'req-model', 'al', 'm', 'paid', \
             'up-paid', 0, 'ok', 5, 0, 0, 1, 0, ?2, 10, ?3, ?4)",
            rusqlite::params![request_id, input, cache_write_5m, cache_write_1h],
        )
        .expect("insert cache-write row");
}

#[test]
fn displayed_input_folds_in_cache_write_buckets() {
    // Arrange: a row with fresh input 100 and a 5m cache-write of 40 (1h zero).
    // The displayed input column means "prompt tokens not served from cache",
    // so it must render the sum 100 + 40 + 0 = 140, not the fresh-only 100.
    let (_dir, _path, db) = temp_db();
    insert_cache_write_row(&db, "cw1", 100, 40, 0);
    let report = report_all(&db, &cost_config(), Some(GroupDim::Model), false);

    // Act
    let out = render_report(&report);
    let row_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("m "))
        .expect("model row present");

    // Assert: header order is key|reqs|err|input|output|cache_read|hit%, so the
    // input cell is field index 3. Pin by position so a stray "140" elsewhere
    // cannot satisfy the assertion. 140 < COMPACT_COUNT_FLOOR renders plain.
    let cells: Vec<&str> = row_line.split_whitespace().collect();
    assert_eq!(
        cells.get(3).copied(),
        Some("140"),
        "displayed input must fold in cache-write (100 + 40): {row_line:?}"
    );
}

#[test]
fn displayed_input_unchanged_for_write_less_provider() {
    // Arrange: an OpenAI-style row reports zero cache-write and no cache_read.
    // With both write buckets zero the displayed input must equal the fresh
    // input (100) -- no regression for write-less providers.
    let (_dir, _path, db) = temp_db();
    insert_cache_write_row(&db, "ow1", 100, 0, 0);
    let report = report_all(&db, &cost_config(), Some(GroupDim::Model), false);

    // Act
    let out = render_report(&report);
    let row_line = out
        .lines()
        .find(|l| l.trim_start().starts_with("m "))
        .expect("model row present");

    // Assert: input cell (field index 3) equals the fresh input unchanged.
    let cells: Vec<&str> = row_line.split_whitespace().collect();
    assert_eq!(
        cells.get(3).copied(),
        Some("100"),
        "write-less provider input must equal fresh input: {row_line:?}"
    );
}

/// Insert a would-trim candidate row with the verdict-related advisory
/// columns. `break_even` = None means unpriced; `k_floor` = None means cold.
fn would_trim_verdict_row(
    db: &UsageDb,
    request_id: &str,
    would_trim_tokens: i64,
    break_even: Option<f64>,
    k_floor: Option<f64>,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             would_trim_tokens, would_trim_break_even_k, would_trim_k_floor) \
             VALUES (1000, 1000, ?1, 'openai', 'req-model', 'al', 'm', 'paid', \
             'up-paid', 0, 'ok', 5, 0, 0, 1, 0, ?2, ?3, ?4)",
            rusqlite::params![request_id, would_trim_tokens, break_even, k_floor],
        )
        .expect("insert would-trim verdict row");
}

#[test]
fn would_trim_verdict_counts_one_of_each_shape() {
    // Arrange: one row of each verdict shape.
    //   unpriced : break_even=None
    //   cold     : break_even=Some(2.0), k_floor=None
    //   met      : break_even=Some(2.0), k_floor=Some(3.0)  (3 >= 2)
    //   unmet    : break_even=Some(5.0), k_floor=Some(2.0)  (2 < 5)
    let (_dir, _path, db) = temp_db();
    would_trim_verdict_row(&db, "v-unpriced", 10_000, None, None);
    would_trim_verdict_row(&db, "v-cold", 10_000, Some(2.0), None);
    would_trim_verdict_row(&db, "v-met", 10_000, Some(2.0), Some(3.0));
    would_trim_verdict_row(&db, "v-unmet", 10_000, Some(5.0), Some(2.0));
    let report = report_all(&db, &cost_config(), None, true);

    // Act
    let wt = &report.would_trim;

    // Assert: counts partition the 4 candidate rows exactly.
    assert_eq!(wt.candidate_requests, 4);
    assert_eq!(wt.verdict_met, 1, "met");
    assert_eq!(wt.verdict_unmet, 1, "unmet");
    assert_eq!(wt.verdict_cold, 1, "cold");
    assert_eq!(wt.verdict_unpriced, 1, "unpriced");
}

#[test]
fn would_trim_render_verdict_line_present_when_candidates() {
    // Arrange
    let (_dir, _path, db) = temp_db();
    would_trim_verdict_row(&db, "vm1", 20_000, Some(2.0), Some(3.0));
    would_trim_verdict_row(&db, "vm2", 20_000, Some(5.0), Some(2.0));
    let report = report_all(&db, &cost_config(), None, true);

    // Act
    let out = render_report(&report);

    // Assert: first line survives, verdict line appended.
    assert!(
        out.contains("would-trim:"),
        "first line must be present: {out}"
    );
    assert!(
        out.contains("verdict: met=1 unmet=1 cold=0 unpriced=0"),
        "verdict line must match counts: {out}"
    );
}

#[test]
fn would_trim_render_no_verdict_line_when_zero_candidates() {
    // Arrange: no candidates at all.
    let (_dir, _path, db) = temp_db();
    paid_row(&db, "plain", Some(10), Some(20));
    let report = report_all(&db, &cost_config(), None, true);

    // Act + Assert: verdict line must not appear.
    let out = render_report(&report);
    assert!(
        !out.contains("verdict:"),
        "no verdict line when no candidates: {out}"
    );
}

/// Insert a row carrying the near-lossless attribution columns.
/// `recorder_version = None` yields a baseline (pre-recorder) row that must
/// never surface in the near-lossless attribution render block.
#[allow(clippy::too_many_arguments)]
fn near_lossless_attribution_row(
    db: &UsageDb,
    request_id: &str,
    recorder_version: Option<i64>,
    dedup_tokens: i64,
    supersession_tokens: i64,
    path_units: i64,
    path_extractable: i64,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, stream, outcome, \
             latency_ms, tool_count, msg_count, attempt_count, fallback_count, \
             would_trim_recorder_version, would_trim_dedup_tokens, \
             would_trim_supersession_tokens, would_trim_path_units, \
             would_trim_path_extractable) \
             VALUES (1000, 1000, ?1, 'openai', 'req-model', 'al', 'm', 'paid', \
             'up-paid', 0, 'ok', 5, 0, 0, 1, 0, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                request_id,
                recorder_version,
                dedup_tokens,
                supersession_tokens,
                path_units,
                path_extractable
            ],
        )
        .expect("insert near-lossless attribution row");
}

#[test]
fn render_detail_shows_near_lossless_attribution_line() {
    // Arrange: two recorder-marked rows.
    let (_dir, _path, db) = temp_db();
    near_lossless_attribution_row(&db, "m1", Some(1), 40_000, 10_000, 4, 3);
    near_lossless_attribution_row(&db, "m2", Some(1), 20_000, 20_000, 2, 2);
    let report = report_all(&db, &cost_config(), None, true);

    // Act
    let out = render_report(&report);

    // Assert: recorder count, per-heuristic freed-token breakdown (summed and
    // humanized: 60_000 -> "60K", 30_000 -> "30K"), the extractability rate
    // (5/6 -> 83%), and the advisory framing.
    assert!(
        out.contains("near-lossless-attribution: 2 reqs recorded"),
        "detail output must surface the near-lossless attribution block: {out}"
    );
    assert!(out.contains("dedup=60K"), "summed dedup tokens: {out}");
    assert!(
        out.contains("supersession=30K"),
        "summed supersession tokens: {out}"
    );
    assert!(
        out.contains("path-extractable 83%"),
        "path-extractability rate divides summed counts: {out}"
    );
    assert!(out.contains("not applied"), "advisory framing: {out}");
}

#[test]
fn render_non_detail_omits_near_lossless_attribution_line() {
    // Arrange: a near-lossless recording exists, but the default table must not
    // surface it (only --detail does).
    let (_dir, _path, db) = temp_db();
    near_lossless_attribution_row(&db, "m1", Some(1), 40_000, 10_000, 4, 3);
    let report = report_all(&db, &cost_config(), None, false);

    // Act + Assert
    let out = render_report(&report);
    assert!(
        !out.contains("near-lossless-attribution"),
        "the default (non-detail) table must omit the near-lossless attribution line: {out}"
    );
}

#[test]
fn render_detail_omits_near_lossless_attribution_line_when_no_recorder_rows() {
    // Arrange: only a baseline (pre-recorder) row -- would_trim_recorder_version
    // is NULL, so it must not surface a near-lossless attribution block even
    // under --detail.
    let (_dir, _path, db) = temp_db();
    near_lossless_attribution_row(&db, "baseline", None, 0, 0, 0, 0);
    let report = report_all(&db, &cost_config(), None, true);

    // Act + Assert: no recorder rows -> no attribution block.
    let out = render_report(&report);
    assert!(
        !out.contains("near-lossless-attribution"),
        "no recorder rows -> no near-lossless attribution line: {out}"
    );
}

// --- k-calibration render tests ------------------------------------------

/// Insert a calibration row: a request with an optional `would_trim_k_floor`
/// (None -> uncalibrated, still counts as future reuse) and enough session
/// context to drive the remaining-future realized-reuse count. `session_id`,
/// `provider_kind`, and `model` must be non-NULL so the calibration SQL
/// includes it; `ts_start` drives the remaining-future ordering; a
/// `cache_read > 0` row counts as a reuse event.
#[allow(clippy::too_many_arguments)]
fn insert_calib_row(
    db: &UsageDb,
    request_id: &str,
    ts_start: i64,
    session_id: &str,
    provider_kind: &str,
    model: &str,
    k_floor: Option<f64>,
    cache_read: i64,
) {
    db.conn()
        .execute(
            "INSERT INTO requests (ts_start, ts_end, request_id, ingress_dialect, \
             requested_model, alias, model, provider, upstream, provider_kind, \
             session_id, stream, outcome, latency_ms, tool_count, msg_count, \
             attempt_count, fallback_count, would_trim_tokens, \
             would_trim_break_even_k, would_trim_k_floor, cache_read) \
             VALUES (?7, ?7, ?1, 'openai', 'req-model', 'al', ?4, 'paid', \
             'up-paid', ?3, ?2, 0, 'ok', 5, 0, 0, 1, 0, 10, 1.0, ?5, ?6)",
            rusqlite::params![
                request_id,
                session_id,
                provider_kind,
                model,
                k_floor,
                cache_read,
                ts_start
            ],
        )
        .expect("insert calibration row");
}

#[test]
fn k_calibration_zero_n_returns_friendly_no_data() {
    // Arrange: empty DB.
    let (_dir, _path, db) = temp_db();

    // Act
    let cal = k_calibration_summary(&db).unwrap();
    let out = render_k_calibration(&cal);

    // Assert: no-data path, not a divide-by-zero panic.
    assert_eq!(cal.n, 0);
    assert!(
        out.contains("no calibrated predictions"),
        "must surface friendly message: {out}"
    );
}

#[test]
fn k_calibration_known_population_matches_remaining_future_values() {
    // Arrange: ONE session with reuse concentrated EARLY, so whole-session
    // realized reuse and REMAINING-FUTURE realized reuse diverge. The v2
    // calibration compares each floor against reuse that remains AFTER that
    // row -- a late over-prediction is now a miss, not a bogus success.
    //   r1 ts=100 hit,  floor=1.0  -> 1 future hit (r2) -> covered (1>=1)
    //   r2 ts=200 hit,  UNCALIBRATED (feeds future reuse, not population)
    //   r3 ts=300 miss, floor=2.0  -> 0 future hits      -> MISS (0<2)
    //   r4 ts=400 miss, floor=0.5  -> 0 future hits      -> MISS (0<0.5)
    // Whole-session (v1) would have blessed all three (group total = 2 hits).
    // Per-row errors: |1-1|/2=0, |2-0|/1=2, |0.5-0|/1=0.5 -> median 0.5.
    let (_dir, _path, db) = temp_db();
    insert_calib_row(&db, "r1", 100, "s1", "anth", "m1", Some(1.0), 5);
    insert_calib_row(&db, "r2", 200, "s1", "anth", "m1", None, 5);
    insert_calib_row(&db, "r3", 300, "s1", "anth", "m1", Some(2.0), 0);
    insert_calib_row(&db, "r4", 400, "s1", "anth", "m1", Some(0.5), 0);

    // Act
    let cal = k_calibration_summary(&db).unwrap();

    // Assert hand-computed remaining-future values.
    assert_eq!(cal.n, 3, "only the 3 calibrated rows form the population");
    let coverage_expected = 1.0_f64 / 3.0;
    assert!(
        (cal.coverage - coverage_expected).abs() < 1e-9,
        "remaining-future coverage mismatch: got {}, expected {}",
        cal.coverage,
        coverage_expected
    );
    assert!(
        (cal.accuracy - 0.5).abs() < 1e-9,
        "per-row-normalized median accuracy mismatch: got {}",
        cal.accuracy
    );
}

#[test]
fn k_calibration_sufficiency_below_200_fails() {
    // Arrange: 3 rows -> n=3 < 200.
    let (_dir, _path, db) = temp_db();
    insert_calib_row(&db, "x1", 100, "s1", "a", "m", Some(1.0), 1);
    insert_calib_row(&db, "x2", 200, "s2", "a", "m", Some(1.0), 1);
    insert_calib_row(&db, "x3", 300, "s3", "a", "m", Some(1.0), 1);

    let cal = k_calibration_summary(&db).unwrap();
    let out = render_k_calibration(&cal);

    assert!(
        out.contains("sufficiency") && out.contains("FAIL"),
        "sufficiency must FAIL when n < 200: {out}"
    );
    assert!(out.contains("overall: FAIL"), "overall must FAIL: {out}");
}

#[test]
fn k_calibration_render_all_pass_when_thresholds_met() {
    // Arrange: craft a KCalibration that passes both gates (coverage +
    // sufficiency). hazard_decay and accuracy are diagnostics and never
    // affect the pass/fail verdict.
    let cal = KCalibration {
        n: 250,
        coverage: 0.92,
        accuracy: 0.30,
        hazard_decay: -0.05,
    };

    let out = render_k_calibration(&cal);

    assert!(out.contains("overall: PASS"), "all gates pass: {out}");
    assert!(!out.contains("FAIL"), "no FAIL in output: {out}");
}

#[test]
fn k_calibration_render_accuracy_is_diagnostic_not_a_gate() {
    // Arrange: accuracy fails its reference threshold (0.80 > 0.40) but
    // coverage and sufficiency both pass. Accuracy is a diagnostic, not a
    // safety gate -- overall must still be PASS.
    let cal = KCalibration {
        n: 250,
        coverage: 0.92,
        accuracy: 0.80,
        hazard_decay: 0.0,
    };

    let out = render_k_calibration(&cal);

    assert!(
        out.contains("overall: PASS"),
        "failing accuracy must not flip the gate: {out}"
    );
    assert!(!out.contains("FAIL"), "no FAIL in output: {out}");
}

#[test]
fn k_calibration_render_surfaces_hazard_decay_diagnostic_line() {
    // Arrange: a decaying hazard should render on its own signed diagnostic
    // line, clearly labelled as NOT a gate.
    let cal = KCalibration {
        n: 250,
        coverage: 0.95,
        accuracy: 0.20,
        hazard_decay: -0.12,
    };

    // Act
    let out = render_k_calibration(&cal);

    // Assert: the line shows the signed value and marks itself a diagnostic.
    assert!(
        out.contains("hazard_decay"),
        "hazard_decay line must be present: {out}"
    );
    assert!(
        out.contains("-0.120"),
        "hazard_decay value must render signed: {out}"
    );
    assert!(
        out.contains("diagnostic") && out.contains("not a gate"),
        "hazard_decay must be labelled a non-gate diagnostic: {out}"
    );
    // It must not flip the overall verdict (a diagnostic, not a gate).
    assert!(out.contains("overall: PASS"), "gates still pass: {out}");
}

#[test]
fn k_calibration_remaining_future_counts_only_later_group_reuse() {
    // Arrange: two rows sharing one (session, provider_kind, model). An early
    // calibrated row sees the LATER row's reuse; the later calibrated row sees
    // none. The v1 whole-session join gave BOTH rows the group total (1); v2
    // gives each row only the reuse that remains after it.
    //   mc1 ts=100 hit,  floor=0.5 -> 0 future hits (mc2 is a miss) -> MISS
    //   mc2 ts=200 miss, floor=0.5 -> 0 future hits                 -> MISS
    let (_dir, _path, db) = temp_db();
    insert_calib_row(&db, "mc1", 100, "shared-sess", "anth", "opus", Some(0.5), 5);
    insert_calib_row(&db, "mc2", 200, "shared-sess", "anth", "opus", Some(0.5), 0);

    // Act
    let cal = k_calibration_summary(&db).unwrap();

    // Assert: both calibrated rows counted; neither has remaining reuse >=
    // 0.5, so coverage is 0 (v1 whole-session would have covered both).
    assert_eq!(cal.n, 2, "both calibrated rows are counted");
    assert_eq!(
        cal.coverage, 0.0,
        "no reuse remains after either row: coverage must be 0, got {}",
        cal.coverage
    );
    // errors: mc1 |0.5-0|/1=0.5, mc2 |0.5-0|/1=0.5 -> median 0.5.
    assert!(
        (cal.accuracy - 0.5).abs() < 1e-9,
        "per-row-normalized median accuracy should be 0.5: got {}",
        cal.accuracy
    );
}
