// Three-assertion pinning set for every counted drop this egress performs:
// the diagnostic fired at its declared level via real `tracing` capture, the
// dropped content is ABSENT FROM THE EMITTED WIRE VALUE (the serialized
// request body, not the typed struct -- a field can survive serialization
// inside an opaque payload while the typed view looks clean), and a
// similar-but-representable sibling SURVIVES in that same emitted value.
//
// Every test reading a counter delta carries
// `#[serial_test::serial(gemini_<class>)]` with the SAME name as every other
// test in this crate that reaches the same arm, incidentally or not: the
// registry is process-global and the runner is threaded, so a guard name no
// sibling shares excludes nothing. Deltas, never absolute values -- counters
// accumulate across the whole test binary.
//
// `include!`d into the `tests` module of `request.rs`; imports live there.

/// The `(gemini, <class>)` counter's current value, read back through the
/// public snapshot. `0` before the key's first increment.
fn gemini_drop_count(class: &str) -> u64 {
    crate::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == "gemini" && e.drop_class == class)
        .map_or(0, |e| e.drop_count)
}

/// The lane's request-volume denominator, read through the registry's own
/// accessor rather than off an arbitrary drop row.
fn gemini_lane_seen_count() -> u64 {
    crate::translation_drop_metrics::translation_lane_seen("gemini")
}

/// The serialized wire body, which is what actually ships upstream. Assert
/// against THIS rather than the typed `GenerateContentRequest`: a dropped
/// field that rode along inside an opaque payload is invisible in the typed
/// view and visible here.
fn wire_body(req: &ChatRequest) -> Value {
    let translated = translate("gemini:test", req).expect("translate ok");
    serde_json::to_value(&translated).expect("serialize ok")
}

/// A single-part user turn's request, the shape `parts_for` builds.
fn req_with_single_part(part: ContentPart) -> ChatRequest {
    ChatRequest {
        model: "gemini-2.5-pro".into(),
        messages: vec![user_with_parts(vec![part])].into(),
        ..Default::default()
    }
}

/// A user turn holding the drop-triggering part AND a representable text
/// sibling, so one emitted body carries both the absence and the survival.
fn req_with_part_beside_text(part: ContentPart, text: &str) -> ChatRequest {
    ChatRequest {
        model: "gemini-2.5-pro".into(),
        messages: vec![user_with_parts(vec![
            part,
            ContentPart::Known(KnownContentPart::Text {
                text: text.into(),
                citations: None,
                cache_control: None,
            }),
        ])]
        .into(),
        ..Default::default()
    }
}

/// Every `text` string anywhere in the emitted body, so a "did the payload
/// leak as prose" assertion cannot be fooled by nesting depth.
fn rendered(body: &Value) -> String {
    serde_json::to_string(body).expect("render")
}

// ---------------------------------------------------------------------------
// The lane denominator sits on the every-request path
// ---------------------------------------------------------------------------

/// The denominator must count a CLEAN request too -- a lane-seen count that
/// only moves on drops makes `drop_rate()` report 1.0 forever.
///
/// Asserts MOVEMENT rather than a delta of exactly one, deliberately: unlike a
/// drop class, `lane_seen` is bumped by every gemini test in this binary, so an
/// exact delta would need the same serial guard on all of them -- and a guard
/// that broad serializes most of the crate for no gain. The "at most one call
/// site per lane" half of the property is a grep-level fact, welded by the
/// census rather than reachable from inside one test.
#[test]
#[serial_test::serial(gemini_image_source_no_inline_bytes)]
fn a_clean_request_moves_the_lane_denominator() {
    // Arrange -- the snapshot exposes a lane's denominator only through one of
    // that lane's drop entries, so a request that DOES drop runs first to
    // establish a key; the clean request under test follows.
    let dropping = req_with_single_part(ContentPart::Known(KnownContentPart::Image {
        source: json!({"type": "url", "url": "https://example.com/cat.png"}),
        cache_control: None,
    }));
    let _ = translate("gemini:test", &dropping).expect("translate ok");
    let clean = base_req();

    // Act
    let before = gemini_lane_seen_count();
    let _ = translate("gemini:test", &clean).expect("translate ok");
    let after = gemini_lane_seen_count();

    // Assert
    assert!(
        after > before,
        "a clean request must still tick the lane denominator ({before} -> {after})"
    );
}

/// A request with several dropped blocks of ONE class is one drop EVENT.
#[test]
#[serial_test::serial(gemini_image_source_no_inline_bytes)]
fn three_dropped_blocks_of_one_class_count_as_one_drop() {
    // Arrange -- three url-shape image sources in one turn.
    let url_image = || {
        ContentPart::Known(KnownContentPart::Image {
            source: json!({"type": "url", "url": "https://example.com/cat.png"}),
            cache_control: None,
        })
    };
    let req = ChatRequest {
        model: "gemini-2.5-pro".into(),
        messages: vec![user_with_parts(vec![url_image(), url_image(), url_image()])].into(),
        ..Default::default()
    };

    // Act
    let before = gemini_drop_count("image_source_no_inline_bytes");
    let _ = translate("gemini:test", &req).expect("translate ok");
    let after = gemini_drop_count("image_source_no_inline_bytes");

    // Assert
    assert_eq!(
        after - before,
        1,
        "three dropped blocks of one class are one drop EVENT, not three"
    );
}

