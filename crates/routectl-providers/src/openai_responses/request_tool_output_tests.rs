// Tool-result parts on the `function_call_output` body: file parts forwarded
// as `input_file`, and the single counted drop for every part with no
// tool-output slot. `include!`d into `request_tests.rs` after
// `request_drop_policy_tests.rs`, whose `responses_drop_count` and
// `translate_capturing` helpers these tests read; all top-level imports live
// in `request_tests.rs`, so do not add `use` lines here.

fn file_part(file: Value) -> ContentPart {
    ContentPart::Known(KnownContentPart::File {
        file,
        cache_control: None,
    })
}

/// A native Responses `input_file` block as the Responses ingress parks it:
/// the forward-compat carrier with the block's flat fields as extras.
fn native_input_file_part(fields: Value) -> ContentPart {
    let Value::Object(extras) = fields else {
        panic!("native input_file fields must be an object");
    };
    ContentPart::Other {
        type_tag: "input_file".into(),
        cache_control: None,
        extras,
    }
}

#[test]
fn tool_result_file_data_ships_as_input_file_in_part_order() {
    // Arrange: a file between two text parts, so order is observable.
    let parts = vec![
        text_part("before"),
        file_part(json!({
            "filename": "notes.txt",
            "file_data": "data:text/plain;base64,S0lURS03NzMx"
        })),
        text_part("after"),
    ];
    let req = req_with(vec![
        user_text("read it"),
        tool_message_parts("call_f1", parts),
    ]);

    // Act
    let before = responses_drop_count("tool_result_part_unsupported");
    let v = translate_to_json(&cfg(), &req);
    let after = responses_drop_count("tool_result_part_unsupported");

    // Assert
    assert_eq!(
        v["input"][1]["output"],
        json!([
            {"type": "input_text", "text": "before"},
            {
                "type": "input_file",
                "file_data": "data:text/plain;base64,S0lURS03NzMx",
                "filename": "notes.txt"
            },
            {"type": "input_text", "text": "after"}
        ])
    );
    assert_eq!(after - before, 0, "a forwarded file is not a drop");
}

