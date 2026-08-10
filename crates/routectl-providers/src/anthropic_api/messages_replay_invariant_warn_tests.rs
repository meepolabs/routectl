// The two aggregated WARNs emitted by `normalize_replay_invariants`:
// exact counts, capped index samples, per-list truncation flags. Both
// lists are sized by the caller-controlled message count, so each one is
// bounded as it is collected. Imports live in the host
// `messages_tests.rs` -- do not add `use` lines here.

/// A Thinking part with no signature at all -- the shape the replay
/// invariant strips.
fn unsigned_thinking_part() -> ContentPart {
    ContentPart::Known(KnownContentPart::Thinking {
        thinking: "reasoning".into(),
        signature: None,
    })
}

fn text_part(text: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::Text {
        text: text.into(),
        citations: None,
        cache_control: None,
    })
}

fn assistant_msg(parts: Vec<ContentPart>) -> Message {
    Message {
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Parts(parts),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// An assistant turn that survives the strip: the unsigned Thinking goes,
/// the Text block keeps the message wire-serializable.
fn partially_stripped_msg() -> Message {
    assistant_msg(vec![text_part("answer"), unsigned_thinking_part()])
}

/// An assistant turn the strip empties completely: unsigned Thinking is
/// its only content, and there is no reasoning_details or tool_calls
/// fallback, so the whole turn is dropped.
fn wholly_dropped_msg() -> Message {
    assistant_msg(vec![unsigned_thinking_part()])
}

fn request_of(messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        model: "claude-sonnet-4".into(),
        messages: messages.into(),
        ..Default::default()
    }
}

fn normalize_capturing(req: &ChatRequest) -> Vec<CapturedEvent> {
    capture_events(|| {
        normalize_replay_invariants("prov-test", req, CoreHistoryReasoning::Strip)
            .expect("normalize succeeds");
    })
}

fn warn_containing<'a>(events: &'a [CapturedEvent], needle: &str) -> &'a CapturedEvent {
    let matches: Vec<_> = events
        .iter()
        .filter(|e| e.message.contains(needle))
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "exactly one WARN containing {needle:?} expected; got events: {events:?}"
    );
    let warn = matches[0];
    assert_eq!(warn.level, tracing::Level::WARN);
    warn
}

/// Split a `Debug`-rendered index list into its element strings so the
/// sample's length and contents can be asserted without pinning the
/// whole rendering byte-for-byte.
fn rendered_index_entries(rendered: &str) -> Vec<String> {
    let inner = rendered
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    if inner.is_empty() {
        return Vec::new();
    }
    inner.split(", ").map(|e| e.trim().to_string()).collect()
}

fn index_entries(warn: &CapturedEvent, field: &str) -> Vec<String> {
    rendered_index_entries(warn.field(field).unwrap_or_else(|| {
        panic!(
            "field `{field}` missing from WARN fields: {:?}",
            warn.fields
        )
    }))
}

const STRIP_WARN: &str = "stripping unsigned thinking blocks from outgoing request";
const DROPPED_TURN_WARN: &str = "dropping assistant turn(s) from outgoing request";

/// More affected messages than the log cap must still produce ONE WARN
/// whose block count and affected-message count stay exact while the
/// index list carries only a capped sample. `dropped_blocks` counts
/// BLOCKS, so the affected-message magnitude is carried by its own field
/// rather than being inferred from the sample's length.
#[test]
fn strip_warn_caps_affected_messages_and_keeps_the_count_exact() {
    // Arrange: 12 turns, each losing exactly one unsigned Thinking block.
    let count = MAX_LOGGED_DIAGNOSTIC_ITEMS + 4;
    let req = request_of((0..count).map(|_| partially_stripped_msg()).collect());

    // Act
    let events = normalize_capturing(&req);

    // Assert
    let warn = warn_containing(&events, STRIP_WARN);
    assert_eq!(
        warn.field("dropped_blocks"),
        Some(count.to_string().as_str()),
        "dropped_blocks must stay exact, uncapped"
    );
    assert_eq!(
        warn.field("affected_messages_count"),
        Some(count.to_string().as_str()),
        "affected_messages_count must stay exact, uncapped"
    );
    assert_eq!(
        warn.field("affected_messages_truncated"),
        Some("true"),
        "a capped sample must be flagged as truncated"
    );
    let entries = index_entries(warn, "affected_messages");
    assert_eq!(
        entries.len(),
        MAX_LOGGED_DIAGNOSTIC_ITEMS,
        "sample must be capped at {MAX_LOGGED_DIAGNOSTIC_ITEMS}; got: {entries:?}"
    );
    assert_eq!(
        entries,
        (0..MAX_LOGGED_DIAGNOSTIC_ITEMS)
            .map(|i| i.to_string())
            .collect::<Vec<_>>(),
        "the sample must be the first-seen prefix of the affected indices"
    );
}

