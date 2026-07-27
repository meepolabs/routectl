//! Unit tests for `log_safe`. Split out so `log_safe.rs` stays under
//! the 800-line file budget. Loaded via
//! `#[cfg(test)] #[path = "log_safe_tests.rs"] mod tests;` from
//! `log_safe.rs`. `super::*` resolves to the `log_safe` module since
//! this file is the body of `mod tests` declared inside `log_safe`.

use super::{
    MAX, MAX_DEBUG_BODY_BYTES, extract_upstream_message, is_json_error_envelope,
    redact_prompts_with_flag, sanitize_capped, sanitize_detail_with_flag, sanitize_for_log,
    sanitize_upstream_body,
};
use serde_json::json;

#[test]
fn detail_unredacted_truncates_and_strips_but_keeps_short_content() {
    // Default (no prompt redaction): a short detail passes through, control
    // chars are filtered, and the length cap bounds the field.
    assert_eq!(
        sanitize_detail_with_flag("tool_use block missing `id`", false),
        "tool_use block missing `id`"
    );
    assert_eq!(sanitize_detail_with_flag("a\nb", false), "a?b");
    let long = "x".repeat(MAX + 500);
    assert_eq!(sanitize_detail_with_flag(&long, false).chars().count(), MAX);
}

#[test]
fn detail_redacted_collapses_to_length_marker() {
    // With prompt redaction on, a detail that embedded a raw request fragment
    // never reaches the log line -- only the char count survives.
    let detail = "tool_use block is not an object: {\"input\":\"sk-live-LEAKED-SECRET\"}";
    let redacted = sanitize_detail_with_flag(detail, true);
    assert!(!redacted.contains("LEAKED"), "{redacted}");
    assert!(!redacted.contains("sk-live"), "{redacted}");
    assert_eq!(
        redacted,
        format!("<redacted len={}>", detail.chars().count())
    );
}

#[test]
fn is_json_error_envelope_true_for_top_level_error_object() {
    assert!(is_json_error_envelope(
        r#"{"error":{"type":"invalid_request_error","message":"x"}}"#
    ));
}

#[test]
fn is_json_error_envelope_false_for_non_json() {
    assert!(!is_json_error_envelope("<html>gateway timeout</html>"));
    assert!(!is_json_error_envelope("plain text error"));
}

#[test]
fn is_json_error_envelope_false_for_json_without_error_key() {
    assert!(!is_json_error_envelope(r#"{"detail":"tenant-7 trace"}"#));
}

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

/// The cap-aware variant lets callers pick a larger limit
/// (4 KB for the debug-level full-body log) while reusing the
/// same HTML collapse + trim logic. Pin the cap behavior so
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

/// The byte-cap variant truncates on a UTF-8 char boundary so all-multi-byte
/// input cannot blow past the byte ceiling. A char-count cap would emit up to
/// `4 * cap` bytes for 4-byte-char input; the byte cap keeps the TOTAL output
/// (excerpt plus marker) at or under `cap` bytes and always valid UTF-8.
/// `\u{1F600}` is a 4-byte UTF-8 sequence (written as an escape to keep the
/// source ASCII-only).
#[test]
fn upstream_body_byte_cap_truncates_on_char_boundary() {
    use super::sanitize_upstream_body_with_byte_cap;
    const MARKER: &str = "... [truncated]";
    let cap = crate::MAX_ERROR_BODY_BYTES;

    // Well ABOVE the byte boundary: 20000 * 4 = 80000 bytes for a 64 KB cap.
    let over = "\u{1F600}".repeat(20_000);
    assert!(over.len() > cap, "sanity: input exceeds the byte cap");
    let got = sanitize_upstream_body_with_byte_cap(&over, cap);
    assert!(
        got.ends_with(MARKER),
        "oversized input must carry the marker"
    );
    // The TOTAL output (excerpt + marker) is a strict byte ceiling.
    assert!(
        got.len() <= cap,
        "byte-cap total output {} exceeded the {cap}-byte ceiling",
        got.len()
    );
    // Slicing a &str on a non-boundary would have panicked; reaching here and
    // round-tripping the bytes confirms the output is valid UTF-8.
    assert_eq!(
        String::from_utf8(got.clone().into_bytes()).unwrap(),
        got,
        "byte-cap output must be valid UTF-8"
    );

    // Exactly AT the byte boundary passes through whole (no marker): 16384 * 4
    // = 65536 bytes == cap.
    let at = "\u{1F600}".repeat(cap / 4);
    assert_eq!(at.len(), cap, "sanity: input is exactly at the byte cap");
    let got_at = sanitize_upstream_body_with_byte_cap(&at, cap);
    assert_eq!(got_at, at, "an at-cap body must pass through unchanged");
}

// -----------------------------------------------------------------
// Redaction tests (ROUTECTL_LOG_REDACT_PROMPTS=1)
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
    assert!(
        part["source"]["data"]
            .as_str()
            .expect("redacted data string")
            .starts_with("<redacted len=")
    );
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
    assert!(
        tc["function"]["arguments"]
            .as_str()
            .expect("redacted arguments")
            .starts_with("<redacted len=")
    );
}

#[test]
fn redact_openai_responses_function_call_output_string_form_redacted() {
    // OpenAI Responses outgoing body carries prior turns'
    // function_call_output items in `input`. The `output` field is the
    // tool result -- either a flat string (most common; codex parity)
    // or an array of typed parts. The flat-string form previously fell
    // into the generic `_` arm of the per-key sweep, which is a no-op
    // on Strings, so the tool result leaked verbatim into the trace
    // log even with ROUTECTL_LOG_REDACT_PROMPTS=1.
    let body = json!({
        "model": "gpt-5",
        "input": [
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "list files"}
            ]},
            {"type": "function_call", "call_id": "call_1",
             "name": "ls", "arguments": "{\"path\":\"/etc\"}"},
            {"type": "function_call_output", "call_id": "call_1",
             "output": "passwd shadow group hosts"},
        ],
    });
    let got = redact_prompts_with_flag(&body, true);
    // Structural fields preserved.
    assert_eq!(got["model"], "gpt-5");
    assert_eq!(got["input"][2]["type"], "function_call_output");
    assert_eq!(got["input"][2]["call_id"], "call_1");
    // The tool-result string is redacted.
    assert!(
        got["input"][2]["output"]
            .as_str()
            .expect("output string redacted")
            .starts_with("<redacted len=")
    );
    // Sanity: sibling redactions still fire.
    assert!(
        got["input"][0]["content"][0]["text"]
            .as_str()
            .expect("input_text redacted")
            .starts_with("<redacted len=")
    );
    assert!(
        got["input"][1]["arguments"]
            .as_str()
            .expect("function_call.arguments redacted")
            .starts_with("<redacted len=")
    );
}