// ---------------------------------------------------------------------------
// image_source_no_inline_bytes
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(gemini_image_source_no_inline_bytes)]
fn image_source_drop_bumps_the_counter_once_per_request() {
    // Arrange -- a url-shape Anthropic image source carries no bytes, beside a
    // representable text sibling.
    let part = ContentPart::Known(KnownContentPart::Image {
        source: json!({"type": "url", "url": "https://example.com/cat.png"}),
        cache_control: None,
    });
    let req = req_with_part_beside_text(part, "describe this");

    // Act
    let before = gemini_drop_count("image_source_no_inline_bytes");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("image_source_no_inline_bytes");

    // Assert 1 -- the WARN fired.
    assert_warned(&events, "dropping non-base64 image source");
    // Assert 2 -- absent from the EMITTED WIRE VALUE, not merely from the
    // typed parts vector.
    let wire = rendered(&body);
    assert!(
        !wire.contains("inlineData") && !wire.contains("example.com/cat.png"),
        "no inlineData and no smuggled URL may reach the wire: {wire}"
    );
    // Assert 3 -- the representable sibling survived in that same body.
    assert!(
        wire.contains("describe this"),
        "the representable text sibling must survive: {wire}"
    );
    assert_eq!(after - before, 1, "one counted drop for the request");
}

/// POSITIVE CONTROL: a base64 image source -- the representable sibling shape
/// of the arm above -- reaches the wire as `inlineData` and counts nothing.
#[test]
#[serial_test::serial(gemini_image_source_no_inline_bytes)]
fn base64_image_source_survives_and_counts_no_drop() {
    // Arrange
    let part = ContentPart::Known(KnownContentPart::Image {
        source: json!({"type": "base64", "media_type": "image/png", "data": "iVBORw0KGgo="}),
        cache_control: None,
    });
    let req = req_with_single_part(part);

    // Act
    let before = gemini_drop_count("image_source_no_inline_bytes");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("image_source_no_inline_bytes");

    // Assert
    let wire = rendered(&body);
    assert!(
        wire.contains("iVBORw0KGgo=") && wire.contains("image/png"),
        "a representable image must reach the wire intact: {wire}"
    );
    assert!(
        !events.iter().any(|e| e.level == tracing::Level::WARN),
        "a representable image must not warn: {events:?}"
    );
    assert_eq!(after - before, 0, "nothing was dropped, nothing counted");
}

// ---------------------------------------------------------------------------
// image_url_data_uri_unparseable
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(gemini_image_url_data_uri_unparseable)]
fn image_url_data_uri_drop_bumps_the_counter_once_per_request() {
    // Arrange -- a data: URI with no `;base64,` separator. Shipping it as text
    // would bill the caller for the payload as prose.
    let req = req_with_part_beside_text(
        image_url_part("data:image/png,notbase64payload"),
        "what is this",
    );

    // Act
    let before = gemini_drop_count("image_url_data_uri_unparseable");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("image_url_data_uri_unparseable");

    // Assert
    assert_warned(&events, "dropping data: image_url");
    let wire = rendered(&body);
    assert!(
        !wire.contains("notbase64payload"),
        "the payload must not ride to the wire as prose: {wire}"
    );
    assert!(
        wire.contains("what is this"),
        "the representable text sibling must survive: {wire}"
    );
    assert_eq!(after - before, 1, "one counted drop for the request");
}

/// POSITIVE CONTROL: a parseable base64 data URI -- the representable sibling
/// of the arm above -- becomes `inlineData` and counts nothing.
#[test]
#[serial_test::serial(gemini_image_url_data_uri_unparseable)]
fn parseable_data_uri_image_url_survives_and_counts_no_drop() {
    // Arrange
    let req = req_with_single_part(image_url_part("data:image/png;base64,iVBORw0KGgo="));

    // Act
    let before = gemini_drop_count("image_url_data_uri_unparseable");
    let body = wire_body(&req);
    let after = gemini_drop_count("image_url_data_uri_unparseable");

    // Assert
    let wire = rendered(&body);
    assert!(
        wire.contains("iVBORw0KGgo=") && wire.contains("inlineData"),
        "a parseable data URI must land as inlineData: {wire}"
    );
    assert_eq!(after - before, 0);
}

// ---------------------------------------------------------------------------
// file_no_inline_bytes -- the arm that had a WARN and no test at all
// ---------------------------------------------------------------------------

/// The `file_id` reference form names an upload in the OpenAI Files namespace
/// that Gemini cannot resolve, and `Part` has no reference-by-id member.
#[test]
#[serial_test::serial(gemini_file_no_inline_bytes)]
fn file_no_inline_bytes_drop_bumps_the_counter_once_per_request() {
    // Arrange
    let req = req_with_part_beside_text(file_part(json!({"file_id": "file-abc123"})), "summarize");

    // Act
    let before = gemini_drop_count("file_no_inline_bytes");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("file_no_inline_bytes");

    // Assert
    assert_warned(
        &events,
        "dropping file part with no inline base64 file_data",
    );
    let wire = rendered(&body);
    assert!(
        !wire.contains("file-abc123") && !wire.contains("inlineData"),
        "the unresolvable file reference must not reach the wire: {wire}"
    );
    assert!(
        wire.contains("summarize"),
        "the representable text sibling must survive: {wire}"
    );
    assert_eq!(after - before, 1, "one counted drop for the request");
}

/// A non-data-URI `file_data` loses the same way and counts the same class.
#[test]
#[serial_test::serial(gemini_file_no_inline_bytes)]
fn non_data_uri_file_data_drop_bumps_the_same_counter() {
    // Arrange
    let req = req_with_part_beside_text(
        file_part(json!({
            "filename": "report.pdf",
            "file_data": "https://example.com/report.pdf",
        })),
        "read it",
    );

    // Act
    let before = gemini_drop_count("file_no_inline_bytes");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("file_no_inline_bytes");

    // Assert
    assert_warned(
        &events,
        "dropping file part with no inline base64 file_data",
    );
    let wire = rendered(&body);
    assert!(
        !wire.contains("example.com/report.pdf"),
        "the unfetchable URL must not ride to the wire: {wire}"
    );
    assert!(wire.contains("read it"));
    assert_eq!(after - before, 1);
}

