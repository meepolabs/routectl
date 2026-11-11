//! Response translator tests for the openai-responses provider.
//!
//! Loaded via `#[path = "response_tests.rs"] mod tests;` in
//! `response.rs` to keep that file under the 800-line cap while
//! still letting the tests reach the `pub(crate)`-visible API.

use super::*;
use serde_json::json;

fn parse(body: Value) -> ChatResponse {
    parse_as(AuthKind::ChatgptOauth, body)
}

fn parse_as(auth_kind: AuthKind, body: Value) -> ChatResponse {
    let typed: ResponsesResponse = serde_json::from_value(body).unwrap();
    translate("test", auth_kind, typed).unwrap()
}

#[test]
fn response_with_message_and_reasoning_collapses_to_content_parts() {
    // Arrange: reasoning + assistant text. parts has only Text (no
    // reasoning blocks in parts), so content collapses to Text and
    // reasoning rides on reasoning_details.
    let body = json!({
        "id": "resp_01",
        "status": "completed",
        "model": "gpt-5-codex",
        "output": [
            {
                "type": "reasoning",
                "id": "rs_1",
                "summary": [{"type": "summary_text", "text": "thinking step"}],
                "content": [{"type": "reasoning_text", "text": "detail"}],
                "encrypted_content": "ENC1"
            },
            {
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "answer"}]
            }
        ]
    });

    // Act
    let resp = parse(body);

    // Assert
    match &resp.choices[0].message.content {
        MessageContent::Text(t) => assert_eq!(t, "answer"),
        other => panic!("expected Text, got {other:?}"),
    }
    let details = &resp.choices[0].message.reasoning_details;
    // summary + content + encrypted_content
    assert_eq!(details.len(), 3);
    assert!(matches!(details[0].kind, ReasoningDetailKind::Summary));
    assert!(matches!(details[1].kind, ReasoningDetailKind::Text));
    assert!(matches!(details[2].kind, ReasoningDetailKind::Encrypted));
    for d in details {
        assert_eq!(
            d.format.as_deref(),
            Some(lane_format_tag(AuthKind::ChatgptOauth))
        );
    }
}

#[test]
fn response_thinking_signature_round_trips_from_encrypted_content() {
    // The replay signature on a Reasoning item must surface as an
    // Encrypted reasoning_details entry so the next-turn ingress
    // can echo it back verbatim (codex arc_monitor.rs:325-336).
    let body = json!({
        "output": [{
            "type": "reasoning",
            "id": "rs_1",
            "summary": [],
            "encrypted_content": "SIG_PAYLOAD"
        }]
    });
    let resp = parse(body);
    let details = &resp.choices[0].message.reasoning_details;
    assert_eq!(details.len(), 1);
    assert!(matches!(details[0].kind, ReasoningDetailKind::Encrypted));
    assert_eq!(details[0].payload["encrypted_content"], "SIG_PAYLOAD");
}

#[test]
fn response_function_call_translates_to_tool_use() {
    let body = json!({
        "status": "completed",
        "output": [{
            "type": "function_call",
            "id": "fc_1",
            "call_id": "call_abc",
            "name": "get_weather",
            "arguments": "{\"loc\":\"Tokyo\"}"
        }]
    });
    let resp = parse(body);
    let tcs = resp.choices[0].message.tool_calls.as_ref().unwrap();
    assert_eq!(tcs.len(), 1);
    assert_eq!(tcs[0]["id"], "call_abc");
    assert_eq!(tcs[0]["function"]["name"], "get_weather");
    assert_eq!(tcs[0]["function"]["arguments"], "{\"loc\":\"Tokyo\"}");
}

#[test]
fn response_status_completed_maps_to_finish_stop() {
    let body = json!({
        "status": "completed",
        "output": [{
            "type": "message",
            "id": "m1",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "ok"}]
        }]
    });
    let resp = parse(body);
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
}

#[test]
fn response_status_completed_with_function_call_maps_to_finish_tool_calls() {
    let body = json!({
        "status": "completed",
        "output": [{
            "type": "function_call",
            "id": "fc",
            "call_id": "c",
            "name": "n",
            "arguments": "{}"
        }]
    });
    let resp = parse(body);
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("tool_calls"));
}

#[test]
fn response_incomplete_max_tokens_maps_to_finish_length() {
    let body = json!({
        "status": "incomplete",
        "incomplete_details": {"reason": "max_output_tokens"},
        "output": [{
            "type": "message",
            "id": "m",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "trun"}]
        }]
    });
    let resp = parse(body);
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("length"));
}