#[test]
fn redact_openai_responses_function_call_output_items_form_recurses() {
    // When the tool returned mixed content (e.g. an image + text), the
    // body becomes an array of typed input_text items. Each item's
    // inner `text` leaf must still be redacted via recursion -- the
    // new "output" arm calls redact_string_or_recurse, which recurses
    // when the value is structured rather than redacting the array
    // wholesale.
    let body = json!({
        "input": [
            {"type": "function_call_output", "call_id": "call_2",
             "output": [
                 {"type": "input_text", "text": "tool result chunk 1"},
                 {"type": "input_text", "text": "tool result chunk 2"},
             ]},
        ],
    });
    let got = redact_prompts_with_flag(&body, true);
    let output_items = got["input"][0]["output"]
        .as_array()
        .expect("output stays an array, not redacted wholesale");
    assert_eq!(output_items.len(), 2);
    assert_eq!(output_items[0]["type"], "input_text");
    assert!(
        output_items[0]["text"]
            .as_str()
            .expect("first item text redacted")
            .starts_with("<redacted len=")
    );
    assert!(
        output_items[1]["text"]
            .as_str()
            .expect("second item text redacted")
            .starts_with("<redacted len=")
    );
}

#[test]
fn redact_openai_responses_response_body_output_array_recurses() {
    // The Responses RESPONSE body's top-level `output` is always an
    // array of items. Adding `output` to the redact_string_or_recurse
    // arm must not collapse this structured array into a `<redacted>`
    // string; it must recurse so existing per-key arms (text,
    // arguments) still fire on the inner items.
    let body = json!({
        "id": "resp_abc",
        "model": "gpt-5",
        "output": [
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "answer"}
            ]},
            {"type": "function_call", "call_id": "c1",
             "name": "f", "arguments": "{\"x\":\"secret\"}"},
        ],
        "usage": {"prompt_tokens": 5, "completion_tokens": 10, "total_tokens": 15},
    });
    let got = redact_prompts_with_flag(&body, true);
    // Structural top-level + usage intact.
    assert_eq!(got["id"], "resp_abc");
    assert_eq!(got["usage"]["total_tokens"], 15);
    // output stays an array (NOT collapsed to a redacted string).
    let output = got["output"]
        .as_array()
        .expect("output stays an array on response bodies");
    assert_eq!(output.len(), 2);
    // Inner redactions fire as before.
    assert_eq!(output[0]["content"][0]["text"], "<redacted len=6>");
    assert!(
        output[1]["arguments"]
            .as_str()
            .expect("arguments redacted")
            .starts_with("<redacted len=")
    );
}

#[test]
fn redact_metadata_user_id_collapsed_when_on_verbatim_when_off() {
    // The non-CC cloak writes `metadata.user_id` as a JSON string
    // carrying device_id / account_uuid / session_id. The session_id
    // is a login-session secret, so with redaction ON the value must
    // collapse to the `<redacted len=N>` placeholder. With redaction
    // OFF (fixture-capture posture) the raw value is left verbatim, and
    // sibling keys are untouched either way.
    let raw_user_id = r#"{"device_id":"abc","account_uuid":"def","session_id":"ghi"}"#;
    let body = json!({
        "model": "claude-sonnet-4-5",
        "metadata": {"user_id": raw_user_id, "other": "keep-me"},
    });

    // ON: user_id collapsed, sibling + structural keys intact.
    let on = redact_prompts_with_flag(&body, true);
    assert_eq!(
        on["metadata"]["user_id"],
        json!(format!("<redacted len={}>", raw_user_id.chars().count()))
    );
    assert_eq!(on["metadata"]["other"], "keep-me");
    assert_eq!(on["model"], "claude-sonnet-4-5");

    // OFF: body cloned unchanged (fixture capture relies on the raw value).
    let off = redact_prompts_with_flag(&body, false);
    assert_eq!(off["metadata"]["user_id"], raw_user_id);
    assert_eq!(off, body);
}

#[test]
fn redact_unknown_shape_passes_through_unchanged() {
    // Unrelated JSON: nothing to redact, structure intact.
    let body = json!({"foo": 1, "bar": ["a", "b"], "baz": {"q": true}});
    let got = redact_prompts_with_flag(&body, true);
    assert_eq!(got, body);
}

#[test]
fn redact_canary_no_sentinel_survives_across_content_surfaces() {
    // CANARY: every prompt-bearing surface added by the body-redaction
    // batch must be covered. Each sentinel is unique so a survivor names
    // the exact leak. Built from raw json! (NOT canonical structs) so it
    // exercises the pre-canonical wire-shape walk, where an unmodeled
    // field could otherwise slip through.
    let doc_data_long = format!("SENTINEL_DOC_DATA{}", "A".repeat(300));
    let body = json!({
        "model": "claude-sonnet-4-5",
        // Bare strings under content-bearing keys.
        "system": ["SENTINEL_SYSTEM_ARR"],
        "messages": [{
            "role": "user",
            "content": [
                "SENTINEL_CONTENT_ARR",
                // OpenAI file block file_data leaf.
                {"type": "file", "file": {
                    "filename": "doc.pdf",
                    "file_data": "SENTINEL_FILE_DATA"
                }},
                // content[2]: Anthropic document block. Its top-level
                // `title` leaf is the user-supplied document title; the
                // `source.data` is a long base64 payload caught by the
                // existing `data` arm (256-byte threshold). `citations`
                // here is config ({enabled:true}), not echoed text.
                {"type": "document",
                 "title": "SENTINEL_DOC_TITLE",
                 "source": {"type": "base64", "media_type": "application/pdf",
                            "data": doc_data_long.as_str()},
                 "citations": {"enabled": true}},
                // content[3]: a text block whose `citations` array echoes
                // the source document's cited_text + document_title.
                {"type": "text",
                 "text": "SENTINEL_CITED_BLOCK_TEXT",
                 "citations": [
                     {"type": "char_location",
                      "cited_text": "SENTINEL_CITED_TEXT",
                      "document_title": "SENTINEL_CITATION_DOCTITLE"}
                 ]},
            ],
        }],
        // Responses-shape flat output array of bare strings.
        "output": ["SENTINEL_OUTPUT_ARR"],
    });

    let got = redact_prompts_with_flag(&body, true);
    let serialized = serde_json::to_string(&got).expect("serialize redacted body");

    for sentinel in [
        "SENTINEL_SYSTEM_ARR",
        "SENTINEL_CONTENT_ARR",
        "SENTINEL_FILE_DATA",
        "SENTINEL_DOC_TITLE",
        "SENTINEL_DOC_DATA",
        "SENTINEL_CITED_BLOCK_TEXT",
        "SENTINEL_CITED_TEXT",
        "SENTINEL_CITATION_DOCTITLE",
        "SENTINEL_OUTPUT_ARR",
    ] {
        assert!(
            !serialized.contains(sentinel),
            "{sentinel} survived redaction: {serialized}"
        );
    }
    // Structural fields stay visible: model, role, file filename, block
    // type discriminators.
    assert_eq!(got["model"], "claude-sonnet-4-5");
    assert_eq!(got["messages"][0]["role"], "user");
    assert_eq!(
        got["messages"][0]["content"][1]["file"]["filename"],
        "doc.pdf"
    );
    assert_eq!(got["messages"][0]["content"][1]["type"], "file");
    assert_eq!(got["messages"][0]["content"][2]["type"], "document");
    // citations value collapsed wholesale to the opaque marker.
    assert_eq!(
        got["messages"][0]["content"][3]["citations"],
        json!({"redacted": true})
    );
}