/// POSITIVE CONTROL: the base64-upload form -- the representable sibling of
/// the two arms above -- reaches the wire as `inlineData` and counts nothing.
#[test]
#[serial_test::serial(gemini_file_no_inline_bytes)]
fn base64_file_data_survives_and_counts_no_drop() {
    // Arrange
    let req = req_with_single_part(file_part(json!({
        "filename": "report.pdf",
        "file_data": "data:application/pdf;base64,JVBERi0xLjQK",
    })));

    // Act
    let before = gemini_drop_count("file_no_inline_bytes");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("file_no_inline_bytes");

    // Assert
    let wire = rendered(&body);
    assert!(
        wire.contains("JVBERi0xLjQK") && wire.contains("application/pdf"),
        "the base64 upload form must reach the wire intact: {wire}"
    );
    assert!(
        !events.iter().any(|e| e.level == tracing::Level::WARN),
        "a representable file part must not warn: {events:?}"
    );
    assert_eq!(after - before, 0, "nothing was dropped, nothing counted");
}

// ---------------------------------------------------------------------------
// document_source_no_inline_bytes
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(gemini_document_source_no_inline_bytes)]
fn document_source_drop_bumps_the_counter_once_per_request() {
    // Arrange -- a url-shape Anthropic document source has no bytes.
    let req = req_with_part_beside_text(
        document_part(json!({"type": "url", "url": "https://example.com/report.pdf"})),
        "summarize it",
    );

    // Act
    let before = gemini_drop_count("document_source_no_inline_bytes");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("document_source_no_inline_bytes");

    // Assert
    assert_warned(&events, "dropping non-base64 document source");
    let wire = rendered(&body);
    assert!(
        !wire.contains("example.com/report.pdf") && !wire.contains("inlineData"),
        "no zero-byte document and no smuggled URL may reach the wire: {wire}"
    );
    assert!(wire.contains("summarize it"));
    assert_eq!(after - before, 1, "one counted drop for the request");
}

/// An empty-payload base64 document loses the same way, same class.
#[test]
#[serial_test::serial(gemini_document_source_no_inline_bytes)]
fn empty_base64_document_drop_bumps_the_same_counter() {
    // Arrange
    let req = req_with_part_beside_text(
        document_part(json!({
            "type": "base64", "media_type": "application/pdf", "data": "",
        })),
        "read it",
    );

    // Act
    let before = gemini_drop_count("document_source_no_inline_bytes");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("document_source_no_inline_bytes");

    // Assert
    assert_warned(&events, "dropping base64 document source with empty data");
    let wire = rendered(&body);
    assert!(
        !wire.contains("inlineData"),
        "a zero-byte document must not become an inlineData part: {wire}"
    );
    assert!(wire.contains("read it"));
    assert_eq!(after - before, 1);
}

/// POSITIVE CONTROL: a base64 document source survives and counts nothing.
#[test]
#[serial_test::serial(gemini_document_source_no_inline_bytes)]
fn base64_document_source_survives_and_counts_no_drop() {
    // Arrange
    let req = req_with_single_part(document_part(json!({
        "type": "base64", "media_type": "application/pdf", "data": "JVBERi0xLjQK",
    })));

    // Act
    let before = gemini_drop_count("document_source_no_inline_bytes");
    let body = wire_body(&req);
    let after = gemini_drop_count("document_source_no_inline_bytes");

    // Assert
    let wire = rendered(&body);
    assert!(
        wire.contains("JVBERi0xLjQK"),
        "a representable document must reach the wire: {wire}"
    );
    assert_eq!(after - before, 0);
}

// ---------------------------------------------------------------------------
// redacted_thinking_unsupported (arm fixed by an earlier task; the counter is
// what this closes -- the drop warned but was never counted)
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(gemini_redacted_thinking_unsupported)]
fn redacted_thinking_drop_bumps_the_counter_once_per_request() {
    // Arrange -- the opaque payload has no Gemini `Part` slot, beside an
    // ordinary thinking part that DOES.
    let req = ChatRequest {
        model: "gemini-2.5-pro".into(),
        messages: vec![user_with_parts(vec![
            redacted_thinking_part("AAECAwQFRedactedBlob"),
            thinking_part("visible reasoning"),
        ])]
        .into(),
        ..Default::default()
    };

    // Act
    let before = gemini_drop_count("redacted_thinking_unsupported");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("redacted_thinking_unsupported");

    // Assert
    assert_warned(&events, "dropping redacted-thinking part");
    let wire = rendered(&body);
    assert!(
        !wire.contains("AAECAwQFRedactedBlob"),
        "the redacted payload must not reach the wire by any path: {wire}"
    );
    assert!(
        wire.contains("visible reasoning"),
        "the representable thinking sibling must survive: {wire}"
    );
    assert_eq!(after - before, 1, "one counted drop for the request");
}

/// POSITIVE CONTROL: an ordinary thinking part alone travels the sibling
/// `Thinking` arm, survives, and counts nothing.
#[test]
#[serial_test::serial(gemini_redacted_thinking_unsupported)]
fn ordinary_thinking_part_survives_and_counts_no_drop() {
    // Arrange
    let req = req_with_single_part(thinking_part("reasoning about the answer"));

    // Act
    let before = gemini_drop_count("redacted_thinking_unsupported");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("redacted_thinking_unsupported");

    // Assert
    let wire = rendered(&body);
    assert!(
        wire.contains("reasoning about the answer"),
        "an un-redacted thinking part must survive: {wire}"
    );
    assert!(
        !events.iter().any(|e| e.level == tracing::Level::WARN),
        "an ordinary thinking part must not warn: {events:?}"
    );
    assert_eq!(after - before, 0);
}