#[test]
fn response_incomplete_content_filter_maps_to_finish_content_filter() {
    let body = json!({
        "status": "incomplete",
        "incomplete_details": {"reason": "content_filter"},
        "output": []
    });
    let resp = parse(body);
    assert_eq!(
        resp.choices[0].finish_reason.as_deref(),
        Some("content_filter")
    );
}

#[test]
fn response_failed_status_maps_to_error() {
    let body = json!({
        "status": "failed",
        "error": {"message": "internal"},
        "output": []
    });
    let resp = parse(body);
    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("error"));
}

#[test]
fn response_unknown_output_item_passes_through_as_other() {
    // Forward compat: a future item type (e.g. web_search_call,
    // mcp_call, custom_tool_call) must not break the response. We
    // surface it as ContentPart::Other in Parts so downstream
    // egresses that know the type can re-emit it. The actual
    // upstream `type` tag is preserved verbatim.
    let body = json!({
        "status": "completed",
        "output": [
            {"type": "web_search_call", "id": "ws_1"},
            {"type": "message", "id": "m1", "role": "assistant",
             "content": [{"type": "output_text", "text": "done"}]}
        ]
    });
    let resp = parse(body);
    match &resp.choices[0].message.content {
        MessageContent::Parts(parts) => {
            assert_eq!(parts.len(), 2);
            match &parts[0] {
                ContentPart::Other {
                    type_tag, extras, ..
                } => {
                    assert_eq!(type_tag, "web_search_call");
                    assert_eq!(extras.get("id").and_then(|v| v.as_str()), Some("ws_1"));
                }
                other => panic!("expected Other, got {other:?}"),
            }
        }
        other => panic!("expected Parts, got {other:?}"),
    }
}