#[test]
fn redact_file_data_redacts_under_both_file_and_input_file_shapes() {
    // file_data is an upload payload and must redact wherever the key
    // appears, regardless of parent `type`. Two wire shapes carry it:
    //   - raw OpenAI Chat: {type:"file", file:{file_data:"<base64>"}}
    //   - provider-normalized OpenAI Responses:
    //       {type:"input_file", file_data:"data:...base64..."}
    // The Responses shape has `file_data` directly on the part object
    // (no nested `file` wrapper), so a structural `type:"file"` special
    // case missed it -- the generic key arm catches both.
    let responses_payload = format!("data:application/pdf;base64,{}", "Z".repeat(2000));
    let body = json!({
        "messages": [{
            "role": "user",
            "content": [
                // Raw OpenAI Chat file block.
                {"type": "file", "file": {
                    "filename": "doc.pdf",
                    "file_data": "SENTINEL_RAW_FILE_DATA"
                }},
                // OpenAI Responses normalized input_file shape.
                {"type": "input_file",
                 "filename": "report.pdf",
                 "file_data": responses_payload},
            ],
        }],
    });
    let got = redact_prompts_with_flag(&body, true);
    let serialized = serde_json::to_string(&got).expect("serialize redacted body");

    // The raw upload payload must not survive under either shape.
    assert!(
        !serialized.contains("SENTINEL_RAW_FILE_DATA"),
        "raw file block file_data leaked: {serialized}"
    );
    assert!(
        !serialized.contains("base64,ZZZ"),
        "input_file base64 payload leaked: {serialized}"
    );

    // Both file_data leaves collapsed to the placeholder.
    assert!(
        got["messages"][0]["content"][0]["file"]["file_data"]
            .as_str()
            .expect("raw file_data redacted")
            .starts_with("<redacted len=")
    );
    assert!(
        got["messages"][0]["content"][1]["file_data"]
            .as_str()
            .expect("input_file file_data redacted")
            .starts_with("<redacted len=")
    );

    // Structural metadata stays visible on both shapes.
    assert_eq!(
        got["messages"][0]["content"][0]["file"]["filename"],
        "doc.pdf"
    );
    assert_eq!(got["messages"][0]["content"][0]["type"], "file");
    assert_eq!(got["messages"][0]["content"][1]["filename"], "report.pdf");
    assert_eq!(got["messages"][0]["content"][1]["type"], "input_file");
}

#[test]
fn redact_nested_string_array_under_content_key_does_not_leak() {
    // CANARY: a nested array under a content-bearing key
    // (`content: [["SENTINEL_NESTED"]]`) previously routed its inner
    // array back through the generic redact_value Array arm, which is a
    // no-op on bare strings -- so the sentinel leaked. redact_content_array
    // now recurses into nested arrays preserving the content context.
    let body = json!({
        "messages": [{
            "role": "user",
            "content": [["SENTINEL_NESTED"]],
        }],
    });
    let got = redact_prompts_with_flag(&body, true);
    let serialized = serde_json::to_string(&got).expect("serialize redacted body");
    assert!(
        !serialized.contains("SENTINEL_NESTED"),
        "nested-array sentinel survived redaction: {serialized}"
    );
    // The nested array structure is preserved; only the leaf collapses.
    assert_eq!(got["messages"][0]["content"][0][0], "<redacted len=15>");
}

#[test]
fn redact_does_not_touch_string_array_under_non_content_key() {
    // NEGATIVE placement guard: bare-string array redaction must
    // fire ONLY under content-bearing keys. A model-id list, the
    // anthropic_beta flags, and arbitrary string arrays under non-content
    // keys MUST survive verbatim -- if bare-string array redaction lived in the generic Array arm
    // it would over-redact all of these.
    let body = json!({
        "models": ["gpt-5", "claude-sonnet-4-5", "gemini-2.5-pro"],
        "anthropic_beta": ["context-1m-2025-08-07", "prompt-cache-1h"],
        "stop": ["END", "STOP"],
        "tags": ["a", "b", "c"],
    });
    let got = redact_prompts_with_flag(&body, true);
    assert_eq!(got, body, "non-content string arrays must survive verbatim");
}

