//! Host-gated reasoning-envelope unwrap on the `redacted_thinking`
//! egress, exercised END TO END through `Provider::normalize_request` so
//! the gate's resolution AT THE CLIENT BOUNDARY (from `cfg.base_url`) is
//! part of what is under test -- a translator-level test would pin the
//! policy while leaving the wiring that selects it unverified.
//!
//! Declared on `mod.rs` via `#[cfg(test)] #[path = ...]`.

use super::*;
use routectl_core::{
    ChatRequest, ContentPart, KnownContentPart, Message, MessageContent, ReasoningDetail,
    ReasoningDetailKind, Role, reasoning_envelope,
};
use routectl_testkit::capture_events;
use serde_json::json;

/// A foreign (Responses-family) reasoning artifact, i.e. the only kind an
/// envelope is ever wrapped around.
const INNER_BLOB: &str = "rsn_abc123-payload";

fn wrapped_envelope() -> String {
    reasoning_envelope::wrap(
        routectl_core::OPENAI_RESPONSES_V1,
        Some("rs_42"),
        INNER_BLOB,
    )
}

fn redacted_part(data: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::RedactedThinking {
        data: data.to_string(),
    })
}

/// An assistant turn carrying the given redacted blocks plus a text block,
/// so the turn is never emptied and always translates through the Parts
/// path.
fn history_with(parts: Vec<ContentPart>) -> ChatRequest {
    let mut all = parts;
    all.push(ContentPart::Known(KnownContentPart::Text {
        text: "answer".into(),
        citations: None,
        cache_control: None,
    }));
    ChatRequest {
        model: "claude-sonnet-4-5".into(),
        messages: vec![
            Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(all),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ]
        .into(),
        max_tokens: Some(1024),
        ..Default::default()
    }
}

fn provider_at(base_url: &str) -> AnthropicApiProvider {
    let mut cfg = AnthropicApiConfig::new("anthropic:test", "test-key");
    cfg.base_url = base_url.to_string();
    AnthropicApiProvider::new(cfg)
}

/// An `Encrypted` reasoning detail tagged with the Anthropic format,
/// carrying `data` verbatim. The format tag is CLIENT-SUPPLIED on the
/// request schema, so this is a shape any caller can send -- the
/// emittability gate checks the tag only and is not an envelope barrier.
fn encrypted_detail(data: &str) -> ReasoningDetail {
    ReasoningDetail {
        kind: ReasoningDetailKind::Encrypted,
        id: None,
        format: Some(ANTHROPIC_FORMAT.to_string()),
        index: Some(0),
        payload: json!({"data": data}),
    }
}

/// `history_with`'s request with `details` attached to its assistant turn,
/// so the `reasoning_details` replay channel is what emits the
/// `redacted_thinking` block.
fn history_with_details(parts: Vec<ContentPart>, details: Vec<ReasoningDetail>) -> ChatRequest {
    let base = history_with(parts);
    let mut msgs: Vec<Message> = base.messages.iter().cloned().collect();
    let last = msgs.len() - 1;
    msgs[last].reasoning_details = details;
    ChatRequest {
        messages: msgs.into(),
        ..base
    }
}

/// A provider with context-management emulation on, whose thinking cache
/// is seeded with one `Encrypted` detail carrying `data` for `tool_use_id`.
/// The reinjection path pulls from the cache, so this is how a wrapped
/// envelope reaches the wire through the cache rather than through the
/// request body.
fn provider_with_cached_thinking(
    base_url: &str,
    tool_use_id: &str,
    data: &str,
) -> AnthropicApiProvider {
    let mut cfg = AnthropicApiConfig::new("anthropic:test", "test-key");
    cfg.base_url = base_url.to_string();
    cfg.context_management = true;
    let provider = AnthropicApiProvider::new(cfg);
    context_management::snapshot_to_cache(
        &provider.thinking_cache,
        "anthropic:test",
        tool_use_id,
        vec![encrypted_detail(data)],
        context_management::DEFAULT_MAX_THINKING_ENTRY_BYTES,
        context_management::THINKING_CACHE_TTL,
        "test-seed",
    );
    provider
}

