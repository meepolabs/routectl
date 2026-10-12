// The seam a dropped forwarded `Role::System` turn leaves in the wire
// `messages[]`: same-role neighbors merge into one message, and a cache
// breakpoint the dropped turn carried moves onto the preceding message.
// Imports live in the host `messages_tests.rs`; the `st_*` builders live in
// `messages_system_turn_tests.rs` -- do not add `use` lines here.

fn seam_blank_cached_turn() -> Message {
    st_system(MessageContent::Parts(vec![ContentPart::Known(
        KnownContentPart::Text {
            text: String::new(),
            citations: None,
            cache_control: Some(CacheControl::ephemeral_5m()),
        },
    )]))
}

fn seam_billing_turn() -> Message {
    st_system(MessageContent::Text(
        "x-anthropic-billing-header: v=1; fp=abc".into(),
    ))
}

/// Text of every block in `msg`, in order; a string carrier reads as one.
fn seam_texts(msg: &AnthropicMessage) -> Vec<String> {
    match &msg.content {
        AnthropicContent::Text(t) => vec![t.clone()],
        AnthropicContent::Blocks(blocks) => blocks
            .iter()
            .map(|b| match b {
                ContentBlock::Text { text, .. } => text.clone(),
                other => format!("<{other:?}>"),
            })
            .collect(),
    }
}

fn seam_breakpoint_count(out: &[AnthropicMessage]) -> usize {
    let body = serde_json::to_string(out).expect("serialize");
    body.matches("\"cache_control\"").count()
}

/// A dropped turn between two same-role neighbors leaves them merged into
/// one message carrying both bodies in order, for each drop verdict and for
/// both conversational roles. The ledger verify inside `translate_messages`
/// would fail the call if the merge were not charged to its own term.
#[test]
fn same_role_neighbors_of_a_dropped_system_turn_merge_in_order() {
    let cases: [(&str, Message, Message, Message, &str); 4] = [
        (
            "user / empty / user",
            st_user("a"),
            st_system(MessageContent::Parts(vec![])),
            st_user("b"),
            "user",
        ),
        (
            "assistant / empty / assistant",
            st_assistant("a"),
            st_system(MessageContent::Text(String::new())),
            st_assistant("b"),
            "assistant",
        ),
        (
            "user / billing / user",
            st_user("a"),
            seam_billing_turn(),
            st_user("b"),
            "user",
        ),
        (
            "user / blank cached / user",
            st_user("a"),
            seam_blank_cached_turn(),
            st_user("b"),
            "user",
        ),
    ];
    for (name, before, dropped, after, role) in cases {
        // Arrange
        let messages = vec![before, dropped, after];

        // Act
        let out = st_forward(&messages);

        // Assert
        assert_eq!(st_roles(&out), vec![role], "{name}");
        assert_eq!(seam_texts(&out[0]), vec!["a", "b"], "{name}");
    }
}

/// Positive control for the merge: the same neighbors around a system turn
/// that FORWARDS stay three messages, and two user turns the caller sent
/// back to back with no system turn between them are left as sent. The
/// merge fires only across a dropped turn's seam.
#[test]
fn same_role_neighbors_merge_only_across_a_dropped_turn() {
    // Arrange
    let forwarded = vec![
        st_user("a"),
        st_system(MessageContent::Text("note".into())),
        st_user("b"),
    ];
    let caller_adjacent = vec![st_user("a"), st_user("b")];

    // Act
    let forwarded_out = st_forward(&forwarded);
    let adjacent_out = st_forward(&caller_adjacent);

    // Assert
    assert_eq!(st_roles(&forwarded_out), vec!["user", "system", "user"]);
    assert_eq!(st_roles(&adjacent_out), vec!["user", "user"]);
}

/// A dropped turn at the head leaves the conversation starting with
/// whatever followed it. A history that opens with an assistant turn is
/// shipped as-is by this walk when no system turn is involved, so the drop
/// produces exactly that wire shape rather than inventing a user turn.
#[test]
fn leading_dropped_system_turn_matches_the_history_without_it() {
    // Arrange
    let with_turn = vec![
        st_system(MessageContent::Parts(vec![])),
        st_assistant("hello"),
        st_user("hi"),
    ];
    let without_turn = vec![st_assistant("hello"), st_user("hi")];

    // Act
    let with_out = st_forward(&with_turn);
    let without_out = st_forward(&without_turn);

    // Assert
    assert_eq!(st_roles(&with_out), vec!["assistant", "user"]);
    assert_eq!(
        serde_json::to_value(&with_out).expect("serialize"),
        serde_json::to_value(&without_out).expect("serialize"),
    );
}

