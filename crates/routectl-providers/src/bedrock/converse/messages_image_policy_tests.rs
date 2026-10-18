// The content policy applied to every image-carrying path on the Converse
// egress: a value a wire slot can hold FORWARDS verbatim (the upstream
// judges it); a value no wire slot can hold is REFUSED as unrepresentable;
// a well-formed image this JSON wire cannot carry keeps its warn-drop.
// Imports live in the host `messages_tests.rs` -- do not add `use` lines
// here.

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

/// The normalize-request detail for a translation expected to be refused.
/// Every refusal on this path has no wire slot to forward into, so the
/// reason is asserted here for each caller.
fn normalize_error_detail(messages: &[Message]) -> String {
    match build_messages(TEST_ID, messages) {
        Err(err @ Error::NormalizeRequest(..)) => {
            assert_eq!(
                err.refusal_reason(),
                Some(RefusalReason::Unrepresentable),
                "a refused content part must carry the unrepresentable reason"
            );
            let Error::NormalizeRequest(_, detail, _) = err else {
                unreachable!("matched above")
            };
            detail
        }
        Err(other) => panic!("expected a NormalizeRequest error, got {other:?}"),
        Ok(blocks) => panic!("expected a refused translation, got Ok({blocks:?})"),
    }
}

/// The full Converse request body built for `messages`.
fn built_body(messages: Vec<Message>) -> Value {
    let cfg = crate::bedrock::BedrockConfig {
        id: TEST_ID.into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-haiku-4-5".into(),
        api_shape: crate::bedrock::BedrockApiShape::Converse,
        creds: crate::bedrock::BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
        adaptive_thinking: None,
    };
    let req = routectl_core::ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: messages.into(),
        ..Default::default()
    };
    crate::bedrock::converse::normalize_request(&cfg, &req).expect("the shape must forward")
}

/// The first content block of the body's first message carrying `key`.
fn first_block<'a>(body: &'a Value, key: &str) -> &'a Value {
    body["messages"][0]["content"]
        .as_array()
        .expect("message content array")
        .iter()
        .find_map(|b| b.get(key))
        .unwrap_or_else(|| panic!("no `{key}` block in the built body: {body}"))
}

/// The first `toolResult.content[]` element of the body carrying `key`.
fn first_tool_result_element<'a>(body: &'a Value, key: &str) -> &'a Value {
    first_block(body, "toolResult")["content"]
        .as_array()
        .expect("toolResult content array")
        .iter()
        .find_map(|b| b.get(key))
        .unwrap_or_else(|| panic!("no `{key}` element in the toolResult: {body}"))
}

/// Assert that translating `messages` fails and the detail names `field`.
fn assert_refused_naming(messages: &[Message], field: &str) {
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

/// Assert the single translated message is a toolResult whose first content
/// element took the JSON fallback rather than failing the request.
fn assert_tool_result_json_fallback(messages: &[Message]) {
    let blocks = only_message_blocks(messages);
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
        "the source must fall back to the JSON wrap, got: {:?}",
        tool_result.content
    );
}

// ---------------------------------------------------------------------------
// FORWARDED -- a value the wire slot can hold reaches the body verbatim
// ---------------------------------------------------------------------------

/// An absent or empty base64 `data` fits `image.source.bytes`; only the
/// upstream can object to an empty image, so it forwards on every carrier.
#[test]
fn empty_image_data_forwards_as_empty_bytes_on_every_carrier() {
    let base64 = |data: Option<&str>| {
        let mut source = json!({"type": "base64", "media_type": "image/png"});
        if let Some(data) = data {
            source["data"] = json!(data);
        }
        source
    };
    for (row, data) in [("absent", None), ("empty", Some(""))] {
        // Arrange
        let message = built_body(vec![user_turn(vec![image_part(base64(data))])]);
        let tool = built_body(vec![tool_turn(vec![image_part(base64(data))])]);
        let raw = built_body(vec![raw_tool_result_turn(json!([
            {"type": "image", "source": base64(data)},
        ]))]);

        // Act / Assert
        assert_eq!(
            first_block(&message, "image"),
            &json!({"format": "png", "source": {"bytes": ""}}),
            "message carrier, {row} data"
        );
        for (carrier, body) in [("tool turn", &tool), ("raw tool_result", &raw)] {
            assert_eq!(
                first_tool_result_element(body, "image"),
                &json!({"format": "png", "source": {"bytes": ""}}),
                "{carrier} carrier, {row} data"
            );
        }
    }
}