// ---------------------------------------------------------------------------
// foreign_reasoning_unreplayable
// ---------------------------------------------------------------------------

/// A `reasoning_details` entry with a foreign `format` tag.
fn reasoning_detail(format: &str, text: &str) -> routectl_core::ReasoningDetail {
    routectl_core::ReasoningDetail {
        kind: routectl_core::ReasoningDetailKind::Text,
        id: None,
        format: Some(format.to_string()),
        index: Some(0),
        payload: json!({"text": text}),
    }
}

/// An assistant turn carrying `details` plus visible text.
fn assistant_with_reasoning(details: Vec<routectl_core::ReasoningDetail>) -> ChatRequest {
    ChatRequest {
        model: "gemini-2.5-pro".into(),
        messages: vec![
            make_user("q"),
            Message {
                role: Role::Assistant,
                content: MessageContent::Text("the answer".into()),
                refusal: None,
                reasoning: None,
                reasoning_details: details,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
        ]
        .into(),
        ..Default::default()
    }
}

#[test]
#[serial_test::serial(gemini_foreign_reasoning_unreplayable)]
fn foreign_reasoning_drop_bumps_the_counter_once_per_request() {
    // Arrange -- one foreign detail (unreplayable) beside one Gemini-origin
    // detail (replayable).
    let req = assistant_with_reasoning(vec![
        reasoning_detail("anthropic-v1", "foreign chain of thought"),
        routectl_core::ReasoningDetail {
            kind: routectl_core::ReasoningDetailKind::Text,
            id: None,
            format: Some(crate::gemini::GEMINI_FORMAT.to_string()),
            index: Some(1),
            payload: json!({"text": "native thought", "thought_signature": "sig9"}),
        },
    ]);

    // Act
    let before = gemini_drop_count("foreign_reasoning_unreplayable");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("foreign_reasoning_unreplayable");

    // Assert 1 -- the DEBUG fired with its structured count.
    assert!(
        events.iter().any(|e| e.level == tracing::Level::DEBUG
            && e.message.contains("skipping reasoning details")
            && e.field("skipped_count") == Some("1")),
        "the skip must be observable with its count: {events:?}"
    );
    // Assert 2 -- absent from the emitted wire value.
    let wire = rendered(&body);
    assert!(
        !wire.contains("foreign chain of thought"),
        "foreign reasoning must not reach the wire: {wire}"
    );
    // Assert 3 -- the replayable sibling survived, signature intact.
    assert!(
        wire.contains("native thought") && wire.contains("sig9"),
        "Gemini-origin reasoning must replay with its signature: {wire}"
    );
    assert_eq!(after - before, 1, "one counted drop for the request");
}

/// POSITIVE CONTROL: an all-native reasoning array replays whole and counts
/// nothing, proving the assertion above is tied to the foreign tag.
#[test]
#[serial_test::serial(gemini_foreign_reasoning_unreplayable)]
fn native_reasoning_only_replays_and_counts_no_drop() {
    // Arrange
    let req = assistant_with_reasoning(vec![routectl_core::ReasoningDetail {
        kind: routectl_core::ReasoningDetailKind::Text,
        id: None,
        format: Some(crate::gemini::GEMINI_FORMAT.to_string()),
        index: Some(0),
        payload: json!({"text": "native thought"}),
    }]);

    // Act
    let before = gemini_drop_count("foreign_reasoning_unreplayable");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("foreign_reasoning_unreplayable");

    // Assert
    assert!(rendered(&body).contains("native thought"));
    assert!(
        !events
            .iter()
            .any(|e| e.message.contains("skipping reasoning details")),
        "an all-native array must not trip the skip path: {events:?}"
    );
    assert_eq!(after - before, 0);
}

// ---------------------------------------------------------------------------
// unknown_content_block_unrepresentable
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(gemini_unknown_content_block_unrepresentable)]
fn unknown_content_block_drop_bumps_the_counter_once_per_request() {
    // Arrange -- a block whose `type` tag the canonical schema does not model.
    let unknown: ContentPart = serde_json::from_value(json!({
        "type": "video_frame_ref",
        "frame_uri": "unmodeled://frame/7",
    }))
    .expect("an unmodeled block deserializes into ContentPart::Other");
    let req = req_with_part_beside_text(unknown, "describe the frame");

    // Act
    let before = gemini_drop_count("unknown_content_block_unrepresentable");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("unknown_content_block_unrepresentable");

    // Assert
    assert!(
        events.iter().any(|e| e.level == tracing::Level::DEBUG
            && e.message.contains("skipping unknown content block type")
            && e.field("type_tag") == Some("video_frame_ref")),
        "the skip must name the dropped tag at DEBUG: {events:?}"
    );
    let wire = rendered(&body);
    assert!(
        !wire.contains("video_frame_ref") && !wire.contains("unmodeled://frame/7"),
        "an unmodeled block must not reach the wire in any form: {wire}"
    );
    assert!(
        wire.contains("describe the frame"),
        "the representable text sibling must survive: {wire}"
    );
    assert_eq!(after - before, 1, "one counted drop for the request");
}

