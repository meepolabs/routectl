// The per-request `(bedrock-converse, <drop_class>)` counters behind the
// plain message-content representability drops: image source, image
// media_type, image_url, document source, document media_type, and the
// OpenAI-shape file part. Imports live in the host `messages_tests.rs` --
// do not add `use` lines here. Shared turn builders come from the
// image-policy and document-policy fragments.
//
// Each drop gets THREE assertions, not two: the WARN fired (through
// `capture_events`, on structured fields rather than rendered text), the
// dropped shape is ABSENT FROM THE EMITTED WIRE VALUE (the serialized
// request body, not merely the typed block vec -- a value can survive
// inside an opaque payload while the typed view looks clean), and a
// representable sibling SURVIVES in that same emitted value.
//
// Every test here is guarded with the drop_class's own serial name because
// the counter registry is process-global and this crate's runner is
// threaded; the guard must also cover any test that reaches the same arm
// incidentally.

/// The current `(bedrock-converse, class)` drop count, read through the
/// same snapshot surface the router's doctor path reads.
fn converse_drop_count(class: &str) -> u64 {
    crate::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == "bedrock-converse" && e.drop_class == class)
        .map_or(0, |e| e.drop_count)
}

/// The EMITTED WIRE VALUE for `messages`: the translated blocks serialized
/// exactly as they ride to AWS. Assertions about a drop must run against
/// this, not against the typed vec -- an opaque `Other` block or a JSON
/// tool-result payload can carry a field the typed view never shows.
fn emitted_wire_value(messages: &[Message]) -> Value {
    let translated = build_messages(TEST_ID, messages).expect("translation must succeed");
    serde_json::to_value(&translated).expect("the Converse message vec must serialize")
}

/// Every `text` member appearing anywhere in the emitted wire value, so a
/// positive control can assert a representable sibling survived without
/// hard-coding the block's position.
fn wire_texts(wire: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect_string_members(wire, "text", &mut out);
    out
}

/// Recursively collect every string value stored under `key`.
fn collect_string_members(v: &Value, key: &str, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                if k == key
                    && let Some(s) = child.as_str()
                {
                    out.push(s.to_string());
                }
                collect_string_members(child, key, out);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_string_members(item, key, out);
            }
        }
        _ => {}
    }
}

/// True when `needle` appears anywhere in the emitted wire value's JSON
/// text. Used for the absence assertion: the dropped payload must not ride
/// to the upstream under ANY key, including inside an opaque blob.
fn wire_contains(wire: &Value, needle: &str) -> bool {
    wire.to_string().contains(needle)
}

/// The three assertions for one content drop, plus the counter delta.
///
/// `dropped_payload` is a value unique to the part expected to drop, so
/// finding it anywhere in the emitted body means the drop never happened.
/// `sibling_text` is the representable part that must survive alongside.
fn assert_content_drop(
    class: &str,
    messages: &[Message],
    warn_needle: &str,
    dropped_payload: &str,
    sibling_text: &str,
) {
    // Arrange
    let before = converse_drop_count(class);

    // Act
    let mut wire = Value::Null;
    let events = capture_events(|| {
        wire = emitted_wire_value(messages);
    });
    let after = converse_drop_count(class);

    // Assert 1 -- the WARN fired, captured structurally.
    assert!(
        events
            .iter()
            .any(|e| e.level == tracing::Level::WARN && e.message.contains(warn_needle)),
        "the drop must stay observable through its WARN `{warn_needle}`; got: {events:?}"
    );

    // Assert 2 -- the dropped payload is absent from the EMITTED WIRE
    // VALUE, not merely from the typed block view. A warn plus a counter
    // that describe a removal the wire never performed is the overclaim
    // this assertion exists to refuse.
    assert!(
        !wire_contains(&wire, dropped_payload),
        "`{dropped_payload}` must not reach the upstream in any form; emitted body: {wire}"
    );

    // Assert 3 -- positive control: the representable sibling survived in
    // that same emitted value, proving the fixture would have shown the
    // payload had it ridden along.
    assert!(
        wire_texts(&wire).iter().any(|t| t == sibling_text),
        "the representable sibling `{sibling_text}` must survive the drop; emitted body: {wire}"
    );

    // The counter advances exactly once for the request.
    assert_eq!(
        after - before,
        1,
        "the `{class}` counter must advance by exactly one for this request"
    );
}

