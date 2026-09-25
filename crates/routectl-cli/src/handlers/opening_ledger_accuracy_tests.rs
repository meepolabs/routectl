//! The opening diagnostics a completed metered stream persists into its
//! usage row's `extra`, read back from a real ledger: source, reason and
//! numbers on fast and slow openings, every terminal label and the
//! endpoint-based vendor verification, reload reasons, and other dialects.
//! Every expected number is derived from the frame the client actually
//! received. No test touches a live port, config or DB.

use std::time::Duration;

use axum::http::StatusCode;
use routectl_providers::anthropic_api::sse::SseState;
use serde_json::{Value, json};

use super::opening_ledger_rig::*;
use super::opening_rig::*;
use crate::ingress::openai::OpenAiIngress;

// ------------------------------------------------------------ fast openings

#[tokio::test]
async fn a_fast_proxy_opening_records_the_rendered_count_and_an_unverified_terminal() {
    // Arrange: the loopback endpoint is Anthropic-compatible, not the vendor.
    let upstream = Upstream::start().await;
    upstream.mount(
        "/anth/v1/messages",
        "",
        Script::sse(anthropic_stream(Some(OPENER), Some(TERMINAL))),
    );
    let (state, ledger) = anthropic_ledger_daemon(&upstream).await;

    // Act
    let turn = turn_as(&state, &body(&turn_one()), "fast-proxy").await;
    let row = ledger.only_row().await;

    // Assert: positive control -- the number is the client's own frame.
    let rendered = frame_input(turn.opening());
    assert_eq!(rendered, OPENER.total());
    let extra = &row.extra;
    assert_eq!(extra["opening_present"], true);
    assert_eq!(extra["opening_source"], "upstream_wire_unverified");
    assert_eq!(extra["opening_reason"], "upstream_opener");
    assert_eq!(extra["opening_input"], rendered);
    assert_eq!(extra["opening_provisional"], false);
    assert_eq!(extra["opening_lane_switched"], false);
    // An explicit final report relayed by a compatible endpoint is only what
    // that endpoint reported.
    assert_eq!(extra["terminal_source"], "explicit_final");
    assert_eq!(extra["terminal_input"], TERMINAL.total());
    assert_eq!(extra["terminal_vendor_verified"], false);
    let first_event = extra["opening_first_event_ms"].as_u64().expect("timed");
    let first_enqueue = extra["opening_first_enqueue_ms"].as_u64().expect("timed");
    assert!(first_event <= first_enqueue, "{extra}");
    assert!(first_enqueue < GRACE.as_millis() as u64, "{extra}");
}

#[tokio::test]
async fn the_opening_input_counts_each_cache_field_once_and_keeps_an_explicit_zero_raw() {
    // Arrange: a fully cached opener (explicit zero uncached input) with a
    // split TTL breakdown, and a close reporting its own zero raw input.
    const CACHED: InputUsage = InputUsage {
        input: 0,
        cache_write_5m: 120,
        cache_write_1h: 30,
        cache_read: 9_000,
    };
    let upstream = Upstream::start().await;
    upstream.mount(
        "/anth/v1/messages",
        "",
        Script::sse(anthropic_stream(Some(CACHED), Some(CACHED))),
    );
    let (state, ledger) = anthropic_ledger_daemon(&upstream).await;

    // Act
    let turn = turn_as(&state, &body(&turn_one()), "cached").await;
    let row = ledger.only_row().await;

    // Assert
    let opening = turn.opening();
    assert_eq!(opening["input_tokens"], 0, "{opening}");
    assert!(
        frame_ttl_breakdown(opening) > 0,
        "fixture carries a breakdown"
    );
    let rendered = frame_input(opening);
    assert_eq!(row.extra["opening_input"], rendered);
    assert_ne!(
        row.extra["opening_input"],
        rendered + frame_ttl_breakdown(opening),
        "the TTL breakdown is not added twice"
    );
    assert_eq!(row.extra["terminal_source"], "explicit_final");
    assert_eq!(row.extra["terminal_input"], CACHED.total());
}

