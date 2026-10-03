// A `cache_control` marker on a canonical part nested inside a `Role::Tool`
// turn's `Parts` is carried as ONE sibling `cachePoint` right after the
// synthesized `toolResult` block: `toolResult.content` has no slot a nested
// marker caches from, and several adjacent cachePoints after one block write
// a single cache entry, so one sibling carries every marker on the turn.
// Imports live in the host `messages_tests.rs` -- do not add `use` lines
// here. Shared turn builders come from the image-policy fragment.

/// A text part carrying the given `cache_control`, otherwise identical to
/// `text_part`.
fn text_part_with_cache_control(text: &str, cache_control: Option<CacheControl>) -> ContentPart {
    ContentPart::Known(KnownContentPart::Text {
        text: text.into(),
        citations: None,
        cache_control,
    })
}

/// A marker with an explicit TTL spelling.
fn marker_with_ttl(ttl: &str) -> CacheControl {
    CacheControl {
        kind: "ephemeral".into(),
        ttl: Some(ttl.into()),
    }
}

/// The TTL of every `cachePoint` block in `blocks`, in emission order.
fn cache_point_ttls(blocks: &[ConverseContentBlock]) -> Vec<Option<String>> {
    blocks
        .iter()
        .filter_map(|b| match b {
            ConverseContentBlock::CachePoint { cache_point } => Some(cache_point.ttl.clone()),
            _ => None,
        })
        .collect()
}

/// Every block of every translated message, flattened in wire order.
fn all_wire_blocks(messages: &[Message]) -> Vec<ConverseContentBlock> {
    build_messages(TEST_ID, messages)
        .expect("the request must translate")
        .into_iter()
        .flat_map(|m| m.content)
        .collect()
}

/// Panic unless every `cachePoint` in `blocks` directly follows a content
/// block: AWS rejects a cachePoint with no preceding block.
fn assert_no_orphan_cache_point(blocks: &[ConverseContentBlock]) {
    for (i, b) in blocks.iter().enumerate() {
        if matches!(b, ConverseContentBlock::CachePoint { .. }) {
            assert!(
                i > 0 && !matches!(blocks[i - 1], ConverseContentBlock::CachePoint { .. }),
                "cachePoint at index {i} has no preceding content block: {blocks:?}"
            );
        }
    }
}

#[test]
fn one_marked_tool_result_part_emits_tool_result_then_cache_point_with_its_ttl() {
    // Arrange
    let messages = vec![tool_turn(vec![text_part_with_cache_control(
        "result body",
        Some(CacheControl::ephemeral_1h()),
    )])];

    // Act
    let blocks = only_message_blocks(&messages);

    // Assert
    assert_eq!(
        blocks.len(),
        2,
        "expected [toolResult, cachePoint], got: {blocks:?}"
    );
    match &blocks[0] {
        ConverseContentBlock::ToolResult { tool_result } => assert!(
            matches!(
                tool_result.content.as_slice(),
                [ConverseToolResultContent::Text { text }] if text == "result body"
            ),
            "the marked text must still translate, got: {:?}",
            tool_result.content
        ),
        other => panic!("expected the toolResult first, got: {other:?}"),
    }
    let wire = serde_json::to_value(&blocks[1]).expect("cachePoint serializes");
    assert_eq!(
        wire,
        json!({"cachePoint": {"type": "default", "ttl": "1h"}}),
        "the sibling must carry the marker's TTL"
    );
}

#[test]
fn a_marker_without_ttl_emits_the_five_minute_wire_default() {
    // Arrange
    let marker = CacheControl {
        kind: "ephemeral".into(),
        ttl: None,
    };
    let messages = vec![tool_turn(vec![text_part_with_cache_control(
        "r",
        Some(marker),
    )])];

    // Act
    let blocks = only_message_blocks(&messages);

    // Assert
    assert_eq!(cache_point_ttls(&blocks), vec![Some("5m".to_string())]);
}

#[test]
fn several_marked_parts_in_one_tool_result_emit_exactly_one_cache_point() {
    // Arrange
    let messages = vec![tool_turn(vec![
        text_part_with_cache_control("a", Some(CacheControl::ephemeral_5m())),
        text_part_with_cache_control("b", Some(CacheControl::ephemeral_5m())),
        text_part_with_cache_control("c", None),
        text_part_with_cache_control("d", Some(CacheControl::ephemeral_5m())),
    ])];

    // Act
    let blocks = only_message_blocks(&messages);

    // Assert
    assert_eq!(
        blocks.len(),
        2,
        "expected [toolResult, cachePoint], got: {blocks:?}"
    );
    assert!(matches!(blocks[0], ConverseContentBlock::ToolResult { .. }));
    assert_eq!(cache_point_ttls(&blocks), vec![Some("5m".to_string())]);
}

