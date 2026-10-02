// The document `source` member on every document-carrying path of the
// Converse egress: a text document with citations enabled ships its raw
// body as `source.text`, because Converse rejects `source.bytes` on a
// cited text-format document; every other combination ships
// `source.bytes`. Imports live in the host `messages_tests.rs` -- do not
// add `use` lines here. Shared turn builders come from the image-policy
// fragment.

/// A plain body carrying a quote and a newline, so a test can tell the raw
/// text from any re-encoding of it.
const CITED_BODY: &str = "line one\nline \"two\"";

fn text_source() -> Value {
    json!({"type": "text", "media_type": "text/plain", "data": CITED_BODY})
}

fn base64_text_source() -> Value {
    json!({
        "type": "base64",
        "media_type": "text/plain",
        "data": B64_STANDARD.encode(CITED_BODY),
    })
}

fn citations_enabled() -> Option<Value> {
    Some(json!({"enabled": true}))
}

fn document_part_with_citations(source: Value, citations: Option<Value>) -> ContentPart {
    ContentPart::Known(KnownContentPart::Document {
        source,
        title: Some("notes".into()),
        citations,
        cache_control: None,
    })
}

/// The emitted `source` member of the single plain-message document.
fn message_document_source(source: Value, citations: Option<Value>) -> Value {
    let messages = [user_turn(vec![document_part_with_citations(
        source, citations,
    )])];
    let blocks = only_message_blocks(&messages);
    let document = blocks
        .iter()
        .find(|b| matches!(b, ConverseContentBlock::Document { .. }))
        .unwrap_or_else(|| panic!("a Document block must be emitted, got: {blocks:?}"));
    serde_json::to_value(document).expect("a Document block serializes")["document"]["source"]
        .clone()
}

/// The emitted `source` member of the first document in the single
/// translated toolResult block.
fn tool_result_document_source(messages: &[Message]) -> Value {
    let blocks = only_message_blocks(messages);
    let tool_result = blocks
        .iter()
        .find(|b| matches!(b, ConverseContentBlock::ToolResult { .. }))
        .unwrap_or_else(|| panic!("a toolResult block must be emitted, got: {blocks:?}"));
    let wire = serde_json::to_value(tool_result).expect("a toolResult block serializes");
    wire["toolResult"]["content"][0]["document"]["source"].clone()
}

/// The emitted `source` member for a canonical Parts tool_result document.
fn parts_tool_result_document_source(source: Value, citations: Option<Value>) -> Value {
    tool_result_document_source(&[tool_turn(vec![document_part_with_citations(
        source, citations,
    )])])
}

/// The emitted `source` member for a raw Anthropic-shape tool_result
/// document element.
fn raw_tool_result_document_source(source: Value, citations: Option<Value>) -> Value {
    let mut element = json!({"type": "document", "source": source, "title": "notes"});
    if let Some(citations) = citations {
        element["citations"] = citations;
    }
    tool_result_document_source(&[raw_tool_result_turn(json!([element]))])
}

// ---------------------------------------------------------------------------
// Plain message document
// ---------------------------------------------------------------------------

#[test]
fn message_text_document_with_citations_ships_the_raw_text() {
    // Arrange / Act
    let source = message_document_source(text_source(), citations_enabled());

    // Assert
    assert_eq!(source, json!({"text": CITED_BODY}));
}

#[test]
fn message_text_document_without_citations_ships_base64_bytes() {
    // Arrange / Act
    let source = message_document_source(text_source(), None);

    // Assert
    assert_eq!(source, json!({"bytes": B64_STANDARD.encode(CITED_BODY)}));
}

/// `{enabled: false}` means citations are off, so it takes the bytes form
/// exactly like an absent value.
#[test]
fn message_text_document_with_citations_disabled_ships_base64_bytes() {
    // Arrange / Act
    let source = message_document_source(text_source(), Some(json!({"enabled": false})));

    // Assert
    assert_eq!(source, json!({"bytes": B64_STANDARD.encode(CITED_BODY)}));
}

/// A base64 source is never decoded back to text, even when citations are
/// enabled and the decoded bytes would be valid UTF-8.
#[test]
fn message_base64_document_with_citations_keeps_its_bytes() {
    // Arrange / Act
    let source = message_document_source(base64_text_source(), citations_enabled());

    // Assert
    assert_eq!(source, json!({"bytes": B64_STANDARD.encode(CITED_BODY)}));
}

// ---------------------------------------------------------------------------
// Canonical Parts tool_result document
// ---------------------------------------------------------------------------

#[test]
fn parts_tool_result_text_document_with_citations_ships_the_raw_text() {
    // Arrange / Act
    let source = parts_tool_result_document_source(text_source(), citations_enabled());

    // Assert
    assert_eq!(source, json!({"text": CITED_BODY}));
}

#[test]
fn parts_tool_result_text_document_without_citations_ships_base64_bytes() {
    // Arrange / Act
    let source = parts_tool_result_document_source(text_source(), None);

    // Assert
    assert_eq!(source, json!({"bytes": B64_STANDARD.encode(CITED_BODY)}));
}

#[test]
fn parts_tool_result_base64_document_with_citations_keeps_its_bytes() {
    // Arrange / Act
    let source = parts_tool_result_document_source(base64_text_source(), citations_enabled());

    // Assert
    assert_eq!(source, json!({"bytes": B64_STANDARD.encode(CITED_BODY)}));
}

// ---------------------------------------------------------------------------
// Raw Anthropic-shape tool_result document element
// ---------------------------------------------------------------------------

#[test]
fn raw_tool_result_text_document_with_citations_ships_the_raw_text() {
    // Arrange / Act
    let source = raw_tool_result_document_source(text_source(), citations_enabled());

    // Assert
    assert_eq!(source, json!({"text": CITED_BODY}));
}

#[test]
fn raw_tool_result_text_document_without_citations_ships_base64_bytes() {
    // Arrange / Act
    let source = raw_tool_result_document_source(text_source(), None);

    // Assert
    assert_eq!(source, json!({"bytes": B64_STANDARD.encode(CITED_BODY)}));
}

/// An unrecognized citations shape is dropped (and counted), so the
/// document ships as though citations were off.
#[test]
fn raw_tool_result_text_document_with_unrecognized_citations_ships_base64_bytes() {
    // Arrange / Act
    let source = raw_tool_result_document_source(text_source(), Some(json!("yes")));

    // Assert
    assert_eq!(source, json!({"bytes": B64_STANDARD.encode(CITED_BODY)}));
}

#[test]
fn raw_tool_result_base64_document_with_citations_keeps_its_bytes() {
    // Arrange / Act
    let source = raw_tool_result_document_source(base64_text_source(), citations_enabled());

    // Assert
    assert_eq!(source, json!({"bytes": B64_STANDARD.encode(CITED_BODY)}));
}