#[test]
fn redact_preserves_structural_identifiers_and_returns_input_when_disabled() {
    // NEGATIVE: model / role / usage / finish_reason and tool names+ids
    // are operator-triage signal, never content -- they must stay visible
    // with redaction ON. And with redaction OFF the body is returned
    // unchanged (the enabled=false short-circuit).
    let body = json!({
        "model": "gpt-5",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "the secret answer",
                "tool_calls": [{
                    "id": "call_abc",
                    "type": "function",
                    "function": {"name": "lookup", "arguments": "{\"q\":\"secret\"}"}
                }],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": {"prompt_tokens": 7, "completion_tokens": 11, "total_tokens": 18},
    });

    // ON: structural identifiers survive; content + arguments redact.
    let on = redact_prompts_with_flag(&body, true);
    assert_eq!(on["model"], "gpt-5");
    assert_eq!(on["choices"][0]["index"], 0);
    assert_eq!(on["choices"][0]["message"]["role"], "assistant");
    assert_eq!(on["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(on["usage"]["total_tokens"], 18);
    let tc = &on["choices"][0]["message"]["tool_calls"][0];
    assert_eq!(tc["id"], "call_abc");
    assert_eq!(tc["type"], "function");
    assert_eq!(tc["function"]["name"], "lookup");
    assert_eq!(on["choices"][0]["message"]["content"], "<redacted len=17>");
    assert!(
        tc["function"]["arguments"]
            .as_str()
            .expect("arguments redacted")
            .starts_with("<redacted len=")
    );

    // OFF: input returned unchanged.
    let off = redact_prompts_with_flag(&body, false);
    assert_eq!(off, body);
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
    assert!(
        got["output"][0]["content"][1]["refusal"]
            .as_str()
            .expect("refusal redacted")
            .starts_with("<redacted len=")
    );
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
    assert!(
        url_val
            .as_str()
            .expect("data URI redacted")
            .starts_with("<redacted len=")
    );
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
    // Body bigger than cap; truncator caps at MAX_TRACE_BODY_BYTES
    // and appends the configured marker. Use a fixed 12 KB cap so the
    // test stays deterministic and survives any operator bump to
    // MAX_TRACE_BODY_BYTES (campaigns occasionally raise the default
    // for full-body debugging).
    let cap = 12 * 1024;
    let big = "x".repeat(2 * cap);
    let body = json!({"messages": [{"role": "user", "content": big}]});
    let got = super::truncate_json_for_log(&body, cap);
    assert!(got.len() <= cap + 64);
    assert!(got.contains(&format!("[truncated at {cap} bytes]")));
}

#[test]
fn truncate_short_body_no_marker() {
    let body = json!({"a": 1});
    let got = super::truncate_json_for_log(&body, super::MAX_TRACE_BODY_BYTES);
    assert_eq!(got, "{\"a\":1}");
    assert!(!got.contains("[truncated"));
}

// ---------------------------------------------------------------------
// structural summary (issue #7)
// ---------------------------------------------------------------------

#[test]
fn extract_structural_summary_extracts_nominal_fields() {
    let body = json!({
        "model": "claude-sonnet-4-5",
        "max_tokens": 4096,
        "stream": true,
        "thinking": {"type": "enabled", "budget_tokens": 8192},
        "tool_choice": "auto",
        "anthropic_beta": ["context-1m-2025-08-07", "prompt-cache-1h"],
        "provider_extras": {"context_management": {"type": "default"}, "mcp_servers": []},
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{"name": "t1"}, {"name": "t2"}],
    });
    let s = super::extract_structural_summary(&body);
    assert_eq!(s.model.as_deref(), Some("claude-sonnet-4-5"));
    assert_eq!(s.max_tokens, Some(4096));
    assert_eq!(s.stream, Some(true));
    assert_eq!(s.thinking_shape.as_deref(), Some("enabled:8192"));
    assert_eq!(s.output_config_effort, None);
    assert_eq!(s.tool_choice_shape.as_deref(), Some("auto"));
    assert_eq!(s.cache_control_count, 0);
    assert_eq!(s.messages_len, 1);
    assert_eq!(s.tools_len, 2);
    assert_eq!(
        s.anthropic_beta,
        vec!["context-1m-2025-08-07", "prompt-cache-1h"]
    );
    // provider_extras keys come back sorted for stable greps.
    assert_eq!(
        s.provider_extras_keys,
        vec!["context_management".to_string(), "mcp_servers".to_string()]
    );
}

#[test]
fn extract_structural_summary_handles_missing_keys() {
    let body = json!({});
    let s = super::extract_structural_summary(&body);
    assert_eq!(s.model, None);
    assert_eq!(s.max_tokens, None);
    assert_eq!(s.stream, None);
    assert_eq!(s.thinking_shape, None);
    assert_eq!(s.output_config_effort, None);
    assert_eq!(s.tool_choice_shape, None);
    assert_eq!(s.cache_control_count, 0);
    assert_eq!(s.messages_len, 0);
    assert_eq!(s.tools_len, 0);
    assert!(s.anthropic_beta.is_empty());
    assert!(s.provider_extras_keys.is_empty());
}

#[test]
fn extract_structural_summary_walks_cache_control_nested() {
    // Three cache_control breakpoints:
    //   - top-level (Anthropic-shape on the request itself)
    //   - one inside messages[0].content[1]
    //   - one inside tools[0]
    let body = json!({
        "cache_control": {"type": "ephemeral"},
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "no breakpoint"},
                {"type": "text", "text": "with breakpoint", "cache_control": {"type": "ephemeral"}},
            ],
        }],
        "tools": [{
            "name": "t1",
            "cache_control": {"type": "ephemeral"},
        }],
    });
    let s = super::extract_structural_summary(&body);
    assert_eq!(s.cache_control_count, 3);
}

#[test]
fn extract_structural_summary_omits_budget_tokens() {
    // The raw budget integer is encoded into the discriminator
    // string. The StructuralSummary struct does NOT carry the raw
    // u32 -- the operator's validator wants a single stable string
    // field per shape.
    let body = json!({"thinking": {"type": "enabled", "budget_tokens": 12345}});
    let s = super::extract_structural_summary(&body);
    assert_eq!(s.thinking_shape.as_deref(), Some("enabled:12345"));
    // Verify there is no separate `budget_tokens` field. Compile-time
    // proof: any `s.budget_tokens` access here would not compile.
    // Runtime documentation: the only place the budget value appears
    // is inside the discriminator string.
    let dump = format!("{:?}", s);
    // The discriminator carries the int; no separate int field exposes it.
    assert!(dump.contains("enabled:12345"));
}

#[test]
fn extract_structural_summary_collapses_tool_choice_shapes() {
    for (input, expected) in [
        (json!("auto"), "auto"),
        (json!("none"), "none"),
        (json!("required"), "required"),
        (
            json!({"type": "function", "function": {"name": "x"}}),
            "function:x",
        ),
        // Flat shape (Anthropic / OpenAI Responses).
        (json!({"type": "function", "name": "x"}), "function:x"),
        // Forward-compat unknown object discriminator.
        (json!({"type": "tool"}), "object:tool"),
    ] {
        let body = json!({"tool_choice": input});
        let s = super::extract_structural_summary(&body);
        assert_eq!(
            s.tool_choice_shape.as_deref(),
            Some(expected),
            "tool_choice {body} should collapse to {expected:?}"
        );
    }
}

