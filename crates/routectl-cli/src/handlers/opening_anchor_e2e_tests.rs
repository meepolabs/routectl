//! End-to-end session anchors through `ingress_handle` and the daemon-owned
//! store: which turns open on the anchor, which invalidations send a turn
//! cold, and which endings publish. Against the scripted loopback upstream in
//! `opening_rig`; no test touches a live port, config or DB.

use std::time::Duration;

use serde_json::json;

use super::opening_rig::*;
use crate::ingress::anthropic::AnthropicIngress;

// ------------------------------------------------------------ anchor

#[tokio::test]
async fn a_translated_second_turn_opens_on_the_anchor() {
    // Arrange
    const ACTUAL: u64 = 31_337;
    let upstream = Upstream::start().await;
    upstream.mount(
        "/compat/v1/chat/completions",
        "",
        Script::sse(openai_stream(Some(ACTUAL))),
    );
    let (state, _dir) = translated_daemon(&upstream).await;
    let first = body(&turn_one());
    let second = body(&turn_two());

    // Act
    let cold = anthropic_turn(&state, &first).await;
    let warm = anthropic_turn(&state, &second).await;

    // Assert
    assert_eq!(cold.opening_input(), raw_estimate(&first));
    let expected = expected_anchor(&first, &second, ACTUAL);
    assert_ne!(
        expected,
        raw_estimate(&second),
        "fixture separates the tiers"
    );
    assert_eq!(warm.opening_input(), expected);
}

#[tokio::test]
async fn the_anchor_is_the_cache_inclusive_terminal_total_and_serves_the_warm_path() {
    // Arrange: turn one opens fast on its upstream opener; turn two is slow,
    // so its provisional opening comes from the route head's anchor.
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
    let (state, _dir) = anthropic_daemon(&upstream).await;
    let first = body(&turn_one());
    let second = body(&turn_two());

    // Act
    let fast = anthropic_turn(&state, &first).await;
    let slow = anthropic_turn(&state, &second).await;

    // Assert
    assert_eq!(fast.opening_input(), OPENER.input);
    assert_eq!(
        slow.opening_input(),
        expected_anchor(&first, &second, TERMINAL.total())
    );
    assert_ne!(
        slow.opening_input(),
        expected_anchor(&first, &second, TERMINAL.input),
        "the cache-exclusive input alone would open far lower"
    );
}

#[tokio::test]
async fn compaction_and_a_same_length_rewrite_miss_the_anchor() {
    // Arrange
    const ACTUAL: u64 = 50_000;
    let upstream = Upstream::start().await;
    upstream.mount(
        "/compat/v1/chat/completions",
        "",
        Script::sse(openai_stream(Some(ACTUAL))),
    );
    let (state, _dir) = translated_daemon(&upstream).await;
    let anchored = body(&turn_two());
    anthropic_turn(&state, &anchored).await;
    let compacted = body(&[text("user", "Summary of the earlier conversation.")]);
    let mut rewritten_messages = turn_two();
    rewritten_messages[1] = text("assistant", "The workspace has one member crane.");
    rewritten_messages.push(text("assistant", "More."));
    rewritten_messages.push(text("user", "Next."));
    let rewritten = body(&rewritten_messages);

    // Act
    let after_rewrite = anthropic_turn(&state, &rewritten).await;
    let after_compaction = anthropic_turn(&state, &compacted).await;

    // Assert
    assert_ne!(
        expected_anchor(&anchored, &rewritten, ACTUAL),
        raw_estimate(&rewritten),
        "fixture: a hit would be visible"
    );
    assert_eq!(after_rewrite.opening_input(), raw_estimate(&rewritten));
    assert_eq!(after_compaction.opening_input(), raw_estimate(&compacted));

    // A compacted turn that completed anchors its own continuation.
    let mut continued_messages = vec![text("user", "Summary of the earlier conversation.")];
    continued_messages.push(text("assistant", "Understood."));
    continued_messages.push(text("user", "Continue."));
    let continued = body(&continued_messages);
    let next = anthropic_turn(&state, &continued).await;
    assert_eq!(
        next.opening_input(),
        expected_anchor(&compacted, &continued, ACTUAL)
    );
}

