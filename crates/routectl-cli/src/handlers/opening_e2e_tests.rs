//! End-to-end opening frames through `ingress_handle`: the real parsers,
//! the router's pre-content buffer and fallback walk, and the flush grace,
//! against the scripted loopback upstream in `opening_rig`. Also the
//! dialects and the non-streaming path the opening work must not touch.
//! No test touches a live port, config or DB.

use axum::http::StatusCode;
use serde_json::{Value, json};

use super::opening_rig::*;
use crate::ingress::anthropic::AnthropicIngress;
use crate::ingress::openai::OpenAiIngress;
use crate::ingress::openai_responses::ResponsesIngress;

// ------------------------------------------------------------ positive control

#[test]
fn the_anthropic_fixture_carries_an_opening_usage_through_the_real_parser() {
    use routectl_providers::anthropic_api::sse::SseState;

    let mut state = SseState::default();
    let stream = anthropic_stream(Some(OPENER), Some(TERMINAL));
    let data = stream
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .expect("message_start data");

    let chunk = state
        .parse_event("fixture", data)
        .expect("parses")
        .expect("opening chunk");

    let opening = chunk
        .upstream_meta
        .and_then(|m| m.opening_usage)
        .expect("positive control: the opening chunk carries upstream usage");
    assert_eq!(u64::from(opening.input_tokens), OPENER.input);
    assert_eq!(
        opening.cache_read_input_tokens.map(u64::from),
        Some(OPENER.cache_read)
    );
    assert!(chunk.usage.is_none(), "never on canonical usage");
}

// ------------------------------------------------------------ fast and slow

#[tokio::test]
async fn a_fast_winner_opens_with_its_exact_upstream_usage() {
    // Arrange
    let upstream = Upstream::start().await;
    upstream.mount(
        "/anth/v1/messages",
        "",
        Script::sse(anthropic_stream(Some(OPENER), Some(TERMINAL))),
    );
    let (state, _dir) = anthropic_daemon(&upstream).await;
    let request = body(&turn_one());

    // Act
    let turn = anthropic_turn(&state, &request).await;

    // Assert: the opening is the upstream's first event, field for field,
    // with the cache fields disjoint from input_tokens.
    assert_eq!(
        turn.opening(),
        &json!({
            "input_tokens": OPENER.input,
            "output_tokens": 0,
            "cache_creation_input_tokens": OPENER.cache_write(),
            "cache_read_input_tokens": OPENER.cache_read,
            "cache_creation": {
                "ephemeral_5m_input_tokens": OPENER.cache_write_5m,
                "ephemeral_1h_input_tokens": OPENER.cache_write_1h,
            },
        })
    );
    assert_ne!(
        OPENER.input,
        raw_estimate(&request),
        "fixture separates the two"
    );
    // The terminal correction is independent of the opening.
    assert_eq!(turn.terminal_usage()["input_tokens"], TERMINAL.input);
    assert!(turn.frames[0].at.duration_since(turn.started) < GRACE);
}

#[tokio::test]
async fn a_slow_dispatch_flushes_one_estimated_opening_at_the_grace_and_corrects_at_the_end() {
    // Arrange: the upstream head arrives only after the grace.
    let upstream = Upstream::start().await;
    upstream.mount(
        "/anth/v1/messages",
        "",
        Script::sse(anthropic_stream(Some(OPENER), Some(TERMINAL))).with_head_delay(PAST_GRACE),
    );
    let (state, _dir) = anthropic_daemon(&upstream).await;
    let request = body(&turn_one());

    // Act
    let turn = anthropic_turn(&state, &request).await;

    // Assert: exactly one message_start, flushed at the grace, carrying the
    // cold raw estimate rather than the upstream opener that came later.
    assert_eq!(turn.opening_input(), raw_estimate(&request));
    assert!(turn.opening().get("cache_read_input_tokens").is_none());
    let first = turn.frames[0].at.duration_since(turn.started);
    assert!(
        first >= GRACE && first < PAST_GRACE,
        "first body byte at the grace, before the upstream head: {first:?}"
    );
    let terminal = turn.terminal_usage();
    assert_eq!(terminal["input_tokens"], TERMINAL.input);
    assert_eq!(terminal["cache_read_input_tokens"], TERMINAL.cache_read);
}

#[tokio::test]
async fn an_upstream_opener_seen_before_the_grace_does_not_open_a_stream_whose_content_is_late() {
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
    let (state, _dir) = anthropic_daemon(&upstream).await;
    let request = body(&turn_one());

    // Act
    let turn = anthropic_turn(&state, &request).await;

    // Assert
    assert_eq!(turn.opening_input(), raw_estimate(&request));
    assert!(turn.opening().get("cache_read_input_tokens").is_none());
}

#[tokio::test]
async fn a_fast_http_error_stays_an_http_error() {
    // Arrange: a test-owned upstream answers 529 overloaded at once.
    let upstream = Upstream::start().await;
    upstream.mount(
        "/anth/v1/messages",
        "",
        Script::error(
            529,
            &json!({"type": "error",
                    "error": {"type": "overloaded_error", "message": "busy"}}),
        ),
    );
    let (state, _dir) = daemon(
        build(config(
            vec![("anth", anthropic_provider(upstream.base(), "/anth"))],
            &[("opus", "anth", "claude-opus-4-7")],
            &[("claude-opus", &["opus"])],
        ))
        .await,
    );

    // Act
    let turn = send(
        &state,
        AnthropicIngress,
        session_headers(Some(SESSION)),
        &body(&turn_one()),
    )
    .await;

    // Assert: premise -- the upstream was reached; the client gets the
    // real status and the Anthropic error envelope, not a stream.
    assert_eq!(upstream.hits("/anth/v1/messages"), 1);
    assert_eq!(turn.status.as_u16(), 529, "{}", turn.raw);
    let envelope: Value = serde_json::from_str(&turn.raw).expect("json envelope");
    assert_eq!(envelope["type"], "error");
    assert_eq!(envelope["error"]["type"], "overloaded_error");
    assert!(turn.named("message_start").is_empty());
    assert!(state.context_anchors.is_empty());
}