/// The blank cached turn Claude Code sends after compaction keeps its
/// breakpoint: it lands on the last block of the preceding message, as the
/// one breakpoint in the output, and the move is counted on the screen line.
#[test]
fn dropped_turn_breakpoint_moves_onto_the_preceding_block() {
    // Arrange
    let messages = vec![st_user("a"), seam_blank_cached_turn(), st_assistant("ok")];

    // Act
    let mut out = Vec::new();
    let events = capture_events(|| out = st_forward(&messages));

    // Assert
    assert_eq!(st_roles(&out), vec!["user", "assistant"]);
    let AnthropicContent::Blocks(blocks) = &out[0].content else {
        panic!(
            "the anchor message must be in block form: {:?}",
            out[0].content
        );
    };
    assert!(
        matches!(
            &blocks[..],
            [ContentBlock::Text { text, cache_control: Some(cc), .. }]
                if text == "a" && *cc == CacheControl::ephemeral_5m()
        ),
        "got {blocks:?}"
    );
    assert_eq!(seam_breakpoint_count(&out), 1);
    let line = events
        .iter()
        .find(|e| e.field("system_turn_breakpoints_moved").is_some())
        .unwrap_or_else(|| panic!("no screen line: {events:?}"));
    assert_eq!(line.field("system_turn_breakpoints_moved"), Some("1"));
    assert_eq!(line.field("system_turn_breakpoints_dropped"), Some("0"));
}

/// The moved breakpoint survives the same-role merge too: it stays on the
/// block that ended the dropped turn's prefix, mid-message, and the request
/// still carries exactly one breakpoint.
#[test]
fn moved_breakpoint_survives_the_merge_at_the_same_boundary() {
    // Arrange
    let messages = vec![st_user("a"), seam_blank_cached_turn(), st_user("b")];

    // Act
    let out = st_forward(&messages);

    // Assert
    let AnthropicContent::Blocks(blocks) = &out[0].content else {
        panic!("merged message must be in block form: {:?}", out[0].content);
    };
    assert!(
        matches!(
            &blocks[..],
            [
                ContentBlock::Text { text: a, cache_control: Some(_), .. },
                ContentBlock::Text { text: b, cache_control: None, .. },
            ] if a == "a" && b == "b"
        ),
        "got {blocks:?}"
    );
    assert_eq!(seam_breakpoint_count(&out), 1);
}

/// With no preceding message the breakpoint has nowhere to go: it is
/// dropped with the turn, and the drop is counted rather than silent.
#[test]
fn leading_dropped_turn_breakpoint_is_dropped_and_counted() {
    // Arrange
    let messages = vec![seam_blank_cached_turn(), st_user("hi")];

    // Act
    let mut out = Vec::new();
    let events = capture_events(|| out = st_forward(&messages));

    // Assert
    assert_eq!(st_roles(&out), vec!["user"]);
    assert_eq!(seam_breakpoint_count(&out), 0);
    let line = events
        .iter()
        .find(|e| e.field("system_turn_breakpoints_dropped").is_some())
        .unwrap_or_else(|| panic!("no screen line: {events:?}"));
    assert_eq!(line.field("system_turn_breakpoints_dropped"), Some("1"));
    assert_eq!(line.field("system_turn_breakpoints_moved"), Some("0"));
}

/// A marker already on the anchor block wins over the moved one: the two
/// sat at the same prefix boundary, so they collapse into one breakpoint and
/// the count falls rather than rising past the cap.
#[test]
fn moved_breakpoint_collapses_into_one_already_on_the_anchor() {
    // Arrange
    let anchored = Message {
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
            text: "a".into(),
            citations: None,
            cache_control: Some(CacheControl::ephemeral_1h()),
        })]),
        ..st_user("")
    };
    let messages = vec![anchored, seam_blank_cached_turn(), st_assistant("ok")];

    // Act
    let out = st_forward(&messages);

    // Assert
    assert_eq!(seam_breakpoint_count(&out), 1);
    let AnthropicContent::Blocks(blocks) = &out[0].content else {
        panic!("expected block form: {:?}", out[0].content);
    };
    assert!(
        matches!(
            &blocks[..],
            [ContentBlock::Text { cache_control: Some(cc), .. }] if cc.effective_ttl() == "1h"
        ),
        "got {blocks:?}"
    );
}