#[test]
fn extract_structural_summary_adaptive_thinking_pairs_with_effort() {
    let body = json!({
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": "high"},
    });
    let s = super::extract_structural_summary(&body);
    assert_eq!(s.thinking_shape.as_deref(), Some("adaptive:high"));
    assert_eq!(s.output_config_effort.as_deref(), Some("high"));
}

#[test]
fn extract_structural_summary_uses_input_for_responses_shape() {
    // OpenAI Responses ingress carries the conversation in `input`
    // rather than `messages`. The structural extractor counts either.
    let body = json!({
        "input": [
            {"role": "user", "content": "a"},
            {"role": "user", "content": "b"},
        ],
    });
    let s = super::extract_structural_summary(&body);
    assert_eq!(s.messages_len, 2);
}

// ---------------------------------------------------------------------
// header trace helpers (headers_to_json)
// ---------------------------------------------------------------------

#[test]
fn headers_to_json_preserves_order_duplicates_and_lossy_decodes() {
    // The wire shape is an ARRAY of [name, value] pairs, not an
    // object: header ORDER and DUPLICATE names (set-cookie, repeated
    // via) must survive. A JSON object would collapse / reorder them.
    // A non-UTF-8 byte value is lossy-decoded, not dropped.
    let pairs: Vec<(&str, &[u8])> = vec![
        ("set-cookie", b"a=1".as_slice()),
        ("x-order", b"second".as_slice()),
        ("set-cookie", b"b=2".as_slice()),
        ("x-binary", &[0xffu8, 0xfe][..]),
    ];

    let got = super::headers_to_json(pairs);

    let arr = got.as_array().expect("top-level array");
    assert_eq!(arr.len(), 4);
    // Order preserved.
    assert_eq!(arr[0], json!(["set-cookie", "a=1"]));
    assert_eq!(arr[1], json!(["x-order", "second"]));
    // Duplicate set-cookie kept as a distinct, later entry (not
    // collapsed onto the first).
    assert_eq!(arr[2], json!(["set-cookie", "b=2"]));
    // Non-UTF-8 bytes lossy-decoded to the replacement char rather
    // than dropping the header.
    assert_eq!(arr[3][0], "x-binary");
    assert!(
        arr[3][1]
            .as_str()
            .expect("value string")
            .contains('\u{FFFD}')
    );
}

#[test]
fn headers_to_json_value_with_newline_serializes_escaped() {
    // A header value carrying a raw newline (a log-injection attempt)
    // must serialize with an ESCAPED `\n`, never a literal newline, so
    // the compact-string emit in the trace helpers cannot forge a
    // second log line on a text-format subscriber.
    let got = super::headers_to_json([("x-evil", "line1\nline2".as_bytes())]);

    let serialized = serde_json::to_string(&got).expect("serialize");

    assert!(
        !serialized.contains('\n'),
        "raw newline leaked into output: {serialized}"
    );
    assert!(
        serialized.contains("\\n"),
        "newline was not escaped: {serialized}"
    );
}

// ---------------------------------------------------------------------
// header trace gate (pure predicates) + message-string contract
// ---------------------------------------------------------------------

#[test]
fn parse_bool_env_accepts_truthy_spellings_case_insensitively() {
    // The toggle decision behind ROUTECTL_TRACE_HEADERS and
    // ROUTECTL_LOG_REDACT_PROMPTS, isolated from the process-frozen
    // OnceLock so both arms are testable. All four spellings, any
    // case, with surrounding whitespace, are truthy -- and the two
    // toggles agree because they share this fn.
    for v in [
        "1", "true", "TRUE", "True", "yes", "YES", "on", "ON", "  on  ", "\ttrue\n",
    ] {
        assert!(super::parse_bool_env(v), "{v:?} should parse truthy");
    }
}

#[test]
fn parse_bool_env_rejects_everything_else() {
    // Anything outside the truthy set -- empty, "0", near-misses -- is
    // false, so a typo cannot silently enable raw-header logging.
    for v in ["", "0", "false", "no", "off", "onn", "tru", "enable", "2"] {
        assert!(!super::parse_bool_env(v), "{v:?} should parse falsey");
    }
}

#[test]
fn header_trace_should_emit_requires_toggle_and_trace() {
    // The four trace_*_headers emitters fire ONLY when the operator
    // opted in (toggle) AND the subscriber has TRACE on. Toggle off ->
    // no emission at any level; toggle on -> emission tracks TRACE.
    // Pure fn keeps both arms unit-testable without the frozen OnceLock
    // or a shared tracing subscriber.
    assert!(super::header_trace_should_emit(true, true));
    assert!(!super::header_trace_should_emit(true, false));
    assert!(!super::header_trace_should_emit(false, true));
    assert!(!super::header_trace_should_emit(false, false));
}

#[test]
fn header_trace_message_consts_match_capture_script_needles() {
    // These exact strings are the parsing contract with
    // scripts/capture_fixtures.sh::extract_headers. Changing one here
    // without updating the script's needles would silently break
    // fixture capture, so pin all four.
    assert_eq!(super::HDR_MSG_INGRESS, "ingress request headers");
    assert_eq!(super::HDR_MSG_OUTGOING, "outgoing request headers");
    assert_eq!(super::HDR_MSG_UPSTREAM, "upstream response headers");
    assert_eq!(super::HDR_MSG_EGRESS, "egress response headers");
}

#[test]
fn redact_replaces_bearer_authorization_keeps_scheme_prefix() {
    // A live access-token JWT in the `authorization` header must collapse
    // to "Bearer [REDACTED]" so journald / log archives never carry the
    // token (it embeds account_id, email, session_id, jti, plan_type).
    let mut headers = super::headers_to_json([(
        "authorization",
        b"Bearer test-bearer-token-not-real".as_slice(),
    )]);
    super::redact_header_values(&mut headers);
    let pair = &headers.as_array().unwrap()[0].as_array().unwrap();
    assert_eq!(pair[0].as_str(), Some("authorization"));
    assert_eq!(pair[1].as_str(), Some("Bearer [REDACTED]"));
}

