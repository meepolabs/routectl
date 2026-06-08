//! End-to-end pin for the per-egress-allowlist contract: a foreign
//! upstream (openai-compat with the DeepSeek dialect) flowing through
//! the canonical normalize seam and the Anthropic ingress render must
//! NOT leak openai-compat envelope keys, vendor usage sub-bags, or a
//! `signature: null` thinking key into the Anthropic-shape body served
//! back to a claude-code style caller.
//!
//! See docs/WIRE-GOTCHAS.md for background on the signature seam this
//! test pins -- Anthropic 400s on a `signature: null` thinking block
//! mid-conversation when a provider switch lands on a non-signing
//! upstream.

use serde_json::{json, Value};

use routectl_cli::ingress::anthropic::AnthropicIngress;
use routectl_cli::ingress::IngressAdapter;
use routectl_providers::openai_compat::dialect::ReasoningDialect;
use routectl_providers::openai_compat::response::{
    normalize, OPENAI_COMPAT_ENVELOPE_KEYS, OPENAI_COMPAT_USAGE_SUBKEYS,
};

/// Build a raw openai-compat response (DeepSeek shape) carrying every
/// field on the openai-compat strip allow-lists
/// (`OPENAI_COMPAT_ENVELOPE_KEYS`, `OPENAI_COMPAT_USAGE_SUBKEYS`) plus
/// a DeepSeek-style unsigned `reasoning_content` so the render path
/// also exercises the no-null-signature seam.
fn deepseek_raw_with_envelope_and_thinking() -> Value {
    json!({
        "id": "chatcmpl-cross-dialect",
        "model": "deepseek-v4-pro",
        "created": 1_700_000_000_i64,
        // openai-compat envelope tells.
        "object": "chat.completion",
        "system_fingerprint": "fp_test",
        "cost": "0",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "the answer is 42",
                // DeepSeek dialect carries reasoning under reasoning_content;
                // the dialect lift hoists it into ReasoningDetail with no
                // signature (deepseek-v4-pro does not sign).
                "reasoning_content": "thinking trace"
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "total_tokens": 120,
            // DeepSeek vendor cache stat -- must lift to canonical
            // cache_read_input_tokens.
            "prompt_cache_hit_tokens": 80,
            "prompt_cache_miss_tokens": 20,
            // OpenAI vendor sub-bags -- must be stripped after lift.
            "prompt_tokens_details": {"cached_tokens": 64},
            "completion_tokens_details": {"reasoning_tokens": 7}
        }
    })
}

/// Drive the full pipeline (openai-compat normalize -> Anthropic
/// ingress render) and assert the Anthropic-shape body served back is
/// clean.
#[test]
fn openai_compat_to_anthropic_render_strips_vendor_keys_and_omits_null_signature() {
    // Arrange
    let raw = deepseek_raw_with_envelope_and_thinking();

    // Act: openai-compat egress normalize -> canonical ChatResponse.
    let canonical = normalize("p-deepseek", raw, ReasoningDialect::DeepSeek)
        .expect("normalize must succeed on a well-formed openai-compat body");

    // Sanity: the dialect lifted reasoning_content into a
    // ReasoningDetail without a signature, so the Anthropic render
    // exercises the no-null-signature path.
    let details = &canonical
        .choices
        .first()
        .expect("choice present")
        .message
        .reasoning_details;
    assert_eq!(details.len(), 1, "DeepSeek dialect must lift one detail");
    assert!(
        details[0].payload.get("signature").is_none(),
        "DeepSeek lift must not synthesize a signature; got payload: {:?}",
        details[0].payload
    );

    // Anthropic ingress render -> wire body served back to caller.
    let body = AnthropicIngress
        .render_response(canonical)
        .expect("render_response must succeed");

    // Assert (1): top-level envelope keys do not leak. Iterate the
    // shared allow-list so a future addition to
    // `OPENAI_COMPAT_ENVELOPE_KEYS` is automatically pinned by this
    // test instead of needing a mirrored literal kept in sync.
    let obj = body.as_object().expect("rendered body is a JSON object");
    for k in OPENAI_COMPAT_ENVELOPE_KEYS {
        assert!(
            !obj.contains_key(*k),
            "openai-compat envelope key {k} must not appear at top level; got body: {body}"
        );
    }

    // Assert (2): usage sub-bags do not leak; canonical lift surfaces
    // the values via Anthropic-shape fields. Same allow-list pivot:
    // iterate `OPENAI_COMPAT_USAGE_SUBKEYS` rather than a copy.
    let usage = body.get("usage").expect("usage rendered");
    let usage_obj = usage.as_object().expect("usage is a JSON object");
    for k in OPENAI_COMPAT_USAGE_SUBKEYS {
        assert!(
            !usage_obj.contains_key(*k),
            "openai-compat usage sub-bag {k} must not appear in rendered usage; got: {usage}"
        );
    }

    // Assert (3): cache_read_input_tokens lifted to the Anthropic-shape
    // canonical surface.
    assert_eq!(
        usage_obj.get("cache_read_input_tokens"),
        Some(&json!(80)),
        "cache_read_input_tokens must be lifted from prompt_cache_hit_tokens; got: {usage}"
    );

    // Assert (4): the rendered thinking block carries NO `signature`
    // key when the source detail had no signature. Real Anthropic
    // 400s on signature:null mid-conversation provider switch.
    let content = body
        .get("content")
        .and_then(|v| v.as_array())
        .expect("content is array");
    let thinking = content
        .iter()
        .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("thinking"))
        .expect("thinking block surfaced from DeepSeek reasoning_content");
    let thinking_obj = thinking.as_object().expect("thinking block is an object");
    assert!(
        !thinking_obj.contains_key("signature"),
        "thinking block must not carry signature: null when source has no signature; got: {thinking}"
    );
    assert_eq!(thinking["thinking"], "thinking trace");
}