/// More wholly dropped turns than the log cap must cap the index list
/// while `dropped_turns` keeps reporting the exact magnitude -- it must
/// never be read back off the capped sample.
#[test]
fn dropped_turn_warn_caps_indices_and_keeps_the_turn_count_exact() {
    // Arrange: 12 turns whose only content is unsigned Thinking.
    let count = MAX_LOGGED_DIAGNOSTIC_ITEMS + 4;
    let req = request_of((0..count).map(|_| wholly_dropped_msg()).collect());

    // Act
    let events = normalize_capturing(&req);

    // Assert
    let warn = warn_containing(&events, DROPPED_TURN_WARN);
    assert_eq!(
        warn.field("dropped_turns"),
        Some(count.to_string().as_str()),
        "dropped_turns must stay exact, not the capped sample's length"
    );
    assert_eq!(
        warn.field("dropped_message_indices_truncated"),
        Some("true"),
        "a capped sample must be flagged as truncated"
    );
    let entries = index_entries(warn, "dropped_message_indices");
    assert_eq!(
        entries.len(),
        MAX_LOGGED_DIAGNOSTIC_ITEMS,
        "sample must be capped at {MAX_LOGGED_DIAGNOSTIC_ITEMS}; got: {entries:?}"
    );
    assert_eq!(
        entries,
        (0..MAX_LOGGED_DIAGNOSTIC_ITEMS)
            .map(|i| i.to_string())
            .collect::<Vec<_>>(),
        "the sample must be the first-seen prefix of the dropped indices"
    );
}

/// At or below the cap, both samples are the complete lists and both
/// truncation flags must read `false` -- so each flag distinguishes a
/// sample from a whole list in BOTH directions.
#[test]
fn both_warns_keep_full_index_lists_when_within_cap() {
    // Arrange: 3 partially stripped turns then 3 wholly dropped ones,
    // well under the cap on both lists.
    let mut messages: Vec<Message> = (0..3).map(|_| partially_stripped_msg()).collect();
    messages.extend((0..3).map(|_| wholly_dropped_msg()));
    let req = request_of(messages);

    // Act
    let events = normalize_capturing(&req);

    // Assert: affected-message list is whole.
    let strip_warn = warn_containing(&events, STRIP_WARN);
    assert_eq!(strip_warn.field("dropped_blocks"), Some("6"));
    assert_eq!(strip_warn.field("affected_messages_count"), Some("6"));
    assert_eq!(
        strip_warn.field("affected_messages_truncated"),
        Some("false"),
        "an uncapped sample must not be flagged as truncated"
    );
    assert_eq!(
        index_entries(strip_warn, "affected_messages"),
        vec!["0", "1", "2", "3", "4", "5"],
        "every affected index must be present when under the cap"
    );

    // Assert: dropped-turn list is whole.
    let dropped_warn = warn_containing(&events, DROPPED_TURN_WARN);
    assert_eq!(dropped_warn.field("dropped_turns"), Some("3"));
    assert_eq!(
        dropped_warn.field("dropped_message_indices_truncated"),
        Some("false"),
        "an uncapped sample must not be flagged as truncated"
    );
    assert_eq!(
        index_entries(dropped_warn, "dropped_message_indices"),
        vec!["3", "4", "5"],
        "every dropped index must be present when under the cap"
    );
}
