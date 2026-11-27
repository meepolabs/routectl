//! Host-gated handling of a routectl reasoning envelope on the
//! `redacted_thinking` egress, exercised END TO END through
//! `Provider::normalize_request` so the gate's resolution AT THE CLIENT
//! BOUNDARY (from `cfg.base_url`) is part of what is under test.
//!
//! At the genuine Anthropic host a recognized envelope wraps an artifact
//! another lane minted, which that host rejects, so the whole block is
//! omitted. Every other target keeps the envelope byte for byte, and a
//! string that is not a recognized envelope ships unchanged everywhere.
//!
//! Declared on `mod.rs` via `#[cfg(test)] #[path = ...]`.

use super::*;
use routectl_core::{
    ChatRequest, ContentPart, CoreHistoryReasoning, KnownContentPart, Message, MessageContent,
    ReasoningDetail, ReasoningDetailKind, Role, reasoning_envelope,
};
use routectl_testkit::capture_events;
use serde_json::json;

const ANTHROPIC_HOST: &str = "https://api.anthropic.com";
const ROUTECTL_HOP: &str = "https://router.internal.example/anthropic";

/// A foreign (Responses-family) reasoning artifact, i.e. the only kind an
/// envelope is ever wrapped around.
const INNER_BLOB: &str = "rsn_abc123-payload";
const SECOND_BLOB: &str = "smry_second-payload";
const ARTIFACT_ID: &str = "rs_42";
const OMIT_EVENT: &str = "reasoning_envelope_omitted";

fn wrapped_envelope() -> String {
    reasoning_envelope::wrap(
        routectl_core::OPENAI_RESPONSES_V1,
        Some(ARTIFACT_ID),
        INNER_BLOB,
    )
}

fn second_envelope() -> String {
    reasoning_envelope::wrap(routectl_core::CODEX_OAUTH, None, SECOND_BLOB)
}

fn redacted_part(data: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::RedactedThinking {
        data: data.to_string(),
        extras: serde_json::Map::new(),
    })
}

fn text_part(text: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::Text {
        text: text.into(),
        citations: None,
        cache_control: None,
        extras: serde_json::Map::new(),
    })
}

fn tool_use_part(id: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::ToolUse {
        id: id.into(),
        name: "lookup".into(),
        input: json!({"q": "x"}),
        cache_control: None,
        extras: serde_json::Map::new(),
    })
}

fn tool_result_part(id: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::ToolResult {
        tool_use_id: id.into(),
        content: json!("42"),
        is_error: None,
        cache_control: None,
        extras: serde_json::Map::new(),
    })
}

fn message(role: Role, content: MessageContent) -> Message {
    Message {
        refusal: None,
        role,
        content,
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        extras: serde_json::Map::new(),
    }
}

fn request(messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        model: "claude-sonnet-4-5".into(),
        messages: messages.into(),
        max_tokens: Some(1024),
        ..Default::default()
    }
}

/// A user turn, an assistant turn carrying `parts`, and a closing user
/// turn carrying `follow_up`.
fn history(parts: Vec<ContentPart>, follow_up: MessageContent) -> ChatRequest {
    request(vec![
        message(Role::User, MessageContent::Text("hi".into())),
        message(Role::Assistant, MessageContent::Parts(parts)),
        message(Role::User, follow_up),
    ])
}

/// [`history`] whose assistant turn keeps a text block beside `parts`, so
/// the turn is never emptied.
fn history_with(parts: Vec<ContentPart>) -> ChatRequest {
    let mut all = parts;
    all.push(text_part("answer"));
    history(all, MessageContent::Text("next".into()))
}

fn provider_at(base_url: &str) -> AnthropicApiProvider {
    let mut cfg = AnthropicApiConfig::new("anthropic:test", "test-key");
    cfg.base_url = base_url.to_string();
    AnthropicApiProvider::new(cfg)
}

/// An `Encrypted` reasoning detail tagged with the Anthropic format,
/// carrying `data` verbatim. The format tag is CLIENT-SUPPLIED on the
/// request schema, so this is a shape any caller can send.
fn encrypted_detail(data: &str) -> ReasoningDetail {
    ReasoningDetail {
        kind: ReasoningDetailKind::Encrypted,
        id: None,
        format: Some(ANTHROPIC_FORMAT.to_string()),
        index: Some(0),
        payload: json!({"data": data}),
    }
}

/// [`history_with`]'s request with `details` attached to its assistant
/// turn, so the `reasoning_details` replay channel is what emits the
/// `redacted_thinking` block.
fn history_with_details(parts: Vec<ContentPart>, details: Vec<ReasoningDetail>) -> ChatRequest {
    let base = history_with(parts);
    let mut msgs: Vec<Message> = base.messages.iter().cloned().collect();
    msgs[1].reasoning_details = details;
    ChatRequest {
        messages: msgs.into(),
        ..base
    }
}