/// Drive a canonical ChatResponse that has a tool_use block in BOTH
/// `tool_calls` and `ContentPart::ToolUse` through the OpenAI ingress
/// render path and assert:
/// (a) `tool_calls` is populated with exactly one entry, and
/// (b) the `content` field contains NO `tool_use` blocks (the
///     `strip_tool_use_parts_when_tool_calls_present` dedup fires).
///
/// The text part survives the strip and collapses to a string.
#[test]
fn openai_render_dedupes_tool_use_when_present_in_both_tool_calls_and_parts() {
    use routectl_cli::ingress::openai::OpenAiIngress;
    use routectl_core::{
        ChatResponse, Choice, ContentPart, KnownContentPart, Message, MessageContent, Role, Usage,
    };

    let resp = ChatResponse {
        id: "chatcmpl-dup".into(),
        model: "claude-haiku-4-5".into(),
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::Text {
                        text: "I will call the tool.".into(),
                        cache_control: None,
                    }),
                    ContentPart::Known(KnownContentPart::ToolUse {
                        id: "call_dedup".into(),
                        name: "calculator".into(),
                        input: json!({"a": 1, "b": 2}),
                        cache_control: None,
                    }),
                ]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_dedup",
                    "type": "function",
                    "function": {"name": "calculator", "arguments": "{\"a\":1,\"b\":2}"}
                })]),
            },
            finish_reason: Some("tool_calls".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 10,
            total_tokens: 15,
            ..Default::default()
        }),
        routectl_provider: None,
        extras: Default::default(),
    };

    let v = OpenAiIngress
        .render_response(resp)
        .expect("render_response must succeed");

    // Assert (1): tool_calls populated with exactly one entry.
    let tool_calls = v["choices"][0]["message"]["tool_calls"]
        .as_array()
        .expect("tool_calls must be an array");
    assert_eq!(
        tool_calls.len(),
        1,
        "expected exactly one tool call; got {tool_calls:?}"
    );
    assert_eq!(tool_calls[0]["id"], "call_dedup");

    // Assert (2): content must NOT contain any tool_use blocks after dedup.
    // After strip, the text-only parts collapse to a string; the content
    // field will be a string, not an array with tool_use blocks.
    let content = &v["choices"][0]["message"]["content"];
    if let Some(arr) = content.as_array() {
        let tool_use_blocks: Vec<_> = arr
            .iter()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
            .collect();
        assert!(
            tool_use_blocks.is_empty(),
            "content must not contain tool_use blocks after dedup; got: {arr:?}"
        );
    }
    // The text part survives the strip and collapses to a string.
    if let Some(s) = content.as_str() {
        assert!(
            s.contains("I will call the tool"),
            "collapsed text content must be preserved; got: {s}"
        );
    }
}
