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
fn user_file_with_no_carrier_fails_the_request() {
    // Arrange: a file part with neither file_data nor file_id -- it names
    // no bytes, so it is malformed rather than unrepresentable.
    let req = req_with(vec![user_file(json!({"filename": "empty.pdf"}))]);

    // Act
    let msg = translate_err(&cfg(), &req);

    // Assert: the error names the missing carriers.
    assert!(msg.contains("file_data"), "message was: {msg}");
    assert!(msg.contains("file_id"), "message was: {msg}");
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
// malformed parts fail the request (a part that names no bytes)
// ---------------------------------------------------------------------------

#[test]
fn user_image_url_part_with_empty_url_fails_the_request() {
    // Arrange: OpenAI-shape image_url block whose url is present but
    // empty. This once took the success branch and shipped an empty
    // image_url upstream with no warning and no drop.
    let req = req_with(vec![user_parts(vec![image_url_part(json!({"url": ""}))])]);

    // Act
    let msg = translate_err(&cfg(), &req);

    // Assert
    assert!(msg.contains("image_url.url"), "message was: {msg}");
}

#[test]
fn tool_result_image_url_part_with_empty_url_fails_the_request() {
    // Arrange: the same empty-url shape on the tool-result path.
    let req = req_with(vec![
        user_text("shot"),
        tool_message_parts("call_1", vec![image_url_part(json!({"url": ""}))]),
    ]);

    // Act
    let msg = translate_err(&cfg(), &req);

    // Assert
    assert!(msg.contains("tool result"), "message was: {msg}");
    assert!(msg.contains("image_url.url"), "message was: {msg}");
}

#[test]
fn user_image_url_part_with_missing_url_fails_the_request() {
    // Arrange: no url field at all.
    let req = req_with(vec![user_parts(vec![image_url_part(
        json!({"detail": "high"}),
    )])]);

    // Act
    let msg = translate_err(&cfg(), &req);

    // Assert
    assert!(msg.contains("image_url.url"), "message was: {msg}");
}

#[test]
fn tool_result_image_url_part_with_missing_url_fails_the_request() {
    // Arrange
    let req = req_with(vec![
        user_text("shot"),
        tool_message_parts("call_1", vec![image_url_part(json!({"detail": "low"}))]),
    ]);

    // Act
    let msg = translate_err(&cfg(), &req);

    // Assert
    assert!(msg.contains("image_url.url"), "message was: {msg}");
}

#[test]
fn user_image_source_with_empty_base64_data_fails_the_request() {
    // Arrange: a base64 source whose data is empty names no bytes.
    let req = req_with(vec![user_parts(vec![image_part(json!({
        "type": "base64",
        "media_type": "image/png",
        "data": ""
    }))])]);

    // Act
    let msg = translate_err(&cfg(), &req);

    // Assert: the field is named and no raw content value is echoed.
    assert!(msg.contains("source.data"), "message was: {msg}");
    assert!(!msg.contains("image/png"), "message was: {msg}");
}

#[test]
fn user_image_source_with_empty_url_fails_the_request() {
    // Arrange
    let req = req_with(vec![user_parts(vec![image_part(
        json!({"type": "url", "url": ""}),
    )])]);

    // Act
    let msg = translate_err(&cfg(), &req);

    // Assert
    assert!(msg.contains("source.url"), "message was: {msg}");
}

#[test]
fn tool_result_image_source_with_empty_base64_data_fails_the_request() {
    // Arrange
    let req = req_with(vec![
        user_text("shot"),
        tool_message_parts(
            "call_1",
            vec![image_part(json!({
                "type": "base64",
                "media_type": "image/png",
                "data": ""
            }))],
        ),
    ]);

    // Act
    let msg = translate_err(&cfg(), &req);

    // Assert
    assert!(msg.contains("source.data"), "message was: {msg}");
    assert!(msg.contains("tool result"), "message was: {msg}");
}

#[test]
fn tool_result_image_source_with_empty_url_fails_the_request() {
    // Arrange
    let req = req_with(vec![
        user_text("shot"),
        tool_message_parts(
            "call_1",
            vec![image_part(json!({"type": "url", "url": ""}))],
        ),
    ]);

    // Act
    let msg = translate_err(&cfg(), &req);

    // Assert
    assert!(msg.contains("source.url"), "message was: {msg}");
    assert!(msg.contains("tool result"), "message was: {msg}");
}

#[test]
fn valid_content_beside_a_malformed_part_is_never_reached() {
    // Arrange: valid text ahead of a malformed image part. The whole
    // request fails rather than shipping a partial turn.
    let req = req_with(vec![user_parts(vec![
        text_part("look at this"),
        image_url_part(json!({"url": ""})),
    ])]);

    // Act
    let msg = translate_err(&cfg(), &req);

    // Assert
    assert!(msg.contains("image_url.url"), "message was: {msg}");
}

// ---------------------------------------------------------------------------
// unrepresentable parts still warn-drop, and their neighbours still ship
// ---------------------------------------------------------------------------

#[test]
fn user_forward_compat_part_drops_and_neighbouring_text_still_ships() {
    // Arrange: an unknown part type beside valid text. The part is
    // well-formed, just unrepresentable here, so the turn still ships.
    let req = req_with(vec![user_parts(vec![
        text_part("hello"),
        ContentPart::Other {
            type_tag: "video_url".into(),
            cache_control: None,
            extras: serde_json::Map::new(),
        },
    ])]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: the text survives; the unknown part is gone.
    assert_eq!(
        v["input"][0]["content"],
        json!([{"type": "input_text", "text": "hello"}])
    );
}

#[test]
fn tool_result_unrepresentable_part_drops_and_image_still_ships() {
    // Arrange: a Document part (no Responses slot) beside a valid image
    // inside a tool result.
    let parts = vec![
        ContentPart::Known(KnownContentPart::Document {
            source: json!({"type": "base64", "media_type": "application/pdf", "data": "JVBER"}),
            title: None,
            citations: None,
            cache_control: None,
        }),
        image_part(json!({"type": "url", "url": "https://example.com/shot.png"})),
    ];
    let req = req_with(vec![user_text("shot"), tool_message_parts("call_3", parts)]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: only the image survives; the request was not rejected.
    assert_eq!(
        v["input"][1]["output"],
        json!([{"type": "input_image", "image_url": "https://example.com/shot.png"}])
    );
}

#[test]
fn tool_result_unknown_image_source_kind_drops_and_text_still_ships() {
    // Arrange: an unknown source kind is forward-compat, not malformed.
    let parts = vec![
        text_part("here"),
        image_part(json!({"type": "s3", "bucket": "b", "key": "k.png"})),
    ];
    let req = req_with(vec![user_text("shot"), tool_message_parts("call_4", parts)]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(
        v["input"][1]["output"],
        json!([{"type": "input_text", "text": "here"}])
    );
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

// ---------------------------------------------------------------------------
// client_metadata: installation id (ChatgptOauth lane only)
// ---------------------------------------------------------------------------

/// A ChatgptOauth config carrying a resolved installation id.
fn cfg_with_installation_id(installation_id: &str) -> OpenAiResponsesConfig {
    let mut c = cfg();
    c.installation_id = Some(installation_id.into());
    c
}

#[test]
fn installation_id_stamped_into_client_metadata_on_chatgpt_oauth() {
    // Arrange
    let req = req_with(vec![user_text("ping")]);

    // Act
    let v = translate_to_json(&cfg_with_installation_id("iid-abc"), &req);

    // Assert
    assert_eq!(
        v["client_metadata"],
        json!({"x-codex-installation-id": "iid-abc"}),
        "got: {v}"
    );
}

#[test]
fn installation_id_merges_alongside_request_client_metadata_keys() {
    // Arrange
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({
        "client_metadata": {"user_id": "u-123"}
    }));

    // Act
    let v = translate_to_json(&cfg_with_installation_id("iid-abc"), &req);

    // Assert
    assert_eq!(
        v["client_metadata"],
        json!({"user_id": "u-123", "x-codex-installation-id": "iid-abc"}),
        "got: {v}"
    );
}

#[test]
fn resolved_installation_id_wins_over_request_supplied_value() {
    // Arrange: a request-reachable client_metadata already carrying the key.
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({
        "client_metadata": {"x-codex-installation-id": "spoofed-iid"}
    }));

    // Act
    let v = translate_to_json(&cfg_with_installation_id("iid-abc"), &req);

    // Assert
    assert_eq!(
        v["client_metadata"]["x-codex-installation-id"], "iid-abc",
        "resolved installation id must win over a request-supplied one: {v}"
    );
}

#[test]
fn non_object_client_metadata_is_replaced_with_installation_id_object() {
    // Arrange: a request-supplied non-object client_metadata, which would
    // otherwise suppress the stamp entirely.
    let mut req = req_with(vec![user_text("ping")]);
    req.provider_extras = Some(json!({
        "client_metadata": "not-an-object"
    }));

    // Act
    let v = translate_to_json(&cfg_with_installation_id("iid-abc"), &req);

    // Assert
    assert_eq!(
        v["client_metadata"],
        json!({"x-codex-installation-id": "iid-abc"}),
        "non-object client_metadata must be replaced by the stamped object: {v}"
    );
}

#[test]
fn absent_installation_id_creates_no_client_metadata_object() {
    // Arrange: the default ChatgptOauth cfg resolves no installation id.
    let req = req_with(vec![user_text("ping")]);
    assert!(
        cfg().installation_id.is_none(),
        "fixture guard: cfg() must carry no installation_id"
    );

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert!(
        v.get("client_metadata").is_none(),
        "no installation_id must leave client_metadata absent: {v}"
    );
}

#[test]
fn api_key_lane_body_omits_installation_id_from_client_metadata() {
    // Arrange: an ApiKey config carrying an installation_id, which would
    // be stamped on the ChatgptOauth lane.
    let mut c = cfg_api_key();
    c.installation_id = Some("iid-abc".into());
    assert_eq!(
        c.auth_kind,
        AuthKind::ApiKey,
        "fixture guard: this negative must run on the ApiKey lane"
    );
    let req = req_with(vec![user_text("ping")]);

    // Act
    let v = translate_to_json(&c, &req);

    // Assert
    assert!(
        v.get("client_metadata").is_none(),
        "ApiKey lane must carry no codex installation id: {v}"
    );
}