/// POSITIVE CONTROL: a MODELED block type in the same position survives and
/// counts nothing, proving the assertion above exercises the unknown-tag arm.
#[test]
#[serial_test::serial(gemini_unknown_content_block_unrepresentable)]
fn modeled_content_block_survives_and_counts_no_drop() {
    // Arrange
    let known: ContentPart = serde_json::from_value(json!({
        "type": "text",
        "text": "plain text block",
    }))
    .expect("a modeled block deserializes");
    let req = req_with_single_part(known);

    // Act
    let before = gemini_drop_count("unknown_content_block_unrepresentable");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("unknown_content_block_unrepresentable");

    // Assert
    assert!(rendered(&body).contains("plain text block"));
    assert!(
        !events
            .iter()
            .any(|e| e.message.contains("skipping unknown content block")),
        "a modeled block must not trip the unknown-tag arm: {events:?}"
    );
    assert_eq!(after - before, 0);
}

// ---------------------------------------------------------------------------
// tool_call_no_function_object
// ---------------------------------------------------------------------------

/// An assistant turn carrying raw OpenAI-shape `tool_calls` values.
fn assistant_with_raw_tool_calls(tool_calls: Vec<Value>) -> ChatRequest {
    ChatRequest {
        model: "gemini-2.5-pro".into(),
        messages: vec![Message {
            role: Role::Assistant,
            content: MessageContent::Null,
            refusal: None,
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: Some(tool_calls),
        }]
        .into(),
        ..Default::default()
    }
}

#[test]
#[serial_test::serial(gemini_tool_call_no_function_object)]
fn tool_call_without_function_drop_bumps_the_counter_once_per_request() {
    // Arrange -- one entry with no `function` object (no name to correlate on)
    // beside a well-formed one.
    let req = assistant_with_raw_tool_calls(vec![
        json!({"id": "call_broken", "type": "function"}),
        json!({
            "id": "call_ok",
            "type": "function",
            "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}
        }),
    ]);

    // Act
    let before = gemini_drop_count("tool_call_no_function_object");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("tool_call_no_function_object");

    // Assert
    assert!(
        events.iter().any(|e| e.level == tracing::Level::DEBUG
            && e.message.contains("tool_call missing 'function' field")),
        "the skip must be observable at DEBUG: {events:?}"
    );
    let wire = rendered(&body);
    assert!(
        !wire.contains("call_broken"),
        "the unnamed call must not reach the wire: {wire}"
    );
    assert!(
        wire.contains("get_weather") && wire.contains("Paris"),
        "the well-formed sibling call must survive: {wire}"
    );
    assert_eq!(after - before, 1, "one counted drop for the request");
}

/// POSITIVE CONTROL: an all-well-formed `tool_calls` array counts nothing.
#[test]
#[serial_test::serial(gemini_tool_call_no_function_object)]
fn well_formed_tool_calls_survive_and_count_no_drop() {
    // Arrange
    let req = assistant_with_raw_tool_calls(vec![json!({
        "id": "call_ok",
        "type": "function",
        "function": {"name": "get_weather", "arguments": "{}"}
    })]);

    // Act
    let before = gemini_drop_count("tool_call_no_function_object");
    let body = wire_body(&req);
    let after = gemini_drop_count("tool_call_no_function_object");

    // Assert
    assert!(rendered(&body).contains("get_weather"));
    assert_eq!(after - before, 0);
}

// ---------------------------------------------------------------------------
// tool_def_unnamed
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(gemini_tool_def_unnamed)]
fn unnamed_tool_def_drop_bumps_the_counter_once_per_request() {
    // Arrange -- a hosted-tool shape with no function name, beside a named
    // function tool that IS representable.
    use routectl_core::ToolDef;
    let req = ChatRequest {
        model: "gemini-2.5-pro".into(),
        messages: vec![make_user("go")].into(),
        tools: Some(vec![
            ToolDef::Other(json!({"type": "web_search"})),
            ToolDef::Other(json!({
                "type": "function",
                "function": {"name": "lookup", "parameters": {"type": "object"}}
            })),
        ]),
        ..Default::default()
    };

    // Act
    let before = gemini_drop_count("tool_def_unnamed");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("tool_def_unnamed");

    // Assert
    assert_warned(&events, "skipping tool def with no usable function name");
    let wire = rendered(&body);
    assert!(
        !wire.contains("web_search"),
        "the hosted-tool shape must not reach the wire: {wire}"
    );
    assert!(
        wire.contains("lookup"),
        "the named sibling declaration must survive: {wire}"
    );
    assert_eq!(after - before, 1, "one counted drop for the request");
}

/// The dependent `tool_choice` drop is a CONSEQUENCE of the declaration drop
/// above and must not be counted a second time -- one lost intent, one count.
#[test]
#[serial_test::serial(gemini_tool_def_unnamed)]
fn tool_choice_dropped_with_its_declarations_counts_only_the_declaration_drop() {
    // Arrange -- the only tool def is nameless AND a tool_choice forces it.
    use routectl_core::ToolDef;
    let req = ChatRequest {
        model: "gemini-2.5-pro".into(),
        messages: vec![make_user("go")].into(),
        tools: Some(vec![ToolDef::Other(json!({"type": "web_search"}))]),
        tool_choice: Some(json!("required")),
        ..Default::default()
    };

    // Act
    let before = gemini_drop_count("tool_def_unnamed");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("tool_def_unnamed");

    // Assert
    assert_warned(
        &events,
        "no tool declarations survived; dropping tool_choice",
    );
    let wire = rendered(&body);
    assert!(
        !wire.contains("toolConfig") && !wire.contains("web_search"),
        "neither the tool nor a declaration-less toolConfig may reach the wire: {wire}"
    );
    assert_eq!(
        after - before,
        1,
        "the consequential tool_choice drop must not be counted a second time"
    );
}

