//! Unit tests for `log_safe`. Split out so `log_safe.rs` stays under
//! the 800-line file budget. Loaded via
//! `#[cfg(test)] #[path = "log_safe_tests.rs"] mod tests;` from
//! `log_safe.rs`. `super::*` resolves to the `log_safe` module since
//! this file is the body of `mod tests` declared inside `log_safe`.

use super::{redact_prompts_with_flag, sanitize_for_log, sanitize_upstream_body};
use serde_json::json;

#[test]
fn ascii_printable_passes_through_unchanged() {
    let s = "claude-sonnet-4-5-20250929";
    assert_eq!(sanitize_for_log(s), s);
}

#[test]
fn space_is_preserved() {
    assert_eq!(sanitize_for_log("a b c"), "a b c");
}

#[test]
fn newline_is_replaced_with_placeholder() {
    // Embedded `\n` would forge fake log lines on text-format
    // tracing subscribers. Must be filtered.
    assert_eq!(sanitize_for_log("a\nb"), "a?b");
}

#[test]
fn ansi_escape_is_replaced_with_placeholder() {
    // ANSI escape sequences could re-color terminal output and
    // hide subsequent log content. Each non-printable byte
    // becomes `?`.
    assert_eq!(sanitize_for_log("\x1b[31mred\x1b[0m"), "?[31mred?[0m");
}

#[test]
fn multibyte_utf8_emoji_replaced_per_char() {
    // Non-ASCII chars are not in the printable set; one
    // placeholder per char regardless of byte width.
    assert_eq!(sanitize_for_log("hi-rocket"), "hi-rocket");
    assert_eq!(sanitize_for_log("hi\u{1F680}rocket"), "hi?rocket");
}

#[test]
fn truncates_at_max_chars() {
    let long = "a".repeat(300);
    let got = sanitize_for_log(&long);
    assert_eq!(got.chars().count(), 256);
    assert!(got.chars().all(|c| c == 'a'));
}

#[test]
fn truncation_happens_before_filter() {
    // `take(256).chars()` runs before the printable filter, so
    // the cap counts EVERY input char including ones that will
    // be replaced. Documents the actual behavior.
    let mut s = String::new();
    for _ in 0..300 {
        s.push('\n');
    }
    let got = sanitize_for_log(&s);
    assert_eq!(got.chars().count(), 256);
    assert!(got.chars().all(|c| c == '?'));
}

#[test]
fn upstream_body_html_collapsed_to_marker() {
    // A misconfigured base_url often lands on a CDN error page.
    // Don't dump multi-KB markup into our error envelope.
    let html = "<!DOCTYPE html><html><head><title>404</title></head>...";
    let got = sanitize_upstream_body(html);
    assert!(got.starts_with("<html error page"), "got: {got}");
}

#[test]
fn upstream_body_short_passes_through_trimmed() {
    let body = "  rate limited, retry in 5s  ";
    assert_eq!(sanitize_upstream_body(body), "rate limited, retry in 5s");
}

#[test]
fn upstream_body_long_truncated_with_marker() {
    let long = "x".repeat(crate::MAX_LOG_BODY_EXCERPT + 100);
    let got = sanitize_upstream_body(&long);
    assert!(
        got.ends_with("... [truncated]"),
        "expected truncation marker; got tail: ...{}",
        &got[got.len().saturating_sub(20)..]
    );
}

/// PR C / FR-1: the cap-aware variant lets callers pick a larger
/// limit (4 KB for the debug-level full-body log) while reusing
/// the same HTML collapse + trim logic. Pin the cap behavior so
/// debug_upstream_error_body's 4 KB ceiling can't silently drift.
#[test]
fn upstream_body_with_cap_respects_explicit_limit() {
    use super::sanitize_upstream_body_with_cap;
    let body = "y".repeat(10_000);
    let got = sanitize_upstream_body_with_cap(&body, super::MAX_DEBUG_BODY_BYTES);
    // 4096 chars + "... [truncated]" tail (15 chars) = 4111
    assert_eq!(
        got.len(),
        super::MAX_DEBUG_BODY_BYTES + "... [truncated]".len()
    );
    assert!(got.ends_with("... [truncated]"));

    // Short bodies pass through unchanged.
    let short = "tiny";
    assert_eq!(
        sanitize_upstream_body_with_cap(short, super::MAX_DEBUG_BODY_BYTES),
        "tiny"
    );

    // HTML collapse still applies regardless of cap.
    let html = "<!DOCTYPE html><html>...500 lines...</html>";
    let got = sanitize_upstream_body_with_cap(html, super::MAX_DEBUG_BODY_BYTES);
    assert!(got.starts_with("<html error page"));
}

