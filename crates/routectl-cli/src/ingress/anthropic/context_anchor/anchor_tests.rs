//! Prefix identity and anchor evaluation, plus the shared fixtures the
//! sibling test modules build on.

use std::sync::Arc;

use routectl_core::test_utils::{assistant_text_msg, user_msg};
use routectl_core::{
    CacheControl, ChatRequest, ContentPart, CustomTool, KnownContentPart, Message, MessageContent,
    Role, SystemBlock, SystemContent, ToolDef,
};
use serde_json::{Value, json};

use super::{
    AnchorKey, AnchorLane, AnchorVerdict, ContextAnchorStore, MissReason, RequestIdentity,
    TurnOutcome, evaluate,
};

const SESSION: &str = "session-a";
const MODEL: &str = "claude-opus";

pub(super) fn lane() -> AnchorLane {
    AnchorLane {
        provider_kind: "openai-compat".into(),
        nickname: "glm".into(),
        upstream_model: "glm-4.6".into(),
        generation: 7,
    }
}

fn other_lane() -> AnchorLane {
    AnchorLane {
        provider_kind: "bedrock".into(),
        ..lane()
    }
}

pub(super) fn text_block(text: &str) -> SystemBlock {
    SystemBlock {
        kind: "text".into(),
        text: text.into(),
        cache_control: None,
        citations: None,
    }
}

pub(super) fn billing_block(checksum: &str) -> SystemBlock {
    text_block(&format!(
        "x-anthropic-billing-header: cc_version=2.1.0; cch={checksum}; cc_prompt_id=p-{checksum};"
    ))
}

pub(super) fn tool(name: &str, description: &str) -> ToolDef {
    ToolDef::Custom(CustomTool {
        name: name.into(),
        description: Some(description.into()),
        input_schema: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        cache_control: None,
        defer_loading: None,
        strict: None,
        type_tag: None,
    })
}

pub(super) fn parts_msg(role: Role, parts: Vec<KnownContentPart>) -> Message {
    Message {
        content: MessageContent::Parts(parts.into_iter().map(ContentPart::Known).collect()),
        role,
        ..user_msg("")
    }
}

pub(super) fn text_part(text: &str, cache_control: Option<CacheControl>) -> KnownContentPart {
    KnownContentPart::Text {
        text: text.into(),
        citations: None,
        cache_control,
    }
}

pub(super) fn tool_use(id: &str, input: Value) -> Message {
    parts_msg(
        Role::Assistant,
        vec![KnownContentPart::ToolUse {
            id: id.into(),
            name: "read_file".into(),
            input,
            cache_control: None,
        }],
    )
}

pub(super) fn tool_result(id: &str, content: Value) -> Message {
    parts_msg(
        Role::User,
        vec![KnownContentPart::ToolResult {
            tool_use_id: id.into(),
            content,
            is_error: None,
            cache_control: None,
        }],
    )
}

/// A Claude-Code-shaped request: billing block plus prose system prompt,
/// two tools, request metadata, and `messages`.
pub(super) fn request(messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        model: MODEL.into(),
        messages: Arc::from(messages),
        system: Some(SystemContent::Blocks(vec![
            billing_block("00001"),
            text_block("You are a careful coding agent."),
        ])),
        tools: Some(vec![
            tool("read_file", "Read a file"),
            tool("write_file", "Write a file"),
        ]),
        max_tokens: Some(4096),
        provider_extras: Some(json!({
            "metadata": {"user_id": "u-1"},
            "context_management": {"edits": [{"type": "clear_thinking"}]},
        })),
        ..Default::default()
    }
}

pub(super) fn history() -> Vec<Message> {
    vec![
        user_msg("Summarize the repository layout."),
        tool_use("toolu_01", json!({"path": "Cargo.toml"})),
        tool_result("toolu_01", json!("[workspace]\nmembers = [\"crates/a\"]")),
        assistant_text_msg("The workspace has one member crate."),
    ]
}

pub(super) fn grown(base: &[Message]) -> Vec<Message> {
    let mut messages = base.to_vec();
    messages.push(user_msg("Now list its dependencies."));
    messages.push(tool_use("toolu_02", json!({"path": "crates/a/Cargo.toml"})));
    messages.push(tool_result(
        "toolu_02",
        json!("[dependencies]\nserde = \"1\""),
    ));
    messages
}

/// Publish `prior` as a successful turn with `actual` input, then check
/// `next` against the resulting anchor on `next_lane`.
pub(super) fn anchor_then_check(
    prior: &ChatRequest,
    actual: u64,
    next: &ChatRequest,
    next_lane: &AnchorLane,
) -> AnchorVerdict {
    let store = ContextAnchorStore::new();
    let key = AnchorKey::new(SESSION, MODEL).expect("key within bounds");
    let pending = store.reserve_turn(key.clone()).measure(prior, None);
    pending.settle(TurnOutcome::Completed {
        served_lane: lane(),
        cache_inclusive_input: Some(actual),
    });
    let ticket = store.reserve_turn(key);
    let record = ticket.current_record();
    let next_pending = ticket.measure(next, record.as_ref().map(|r| r.message_count()));
    evaluate(record.as_deref(), next_pending.identity(), next_lane)
}