#[test]
fn mixed_ttls_on_one_tool_result_resolve_to_the_longest() {
    // Arrange -- 1h before 5m is the only order the TTL validator admits.
    let messages = vec![tool_turn(vec![
        text_part_with_cache_control("a", Some(CacheControl::ephemeral_1h())),
        text_part_with_cache_control("b", Some(CacheControl::ephemeral_5m())),
    ])];

    // Act
    let blocks = only_message_blocks(&messages);

    // Assert
    assert_eq!(cache_point_ttls(&blocks), vec![Some("1h".to_string())]);
}

#[test]
fn an_unrecognized_ttl_outranks_five_minutes_and_forwards_verbatim() {
    // Arrange -- 5m FIRST: the validator admits this order, and it is the
    // one where "longest" and "first" disagree.
    let messages = vec![tool_turn(vec![
        text_part_with_cache_control("a", Some(CacheControl::ephemeral_5m())),
        text_part_with_cache_control("b", Some(marker_with_ttl("forever"))),
    ])];

    // Act
    let blocks = only_message_blocks(&messages);

    // Assert
    assert_eq!(cache_point_ttls(&blocks), vec![Some("forever".to_string())]);
}

#[test]
fn an_unmarked_tool_result_emits_no_cache_point() {
    // Arrange
    let messages = vec![tool_turn(vec![text_part_with_cache_control("r", None)])];

    // Act
    let blocks = only_message_blocks(&messages);

    // Assert
    assert_eq!(
        blocks.len(),
        1,
        "expected only the toolResult, got: {blocks:?}"
    );
    assert!(matches!(blocks[0], ConverseContentBlock::ToolResult { .. }));
}

#[test]
fn a_carried_tool_result_marker_does_not_warn() {
    // Arrange
    let messages = vec![tool_turn(vec![text_part_with_cache_control(
        "r",
        Some(CacheControl::ephemeral_5m()),
    )])];

    // Act
    let mut blocks = Vec::new();
    let events = capture_events(|| {
        blocks = only_message_blocks(&messages);
    });

    // Assert
    assert_eq!(
        cache_point_ttls(&blocks).len(),
        1,
        "premise: the marker is carried"
    );
    assert!(
        !events.iter().any(|e| e.level == tracing::Level::WARN),
        "a marker that reaches the wire must not be reported as dropped, got: {events:?}"
    );
}

/// A part with no typed toolResult carrier rides as an opaque JSON payload.
/// The marker anchors on the enclosing toolResult instead, never inside the
/// payload, so the wire carries it exactly once.
#[test]
fn a_fallback_wrapped_marked_part_anchors_on_the_tool_result_not_in_its_payload() {
    // Arrange -- a tool_use-shaped part has no toolResult carrier.
    let marked: ContentPart = serde_json::from_value(json!({
        "type": "tool_use",
        "id": "tu_1",
        "name": "probe",
        "input": {},
        "cache_control": {"type": "ephemeral"}
    }))
    .expect("a tool_use part with a marker");
    let messages = vec![tool_turn(vec![marked])];

    // Act
    let blocks = only_message_blocks(&messages);

    // Assert
    assert_eq!(
        blocks.len(),
        2,
        "expected [toolResult, cachePoint], got: {blocks:?}"
    );
    let ConverseContentBlock::ToolResult { tool_result } = &blocks[0] else {
        panic!("expected the toolResult first, got: {blocks:?}");
    };
    let json = tool_result
        .content
        .iter()
        .find_map(|c| match c {
            ConverseToolResultContent::Json { json } => Some(json),
            _ => None,
        })
        .expect("the part must land as a Json payload");
    assert!(
        json.get("cache_control").is_none(),
        "the marker must not also ride inside the opaque payload, got: {json}"
    );
    assert_eq!(json.get("name").and_then(Value::as_str), Some("probe"));
    assert_eq!(cache_point_ttls(&blocks), vec![Some("5m".to_string())]);
}