// -----------------------------------------------------------------
// Redaction tests (FR-2: ROUTECTL_LOG_REDACT_PROMPTS=1)
// -----------------------------------------------------------------

#[test]
fn redact_disabled_returns_clone_unchanged() {
    let body = json!({
        "model": "claude-sonnet-4-5",
        "messages": [{"role":"user","content":"secret"}],
    });
    let got = redact_prompts_with_flag(&body, false);
    assert_eq!(got, body);
}

#[test]
fn redact_openai_chat_string_content_replaces_user_text() {
    // OpenAI Chat Completions request shape: messages[].content
    // is a plain string. Redaction must replace the string and
    // preserve sibling structural fields (role, model).
    let body = json!({
        "model": "gpt-5",
        "temperature": 0.7,
        "messages": [
            {"role": "system", "content": "You are a helpful assistant."},
            {"role": "user", "content": "secret prompt"},
        ],
        "tools": [{"type": "function", "function": {"name": "foo"}}],
    });
    let got = redact_prompts_with_flag(&body, true);
    assert_eq!(got["model"], "gpt-5");
    assert_eq!(got["temperature"], 0.7);
    assert_eq!(got["messages"][0]["role"], "system");
    assert_eq!(got["messages"][0]["content"], "<redacted len=28>");
    assert_eq!(got["messages"][1]["role"], "user");
    assert_eq!(got["messages"][1]["content"], "<redacted len=13>");
    // Tool defs preserve structure (function.name is structural).
    assert_eq!(got["tools"][0]["function"]["name"], "foo");
}

#[test]
fn redact_openai_chat_array_content_replaces_text_blocks() {
    // OpenAI Chat Completions also accepts array-of-parts content.
    let body = json!({
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "alpha"},
                {"type": "image_url", "image_url": {"url": "https://x"}},
            ],
        }],
    });
    let got = redact_prompts_with_flag(&body, true);
    assert_eq!(got["messages"][0]["content"][0]["type"], "text");
    assert_eq!(got["messages"][0]["content"][0]["text"], "<redacted len=5>");
    // image_url is not text; preserve.
    assert_eq!(
        got["messages"][0]["content"][1]["image_url"]["url"],
        "https://x"
    );
}

#[test]
fn redact_anthropic_messages_replaces_text_thinking_and_tool_input() {
    // Anthropic Messages: top-level system, content array of parts
    // (text/thinking/tool_use/tool_result).
    let body = json!({
        "model": "claude-sonnet-4-5",
        "system": "You are helpful.",
        "messages": [{
            "role": "assistant",
            "content": [
                {"type": "text", "text": "answer"},
                {"type": "thinking", "thinking": "let me think"},
                {"type": "tool_use", "id": "t1", "name": "calc",
                 "input": {"x": 1, "expr": "secret"}},
                {"type": "tool_result", "tool_use_id": "t1",
                 "content": "result body"},
            ],
        }],
        "tools": [{"name": "calc", "input_schema": {"type": "object"}}],
    });
    let got = redact_prompts_with_flag(&body, true);
    // Structural preservation.
    assert_eq!(got["model"], "claude-sonnet-4-5");
    assert_eq!(got["messages"][0]["role"], "assistant");
    assert_eq!(got["messages"][0]["content"][2]["id"], "t1");
    assert_eq!(got["messages"][0]["content"][2]["name"], "calc");
    assert_eq!(got["messages"][0]["content"][3]["tool_use_id"], "t1");
    // Tool definition structure preserved (name, schema type).
    assert_eq!(got["tools"][0]["name"], "calc");
    assert_eq!(got["tools"][0]["input_schema"]["type"], "object");
    // Redactions.
    assert_eq!(got["system"], "<redacted len=16>");
    assert_eq!(got["messages"][0]["content"][0]["text"], "<redacted len=6>");
    assert_eq!(
        got["messages"][0]["content"][1]["thinking"],
        "<redacted len=12>"
    );
    // tool_use input replaced wholesale.
    assert_eq!(
        got["messages"][0]["content"][2]["input"],
        json!({"redacted": true})
    );
    // tool_result content (string variant).
    assert_eq!(
        got["messages"][0]["content"][3]["content"],
        "<redacted len=11>"
    );
}