/// A tool-use history the context-management reinjection path acts on:
/// one assistant turn with a `tool_calls` entry (so the wire carries a
/// ToolUse block whose preceding block is Text, not thinking) plus the
/// `clear_thinking` edit that arms the reinjection.
fn tool_use_history_with_clear_thinking_edit(tool_use_id: &str) -> ChatRequest {
    ChatRequest {
        model: "claude-sonnet-4-5".into(),
        max_tokens: Some(1024),
        messages: vec![
            Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("use the tool".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("calling the tool".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": tool_use_id,
                    "type": "function",
                    "function": {"name": "calc", "arguments": "{}"}
                })]),
            },
            Message {
                refusal: None,
                role: Role::Tool,
                content: MessageContent::Text("42".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: Some(tool_use_id.to_string()),
                tool_calls: None,
            },
        ]
        .into(),
        provider_extras: Some(json!({
            "context_management": {
                "edits": [{
                    "type": context_management::CLEAR_THINKING_EDIT_TYPE,
                    "keep": "all"
                }]
            }
        })),
        ..Default::default()
    }
}

/// Every `redacted_thinking` `data` value in the assembled body, in wire
/// order.
fn redacted_data(body: &Value) -> Vec<String> {
    body["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .filter_map(|m| m["content"].as_array())
        .flatten()
        .filter(|b| b["type"] == "redacted_thinking")
        .map(|b| {
            b["data"]
                .as_str()
                .expect("redacted_thinking data is a string")
                .to_string()
        })
        .collect()
}

/// At the GENUINE Anthropic host a recognized envelope is replaced by its
/// unwrapped inner blob: exactly the bytes the caller would have sent with
/// no routectl in the path.
#[test]
fn envelope_unwraps_to_inner_blob_at_the_anthropic_host() {
    // Arrange
    let provider = provider_at("https://api.anthropic.com");
    let req = history_with(vec![redacted_part(&wrapped_envelope())]);

    // Act
    let body = provider.normalize_request(&req).expect("normalize");

    // Assert
    assert_eq!(
        redacted_data(&body),
        vec![INNER_BLOB.to_string()],
        "the wire data must be the inner blob, byte-exact: {body}"
    );
}

/// The routectl-to-routectl continuity guard. A non-Anthropic target (a
/// routectl hop, or any Anthropic-compatible third-party host) MUST see
/// the envelope byte-for-byte -- the next hop needs it to recover the
/// artifact's scheme and id, so an unwrap here breaks a working path.
#[test]
fn envelope_is_byte_identical_at_a_non_anthropic_target() {
    // Arrange
    let envelope = wrapped_envelope();
    let provider = provider_at("https://router.internal.example/anthropic");
    let req = history_with(vec![redacted_part(&envelope)]);

    // Act
    let body = provider.normalize_request(&req).expect("normalize");

    // Assert
    assert_eq!(
        redacted_data(&body),
        vec![envelope],
        "a non-Anthropic target must keep the envelope verbatim: {body}"
    );
}

/// A malformed or unknown-version string is not an envelope, so it rides
/// through verbatim even at the terminal host: no partial parse, no guess
/// at what the bytes might have been.
#[test]
fn unrecognized_envelope_stays_verbatim_at_the_anthropic_host() {
    // Arrange -- an unknown version prefix, a truncated envelope, an
    // envelope with an empty blob, and a bare provider-native blob.
    let candidates = vec![
        format!(
            "rctl9.{}.rs_42.{INNER_BLOB}",
            routectl_core::OPENAI_RESPONSES_V1
        ),
        format!("rctl1.{}.rs_42", routectl_core::OPENAI_RESPONSES_V1),
        format!("rctl1.{}.rs_42.", routectl_core::OPENAI_RESPONSES_V1),
        INNER_BLOB.to_string(),
    ];
    let provider = provider_at("https://api.anthropic.com");

    for candidate in candidates {
        let req = history_with(vec![redacted_part(&candidate)]);

        // Act
        let body = provider.normalize_request(&req).expect("normalize");

        // Assert
        assert_eq!(
            redacted_data(&body),
            vec![candidate.clone()],
            "an unrecognized envelope must stay verbatim: {candidate}"
        );
    }
}

/// A history carrying TWO wrapped blocks emits at most ONE WARN -- a
/// history can hold unbounded reasoning blocks, so a per-block WARN would
/// be a log amplifier driven by request content. The line carries only a
/// constant event name, the provider id, and a count: never the blob, the
/// claimed artifact id, the claimed scheme, or any digest of those.
#[test]
fn two_wrapped_blocks_emit_one_warn_carrying_no_envelope_content() {
    // Arrange
    let envelope = wrapped_envelope();
    let second = reasoning_envelope::wrap(routectl_core::CODEX_OAUTH, None, "smry_second-payload");
    let provider = provider_at("https://api.anthropic.com");
    let req = history_with(vec![redacted_part(&envelope), redacted_part(&second)]);

    // Act
    let mut body = None;
    let events = capture_events(|| {
        body = Some(provider.normalize_request(&req).expect("normalize"));
    });
    let body = body.expect("normalize ran");

    // Assert -- both blocks were unwrapped.
    assert_eq!(
        redacted_data(&body),
        vec![INNER_BLOB.to_string(), "smry_second-payload".to_string()],
        "both envelopes must unwrap: {body}"
    );

    // Assert -- exactly one WARN, with the expected field set.
    let warns: Vec<_> = events
        .iter()
        .filter(|e| e.field("event") == Some("reasoning_envelope_unwrapped"))
        .collect();
    assert_eq!(
        warns.len(),
        1,
        "two unwrapped blocks must emit exactly one WARN; got: {events:?}"
    );
    let warn = warns[0];
    assert_eq!(warn.level, tracing::Level::WARN);
    assert_eq!(warn.field("provider"), Some("anthropic:test"));
    assert_eq!(warn.field("unwrapped_count"), Some("2"));

    // Assert -- no envelope-derived value reaches ANY captured event at
    // any level, neither whole nor as a fragment.
    let rendered = format!("{events:?}");
    for forbidden in [
        INNER_BLOB,
        "smry_second-payload",
        "rs_42",
        routectl_core::OPENAI_RESPONSES_V1,
        routectl_core::CODEX_OAUTH,
        "rctl1",
    ] {
        assert!(
            !rendered.contains(forbidden),
            "log output must not carry `{forbidden}`: {rendered}"
        );
    }
}

/// The `reasoning_details` replay channel reaches the terminal host too.
/// A client tags an `Encrypted` detail `anthropic-claude-v1` and puts a
/// wrapped envelope in its `data`; the emittability gate checks only the
/// tag, so the block is emitted -- and must be emitted as the INNER BLOB.
#[test]
fn reasoning_details_envelope_unwraps_to_inner_blob_at_the_anthropic_host() {
    // Arrange
    let provider = provider_at("https://api.anthropic.com");
    let req = history_with_details(vec![], vec![encrypted_detail(&wrapped_envelope())]);

    // Act
    let body = provider.normalize_request(&req).expect("normalize");

    // Assert
    assert_eq!(
        redacted_data(&body),
        vec![INNER_BLOB.to_string()],
        "the reasoning_details channel must emit the inner blob: {body}"
    );
}

/// Continuity guard for the `reasoning_details` channel: a non-Anthropic
/// target keeps the envelope byte-for-byte, so the next hop can still
/// recover the artifact's claimed scheme and id.
#[test]
fn reasoning_details_envelope_is_byte_identical_at_a_non_anthropic_target() {
    // Arrange
    let envelope = wrapped_envelope();
    let provider = provider_at("https://router.internal.example/anthropic");
    let req = history_with_details(vec![], vec![encrypted_detail(&envelope)]);

    // Act
    let body = provider.normalize_request(&req).expect("normalize");

    // Assert
    assert_eq!(
        redacted_data(&body),
        vec![envelope],
        "a non-Anthropic target must keep the envelope verbatim: {body}"
    );
}

/// The context-management reinjection channel reaches the terminal host:
/// a cached `Encrypted` detail whose `data` holds a wrapped envelope is
/// reinjected before the ToolUse block, and must ship as the inner blob.
#[test]
fn cache_reinjected_envelope_unwraps_to_inner_blob_at_the_anthropic_host() {
    // Arrange
    let provider = provider_with_cached_thinking(
        "https://api.anthropic.com",
        "toolu_cm1",
        &wrapped_envelope(),
    );
    let req = tool_use_history_with_clear_thinking_edit("toolu_cm1");

    // Act
    let body = provider.normalize_request(&req).expect("normalize");

    // Assert
    assert_eq!(
        redacted_data(&body),
        vec![INNER_BLOB.to_string()],
        "the reinjection channel must emit the inner blob: {body}"
    );
}

/// Continuity guard for the reinjection channel: a non-Anthropic target
/// keeps the reinjected envelope byte-for-byte.
#[test]
fn cache_reinjected_envelope_is_byte_identical_at_a_non_anthropic_target() {
    // Arrange
    let envelope = wrapped_envelope();
    let provider = provider_with_cached_thinking(
        "https://router.internal.example/anthropic",
        "toolu_cm1",
        &envelope,
    );
    let req = tool_use_history_with_clear_thinking_edit("toolu_cm1");

    // Act
    let body = provider.normalize_request(&req).expect("normalize");

    // Assert
    assert_eq!(
        redacted_data(&body),
        vec![envelope],
        "a non-Anthropic target must keep the reinjected envelope verbatim: {body}"
    );
}

/// Envelopes arriving through MORE than one channel in a single request
/// still produce exactly ONE WARN, whose count covers every channel. The
/// tally is owned per request rather than per translation pass precisely
/// so a multi-channel history cannot multiply the log line.
#[test]
fn envelopes_on_two_channels_emit_one_warn_with_the_combined_count() {
    // Arrange -- a content-part envelope and a reasoning_details envelope
    // on the same assistant turn.
    let part_envelope = wrapped_envelope();
    let detail_envelope =
        reasoning_envelope::wrap(routectl_core::CODEX_OAUTH, None, "smry_detail-payload");
    let provider = provider_at("https://api.anthropic.com");
    let req = history_with_details(
        vec![redacted_part(&part_envelope)],
        vec![encrypted_detail(&detail_envelope)],
    );

    // Act
    let mut body = None;
    let events = capture_events(|| {
        body = Some(provider.normalize_request(&req).expect("normalize"));
    });
    let body = body.expect("normalize ran");

    // Assert -- both channels unwrapped.
    let data = redacted_data(&body);
    assert!(
        data.contains(&INNER_BLOB.to_string()) && data.contains(&"smry_detail-payload".to_string()),
        "both channels must emit their inner blob: {body}"
    );

    // Assert -- one WARN, counting both.
    let warns: Vec<_> = events
        .iter()
        .filter(|e| e.field("event") == Some("reasoning_envelope_unwrapped"))
        .collect();
    assert_eq!(
        warns.len(),
        1,
        "two channels must still emit exactly one WARN; got: {events:?}"
    );
    assert_eq!(warns[0].field("unwrapped_count"), Some("2"));

    // Assert -- still no envelope-derived value in any captured event.
    let rendered = format!("{events:?}");
    for forbidden in [
        INNER_BLOB,
        "smry_detail-payload",
        "rs_42",
        routectl_core::OPENAI_RESPONSES_V1,
        routectl_core::CODEX_OAUTH,
        "rctl1",
    ] {
        assert!(
            !rendered.contains(forbidden),
            "log output must not carry `{forbidden}`: {rendered}"
        );
    }
}
