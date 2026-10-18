// The content policy applied to every document-carrying path on the
// Converse egress: a value a wire slot can hold FORWARDS verbatim; a value
// no wire slot can hold is REFUSED as unrepresentable; a well-formed
// document this JSON wire cannot carry keeps its warn-drop or its JSON
// fallback.
// Imports live in the host `messages_tests.rs` -- do not add `use` lines
// here. Shared turn builders come from the image-policy fragment.

fn document_part(source: Value) -> ContentPart {
    ContentPart::Known(KnownContentPart::Document {
        source,
        title: Some("notes".into()),
        citations: None,
        cache_control: None,
    })
}

fn file_part(file: Value) -> ContentPart {
    ContentPart::Known(KnownContentPart::File {
        file,
        cache_control: None,
    })
}

/// A raw Anthropic-shape tool_result array carrying one document element
/// with the given `source`.
fn raw_document_turn(source: Value) -> Message {
    raw_tool_result_turn(json!([{
        "type": "document",
        "source": source,
        "title": "notes",
    }]))
}

/// Assert the document dropped but the sibling anchor text survived, and
/// the operator-facing WARN carrying `needle` fired.
fn assert_document_warn_dropped(messages: &[Message], needle: &str) {
    let mut blocks = Vec::new();
    let events = capture_events(|| {
        blocks = only_message_blocks(messages);
    });
    assert!(
        !blocks
            .iter()
            .any(|b| matches!(b, ConverseContentBlock::Document { .. })),
        "an unrepresentable document must be dropped, got: {blocks:?}"
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
// FORWARDED -- a value the wire slot can hold reaches the body verbatim
// ---------------------------------------------------------------------------

/// An absent or empty `data` fits the document source member for both
/// recognized kinds; only the upstream can object to an empty document, so
/// it forwards on every carrier.
#[test]
fn empty_document_data_forwards_as_an_empty_source_on_every_carrier() {
    let source = |kind: &str, media_type: &str, data: Option<&str>| {
        let mut source = json!({"type": kind, "media_type": media_type});
        if let Some(data) = data {
            source["data"] = json!(data);
        }
        source
    };
    for (kind, media_type, format) in [
        ("base64", "application/pdf", "pdf"),
        ("text", "text/plain", "txt"),
    ] {
        for (row, data) in [("absent", None), ("empty", Some(""))] {
            // Arrange
            let expected = json!({"format": format, "name": "notes", "source": {"bytes": ""}});
            let message = built_body(vec![user_turn(vec![document_part(source(
                kind, media_type, data,
            ))])]);
            let tool = built_body(vec![tool_turn(vec![document_part(source(
                kind, media_type, data,
            ))])]);
            let raw = built_body(vec![raw_document_turn(source(kind, media_type, data))]);

            // Act / Assert
            assert_eq!(
                first_block(&message, "document"),
                &expected,
                "message carrier, {kind} source, {row} data"
            );
            for (carrier, body) in [("tool turn", &tool), ("raw tool_result", &raw)] {
                assert_eq!(
                    first_tool_result_element(body, "document"),
                    &expected,
                    "{carrier} carrier, {kind} source, {row} data"
                );
            }
        }
    }
}

/// An absent or empty `media_type` fits `document.format` as `""`; it must
/// not fall into the unmapped-media drop or the JSON fallback, which are
/// for a nonempty type AWS's table lacks.
#[test]
fn empty_document_media_type_forwards_as_empty_format_on_every_carrier() {
    let source = |media_type: Option<&str>| {
        let mut source = json!({"type": "base64", "data": "JVBERi0xLjQ="});
        if let Some(media_type) = media_type {
            source["media_type"] = json!(media_type);
        }
        source
    };
    for (row, media_type) in [("absent", None), ("empty", Some(""))] {
        // Arrange
        let expected = json!({"format": "", "name": "notes", "source": {"bytes": "JVBERi0xLjQ="}});
        let message = built_body(vec![user_turn(vec![document_part(source(media_type))])]);
        let tool = built_body(vec![tool_turn(vec![document_part(source(media_type))])]);
        let raw = built_body(vec![raw_document_turn(source(media_type))]);

        // Act / Assert
        assert_eq!(
            first_block(&message, "document"),
            &expected,
            "message carrier, {row} media_type"
        );
        for (carrier, body) in [("tool turn", &tool), ("raw tool_result", &raw)] {
            assert_eq!(
                first_tool_result_element(body, "document"),
                &expected,
                "{carrier} carrier, {row} media_type"
            );
        }
    }
}

/// A nonempty unmapped media type still takes its drop when the payload is
/// empty: the empty payload forwards, but there is no format to ride.
#[test]
#[serial_test::serial(bedrock_converse_document_media_type_unsupported)]
fn empty_document_data_with_an_unmapped_media_type_still_warn_drops() {
    // Arrange
    let messages = vec![user_turn(vec![
        text_part("anchor"),
        document_part(json!({
            "type": "base64",
            "media_type": "application/vnd.sqlite3",
            "data": "",
        })),
    ])];

    // Act / Assert
    assert_document_warn_dropped(
        &messages,
        "dropping document with unmapped media_type on Converse egress",
    );
}

// ---------------------------------------------------------------------------
// REFUSED -- no wire slot can hold the value
// ---------------------------------------------------------------------------

/// A non-string `data` has no `String` slot to ride in.
#[test]
fn document_source_with_non_string_data_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![document_part(json!({
        "type": "base64",
        "media_type": "application/pdf",
        "data": 42,
    }))])];

    // Act / Assert
    assert_refused_naming(&messages, "source.data");
}

