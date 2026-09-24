//! Where a cache annotation may be stripped, and which lane changes
//! invalidate an anchor.

use routectl_core::test_utils::user_msg;
use routectl_core::{
    CacheControl, ChatRequest, ContentPart, KnownContentPart, Message, MessageContent, Role,
    ToolDef,
};
use serde_json::{Value, json};

use super::tests::{
    anchor_then_check, assert_hit, assert_miss, grown, history, history_ending_with, lane,
    parts_msg, request, text_part, tool_result,
};
use super::{AnchorLane, MissReason};

// ------------------------------------------------ annotation positions

/// The history ending with `message`, followed by one more user turn.
fn then_go_on(message: Message) -> Vec<Message> {
    let mut messages = history_ending_with(message);
    messages.push(user_msg("go on"));
    messages
}

#[test]
fn a_cache_control_key_inside_tool_arguments_is_prompt_data() {
    // Arrange: the model passed an argument literally named cache_control,
    // and the block itself carries a real annotation, so the stripping path
    // runs over it.
    let call = |value: &str| {
        parts_msg(
            Role::Assistant,
            vec![KnownContentPart::ToolUse {
                id: "toolu_05".into(),
                name: "set_config".into(),
                input: json!({"path": "a.toml", "cache_control": value}),
                cache_control: Some(CacheControl::ephemeral_5m()),
            }],
        )
    };
    let prior = request(history_ending_with(call("keep")));
    let next = request(then_go_on(call("drop")));

    // Act
    let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

    // Assert
    assert_miss(verdict, MissReason::PrefixChanged);
}

#[test]
fn a_cache_control_key_nested_deep_in_a_tool_result_is_prompt_data() {
    // Only a block's own cache_control is an annotation; a key of that name
    // inside the block's payload is content. The block's own marker makes
    // the stripping path run over the payload.
    let nested = |value: &str| {
        tool_result(
            "toolu_06",
            json!([{
                "type": "text",
                "text": "cfg",
                "cache_control": {"type": "ephemeral"},
                "citations": {"cache_control": value},
            }]),
        )
    };
    let prior = request(history_ending_with(nested("a")));
    let next = request(then_go_on(nested("b")));

    let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

    assert_miss(verdict, MissReason::PrefixChanged);
}

#[test]
fn a_tool_result_with_non_block_array_content_is_hashed_verbatim() {
    // A tool result whose content array holds non-block values: nothing in
    // it is a content-block annotation.
    let odd = |value: &str| tool_result("toolu_07", json!([{"cache_control": value}]));
    let prior = request(history_ending_with(odd("a")));
    let next = request(then_go_on(odd("b")));

    let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

    assert_miss(verdict, MissReason::PrefixChanged);
}

fn other_part(type_tag: &str, cache_control: Option<CacheControl>, body: Value) -> ContentPart {
    let Value::Object(extras) = body else {
        panic!("fixture body must be an object");
    };
    ContentPart::Other {
        type_tag: type_tag.into(),
        cache_control,
        extras,
    }
}

fn other_msg(part: ContentPart) -> Message {
    Message {
        content: MessageContent::Parts(vec![part]),
        ..user_msg("")
    }
}

#[test]
fn an_unrecognized_block_is_hashed_verbatim_including_its_marker() {
    // An unmodeled block type: the anchor cannot know whether its
    // cache_control is an annotation, so a marker change must miss.
    let block = |cc| other_part("future_block", cc, json!({"payload": "p"}));
    let prior = request(history_ending_with(other_msg(block(Some(
        CacheControl::ephemeral_5m(),
    )))));
    let next = request(then_go_on(other_msg(block(None))));

    let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

    assert_miss(verdict, MissReason::PrefixChanged);
}

#[test]
fn an_unrecognized_block_keeps_its_marker_beside_a_stripped_known_block() {
    // A known marked block sends the message down the stripping path; the
    // unmodeled block in the same message must still keep its marker.
    let msg = |cc: Option<CacheControl>| Message {
        content: MessageContent::Parts(vec![
            ContentPart::Known(text_part("see below", cc.clone())),
            other_part("future_block", cc, json!({"payload": "p"})),
        ]),
        ..user_msg("")
    };
    let prior = request(history_ending_with(msg(Some(CacheControl::ephemeral_5m()))));
    let next = request(then_go_on(msg(None)));

    let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

    assert_miss(verdict, MissReason::PrefixChanged);
}

#[test]
fn a_server_tool_use_block_moves_its_marker_without_missing() {
    // `server_tool_use` is the one unmodeled block type whose top-level
    // cache_control the core schema already treats as a block annotation.
    let block = |cc| {
        other_part(
            "server_tool_use",
            cc,
            json!({"id": "srvtu_01", "name": "web_search", "input": {"query": "rust"}}),
        )
    };
    let prior = request(history_ending_with(other_msg(block(Some(
        CacheControl::ephemeral_5m(),
    )))));
    let next = request(then_go_on(other_msg(block(None))));

    let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

    assert_hit(verdict);
}