#[test]
fn redact_anthropic_system_array_form_recurses_into_blocks() {
    // Anthropic system can be an array of {type:"text", text:...}
    // blocks; ensure recursion hits the inner text fields.
    let body = json!({
        "system": [
            {"type": "text", "text": "block one", "cache_control": {"type": "ephemeral"}},
            {"type": "text", "text": "block two"},
        ],
    });
    let got = redact_prompts_with_flag(&body, true);
    assert_eq!(got["system"][0]["type"], "text");
    assert_eq!(got["system"][0]["text"], "<redacted len=9>");
    // cache_control is structural; preserve.
    assert_eq!(got["system"][0]["cache_control"]["type"], "ephemeral");
    assert_eq!(got["system"][1]["text"], "<redacted len=9>");
}

#[test]
fn redact_openai_responses_replaces_instructions_and_input_text() {
    // OpenAI Responses: top-level instructions + input array of
    // {type:"input_text", text:...} parts.
    let body = json!({
        "model": "gpt-5",
        "instructions": "you are helpful",
        "input": [
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "the prompt"},
            ]},
        ],
        "tool_choice": "auto",
    });
    let got = redact_prompts_with_flag(&body, true);
    assert_eq!(got["model"], "gpt-5");
    assert_eq!(got["tool_choice"], "auto");
    assert_eq!(got["instructions"], "<redacted len=15>");
    assert_eq!(got["input"][0]["content"][0]["text"], "<redacted len=10>");
    // Structural type preserved.
    assert_eq!(got["input"][0]["content"][0]["type"], "input_text");
}

#[test]
fn redact_response_body_preserves_finish_reason_and_usage() {
    // OpenAI Chat Completions response body: choices[].message.content
    // gets redacted; usage / finish_reason / model / id stay intact
    // so operators can still triage cost + termination.
    let body = json!({
        "id": "chatcmpl-abc",
        "model": "gpt-5",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "long answer"},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 10, "completion_tokens": 20, "total_tokens": 30},
    });
    let got = redact_prompts_with_flag(&body, true);
    assert_eq!(got["id"], "chatcmpl-abc");
    assert_eq!(got["model"], "gpt-5");
    assert_eq!(got["choices"][0]["finish_reason"], "stop");
    assert_eq!(got["choices"][0]["index"], 0);
    assert_eq!(got["usage"]["prompt_tokens"], 10);
    assert_eq!(got["usage"]["completion_tokens"], 20);
    assert_eq!(got["usage"]["total_tokens"], 30);
    assert_eq!(got["choices"][0]["message"]["content"], "<redacted len=11>");
}

#[test]
fn redact_anthropic_response_redacts_text_and_tool_input() {
    // Anthropic Messages response shape mirrors the request shape
    // for content blocks. usage + stop_reason must survive.
    let body = json!({
        "id": "msg_01",
        "model": "claude-sonnet-4-5",
        "stop_reason": "end_turn",
        "content": [
            {"type": "text", "text": "answer"},
            {"type": "tool_use", "id": "t1", "name": "calc",
             "input": {"x": 1}},
        ],
        "usage": {"input_tokens": 5, "output_tokens": 10},
    });
    let got = redact_prompts_with_flag(&body, true);
    assert_eq!(got["id"], "msg_01");
    assert_eq!(got["stop_reason"], "end_turn");
    assert_eq!(got["usage"]["input_tokens"], 5);
    assert_eq!(got["content"][0]["text"], "<redacted len=6>");
    assert_eq!(got["content"][1]["input"], json!({"redacted": true}));
    assert_eq!(got["content"][1]["name"], "calc");
}

#[test]
fn redact_image_data_long_string_is_replaced() {
    // Long base64 image data gets redacted; short MIME-like strings
    // do not.
    let long_data = "A".repeat(2000);
    let body = json!({
        "messages": [{
            "role": "user",
            "content": [
                {"type": "image", "source": {"type": "base64",
                    "media_type": "image/png", "data": long_data}},
            ],
        }],
    });
    let got = redact_prompts_with_flag(&body, true);
    let part = &got["messages"][0]["content"][0];
    assert_eq!(part["source"]["type"], "base64");
    // Short MIME string preserved (under 256 chars).
    assert_eq!(part["source"]["media_type"], "image/png");
    // Long data string redacted.
    assert!(part["source"]["data"]
        .as_str()
        .expect("redacted data string")
        .starts_with("<redacted len="));
}