pub(super) fn assert_miss(verdict: AnchorVerdict, reason: MissReason) {
    assert_eq!(verdict, AnchorVerdict::Miss(reason));
}

pub(super) fn assert_hit(verdict: AnchorVerdict) {
    assert!(
        matches!(verdict, AnchorVerdict::Hit { .. }),
        "expected a hit, got {verdict:?}"
    );
}

pub(super) fn serialized_len(req: &ChatRequest) -> usize {
    serde_json::to_string(req)
        .expect("fixture serializes")
        .len()
}

// ---------------------------------------------------------------- growth

#[test]
fn appended_turns_hit_with_prior_actual_plus_normalized_growth() {
    // Arrange
    let prior = request(history());
    let next = request(grown(&history()));
    let prior_norm = RequestIdentity::measure(&prior, None).normalized_estimate();
    let next_norm = RequestIdentity::measure(&next, None).normalized_estimate();

    // Act
    let verdict = anchor_then_check(&prior, 50_000, &next, &lane());

    // Assert
    assert!(next_norm > prior_norm);
    assert_eq!(
        verdict,
        AnchorVerdict::Hit {
            opening_input: 50_000 + (next_norm - prior_norm),
        }
    );
}

#[test]
fn identical_retry_hits_with_zero_growth() {
    let prior = request(history());

    let verdict = anchor_then_check(&prior, 12_345, &prior.clone(), &lane());

    assert_eq!(
        verdict,
        AnchorVerdict::Hit {
            opening_input: 12_345
        }
    );
}

#[test]
fn cache_breakpoint_moving_to_the_newest_message_still_hits() {
    // Arrange: the client marks the last message of each request; on the
    // next turn that marker moves forward to the new last message.
    let marker = Some(CacheControl::ephemeral_5m());
    let mut prior_messages = history();
    prior_messages.push(parts_msg(
        Role::User,
        vec![text_part("Which crate owns parsing?", marker.clone())],
    ));
    let mut next_messages = history();
    next_messages.push(parts_msg(
        Role::User,
        vec![text_part("Which crate owns parsing?", None)],
    ));
    next_messages.push(assistant_text_msg("Crate a."));
    next_messages.push(parts_msg(Role::User, vec![text_part("Thanks.", marker)]));
    let prior = ChatRequest {
        cache_control: Some(CacheControl::ephemeral_5m()),
        ..request(prior_messages)
    };
    let next = request(next_messages);

    // Act
    let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

    // Assert
    assert_hit(verdict);
}

#[test]
fn cache_marker_on_a_nested_tool_result_block_is_ignored() {
    let block = |cache_control: Option<Value>| {
        let mut b = json!({"type": "text", "text": "file contents"});
        if let Some(cc) = cache_control {
            b["cache_control"] = cc;
        }
        tool_result("toolu_09", Value::Array(vec![b]))
    };
    let mut prior_messages = history();
    prior_messages.push(block(Some(json!({"type": "ephemeral"}))));
    let mut next_messages = history();
    next_messages.push(block(None));
    next_messages.push(assistant_text_msg("Read it."));

    let verdict = anchor_then_check(
        &request(prior_messages),
        1_000,
        &request(next_messages),
        &lane(),
    );

    assert_hit(verdict);
}

#[test]
fn cache_markers_moving_between_system_blocks_and_tools_still_hit() {
    // Arrange
    let marked = |text: &str| SystemBlock {
        cache_control: Some(CacheControl::ephemeral_1h()),
        ..text_block(text)
    };
    let marked_tool = ToolDef::Custom(CustomTool {
        cache_control: Some(CacheControl::ephemeral_1h()),
        ..match tool("write_file", "Write a file") {
            ToolDef::Custom(custom) => custom,
            ToolDef::Other(_) => unreachable!("fixture builds a custom tool"),
        }
    });
    let prior = ChatRequest {
        system: Some(SystemContent::Blocks(vec![
            billing_block("00001"),
            marked("You are a careful coding agent."),
        ])),
        ..request(history())
    };
    let next = ChatRequest {
        tools: Some(vec![tool("read_file", "Read a file"), marked_tool]),
        ..request(grown(&history()))
    };

    // Act
    let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

    // Assert
    assert_hit(verdict);
}

