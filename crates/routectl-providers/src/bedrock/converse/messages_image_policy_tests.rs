// The two-class content policy applied to every image-carrying path on the
// Converse egress: MALFORMED (the caller asked to send image bytes and named
// none) fails the request; UNREPRESENTABLE (a well-formed image this JSON
// wire cannot carry) keeps its warn-drop. Imports live in the host
// `messages_tests.rs` -- do not add `use` lines here.

/// A user turn carrying the given parts, plus nothing else.
fn user_turn(parts: Vec<ContentPart>) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(parts),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// A `Role::Tool` turn carrying the given parts. Routes through
/// `translate_part_for_tool_result` -> `image_source_to_tool_result`.
fn tool_turn(parts: Vec<ContentPart>) -> Message {
    Message {
        refusal: None,
        role: Role::Tool,
        content: MessageContent::Parts(parts),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: Some("toolu_X".into()),
        tool_calls: None,
    }
}

/// A `Role::Tool` turn whose tool_result content is the raw Anthropic-shape
/// array. Routes through `translate_tool_result_array_element`.
fn raw_tool_result_turn(content: Value) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ToolResult {
            tool_use_id: "tu_img".into(),
            content,
            is_error: None,
            cache_control: None,
        })]),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn image_part(source: Value) -> ContentPart {
    ContentPart::Known(KnownContentPart::Image {
        source,
        cache_control: None,
    })
}

fn image_url_part(image_url: Value) -> ContentPart {
    ContentPart::Known(KnownContentPart::ImageUrl {
        image_url,
        cache_control: None,
    })
}

fn text_part(text: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::Text {
        text: text.into(),
        citations: None,
        cache_control: None,
    })
}

/// The normalize-request detail for a translation expected to fail.
fn normalize_error_detail(messages: &[Message]) -> String {
    match build_messages(TEST_ID, messages) {
        Err(Error::NormalizeRequest(_, detail)) => detail,
        Err(other) => panic!("expected a NormalizeRequest error, got {other:?}"),
        Ok(blocks) => panic!("expected a malformed-image failure, got Ok({blocks:?})"),
    }
}

/// Assert that translating `messages` fails and the detail names `field`.
fn assert_malformed_naming(messages: &[Message], field: &str) {
    let detail = normalize_error_detail(messages);
    assert!(
        detail.contains(field),
        "the error must name the offending field `{field}`; got: {detail}"
    );
}

/// Every content block of the single surviving message.
fn only_message_blocks(messages: &[Message]) -> Vec<ConverseContentBlock> {
    let out = build_messages(TEST_ID, messages).expect("an unrepresentable image must not fail");
    assert_eq!(
        out.len(),
        1,
        "expected exactly one translated message, got: {out:?}"
    );
    out.into_iter().next().expect("one message").content
}

/// Assert the image dropped but the sibling anchor text survived, and the
/// operator-facing WARN carrying `needle` fired.
fn assert_warn_dropped(messages: &[Message], needle: &str) {
    let mut blocks = Vec::new();
    let events = capture_events(|| {
        blocks = only_message_blocks(messages);
    });
    assert!(
        !blocks
            .iter()
            .any(|b| matches!(b, ConverseContentBlock::Image { .. })),
        "an unrepresentable image must be dropped, got: {blocks:?}"
    );
    assert!(
        blocks
            .iter()
            .any(|b| matches!(b, ConverseContentBlock::Text { .. })),
        "the sibling anchor text must survive the drop, got: {blocks:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.level == tracing::Level::WARN && e.message.contains(needle)),
        "the drop must stay observable through its WARN `{needle}`; got: {events:?}"
    );
}

// ---------------------------------------------------------------------------
// MALFORMED -- the regression probe: an image that names no bytes
// ---------------------------------------------------------------------------