#[test]
fn redact_function_call_arguments_string_redacted() {
    // OpenAI function_call shape: arguments is a JSON-encoded string
    // carrying tool input args (often user-derived).
    let body = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "foo", "arguments": "{\"x\":\"secret\"}"},
                }],
            },
        }],
    });
    let got = redact_prompts_with_flag(&body, true);
    // Structural fields preserved.
    let tc = &got["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(tc["id"], "call_1");
    assert_eq!(tc["type"], "function");
    assert_eq!(tc["function"]["name"], "foo");
    // arguments redacted.
    assert!(tc["function"]["arguments"]
        .as_str()
        .expect("redacted arguments")
        .starts_with("<redacted len="));
}

#[test]
fn redact_unknown_shape_passes_through_unchanged() {
    // Unrelated JSON: nothing to redact, structure intact.
    let body = json!({"foo": 1, "bar": ["a", "b"], "baz": {"q": true}});
    let got = redact_prompts_with_flag(&body, true);
    assert_eq!(got, body);
}

#[test]
fn redact_bedrock_converse_tool_use_input() {
    // Bedrock Converse wire shape: {"toolUse": {"toolUseId":...,
    // "name":..., "input": <Value>}} with NO `type` key on the
    // parent. The Anthropic-shape `type:"tool_use"` arm does not
    // fire here; the dedicated `toolUse` parent-object arm must.
    let body = json!({
        "messages": [{
            "role": "assistant",
            "content": [{
                "toolUse": {
                    "toolUseId": "tooluse_abc",
                    "name": "calc",
                    "input": {"x": 1, "expr": "secret expression"}
                }
            }]
        }],
    });
    let got = redact_prompts_with_flag(&body, true);
    let tu = &got["messages"][0]["content"][0]["toolUse"];
    // Structural fields preserved.
    assert_eq!(tu["toolUseId"], "tooluse_abc");
    assert_eq!(tu["name"], "calc");
    // Input redacted wholesale.
    assert_eq!(tu["input"], json!({"redacted": true}));
}