/// A provider with context-management emulation on, whose thinking cache
/// is seeded with one `Encrypted` detail carrying `data` for `tool_use_id`.
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
/// one assistant turn with a `tool_calls` entry plus the `clear_thinking`
/// edit that arms the reinjection.
fn tool_use_history_with_clear_thinking_edit(tool_use_id: &str) -> ChatRequest {
    let mut assistant = message(Role::Assistant, MessageContent::Text("calling".into()));
    assistant.tool_calls = Some(vec![json!({
        "id": tool_use_id,
        "type": "function",
        "function": {"name": "calc", "arguments": "{}"}
    })]);
    let mut tool = message(Role::Tool, MessageContent::Text("42".into()));
    tool.tool_call_id = Some(tool_use_id.to_string());
    ChatRequest {
        provider_extras: Some(json!({
            "context_management": {
                "edits": [{
                    "type": context_management::CLEAR_THINKING_EDIT_TYPE,
                    "keep": "all"
                }]
            }
        })),
        ..request(vec![
            message(Role::User, MessageContent::Text("use the tool".into())),
            assistant,
            tool,
        ])
    }
}

/// One way a recognized envelope reaches the wire, isolated so each
/// construction site is exercised by exactly one row.
#[derive(Clone, Copy, Debug)]
enum Channel {
    ContentPart,
    ReasoningDetails,
    CacheReinjection,
}

impl Channel {
    const ALL: [Self; 3] = [
        Self::ContentPart,
        Self::ReasoningDetails,
        Self::CacheReinjection,
    ];