/// An absent or empty `media_type` fits `image.format` as `""`; it must not
/// fall into the unmapped-media drop, which is for a nonempty type AWS's
/// table lacks.
#[test]
fn empty_image_media_type_forwards_as_empty_format_on_every_carrier() {
    let base64 = |media_type: Option<&str>| {
        let mut source = json!({"type": "base64", "data": "AAAA"});
        if let Some(media_type) = media_type {
            source["media_type"] = json!(media_type);
        }
        source
    };
    for (row, media_type) in [("absent", None), ("empty", Some(""))] {
        // Arrange
        let message = built_body(vec![user_turn(vec![image_part(base64(media_type))])]);
        let tool = built_body(vec![tool_turn(vec![image_part(base64(media_type))])]);
        let raw = built_body(vec![raw_tool_result_turn(json!([
            {"type": "image", "source": base64(media_type)},
        ]))]);

        // Act / Assert
        assert_eq!(
            first_block(&message, "image"),
            &json!({"format": "", "source": {"bytes": "AAAA"}}),
            "message carrier, {row} media_type"
        );
        for (carrier, body) in [("tool turn", &tool), ("raw tool_result", &raw)] {
            assert_eq!(
                first_tool_result_element(body, "image"),
                &json!({"format": "", "source": {"bytes": "AAAA"}}),
                "{carrier} carrier, {row} media_type"
            );
        }
    }
}

/// A `data:` URI declaring base64 with no payload names a mapped format and
/// an empty byte string, both of which the image slot holds.
#[test]
fn image_url_data_uri_with_empty_payload_forwards_as_empty_bytes() {
    // Arrange
    let messages = vec![user_turn(vec![image_url_part(
        json!({"url": "data:image/png;base64,"}),
    )])];

    // Act
    let body = built_body(messages);

    // Assert
    assert_eq!(
        first_block(&body, "image"),
        &json!({"format": "png", "source": {"bytes": ""}})
    );
}

/// A nonempty unmapped media type still takes its drop when the payload is
/// empty: the empty payload forwards, but there is no format to ride.
#[test]
fn empty_data_with_an_unmapped_media_type_still_warn_drops() {
    // Arrange
    let messages = vec![user_turn(vec![
        text_part("anchor"),
        image_part(json!({"type": "base64", "media_type": "image/tiff", "data": ""})),
    ])];

    // Act / Assert
    assert_warn_dropped(
        &messages,
        "dropping image with unmapped media_type on Converse egress",
    );
}

// ---------------------------------------------------------------------------
// REFUSED -- no wire slot can hold the value
// ---------------------------------------------------------------------------

/// A non-string `data` has no `String` slot to ride in.
#[test]
fn image_source_with_non_string_data_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![image_part(json!({
        "type": "base64",
        "media_type": "image/png",
        "data": 42,
    }))])];

    // Act / Assert
    assert_refused_naming(&messages, "source.data");
}

/// A `source` that is not a JSON object carries no field to read.
#[test]
fn image_source_that_is_not_an_object_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![image_part(json!("not-an-object"))])];

    // Act / Assert
    assert_refused_naming(&messages, "source");
}

/// An absent `source.type` leaves the source shape unnamed, and the shape
/// is what selects the wire member.
#[test]
fn image_source_with_absent_type_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![image_part(json!({
        "media_type": "image/png",
        "data": "AAAA",
    }))])];

    // Act / Assert
    assert_refused_naming(&messages, "source.type");
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
    assert_refused_naming(&messages, "source.type");
}