#[test]
fn per_request_billing_checksum_changes_still_hit() {
    // Arrange
    let prior = request(history());
    let next = ChatRequest {
        system: Some(SystemContent::Blocks(vec![
            billing_block("7f3a2"),
            text_block("You are a careful coding agent."),
        ])),
        ..request(grown(&history()))
    };

    // Act
    let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

    // Assert
    assert_hit(verdict);
}

#[test]
fn changed_request_metadata_misses() {
    // The Anthropic egress forwards `metadata` upstream, so it is hashed.
    let prior = request(history());
    let next = ChatRequest {
        provider_extras: Some(json!({
            "metadata": {"user_id": "u-2"},
            "context_management": {"edits": [{"type": "clear_thinking"}]},
        })),
        ..request(grown(&history()))
    };

    let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

    assert_miss(verdict, MissReason::PrefixChanged);
}

#[test]
fn added_diagnostics_extra_misses() {
    let prior = request(history());
    let next = ChatRequest {
        provider_extras: Some(json!({
            "metadata": {"user_id": "u-1"},
            "diagnostics": {"previous_message_id": "msg_02"},
            "context_management": {"edits": [{"type": "clear_thinking"}]},
        })),
        ..request(grown(&history()))
    };

    let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

    assert_miss(verdict, MissReason::PrefixChanged);
}

#[test]
fn sampling_parameter_changes_still_hit() {
    let prior = request(history());
    let next = ChatRequest {
        temperature: Some(0.2),
        max_tokens: Some(64_000),
        stop: Some(vec!["END".into()]),
        ..request(grown(&history()))
    };

    let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

    assert_hit(verdict);
}

pub(super) fn history_ending_with(message: Message) -> Vec<Message> {
    let mut messages = history();
    messages.push(message);
    messages
}

// ---------------------------------------------------------------- misses

#[test]
fn same_length_rewrite_of_a_middle_message_misses() {
    // Arrange
    let prior = request(history());
    let mut edited = grown(&history());
    edited[2] = tool_result("toolu_01", json!("[workspace]\nmembers = [\"crates/b\"]"));
    let unedited = request(grown(&history()));
    let next = request(edited);

    // Act
    let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

    // Assert
    assert_eq!(serialized_len(&next), serialized_len(&unedited));
    assert_miss(verdict, MissReason::PrefixChanged);
}

#[test]
fn rewrite_of_the_last_anchored_message_misses() {
    // The boundary message: a check over the first N-1 messages would pass.
    let prior = request(history());
    let mut edited = grown(&history());
    edited[3] = assistant_text_msg("The workspace has one member crate!");

    let verdict = anchor_then_check(&prior, 1_000, &request(edited), &lane());

    assert_miss(verdict, MissReason::PrefixChanged);
}

#[test]
fn rewrite_of_the_first_message_misses() {
    let prior = request(history());
    let mut edited = grown(&history());
    edited[0] = user_msg("Summarize the repository layouts");

    let verdict = anchor_then_check(&prior, 1_000, &request(edited), &lane());

    assert_miss(verdict, MissReason::PrefixChanged);
}

#[test]
fn same_byte_length_unicode_substitution_misses() {
    // Arrange: two different two-byte characters.
    let prior = request(vec![user_msg("caf\u{e9} order"), assistant_text_msg("ok")]);
    let next = request(vec![
        user_msg("caf\u{e8} order"),
        assistant_text_msg("ok"),
        user_msg("more"),
    ]);

    // Act
    let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

    // Assert
    assert_eq!("\u{e9}".len(), "\u{e8}".len());
    assert_miss(verdict, MissReason::PrefixChanged);
}

#[test]
fn compacted_shorter_history_misses() {
    let prior = request(grown(&history()));
    let compacted = request(vec![
        user_msg("Summary of the conversation so far."),
        user_msg("Continue."),
    ]);

    let verdict = anchor_then_check(&prior, 1_000, &compacted, &lane());

    assert_miss(verdict, MissReason::HistoryShrank);
}

#[test]
fn compaction_that_keeps_the_message_count_misses() {
    let prior = request(history());
    let mut rewritten: Vec<Message> = (0..history().len())
        .map(|i| user_msg(&format!("summary part {i}")))
        .collect();
    rewritten.push(user_msg("Continue."));

    let verdict = anchor_then_check(&prior, 1_000, &request(rewritten), &lane());

    assert_miss(verdict, MissReason::PrefixChanged);
}

#[test]
fn changed_system_prompt_misses() {
    let prior = request(history());
    let next = ChatRequest {
        system: Some(SystemContent::Blocks(vec![
            billing_block("00001"),
            text_block("You are a terse coding agent."),
        ])),
        ..request(grown(&history()))
    };

    let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

    assert_miss(verdict, MissReason::PrefixChanged);
}