/// POSITIVE CONTROL: an all-named tool array survives and counts nothing.
#[test]
#[serial_test::serial(gemini_tool_def_unnamed)]
fn named_tool_defs_survive_and_count_no_drop() {
    // Arrange
    use routectl_core::ToolDef;
    let req = ChatRequest {
        model: "gemini-2.5-pro".into(),
        messages: vec![make_user("go")].into(),
        tools: Some(vec![ToolDef::Other(json!({
            "type": "function",
            "function": {"name": "lookup", "parameters": {"type": "object"}}
        }))]),
        ..Default::default()
    };

    // Act
    let before = gemini_drop_count("tool_def_unnamed");
    let body = wire_body(&req);
    let after = gemini_drop_count("tool_def_unnamed");

    // Assert
    assert!(rendered(&body).contains("lookup"));
    assert_eq!(after - before, 0);
}

// ---------------------------------------------------------------------------
// response_format_unrepresentable
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(gemini_response_format_unrepresentable)]
fn unrecognized_response_format_drop_bumps_the_counter_once_per_request() {
    // Arrange -- an unmodeled structured-output mode, with a max_tokens knob
    // so the body still carries a generationConfig for the sibling assertion.
    let req = ChatRequest {
        response_format: Some(json!({"type": "grammar", "grammar": "root ::= digit+"})),
        max_tokens: Some(256),
        ..base_req()
    };

    // Act
    let before = gemini_drop_count("response_format_unrepresentable");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("response_format_unrepresentable");

    // Assert
    assert_warned(&events, "unrecognized response_format shape");
    let wire = rendered(&body);
    assert!(
        !wire.contains("grammar") && !wire.contains("responseMimeType"),
        "no guessed mime and no smuggled grammar may reach the wire: {wire}"
    );
    assert!(
        wire.contains("maxOutputTokens"),
        "the representable sibling knob must survive: {wire}"
    );
    assert_eq!(after - before, 1, "one counted drop for the request");
}

/// POSITIVE CONTROL: `json_object` -- the representable sibling shape -- maps
/// onto `responseMimeType` and counts nothing.
#[test]
#[serial_test::serial(gemini_response_format_unrepresentable)]
fn json_object_response_format_survives_and_counts_no_drop() {
    // Arrange
    let req = ChatRequest {
        response_format: Some(json!({"type": "json_object"})),
        ..base_req()
    };

    // Act
    let before = gemini_drop_count("response_format_unrepresentable");
    let body = wire_body(&req);
    let after = gemini_drop_count("response_format_unrepresentable");

    // Assert
    let wire = rendered(&body);
    assert!(
        wire.contains("application/json"),
        "json_object must map onto responseMimeType: {wire}"
    );
    assert_eq!(after - before, 0);
}

// ---------------------------------------------------------------------------
// schema_keyword_unsupported
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(gemini_schema_keyword_unsupported)]
fn schema_keyword_drop_bumps_the_counter_once_per_request() {
    // Arrange -- a pydantic-shaped tool schema whose `additionalProperties`
    // and `allOf` are constraints Gemini's Schema proto cannot carry, beside a
    // `type` and `properties` that translate fine.
    use routectl_core::{CustomTool, ToolDef};
    let req = ChatRequest {
        model: "gemini-2.5-pro".into(),
        messages: vec![make_user("go")].into(),
        tools: Some(vec![ToolDef::Custom(CustomTool {
            name: "calc".into(),
            description: None,
            input_schema: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "op": {"allOf": [{"type": "string"}]},
                    "n": {"type": "integer"}
                }
            }),
            cache_control: None,
            defer_loading: None,
            strict: None,
            type_tag: None,
        })]),
        ..Default::default()
    };

    // Act
    let before = gemini_drop_count("schema_keyword_unsupported");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("schema_keyword_unsupported");

    // Assert
    assert_warned(&events, "dropping JSON Schema keywords");
    let wire = rendered(&body);
    assert!(
        !wire.contains("additionalProperties") && !wire.contains("allOf"),
        "the unsupported keywords must not reach the wire: {wire}"
    );
    assert!(
        wire.contains("INTEGER") && wire.contains("calc"),
        "the translatable part of the schema must survive: {wire}"
    );
    assert_eq!(after - before, 1, "one counted drop for the request");
}

/// POSITIVE CONTROL: a schema built only from keywords Gemini DOES accept
/// reaches the wire whole, warns nothing, and counts nothing -- proving the
/// assertion above is tied to the unsupported keywords, not to any schema.
#[test]
#[serial_test::serial(gemini_schema_keyword_unsupported)]
fn gemini_subset_schema_survives_and_counts_no_drop() {
    // Arrange
    use routectl_core::{CustomTool, ToolDef};
    let req = ChatRequest {
        model: "gemini-2.5-pro".into(),
        messages: vec![make_user("go")].into(),
        tools: Some(vec![ToolDef::Custom(CustomTool {
            name: "calc".into(),
            description: Some("adds".into()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "when": {"type": "string", "format": "date-time"},
                    "n": {"type": "integer", "enum": ["1", "2"]}
                },
                "required": ["n"]
            }),
            cache_control: None,
            defer_loading: None,
            strict: None,
            type_tag: None,
        })]),
        ..Default::default()
    };

    // Act
    let before = gemini_drop_count("schema_keyword_unsupported");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("schema_keyword_unsupported");

    // Assert
    let wire = rendered(&body);
    assert!(
        wire.contains("date-time") && wire.contains("STRING") && wire.contains("INTEGER"),
        "a subset-legal schema must reach the wire whole: {wire}"
    );
    assert!(
        !events.iter().any(|e| e.level == tracing::Level::WARN),
        "a subset-legal schema must not warn: {events:?}"
    );
    assert_eq!(after - before, 0, "nothing was lost, nothing counted");
}