/// A `source` that is not a JSON object carries no field to read.
#[test]
fn document_source_that_is_not_an_object_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![document_part(json!("not-an-object"))])];

    // Act / Assert
    assert_refused_naming(&messages, "source");
}

/// An absent `source.type` leaves the source shape unnamed, and the shape
/// is what selects the wire member.
#[test]
fn document_source_with_absent_type_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![document_part(json!({
        "media_type": "application/pdf",
        "data": "JVBERi0xLjQ=",
    }))])];

    // Act / Assert
    assert_refused_naming(&messages, "source.type");
}

/// An empty `source.type` is the absent case wearing a string.
#[test]
fn document_source_with_empty_type_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![document_part(json!({
        "type": "",
        "media_type": "application/pdf",
        "data": "JVBERi0xLjQ=",
    }))])];

    // Act / Assert
    assert_refused_naming(&messages, "source.type");
}

/// A non-string field fits no `String` wire slot, and each refusing
/// carrier must say so by naming its own field. Table-driven because the
/// classification is one rule applied across shapes, not several separate
/// behaviors.
#[test]
fn non_string_document_fields_fail_the_request_naming_the_field() {
    // Arrange
    let cases: Vec<(Vec<Message>, &str)> = vec![
        (
            vec![user_turn(vec![document_part(json!({
                "type": 7,
                "media_type": "application/pdf",
                "data": "JVBERi0xLjQ=",
            }))])],
            "source.type",
        ),
        (
            vec![user_turn(vec![document_part(json!({
                "type": "base64",
                "media_type": 7,
                "data": "JVBERi0xLjQ=",
            }))])],
            "source.media_type",
        ),
        (
            vec![tool_turn(vec![document_part(json!({
                "type": "base64",
                "media_type": 7,
                "data": "JVBERi0xLjQ=",
            }))])],
            "source.media_type",
        ),
        (
            vec![tool_turn(vec![document_part(json!({
                "type": "base64",
                "media_type": "application/pdf",
                "data": 42,
            }))])],
            "source.data",
        ),
        (
            vec![raw_document_turn(json!({
                "type": "base64",
                "media_type": 7,
                "data": "JVBERi0xLjQ=",
            }))],
            "source.media_type",
        ),
        (
            vec![raw_document_turn(json!({
                "type": "text",
                "media_type": "text/plain",
                "data": 42,
            }))],
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

/// A structurally complete document whose `media_type` AWS does not model is
/// a drop, not a refusal -- AWS's format table is bounded and the
/// caller did nothing wrong. Pinned so the fix cannot over-reach.
#[test]
#[serial_test::serial(bedrock_converse_document_media_type_unsupported)]
fn structurally_complete_unmapped_document_media_type_still_warn_drops() {
    // Arrange
    let messages = vec![user_turn(vec![
        text_part("anchor"),
        document_part(json!({
            "type": "base64",
            "media_type": "application/vnd.sqlite3",
            "data": "JVBERi0xLjQ=",
        })),
    ])];

    // Act / Assert
    assert_document_warn_dropped(
        &messages,
        "dropping document with unmapped media_type on Converse egress",
    );
}

/// An unknown but NONEMPTY source kind may be a valid vendor shape this
/// build has not learned yet, and a `url` ref is a document AWS's JSON wire
/// cannot dereference. Tightening either to an error would 400 working
/// traffic, so the forward-compat default is pinned here.
#[test]
#[serial_test::serial(bedrock_converse_document_source_unrepresentable)]
fn unrecognized_document_source_kind_still_warn_drops() {
    // Arrange
    for kind in ["url", "some-future-source-kind"] {
        let messages = vec![user_turn(vec![
            text_part("anchor"),
            document_part(json!({
                "type": kind,
                "media_type": "application/pdf",
                "data": "JVBERi0xLjQ=",
            })),
        ])];

        // Act / Assert
        assert_document_warn_dropped(
            &messages,
            "dropping unsupported document source type on Converse egress",
        );
    }
}

/// An OpenAI-shape `file` part with no inline bytes has nothing to translate
/// and keeps its own warn-drop: the rewrite to a canonical document source
/// never happens, so the document policy is never reached. Pinned because a
/// naive "documents now error" reading would break these shapes.
#[test]
#[serial_test::serial(bedrock_converse_file_part_unrepresentable)]
fn untranslatable_file_parts_still_warn_drop() {
    // Arrange
    for file in [
        json!({"file_id": "file-abc"}),
        json!({"filename": "x.pdf", "file_data": "data:application/pdf;base64,"}),
        json!({"filename": "x.txt", "file_data": "data:text/plain;base64,aGk="}),
    ] {
        let messages = vec![user_turn(vec![text_part("anchor"), file_part(file)])];

        // Act / Assert
        assert_document_warn_dropped(
            &messages,
            "dropping file part on Converse egress; only base64 PDF data URIs are supported",
        );
    }
}

// ---------------------------------------------------------------------------
// TOOL-RESULT CARRIERS -- the JSON fallback the plain document path lacks
// ---------------------------------------------------------------------------

/// The tool-result carriers are not the plain document path: an `Ok(None)`
/// there still delivers the payload to the model as a JSON-wrapped tool
/// result, so a source this egress cannot READ takes the fallback rather
/// than a refusal. Erroring instead converts a working 200 into a 400. Each
/// shape is exercised through BOTH tool-result carriers -- the raw
/// Anthropic-shape array element and the canonical `Role::Tool` part --
/// because they translate one source shape and must agree on it.
#[test]
fn unreadable_tool_result_document_sources_keep_the_json_fallback() {
    // Arrange: sources whose KIND this wire cannot carry, or whose media
    // type AWS does not model. None is provably a broken carryable source.
    let sources = vec![
        json!("future-provider-shape"),
        json!({"media_type": "application/pdf", "data": "JVBERi0xLjQ="}),
        json!({"type": "", "media_type": "application/pdf", "data": "JVBERi0xLjQ="}),
        json!({"type": 7, "media_type": "application/pdf", "data": "JVBERi0xLjQ="}),
        json!({"type": "url", "url": "https://example.com/x.pdf"}),
        json!({
            "type": "base64",
            "media_type": "application/vnd.sqlite3",
            "data": "JVBERi0xLjQ=",
        }),
    ];

    // Act / Assert
    for source in sources {
        assert_tool_result_json_fallback(&[raw_document_turn(source.clone())]);
        assert_tool_result_json_fallback(&[tool_turn(vec![document_part(source)])]);
    }
}

// ---------------------------------------------------------------------------
// Atomicity and diagnostics
// ---------------------------------------------------------------------------

/// A refused document AFTER a valid one fails the whole normalization.
/// Nothing dispatches upstream: shipping the remaining parts and returning
/// 200 would silently lose the refused part.
#[test]
fn valid_document_then_refused_document_fails_the_whole_request() {
    // Arrange
    let messages = vec![user_turn(vec![
        document_part(json!({
            "type": "base64",
            "media_type": "application/pdf",
            "data": "JVBERi0xLjQ=",
        })),
        document_part(json!({
            "type": "base64",
            "media_type": "application/pdf",
            "data": 42,
        })),
    ])];

    // Act / Assert
    assert_refused_naming(&messages, "source.data");
}

/// The reverse ordering fails identically -- the failure cannot depend on
/// where in the turn the refused part sits.
#[test]
fn refused_document_then_valid_document_fails_the_whole_request() {
    // Arrange
    let messages = vec![user_turn(vec![
        document_part(json!({
            "type": "base64",
            "media_type": "application/pdf",
            "data": 42,
        })),
        document_part(json!({
            "type": "base64",
            "media_type": "application/pdf",
            "data": "JVBERi0xLjQ=",
        })),
    ])];

    // Act / Assert
    assert_refused_naming(&messages, "source.data");
}

/// `Error::NormalizeRequest` reaches the ingress as a 400 whose detail is
/// logged server-side, so the detail must name the offending FIELD and never
/// echo a caller-controlled VALUE -- payload bytes, media types and titles
/// are caller data.
#[test]
fn refused_document_errors_never_echo_a_caller_value() {
    // Arrange: each case carries a distinctive caller value in a field the
    // error is NOT about, so an echo is unambiguous.
    let cases: Vec<(Vec<Message>, &str)> = vec![
        (
            vec![user_turn(vec![document_part(json!({
                "type": "base64",
                "media_type": "application/sentinel-media-type",
                "data": 42,
            }))])],
            "application/sentinel-media-type",
        ),
        (
            vec![user_turn(vec![document_part(json!({
                "type": "base64",
                "media_type": 7,
                "data": "sentinel-payload-bytes",
            }))])],
            "sentinel-payload-bytes",
        ),
        (
            vec![user_turn(vec![document_part(json!({
                "type": "",
                "media_type": "application/sentinel-media-type",
                "data": "sentinel-payload-bytes",
            }))])],
            "sentinel-payload-bytes",
        ),
        (
            vec![tool_turn(vec![document_part(json!({
                "type": "base64",
                "media_type": "application/sentinel-media-type",
                "data": 42,
            }))])],
            "application/sentinel-media-type",
        ),
        (
            vec![raw_document_turn(json!({
                "type": "base64",
                "media_type": "application/sentinel-media-type",
                "data": 42,
            }))],
            "application/sentinel-media-type",
        ),
    ];

    // Act / Assert
    for (messages, leak) in cases {
        let detail = normalize_error_detail(&messages);
        assert!(
            !detail.contains(leak),
            "the error detail must not echo the caller value `{leak}`; got: {detail}"
        );
    }
}

/// Both per-request tallies are flushed on the error arm. A citation drop
/// and an unsigned-reasoning skip recorded BEFORE a refused document are
/// aggregate events the operator is owed regardless of how the request ends;
/// without a flush on the `Err` path both are silently swallowed and only
/// the translation failure surfaces.
#[test]
#[serial_test::serial(bedrock_converse_reasoning_signature_missing_drop)]
fn tallies_recorded_before_a_refused_document_still_flush_on_the_error_path() {
    // Arrange: turn 0 drops a citations value, turn 1 skips an unsigned
    // reasoning detail, turn 2 carries the refused document.
    let messages = vec![
        user_turn(vec![ContentPart::Known(KnownContentPart::Document {
            source: json!({"type": "text", "media_type": "text/plain", "data": "notes"}),
            title: Some("notes".into()),
            citations: Some(json!("yes")),
            cache_control: None,
        })]),
        assistant_turn(vec![unsigned_detail(Some(0))]),
        user_turn(vec![document_part(json!("not-an-object"))]),
    ];

    // Act
    let mut result_is_err = false;
    let events = capture_events(|| {
        result_is_err = build_messages(TEST_ID, &messages).is_err();
    });

    // Assert
    assert!(result_is_err, "the refused document must fail the request");
    assert!(
        events
            .iter()
            .any(|e| e.level == tracing::Level::WARN && e.field("dropped_count") == Some("1")),
        "the citations drop recorded before the error must still reach the operator; got: {events:?}"
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| e.level == tracing::Level::DEBUG && e.field("skipped_count") == Some("1"))
            .count(),
        1,
        "the reasoning skip recorded before the error must still reach the operator; got: {events:?}"
    );
}

// ---------------------------------------------------------------------------
// The refusal register for this egress: one row per refusing site
// ---------------------------------------------------------------------------

/// Every site on this egress that still refuses does so because no wire
/// slot can hold the value, and carries the `unrepresentable` reason. One
/// row per site, each row reaching exactly that site (its detail prefix is
/// unique to it), so a site that changes reason or stops refusing names
/// itself.
#[test]
fn every_refusing_site_carries_the_unrepresentable_reason() {
    // Arrange
    let rows: Vec<(&str, Vec<Message>)> = vec![
        (
            "image content part on a Converse message has a malformed source",
            vec![user_turn(vec![image_part(json!("not-an-object"))])],
        ),
        (
            "image content part on a Converse message names no source shape",
            vec![user_turn(vec![image_part(json!({"data": "AAAA"}))])],
        ),
        (
            "image content part on a Converse message has no usable url",
            vec![user_turn(vec![image_part(
                json!({"type": "url", "url": ""}),
            )])],
        ),
        (
            "image content part on a Converse message has an unusable image format",
            vec![user_turn(vec![image_part(
                json!({"type": "base64", "media_type": 7, "data": "AAAA"}),
            )])],
        ),
        (
            "image content part on a Converse message has an unusable base64 payload",
            vec![user_turn(vec![image_part(
                json!({"type": "base64", "media_type": "image/png", "data": 42}),
            )])],
        ),
        (
            "image_url content part on a Converse message has no usable url",
            vec![user_turn(vec![image_url_part(json!({"url": ""}))])],
        ),
        (
            "document content part on a Converse message has a malformed source",
            vec![user_turn(vec![document_part(json!("not-an-object"))])],
        ),
        (
            "document content part on a Converse message names no source shape",
            vec![user_turn(vec![document_part(json!({"type": ""}))])],
        ),
        (
            "document content part on a Converse message has an unusable document format",
            vec![user_turn(vec![document_part(
                json!({"type": "base64", "media_type": 7, "data": "JVBERi0xLjQ="}),
            )])],
        ),
        (
            "document content part on a Converse message has an unusable payload",
            vec![user_turn(vec![document_part(
                json!({"type": "text", "media_type": "text/plain", "data": 42}),
            )])],
        ),
        (
            "image block in a Converse tool result has an unusable image format",
            vec![tool_turn(vec![image_part(
                json!({"type": "base64", "media_type": 7, "data": "AAAA"}),
            )])],
        ),
        (
            "image block in a Converse tool result has an unusable base64 payload",
            vec![raw_tool_result_turn(json!([{
                "type": "image",
                "source": {"type": "base64", "media_type": "image/png", "data": 42},
            }]))],
        ),
        (
            "document block in a Converse tool result has an unusable document format",
            vec![raw_document_turn(
                json!({"type": "base64", "media_type": 7, "data": "JVBERi0xLjQ="}),
            )],
        ),
        (
            "document block in a Converse tool result has an unusable payload",
            vec![tool_turn(vec![document_part(
                json!({"type": "base64", "media_type": "application/pdf", "data": 42}),
            )])],
        ),
    ];

    // Act / Assert
    for (site, messages) in rows {
        let detail = normalize_error_detail(&messages);
        assert!(
            detail.starts_with(site),
            "the row for `{site}` reached another site: {detail}"
        );
    }
}