/// A url-shape source whose `url` is empty has no slot to forward into:
/// the Converse image source is bytes-only. The nonempty url-shape source
/// is the one that stays a warn-drop.
#[test]
fn url_shape_image_source_with_empty_url_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![image_part(json!({
        "type": "url",
        "url": "",
    }))])];

    // Act / Assert
    assert_refused_naming(&messages, "source.url");
}

/// An `image_url` part with no url at all; the image source has no url
/// member to carry one.
#[test]
fn image_url_with_absent_url_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![image_url_part(json!({}))])];

    // Act / Assert
    assert_refused_naming(&messages, "image_url.url");
}

/// An `image_url` part with an empty url.
#[test]
fn image_url_with_empty_url_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![image_url_part(json!({"url": ""}))])];

    // Act / Assert
    assert_refused_naming(&messages, "image_url.url");
}

/// A non-string field fits no `String` wire slot, and each refusing
/// carrier must say so by naming its own field. Table-driven because the
/// classification is one rule applied across shapes, not separate
/// behaviors.
#[test]
fn non_string_required_fields_fail_the_request_naming_the_field() {
    // Arrange
    let cases: Vec<(Vec<Message>, &str)> = vec![
        (
            vec![user_turn(vec![image_part(json!({
                "type": 7,
                "media_type": "image/png",
                "data": "AAAA",
            }))])],
            "source.type",
        ),
        (
            vec![user_turn(vec![image_part(json!({
                "type": "base64",
                "media_type": 7,
                "data": "AAAA",
            }))])],
            "source.media_type",
        ),
        (
            vec![user_turn(vec![image_part(json!({
                "type": "base64",
                "media_type": "image/png",
                "data": 42,
            }))])],
            "source.data",
        ),
        (
            vec![user_turn(vec![image_url_part(json!({"url": 42}))])],
            "image_url.url",
        ),
        (
            vec![tool_turn(vec![image_part(json!({
                "type": "base64",
                "media_type": 7,
                "data": "AAAA",
            }))])],
            "source.media_type",
        ),
        (
            vec![tool_turn(vec![image_part(json!({
                "type": "base64",
                "media_type": "image/png",
                "data": 42,
            }))])],
            "source.data",
        ),
    ];

    // Act / Assert
    for (messages, field) in cases {
        assert_refused_naming(&messages, field);
    }
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
/// a drop, not a refusal -- AWS's format table is bounded and the
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
// TOOL-RESULT CARRIER -- the JSON fallback the plain image path does not have
// ---------------------------------------------------------------------------

/// The tool-result carrier is not the plain image path: an `Ok(None)` here
/// still delivers the payload to the model as a JSON-wrapped tool result,
/// so a source this egress cannot read takes the fallback rather than a
/// refusal. Erroring instead converts a working 200 into a 400. Each
/// shape is exercised through BOTH tool-result carriers -- the raw
/// Anthropic-shape array element and the canonical `Role::Tool` part --
/// because they translate one source shape and must agree on it.
#[test]
fn unreadable_tool_result_image_sources_keep_the_json_fallback() {
    // Arrange: sources naming no base64 bytes, none of them provably a
    // broken base64 source.
    let sources = vec![
        json!("future-provider-shape"),
        json!({"media_type": "image/png", "data": "AAAA"}),
        json!({"type": "", "media_type": "image/png", "data": "AAAA"}),
        json!({"type": 7, "media_type": "image/png", "data": "AAAA"}),
        json!({"type": "url", "url": ""}),
        json!({"type": "url", "url": "https://example.com/x.png"}),
    ];

    // Act / Assert
    for source in sources {
        assert_tool_result_json_fallback(&[raw_tool_result_turn(json!([{
            "type": "image",
            "source": source.clone(),
        }]))]);
        assert_tool_result_json_fallback(&[tool_turn(vec![image_part(source)])]);
    }
}

// ---------------------------------------------------------------------------
// Atomicity and diagnostics
// ---------------------------------------------------------------------------

/// A refused image AFTER a valid one fails the whole normalization.
/// Nothing dispatches upstream: shipping the remaining parts and returning
/// 200 would silently lose the refused part.
#[test]
fn valid_image_then_refused_image_fails_the_whole_request() {
    // Arrange
    let messages = vec![user_turn(vec![
        image_part(json!({"type": "base64", "media_type": "image/png", "data": "AAAA"})),
        image_part(json!({"type": "base64", "media_type": "image/png", "data": 42})),
    ])];

    // Act / Assert
    assert_refused_naming(&messages, "source.data");
}

/// The reverse ordering fails identically -- the failure cannot depend on
/// where in the turn the refused part sits.
#[test]
fn refused_image_then_valid_image_fails_the_whole_request() {
    // Arrange
    let messages = vec![user_turn(vec![
        image_part(json!({"type": "base64", "media_type": "image/png", "data": 42})),
        image_part(json!({"type": "base64", "media_type": "image/png", "data": "AAAA"})),
    ])];

    // Act / Assert
    assert_refused_naming(&messages, "source.data");
}

/// `Error::NormalizeRequest` reaches the ingress as a 400 whose detail is
/// logged server-side, so the detail must name the offending FIELD and
/// never echo a caller-controlled VALUE -- payload bytes, media types and
/// urls are caller data.
#[test]
fn refused_image_errors_never_echo_a_caller_value() {
    // Arrange: each case carries a distinctive caller value in a field the
    // error is NOT about, so an echo is unambiguous.
    let non_string_data = vec![user_turn(vec![image_part(json!({
        "type": "base64",
        "media_type": "image/sentinel-media-type",
        "data": 42,
    }))])];
    let non_string_media_type = vec![user_turn(vec![image_part(json!({
        "type": "base64",
        "media_type": 7,
        "data": "sentinel-payload-bytes",
    }))])];
    let bad_url = vec![user_turn(vec![image_url_part(json!({
        "url": "",
        "detail": "sentinel-detail-value",
    }))])];

    // Act / Assert
    for (messages, leak) in [
        (non_string_data, "image/sentinel-media-type"),
        (non_string_media_type, "sentinel-payload-bytes"),
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
/// and an unsigned-reasoning skip recorded BEFORE a refused image are
/// aggregate events the operator is owed regardless of how the request
/// ends; without a flush on the `Err` path both are silently swallowed and
/// only the translation failure surfaces.
#[test]
fn tallies_recorded_before_a_refused_image_still_flush_on_the_error_path() {
    // Arrange: turn 0 drops a citations value, turn 1 skips an unsigned
    // reasoning detail, turn 2 carries the refused image.
    let messages = vec![
        user_turn(vec![ContentPart::Known(KnownContentPart::Document {
            source: json!({"type": "text", "media_type": "text/plain", "data": "notes"}),
            title: Some("notes".into()),
            citations: Some(json!("yes")),
            cache_control: None,
        })]),
        assistant_turn(vec![unsigned_detail(Some(0))]),
        user_turn(vec![image_part(json!("not-an-object"))]),
    ];

    // Act
    let mut result_is_err = false;
    let events = capture_events(|| {
        result_is_err = build_messages(TEST_ID, &messages).is_err();
    });

    // Assert
    assert!(result_is_err, "the refused image must fail the request");
    let citations_warn = events
        .iter()
        .any(|e| e.level == tracing::Level::WARN && e.field("dropped_count") == Some("1"));
    assert!(
        citations_warn,
        "the citations drop recorded before the error must still reach the operator; got: {events:?}"
    );
    let reasoning_skips = events
        .iter()
        .filter(|e| e.level == tracing::Level::DEBUG && e.field("skipped_count") == Some("1"))
        .count();
    assert_eq!(
        reasoning_skips, 1,
        "the reasoning skip recorded before the error must still reach the operator; got: {events:?}"
    );
}