/// The core defect. A base64 image source whose `data` is present but empty
/// names no bytes at all, yet the pre-fix egress built
/// `source: {bytes: ""}` and shipped an EMPTY IMAGE upstream with HTTP 200,
/// no WARN and no drop: the caller believes the model saw an image, and it
/// saw nothing. Naming no bytes is malformed at every egress, so the request
/// must fail instead.
#[test]
fn image_source_with_empty_data_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![image_part(json!({
        "type": "base64",
        "media_type": "image/png",
        "data": "",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

/// The same defect through the absent-field door: `data` missing entirely
/// read as `""` and shipped as an empty image.
#[test]
fn image_source_with_absent_data_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![image_part(json!({
        "type": "base64",
        "media_type": "image/png",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

/// A non-string `data` names no bytes either -- the pre-fix `as_str()` fell
/// through to the same empty-image emit.
#[test]
fn image_source_with_non_string_data_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![image_part(json!({
        "type": "base64",
        "media_type": "image/png",
        "data": 42,
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

/// The tool-result carrier of the identical defect: the same empty-`data`
/// source reached AWS as `{image: {source: {bytes: ""}}}` inside a
/// toolResult.
#[test]
fn tool_result_image_with_empty_data_fails_the_request() {
    // Arrange
    let messages = vec![tool_turn(vec![image_part(json!({
        "type": "base64",
        "media_type": "image/png",
        "data": "",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

/// The tool-result carrier, absent-field door.
#[test]
fn tool_result_image_with_absent_data_fails_the_request() {
    // Arrange
    let messages = vec![tool_turn(vec![image_part(json!({
        "type": "base64",
        "media_type": "image/png",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

/// The raw Anthropic-shape tool_result content array is a third carrier of
/// the same source shape, and shipped the same empty image.
#[test]
fn raw_tool_result_image_with_empty_data_fails_the_request() {
    // Arrange
    let messages = vec![raw_tool_result_turn(json!([{
        "type": "image",
        "source": {"type": "base64", "media_type": "image/png", "data": ""},
    }]))];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

/// An `image_url` data URI declaring base64 with an empty payload is the
/// same "asked to send bytes, named none" shape reached through the
/// OpenAI-shape part: the pre-fix parse split cleanly and emitted
/// `bytes: ""`.
#[test]
fn image_url_data_uri_with_empty_payload_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![image_url_part(
        json!({"url": "data:image/png;base64,"}),
    )])];

    // Act / Assert
    assert_malformed_naming(&messages, "image_url.url");
}

// ---------------------------------------------------------------------------
// MALFORMED -- the remaining structurally provable cases
// ---------------------------------------------------------------------------

/// A `source` that is not a JSON object carries no field to read.
#[test]
fn image_source_that_is_not_an_object_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![image_part(json!("not-an-object"))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source");
}

/// An absent `source.type` leaves the source shape unnamed.
#[test]
fn image_source_with_absent_type_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![image_part(json!({
        "media_type": "image/png",
        "data": "AAAA",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.type");
}

/// An empty `source.type` is the absent case wearing a string.
#[test]
fn image_source_with_empty_type_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![image_part(json!({
        "type": "",
        "media_type": "image/png",
        "data": "AAAA",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.type");
}

/// A base64 source with no `media_type` cannot name an AWS image format,
/// and the field is required rather than defaultable: guessing a format
/// ships bytes the model decodes as the wrong thing.
#[test]
fn base64_image_source_with_absent_media_type_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![image_part(json!({
        "type": "base64",
        "data": "AAAA",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.media_type");
}

/// An empty `media_type` on a base64 source, same class as absent.
#[test]
fn base64_image_source_with_empty_media_type_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![image_part(json!({
        "type": "base64",
        "media_type": "",
        "data": "AAAA",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.media_type");
}

/// A url-shape source whose `url` is empty names neither bytes nor a
/// location. It is the direct analogue of the empty `image_url.url` case
/// and fails for the same reason -- the nonempty url-shape source is the
/// one that stays a warn-drop.
#[test]
fn url_shape_image_source_with_empty_url_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![image_part(json!({
        "type": "url",
        "url": "",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.url");
}

/// An `image_url` part with no url at all.
#[test]
fn image_url_with_absent_url_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![image_url_part(json!({}))])];

    // Act / Assert
    assert_malformed_naming(&messages, "image_url.url");
}

/// An `image_url` part with an empty url.
#[test]
fn image_url_with_empty_url_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![image_url_part(json!({"url": ""}))])];

    // Act / Assert
    assert_malformed_naming(&messages, "image_url.url");
}

/// Required-field STRUCTURE is validated before representability. An
/// empty-`data` part whose `media_type` is also unmapped must fail as
/// malformed rather than disappearing into the unsupported-media warn-drop
/// -- the caller's request is broken, and reporting it as "this route
/// cannot carry TIFF" would send them fixing the wrong thing.
#[test]
fn empty_data_outranks_an_unmapped_media_type() {
    // Arrange
    let messages = vec![user_turn(vec![image_part(json!({
        "type": "base64",
        "media_type": "image/tiff",
        "data": "",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

// ---------------------------------------------------------------------------
// UNREPRESENTABLE -- the warn-drops that must NOT become errors
// ---------------------------------------------------------------------------

/// A nonempty url-shape source is a well-formed image the Converse JSON
/// wire simply cannot carry. It keeps its warn-drop: the request is
/// legitimate and the rest of the turn still ships.
#[test]
fn nonempty_url_shape_image_source_still_warn_drops() {
    // Arrange
    let messages = vec![user_turn(vec![
        text_part("anchor"),
        image_part(json!({"type": "url", "url": "https://example.com/x.png"})),
    ])];

    // Act / Assert
    assert_warn_dropped(
        &messages,
        "dropping non-base64 image source on Converse egress",
    );
}

/// An unknown but NONEMPTY source `type` may be a valid vendor shape this
/// build has not learned yet. Tightening it to an error would 400 working
/// traffic the day a vendor ships one, so the forward-compat default is
/// pinned here: it must stay a warn-drop.
#[test]
fn unknown_nonempty_image_source_type_still_warn_drops() {
    // Arrange
    let messages = vec![user_turn(vec![
        text_part("anchor"),
        image_part(json!({
            "type": "some-future-source-kind",
            "media_type": "image/png",
            "data": "AAAA",
        })),
    ])];

    // Act / Assert
    assert_warn_dropped(
        &messages,
        "dropping non-base64 image source on Converse egress",
    );
}

/// A structurally complete source whose `media_type` AWS does not model is
/// unrepresentable, not malformed -- AWS's format table is bounded and the
/// caller did nothing wrong.
#[test]
fn structurally_complete_unmapped_media_type_still_warn_drops() {
    // Arrange
    let messages = vec![user_turn(vec![
        text_part("anchor"),
        image_part(json!({
            "type": "base64",
            "media_type": "image/tiff",
            "data": "AAAA",
        })),
    ])];

    // Act / Assert
    assert_warn_dropped(
        &messages,
        "dropping image with unmapped media_type on Converse egress",
    );
}

/// A well-formed non-data-URI image reference is a location this JSON wire
/// cannot dereference. Unchanged warn-drop.
#[test]
fn non_data_uri_image_url_still_warn_drops() {
    // Arrange
    let messages = vec![user_turn(vec![
        text_part("anchor"),
        image_url_part(json!({"url": "https://example.com/x.png"})),
    ])];

    // Act / Assert
    assert_warn_dropped(
        &messages,
        "dropping image_url on Converse egress; only base64 data URIs are supported",
    );
}

/// On the raw tool_result array a url-shape image source keeps the JSON
/// fallback -- the "cannot represent" answer for that path. It must not be
/// read as a base64 source with missing bytes.
#[test]
fn raw_tool_result_url_shape_image_source_keeps_the_json_fallback() {
    // Arrange: a mappable media_type alongside the url shape is what made
    // the pre-fix code emit `bytes: ""` here.
    let messages = vec![raw_tool_result_turn(json!([{
        "type": "image",
        "source": {
            "type": "url",
            "url": "https://example.com/x.png",
            "media_type": "image/png",
        },
    }]))];

    // Act
    let blocks = only_message_blocks(&messages);

    // Assert
    let ConverseContentBlock::ToolResult { tool_result } =
        blocks.first().expect("a toolResult block")
    else {
        panic!("expected a toolResult block, got: {blocks:?}");
    };
    assert!(
        matches!(
            tool_result.content.first(),
            Some(ConverseToolResultContent::Json { .. })
        ),
        "a url-shape source must fall back to the JSON wrap, got: {:?}",
        tool_result.content
    );
}

// ---------------------------------------------------------------------------
// Atomicity and diagnostics
// ---------------------------------------------------------------------------

/// A malformed image AFTER a valid one fails the whole normalization.
/// Nothing dispatches upstream: shipping the remaining parts and returning
/// 200 is exactly the silent content loss being fixed.
#[test]
fn valid_image_then_malformed_image_fails_the_whole_request() {
    // Arrange
    let messages = vec![user_turn(vec![
        image_part(json!({"type": "base64", "media_type": "image/png", "data": "AAAA"})),
        image_part(json!({"type": "base64", "media_type": "image/png", "data": ""})),
    ])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

/// The reverse ordering fails identically -- the failure cannot depend on
/// where in the turn the malformed part sits.
#[test]
fn malformed_image_then_valid_image_fails_the_whole_request() {
    // Arrange
    let messages = vec![user_turn(vec![
        image_part(json!({"type": "base64", "media_type": "image/png", "data": ""})),
        image_part(json!({"type": "base64", "media_type": "image/png", "data": "AAAA"})),
    ])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

/// `Error::NormalizeRequest` reaches the ingress as a 400 whose detail is
/// logged server-side, so the detail must name the offending FIELD and
/// never echo a caller-controlled VALUE -- payload bytes, media types and
/// urls are caller data.
#[test]
fn malformed_image_errors_never_echo_a_caller_value() {
    // Arrange: each case carries a distinctive caller value in a field the
    // error is NOT about, so an echo is unambiguous.
    let empty_data = vec![user_turn(vec![image_part(json!({
        "type": "base64",
        "media_type": "image/sentinel-media-type",
        "data": "",
    }))])];
    let absent_media_type = vec![user_turn(vec![image_part(json!({
        "type": "base64",
        "data": "sentinel-payload-bytes",
    }))])];
    let bad_url = vec![user_turn(vec![image_url_part(json!({
        "url": "",
        "detail": "sentinel-detail-value",
    }))])];

    // Act / Assert
    for (messages, leak) in [
        (empty_data, "image/sentinel-media-type"),
        (absent_media_type, "sentinel-payload-bytes"),
        (bad_url, "sentinel-detail-value"),
    ] {
        let detail = normalize_error_detail(&messages);
        assert!(
            !detail.contains(leak),
            "the error detail must not echo the caller value `{leak}`; got: {detail}"
        );
    }
}

/// Both per-request tallies are flushed on the error arm. A citation drop
/// and an unsigned-reasoning skip recorded BEFORE a malformed image are
/// aggregate WARNs the operator is owed regardless of how the request
/// ends; without a flush on the `Err` path both are silently swallowed and
/// only the translation failure surfaces.
#[test]
fn tallies_recorded_before_a_malformed_image_still_flush_on_the_error_path() {
    // Arrange: turn 0 drops a citations value, turn 1 skips an unsigned
    // reasoning detail, turn 2 carries the malformed image.
    let messages = vec![
        user_turn(vec![ContentPart::Known(KnownContentPart::Document {
            source: json!({"type": "text", "media_type": "text/plain", "data": "notes"}),
            title: Some("notes".into()),
            citations: Some(json!("yes")),
            cache_control: None,
        })]),
        assistant_turn(vec![unsigned_detail(Some(0))]),
        user_turn(vec![image_part(json!({
            "type": "base64",
            "media_type": "image/png",
            "data": "",
        }))]),
    ];

    // Act
    let mut result_is_err = false;
    let events = capture_events(|| {
        result_is_err = build_messages(TEST_ID, &messages).is_err();
    });

    // Assert
    assert!(result_is_err, "the malformed image must fail the request");
    let citations_warn = events
        .iter()
        .any(|e| e.level == tracing::Level::WARN && e.field("dropped_count") == Some("1"));
    assert!(
        citations_warn,
        "the citations drop recorded before the error must still reach the operator; got: {events:?}"
    );
    let reasoning_warn = events
        .iter()
        .any(|e| e.level == tracing::Level::WARN && e.field("skipped_count") == Some("1"));
    assert!(
        reasoning_warn,
        "the reasoning skip recorded before the error must still reach the operator; got: {events:?}"
    );
}