#[test]
fn other_unmodeled_server_block_types_hash_their_marker_verbatim() {
    // Block types this repo does not model or verify: a marker change is
    // treated as a prompt change.
    for type_tag in [
        "web_search_tool_result",
        "web_fetch_tool_result",
        "code_execution_tool_result",
        "mcp_tool_use",
        "mcp_tool_result",
        "search_result",
        "container_upload",
    ] {
        let block = |cc| other_part(type_tag, cc, json!({"tool_use_id": "srvtoolu_1"}));
        let prior = request(history_ending_with(other_msg(block(Some(
            CacheControl::ephemeral_5m(),
        )))));
        let next = request(then_go_on(other_msg(block(None))));

        let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

        assert_eq!(
            verdict,
            super::AnchorVerdict::Miss(MissReason::PrefixChanged),
            "{type_tag} must hash verbatim"
        );
    }
}

#[test]
fn a_typed_block_inside_a_tool_result_moves_its_marker_without_missing() {
    for block_type in ["text", "image", "document"] {
        let result = |marked: bool| {
            let mut block = json!({"type": block_type, "text": "t", "source": {"type": "text"}});
            if marked {
                block["cache_control"] = json!({"type": "ephemeral"});
            }
            tool_result("toolu_08", Value::Array(vec![block]))
        };
        let prior = request(history_ending_with(result(true)));
        let next = request(then_go_on(result(false)));

        let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

        assert!(
            matches!(verdict, super::AnchorVerdict::Hit { .. }),
            "{block_type}: {verdict:?}"
        );
    }
}

#[test]
fn an_unverified_block_inside_a_tool_result_hashes_its_marker_verbatim() {
    let result = |marked: bool| {
        let mut block = json!({"type": "search_result", "source": "s", "content": []});
        if marked {
            block["cache_control"] = json!({"type": "ephemeral"});
        }
        tool_result("toolu_08", Value::Array(vec![block]))
    };
    let prior = request(history_ending_with(result(true)));
    let next = request(then_go_on(result(false)));

    let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

    assert_miss(verdict, MissReason::PrefixChanged);
}

#[test]
fn a_passthrough_tool_is_hashed_verbatim_including_its_marker() {
    // `ToolDef::Other` covers builtin and future tool shapes; only the
    // typed custom tool's own marker is known to be an annotation.
    let builtin = |cc: Option<Value>| {
        let mut t = json!({"type": "future_tool_20990101", "name": "f"});
        if let Some(cc) = cc {
            t["cache_control"] = cc;
        }
        ToolDef::Other(t)
    };
    let prior = ChatRequest {
        tools: Some(vec![builtin(Some(json!({"type": "ephemeral"})))]),
        ..request(history())
    };
    let next = ChatRequest {
        tools: Some(vec![builtin(None)]),
        ..request(grown(&history()))
    };

    let verdict = anchor_then_check(&prior, 1_000, &next, &lane());

    assert_miss(verdict, MissReason::PrefixChanged);
}

// ------------------------------------------------------------ lane identity

#[test]
fn a_nickname_repointed_at_another_upstream_model_misses() {
    let repointed = AnchorLane {
        upstream_model: "glm-5".into(),
        ..lane()
    };

    let verdict = anchor_then_check(
        &request(history()),
        1_000,
        &request(grown(&history())),
        &repointed,
    );

    assert_miss(verdict, MissReason::LaneChanged);
}

#[test]
fn a_new_router_generation_misses() {
    let reloaded = AnchorLane {
        generation: lane().generation + 1,
        ..lane()
    };

    let verdict = anchor_then_check(
        &request(history()),
        1_000,
        &request(grown(&history())),
        &reloaded,
    );

    assert_miss(verdict, MissReason::GenerationChanged);
}

#[test]
fn a_different_nickname_on_the_same_upstream_misses() {
    let renamed = AnchorLane {
        model: "glm-alias".into(),
        ..lane()
    };

    let verdict = anchor_then_check(
        &request(history()),
        1_000,
        &request(grown(&history())),
        &renamed,
    );

    assert_miss(verdict, MissReason::LaneChanged);
}

#[test]
fn seat_rotation_within_the_same_target_hits() {
    // The lane has no seat field: two seats of one target build equal
    // lanes from the same resolved target.
    let seat_b = AnchorLane {
        provider_kind: lane().provider_kind,
        model: lane().model,
        upstream_model: lane().upstream_model,
        generation: lane().generation,
    };

    let verdict = anchor_then_check(
        &request(history()),
        1_000,
        &request(grown(&history())),
        &seat_b,
    );

    assert_hit(verdict);
}