#[tokio::test]
async fn server_tool_input_changing_the_terminal_count_keeps_both_numbers() {
    // Arrange: the close reports more input than the opener (server-side
    // tool work added input mid-response).
    let end = format!(
        "event: message_delta\ndata: {}\n\nevent: message_stop\ndata: {}\n\n",
        json!({"type": "message_delta",
               "delta": {"stop_reason": "end_turn", "stop_sequence": null},
               "usage": {"input_tokens": TERMINAL.input, "output_tokens": 9,
                         "cache_creation_input_tokens": TERMINAL.cache_write(),
                         "cache_read_input_tokens": TERMINAL.cache_read,
                         "server_tool_use": {"web_search_requests": 1}}}),
        json!({"type": "message_stop"}),
    );
    let upstream = Upstream::start().await;
    upstream.mount(
        "/anth/v1/messages",
        "",
        Script::sse(anthropic_start(Some(OPENER)) + &anthropic_content() + &end),
    );
    let (state, ledger) = anthropic_ledger_daemon(&upstream).await;

    // Act
    let turn = turn_as(&state, &body(&turn_one()), "server-tool").await;
    let row = ledger.only_row().await;

    // Assert
    assert_ne!(OPENER.total(), TERMINAL.total(), "fixture separates them");
    assert_eq!(row.extra["opening_input"], frame_input(turn.opening()));
    assert_eq!(row.extra["terminal_input"], TERMINAL.total());
}

#[tokio::test]
async fn a_fast_vendor_opening_and_its_explicit_close_are_verified() {
    // Arrange: parser output as the first-party endpoint produces it.
    let upstream = Upstream::start().await;
    let router = anthropic_router(&upstream).await;
    let mut vendor = SseState::new("test").with_vendor_opening();
    let chunks = parse(&anthropic_stream(Some(OPENER), Some(TERMINAL)), &mut vendor);
    let ledger = Ledger::new();

    // Act
    let frames = gate_turn(router, chunks, Duration::ZERO, &ledger, "fast-vendor").await;
    let row = ledger.only_row().await;

    // Assert
    let rendered = frame_input(opening_usage_of(&frames));
    assert_eq!(rendered, OPENER.total(), "positive control");
    assert_eq!(row.extra["opening_source"], "upstream_wire");
    assert_eq!(row.extra["opening_input"], rendered);
    assert_eq!(row.extra["terminal_source"], "explicit_final");
    assert_eq!(row.extra["terminal_vendor_verified"], true);
    assert_eq!(row.extra["opening_lane_switched"], false);
}

// ------------------------------------------------------------ slow openings

#[tokio::test]
async fn a_slow_anchored_turn_records_a_provisional_anchor_and_a_late_first_event() {
    // Arrange: turn one opens fast and anchors; turn two's head arrives after
    // the grace, so it opens on the anchor and its upstream first event
    // lands after the first body byte.
    let upstream = Upstream::start().await;
    upstream.mount(
        "/anth/v1/messages",
        "dependencies",
        Script::sse(anthropic_stream(Some(OPENER), Some(TERMINAL))).with_head_delay(PAST_GRACE),
    );
    upstream.mount(
        "/anth/v1/messages",
        "",
        Script::sse(anthropic_stream(Some(OPENER), Some(TERMINAL))),
    );
    let (state, ledger) = anthropic_ledger_daemon(&upstream).await;
    let first = body(&turn_one());
    let second = body(&turn_two());

    // Act
    turn_as(&state, &first, "anchor-1").await;
    let slow = turn_as(&state, &second, "anchor-2").await;
    let rows = ledger.rows(2).await;

    // Assert
    let extra = &rows[1].extra;
    assert_eq!(extra["opening_source"], "anchor");
    assert_eq!(extra["opening_reason"], "anchor_hit");
    assert_eq!(extra["opening_provisional"], true);
    assert_eq!(extra["opening_input"], frame_input(slow.opening()));
    assert_eq!(extra["opening_lane_switched"], false);
    let first_enqueue = extra["opening_first_enqueue_ms"].as_u64().expect("timed");
    let first_event = extra["opening_first_event_ms"].as_u64().expect("timed");
    assert!(first_enqueue >= GRACE.as_millis() as u64, "{extra}");
    assert!(first_event > first_enqueue, "{extra}");
}