fn seam_signed_reasoning_assistant(text: &str) -> Message {
    Message {
        reasoning_details: vec![ReasoningDetail {
            kind: ReasoningDetailKind::Text,
            id: Some("d0".into()),
            format: Some(ANTHROPIC_FORMAT.into()),
            index: Some(0),
            payload: serde_json::json!({"text": "plan", "signature": "sig"}),
        }],
        ..st_assistant(text)
    }
}

/// Wire type tag of every block in `msg`, in order; a string carrier reads
/// as one text block.
fn seam_block_kinds(msg: &AnthropicMessage) -> Vec<&'static str> {
    match &msg.content {
        AnthropicContent::Text(_) => vec!["text"],
        AnthropicContent::Blocks(blocks) => blocks
            .iter()
            .map(|b| match b {
                ContentBlock::Text { .. } => "text",
                ContentBlock::Image { .. } => "image",
                ContentBlock::Document { .. } => "document",
                ContentBlock::Thinking { .. } => "thinking",
                ContentBlock::RedactedThinking { .. } => "redacted_thinking",
                ContentBlock::ToolResult { .. } => "tool_result",
                ContentBlock::ToolUse { .. } => "tool_use",
                _ => "other",
            })
            .collect(),
    }
}

/// A merge that would need a reorder to be legal is skipped: a tool_result
/// run after a user text turn, and an assistant turn opening with signed
/// thinking after an assistant text turn, each stay two messages in their
/// original block order. Each skip is counted on the screen line, and the
/// ledger verify inside `translate_messages` would fail the call if a
/// skipped merge were charged as a merge.
#[test]
fn merge_across_a_dropped_turn_is_skipped_when_it_would_need_a_reorder() {
    let cases: [(&str, Message, Message, &str, Vec<&str>); 2] = [
        (
            "user text / empty / tool_result",
            st_user("prior text"),
            st_tool("call_1", "result"),
            "user",
            vec!["tool_result"],
        ),
        (
            "assistant text / empty / assistant thinking",
            st_assistant("old text"),
            seam_signed_reasoning_assistant("new text"),
            "assistant",
            vec!["thinking", "text"],
        ),
    ];
    for (name, before, after, role, after_kinds) in cases {
        // Arrange
        let messages = vec![before, st_system(MessageContent::Parts(vec![])), after];

        // Act
        let mut out = Vec::new();
        let events = capture_events(|| out = st_forward(&messages));

        // Assert
        assert_eq!(st_roles(&out), vec![role, role], "{name}");
        assert_eq!(seam_block_kinds(&out[0]), vec!["text"], "{name}");
        assert_eq!(seam_block_kinds(&out[1]), after_kinds, "{name}");
        let line = events
            .iter()
            .find(|e| e.field("system_turn_merges_skipped").is_some())
            .unwrap_or_else(|| panic!("{name}: no screen line: {events:?}"));
        assert_eq!(
            line.field("system_turn_merges_skipped"),
            Some("1"),
            "{name}"
        );
    }
}

/// Positive control for the skip: merges that stay legal without a reorder
/// still fire. A tool_result run joins a preceding tool_result-only user
/// message, a user text joins after a tool_result, and an assistant turn
/// whose thinking sits in the EARLIER message keeps it first.
#[test]
fn merge_across_a_dropped_turn_still_fires_when_order_stays_legal() {
    let cases: [(&str, Vec<Message>, &str, Vec<&str>); 3] = [
        (
            "tool_result / empty / tool_result",
            vec![
                st_tool("call_1", "one"),
                st_system(MessageContent::Parts(vec![])),
                st_tool("call_2", "two"),
            ],
            "user",
            vec!["tool_result", "tool_result"],
        ),
        (
            "tool_result / empty / user text",
            vec![
                st_tool("call_1", "one"),
                st_system(MessageContent::Parts(vec![])),
                st_user("follow-up"),
            ],
            "user",
            vec!["tool_result", "text"],
        ),
        (
            "assistant thinking / empty / assistant text",
            vec![
                seam_signed_reasoning_assistant("first"),
                st_system(MessageContent::Parts(vec![])),
                st_assistant("second"),
            ],
            "assistant",
            vec!["thinking", "text", "text"],
        ),
    ];
    for (name, messages, role, kinds) in cases {
        // Act
        let mut out = Vec::new();
        let events = capture_events(|| out = st_forward(&messages));

        // Assert
        assert_eq!(st_roles(&out), vec![role], "{name}");
        assert_eq!(seam_block_kinds(&out[0]), kinds, "{name}");
        let line = events
            .iter()
            .find(|e| e.field("system_turn_merges_skipped").is_some())
            .unwrap_or_else(|| panic!("{name}: no screen line: {events:?}"));
        assert_eq!(
            line.field("system_turn_merges_skipped"),
            Some("0"),
            "{name}"
        );
    }
}

