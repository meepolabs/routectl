// Mid-conversation `Role::System` turns on the anthropic-api egress: the
// forward-vs-lift split, the billing/attribution screen on the forwarded
// path, the tool-run boundary, positional legality against the whole-turn
// drop, the accounted-identity ledger, and the empty-content reject.
// Imports live in the host `messages_tests.rs` -- do not add `use` lines
// here.

fn st_user(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Text(text.to_string()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn st_assistant(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Text(text.to_string()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn st_system(content: MessageContent) -> Message {
    Message {
        refusal: None,
        role: Role::System,
        content,
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn st_tool(id: &str, text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::Tool,
        content: MessageContent::Text(text.to_string()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: Some(id.to_string()),
        tool_calls: None,
    }
}

fn st_text_part(text: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::Text {
        text: text.to_string(),
        citations: None,
        cache_control: None,
    })
}

/// An unrecognized block tag that still carries a `text` field -- the shape
/// the Anthropic egress passes through verbatim.
fn st_other_text_part(type_tag: &str, text: &str) -> ContentPart {
    let mut extras = serde_json::Map::new();
    extras.insert("text".into(), Value::String(text.to_string()));
    ContentPart::Other {
        type_tag: type_tag.to_string(),
        cache_control: None,
        extras,
    }
}

/// A document block whose source carries its payload as inline text.
fn st_document_text_part(text: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::Document {
        source: json!({"type": "text", "media_type": "text/plain", "data": text}),
        title: None,
        citations: None,
        cache_control: None,
    })
}

fn st_roles(out: &[AnthropicMessage]) -> Vec<&'static str> {
    out.iter()
        .map(|m| match m.role {
            AnthropicRole::User => "user",
            AnthropicRole::Assistant => "assistant",
            AnthropicRole::System => "system",
        })
        .collect()
}

fn st_forward(messages: &[Message]) -> Vec<AnthropicMessage> {
    translate_messages(
        "anthropic",
        messages,
        SystemTurnPolicy::Forward,
        &mut passthrough_tally(),
    )
    .expect("translate")
}

/// Forward policy: a system turn between a user and an assistant turn
/// reaches the wire at its original position, with its content unchanged.
#[test]
fn forward_policy_emits_system_turn_in_place() {
    // Arrange
    let messages = vec![
        st_user("hi"),
        st_system(MessageContent::Text("mid-conversation note".into())),
        st_assistant("ok"),
    ];

    // Act
    let out = st_forward(&messages);

    // Assert
    assert_eq!(st_roles(&out), vec!["user", "system", "assistant"]);
    let AnthropicContent::Text(text) = &out[1].content else {
        panic!("expected Text content, got {:?}", out[1].content);
    };
    assert_eq!(text, "mid-conversation note");
}

/// Positive control for the test above: under the lift policy the same
/// input emits NO system turn, so the assertion there can actually fail.
#[test]
fn lift_policy_emits_no_system_turn() {
    // Arrange
    let messages = vec![
        st_user("hi"),
        st_system(MessageContent::Text("mid-conversation note".into())),
        st_assistant("ok"),
    ];

    // Act
    let out = translate_messages(
        "anthropic",
        &messages,
        SystemTurnPolicy::Lift,
        &mut passthrough_tally(),
    )
    .expect("translate");

    // Assert
    assert_eq!(st_roles(&out), vec!["user", "assistant"]);
}

/// The billing/attribution screen covers the forwarded path: a system turn
/// that is nothing but the Claude Code fingerprint never reaches the wire.
#[test]
fn forwarded_system_turn_carrying_only_billing_block_is_dropped() {
    // Arrange
    let messages = vec![
        st_user("hi"),
        st_system(MessageContent::Text(
            "x-anthropic-billing-header: v=1; fp=abc".into(),
        )),
        st_assistant("ok"),
    ];

    // Act
    let out = st_forward(&messages);

    // Assert
    assert_eq!(st_roles(&out), vec!["user", "assistant"]);
}

/// Paired positive control: a non-billing system turn survives the same
/// screen, and a mixed turn keeps its non-billing block.
#[test]
fn forwarded_system_turn_keeps_non_billing_blocks() {
    // Arrange
    let messages = vec![
        st_user("hi"),
        st_system(MessageContent::Parts(vec![
            st_text_part("x-anthropic-billing-header: v=1; fp=abc"),
            st_text_part("be concise"),
        ])),
        st_assistant("ok"),
    ];

    // Act
    let out = st_forward(&messages);

    // Assert
    assert_eq!(st_roles(&out), vec!["user", "system", "assistant"]);
    let AnthropicContent::Blocks(blocks) = &out[1].content else {
        panic!("expected Blocks content, got {:?}", out[1].content);
    };
    assert_eq!(blocks.len(), 1, "only the billing block is dropped");
    let ContentBlock::Text { text, .. } = &blocks[0] else {
        panic!("expected a Text block, got {:?}", blocks[0]);
    };
    assert_eq!(text, "be concise");
}

/// The screen is per-CARRIER, not per-known-type: a billing block wearing an
/// unrecognized `type` tag (which the egress would otherwise pass through
/// verbatim) is stripped like a typed text block.
#[test]
fn forwarded_system_turn_strips_billing_text_under_an_unrecognized_tag() {
    // Arrange
    let messages = vec![
        st_user("hi"),
        st_system(MessageContent::Parts(vec![
            st_other_text_part("Text", "x-anthropic-billing-header: v=1; fp=abc"),
            st_text_part("be concise"),
        ])),
        st_assistant("ok"),
    ];

    // Act
    let out = st_forward(&messages);

    // Assert
    assert_eq!(st_roles(&out), vec!["user", "system", "assistant"]);
    let AnthropicContent::Blocks(blocks) = &out[1].content else {
        panic!("expected Blocks content, got {:?}", out[1].content);
    };
    assert_eq!(blocks.len(), 1, "the odd-tag billing block is dropped");
    let ContentBlock::Text { text, .. } = &blocks[0] else {
        panic!("expected a Text block, got {:?}", blocks[0]);
    };
    assert_eq!(text, "be concise");
}

/// A document whose source carries the fingerprint inline is screened too.
#[test]
fn forwarded_system_turn_strips_billing_text_in_a_document_source() {
    // Arrange
    let messages = vec![
        st_user("hi"),
        st_system(MessageContent::Parts(vec![
            st_document_text_part("x-anthropic-billing-header: v=1; fp=abc"),
            st_text_part("be concise"),
        ])),
        st_assistant("ok"),
    ];

    // Act
    let out = st_forward(&messages);

    // Assert
    let AnthropicContent::Blocks(blocks) = &out[1].content else {
        panic!("expected Blocks content, got {:?}", out[1].content);
    };
    assert_eq!(blocks.len(), 1, "the billing document is dropped");
    let ContentBlock::Text { text, .. } = &blocks[0] else {
        panic!("expected a Text block, got {:?}", blocks[0]);
    };
    assert_eq!(text, "be concise");
}

/// Paired positive control for the two rejects above: the same carriers with
/// NON-billing text still forward, so the screen is not a blanket drop of
/// unrecognized or document blocks.
#[test]
fn forwarded_system_turn_keeps_non_billing_other_and_document_blocks() {
    // Arrange
    let messages = vec![
        st_user("hi"),
        st_system(MessageContent::Parts(vec![
            st_other_text_part("Text", "be concise"),
            st_document_text_part("reference material"),
        ])),
        st_assistant("ok"),
    ];

    // Act
    let out = st_forward(&messages);

    // Assert
    assert_eq!(st_roles(&out), vec!["user", "system", "assistant"]);
    let AnthropicContent::Blocks(blocks) = &out[1].content else {
        panic!("expected Blocks content, got {:?}", out[1].content);
    };
    assert_eq!(blocks.len(), 2, "both non-billing carriers forward");
}

/// A mixed turn that loses only its billing block is reported as loudly as
/// one removed wholesale: the surviving text forwards, the strip is counted
/// in blocks, and the WARN is one line for the request.
#[test]
fn partial_billing_strip_is_counted_and_warns_exactly_once() {
    // Arrange
    let messages = vec![
        st_user("hi"),
        st_system(MessageContent::Parts(vec![
            st_text_part("x-anthropic-billing-header: v=1; fp=abc"),
            st_text_part("be concise"),
        ])),
        st_assistant("ok"),
    ];

    // Act
    let events = capture_events(|| {
        let out = st_forward(&messages);
        assert_eq!(st_roles(&out), vec!["user", "system", "assistant"]);
    });

    // Assert
    let lines: Vec<&CapturedEvent> = events
        .iter()
        .filter(|e| e.field("system_blocks_stripped").is_some())
        .collect();
    assert_eq!(lines.len(), 1, "one WARN per request: {events:?}");
    assert_eq!(lines[0].level, tracing::Level::WARN);
    assert_eq!(lines[0].field("system_blocks_stripped"), Some("1"));
    assert_eq!(lines[0].field("system_turns_dropped"), Some("0"));
}

/// A forwarded turn whose every surviving text block is whitespace-only
/// would 400 upstream as an empty system turn, so it is the same local Err
/// as an empty one -- mirroring the canonical path's blank test.
#[test]
fn whitespace_only_forwarded_system_turn_errors_with_its_index() {
    // Arrange
    let messages = vec![
        st_user("hi"),
        st_system(MessageContent::Parts(vec![st_text_part("   ")])),
        st_assistant("ok"),
    ];

    // Act
    let err = translate_messages(
        "anthropic",
        &messages,
        SystemTurnPolicy::Forward,
        &mut passthrough_tally(),
    )
    .expect_err("must reject a blank system turn");

    // Assert
    let rendered = err.to_string();
    assert!(rendered.contains("messages[1]"), "got: {rendered}");
}

/// A system turn is a tool-run boundary: two tool results separated by one
/// stay in separate wire messages, and the system turn keeps its position
/// between them.
#[test]
fn system_turn_breaks_a_tool_run() {
    // Arrange
    let messages = vec![
        st_user("weather?"),
        st_assistant("looking"),
        st_tool("call_1", "sunny"),
        st_system(MessageContent::Text("note".into())),
        st_tool("call_2", "72F"),
    ];

    // Act
    let out = st_forward(&messages);

    // Assert
    assert_eq!(
        st_roles(&out),
        vec!["user", "assistant", "user", "system", "user"],
        "the tool results must not fold across the system turn"
    );
}

/// Positional legality: the unsigned-thinking whole-turn drop must not
/// strand a system turn. A wire system turn has to precede an assistant
/// turn or end the array, so the emptied assistant turn is KEPT and rides
/// through on the empty-content backstop instead.
#[test]
fn whole_turn_drop_is_refused_after_a_system_turn() {
    // Arrange
    let unsigned_thinking = ContentPart::Known(KnownContentPart::Thinking {
        thinking: "reasoning".into(),
        signature: None,
    });
    let req = ChatRequest {
        model: "claude-opus-4-8".into(),
        messages: vec![
            st_user("hi"),
            st_system(MessageContent::Text("note".into())),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![unsigned_thinking]),
                reasoning: None,
                reasoning_details: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            st_user("continue"),
        ]
        .into(),
        max_tokens: Some(1024),
        ..Default::default()
    };

    // Act
    let normalized = normalize_replay_invariants(
        "anthropic",
        &req,
        CoreHistoryReasoning::Auto,
        SystemTurnPolicy::Forward,
    )
    .expect("strip");
    let out = st_forward(&normalized);

    // Assert
    assert_eq!(
        st_roles(&out),
        vec!["user", "system", "assistant", "user"],
        "the assistant turn must survive so the system turn stays legal"
    );
    let AnthropicContent::Blocks(blocks) = &out[2].content else {
        panic!("expected Blocks content, got {:?}", out[2].content);
    };
    assert_eq!(blocks.len(), 1, "the backstop emits exactly one block");
    assert!(matches!(
        &blocks[0],
        ContentBlock::Text { text, .. } if text.is_empty()
    ));
}

/// Same drop with no system turn in front of it: the turn IS dropped.
/// Positive control proving the assertion above tests the system-turn
/// guard rather than a blanket refusal to drop.
#[test]
fn whole_turn_drop_still_fires_without_a_preceding_system_turn() {
    // Arrange
    let unsigned_thinking = ContentPart::Known(KnownContentPart::Thinking {
        thinking: "reasoning".into(),
        signature: None,
    });
    let req = ChatRequest {
        model: "claude-opus-4-8".into(),
        messages: vec![
            st_user("hi"),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![unsigned_thinking]),
                reasoning: None,
                reasoning_details: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            st_user("continue"),
        ]
        .into(),
        max_tokens: Some(1024),
        ..Default::default()
    };

    // Act
    let normalized = normalize_replay_invariants(
        "anthropic",
        &req,
        CoreHistoryReasoning::Auto,
        SystemTurnPolicy::Forward,
    )
    .expect("strip");
    let out = st_forward(&normalized);

    // Assert
    assert_eq!(st_roles(&out), vec!["user", "user"]);
}

/// The drop-refusal is lane-gated: on the Lift lane no system turn reaches
/// the wire, so the positional rationale does not apply and the emptied
/// assistant turn is dropped exactly as it was before forwarding existed.
#[test]
fn whole_turn_drop_still_fires_after_a_system_turn_on_the_lift_lane() {
    // Arrange
    let unsigned_thinking = ContentPart::Known(KnownContentPart::Thinking {
        thinking: "reasoning".into(),
        signature: None,
    });
    let req = ChatRequest {
        model: "claude-opus-4-8".into(),
        messages: vec![
            st_user("hi"),
            st_system(MessageContent::Text("note".into())),
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![unsigned_thinking]),
                reasoning: None,
                reasoning_details: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            st_user("continue"),
        ]
        .into(),
        max_tokens: Some(1024),
        ..Default::default()
    };

    // Act
    let normalized = normalize_replay_invariants(
        "anthropic",
        &req,
        CoreHistoryReasoning::Auto,
        SystemTurnPolicy::Lift,
    )
    .expect("strip");
    let out = translate_messages(
        "anthropic",
        &normalized,
        SystemTurnPolicy::Lift,
        &mut passthrough_tally(),
    )
    .expect("translate");

    // Assert
    assert_eq!(
        st_roles(&out),
        vec!["user", "user"],
        "the lift lane keeps its previous drop behavior"
    );
}