#[tokio::test]
async fn a_cold_slow_turn_records_a_provisional_raw_opening() {
    // Arrange: a translated lane (no upstream opener) slower than the grace.
    let upstream = Upstream::start().await;
    upstream.mount(
        "/compat/v1/chat/completions",
        "",
        Script::sse(openai_stream(Some(4_321))).with_head_delay(PAST_GRACE),
    );
    let (state, ledger) = translated_ledger_daemon(&upstream).await;
    let request = body(&turn_one());

    // Act
    let turn = turn_as(&state, &request, "cold-slow").await;
    let row = ledger.only_row().await;

    // Assert
    assert_eq!(turn.opening_input(), raw_estimate(&request));
    assert_eq!(row.extra["opening_source"], "raw");
    assert_eq!(row.extra["opening_reason"], "cold");
    assert_eq!(row.extra["opening_provisional"], true);
    assert_eq!(row.extra["opening_input"], raw_estimate(&request));
    assert!(
        row.extra.get("opening_first_event_ms").is_none(),
        "{}",
        row.extra
    );
    assert_eq!(row.extra["terminal_source"], "explicit_final");
    assert_eq!(row.extra["terminal_input"], 4_321);
}

#[tokio::test]
async fn a_first_event_before_the_grace_with_late_content_is_not_an_upstream_opening() {
    // Arrange: message_start at once, first content only after the grace.
    let upstream = Upstream::start().await;
    upstream.mount(
        "/anth/v1/messages",
        "",
        Script::split(
            anthropic_start(Some(OPENER)),
            PAST_GRACE,
            anthropic_rest(Some(TERMINAL)),
        ),
    );
    let (state, ledger) = anthropic_ledger_daemon(&upstream).await;

    // Act
    turn_as(&state, &body(&turn_one()), "late-content").await;
    let row = ledger.only_row().await;

    // Assert: both instants recorded, and the opening is the estimate.
    let first_event = row.extra["opening_first_event_ms"].as_u64().expect("timed");
    let first_content = u64::try_from(row.ttfb_ms.expect("ttfb")).expect("positive");
    assert!(first_event < GRACE.as_millis() as u64, "{}", row.extra);
    assert!(first_content >= GRACE.as_millis() as u64, "{row:?}");
    assert_eq!(row.extra["opening_source"], "raw");
    assert_eq!(row.extra["opening_provisional"], true);
}

#[tokio::test]
async fn an_unresolved_head_records_its_reason_and_the_lane_that_then_served() {
    // Arrange: no route resolves the requested model before dispatch; the
    // dispatch then resolves after the grace on a lane of its own.
    let upstream = Upstream::start().await;
    let router = build(config(
        vec![("anth", anthropic_provider(upstream.base(), "/anth"))],
        &[("opus", "anth", "claude-opus-4-7")],
        &[],
    ))
    .await;
    assert!(router.opening_lane("claude-opus").is_none(), "premise");
    let chunks = parse(
        &anthropic_stream(None, Some(TERMINAL)),
        &mut SseState::default(),
    );
    let ledger = Ledger::new();

    // Act
    let frames = gate_turn(router, chunks, PAST_GRACE, &ledger, "unresolved").await;
    let row = ledger.only_row().await;

    // Assert
    assert_eq!(
        row.extra["opening_input"],
        frame_input(opening_usage_of(&frames))
    );
    assert_eq!(row.extra["opening_source"], "raw");
    assert_eq!(row.extra["opening_reason"], "lane_unresolved");
    assert_eq!(row.extra["opening_provisional"], true);
    assert_eq!(row.extra["opening_lane_switched"], true);
}

// ------------------------------------------------------------ terminal labels

#[tokio::test]
async fn a_direct_vendor_explicit_close_without_an_opener_is_verified_and_a_proxy_one_is_not() {
    // Arrange: identical streams whose message_start carries no usage, one
    // parsed as the first-party endpoint, one as a compatible endpoint.
    let upstream = Upstream::start().await;
    let stream = anthropic_stream(None, Some(TERMINAL));
    let direct = parse(&stream, &mut SseState::new("test").with_vendor_opening());
    let proxy = parse(&stream, &mut SseState::default());
    let ledger = Ledger::new();

    // Act
    gate_turn(
        anthropic_router(&upstream).await,
        direct,
        Duration::ZERO,
        &ledger,
        "direct-no-opener",
    )
    .await;
    gate_turn(
        anthropic_router(&upstream).await,
        proxy,
        Duration::ZERO,
        &ledger,
        "proxy-no-opener",
    )
    .await;
    let rows = ledger.rows(2).await;

    // Assert: same labels and numbers; only the endpoint separates them.
    for row in &rows {
        assert_eq!(row.extra["terminal_source"], "explicit_final");
        assert_eq!(row.extra["terminal_input"], TERMINAL.total());
        assert!(row.extra.get("opening_first_event_ms").is_none());
    }
    assert_eq!(rows[0].extra["terminal_vendor_verified"], true);
    assert_eq!(rows[1].extra["terminal_vendor_verified"], false);
}

