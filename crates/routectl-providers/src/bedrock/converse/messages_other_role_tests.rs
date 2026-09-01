// `Role::Other` on the Converse egress: Converse only models `user` and
// `assistant`, so an unrecognized role forwards as `user` with one DEBUG
// naming the dropped tag. Imports live in the host `messages_tests.rs` --
// do not add `use` lines here.

fn other_role_turn(tag: &str, text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::Other(tag.to_string()),
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn run_translate(messages: &[Message]) -> (Result<Vec<ConverseMessage>>, Vec<CapturedEvent>) {
    let mut tally = CitationsDropTally::new(TEST_ID);
    let mut reasoning = ReasoningSkipTally::new(TEST_ID);
    let mut cc_tally = ToolResultCacheControlDropTally::new(TEST_ID);
    let mut content_drops = ContentDropTally::default();
    let mut out = None;
    let events = capture_events(|| {
        out = Some(translate_messages(
            TEST_ID,
            messages,
            &mut tally,
            &mut reasoning,
            &mut cc_tally,
            &mut content_drops,
        ));
    });
    (out.expect("closure always runs"), events)
}

/// An unrecognized role forwards its content as a `user` turn and emits
/// exactly one DEBUG naming the original tag.
#[test]
fn other_role_forwards_as_user_with_debug() {
    // Arrange
    let messages = vec![other_role_turn("narrator", "hello there")];

    // Act
    let (result, events) = run_translate(&messages);

    // Assert
    let translated = result.expect("translation must succeed");
    assert_eq!(translated.len(), 1, "the turn must survive translation");
    assert_eq!(
        translated[0].role, "user",
        "must forward as the closest legal role"
    );
    let debug_events: Vec<_> = events
        .iter()
        .filter(|e| e.level == tracing::Level::DEBUG && e.field("role") == Some("narrator"))
        .collect();
    assert_eq!(
        debug_events.len(),
        1,
        "exactly one DEBUG must name the dropped role tag, got: {events:?}"
    );
}

/// Sibling positive control: a recognized `Role::User` turn takes the
/// ordinary path and emits no such DEBUG, proving the assertion above
/// actually exercises the `Role::Other` arm rather than firing regardless
/// of role.
#[test]
fn known_user_role_emits_no_unrecognized_role_debug() {
    // Arrange
    let messages = vec![user_turn(vec![text_part("hello there")])];

    // Act
    let (result, events) = run_translate(&messages);

    // Assert
    let translated = result.expect("translation must succeed");
    assert_eq!(translated.len(), 1);
    assert_eq!(translated[0].role, "user");
    assert!(
        !events
            .iter()
            .any(|e| e.message.contains("unrecognized message role")),
        "a recognized role must not trip the unrecognized-role fallback, got: {events:?}"
    );
}