/// An unsupported `format` is a constraint loss on the same class -- the
/// counter must not be blind to it just because no keyword was stripped.
#[test]
#[serial_test::serial(gemini_schema_keyword_unsupported)]
fn unsupported_format_reports_a_drop() {
    // Arrange
    let (cleaned, dropped) =
        crate::gemini::schema::clean_schema_reporting(&json!({"type": "string", "format": "uri"}));

    // Assert
    assert!(
        cleaned.get("format").is_none(),
        "an unsupported format is stripped"
    );
    assert!(dropped, "stripping a caller format is a reported loss");
}

/// A supported `format` is renormalized, not lost, so it must NOT report.
#[test]
#[serial_test::serial(gemini_schema_keyword_unsupported)]
fn supported_format_reports_no_drop() {
    // Arrange
    let (cleaned, dropped) = crate::gemini::schema::clean_schema_reporting(
        &json!({"type": "string", "format": "date-time"}),
    );

    // Assert
    assert_eq!(cleaned["format"], "date-time");
    assert!(!dropped, "a surviving format is not a loss");
}

/// Each constraint keyword the proto rejects reports, individually.
#[test]
#[serial_test::serial(gemini_schema_keyword_unsupported)]
fn unsupported_keywords_report_a_drop() {
    for keyword in [
        "additionalProperties",
        "allOf",
        "not",
        "const",
        "patternProperties",
    ] {
        // Arrange
        let schema = json!({"type": "object", keyword: json!(false)});

        // Act
        let (cleaned, dropped) = crate::gemini::schema::clean_schema_reporting(&schema);

        // Assert
        assert!(cleaned.get(keyword).is_none(), "{keyword} must be stripped");
        assert!(dropped, "stripping {keyword} loses a caller constraint");
    }
}

/// `$schema` and the `$defs`/`definitions` containers are metadata and a ref
/// sidecar whose targets already reach the wire inlined, so stripping them
/// loses nothing and must NOT report -- the paired positive control proving
/// the assertion above is not just "any strip reports".
#[test]
#[serial_test::serial(gemini_schema_keyword_unsupported)]
fn structural_keyword_strips_report_no_drop() {
    // Arrange -- a resolvable ref, so the def's shape survives inlined.
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "$defs": {"Inner": {"type": "string"}},
        "properties": {"a": {"$ref": "#/$defs/Inner"}}
    });

    // Act
    let (cleaned, dropped) = crate::gemini::schema::clean_schema_reporting(&schema);

    // Assert
    assert!(cleaned.get("$schema").is_none());
    assert!(cleaned.get("$defs").is_none());
    assert_eq!(
        cleaned["properties"]["a"]["type"], "STRING",
        "the def's shape must survive inlined: {cleaned}"
    );
    assert!(
        !dropped,
        "metadata and an inlined ref sidecar lose nothing: {cleaned}"
    );
}

/// An UNRESOLVABLE ref degrades to an unconstrained schema -- the caller's
/// shape is genuinely gone, so this one does report.
#[test]
#[serial_test::serial(gemini_schema_keyword_unsupported)]
fn unresolvable_ref_reports_a_drop() {
    // Arrange -- no `$defs` backs the pointer.
    let schema = json!({"type": "object", "properties": {"a": {"$ref": "#/$defs/Missing"}}});

    // Act
    let (cleaned, dropped) = crate::gemini::schema::clean_schema_reporting(&schema);

    // Assert
    assert!(
        cleaned["properties"]["a"].get("$ref").is_none(),
        "the unresolvable ref must not reach the wire: {cleaned}"
    );
    assert!(dropped, "a degraded ref loses the caller's nested shape");
}

// ---------------------------------------------------------------------------
// cache_control_unsupported
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(gemini_cache_control_unsupported)]
fn cache_control_drop_bumps_the_counter_once_per_request() {
    // Arrange -- a marked text part beside an unmarked one.
    let marked = ContentPart::Known(KnownContentPart::Text {
        text: "cache me".into(),
        citations: None,
        cache_control: Some(CacheControl::ephemeral_5m()),
    });
    let req = req_with_part_beside_text(marked, "plain sibling");

    // Act
    let before = gemini_drop_count("cache_control_unsupported");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("cache_control_unsupported");

    // Assert
    assert_warned(&events, "cache_control dropped");
    let wire = rendered(&body);
    assert!(
        !wire.contains("cache_control") && !wire.contains("ephemeral"),
        "the marker must not survive into the emitted body by any path: {wire}"
    );
    assert!(
        wire.contains("cache me") && wire.contains("plain sibling"),
        "both texts must still reach the wire; only the marker is dropped: {wire}"
    );
    assert_eq!(after - before, 1, "one counted drop for the request");
}

/// POSITIVE CONTROL: an unmarked request warns nothing and counts nothing,
/// proving the warning above is tied to the marker.
#[test]
#[serial_test::serial(gemini_cache_control_unsupported)]
fn unmarked_request_counts_no_cache_control_drop() {
    // Arrange
    let req = base_req();

    // Act
    let before = gemini_drop_count("cache_control_unsupported");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("cache_control_unsupported");

    // Assert
    assert!(rendered(&body).contains("hello"));
    assert!(
        !events
            .iter()
            .any(|e| e.message.contains("cache_control dropped")),
        "an unmarked request must not trip the marker warning: {events:?}"
    );
    assert_eq!(after - before, 0);
}

// ---------------------------------------------------------------------------
// reasoning_effort_unrecognized
// ---------------------------------------------------------------------------