#[test]
fn redact_handles_mixed_case_authorization_header_name() {
    // `reqwest::HeaderMap::iter` lowercases names but operator-supplied
    // header_extras and other code paths may pass `Authorization` /
    // `AUTHORIZATION`. Match case-insensitively.
    for name in ["Authorization", "AUTHORIZATION", "authorization"] {
        let mut headers = super::headers_to_json([(name, b"Bearer XYZ".as_slice())]);
        super::redact_header_values(&mut headers);
        let pair = &headers.as_array().unwrap()[0].as_array().unwrap();
        assert_eq!(pair[1].as_str(), Some("Bearer [REDACTED]"));
    }
}

#[test]
fn redact_replaces_bare_x_api_key_with_redacted() {
    // Anthropic-API api keys ride on `x-api-key` rather than the
    // Bearer scheme. Replace with the bare `[REDACTED]` (no scheme to
    // preserve).
    let mut headers = super::headers_to_json([("x-api-key", b"test-api-key-not-real".as_slice())]);
    super::redact_header_values(&mut headers);
    let pair = &headers.as_array().unwrap()[0].as_array().unwrap();
    assert_eq!(pair[0].as_str(), Some("x-api-key"));
    assert_eq!(pair[1].as_str(), Some("[REDACTED]"));
}

#[test]
fn redact_replaces_bare_x_goog_api_key_with_redacted() {
    // Google Gemini api keys ride on `x-goog-api-key`. Like `x-api-key`,
    // there is no Bearer scheme to preserve -- collapse to `[REDACTED]`
    // so an enabled header trace never carries the live Gemini key.
    let mut headers =
        super::headers_to_json([("x-goog-api-key", b"gemini-key-not-real".as_slice())]);
    super::redact_header_values(&mut headers);
    let pair = &headers.as_array().unwrap()[0].as_array().unwrap();
    assert_eq!(pair[0].as_str(), Some("x-goog-api-key"));
    assert_eq!(pair[1].as_str(), Some("[REDACTED]"));
}

#[test]
fn redact_replaces_mitm_seam_nonce_header_with_redacted() {
    // The MITM front-proxy's seam header carries the per-process
    // unguessable nonce (routectl_cli::ingress::MitmSeamNonce) that makes
    // the seam unspoofable -- an enabled ingress header trace must never
    // print it verbatim, or a trace log becomes a way to learn the value.
    let mut headers = super::headers_to_json([(
        "x-routectl-mitm-proxied",
        b"not-the-real-nonce-value".as_slice(),
    )]);
    super::redact_header_values(&mut headers);
    let pair = &headers.as_array().unwrap()[0].as_array().unwrap();
    assert_eq!(pair[0].as_str(), Some("x-routectl-mitm-proxied"));
    assert_eq!(pair[1].as_str(), Some("[REDACTED]"));
}

#[test]
fn redact_preserves_non_secret_headers_verbatim() {
    // Only secret-bearing names are redacted; anthropic-version /
    // anthropic-beta / originator must round-trip unchanged so the
    // fixture-capture pipeline (and the operator triage flow) still
    // sees the real wire values.
    let mut headers = super::headers_to_json([
        ("authorization", b"Bearer secret-jwt".as_slice()),
        ("anthropic-version", b"2023-06-01".as_slice()),
        (
            "anthropic-beta",
            b"context-management-2026-05-29".as_slice(),
        ),
        ("originator", b"codex_cli_rs".as_slice()),
    ]);
    super::redact_header_values(&mut headers);
    let arr = headers.as_array().unwrap();
    assert_eq!(arr[0][1].as_str(), Some("Bearer [REDACTED]"));
    assert_eq!(arr[1][1].as_str(), Some("2023-06-01"));
    assert_eq!(arr[2][1].as_str(), Some("context-management-2026-05-29"));
    assert_eq!(arr[3][1].as_str(), Some("codex_cli_rs"));
}

#[test]
fn redact_handles_authorization_value_without_bearer_prefix() {
    // A non-Bearer `authorization` (e.g. a raw token from a
    // misconfigured upstream) collapses to bare `[REDACTED]` -- we do
    // not want to leak the scheme guess back to the operator.
    let mut headers = super::headers_to_json([("authorization", b"Basic dXNlcjpwYXNz".as_slice())]);
    super::redact_header_values(&mut headers);
    let pair = &headers.as_array().unwrap()[0].as_array().unwrap();
    assert_eq!(pair[1].as_str(), Some("[REDACTED]"));
}

#[test]
fn redact_proxy_authorization_header() {
    // Same redaction surface as `authorization` for proxy-tunneled
    // upstream connections.
    let mut headers =
        super::headers_to_json([("proxy-authorization", b"Bearer proxy-jwt".as_slice())]);
    super::redact_header_values(&mut headers);
    let pair = &headers.as_array().unwrap()[0].as_array().unwrap();
    assert_eq!(pair[1].as_str(), Some("Bearer [REDACTED]"));
}

#[test]
fn redact_is_idempotent_on_already_redacted_value() {
    // A second pass over an already-redacted vector must be a no-op
    // (same value -- still secret-shaped, still matches the rule).
    let mut headers = super::headers_to_json([("authorization", b"Bearer [REDACTED]".as_slice())]);
    super::redact_header_values(&mut headers);
    super::redact_header_values(&mut headers);
    let pair = &headers.as_array().unwrap()[0].as_array().unwrap();
    assert_eq!(pair[1].as_str(), Some("Bearer [REDACTED]"));
}

#[test]
fn redact_x_amz_security_token_mixed_case_redacted() {
    // The SigV4 STS session credential rides on `x-amz-security-token`.
    // Any `x-amz-` header that is NOT signing metadata must redact, and
    // the name match is case-insensitive so a mixed-case header from a
    // signing library still collapses.
    for name in [
        "x-amz-security-token",
        "X-Amz-Security-Token",
        "X-AMZ-SECURITY-TOKEN",
    ] {
        let mut headers = super::headers_to_json([(name, b"FwoGZXIvYXdzEXAMPLE".as_slice())]);
        super::redact_header_values(&mut headers);
        let pair = &headers.as_array().unwrap()[0].as_array().unwrap();
        assert_eq!(
            pair[1].as_str(),
            Some("[REDACTED]"),
            "{name} should redact to bare [REDACTED]"
        );
    }
}