#[test]
fn response_text_only_collapses_to_message_content_text() {
    let body = json!({
        "status": "completed",
        "output": [{
            "type": "message",
            "id": "m1",
            "role": "assistant",
            "content": [
                {"type": "output_text", "text": "hello "},
                {"type": "output_text", "text": "world"}
            ]
        }]
    });
    let resp = parse(body);
    match &resp.choices[0].message.content {
        MessageContent::Text(t) => assert_eq!(t, "hello world"),
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn response_mixed_parts_keeps_message_content_parts() {
    // Message text + a refusal block forces Parts (refusal is not
    // plain Text). The text block also survives in parts.
    let body = json!({
        "status": "completed",
        "output": [{
            "type": "message",
            "id": "m1",
            "role": "assistant",
            "content": [
                {"type": "output_text", "text": "first"},
                {"type": "refusal", "refusal": "no"}
            ]
        }]
    });
    let resp = parse(body);
    match &resp.choices[0].message.content {
        MessageContent::Parts(parts) => {
            assert_eq!(parts.len(), 2);
            match &parts[1] {
                ContentPart::Other {
                    type_tag, extras, ..
                } => {
                    assert_eq!(type_tag, "refusal");
                    assert_eq!(extras.get("refusal").and_then(|v| v.as_str()), Some("no"));
                }
                other => panic!("expected Other, got {other:?}"),
            }
        }
        other => panic!("expected Parts, got {other:?}"),
    }
}

#[test]
fn response_function_call_arguments_invalid_json_preserved_as_string() {
    // Arrange: an upstream that ships arguments that aren't valid
    // JSON (truncated, partial, or model hallucination). The
    // canonical ContentPart::ToolUse.input must preserve the
    // original string verbatim so the egress can re-emit it
    // rather than silently nulling.
    let body = json!({
        "status": "completed",
        "output": [{
            "type": "function_call",
            "id": "fc",
            "call_id": "call_zz",
            "name": "n",
            "arguments": "not valid json"
        }]
    });

    // Act
    let resp = parse(body);

    // Assert: tool_calls still carries the raw string (OpenAI
    // wire shape) AND ContentPart::ToolUse.input preserves it
    // as a JSON string rather than Value::Null.
    match &resp.choices[0].message.content {
        MessageContent::Parts(parts) => {
            let tu = parts
                .iter()
                .find_map(|p| match p {
                    ContentPart::Known(KnownContentPart::ToolUse { input, .. }) => Some(input),
                    _ => None,
                })
                .expect("ToolUse part present");
            assert_eq!(tu, &Value::String("not valid json".to_string()));
        }
        other => panic!("expected Parts, got {other:?}"),
    }
}

#[test]
fn response_function_call_empty_arguments_translate_to_an_empty_object() {
    // Arrange
    let body = json!({
        "status": "completed",
        "output": [{
            "type": "function_call",
            "id": "fc",
            "call_id": "call_now",
            "name": "now",
            "arguments": ""
        }]
    });

    // Act
    let resp = parse(body);

    // Assert: both canonical carriers hold the empty object.
    let msg = &resp.choices[0].message;
    let tcs = msg.tool_calls.as_ref().expect("tool_calls");
    assert_eq!(tcs[0]["function"]["arguments"], "{}");
    let MessageContent::Parts(parts) = &msg.content else {
        panic!("expected Parts, got {:?}", msg.content);
    };
    let input = parts
        .iter()
        .find_map(|p| match p {
            ContentPart::Known(KnownContentPart::ToolUse { input, .. }) => Some(input),
            _ => None,
        })
        .expect("ToolUse part present");
    assert_eq!(input, &json!({}));
}

#[test]
fn response_reasoning_with_text_variant_is_recognized() {
    // Arrange: a Reasoning content array using the plain "text"
    // discriminant (codex's ReasoningItemContent::Text variant)
    // rather than "reasoning_text". The translator must still
    // accumulate it as a Text reasoning detail.
    let body = json!({
        "status": "completed",
        "output": [{
            "type": "reasoning",
            "id": "rs_1",
            "summary": [],
            "content": [{"type": "text", "text": "hello"}]
        }]
    });

    // Act
    let resp = parse(body);

    // Assert: at least one Text-kind reasoning detail with the
    // payload preserved verbatim.
    let details = &resp.choices[0].message.reasoning_details;
    let text_detail = details
        .iter()
        .find(|d| matches!(d.kind, ReasoningDetailKind::Text))
        .expect("Text reasoning detail emitted");
    assert_eq!(text_detail.payload["text"], "hello");
}

#[test]
fn response_unknown_output_item_preserves_value() {
    // Arrange: an output item with an unknown type carrying a
    // nested payload. The translator's ContentPart::Other arm
    // must lift the type tag verbatim and preserve every other
    // field in extras.
    let body = json!({
        "status": "completed",
        "output": [
            {
                "type": "new_unknown_thing",
                "payload": {"foo": "bar"},
                "id": "x_1"
            },
            {
                "type": "message",
                "id": "m1",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "ok"}]
            }
        ]
    });

    // Act
    let resp = parse(body);

    // Assert
    match &resp.choices[0].message.content {
        MessageContent::Parts(parts) => {
            let other = parts
                .iter()
                .find_map(|p| match p {
                    ContentPart::Other {
                        type_tag, extras, ..
                    } => Some((type_tag, extras)),
                    _ => None,
                })
                .expect("Other part present");
            assert_eq!(other.0, "new_unknown_thing");
            assert_eq!(other.1.get("payload"), Some(&json!({"foo": "bar"})));
            assert_eq!(other.1.get("id").and_then(|v| v.as_str()), Some("x_1"));
        }
        other => panic!("expected Parts, got {other:?}"),
    }
}

#[test]
fn response_reasoning_detail_id_propagates_upstream_item_id() {
    // Arrange: a Reasoning item with a stable upstream id.
    // Every emitted ReasoningDetail for that item must carry the
    // same id so the egress can group details back into one
    // Reasoning input item on the next-turn replay.
    let body = json!({
        "output": [{
            "type": "reasoning",
            "id": "rs_stable_42",
            "summary": [{"type": "summary_text", "text": "step"}],
            "content": [{"type": "reasoning_text", "text": "detail"}],
            "encrypted_content": "SIG"
        }]
    });

    // Act
    let resp = parse(body);

    // Assert: summary + content + encrypted_content all share
    // the upstream id.
    let details = &resp.choices[0].message.reasoning_details;
    assert!(!details.is_empty());
    for d in details {
        assert_eq!(d.id.as_deref(), Some("rs_stable_42"));
    }
}

#[test]
fn response_usage_extracts_input_output_tokens() {
    let body = json!({
        "status": "completed",
        "output": [{
            "type": "message",
            "id": "m1",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "x"}]
        }],
        "usage": {
            "input_tokens": 100,
            "input_tokens_details": {"cached_tokens": 40},
            "output_tokens": 25,
            "output_tokens_details": {"reasoning_tokens": 15},
            "total_tokens": 125
        }
    });
    let resp = parse(body);
    let u = resp.usage.unwrap();
    assert_eq!(u.prompt_tokens, 100);
    assert_eq!(u.completion_tokens, 25);
    assert_eq!(u.total_tokens, 125);
    assert_eq!(u.cache_read_input_tokens, Some(40));
    assert_eq!(u.reasoning_tokens, Some(15));
}

