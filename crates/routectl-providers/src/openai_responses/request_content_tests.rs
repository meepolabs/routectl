// Per-part content coverage: user image parts, user file parts, tool results
// carrying image parts, and the client_metadata passthrough. `include!`d into
// `request_tests.rs`; all top-level imports live there, so do not add `use`
// lines here.

// ---------------------------------------------------------------------------
// user image content
// ---------------------------------------------------------------------------

#[test]
fn user_image_base64_translates_to_input_image_data_url() {
    // Arrange: user turn containing a base64 PNG image.
    let req = req_with(vec![user_image_base64("image/png", "AAAA")]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: content block becomes {type:"input_image",
    // image_url:"data:image/png;base64,AAAA"}.
    let content = &v["input"][0]["content"][0];
    assert_eq!(content["type"], "input_image");
    assert_eq!(content["image_url"], "data:image/png;base64,AAAA");
    // detail is absent (None -> omitted).
    assert!(content.get("detail").is_none());
}

#[test]
fn user_image_url_translates_to_input_image_url() {
    // Arrange: user turn carrying an https URL image source.
    let req = req_with(vec![user_image_url("https://example.com/cat.jpg")]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    let content = &v["input"][0]["content"][0];
    assert_eq!(content["type"], "input_image");
    assert_eq!(content["image_url"], "https://example.com/cat.jpg");
}

#[test]
fn user_image_unknown_source_kind_warns_and_drops() {
    // Arrange: source.type is an unsupported kind (forward-compat
    // extension). The part should be dropped; the message item should
    // still be emitted but with no content blocks (empty -> skipped).
    let msg = Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Image {
            source: json!({"type": "s3", "bucket": "my-bucket", "key": "img.png"}),
            cache_control: None,
        })]),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    };
    let req = req_with(vec![msg]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: the single unknown-source image was dropped so the user
    // message has no content and was skipped entirely.
    assert_eq!(v["input"], json!([]));
}

// ---------------------------------------------------------------------------
// user file content (OpenAI-shape File -> Responses input_file)
// ---------------------------------------------------------------------------

#[test]
fn user_file_data_translates_to_input_file_with_filename() {
    // Arrange: OpenAI-shape file part carrying inline base64 + filename.
    let req = req_with(vec![user_file(json!({
        "filename": "draft.pdf",
        "file_data": "data:application/pdf;base64,JVBER"
    }))]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: an input_file item carries file_data + filename (no drop).
    let content = &v["input"][0]["content"][0];
    assert_eq!(content["type"], "input_file");
    assert_eq!(content["file_data"], "data:application/pdf;base64,JVBER");
    assert_eq!(content["filename"], "draft.pdf");
    assert!(content.get("file_id").is_none());
}

#[test]
fn user_file_id_only_translates_to_input_file_with_file_id() {
    // Arrange: OpenAI-shape file part referencing a prior upload.
    let req = req_with(vec![user_file(json!({"file_id": "file-abc123"}))]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: input_file item carries file_id; file_data/filename absent.
    let content = &v["input"][0]["content"][0];
    assert_eq!(content["type"], "input_file");
    assert_eq!(content["file_id"], "file-abc123");
    assert!(content.get("file_data").is_none());
    assert!(content.get("filename").is_none());
}

#[test]
fn user_file_with_no_carrier_is_dropped() {
    // Arrange: a file part with neither file_data nor file_id.
    let req = req_with(vec![user_file(json!({"filename": "empty.pdf"}))]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: nothing to forward; the user message is skipped entirely.
    assert_eq!(v["input"], json!([]));
}

#[test]
fn user_document_anthropic_shape_still_drops() {
    // Arrange: Anthropic-shape Document part (out of scope for the
    // codex target; remains dropped at parity with the reference).
    let msg = Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Document {
            source: json!({
                "type": "base64",
                "media_type": "application/pdf",
                "data": "JVBER"
            }),
            title: Some("spec.pdf".into()),
            citations: None,
            cache_control: None,
        })]),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    };
    let req = req_with(vec![msg]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: Document is dropped; no content -> message skipped.
    assert_eq!(v["input"], json!([]));
}

// ---------------------------------------------------------------------------
// tool result with image parts
// ---------------------------------------------------------------------------

#[test]
fn tool_role_text_only_translates_to_string_output() {
    // Arrange: single text part -- common path.
    let req = req_with(vec![user_text("run"), tool_message("call_1", "result")]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: output is a flat string.
    let fco = &v["input"][1];
    assert_eq!(fco["type"], "function_call_output");
    assert_eq!(fco["call_id"], "call_1");
    assert_eq!(fco["output"], json!("result"));
}

#[test]
fn tool_role_with_image_part_translates_to_items_array() {
    // Arrange: tool result contains only an image (e.g. screenshot tool).
    let parts = vec![ContentPart::Known(KnownContentPart::Image {
        source: json!({
            "type": "base64",
            "media_type": "image/png",
            "data": "iVBORw"
        }),
        cache_control: None,
    })];
    let req = req_with(vec![
        user_text("screenshot"),
        tool_message_parts("call_9", parts),
    ]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: output is an items array with one input_image entry.
    let fco = &v["input"][1];
    assert_eq!(fco["type"], "function_call_output");
    assert_eq!(
        fco["output"],
        json!([
            {"type": "input_image", "image_url": "data:image/png;base64,iVBORw"}
        ])
    );
}

#[test]
fn tool_role_mixed_text_and_image_emits_items_array() {
    // Arrange: tool result has both text and an image.
    let parts = vec![
        ContentPart::Known(KnownContentPart::Text {
            text: "here is the screenshot".into(),
            citations: None,
            cache_control: None,
        }),
        ContentPart::Known(KnownContentPart::Image {
            source: json!({
                "type": "url",
                "url": "https://example.com/shot.png"
            }),
            cache_control: None,
        }),
    ];
    let req = req_with(vec![
        user_text("screenshot"),
        tool_message_parts("call_7", parts),
    ]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: mixed -> items array with both kinds present.
    let fco = &v["input"][1];
    assert_eq!(
        fco["output"],
        json!([
            {"type": "input_text", "text": "here is the screenshot"},
            {"type": "input_image", "image_url": "https://example.com/shot.png"}
        ])
    );
}

// ---------------------------------------------------------------------------
// client_metadata passthrough
// ---------------------------------------------------------------------------

#[test]
fn provider_extras_client_metadata_forwards() {
    // Arrange
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({
        "client_metadata": {"user_id": "u-123", "session": "s-abc"}
    }));

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: client_metadata forwarded verbatim.
    assert_eq!(
        v["client_metadata"],
        json!({"user_id": "u-123", "session": "s-abc"})
    );
}
