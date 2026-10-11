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