    /// The provider and the request that carry `envelope` through this
    /// channel and no other.
    fn fixture(self, base_url: &str, envelope: &str) -> (AnthropicApiProvider, ChatRequest) {
        match self {
            Self::ContentPart => (
                provider_at(base_url),
                history_with(vec![redacted_part(envelope)]),
            ),
            Self::ReasoningDetails => (
                provider_at(base_url),
                history_with_details(vec![], vec![encrypted_detail(envelope)]),
            ),
            Self::CacheReinjection => (
                provider_with_cached_thinking(base_url, "toolu_cm1", envelope),
                tool_use_history_with_clear_thinking_edit("toolu_cm1"),
            ),
        }
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

/// `(role, block types)` of every wire message, in order.
fn shape(body: &Value) -> Vec<(String, Vec<String>)> {
    body["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .map(|m| {
            let role = m["role"].as_str().expect("role").to_string();
            let types = match &m["content"] {
                Value::Array(blocks) => blocks
                    .iter()
                    .map(|b| b["type"].as_str().expect("block type").to_string())
                    .collect(),
                _ => vec!["<string>".to_string()],
            };
            (role, types)
        })
        .collect()
}

fn owned_shape(rows: &[(&str, &[&str])]) -> Vec<(String, Vec<String>)> {
    rows.iter()
        .map(|(role, types)| {
            (
                (*role).to_string(),
                types.iter().map(|t| (*t).to_string()).collect(),
            )
        })
        .collect()
}

fn normalize_capturing(
    provider: &AnthropicApiProvider,
    req: &ChatRequest,
) -> (Value, Vec<routectl_testkit::CapturedEvent>) {
    let mut body = None;
    let events = capture_events(|| {
        body = Some(provider.normalize_request(req).expect("normalize"));
    });
    (body.expect("normalize ran"), events)
}

fn omit_events(
    events: &[routectl_testkit::CapturedEvent],
) -> Vec<&routectl_testkit::CapturedEvent> {
    events
        .iter()
        .filter(|e| e.field("event") == Some(OMIT_EVENT))
        .collect()
}

/// Neither the envelope nor the artifact it wraps appears anywhere in
/// `body`.
fn assert_no_foreign_artifact(body: &Value, context: &str) {
    let rendered = body.to_string();
    for forbidden in ["rctl1", INNER_BLOB, SECOND_BLOB] {
        assert!(
            !rendered.contains(forbidden),
            "{context}: the wire body carries `{forbidden}`: {body}"
        );
    }
}

/// At the genuine Anthropic host the envelope's block is omitted on each
/// channel that can build one. Each row feeds exactly one channel, so a
/// channel that ships the envelope or its inner blob fails its own row.
#[test]
fn each_channel_omits_a_foreign_envelope_at_the_anthropic_host() {
    for channel in Channel::ALL {
        // Arrange
        let (provider, req) = channel.fixture(ANTHROPIC_HOST, &wrapped_envelope());

        // Act
        let body = provider.normalize_request(&req).expect("normalize");

        // Assert
        assert_no_foreign_artifact(&body, &format!("{channel:?}"));
        assert!(
            redacted_data(&body).is_empty(),
            "{channel:?}: no redacted_thinking block may remain: {body}"
        );
    }
}

/// The routectl-to-routectl continuity guard, per channel. A target that
/// is not the genuine host (another routectl hop, any Anthropic-compatible
/// third party) MUST see the envelope byte for byte: the next hop needs
/// it to recover the artifact's scheme and id.
#[test]
fn each_channel_keeps_the_envelope_byte_identical_at_a_non_anthropic_target() {
    for channel in Channel::ALL {
        // Arrange
        let envelope = wrapped_envelope();
        let (provider, req) = channel.fixture(ROUTECTL_HOP, &envelope);

        // Act
        let body = provider.normalize_request(&req).expect("normalize");

        // Assert
        assert_eq!(
            redacted_data(&body),
            vec![envelope],
            "{channel:?}: a non-Anthropic target must keep the envelope verbatim: {body}"
        );
    }
}

/// Omission leaves the rest of the turn exactly as the client sent it,
/// and a turn the omission empties is dropped rather than shipped as
/// `content: []`. Tool correlation is kept: the tool_use survives in place
/// and its tool_result still follows it.
#[test]
fn omitting_a_foreign_envelope_keeps_the_rest_of_the_history() {
    struct Row {
        name: &'static str,
        request: fn() -> ChatRequest,
        expected: &'static [(&'static str, &'static [&'static str])],
    }
    let rows = [
        Row {
            name: "envelope-only assistant turn is dropped",
            request: || {
                history(
                    vec![redacted_part(&wrapped_envelope())],
                    MessageContent::Text("next".into()),
                )
            },
            expected: &[("user", &["<string>"]), ("user", &["<string>"])],
        },
        Row {
            name: "envelope plus text keeps the text",
            request: || {
                history(
                    vec![redacted_part(&wrapped_envelope()), text_part("answer")],
                    MessageContent::Text("next".into()),
                )
            },
            expected: &[
                ("user", &["<string>"]),
                ("assistant", &["text"]),
                ("user", &["<string>"]),
            ],
        },
        Row {
            name: "envelope plus tool_use keeps the tool exchange",
            request: || {
                history(
                    vec![
                        redacted_part(&wrapped_envelope()),
                        text_part("calling"),
                        tool_use_part("toolu_1"),
                    ],
                    MessageContent::Parts(vec![tool_result_part("toolu_1")]),
                )
            },
            expected: &[
                ("user", &["<string>"]),
                ("assistant", &["text", "tool_use"]),
                ("user", &["tool_result"]),
            ],
        },
        Row {
            name: "tool turn emptied by an omitted envelope still answers its tool_use",
            request: || {
                let mut assistant = message(Role::Assistant, MessageContent::Text("c".into()));
                assistant.tool_calls = Some(vec![json!({
                    "id": "toolu_1", "type": "function",
                    "function": {"name": "lookup", "arguments": "{}"}
                })]);
                let mut tool = message(
                    Role::Tool,
                    MessageContent::Parts(vec![redacted_part(&wrapped_envelope())]),
                );
                tool.tool_call_id = Some("toolu_1".into());
                request(vec![
                    message(Role::User, MessageContent::Text("hi".into())),
                    assistant,
                    tool,
                ])
            },
            expected: &[
                ("user", &["<string>"]),
                ("assistant", &["text", "tool_use"]),
                ("user", &["tool_result"]),
            ],
        },
        Row {
            name: "two envelopes and nothing else drop the turn",
            request: || {
                history(
                    vec![
                        redacted_part(&wrapped_envelope()),
                        redacted_part(&second_envelope()),
                    ],
                    MessageContent::Text("next".into()),
                )
            },
            expected: &[("user", &["<string>"]), ("user", &["<string>"])],
        },
    ];

    for row in rows {
        // Arrange
        let provider = provider_at(ANTHROPIC_HOST);

        // Act
        let body = provider
            .normalize_request(&(row.request)())
            .expect("normalize");

        // Assert
        assert_no_foreign_artifact(&body, row.name);
        assert_eq!(
            shape(&body),
            owned_shape(row.expected),
            "{}: {body}",
            row.name
        );
    }
}