/// A gemini-3 request (the thinkingLevel arm) carrying `reasoning.effort`.
fn req_with_effort(effort: &str) -> ChatRequest {
    ChatRequest {
        model: "gemini-3-pro-preview".into(),
        messages: vec![make_user("hi")].into(),
        reasoning: Some(routectl_core::ReasoningConfig {
            effort: Some(effort.to_string()),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[test]
#[serial_test::serial(gemini_reasoning_effort_unrecognized)]
fn unrecognized_reasoning_effort_drop_bumps_the_counter_once() {
    // Arrange: a token outside the canonical effort vocabulary.
    let req = req_with_effort("turbo");

    // Act
    let before = gemini_drop_count("reasoning_effort_unrecognized");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("reasoning_effort_unrecognized");

    // Assert 1: the diagnostic fired.
    assert_warned(&events, "dropping an unrecognized reasoning effort token");

    // Assert 2: no thinkingLevel reached the emitted wire value. Asserting on
    // the serialized body rather than the typed config is what would catch the
    // token riding along inside the thinkingConfig object.
    let wire = rendered(&body);
    assert!(
        !wire.contains("thinkingLevel") && !wire.contains("turbo"),
        "an unmappable effort must leave no level on the wire: {wire}"
    );

    // Assert 3: positive control -- the turn itself still ships.
    assert!(wire.contains("hi"), "the request must survive: {wire}");

    assert_eq!(after - before, 1, "one counted drop for the request");
}

#[test]
#[serial_test::serial(gemini_reasoning_effort_unrecognized)]
fn a_recognized_reasoning_effort_maps_to_a_level_and_counts_no_drop() {
    // The paired positive control for the test above: a token IN the
    // vocabulary must reach the wire as a level and count nothing.
    let req = req_with_effort("high");

    let before = gemini_drop_count("reasoning_effort_unrecognized");
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));
    let after = gemini_drop_count("reasoning_effort_unrecognized");

    let wire = rendered(&body);
    assert!(
        wire.contains("thinkingLevel") && wire.contains("high"),
        "a recognized effort must ship as a level: {wire}"
    );
    assert!(
        !events
            .iter()
            .any(|e| e.message.contains("unrecognized reasoning effort")),
        "a representable effort must not warn, got: {events:?}"
    );
    assert_eq!(after - before, 0, "no drop for a representable effort");
}

// ---------------------------------------------------------------------------
// The policy-action vocabulary on this lane. A managed-key override refusal is
// a POLICY ACTION, not a drop: the upstream would accept the colliding value
// and routectl refuses it to keep its own assembled body authoritative.
// ---------------------------------------------------------------------------

/// The `(gemini, <class>)` policy-action counter, read back through the public
/// snapshot.
fn gemini_policy_action_count(class: &str) -> u64 {
    crate::translation_drop_metrics::translation_policy_action_snapshot()
        .into_iter()
        .find(|e| e.lane == "gemini" && e.policy_class == class)
        .map_or(0, |e| e.action_count)
}

#[test]
#[serial_test::serial(gemini_provider_extra_managed_key_conflict)]
fn payload_extras_managed_key_override_bumps_the_policy_action_counter_once() {
    // Arrange -- two managed keys in one request, so the ONCE-per-request
    // contract is what the delta proves rather than a coincidence of one key.
    let req = base_req();
    let extras = json!({
        "generationConfig": {"temperature": 9.9},
        "contents": "client-clobber",
        "safetySettings": [{"category": "HARM_CATEGORY_HATE_SPEECH"}],
    });

    let before = gemini_policy_action_count("provider_extra_managed_key_conflict");
    let drops_before = gemini_drop_count("provider_extra_managed_key_conflict");
    let mut body = Value::Null;
    let events =
        routectl_testkit::capture_events(|| body = normalize_body("gemini:test", &req, &extras));
    let after = gemini_policy_action_count("provider_extra_managed_key_conflict");
    let drops_after = gemini_drop_count("provider_extra_managed_key_conflict");

    // Assert 1: the refusal is declared at WARN, once per colliding key.
    assert!(
        events.iter().any(|e| e
            .message
            .contains("attempted to override routectl-managed key")),
        "the refusal must warn, got: {events:?}"
    );

    // Assert 2: the assembled value survives and the override never lands.
    let wire = rendered(&body);
    assert!(
        !wire.contains("client-clobber") && !wire.contains("9.9"),
        "no managed-key override may reach the wire: {wire}"
    );

    // Assert 3: a non-managed sibling still merges, so the guard is scoped.
    assert!(
        body.get("safetySettings").is_some(),
        "a representable extra must still merge: {wire}"
    );

    assert_eq!(
        after - before,
        1,
        "two colliding keys in one request count ONE policy action"
    );
    assert_eq!(
        drops_after, drops_before,
        "a policy action must not reach the drop vocabulary; the two are disjoint"
    );
}

#[test]
#[serial_test::serial(gemini_provider_extra_managed_key_conflict)]
fn payload_extras_with_no_managed_key_counts_no_policy_action() {
    // The paired positive control: without a collision the guard must not
    // fire, so the counter above measures the refusal and not the merge.
    let req = base_req();
    let extras = json!({"safetySettings": [{"category": "HARM_CATEGORY_HATE_SPEECH"}]});

    let before = gemini_policy_action_count("provider_extra_managed_key_conflict");
    let mut body = Value::Null;
    let events =
        routectl_testkit::capture_events(|| body = normalize_body("gemini:test", &req, &extras));
    let after = gemini_policy_action_count("provider_extra_managed_key_conflict");

    assert!(
        body.get("safetySettings").is_some(),
        "the non-managed extra must merge: {}",
        rendered(&body)
    );
    assert!(
        !events.iter().any(|e| e
            .message
            .contains("attempted to override routectl-managed key")),
        "no collision must warn, got: {events:?}"
    );
    assert_eq!(after - before, 0, "no refusal, no policy action");
}