#[tokio::test]
async fn missing_partial_and_proxy_opening_terminals_are_each_labelled() {
    // Arrange: an interim-only translated stream, a close stating only a
    // cache read, and a compatible endpoint's output-only close.
    let partial_end = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":9,\"cache_read_input_tokens\":900}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let upstream = Upstream::start().await;
    upstream.mount(
        "/compat/v1/chat/completions",
        "",
        Script::sse(openai_interim_only_stream(5_000)),
    );
    upstream.mount(
        "/anth/v1/messages",
        "partial",
        Script::sse(
            anthropic_start(Some(InputUsage::plain(0))) + &anthropic_content() + partial_end,
        ),
    );
    upstream.mount(
        "/anth/v1/messages",
        "",
        Script::sse(
            anthropic_start(Some(OPENER)) + &anthropic_content() + &anthropic_end_output_only(),
        ),
    );
    let (translated, translated_ledger) = translated_ledger_daemon(&upstream).await;
    let (anthropic, anthropic_ledger) = anthropic_ledger_daemon(&upstream).await;

    // Act
    turn_as(&translated, &body(&turn_one()), "interim").await;
    turn_as(
        &anthropic,
        &body(&[text("user", "a partial close")]),
        "partial",
    )
    .await;
    turn_as(&anthropic, &body(&turn_one()), "proxy-close").await;
    let interim = translated_ledger.only_row().await;
    let anthropic_rows = anthropic_ledger.rows(2).await;

    // Assert
    assert_eq!(interim.extra["terminal_source"], "missing");
    assert!(interim.extra.get("terminal_input").is_none());
    assert_eq!(anthropic_rows[0].extra["terminal_source"], "partial_final");
    assert_eq!(anthropic_rows[0].extra["terminal_input"], 900);
    assert_eq!(anthropic_rows[1].extra["terminal_source"], "proxy_opening");
    assert_eq!(anthropic_rows[1].extra["terminal_vendor_verified"], false);
    assert_eq!(anthropic_rows[1].extra["terminal_input"], OPENER.total());
}

#[tokio::test]
async fn a_hot_reload_records_the_generation_change_as_the_reason() {
    // Arrange
    const ACTUAL: u64 = 12_345;
    let upstream = Upstream::start().await;
    upstream.mount(
        "/compat/v1/chat/completions",
        "",
        Script::sse(openai_stream(Some(ACTUAL))),
    );
    let (state, ledger) = translated_ledger_daemon(&upstream).await;
    turn_as(&state, &body(&turn_one()), "reload-1").await;

    // Act
    reload(
        &state,
        build(translated_config(upstream.base(), "glm-4.6")).await,
    );
    let after = turn_as(&state, &body(&turn_two()), "reload-2").await;
    let rows = ledger.rows(2).await;

    // Assert
    assert_eq!(rows[0].extra["opening_reason"], "cold");
    assert_eq!(rows[1].extra["opening_reason"], "generation_changed");
    assert_eq!(rows[1].extra["opening_source"], "raw");
    assert_eq!(rows[1].extra["opening_provisional"], false);
    assert_eq!(rows[1].extra["opening_input"], after.opening_input());
}

// ------------------------------------------------------------ other dialects

#[tokio::test]
async fn an_openai_stream_row_carries_no_opening_keys() {
    // Arrange
    let upstream = Upstream::start().await;
    upstream.mount(
        "/compat/v1/chat/completions",
        "",
        Script::sse(openai_stream(Some(700))),
    );
    let (state, ledger) = translated_ledger_daemon(&upstream).await;
    let chat = json!({"model": "claude-opus", "stream": true,
                      "messages": [{"role": "user", "content": "hi"}]});

    // Act
    let openai = send_as(
        &state,
        OpenAiIngress,
        session_headers(None),
        &chat,
        Some("openai"),
    )
    .await;
    turn_as(&state, &body(&turn_one()), "anthropic").await;
    let rows = ledger.rows(2).await;

    // Assert: positive control -- the Anthropic row on the same lane has them.
    assert_eq!(openai.status, StatusCode::OK, "{}", openai.raw);
    assert_eq!(rows[0].ingress, "openai");
    assert_eq!(rows[0].extra, Value::Null);
    assert_eq!(rows[1].ingress, "anthropic");
    assert_eq!(rows[1].extra["opening_present"], true);
}