#[test]
fn tool_result_file_id_only_ships_as_input_file_with_file_id() {
    // Arrange
    let parts = vec![file_part(json!({"file_id": "file-abc123"}))];
    let req = req_with(vec![
        user_text("read it"),
        tool_message_parts("call_f2", parts),
    ]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert: only the carrier that was set reaches the wire.
    assert_eq!(
        v["input"][1]["output"],
        json!([{"type": "input_file", "file_id": "file-abc123"}])
    );
}

#[test]
fn tool_result_file_with_no_carrier_forwards_as_a_bare_input_file() {
    // Arrange: a filename alone, beside text so the typed body is built.
    let parts = vec![
        text_part("see file"),
        file_part(json!({"filename": "empty.pdf"})),
    ];
    let req = req_with(vec![
        user_text("read it"),
        tool_message_parts("call_f3", parts),
    ]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(
        v["input"][1]["output"],
        json!([
            {"type": "input_text", "text": "see file"},
            {"type": "input_file", "filename": "empty.pdf"}
        ])
    );
}

#[test]
fn tool_result_file_whose_only_carrier_is_not_a_string_is_refused_as_unrepresentable() {
    // Arrange
    let parts = vec![native_input_file_part(json!({"file_url": ["a", "b"]}))];
    let req = req_with(vec![
        user_text("read it"),
        tool_message_parts("call_f5", parts),
    ]);

    // Act
    let err = translate(&cfg(), &req).expect_err("a non-string carrier has no slot");

    // Assert
    assert_eq!(err.refusal_reason(), Some(RefusalReason::Unrepresentable));
    assert!(
        err.to_string().contains("no string carrier"),
        "error was: {err}"
    );
}

#[test]
fn tool_result_native_input_file_is_forwarded_from_its_extras() {
    // Arrange: the same-dialect block, carried by url.
    let parts = vec![native_input_file_part(json!({
        "file_url": "https://example.com/report.pdf",
        "filename": "report.pdf"
    }))];
    let req = req_with(vec![
        user_text("read it"),
        tool_message_parts("call_f4", parts),
    ]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(
        v["input"][1]["output"],
        json!([{
            "type": "input_file",
            "file_url": "https://example.com/report.pdf",
            "filename": "report.pdf"
        }])
    );
}

#[test]
fn tool_result_unsupported_parts_drop_from_the_wire_and_count_once() {
    // Arrange: one text part beside three parts with no tool-output slot.
    let parts = vec![
        text_part("kept"),
        ContentPart::Known(KnownContentPart::Thinking {
            thinking: "marker-dropped-thinking".into(),
            signature: None,
        }),
        ContentPart::Known(KnownContentPart::Document {
            source: json!({"type": "base64", "media_type": "application/pdf", "data": "marker-dropped-doc"}),
            title: None,
            citations: None,
            cache_control: None,
        }),
        ContentPart::Known(KnownContentPart::ToolUse {
            id: "toolu_nested".into(),
            name: "marker_dropped_tool".into(),
            input: json!({}),
            cache_control: None,
        }),
    ];
    let req = req_with(vec![user_text("run"), tool_message_parts("call_d1", parts)]);

    // Act
    let before = responses_drop_count("tool_result_part_unsupported");
    let (wire, events) = translate_capturing(&cfg(), &req);
    let after = responses_drop_count("tool_result_part_unsupported");

    // Assert 1: each drop is logged at WARN on the tool role.
    let drops: Vec<_> = events
        .iter()
        .filter(|e| e.message.contains("dropping unsupported tool result part"))
        .collect();
    assert_eq!(drops.len(), 3, "one WARN per dropped part, got: {events:?}");
    assert!(drops.iter().all(|e| e.level == tracing::Level::WARN));
    assert!(drops.iter().all(|e| e.field("role") == Some("tool")));

    // Assert 2 + 3: only the text survives, and nothing dropped reaches the wire.
    assert_eq!(
        wire["input"][1]["output"],
        json!([{"type": "input_text", "text": "kept"}])
    );
    assert!(
        !wire.to_string().contains("marker-dropped")
            && !wire.to_string().contains("marker_dropped"),
        "dropped content must not reach the wire: {wire}"
    );

    // Assert 4: three dropped parts are one drop event for the request.
    assert_eq!(after - before, 1);
}

#[test]
fn tool_result_file_on_the_mantle_lane_drops_and_counts() {
    // Arrange: a file beside text, bound for the mantle lane.
    let parts = vec![
        text_part("kept"),
        file_part(json!({"file_data": "data:text/plain;base64,bWFya2Vy"})),
    ];
    let req = req_with(vec![
        user_text("read it"),
        tool_message_parts("call_m1", parts),
    ]);

    // Act
    let before = responses_drop_count("tool_result_part_unsupported");
    let v = translate_to_json(&cfg_bedrock_mantle(), &req);
    let after = responses_drop_count("tool_result_part_unsupported");

    // Assert: the text ships, the file does not, and the drop is counted.
    assert_eq!(
        v["input"][1]["output"],
        json!([{"type": "input_text", "text": "kept"}])
    );
    assert_eq!(after - before, 1);
}

#[test]
fn tool_result_with_only_text_parts_still_collapses_to_a_string() {
    // Arrange
    let parts = vec![text_part("one"), text_part("two")];
    let req = req_with(vec![user_text("run"), tool_message_parts("call_t1", parts)]);

    // Act
    let v = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(v["input"][1]["output"], json!("one\ntwo"));
}

#[test]
fn an_absent_or_empty_tool_output_id_reaches_the_body_as_an_empty_call_id() {
    let tool_result_part = ContentPart::Known(KnownContentPart::ToolResult {
        tool_use_id: String::new(),
        content: json!("sunny"),
        is_error: None,
        cache_control: None,
    });
    let mut absent_id = tool_message("unused", "sunny");
    absent_id.tool_call_id = None;
    let rows = [
        ("tool message, absent id", absent_id),
        ("tool message, empty id", tool_message("", "sunny")),
        (
            "tool_result part, empty id",
            user_parts(vec![tool_result_part]),
        ),
    ];
    for (name, message) in rows {
        // Arrange
        let req = req_with(vec![user_text("weather?"), message]);

        // Act
        let v = translate_to_json(&cfg(), &req);

        // Assert
        assert_eq!(
            v["input"][1],
            json!({"type": "function_call_output", "call_id": "", "output": "sunny"}),
            "{name}"
        );
    }
}