/// The only drop arm on a Role::Tool-adjacent turn is the plain user path:
/// a dropped marked block must not leave a cachePoint behind, even when it
/// coalesces into the same wire message as a carried tool-result marker.
///
/// Guarded on the image_url drop class it reaches incidentally: the dropped
/// block bumps the process-global counter, which would otherwise land inside
/// another test's before/after window for that class.
#[test]
#[serial_test::serial(bedrock_converse_image_url_unrepresentable)]
fn a_dropped_marked_block_beside_a_tool_result_leaves_no_orphan_cache_point() {
    // Arrange -- the image_url drops on Converse; the tool turn and the user
    // turn coalesce into one wire user message.
    let messages = vec![
        tool_turn(vec![text_part_with_cache_control(
            "r",
            Some(CacheControl::ephemeral_5m()),
        )]),
        user_turn(vec![ContentPart::Known(KnownContentPart::ImageUrl {
            image_url: json!({"url": "https://example.com/x.png"}),
            cache_control: Some(CacheControl::ephemeral_5m()),
        })]),
    ];

    // Act
    let blocks = all_wire_blocks(&messages);

    // Assert
    assert_eq!(
        blocks.len(),
        2,
        "expected [toolResult, cachePoint], got: {blocks:?}"
    );
    assert!(matches!(blocks[0], ConverseContentBlock::ToolResult { .. }));
    assert!(matches!(blocks[1], ConverseContentBlock::CachePoint { .. }));
}

/// At exactly the 4-breakpoint cap, the wire must never carry more
/// cachePoints than the canonical walk charged. Dense: every message
/// carrier that charges a marker appears, and the assertion counts.
#[test]
fn a_request_at_the_four_marker_cap_never_emits_more_than_four_cache_points() {
    // Arrange -- 4 charged markers: two on one tool result, one on a
    // second tool result, one on a user text.
    let messages = vec![
        user_turn(vec![text_part_with_cache_control(
            "q",
            Some(CacheControl::ephemeral_5m()),
        )]),
        tool_turn(vec![
            text_part_with_cache_control("a", Some(CacheControl::ephemeral_5m())),
            text_part_with_cache_control("b", Some(CacheControl::ephemeral_5m())),
        ]),
        tool_turn(vec![text_part_with_cache_control(
            "c",
            Some(CacheControl::ephemeral_5m()),
        )]),
    ];
    let req = routectl_core::ChatRequest {
        messages: messages.clone().into(),
        ..Default::default()
    };
    let charged = routectl_core::CacheBreakpointSource::cache_breakpoints(&req).len();
    assert_eq!(charged, 4, "premise: the fixture charges exactly the cap");
    routectl_core::cache_control::validate_source(&req).expect("4 markers are within the cap");

    // Act
    let blocks = all_wire_blocks(&messages);

    // Assert
    let wire = cache_point_ttls(&blocks).len();
    assert!(
        wire <= charged,
        "wire cachePoints {wire} exceed charged {charged}"
    );
    assert_eq!(
        wire, 3,
        "user text 1 + first tool result 1 + second 1, got: {blocks:?}"
    );
    assert_no_orphan_cache_point(&blocks);
}

/// A marker nested inside raw Anthropic-shape `tool_result` JSON is not
/// charged against the cap, so it must not be carried either: carrying it
/// could push a cap-valid request past the 4 cachePoints AWS enforces.
#[test]
fn a_marker_nested_in_raw_tool_result_json_is_not_carried() {
    // Arrange
    let nested = json!([
        {"type": "text", "text": "t", "cache_control": {"type": "ephemeral"}},
        {"type": "custom", "x": 1, "cache_control": {"type": "ephemeral"}}
    ]);
    let messages = vec![raw_tool_result_turn(nested)];
    let req = routectl_core::ChatRequest {
        messages: messages.clone().into(),
        ..Default::default()
    };
    assert_eq!(
        routectl_core::CacheBreakpointSource::cache_breakpoints(&req).len(),
        0,
        "premise: nested markers are not charged"
    );

    // Act
    let blocks = only_message_blocks(&messages);

    // Assert
    assert_eq!(
        blocks.len(),
        1,
        "expected only the toolResult, got: {blocks:?}"
    );
    let ConverseContentBlock::ToolResult { tool_result } = &blocks[0] else {
        panic!("expected a toolResult, got: {blocks:?}");
    };
    let wire = serde_json::to_value(&tool_result.content).expect("content serializes");
    assert_eq!(
        wire,
        json!([
            {"text": "t"},
            {"json": {"type": "custom", "x": 1, "cache_control": {"type": "ephemeral"}}}
        ]),
        "the nested content must translate exactly as before"
    );
}
