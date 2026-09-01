// The two-class content policy applied to every document-carrying path on
// the Converse egress: MALFORMED (the caller asked to send a document and
// named none) fails the request; UNREPRESENTABLE (a well-formed document
// this JSON wire cannot carry) keeps its warn-drop or its JSON fallback.
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
// MALFORMED -- the regression probe: a document that names no payload
// ---------------------------------------------------------------------------

/// The core defect. A base64 document source whose `data` is present but
/// empty names no bytes at all, yet the pre-fix egress built
/// `source: {bytes: ""}` and shipped an EMPTY DOCUMENT upstream with HTTP
/// 200, no WARN and no drop: the caller believes the model read a document,
/// and it read nothing. Naming no payload is malformed at every egress, so
/// the request must fail instead.
#[test]
fn document_source_with_empty_data_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![document_part(json!({
        "type": "base64",
        "media_type": "application/pdf",
        "data": "",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

/// The same defect through the absent-field door: `data` missing entirely
/// read as `""` and shipped as an empty document.
#[test]
fn document_source_with_absent_data_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![document_part(json!({
        "type": "base64",
        "media_type": "application/pdf",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

/// A non-string `data` names no payload either -- the pre-fix `as_str()`
/// fell through to the same empty-document emit.
#[test]
fn document_source_with_non_string_data_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![document_part(json!({
        "type": "base64",
        "media_type": "application/pdf",
        "data": 42,
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

/// The `text` source kind carries the identical hole: an empty UTF-8 body
/// base64-encodes to an empty payload, so the pre-fix egress shipped a
/// document with no content on an `Ok` here too.
#[test]
fn text_document_source_with_empty_data_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![document_part(json!({
        "type": "text",
        "media_type": "text/plain",
        "data": "",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

/// The text kind, absent-field door.
#[test]
fn text_document_source_with_absent_data_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![document_part(json!({
        "type": "text",
        "media_type": "text/plain",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

/// A `source` that is not a JSON object carries no field to read. The
/// pre-fix path warn-dropped this, losing the content on a 200.
#[test]
fn document_source_that_is_not_an_object_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![document_part(json!("not-an-object"))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source");
}

/// An absent `source.type` leaves the source shape unnamed.
#[test]
fn document_source_with_absent_type_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![document_part(json!({
        "media_type": "application/pdf",
        "data": "JVBERi0xLjQ=",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.type");
}

/// An empty `source.type` is the absent case wearing a string. The pre-fix
/// path read it as a string and warn-dropped it as an unsupported kind.
#[test]
fn document_source_with_empty_type_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![document_part(json!({
        "type": "",
        "media_type": "application/pdf",
        "data": "JVBERi0xLjQ=",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.type");
}

/// A wire-carryable source with no `media_type` cannot name an AWS document
/// format, and the field is required rather than defaultable: guessing a
/// format ships bytes the model decodes as the wrong thing.
#[test]
fn document_source_with_absent_media_type_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![document_part(json!({
        "type": "base64",
        "data": "JVBERi0xLjQ=",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.media_type");
}

/// An empty `media_type`, same class as absent.
#[test]
fn document_source_with_empty_media_type_fails_the_request() {
    // Arrange
    let messages = vec![user_turn(vec![document_part(json!({
        "type": "base64",
        "media_type": "",
        "data": "JVBERi0xLjQ=",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.media_type");
}

/// Required-field STRUCTURE is validated before representability. An
/// empty-`data` document whose `media_type` is also unmapped must fail as
/// malformed rather than disappearing into the unsupported-media warn-drop
/// -- the caller's request is broken, and reporting it as "this route cannot
/// carry that media type" would send them fixing the wrong thing.
#[test]
fn empty_document_data_outranks_an_unmapped_media_type() {
    // Arrange
    let messages = vec![user_turn(vec![document_part(json!({
        "type": "base64",
        "media_type": "application/vnd.sqlite3",
        "data": "",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

/// A non-string field is neither absent nor a usable value; the policy
/// treats it as the absent case, and each malformed carrier must say so by
/// naming its own field. Table-driven because the classification is one
/// rule applied across shapes, not several separate behaviors.
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
    ];

    // Act / Assert
    for (messages, field) in cases {
        assert_malformed_naming(&messages, field);
    }
}

// ---------------------------------------------------------------------------
// MALFORMED -- the tool-result carriers, within their narrower class
// ---------------------------------------------------------------------------

/// The canonical `Role::Tool` carrier of the identical defect: the same
/// empty-`data` source reached AWS as `{document: {source: {bytes: ""}}}`
/// inside a toolResult. The JSON fallback cannot rescue this shape -- a
/// wrapped source naming no bytes delivers no document either way -- so it
/// is malformed on this carrier too.
#[test]
fn tool_result_document_with_empty_data_fails_the_request() {
    // Arrange
    let messages = vec![tool_turn(vec![document_part(json!({
        "type": "base64",
        "media_type": "application/pdf",
        "data": "",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

/// The canonical tool-result carrier, absent-field door.
#[test]
fn tool_result_document_with_absent_data_fails_the_request() {
    // Arrange
    let messages = vec![tool_turn(vec![document_part(json!({
        "type": "base64",
        "media_type": "application/pdf",
    }))])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

/// The raw Anthropic-shape tool_result content array is the third carrier of
/// the same source shape, and shipped the same empty document.
#[test]
fn raw_tool_result_document_with_empty_data_fails_the_request() {
    // Arrange
    let messages = vec![raw_document_turn(json!({
        "type": "base64",
        "media_type": "application/pdf",
        "data": "",
    }))];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

/// The raw array carrier, absent-field door. Note the pre-fix code defaulted
/// an absent source `type` to `"base64"` here, so this shape reached the
/// wire as an empty document rather than taking the fallback.
#[test]
fn raw_tool_result_document_with_absent_data_fails_the_request() {
    // Arrange
    let messages = vec![raw_document_turn(json!({
        "type": "base64",
        "media_type": "application/pdf",
    }))];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

/// A `text` source with an empty body on the tool-result carriers, which
/// base64-encoded to an empty payload exactly as on the plain path.
#[test]
fn tool_result_text_document_with_empty_data_fails_the_request() {
    // Arrange / Act / Assert
    assert_malformed_naming(
        &[tool_turn(vec![document_part(json!({
            "type": "text",
            "media_type": "text/plain",
            "data": "",
        }))])],
        "source.data",
    );
    assert_malformed_naming(
        &[raw_document_turn(json!({
            "type": "text",
            "media_type": "text/plain",
            "data": "",
        }))],
        "source.data",
    );
}

/// A wire-carryable source naming no `media_type` on the tool-result
/// carriers: the format cannot be guessed, so this is the same malformed
/// class as on the plain path.
#[test]
fn tool_result_document_with_absent_media_type_fails_the_request() {
    // Arrange / Act / Assert
    assert_malformed_naming(
        &[tool_turn(vec![document_part(json!({
            "type": "base64",
            "data": "JVBERi0xLjQ=",
        }))])],
        "source.media_type",
    );
    assert_malformed_naming(
        &[raw_document_turn(json!({
            "type": "base64",
            "data": "JVBERi0xLjQ=",
        }))],
        "source.media_type",
    );
}

// ---------------------------------------------------------------------------
// UNREPRESENTABLE -- the warn-drops that must NOT become errors
// ---------------------------------------------------------------------------

/// A structurally complete document whose `media_type` AWS does not model is
/// unrepresentable, not malformed -- AWS's format table is bounded and the
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
/// result, so a source this egress cannot READ is unrepresentable rather
/// than malformed. Erroring instead converts a working 200 into a 400. Each
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

/// A malformed document AFTER a valid one fails the whole normalization.
/// Nothing dispatches upstream: shipping the remaining parts and returning
/// 200 is exactly the silent content loss being fixed.
#[test]
fn valid_document_then_malformed_document_fails_the_whole_request() {
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
            "data": "",
        })),
    ])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

/// The reverse ordering fails identically -- the failure cannot depend on
/// where in the turn the malformed part sits.
#[test]
fn malformed_document_then_valid_document_fails_the_whole_request() {
    // Arrange
    let messages = vec![user_turn(vec![
        document_part(json!({
            "type": "base64",
            "media_type": "application/pdf",
            "data": "",
        })),
        document_part(json!({
            "type": "base64",
            "media_type": "application/pdf",
            "data": "JVBERi0xLjQ=",
        })),
    ])];

    // Act / Assert
    assert_malformed_naming(&messages, "source.data");
}

/// `Error::NormalizeRequest` reaches the ingress as a 400 whose detail is
/// logged server-side, so the detail must name the offending FIELD and never
/// echo a caller-controlled VALUE -- payload bytes, media types and titles
/// are caller data.
#[test]
fn malformed_document_errors_never_echo_a_caller_value() {
    // Arrange: each case carries a distinctive caller value in a field the
    // error is NOT about, so an echo is unambiguous.
    let cases: Vec<(Vec<Message>, &str)> = vec![
        (
            vec![user_turn(vec![document_part(json!({
                "type": "base64",
                "media_type": "application/sentinel-media-type",
                "data": "",
            }))])],
            "application/sentinel-media-type",
        ),
        (
            vec![user_turn(vec![document_part(json!({
                "type": "base64",
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
                "data": "",
            }))])],
            "application/sentinel-media-type",
        ),
        (
            vec![raw_document_turn(json!({
                "type": "base64",
                "media_type": "application/sentinel-media-type",
                "data": "",
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
/// and an unsigned-reasoning skip recorded BEFORE a malformed document are
/// aggregate WARNs the operator is owed regardless of how the request ends;
/// without a flush on the `Err` path both are silently swallowed and only
/// the translation failure surfaces.
#[test]
#[serial_test::serial(bedrock_converse_reasoning_signature_missing_drop)]
fn tallies_recorded_before_a_malformed_document_still_flush_on_the_error_path() {
    // Arrange: turn 0 drops a citations value, turn 1 skips an unsigned
    // reasoning detail, turn 2 carries the malformed document.
    let messages = vec![
        user_turn(vec![ContentPart::Known(KnownContentPart::Document {
            source: json!({"type": "text", "media_type": "text/plain", "data": "notes"}),
            title: Some("notes".into()),
            citations: Some(json!("yes")),
            cache_control: None,
        })]),
        assistant_turn(vec![unsigned_detail(Some(0))]),
        user_turn(vec![document_part(json!({
            "type": "base64",
            "media_type": "application/pdf",
            "data": "",
        }))]),
    ];

    // Act
    let mut result_is_err = false;
    let events = capture_events(|| {
        result_is_err = build_messages(TEST_ID, &messages).is_err();
    });

    // Assert
    assert!(
        result_is_err,
        "the malformed document must fail the request"
    );
    assert!(
        events
            .iter()
            .any(|e| e.level == tracing::Level::WARN && e.field("dropped_count") == Some("1")),
        "the citations drop recorded before the error must still reach the operator; got: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.level == tracing::Level::WARN && e.field("skipped_count") == Some("1")),
        "the reasoning skip recorded before the error must still reach the operator; got: {events:?}"
    );
}