#[test]
fn response_usage_derives_absent_total_from_components() {
    let body = json!({
        "status": "completed",
        "output": [{
            "type": "message",
            "id": "m1",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "x"}]
        }],
        "usage": {
            "input_tokens": 100,
            "output_tokens": 25
        }
    });
    let resp = parse(body);
    let u = resp.usage.unwrap();
    assert_eq!(u.total_tokens, 125);
}

#[test]
fn codex_resets_in_seconds_lifted() {
    // Arrange: a usage-limit body with the relative reset form.
    let body = json!({
        "error": {
            "type": "usage_limit_reached",
            "message": "5-hour cap reached",
            "resets_in_seconds": 1800
        }
    });

    // Act
    let hint = codex_reset_hint(&body);

    // Assert: the relative count is taken verbatim.
    assert_eq!(hint, Some(Duration::from_mins(30)));
}

#[test]
fn codex_resets_at_epoch_computed() {
    // Arrange: a usage-limit body with only the absolute reset form,
    // set to a fixed far-future epoch (year ~2286).
    let far_future: u64 = 9_999_999_999;
    let body = json!({
        "error": {
            "type": "usage_limit_reached",
            "message": "cap reached",
            "resets_at": far_future
        }
    });

    // Act
    let hint = codex_reset_hint(&body).expect("far-future epoch must yield a hint");

    // Assert: positive and bounded by the absolute target (now > 0).
    assert!(hint > Duration::ZERO, "future reset must be positive");
    assert!(
        hint <= Duration::from_secs(far_future),
        "delay cannot exceed the absolute target"
    );
}

#[test]
fn non_usage_limit_returns_none() {
    // Arrange: a different error type carrying reset fields anyway.
    let body = json!({
        "error": {
            "type": "rate_limit_exceeded",
            "resets_in_seconds": 60,
            "resets_at": 9_999_999_999u64
        }
    });

    // Act + Assert: only `usage_limit_reached` qualifies.
    assert!(codex_reset_hint(&body).is_none());
}

#[test]
fn garbage_returns_none() {
    // Arrange: bodies with no usable structure.
    let no_error = json!({ "foo": "bar" });
    let usage_limit_no_fields = json!({
        "error": { "type": "usage_limit_reached", "message": "no reset fields" }
    });
    let not_an_object = json!("plain string");

    // Act + Assert
    assert!(codex_reset_hint(&no_error).is_none());
    assert!(codex_reset_hint(&usage_limit_no_fields).is_none());
    assert!(codex_reset_hint(&not_an_object).is_none());
}

#[test]
fn translate_carries_the_first_message_item_id() {
    // Arrange: a reasoning item first, then two message items.
    let body = json!({
        "id": "resp_01",
        "status": "completed",
        "model": "gpt-5-codex",
        "output": [
            {"type": "reasoning", "id": "rs_1", "summary": []},
            {"type": "message", "id": "msg_first", "role": "assistant",
             "content": [{"type": "output_text", "text": "a"}]},
            {"type": "message", "id": "msg_second", "role": "assistant",
             "content": [{"type": "output_text", "text": "b"}]}
        ]
    });

    // Act
    let resp = parse(body);

    // Assert
    let meta = resp.upstream_meta.expect("envelope carried");
    assert_eq!(meta.responses_message_item_id(), Some("msg_first"));
    assert_eq!(meta.responses_created_at(), None);
}

#[test]
fn translate_carries_no_envelope_without_a_message_item_id_or_member() {
    let rows = [
        (
            "message without id",
            json!([{"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": "a"}]}]),
        ),
        (
            "message with empty id",
            json!([{"type": "message", "id": "", "role": "assistant",
                    "content": [{"type": "output_text", "text": "a"}]}]),
        ),
        (
            "function call with only typed members",
            json!([{"type": "function_call", "call_id": "call_1",
                    "name": "f", "arguments": "{}"}]),
        ),
    ];
    for (name, output) in rows {
        // Arrange
        let body = json!({"id": "resp_01", "status": "completed", "model": "m", "output": output});

        // Act
        let resp = parse(body);

        // Assert: only the dialect tag is carried.
        assert_eq!(
            resp.upstream_meta,
            Some(UpstreamMeta::default().with_wire_dialect(IngressDialect::Responses)),
            "{name}"
        );
    }
}