#[tokio::test]
async fn a_reused_session_id_and_another_requested_model_open_cold() {
    // Arrange
    const ACTUAL: u64 = 20_000;
    let upstream = Upstream::start().await;
    upstream.mount(
        "/compat/v1/chat/completions",
        "",
        Script::sse(openai_stream(Some(ACTUAL))),
    );
    let (state, _dir) = translated_daemon(&upstream).await;
    anthropic_turn(&state, &body(&turn_one())).await;
    let subagent = body(&[
        text("user", "You are a subagent. Search for tests."),
        text("assistant", "Searching."),
        text("user", "Report back."),
    ]);
    let other_model = messages_body("claude-haiku", &turn_two(), true);

    // Act
    let sub = anthropic_turn(&state, &subagent).await;
    let haiku = anthropic_turn(&state, &other_model).await;

    // Assert
    assert_eq!(sub.opening_input(), raw_estimate(&subagent));
    assert_eq!(haiku.opening_input(), raw_estimate(&other_model));
}

#[tokio::test]
async fn a_hot_reload_keeps_the_store_but_a_new_generation_or_upstream_opens_cold() {
    // Arrange
    const ACTUAL: u64 = 12_345;
    let upstream = Upstream::start().await;
    upstream.mount(
        "/compat/v1/chat/completions",
        "",
        Script::sse(openai_stream(Some(ACTUAL))),
    );
    let (state, _dir) = translated_daemon(&upstream).await;
    let first = body(&turn_one());
    let second = body(&turn_two());
    let third = body(&turn_three());
    anthropic_turn(&state, &first).await;
    assert_eq!(state.context_anchors.len(), 1);

    // Act: a reload with an identical config moves only the generation.
    reload(
        &state,
        build(translated_config(upstream.base(), "glm-4.6")).await,
    );
    let after_reload = anthropic_turn(&state, &second).await;
    let reanchored = anthropic_turn(&state, &third).await;
    // Then the nickname is repointed at another upstream model.
    reload(
        &state,
        build(translated_config(upstream.base(), "glm-5")).await,
    );
    let mut fourth_messages = turn_three();
    fourth_messages.push(text("assistant", "One."));
    fourth_messages.push(text("user", "Thanks."));
    let fourth = body(&fourth_messages);
    let after_repoint = anthropic_turn(&state, &fourth).await;

    // Assert
    assert_eq!(after_reload.opening_input(), raw_estimate(&second));
    assert_eq!(
        reanchored.opening_input(),
        expected_anchor(&second, &third, ACTUAL),
        "the retained store anchors again under the new generation"
    );
    assert_eq!(after_repoint.opening_input(), raw_estimate(&fourth));
}

#[tokio::test]
async fn a_fresh_daemon_starts_cold() {
    // Arrange
    const ACTUAL: u64 = 12_345;
    let upstream = Upstream::start().await;
    upstream.mount(
        "/compat/v1/chat/completions",
        "",
        Script::sse(openai_stream(Some(ACTUAL))),
    );
    let (old, _old_dir) = translated_daemon(&upstream).await;
    anthropic_turn(&old, &body(&turn_one())).await;
    assert_eq!(
        old.context_anchors.len(),
        1,
        "premise: the old daemon anchored"
    );

    // Act
    let (fresh, _fresh_dir) = translated_daemon(&upstream).await;
    let second = body(&turn_two());
    let turn = anthropic_turn(&fresh, &second).await;

    // Assert
    assert_eq!(turn.opening_input(), raw_estimate(&second));
}

#[tokio::test]
async fn a_seat_rotation_within_one_pool_keeps_the_anchor() {
    // Arrange: a two-seat round-robin pool over one upstream model; openers
    // omitted so the fast path shows the selected opening.
    const ACTUAL: InputUsage = InputUsage::plain(8_888);
    let upstream = Upstream::start().await;
    for prefix in ["/seat-a", "/seat-b"] {
        upstream.mount(
            &format!("{prefix}/v1/messages"),
            "",
            Script::sse(anthropic_stream(None, Some(ACTUAL))),
        );
    }
    let mut pooled = config(
        vec![
            ("seat-a", oauth_seat(upstream.base(), "/seat-a", "seat-a")),
            ("seat-b", oauth_seat(upstream.base(), "/seat-b", "seat-b")),
        ],
        &[("opus", "pool", "claude-opus-4-7")],
        &[("claude-opus", &["opus"])],
    );
    pooled.pools.insert(
        "pool".to_string(),
        routectl_router::config::PoolEntry::new(vec!["seat-a".into(), "seat-b".into()])
            .with_seat_selection(routectl_router::config::SeatSelection::RoundRobin),
    );
    let (state, _dir) = daemon(build(pooled).await);
    let first = body(&turn_one());
    let second = body(&turn_two());

    // Act
    anthropic_turn(&state, &first).await;
    let rotated = anthropic_turn(&state, &second).await;

    // Assert: premise -- the two turns landed on different seats.
    assert_eq!(upstream.hits("/seat-a/v1/messages"), 1);
    assert_eq!(upstream.hits("/seat-b/v1/messages"), 1);
    assert_eq!(
        rotated.opening_input(),
        expected_anchor(&first, &second, ACTUAL.total())
    );
}

