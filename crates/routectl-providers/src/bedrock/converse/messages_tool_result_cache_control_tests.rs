// A `cache_control` marker carried on a canonical part nested inside a
// `Role::Tool` turn's `Parts` has no wire slot to land on once that part
// becomes a `toolResult.content` element: unlike a top-level message
// content block, `toolResult.content` defines no sibling `cachePoint`
// member at that nesting depth. The drop is deliberate; this pair pins
// that it is also observable and scoped to only the marked part. Imports
// live in the host `messages_tests.rs` -- do not add `use` lines here.
// Shared turn builders come from the image-policy fragment.

/// A text part carrying the given `cache_control`, otherwise identical to
/// `text_part`.
fn text_part_with_cache_control(text: &str, cache_control: Option<CacheControl>) -> ContentPart {
    ContentPart::Known(KnownContentPart::Text {
        text: text.into(),
        citations: None,
        cache_control,
    })
}

/// The tool-result content elements of the single surviving message's sole
/// `ToolResult` block.
fn only_tool_result_content(messages: &[Message]) -> Vec<ConverseToolResultContent> {
    let blocks = only_message_blocks(messages);
    assert_eq!(
        blocks.len(),
        1,
        "expected exactly one content block, got: {blocks:?}"
    );
    match blocks.into_iter().next().expect("one block") {
        ConverseContentBlock::ToolResult { tool_result } => tool_result.content,
        other => panic!("expected a ToolResult block, got: {other:?}"),
    }
}

/// NEGATIVE CONTROL: a nested `cache_control` marker on a tool-result part
/// drops silently from the wire but must still surface through the
/// aggregated WARN, and the text it was attached to must still translate.
#[test]
fn nested_tool_result_cache_control_marker_drops_and_warns() {
    // Arrange
    let messages = vec![tool_turn(vec![text_part_with_cache_control(
        "result body",
        Some(CacheControl::ephemeral_5m()),
    )])];

    // Act
    let mut content = Vec::new();
    let events = capture_events(|| {
        content = only_tool_result_content(&messages);
    });

    // Assert
    assert!(
        content.iter().any(
            |c| matches!(c, ConverseToolResultContent::Text { text } if text == "result body")
        ),
        "the marked text must still translate despite the dropped marker, got: {content:?}"
    );
    assert!(
        events.iter().any(|e| e.level == tracing::Level::WARN
            && e.message.contains("cache_control")
            && e.message.contains("toolResult.content")),
        "the drop must stay observable through its WARN, got: {events:?}"
    );
}

/// POSITIVE CONTROL: a sibling tool-result part with no `cache_control`
/// marker must translate with no WARN at all -- proving the fixture above
/// would have surfaced a WARN that was not actually tied to the marker.
#[test]
fn nested_tool_result_without_cache_control_does_not_warn() {
    // Arrange
    let messages = vec![tool_turn(vec![text_part_with_cache_control(
        "result body",
        None,
    )])];

    // Act
    let mut content = Vec::new();
    let events = capture_events(|| {
        content = only_tool_result_content(&messages);
    });

    // Assert
    assert!(
        content.iter().any(
            |c| matches!(c, ConverseToolResultContent::Text { text } if text == "result body")
        ),
        "the unmarked text must translate unchanged, got: {content:?}"
    );
    assert!(
        !events.iter().any(|e| e.level == tracing::Level::WARN),
        "an unmarked tool-result part must not warn at all, got: {events:?}"
    );
}