/// A skipped merge still keeps the dropped turn's breakpoint: it lands on
/// the preceding message's last block, and the request carries exactly one.
#[test]
fn dropped_turn_breakpoint_moves_when_the_merge_is_skipped() {
    // Arrange
    let messages = vec![
        st_user("prior text"),
        seam_blank_cached_turn(),
        st_tool("call_1", "result"),
    ];

    // Act
    let out = st_forward(&messages);

    // Assert
    assert_eq!(st_roles(&out), vec!["user", "user"]);
    let AnthropicContent::Blocks(blocks) = &out[0].content else {
        panic!(
            "the anchor message must be in block form: {:?}",
            out[0].content
        );
    };
    assert!(
        matches!(
            &blocks[..],
            [ContentBlock::Text { text, cache_control: Some(_), .. }] if text == "prior text"
        ),
        "got {blocks:?}"
    );
    assert_eq!(seam_breakpoint_count(&out), 1);
}

fn seam_user_parts(parts: Vec<ContentPart>) -> Message {
    Message {
        content: MessageContent::Parts(parts),
        ..st_user("")
    }
}

fn seam_image_part() -> ContentPart {
    ContentPart::Known(KnownContentPart::Image {
        source: json!({"type": "base64", "media_type": "image/png", "data": "AAAA"}),
        cache_control: None,
    })
}

fn seam_encrypted_reasoning_assistant(text: &str) -> Message {
    Message {
        reasoning_details: vec![ReasoningDetail {
            kind: ReasoningDetailKind::Encrypted,
            id: Some("r0".into()),
            format: Some(ANTHROPIC_FORMAT.into()),
            index: Some(0),
            payload: serde_json::json!({"data": "opaque"}),
        }],
        ..st_assistant(text)
    }
}

fn seam_skipped_count(events: &[CapturedEvent]) -> Option<&str> {
    events
        .iter()
        .find_map(|e| e.field("system_turn_merges_skipped"))
}

/// A tool_result run after a dropped turn never merges into a preceding
/// user message holding anything but tool_result blocks, whatever carrier
/// that content arrived in: a block list of text, an image, or a document
/// (the string form is a row of the reorder table above). Translation succeeds (the ledger verify inside
/// `translate_messages` accepts the skip), the pair stays two messages, and
/// the tool_result leads the second.
#[test]
fn tool_result_after_a_dropped_turn_never_merges_behind_non_tool_result_content() {
    let cases: [(&str, Message, &str); 3] = [
        (
            "text block",
            seam_user_parts(vec![st_text_part("prior text")]),
            "text",
        ),
        (
            "image block",
            seam_user_parts(vec![seam_image_part()]),
            "image",
        ),
        (
            "document block",
            seam_user_parts(vec![st_document_text_part("doc body")]),
            "document",
        ),
    ];
    for (name, before, before_kind) in cases {
        // Arrange
        let messages = vec![
            before,
            st_system(MessageContent::Parts(vec![])),
            st_tool("call_1", "result"),
        ];

        // Act
        let mut out = Vec::new();
        let events = capture_events(|| out = st_forward(&messages));

        // Assert
        assert_eq!(st_roles(&out), vec!["user", "user"], "{name}");
        assert_eq!(seam_block_kinds(&out[0]), vec![before_kind], "{name}");
        assert_eq!(seam_block_kinds(&out[1]), vec!["tool_result"], "{name}");
        assert_eq!(seam_skipped_count(&events), Some("1"), "{name}");
    }
}

/// An assistant turn whose reasoning replays as `redacted_thinking` (an
/// encrypted detail) counts as opening with thinking: the merge into a
/// preceding assistant text turn is skipped, and the redacted block still
/// leads its own message.
#[test]
fn redacted_thinking_after_a_dropped_turn_skips_the_merge() {
    // Arrange
    let messages = vec![
        st_assistant("old text"),
        st_system(MessageContent::Parts(vec![])),
        seam_encrypted_reasoning_assistant("new text"),
    ];

    // Act
    let mut out = Vec::new();
    let events = capture_events(|| out = st_forward(&messages));

    // Assert
    assert_eq!(st_roles(&out), vec!["assistant", "assistant"]);
    assert_eq!(seam_block_kinds(&out[0]), vec!["text"]);
    assert_eq!(seam_block_kinds(&out[1]), vec!["redacted_thinking", "text"]);
    assert_eq!(seam_skipped_count(&events), Some("1"));
}