#[tokio::test]
async fn a_calibrated_cold_opening_and_an_uncorrected_anchor() {
    // Arrange: seed the lane's calibration with a 1.5x correction.
    const ACTUAL: u64 = 9_000;
    let upstream = Upstream::start().await;
    upstream.mount(
        "/compat/v1/chat/completions",
        "",
        Script::sse(openai_stream(Some(ACTUAL))),
    );
    let router = build(translated_config(upstream.base(), "glm-4.6")).await;
    for i in 0..12_u64 {
        router.record_calibration_sample(
            Some("openai-compat"),
            Some("glm"),
            Some(&format!("cohort-{}", i % 3)),
            1_000,
            1_500,
            std::time::SystemTime::now(),
        );
    }
    let first = body(&turn_one());
    let calibrated = router
        .calibrated_estimate(
            "openai-compat",
            "glm",
            routectl_router::estimate_total_tokens(&canonical(&first)),
        )
        .expect("positive control: the lane has a factor");
    let (state, _dir) = daemon(router);
    let second = body(&turn_two());

    // Act
    let cold = anthropic_turn(&state, &first).await;
    let warm = anthropic_turn(&state, &second).await;

    // Assert
    assert_ne!(calibrated, raw_estimate(&first));
    assert_eq!(cold.opening_input(), calibrated);
    assert_eq!(
        warm.opening_input(),
        expected_anchor(&first, &second, ACTUAL)
    );
}

#[tokio::test]
async fn non_ascii_and_a_large_image_and_tool_result_anchor_like_any_turn() {
    // Arrange
    const ACTUAL: u64 = 180_000;
    let upstream = Upstream::start().await;
    upstream.mount(
        "/compat/v1/chat/completions",
        "",
        Script::sse(openai_stream(Some(ACTUAL))),
    );
    let (state, _dir) = translated_daemon(&upstream).await;
    let image = "iVBORw0KGgo".repeat(40_000);
    let big_result = "\u{65e5}\u{672c}\u{8a9e} line of output\n".repeat(20_000);
    let history = vec![
        json!({"role": "user", "content": [
            {"type": "text", "text": "Describe this, s'il vous pla\u{ee}t \u{1f600}"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": image}},
        ]}),
        json!({"role": "assistant", "content": [
            {"type": "tool_use", "id": "toolu_01", "name": "read", "input": {"path": "\u{e9}.txt"}},
        ]}),
        json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_01", "content": big_result},
        ]}),
    ];
    let mut grown = history.clone();
    grown.push(text("assistant", "Done \u{2014} the file is long."));
    grown.push(text("user", "Summarize it."));
    let first = body(&history);
    let second = body(&grown);

    // Act
    anthropic_turn(&state, &first).await;
    let turn = anthropic_turn(&state, &second).await;

    // Assert
    assert_eq!(
        turn.opening_input(),
        expected_anchor(&first, &second, ACTUAL)
    );
}

// ------------------------------------------------------------ no publication

#[tokio::test]
async fn a_turn_without_terminal_usage_publishes_no_anchor() {
    let upstream = Upstream::start().await;
    upstream.mount(
        "/compat/v1/chat/completions",
        "",
        Script::sse(openai_stream(None)),
    );
    let (state, _dir) = translated_daemon(&upstream).await;

    anthropic_turn(&state, &body(&turn_one())).await;

    assert!(state.context_anchors.is_empty());
}

#[tokio::test]
async fn a_mid_stream_error_publishes_no_anchor() {
    let upstream = Upstream::start().await;
    upstream.mount(
        "/anth/v1/messages",
        "",
        Script::sse(anthropic_start(Some(OPENER)) + &anthropic_content() + &anthropic_overloaded()),
    );
    let (state, _dir) = anthropic_daemon(&upstream).await;

    let turn = anthropic_turn(&state, &body(&turn_one())).await;

    assert_eq!(
        turn.named("error").len(),
        1,
        "premise: the stream failed: {}",
        turn.raw
    );
    assert!(state.context_anchors.is_empty());
}