/// The accounted-identity ledger holds across a request mixing every
/// shape the walk handles: forwarded system turns, a billing-screened
/// system turn, a folded tool run, and plain user / assistant turns.
#[test]
fn ledger_accounts_for_every_lossy_point() {
    // Arrange
    let messages = vec![
        st_user("weather?"),
        st_system(MessageContent::Text("note".into())),
        st_assistant("looking"),
        st_tool("call_1", "sunny"),
        st_tool("call_2", "72F"),
        st_system(MessageContent::Text(
            "x-anthropic-billing-header: v=1; fp=abc".into(),
        )),
        st_assistant("done"),
    ];

    // Act
    let out = st_forward(&messages);

    // Assert
    assert_eq!(
        st_roles(&out),
        vec!["user", "system", "assistant", "user", "assistant"],
        "two tool turns fold into one message and the billing turn drops"
    );
}

/// Direct ledger coverage: a shortfall the walk cannot attribute to a
/// named lossy term is a hard error rather than a silent deletion.
#[test]
fn ledger_verify_rejects_an_unaccounted_shortfall() {
    // Arrange
    let ledger = MessageLedger {
        consumed: 4,
        ..MessageLedger::default()
    };

    // Act
    let err = ledger.verify("anthropic", 3).expect_err("must reject");

    // Assert
    let rendered = err.to_string();
    assert!(rendered.contains("emitted 3"), "got: {rendered}");
    assert!(rendered.contains("accounted for 4"), "got: {rendered}");
}