/// Two dropped turns in one history are each accounted on their own: the
/// first seam merges its user neighbors, the second is skipped because a
/// tool_result cannot follow that merged text, and the ledger verify inside
/// `translate_messages` balances both drops, the one merge and the one skip.
#[test]
fn two_dropped_turns_in_a_row_merge_then_skip_and_balance_the_ledger() {
    // Arrange
    let messages = vec![
        st_user("a"),
        st_system(MessageContent::Parts(vec![])),
        st_user("b"),
        st_system(MessageContent::Text(String::new())),
        st_tool("call_1", "result"),
    ];

    // Act
    let mut out = Vec::new();
    let events = capture_events(|| out = st_forward(&messages));

    // Assert
    assert_eq!(st_roles(&out), vec!["user", "user"]);
    assert_eq!(seam_texts(&out[0]), vec!["a", "b"]);
    assert_eq!(seam_block_kinds(&out[1]), vec!["tool_result"]);
    assert_eq!(seam_skipped_count(&events), Some("1"));
    let line = events
        .iter()
        .find(|e| e.field("system_turns_dropped_empty").is_some())
        .unwrap_or_else(|| panic!("no screen line: {events:?}"));
    assert_eq!(line.field("system_turns_dropped_empty"), Some("2"));
}

/// A dropped turn's breakpoint re-anchors on a tool_result block when the
/// preceding message holds nothing else.
#[test]
fn dropped_turn_breakpoint_moves_onto_a_preceding_tool_result() {
    // Arrange
    let caller = Message {
        tool_calls: Some(vec![json!({
            "id": "call_1",
            "type": "function",
            "function": {"name": "calc", "arguments": "{}"}
        })]),
        ..st_assistant("calling")
    };
    // Anthropic's prompt-caching docs list tool_result blocks as cacheable.
    let messages = vec![
        caller,
        st_tool("call_1", "42"),
        seam_blank_cached_turn(),
        st_assistant("done"),
    ];

    // Act
    let out = st_forward(&messages);

    // Assert
    assert_eq!(st_roles(&out), vec!["assistant", "user", "assistant"]);
    let AnthropicContent::Blocks(blocks) = &out[1].content else {
        panic!("expected block form: {:?}", out[1].content);
    };
    assert!(
        matches!(
            &blocks[..],
            [ContentBlock::ToolResult { tool_use_id, cache_control: Some(cc), .. }]
                if tool_use_id == "call_1" && *cc == CacheControl::ephemeral_5m()
        ),
        "got {blocks:?}"
    );
    assert_eq!(seam_breakpoint_count(&out), 1);
}

/// Push `user / dropped system / user` the way the walk does, returning the
/// list for inspection alongside the ledger it charged.
fn seam_join(wire: WireMessages) -> (WireMessages, MessageLedger) {
    let mut wire = wire;
    let mut ledger = MessageLedger::default();
    let user = |text: &str| AnthropicMessage {
        role: AnthropicRole::User,
        content: AnthropicContent::Text(text.into()),
    };
    wire.push(0, user("a"), &mut ledger);
    let _ = wire.drop_system_turn(None);
    wire.push(2, user("b"), &mut ledger);
    (wire, ledger)
}

#[test]
fn only_an_admitted_tap_tracks_the_origins_a_join_is_declared_from() {
    let unadmitted = routectl_core::wire_check::WireTap::default();
    let admitted = routectl_core::wire_check::WireTap::admitted();
    let rows = [
        ("no tap", None, false, vec![]),
        ("unadmitted tap", Some(&unadmitted), false, vec![]),
        ("admitted tap", Some(&admitted), true, vec![(0, 2)]),
    ];

    for (name, tap, tracks, joins) in rows {
        let (wire, ledger) = seam_join(WireMessages::for_tap(3, tap));

        assert_eq!(wire.origins.is_some(), tracks, "row {name}");
        assert_eq!(wire.merged, joins, "row {name}");
        assert_eq!(wire.messages.len(), 1, "row {name}");
        assert_eq!(ledger.merged_across_dropped_system_turn, 1, "row {name}");
    }
}