#[tokio::test]
async fn a_client_that_hangs_up_mid_stream_publishes_no_anchor() {
    // Arrange: the terminal usage would arrive a second after first content.
    let upstream = Upstream::start().await;
    upstream.mount(
        "/anth/v1/messages",
        "",
        Script::split(
            anthropic_start(Some(OPENER)) + &anthropic_content(),
            Duration::from_secs(1),
            anthropic_end(Some(TERMINAL)),
        ),
    );
    let (state, _dir) = anthropic_daemon(&upstream).await;
    let bytes = axum::body::Bytes::from(serde_json::to_vec(&body(&turn_one())).unwrap());

    // Act: read the first body piece, then hang up.
    let resp = super::ingress_handle(
        std::sync::Arc::clone(&state),
        session_headers(Some(SESSION)),
        None,
        Ok(bytes),
        AnthropicIngress,
    )
    .await;
    let mut stream = resp.into_body().into_data_stream();
    futures::StreamExt::next(&mut stream)
        .await
        .expect("first piece")
        .expect("first piece readable");
    drop(stream);
    tokio::time::sleep(Duration::from_millis(1_500)).await;

    // Assert
    assert!(state.context_anchors.is_empty());
}

#[tokio::test]
async fn an_older_overlapping_turn_finishing_last_does_not_replace_the_newer_anchor() {
    // Arrange: the older turn's usage arrives a second late.
    const OLD_ACTUAL: u64 = 1_111;
    const NEW_ACTUAL: u64 = 22_222;
    let upstream = Upstream::start().await;
    let old_stream = openai_stream(Some(OLD_ACTUAL));
    let (old_head, old_tail) = old_stream.split_at(old_stream.find("\n\n").unwrap() + 2);
    upstream.mount(
        "/compat/v1/chat/completions",
        "overlap-old",
        Script::split(
            old_head.to_string(),
            Duration::from_secs(1),
            old_tail.to_string(),
        ),
    );
    upstream.mount(
        "/compat/v1/chat/completions",
        "",
        Script::sse(openai_stream(Some(NEW_ACTUAL))),
    );
    let (state, _dir) = translated_daemon(&upstream).await;
    let older = body(&[text("user", "overlap-old question")]);
    let newer = body(&turn_one());

    // Act
    let older_state = std::sync::Arc::clone(&state);
    let older_body = older.clone();
    let older_turn = tokio::spawn(async move { anthropic_turn(&older_state, &older_body).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    anthropic_turn(&state, &newer).await;
    older_turn.await.expect("older turn");
    let next = body(&turn_two());
    let continued = anthropic_turn(&state, &next).await;

    // Assert
    assert_eq!(
        continued.opening_input(),
        expected_anchor(&newer, &next, NEW_ACTUAL)
    );
}

#[tokio::test]
async fn interim_usage_then_a_clean_end_publishes_no_anchor_until_terminal_usage_arrives() {
    // Arrange: turn one carries only interim usage; turn two carries a real
    // terminal usage chunk.
    const INTERIM: u64 = 64_000;
    const TERMINAL_ACTUAL: u64 = 31_000;
    let upstream = Upstream::start().await;
    upstream.mount(
        "/compat/v1/chat/completions",
        "dependencies",
        Script::sse(openai_stream(Some(TERMINAL_ACTUAL))),
    );
    upstream.mount(
        "/compat/v1/chat/completions",
        "",
        Script::sse(openai_interim_only_stream(INTERIM)),
    );
    let (state, _dir) = translated_daemon(&upstream).await;
    let first = body(&turn_one());
    let second = body(&turn_two());
    let third = body(&turn_three());

    // Act
    let interim = anthropic_turn(&state, &first).await;
    let anchors_after_interim = state.context_anchors.len();
    let cold = anthropic_turn(&state, &second).await;
    let warm = anthropic_turn(&state, &third).await;

    // Assert: positive control -- the interim usage reached the client
    // stream (and so the capture that ledgers it).
    assert!(
        interim.raw.contains(&INTERIM.to_string()),
        "interim usage was rendered: {}",
        interim.raw
    );
    assert_eq!(anchors_after_interim, 0, "interim usage never anchors");
    assert_eq!(cold.opening_input(), raw_estimate(&second));
    assert_eq!(
        warm.opening_input(),
        expected_anchor(&second, &third, TERMINAL_ACTUAL)
    );
}

#[tokio::test]
async fn a_compatible_proxy_output_only_close_stays_cold_but_its_explicit_close_anchors() {
    // Arrange: the loopback upstream is an Anthropic-compatible endpoint,
    // not the first-party host. Turn one closes with output only, so its
    // total is the proxy's own opening. Turn two closes with an explicit
    // input report (a routectl back hop's shape). Turn three sends no
    // opener, so its client opening shows the anchor tier.
    let upstream = Upstream::start().await;
    upstream.mount(
        "/anth/v1/messages",
        "Which version",
        Script::sse(anthropic_stream(None, Some(TERMINAL))),
    );
    upstream.mount(
        "/anth/v1/messages",
        "dependencies",
        Script::sse(anthropic_stream(Some(OPENER), Some(TERMINAL))),
    );
    upstream.mount(
        "/anth/v1/messages",
        "",
        Script::sse(
            anthropic_start(Some(OPENER)) + &anthropic_content() + &anthropic_end_output_only(),
        ),
    );
    let (state, _dir) = anthropic_daemon(&upstream).await;
    let first = body(&turn_one());
    let second = body(&turn_two());
    let third = body(&turn_three());

    // Act
    let proxy_close = anthropic_turn(&state, &first).await;
    let anchors_after_proxy_close = state.context_anchors.len();
    anthropic_turn(&state, &second).await;
    let chained = anthropic_turn(&state, &third).await;

    // Assert: positive control -- the output-only close still reports the
    // carried total to the client.
    assert_eq!(
        proxy_close.terminal_usage()["cache_read_input_tokens"],
        OPENER.cache_read
    );
    assert_eq!(
        anchors_after_proxy_close, 0,
        "an unverified opening never anchors"
    );
    assert_eq!(
        chained.opening_input(),
        expected_anchor(&second, &third, TERMINAL.total())
    );
}

#[tokio::test]
async fn a_losing_attempts_terminal_input_never_anchors() {
    // Arrange: lane a sends an explicit terminal count, then fails before
    // any content; lane b wins with its own explicit count.
    const LOSER: u64 = 777_003;
    const WINNER: u64 = 12_000;
    let upstream = Upstream::start().await;
    upstream.mount(
        "/a/v1/messages",
        "",
        Script::sse(
            anthropic_start(None)
                + &anthropic_end(Some(InputUsage::plain(LOSER)))
                    .replace("event: message_stop", "event: ignored")
                + &anthropic_overloaded(),
        ),
    );
    upstream.mount(
        "/b/v1/messages",
        "",
        Script::sse(anthropic_stream(None, Some(InputUsage::plain(WINNER)))),
    );
    let (state, _dir) = daemon(
        build(config(
            vec![
                ("a", anthropic_provider(upstream.base(), "/a")),
                ("b", anthropic_provider(upstream.base(), "/b")),
            ],
            &[
                ("opus-a", "a", "claude-opus-4-7"),
                ("opus-b", "b", "claude-opus-4-7"),
            ],
            &[("claude-opus", &["opus-a", "opus-b"])],
        ))
        .await,
    );
    let first = body(&turn_one());
    let second = body(&turn_two());

    // Act
    anthropic_turn(&state, &first).await;
    let next = anthropic_turn(&state, &second).await;

    // Assert: premise -- both lanes ran on each turn's walk.
    assert!(upstream.hits("/a/v1/messages") >= 1);
    assert_eq!(
        next.opening_input(),
        expected_anchor(&first, &second, WINNER)
    );
}

#[tokio::test]
async fn a_close_reporting_a_cache_read_without_raw_input_does_not_anchor() {
    // Arrange: a raw-zero opener, then a closing delta stating only the
    // cache read. The client still sees the 900 total.
    let raw_zero = anthropic_start(Some(InputUsage::plain(0)));
    let partial_end = "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":9,\"cache_read_input_tokens\":900}}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let upstream = Upstream::start().await;
    upstream.mount(
        "/anth/v1/messages",
        "",
        Script::sse(raw_zero + &anthropic_content() + partial_end),
    );
    let (state, _dir) = anthropic_daemon(&upstream).await;

    // Act
    let turn = anthropic_turn(&state, &body(&turn_one())).await;

    // Assert
    assert_eq!(turn.terminal_usage()["cache_read_input_tokens"], 900);
    assert!(state.context_anchors.is_empty());
}