#[test]
fn redact_x_amz_signing_metadata_survives_verbatim() {
    // `x-amz-date` and `x-amz-content-sha256` are non-secret signing
    // metadata an operator needs to triage a SigV4 request; they must
    // survive the `x-amz-` prefix redaction rule verbatim.
    let mut headers = super::headers_to_json([
        ("x-amz-date", b"20260616T000000Z".as_slice()),
        (
            "X-Amz-Content-Sha256",
            b"e3b0c44298fc1c149afbf4c8996fb924".as_slice(),
        ),
        ("x-amz-security-token", b"STSTOKEN".as_slice()),
    ]);
    super::redact_header_values(&mut headers);
    let arr = headers.as_array().unwrap();
    // Assert by header NAME (case-insensitive), not positional index, so
    // the test does not silently pass if header ordering changes.
    let value_for = |name: &str| -> Option<String> {
        arr.iter().find_map(|entry| {
            let pair = entry.as_array()?;
            let n = pair.first()?.as_str()?;
            if n.eq_ignore_ascii_case(name) {
                pair.get(1)?.as_str().map(str::to_string)
            } else {
                None
            }
        })
    };
    assert_eq!(value_for("x-amz-date").as_deref(), Some("20260616T000000Z"));
    assert_eq!(
        value_for("x-amz-content-sha256").as_deref(),
        Some("e3b0c44298fc1c149afbf4c8996fb924")
    );
    // The session credential alongside the metadata still redacts.
    assert_eq!(
        value_for("x-amz-security-token").as_deref(),
        Some("[REDACTED]")
    );
}

#[test]
fn redact_cookie_and_set_cookie_redacted() {
    // Session credentials ride on `cookie` (request echo) and
    // `set-cookie` (response set). Both must redact to the bare marker,
    // case-insensitively, in either direction.
    let mut headers = super::headers_to_json([
        ("set-cookie", b"session=abc123; HttpOnly".as_slice()),
        ("Cookie", b"session=abc123".as_slice()),
    ]);
    super::redact_header_values(&mut headers);
    let arr = headers.as_array().unwrap();
    assert_eq!(arr[0][0].as_str(), Some("set-cookie"));
    assert_eq!(arr[0][1].as_str(), Some("[REDACTED]"));
    assert_eq!(arr[1][0].as_str(), Some("Cookie"));
    assert_eq!(arr[1][1].as_str(), Some("[REDACTED]"));
}

#[test]
fn redact_header_value_no_panic_when_byte_7_is_continuation_byte() {
    // Six ASCII digits followed by a 2-byte char places a UTF-8
    // continuation byte at index 7, so a naive `value[..7]` slice would
    // panic on a non-char-boundary. The scheme prefix must be compared
    // over bytes so a crafted header value cannot abort the trace poll.
    let value = "123456\u{00e9}";
    assert_eq!(super::redact_header_value(value), "[REDACTED]");
}

#[test]
fn redact_header_value_no_panic_when_lossy_replacement_char_straddles_7() {
    // Header values arrive via `String::from_utf8_lossy`, which turns any
    // non-UTF-8 byte into U+FFFD (3 bytes). Five ASCII bytes then a U+FFFD
    // put that 3-byte char across index 7 (bytes 5..8), so byte 7 is a
    // continuation byte; the byte-compare must not panic.
    let value = "12345\u{FFFD}x";
    assert_eq!(super::redact_header_value(value), "[REDACTED]");
}

#[test]
fn redact_header_value_bearer_still_redacts_to_scheme_marker() {
    assert_eq!(
        super::redact_header_value("Bearer abc.def"),
        "Bearer [REDACTED]"
    );
}

#[test]
fn redact_header_value_basic_scheme_collapses_to_bare_marker() {
    // The `Basic` scheme is not `Bearer `; it must not keep a scheme
    // prefix and collapses to the bare marker.
    assert_eq!(
        super::redact_header_value("Basic dXNlcjpwYXNz"),
        "[REDACTED]"
    );
}

// ---------------------------------------------------------------------
// sanitize_capped / debug body control-char stripping
// ---------------------------------------------------------------------

#[test]
fn sanitize_capped_strips_control_chars_at_debug_body_cap() {
    // The debug-body path (debug_upstream_error_body) pipes the output
    // of sanitize_upstream_body_with_cap through sanitize_capped so a
    // malicious/compromised upstream cannot forge fake log lines at
    // DEBUG via embedded CR/LF/ANSI escapes (up to 4 KB injection).
    //
    // This test pins three requirements in one pass:
    //   1. CR, LF, ESC are stripped (no log-injection possible).
    //   2. Output length EXCEEDS 256 chars (the 4 KB cap is honored, not
    //      the 256-char sanitize_for_log cap -- proves no silent cap
    //      regression from the refactor).
    //   3. sanitize_for_log still caps at MAX (regression guard for the
    //      sanitize_capped extraction refactor).
    let input = format!("{}\r\n\x1b[31m{}", "A".repeat(300), "B".repeat(50));

    let got = sanitize_capped(&input, MAX_DEBUG_BODY_BYTES);

    // Requirement 1: no control chars survive.
    assert!(!got.contains('\r'), "CR should be stripped from debug body");
    assert!(!got.contains('\n'), "LF should be stripped from debug body");
    assert!(
        !got.contains('\x1b'),
        "ESC should be stripped from debug body"
    );

    // Requirement 2: length exceeds the 256-char sanitize_for_log cap,
    // confirming the 4 KB debug cap is in effect.
    assert!(
        got.chars().count() > 256,
        "sanitize_capped at MAX_DEBUG_BODY_BYTES must NOT cap at 256; got len {}",
        got.chars().count()
    );

    // Requirement 3: sanitize_for_log still caps at MAX (256).
    let short = sanitize_for_log(&input);
    assert_eq!(
        short.chars().count(),
        MAX,
        "sanitize_for_log must still cap at MAX after refactor"
    );
}

#[test]
fn redact_bedrock_inline_source_bytes_redacted() {
    // Bedrock inline image/document blocks carry the base64 payload
    // under `source.bytes`. With redaction on, the long base64 string
    // must collapse to the `<redacted len=N>` marker so it does not
    // leak into the trace log.
    let payload = "QUFB".repeat(100); // long base64-ish string
    let body = json!({
        "messages": [{
            "role": "user",
            "content": [{
                "type": "image",
                "source": {"bytes": payload.clone()}
            }]
        }]
    });
    let got = redact_prompts_with_flag(&body, true);
    let redacted = got["messages"][0]["content"][0]["source"]["bytes"]
        .as_str()
        .expect("source.bytes stays a string");
    assert!(
        redacted.starts_with("<redacted len="),
        "source.bytes should be redacted, got {redacted}"
    );
    assert!(
        !redacted.contains("QUFB"),
        "raw base64 must not survive in source.bytes"
    );
}