/// Cache counts one usage object yields: (read, write total, split as
/// `(lifetime seconds, tokens)` pairs).
type CacheCounts = (Option<u32>, Option<u32>, Option<Vec<(u32, u32)>>);

fn complete_cache_counts(usage: &Value) -> (u32, CacheCounts) {
    let body = json!({"status": "completed", "output": [], "usage": usage});
    let u = parse(body).usage.expect("complete usage");
    let split = u.cache_creation.map(|c| c.iter().collect());
    (
        u.prompt_tokens,
        (
            u.cache_read_input_tokens,
            u.cache_creation_input_tokens,
            split,
        ),
    )
}

fn stream_cache_counts(usage: &Value) -> (u32, CacheCounts) {
    let mut state = super::super::sse::ResponsesStreamState::default();
    let event: super::super::response_types::ResponsesStreamEvent = serde_json::from_value(json!({
        "type": "response.completed",
        "response": {"id": "r", "model": "m", "status": "completed", "output": [],
                     "usage": usage}
    }))
    .expect("event parse");
    let chunks = state.parse_event("test", event).expect("event processing");
    let u = chunks
        .last()
        .and_then(|c| c.usage.clone())
        .expect("stream usage");
    let split = u.cache_creation.map(|c| c.iter().collect());
    (
        u.prompt_tokens.expect("stream prompt tokens"),
        (
            u.cache_read_input_tokens,
            u.cache_creation_input_tokens,
            split,
        ),
    )
}

/// Every row runs through the complete translator and the streaming
/// `response.completed` path, which must agree with each other and with
/// the row's expected counts.
///
/// The "documented write sample" row copies the usage shape a public
/// OpenAI Responses report shows for a fresh prompt (TanStack/ai issue
/// 1734: input_tokens 1941, input_tokens_details cache_write_tokens 1938,
/// cached_tokens 0). That shape is a derived claim taken from a
/// third-party report, not a
/// capture from this repo's corpus: no committed fixture shows the
/// ChatGPT-OAuth backend sending `cache_write_tokens`, which is why an
/// absent field must stay `None` rather than become 0.
#[test]
fn cache_usage_is_equal_on_complete_and_stream_paths() {
    let usage = |details: Option<Value>| {
        let mut u = json!({"input_tokens": 1941, "output_tokens": 5, "total_tokens": 1946});
        if let Some(d) = details {
            u["input_tokens_details"] = d;
        }
        u
    };
    let rows: [(&str, Value, CacheCounts); 7] = [
        (
            "cached only",
            usage(Some(json!({"cached_tokens": 40}))),
            (Some(40), None, None),
        ),
        (
            "documented write sample, total with no split",
            usage(Some(
                json!({"cache_write_tokens": 1938, "cached_tokens": 0}),
            )),
            (Some(0), Some(1938), None),
        ),
        (
            "write total plus a duration-named member is split",
            usage(Some(json!({
                "cache_write_tokens": 1938,
                "cache_write_1h_tokens": 1500,
                "cached_tokens": 0
            }))),
            (Some(0), Some(1938), Some(vec![(3600, 1500)])),
        ),
        (
            "duration-named members alone sum to the total",
            usage(Some(json!({
                "cache_write_30m_tokens": 700,
                "cache_write_1h_tokens": 300
            }))),
            (None, Some(1000), Some(vec![(1800, 700), (3600, 300)])),
        ),
        (
            "no write field stays absent",
            usage(None),
            (None, None, None),
        ),
        (
            "observed zero write stays zero",
            usage(Some(json!({"cache_write_tokens": 0, "cached_tokens": 12}))),
            (Some(12), Some(0), None),
        ),
        (
            "read-side duration name is never a write",
            usage(Some(json!({"cached_1h_tokens": 9}))),
            (None, None, None),
        ),
    ];

    for (name, usage, want) in rows {
        let complete = complete_cache_counts(&usage);
        let stream = stream_cache_counts(&usage);
        assert_eq!(complete, stream, "{name}: complete and stream differ");
        assert_eq!(complete, (1941, want), "{name}");
    }
}