/// A url-shape image source is well-formed and unrepresentable on this JSON
/// wire, so the part drops while the anchor text ships.
#[test]
#[serial_test::serial(bedrock_converse_image_source_unrepresentable)]
fn unrepresentable_image_source_bumps_the_drop_counter_once() {
    // Arrange
    let messages = vec![user_turn(vec![
        text_part("anchor-image-source"),
        image_part(json!({"type": "url", "url": "https://example.invalid/sentinel-src.png"})),
    ])];

    // Act / Assert
    assert_content_drop(
        "image_source_unrepresentable",
        &messages,
        "dropping non-base64 image source on Converse egress",
        "sentinel-src.png",
        "anchor-image-source",
    );
}

/// Two unrepresentable image sources in ONE request is one drop EVENT, not
/// two: the counter is flushed once per `build_messages` call, which is the
/// placement this assertion pins.
#[test]
#[serial_test::serial(bedrock_converse_image_source_unrepresentable)]
fn two_unrepresentable_image_sources_bump_the_drop_counter_once() {
    // Arrange
    let messages = vec![user_turn(vec![
        text_part("anchor"),
        image_part(json!({"type": "url", "url": "https://example.invalid/a.png"})),
        image_part(json!({"type": "url", "url": "https://example.invalid/b.png"})),
    ])];
    let before = converse_drop_count("image_source_unrepresentable");

    // Act
    build_messages(TEST_ID, &messages).expect("unrepresentable sources must not fail");
    let after = converse_drop_count("image_source_unrepresentable");

    // Assert
    assert_eq!(
        after - before,
        1,
        "two dropped images in one request is one drop event, not two"
    );
}

/// A structurally complete image whose media_type is outside AWS's format
/// table drops on representability, distinctly from the source-shape class.
#[test]
#[serial_test::serial(bedrock_converse_image_media_type_unsupported)]
fn unmapped_image_media_type_bumps_the_drop_counter_once() {
    // Arrange
    let messages = vec![user_turn(vec![
        text_part("anchor-image-media"),
        image_part(json!({
            "type": "base64",
            "media_type": "image/tiff",
            "data": "SENTINELIMAGEBYTES",
        })),
    ])];

    // Act / Assert
    assert_content_drop(
        "image_media_type_unsupported",
        &messages,
        "dropping image with unmapped media_type on Converse egress",
        "SENTINELIMAGEBYTES",
        "anchor-image-media",
    );
}

/// A non-data-URI `image_url` is a location this JSON wire cannot
/// dereference.
#[test]
#[serial_test::serial(bedrock_converse_image_url_unrepresentable)]
fn unrepresentable_image_url_bumps_the_drop_counter_once() {
    // Arrange
    let messages = vec![user_turn(vec![
        text_part("anchor-image-url"),
        image_url_part(json!({"url": "https://example.invalid/sentinel-url.png"})),
    ])];

    // Act / Assert
    assert_content_drop(
        "image_url_unrepresentable",
        &messages,
        "dropping image_url on Converse egress",
        "sentinel-url.png",
        "anchor-image-url",
    );
}

/// An unrecognized document source kind is unrepresentable rather than
/// malformed: the kind may be a vendor shape a later build learns.
#[test]
#[serial_test::serial(bedrock_converse_document_source_unrepresentable)]
fn unrepresentable_document_source_bumps_the_drop_counter_once() {
    // Arrange
    let messages = vec![user_turn(vec![
        text_part("anchor-doc-source"),
        document_part(json!({
            "type": "some-future-source-kind",
            "media_type": "application/pdf",
            "data": "SENTINELDOCSOURCE",
        })),
    ])];

    // Act / Assert
    assert_content_drop(
        "document_source_unrepresentable",
        &messages,
        "dropping unsupported document source type on Converse egress",
        "SENTINELDOCSOURCE",
        "anchor-doc-source",
    );
}