#[test]
fn changed_tool_definitions_miss() {
    let prior = request(history());
    let next = ChatRequest {
        tools: Some(vec![tool("read_file", "Read one file")]),
        ..request(grown(&history()))
    };

    let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

    assert_miss(verdict, MissReason::PrefixChanged);
}

#[test]
fn changed_prompt_editing_extras_miss() {
    // `context_management` can clear old content server-side, so it is
    // prompt-affecting even though it rides in the passthrough extras.
    let prior = request(history());
    let next = ChatRequest {
        provider_extras: Some(json!({
            "metadata": {"user_id": "u-1"},
            "context_management": {"edits": [{"type": "clear_tool_uses"}]},
        })),
        ..request(grown(&history()))
    };

    let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

    assert_miss(verdict, MissReason::PrefixChanged);
}

#[test]
fn a_billing_looking_line_inside_prose_is_still_hashed() {
    // Only a block that STARTS with the billing marker is excluded.
    let prose = |checksum: &str| {
        text_block(&format!(
            "Docs: x-anthropic-billing-header: cch={checksum}; is a header."
        ))
    };
    let prior = ChatRequest {
        system: Some(SystemContent::Blocks(vec![prose("00001")])),
        ..request(history())
    };
    let next = ChatRequest {
        system: Some(SystemContent::Blocks(vec![prose("00002")])),
        ..request(grown(&history()))
    };

    let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

    assert_miss(verdict, MissReason::PrefixChanged);
}

#[test]
fn a_different_serving_lane_misses() {
    let verdict = anchor_then_check(
        &request(history()),
        1_000,
        &request(grown(&history())),
        &other_lane(),
    );

    assert_miss(verdict, MissReason::LaneChanged);
}

#[test]
fn a_different_requested_model_is_cold() {
    // Arrange
    let store = ContextAnchorStore::new();
    let key = AnchorKey::new(SESSION, MODEL).expect("key within bounds");
    let pending = store.reserve_turn(key).measure(&request(history()), None);
    pending.settle(TurnOutcome::Completed {
        served_lane: lane(),
        cache_inclusive_input: Some(1_000),
    });

    // Act
    let record = store.get(&AnchorKey::new(SESSION, "claude-haiku").expect("key within bounds"));
    let identity = RequestIdentity::measure(&request(grown(&history())), None);

    // Assert
    assert!(record.is_none());
    assert_miss(
        evaluate(record.as_deref(), &identity, &lane()),
        MissReason::Cold,
    );
}

#[test]
fn a_second_conversation_reusing_the_session_id_misses() {
    // Arrange: a subagent shares the session key but has its own prompt.
    let main = request(grown(&history()));
    let subagent = ChatRequest {
        system: Some(SystemContent::Blocks(vec![
            billing_block("00009"),
            text_block("You are a search subagent."),
        ])),
        tools: Some(vec![tool("grep", "Search files")]),
        ..request(vec![
            user_msg("Find every caller of parse()."),
            assistant_text_msg("Searching."),
            user_msg("Continue."),
            assistant_text_msg("Found three."),
            user_msg("List them."),
            assistant_text_msg("a, b, c"),
            user_msg("Thanks."),
            assistant_text_msg("Done."),
        ])
    };

    // Act
    let verdict = anchor_then_check(&main, 1_000, &subagent, &lane());

    // Assert
    assert!(subagent.messages.len() > main.messages.len());
    assert_miss(verdict, MissReason::PrefixChanged);
}

#[test]
fn no_record_is_cold() {
    let identity = RequestIdentity::measure(&request(history()), None);

    assert_miss(evaluate(None, &identity, &lane()), MissReason::Cold);
}

#[test]
fn an_empty_prior_history_still_verifies_the_stable_fields() {
    let prior = request(vec![]);
    let same_fields = request(history());
    let changed_fields = ChatRequest {
        tools: None,
        ..request(history())
    };

    let hit = anchor_then_check(&prior, 100, &same_fields, &lane());
    let miss = anchor_then_check(&prior, 100, &changed_fields, &lane());

    assert_hit(hit);
    assert_miss(miss, MissReason::PrefixChanged);
}

// ------------------------------------------------------------ retention

#[test]
fn a_record_retains_no_prompt_text() {
    // Arrange
    let prompt_marker = "zebra-sentinel-4417";
    let req = request(vec![user_msg(prompt_marker)]);
    let store = ContextAnchorStore::new();
    let key = AnchorKey::new(SESSION, MODEL).expect("key within bounds");
    let pending = store.reserve_turn(key.clone()).measure(&req, None);

    // Act
    pending.settle(TurnOutcome::Completed {
        served_lane: lane(),
        cache_inclusive_input: Some(10),
    });
    let record = store.get(&key).expect("published");

    // Assert
    assert!(
        format!("{req:?}").contains(prompt_marker),
        "positive control"
    );
    assert!(!format!("{record:?}").contains(prompt_marker));
}