/// The tool exchange row, by value: the surviving tool_use and the
/// tool_result answering it carry the same id, and the text the client
/// sent ships unchanged.
#[test]
fn the_tool_exchange_beside_an_omitted_envelope_still_correlates() {
    // Arrange
    let provider = provider_at(ANTHROPIC_HOST);
    let req = history(
        vec![
            redacted_part(&wrapped_envelope()),
            text_part("calling"),
            tool_use_part("toolu_1"),
        ],
        MessageContent::Parts(vec![tool_result_part("toolu_1")]),
    );

    // Act
    let body = provider.normalize_request(&req).expect("normalize");

    // Assert
    let assistant = &body["messages"][1]["content"];
    assert_eq!(assistant[0], json!({"type": "text", "text": "calling"}));
    assert_eq!(assistant[1]["id"], "toolu_1");
    assert_eq!(body["messages"][2]["content"][0]["tool_use_id"], "toolu_1");
}

/// Anything that is not a recognized envelope ships unchanged at the
/// terminal host: a native Anthropic blob, an unknown version prefix, a
/// truncated envelope and an envelope with an empty blob. Nothing is
/// omitted, so no omission is reported.
#[test]
fn a_string_that_is_not_a_recognized_envelope_ships_unchanged_at_the_anthropic_host() {
    let candidates = [
        ("native blob", "EqQBCkgIARABGAIiQK-native".to_string()),
        (
            "unknown version",
            format!(
                "rctl9.{}.{ARTIFACT_ID}.{INNER_BLOB}",
                routectl_core::OPENAI_RESPONSES_V1
            ),
        ),
        (
            "truncated",
            format!("rctl1.{}.{ARTIFACT_ID}", routectl_core::OPENAI_RESPONSES_V1),
        ),
        (
            "empty blob",
            format!(
                "rctl1.{}.{ARTIFACT_ID}.",
                routectl_core::OPENAI_RESPONSES_V1
            ),
        ),
    ];
    let provider = provider_at(ANTHROPIC_HOST);

    for (name, candidate) in candidates {
        // Arrange
        let req = history_with(vec![redacted_part(&candidate)]);

        // Act
        let (body, events) = normalize_capturing(&provider, &req);

        // Assert
        assert_eq!(
            redacted_data(&body),
            vec![candidate.clone()],
            "{name}: must ship byte for byte: {body}"
        );
        assert!(
            omit_events(&events).is_empty(),
            "{name}: nothing was omitted, so no omission line: {events:?}"
        );
    }
}

/// Envelopes on every channel of one request produce exactly ONE DEBUG
/// line, whose count covers them all. A history can hold unbounded
/// reasoning blocks, so a per-block line would be a log amplifier driven
/// by request content. The line carries a constant event name, the
/// provider id and the count: never the blob, the claimed id, the claimed
/// scheme, or any fragment of the envelope.
#[test]
fn one_debug_line_counts_every_omitted_block_and_carries_no_payload() {
    // Arrange -- two content-part envelopes and one reasoning_details
    // envelope on the same assistant turn.
    let provider = provider_at(ANTHROPIC_HOST);
    let req = history_with_details(
        vec![
            redacted_part(&wrapped_envelope()),
            redacted_part(&second_envelope()),
        ],
        vec![encrypted_detail(&wrapped_envelope())],
    );

    // Act
    let (body, events) = normalize_capturing(&provider, &req);

    // Assert
    assert_no_foreign_artifact(&body, "multi-channel");
    let lines = omit_events(&events);
    assert_eq!(lines.len(), 1, "exactly one omission line: {events:?}");
    let line = lines[0];
    assert_eq!(line.level, tracing::Level::DEBUG);
    assert_eq!(line.field("provider"), Some("anthropic:test"));
    assert_eq!(line.field("omitted_count"), Some("3"));
    let rendered = format!("{events:?}");
    for forbidden in [
        INNER_BLOB,
        SECOND_BLOB,
        ARTIFACT_ID,
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

/// A lane that keeps unsigned thinking for an upstream that needs it echoed
/// still omits a foreign envelope: no history policy makes the genuine host
/// accept an artifact it did not mint.
#[test]
fn a_foreign_envelope_is_omitted_under_the_preserve_history_policy() {
    // Arrange
    let provider = provider_at(ANTHROPIC_HOST);
    let mut req = history_with(vec![redacted_part(&wrapped_envelope())]);
    req.routectl_internal.history_reasoning = Some(CoreHistoryReasoning::Preserve);

    // Act
    let body = provider.normalize_request(&req).expect("normalize");

    // Assert
    assert_no_foreign_artifact(&body, "preserve");
    assert_eq!(
        shape(&body),
        owned_shape(&[
            ("user", &["<string>"]),
            ("assistant", &["text"]),
            ("user", &["<string>"]),
        ]),
        "{body}"
    );
}