/// A document whose media_type is outside AWS's format table drops on
/// representability.
#[test]
#[serial_test::serial(bedrock_converse_document_media_type_unsupported)]
fn unmapped_document_media_type_bumps_the_drop_counter_once() {
    // Arrange
    let messages = vec![user_turn(vec![
        text_part("anchor-doc-media"),
        document_part(json!({
            "type": "base64",
            "media_type": "application/x-sentinel-type",
            "data": "SENTINELDOCBYTES",
        })),
    ])];

    // Act / Assert
    assert_content_drop(
        "document_media_type_unsupported",
        &messages,
        "dropping document with unmapped media_type on Converse egress",
        "SENTINELDOCBYTES",
        "anchor-doc-media",
    );
}

/// An OpenAI-shape file part with no inline base64 PDF bytes has no
/// Converse carrier -- the JSON wire cannot hold a raw file block, so
/// passthrough is not available the way it is for `ContentPart::Other`.
#[test]
#[serial_test::serial(bedrock_converse_file_part_unrepresentable)]
fn untranslatable_file_part_bumps_the_drop_counter_once() {
    // Arrange -- a file_id-only reference names bytes that live upstream.
    let messages = vec![user_turn(vec![
        text_part("anchor-file"),
        file_part(json!({"file_id": "file-sentinel-reference"})),
    ])];

    // Act / Assert
    assert_content_drop(
        "file_part_unrepresentable",
        &messages,
        "dropping file part on Converse egress",
        "file-sentinel-reference",
        "anchor-file",
    );
}

/// POSITIVE CONTROL for the whole group: a request carrying only
/// representable siblings of every shape above -- a base64 PNG, a base64
/// PDF document, a base64 PDF file part -- advances NO content-drop counter
/// and emits no drop WARN. Without this, every assertion above could pass
/// against an implementation that counted unconditionally.
///
/// Guarded on ALL SIX class names in one `serial` call, not one name per
/// attribute: this test reads every counter in the group back, so a
/// concurrent test bumping any of them lands inside this before/after
/// window. Stacked single-key attributes would leave five of the six
/// unguarded, and the resulting failure is invisible single-threaded.
#[test]
#[serial_test::serial(
    bedrock_converse_image_source_unrepresentable,
    bedrock_converse_image_media_type_unsupported,
    bedrock_converse_image_url_unrepresentable,
    bedrock_converse_document_source_unrepresentable,
    bedrock_converse_document_media_type_unsupported,
    bedrock_converse_file_part_unrepresentable
)]
fn representable_content_parts_advance_no_content_drop_counter() {
    // Arrange
    let classes = [
        "image_source_unrepresentable",
        "image_media_type_unsupported",
        "image_url_unrepresentable",
        "document_source_unrepresentable",
        "document_media_type_unsupported",
        "file_part_unrepresentable",
    ];
    let before: Vec<u64> = classes.iter().map(|c| converse_drop_count(c)).collect();
    let messages = vec![user_turn(vec![
        text_part("survivor"),
        image_part(json!({
            "type": "base64",
            "media_type": "image/png",
            "data": "AAAA",
        })),
        image_url_part(json!({"url": "data:image/png;base64,BBBB"})),
        document_part(json!({
            "type": "base64",
            "media_type": "application/pdf",
            "data": "SURVIVINGDOC",
        })),
        file_part(json!({
            "filename": "notes.pdf",
            "file_data": "data:application/pdf;base64,SURVIVINGFILE",
        })),
    ])];

    // Act
    let mut wire = Value::Null;
    let events = capture_events(|| {
        wire = emitted_wire_value(&messages);
    });
    let after: Vec<u64> = classes.iter().map(|c| converse_drop_count(c)).collect();

    // Assert -- every representable payload reached the wire.
    for payload in ["SURVIVINGDOC", "SURVIVINGFILE"] {
        assert!(
            wire_contains(&wire, payload),
            "`{payload}` is representable and must reach the upstream; emitted body: {wire}"
        );
    }
    assert!(
        wire_texts(&wire).iter().any(|t| t == "survivor"),
        "the anchor text must survive; emitted body: {wire}"
    );
    assert!(
        !events
            .iter()
            .any(|e| e.level == tracing::Level::WARN && e.message.contains("dropping")),
        "nothing was unrepresentable, so no drop WARN is owed; got: {events:?}"
    );
    assert_eq!(
        after, before,
        "a request with nothing dropped must advance no content-drop counter"
    );
}