#[test]
fn translate_carries_unmodeled_top_level_members_on_the_envelope() {
    // Arrange: a modeled `error: null` and two members the parser does not
    // model, one of them non-ASCII.
    let body = json!({
        "id": "resp_01", "object": "response", "status": "completed", "model": "m",
        "error": null, "service_tier": "default", "metadata": {"note": "caf\u{e9}"},
        "output": []
    });

    // Act
    let resp = parse(body);

    // Assert
    let meta = resp.upstream_meta.expect("carrier");
    assert_eq!(meta.wire_dialect, Some(IngressDialect::Responses));
    let extras = &meta.openai_responses.expect("envelope").extras;
    let keys: Vec<&str> = extras.keys().map(String::as_str).collect();
    assert_eq!(keys, ["metadata", "service_tier"]);
    assert_eq!(extras["metadata"], json!({"note": "caf\u{e9}"}));
}

#[test]
fn translate_usage_carries_unmodeled_members_and_details_objects_whole() {
    // Arrange
    let body = json!({
        "id": "resp_01", "status": "completed", "model": "m", "output": [],
        "usage": {
            "input_tokens": 10, "output_tokens": 4, "total_tokens": 14,
            "input_tokens_details": {"cached_tokens": 6, "audio_tokens": 1},
            "output_tokens_details": {"reasoning_tokens": 2, "x": 3},
            "cost": {"usd": 0.01}
        }
    });

    // Act
    let usage = parse(body).usage.expect("usage");

    // Assert: typed reads unchanged, every member carried.
    assert_eq!(usage.cache_read_input_tokens, Some(6));
    assert_eq!(usage.reasoning_tokens, Some(2));
    assert_eq!(
        Value::Object(usage.extras),
        json!({
            "input_tokens_details": {"cached_tokens": 6, "audio_tokens": 1},
            "output_tokens_details": {"reasoning_tokens": 2, "x": 3},
            "cost": {"usd": 0.01}
        })
    );
}

#[test]
fn translate_usage_without_members_carries_nothing() {
    // Arrange
    let body = json!({
        "id": "resp_01", "status": "completed", "model": "m", "output": [],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
    });

    // Act
    let usage = parse(body).usage.expect("usage");

    // Assert
    assert!(usage.extras.is_empty());
}

#[test]
fn translate_carries_message_and_function_call_item_members_by_identity() {
    // Arrange: the item shapes a Responses upstream sends, one value
    // non-ASCII; a reasoning item's members ride its own map.
    let body = json!({
        "id": "resp_01", "status": "completed", "model": "m",
        "output": [
            {"type": "reasoning", "id": "rs_1", "summary": [], "metadata": {"turn_id": "t"}},
            {"type": "message", "id": "msg_1", "role": "assistant", "status": "completed",
             "phase": "final_answer", "metadata": {"turn_id": "caf\u{e9}"},
             "content": [{"type": "output_text", "text": "a"}]},
            {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "f",
             "arguments": "{}", "status": "completed", "namespace": "ns"}
        ]
    });

    // Act
    let resp = parse(body);

    // Assert
    let envelope = resp
        .upstream_meta
        .and_then(|meta| meta.openai_responses)
        .expect("envelope");
    let members = envelope.item_members;
    assert_eq!(
        Value::Object(members.message["msg_1"].clone()),
        json!({"phase": "final_answer", "metadata": {"turn_id": "caf\u{e9}"}})
    );
    assert_eq!(
        Value::Object(members.function_call["call_1"].clone()),
        json!({"id": "fc_1", "status": "completed"})
    );
    assert_eq!(members.message.len(), 1);
    assert_eq!(members.function_call.len(), 1);
    assert!(envelope.added_item_members.is_empty());
}

#[test]
fn translate_carries_reasoning_item_members_by_item_id() {
    // Arrange: the reasoning item members a codex backend stamps.
    let body = json!({
        "id": "resp_01", "status": "completed", "model": "m",
        "output": [
            {"type": "reasoning", "id": "rs_1", "status": "completed", "summary": [],
             "encrypted_content": "enc", "metadata": {"turn_id": "t"},
             "internal_chat_message_metadata_passthrough": {"k": "caf\u{e9}"}}
        ]
    });

    // Act
    let resp = parse(body);

    // Assert
    let envelope = resp
        .upstream_meta
        .and_then(|meta| meta.openai_responses)
        .expect("envelope");
    assert_eq!(
        Value::Object(envelope.item_members.reasoning["rs_1"].clone()),
        json!({"status": "completed", "metadata": {"turn_id": "t"},
               "internal_chat_message_metadata_passthrough": {"k": "caf\u{e9}"}})
    );
}