#[test]
fn redact_openai_responses_refusal_replaced() {
    // OpenAI Responses Refusal block carries safety-flag text
    // derived from the user's prompt; must be redacted.
    let body = json!({
        "id": "resp_abc",
        "model": "gpt-5",
        "output": [{
            "type": "message",
            "role": "assistant",
            "content": [
                {"type": "output_text", "text": "answer"},
                {"type": "refusal", "refusal": "I cannot help with that secret"},
            ],
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 10, "total_tokens": 15},
    });
    let got = redact_prompts_with_flag(&body, true);
    // Structural preservation.
    assert_eq!(got["id"], "resp_abc");
    assert_eq!(got["model"], "gpt-5");
    assert_eq!(got["usage"]["total_tokens"], 15);
    assert_eq!(got["output"][0]["content"][1]["type"], "refusal");
    // Redactions.
    assert_eq!(got["output"][0]["content"][0]["text"], "<redacted len=6>");
    assert!(got["output"][0]["content"][1]["refusal"]
        .as_str()
        .expect("refusal redacted")
        .starts_with("<redacted len="));
}

#[test]
fn redact_bedrock_converse_tool_result_json_and_text() {
    // Bedrock Converse tool result: {"toolResult": {"toolUseId":...,
    // "content": [{"json": <arbitrary Value>} | {"text":...}]}}.
    // Both the json sub-value AND the text sub-value carry tool-
    // returned data that may echo prompt-derived content.
    let body = json!({
        "messages": [{
            "role": "user",
            "content": [{
                "toolResult": {
                    "toolUseId": "tooluse_abc",
                    "content": [
                        {"json": {"result": "secret structured payload"}},
                        {"text": "secret text result"}
                    ]
                }
            }]
        }],
    });
    let got = redact_prompts_with_flag(&body, true);
    let tr = &got["messages"][0]["content"][0]["toolResult"];
    assert_eq!(tr["toolUseId"], "tooluse_abc");
    assert_eq!(tr["content"][0]["json"], json!({"redacted": true}));
    // The text leaf is redacted exactly once (by the generic per-key
    // sweep on recursion, NOT by the toolResult parent handler that
    // would otherwise double-redact and lose the original char count).
    assert_eq!(tr["content"][1]["text"], "<redacted len=18>");
}

#[test]
fn redact_bedrock_converse_reasoning_redacted_content_replaced() {
    // Bedrock Converse `reasoningContent` carries either
    // `reasoningText.{text,signature}` (covered by the generic `text`
    // sweep) or `redactedContent` (an opaque AWS-redacted byte blob
    // derived from the prompt). The opaque variant must not flow
    // verbatim into a routectl trace log.
    let body = json!({
        "output": {
            "message": {
                "role": "assistant",
                "content": [
                    {"reasoningContent": {"reasoningText": {
                        "text": "thinking out loud",
                        "signature": "abc123"
                    }}},
                    {"reasoningContent": {"redactedContent": "BASE64SAFETYBYTES"}},
                ]
            }
        }
    });
    let got = redact_prompts_with_flag(&body, true);
    let parts = &got["output"]["message"]["content"];
    // reasoningText.text covered by generic sweep.
    assert_eq!(
        parts[0]["reasoningContent"]["reasoningText"]["text"],
        "<redacted len=17>"
    );
    // signature is not redacted (operator triage signal -- AWS
    // round-trips it for thinking continuity, not user content).
    assert_eq!(
        parts[0]["reasoningContent"]["reasoningText"]["signature"],
        "abc123"
    );
    // redactedContent collapsed to opaque marker.
    assert_eq!(
        parts[1]["reasoningContent"]["redactedContent"],
        json!({"redacted": true})
    );
}

#[test]
fn redact_openai_image_url_data_uri_replaced() {
    // OpenAI Chat Completions image_url shape:
    // `{type:"image_url", image_url:{url:"data:image/png;base64,..."}}`.
    // The data URI carries base64 image bytes (potentially MB).
    let data_uri = format!("data:image/png;base64,{}", "A".repeat(2000));
    let body = json!({
        "messages": [{
            "role": "user",
            "content": [
                {"type": "image_url", "image_url": {"url": data_uri}},
            ],
        }],
    });
    let got = redact_prompts_with_flag(&body, true);
    let url_val = &got["messages"][0]["content"][0]["image_url"]["url"];
    assert!(url_val
        .as_str()
        .expect("data URI redacted")
        .starts_with("<redacted len="));
}

#[test]
fn redact_openai_image_url_https_passes_through() {
    // Plain https URL is not user content; must NOT be redacted.
    let body = json!({
        "messages": [{
            "role": "user",
            "content": [
                {"type": "image_url", "image_url": {"url": "https://example.com/img.png"}},
            ],
        }],
    });
    let got = redact_prompts_with_flag(&body, true);
    assert_eq!(
        got["messages"][0]["content"][0]["image_url"]["url"],
        "https://example.com/img.png"
    );
}

#[test]
fn truncate_handles_utf8_char_boundary_safely() {
    // serde_json preserves non-ASCII codepoints as multi-byte UTF-8
    // (does NOT escape them as \uXXXX), so a naive byte-slice at
    // the cap can land mid-codepoint and panic. Construct a body
    // whose serialized form has a multi-byte char crossing the
    // cap boundary and assert truncate does not panic.
    // Each emoji is 4 UTF-8 bytes (rocket: F0 9F 9A 80).
    let mut s = String::with_capacity(20_000);
    // 4-byte aligned padding.
    for _ in 0..3000 {
        s.push('a');
    }
    // 4-byte emoji sequence; with a 12000-byte cap and ~3000
    // padding chars before, the cap will fall partway through
    // the emoji bytes.
    for _ in 0..3000 {
        s.push('\u{1F680}'); // rocket
    }
    let body = json!({"messages": [{"role": "user", "content": s}]});
    let got = super::truncate_json_for_log(&body, 12_000);
    assert!(got.contains("[truncated at 12000 bytes]"));
    // The truncated head must be valid UTF-8 (no panic, and the
    // returned String round-trips through char counting).
    let _ = got.chars().count();
}

#[test]
fn truncate_caps_at_max_with_marker() {
    // 32 KB string serialized into a JSON value; truncator caps at
    // MAX_TRACE_BODY_BYTES (16 KB) and appends a marker.
    let big = "x".repeat(32 * 1024);
    let body = json!({"messages": [{"role": "user", "content": big}]});
    let got = super::truncate_json_for_log(&body, super::MAX_TRACE_BODY_BYTES);
    assert!(got.len() <= super::MAX_TRACE_BODY_BYTES + 64);
    assert!(got.contains("[truncated at 16384 bytes]"));
}

#[test]
fn truncate_short_body_no_marker() {
    let body = json!({"a": 1});
    let got = super::truncate_json_for_log(&body, super::MAX_TRACE_BODY_BYTES);
    assert_eq!(got, "{\"a\":1}");
    assert!(!got.contains("[truncated"));
}