#[test]
fn redact_numeric_bytes_outside_source_stays_visible() {
    // A `bytes` field that is NOT a string under a `source` object --
    // e.g. a numeric byte counter -- must stay visible. Proves the
    // redaction is parent-gated, not a blind any-key `bytes` match.
    //
    // The numeric and short-string leaves below are weak negatives on
    // their own: a blind any-key `bytes` match would ALSO spare them
    // (numbers are not redacted, and short strings fall under the
    // 256-char long-string threshold). The LONG-string leaves are the
    // load-bearing case -- a blind any-key match WOULD wrongly redact a
    // 300-char `bytes` string, so their survival genuinely proves the
    // redaction is gated on the `source` parent.
    let long_blob = "Z".repeat(300);
    let body = json!({
        "stats": {"bytes": 1234},
        "meta": {"bytes": "short"},
        // Long string `bytes` leaf NOT under a `source` object.
        "blob": {"bytes": long_blob.clone()},
        // Long string `bytes` leaf at the top level (no parent object key).
        "bytes": long_blob.clone(),
    });
    let got = redact_prompts_with_flag(&body, true);
    assert_eq!(
        got["stats"]["bytes"], 1234,
        "numeric bytes outside source must stay visible"
    );
    assert_eq!(
        got["meta"]["bytes"], "short",
        "short string bytes outside source must stay visible"
    );
    assert_eq!(
        got["blob"]["bytes"], long_blob,
        "long string bytes under non-source parent must stay visible"
    );
    assert_eq!(
        got["bytes"], long_blob,
        "long string bytes at the top level must stay visible"
    );
}

#[test]
fn redact_openai_responses_annotations_collapsed() {
    // POSITIVE: an OpenAI Responses `output_text` block carries
    // `annotations[*]` url_citation entries whose `title`, source
    // `url`, and quoted `text` echo the cited document. None redact
    // under the per-key sweep (no `title` arm; plain https url skips
    // the data-URI-only `url` arm), so the whole `annotations` value
    // must collapse to the opaque marker -- taking title, url, AND
    // quoted text with it.
    let body = json!({
        "type": "output_text",
        "text": "hello",
        "annotations": [{
            "type": "url_citation",
            "title": "SENTINEL_TITLE",
            "url": "https://SENTINEL_URL",
            "text": "SENTINEL_QUOTE",
        }],
    });
    let got = redact_prompts_with_flag(&body, true);
    let serialized = serde_json::to_string(&got).expect("serialize redacted body");
    // Structural assert: the whole annotations value collapses.
    assert_eq!(got["annotations"], json!({"redacted": true}));
    // Sensitive substrings (title, source url, quoted text) must all be
    // gone from the serialized body.
    assert!(
        !serialized.contains("SENTINEL_TITLE"),
        "annotation title survived redaction: {serialized}"
    );
    assert!(
        !serialized.contains("SENTINEL_URL"),
        "annotation source url survived redaction: {serialized}"
    );
    assert!(
        !serialized.contains("SENTINEL_QUOTE"),
        "annotation quoted text survived redaction: {serialized}"
    );
}

#[test]
fn redact_annotations_preserves_sibling_structure() {
    // STRUCTURE-PRESERVATION: in the same block, the sibling
    // structural `type:"output_text"` stays visible and the block-level
    // `text` redacts via its own arm to `<redacted len=N>`.
    let body = json!({
        "type": "output_text",
        "text": "hello",
        "annotations": [{
            "type": "url_citation",
            "title": "SENTINEL_TITLE",
            "url": "https://example.com/source",
            "text": "SENTINEL_QUOTE",
        }],
    });
    let got = redact_prompts_with_flag(&body, true);
    assert_eq!(got["type"], "output_text");
    assert_eq!(got["text"], "<redacted len=5>");
}

#[test]
fn redact_text_block_still_collapses_to_exact_marker() {
    // REGRESSION CANARY: the annotations arm must not perturb a plain
    // top-level `{type:"text", text}` block -- it still redacts to
    // exactly `<redacted len=5>`.
    let body = json!({"type": "text", "text": "hello"});
    let got = redact_prompts_with_flag(&body, true);
    assert_eq!(got["text"], "<redacted len=5>");
}

#[test]
fn redact_audio_transcript_redacted() {
    // POSITIVE: an audio block's `transcript` leaf carries the spoken
    // content transcription -- user content by nature -- so it redacts
    // via the always-redact text-leaf arm regardless of nesting under
    // `input_audio`.
    let body = json!({
        "type": "input_audio",
        "input_audio": {"transcript": "SENTINEL_TRANSCRIPT", "format": "wav"},
    });
    let got = redact_prompts_with_flag(&body, true);
    assert_eq!(
        got["input_audio"]["transcript"], "<redacted len=19>",
        "audio transcript must be redacted as content"
    );
    // The non-content sibling `format` stays visible.
    assert_eq!(got["input_audio"]["format"], "wav");
}

#[test]
fn extract_upstream_message_returns_error_message_verbatim_from_json_envelope() {
    // The primary branch: a standard `{"error":{"message":...}}` upstream
    // body yields the message string verbatim -- operators reading
    // `body_excerpt=` see the clean upstream error, not the JSON envelope.
    let body = r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error","code":"invalid_api_key"}}"#;
    assert_eq!(extract_upstream_message(body), "Incorrect API key provided");
}

#[test]
fn extract_upstream_message_falls_back_to_sanitized_body_when_no_error_message() {
    // JSON that parses but carries no `/error/message` pointer must not
    // silently return an empty or sibling value -- it falls back to the
    // sanitized excerpt of the whole body.
    let body = r#"{"detail":"tenant-7 trace","status":503}"#;
    assert_eq!(extract_upstream_message(body), sanitize_upstream_body(body));
}

#[test]
fn extract_upstream_message_falls_back_when_error_message_is_not_a_string() {
    // `error.message` present but non-string (a number here) fails the
    // `as_str` guard, so the helper falls back to the sanitized body
    // rather than stringifying the non-string node.
    let body = r#"{"error":{"message":123}}"#;
    assert_eq!(extract_upstream_message(body), sanitize_upstream_body(body));
}