// ------------------------------------------------------------ fallback

#[tokio::test]
async fn a_failed_pre_content_attempt_never_leaks_its_opener_into_the_fallback() {
    // Arrange: lane a opens then fails before content; lane b wins.
    const FAILED_OPENER: InputUsage = InputUsage::plain(777_001);
    const WINNER: InputUsage = InputUsage::plain(4_242);
    let upstream = Upstream::start().await;
    upstream.mount(
        "/a/v1/messages",
        "",
        Script::sse(anthropic_start(Some(FAILED_OPENER)) + &anthropic_overloaded()),
    );
    upstream.mount(
        "/b/v1/messages",
        "",
        Script::sse(anthropic_stream(Some(WINNER), Some(WINNER))),
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

    // Act
    let turn = anthropic_turn(&state, &body(&turn_one())).await;

    // Assert: premise -- both lanes were tried.
    assert_eq!(upstream.hits("/a/v1/messages"), 1);
    assert_eq!(upstream.hits("/b/v1/messages"), 1);
    assert_eq!(turn.opening_input(), WINNER.input);
    assert!(!turn.raw.contains(&FAILED_OPENER.input.to_string()));
}

#[tokio::test]
async fn a_failed_opener_does_not_leak_into_a_translated_fallback() {
    // Arrange
    const FAILED_OPENER: InputUsage = InputUsage::plain(777_002);
    let upstream = Upstream::start().await;
    upstream.mount(
        "/a/v1/messages",
        "",
        Script::sse(anthropic_start(Some(FAILED_OPENER)) + &anthropic_overloaded()),
    );
    upstream.mount(
        "/compat/v1/chat/completions",
        "",
        Script::sse(openai_stream(Some(900))),
    );
    let (state, _dir) = daemon(
        build(config(
            vec![
                ("a", anthropic_provider(upstream.base(), "/a")),
                ("compat", compat_provider(upstream.base(), "/compat")),
            ],
            &[
                ("opus-a", "a", "claude-opus-4-7"),
                ("glm", "compat", "glm-4.6"),
            ],
            &[("claude-opus", &["opus-a", "glm"])],
        ))
        .await,
    );
    let request = body(&turn_one());

    // Act
    let turn = anthropic_turn(&state, &request).await;

    // Assert
    assert_eq!(upstream.hits("/a/v1/messages"), 1);
    assert_eq!(turn.opening_input(), raw_estimate(&request));
    assert!(!turn.raw.contains(&FAILED_OPENER.input.to_string()));
}

// ------------------------------------------------------------ other dialects

#[tokio::test]
async fn openai_and_responses_streams_do_no_anchor_work() {
    // Arrange
    let upstream = Upstream::start().await;
    upstream.mount(
        "/compat/v1/chat/completions",
        "",
        Script::sse(openai_stream(Some(700))),
    );
    let (state, _dir) = translated_daemon(&upstream).await;
    let mut headers = session_headers(None);
    headers.insert(
        "x-session-id",
        axum::http::HeaderValue::from_static("openai-session"),
    );
    let chat = json!({"model": "claude-opus", "stream": true,
                      "messages": [{"role": "user", "content": "hi"}]});
    let responses = json!({"model": "claude-opus", "stream": true, "input": "hi"});

    // Act
    let chat_turn = send(&state, OpenAiIngress, headers.clone(), &chat).await;
    let responses_turn = send(&state, ResponsesIngress, headers, &responses).await;

    // Assert: both streamed successfully, and neither reserved or published.
    assert_eq!(chat_turn.status, StatusCode::OK, "{}", chat_turn.raw);
    assert_eq!(
        responses_turn.status,
        StatusCode::OK,
        "{}",
        responses_turn.raw
    );
    assert!(chat_turn.raw.contains("[DONE]"));
    assert!(state.context_anchors.is_empty());

    // Positive control: the same lane and a session through the Anthropic
    // ingress does anchor.
    anthropic_turn(&state, &body(&turn_one())).await;
    assert_eq!(state.context_anchors.len(), 1);
}

#[tokio::test]
async fn a_non_streaming_request_is_unaffected() {
    let upstream = Upstream::start().await;
    upstream.mount(
        "/anth/v1/messages",
        "",
        Script::json(&json!({
            "id": "msg_01", "type": "message", "role": "assistant", "model": "claude-opus-4-7",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn", "stop_sequence": null,
            "usage": {"input_tokens": 5, "output_tokens": 1}
        })),
    );
    let (state, _dir) = anthropic_daemon(&upstream).await;

    let turn = send(
        &state,
        AnthropicIngress,
        session_headers(Some(SESSION)),
        &messages_body("claude-opus", &turn_one(), false),
    )
    .await;

    assert_eq!(turn.status, StatusCode::OK);
    let resp: Value = serde_json::from_str(&turn.raw).expect("json body");
    assert_eq!(resp["usage"]["input_tokens"], 5);
    assert!(state.context_anchors.is_empty());
}