/// A forwarded system turn whose content translates to nothing is a local
/// error naming the message index: Anthropic rejects a system turn with no
/// content, and its error does not say which message.
#[test]
fn null_content_system_turn_on_the_forward_path_errors_with_its_index() {
    // Arrange
    let messages = vec![
        st_user("hi"),
        st_system(MessageContent::Null),
        st_assistant("ok"),
    ];

    // Act
    let err = translate_messages(
        "anthropic",
        &messages,
        SystemTurnPolicy::Forward,
        &mut passthrough_tally(),
    )
    .expect_err("must reject an empty system turn");

    // Assert
    let rendered = err.to_string();
    assert!(rendered.contains("messages[1]"), "got: {rendered}");
}

/// The same Null-content turn under the lift policy is not an error: the
/// lift consumed it and it never reaches the wire.
#[test]
fn null_content_system_turn_on_the_lift_path_is_not_an_error() {
    // Arrange
    let messages = vec![
        st_user("hi"),
        st_system(MessageContent::Null),
        st_assistant("ok"),
    ];

    // Act
    let out = translate_messages(
        "anthropic",
        &messages,
        SystemTurnPolicy::Lift,
        &mut passthrough_tally(),
    )
    .expect("translate");

    // Assert
    assert_eq!(st_roles(&out), vec!["user", "assistant"]);
}

/// One DEBUG line per request names how many system turns were forwarded;
/// no message content reaches it.
#[test]
fn forwarded_system_turns_emit_one_debug_line_with_the_count() {
    // Arrange
    let messages = vec![
        st_user("hi"),
        st_system(MessageContent::Text("secret note".into())),
        st_assistant("ok"),
        st_system(MessageContent::Text("another".into())),
        st_assistant("done"),
    ];

    // Act
    let events = capture_events(|| {
        let _ = st_forward(&messages);
    });

    // Assert
    let lines: Vec<&CapturedEvent> = events
        .iter()
        .filter(|e| e.field("system_turns_forwarded").is_some())
        .collect();
    assert_eq!(lines.len(), 1, "one line per request: {events:?}");
    assert_eq!(lines[0].field("system_turns_forwarded"), Some("2"));
    assert!(
        !events.iter().any(|e| format!("{e:?}").contains("secret")),
        "message content must never be logged: {events:?}"
    );
}
