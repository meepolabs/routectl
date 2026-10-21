use super::*;
use routectl_core::{ChatRequest, Message, MessageContent, Role};
use serde_json::json;

fn empty_req() -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        ..Default::default()
    }
}

fn run(messages: Value) -> Map<String, Value> {
    let req = empty_req();
    let mut obj = Map::new();
    obj.insert("messages".into(), messages);
    lift("test", &mut obj, &req, false).unwrap();
    obj
}

/// Variant of `run` that surfaces the `Result` so tests can assert
/// on the error path without a panic.
fn run_result(messages: Value) -> routectl_core::Result<Map<String, Value>> {
    let req = empty_req();
    let mut obj = Map::new();
    obj.insert("messages".into(), messages);
    lift("test", &mut obj, &req, false)?;
    Ok(obj)
}

#[test]
fn single_tool_result_becomes_role_tool_message() {
    // Arrange
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_01ABC", "content": "4"}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    let msgs = obj["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1, "single tool_result -> single tool message");
    assert_eq!(msgs[0]["role"], "tool");
    assert_eq!(msgs[0]["tool_call_id"], "toolu_01ABC");
    assert_eq!(msgs[0]["content"], "4");
}

#[test]
fn multiple_tool_results_split_into_multiple_messages() {
    // Arrange
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "first"},
            {"type": "tool_result", "tool_use_id": "t2", "content": "second"}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    let msgs = obj["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["role"], "tool");
    assert_eq!(msgs[0]["tool_call_id"], "t1");
    assert_eq!(msgs[1]["role"], "tool");
    assert_eq!(msgs[1]["tool_call_id"], "t2");
}

#[test]
fn mixed_text_and_tool_result_splits_into_user_then_tool() {
    // Arrange
    let messages = json!([
        {"role": "user", "content": [
            {"type": "text", "text": "see result:"},
            {"type": "tool_result", "tool_use_id": "t1", "content": "the answer"}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    let msgs = obj["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["content"], "see result:");
    assert_eq!(msgs[1]["role"], "tool");
    assert_eq!(msgs[1]["tool_call_id"], "t1");
    assert_eq!(msgs[1]["content"], "the answer");
}

#[test]
fn tool_result_image_hoists_into_following_user_message() {
    // Arrange -- tool_result containing text and an Anthropic image block.
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "text", "text": "here is the rendering"},
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "ZZ=="
                }}
            ]}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert -- the text stays on the tool message, the lifted image
    // moves to one user message right after it.
    let msgs = obj["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "got: {msgs:?}");
    assert_eq!(msgs[0]["role"], "tool");
    assert_eq!(msgs[0]["tool_call_id"], "t1");
    assert_eq!(msgs[0]["content"], json!("here is the rendering"));
    assert_eq!(msgs[1]["role"], "user");
    assert_eq!(
        msgs[1]["content"],
        json!([{"type": "image_url", "image_url": {"url": "data:image/png;base64,ZZ=="}}])
    );
}

#[test]
fn parallel_tool_result_images_hoist_into_one_user_message_after_all_tools() {
    // Arrange -- two results of one parallel call turn, each with an image.
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "text", "text": "one"},
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "SU1HMQ=="
                }}
            ]},
            {"type": "tool_result", "tool_use_id": "t2", "content": [
                {"type": "text", "text": "two"},
                {"type": "image", "source": {"type": "url", "url": "https://example.com/2.png"}}
            ]}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert -- [tool, tool, user(img1, img2)]: the tool messages stay
    // contiguous and the images keep source order.
    let msgs = obj["messages"].as_array().unwrap();
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["tool", "tool", "user"], "got: {msgs:?}");
    assert_eq!(msgs[0]["tool_call_id"], "t1");
    assert_eq!(msgs[0]["content"], json!("one"));
    assert_eq!(msgs[1]["tool_call_id"], "t2");
    assert_eq!(msgs[1]["content"], json!("two"));
    assert_eq!(
        msgs[2]["content"],
        json!([
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,SU1HMQ=="}},
            {"type": "image_url", "image_url": {"url": "https://example.com/2.png"}}
        ])
    );
}

#[test]
fn image_only_tool_result_keeps_empty_string_tool_message() {
    // Arrange
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "ZZ=="
                }}
            ]}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    let msgs = obj["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "got: {msgs:?}");
    assert_eq!(msgs[0]["role"], "tool");
    assert_eq!(msgs[0]["tool_call_id"], "t1");
    assert_eq!(msgs[0]["content"], json!(""));
    assert_eq!(msgs[1]["role"], "user");
    assert_eq!(msgs[1]["content"][0]["type"], "image_url");
}

#[test]
fn user_text_after_tool_results_follows_hoisted_images() {
    // Arrange -- user text trailing the results in the same message.
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "ZZ=="
                }}
            ]},
            {"type": "text", "text": "what do you see?"}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert -- one user message: images first, then the text.
    let msgs = obj["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "got: {msgs:?}");
    assert_eq!(msgs[0]["role"], "tool");
    assert_eq!(msgs[1]["role"], "user");
    let content = msgs[1]["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    assert_eq!(content[0]["type"], "image_url");
    assert_eq!(
        content[1],
        json!({"type": "text", "text": "what do you see?"})
    );
}

#[test]
fn leading_user_text_stays_ahead_of_tool_message_with_image() {
    // Arrange
    let messages = json!([
        {"role": "user", "content": [
            {"type": "text", "text": "see result:"},
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "ZZ=="
                }}
            ]}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    let msgs = obj["messages"].as_array().unwrap();
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().unwrap()).collect();
    assert_eq!(roles, ["user", "tool", "user"], "got: {msgs:?}");
    assert_eq!(msgs[0]["content"], json!("see result:"));
    assert_eq!(msgs[2]["content"][0]["type"], "image_url");
}

#[test]
fn text_only_tool_results_emit_no_user_message() {
    // Arrange -- positive control: no image, nothing hoisted.
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "text", "text": "a"}
            ]},
            {"type": "tool_result", "tool_use_id": "t2", "content": "b"}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    assert_eq!(
        obj["messages"],
        json!([
            {"role": "tool", "tool_call_id": "t1", "content": "a"},
            {"role": "tool", "tool_call_id": "t2", "content": "b"}
        ])
    );
}

fn image_block(data: &str) -> Value {
    json!({"type": "image", "source": {
        "type": "base64", "media_type": "image/png", "data": data
    }})
}

fn image_url_part(data: &str) -> Value {
    json!({"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{data}")}})
}

fn roles(obj: &Map<String, Value>) -> Vec<&str> {
    obj["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect()
}

#[test]
fn run_split_across_user_messages_hoists_images_after_the_whole_run() {
    // Arrange -- one parallel call turn whose results arrive as two
    // consecutive canonical user messages, each result with an image.
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1",
             "content": [{"type": "text", "text": "one"}, image_block("QQ==")]}
        ]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t2",
             "content": [{"type": "text", "text": "two"}, image_block("Qg==")]}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    assert_eq!(
        obj["messages"],
        json!([
            {"role": "tool", "tool_call_id": "t1", "content": "one"},
            {"role": "tool", "tool_call_id": "t2", "content": "two"},
            {"role": "user", "content": [image_url_part("QQ=="), image_url_part("Qg==")]}
        ])
    );
}

#[test]
fn run_split_across_user_messages_with_image_only_in_first_result() {
    // Arrange
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [image_block("QQ==")]}
        ]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t2", "content": "two"}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    assert_eq!(
        obj["messages"],
        json!([
            {"role": "tool", "tool_call_id": "t1", "content": ""},
            {"role": "tool", "tool_call_id": "t2", "content": "two"},
            {"role": "user", "content": [image_url_part("QQ==")]}
        ])
    );
}

#[test]
fn split_run_hoisted_images_precede_trailing_user_text_of_last_message() {
    // Arrange
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [image_block("QQ==")]}
        ]},
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t2", "content": [image_block("Qg==")]},
            {"type": "text", "text": "compare them"}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    assert_eq!(roles(&obj), ["tool", "tool", "user"]);
    assert_eq!(
        obj["messages"][2]["content"],
        json!([
            image_url_part("QQ=="),
            image_url_part("Qg=="),
            {"type": "text", "text": "compare them"}
        ])
    );
}

#[test]
fn hoisted_images_flush_before_a_message_that_does_not_continue_the_run() {
    let cases = [
        (
            "assistant turn",
            json!({"role": "assistant", "content": "next"}),
            vec!["tool", "user", "assistant"],
        ),
        (
            "plain user string",
            json!({"role": "user", "content": "plain"}),
            vec!["tool", "user", "user"],
        ),
        (
            "user message opening with text",
            json!({"role": "user", "content": [
                {"type": "text", "text": "first"},
                {"type": "tool_result", "tool_use_id": "t2", "content": "two"}
            ]}),
            vec!["tool", "user", "user", "tool"],
        ),
    ];
    for (name, next, expected_roles) in cases {
        // Arrange
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": [image_block("QQ==")]}
            ]},
            next
        ]);

        // Act
        let obj = run(messages);

        // Assert
        assert_eq!(roles(&obj), expected_roles, "{name}");
        assert_eq!(
            obj["messages"][1]["content"],
            json!([image_url_part("QQ==")]),
            "{name}: the image must flush right after its run"
        );
    }
}

#[test]
fn image_only_results_in_a_parallel_run_hoist_together() {
    // Arrange
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [image_block("QQ==")]},
            {"type": "tool_result", "tool_use_id": "t2", "content": [image_block("Qg==")]}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    assert_eq!(
        obj["messages"],
        json!([
            {"role": "tool", "tool_call_id": "t1", "content": ""},
            {"role": "tool", "tool_call_id": "t2", "content": ""},
            {"role": "user", "content": [image_url_part("QQ=="), image_url_part("Qg==")]}
        ])
    );
}

#[test]
fn inner_block_already_in_image_url_shape_is_hoisted() {
    // Arrange
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1",
             "content": [{"type": "text", "text": "done"}, image_url_part("QQ==")]}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    assert_eq!(
        obj["messages"],
        json!([
            {"role": "tool", "tool_call_id": "t1", "content": "done"},
            {"role": "user", "content": [image_url_part("QQ==")]}
        ])
    );
}

#[test]
fn tool_message_content_is_a_string_whenever_only_text_survives() {
    let document = json!({"type": "document", "source": {
        "type": "base64", "media_type": "application/pdf", "data": "AA=="
    }});
    let text = json!({"type": "text", "text": "x"});
    let cases = [
        (
            "text and dropped document",
            json!([text.clone(), document.clone()]),
            "x",
            1,
        ),
        (
            "text, image and dropped document",
            json!([text, image_block("QQ=="), document.clone()]),
            "x",
            2,
        ),
        ("empty array", json!([]), "", 1),
        ("every block dropped", json!([document]), "", 1),
    ];
    for (name, content, expected, expected_len) in cases {
        // Arrange
        let messages = json!([
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": content}
            ]}
        ]);

        // Act
        let obj = run(messages);

        // Assert
        assert_eq!(obj["messages"][0]["content"], json!(expected), "{name}");
        assert_eq!(
            obj["messages"].as_array().unwrap().len(),
            expected_len,
            "{name}"
        );
    }
}

#[test]
fn tool_result_text_only_array_collapses_to_string() {
    // Arrange -- a tool_result whose content is an array with a
    // single text block. Strict backends 400 on the array; we must
    // collapse it to a plain string.
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "text", "text": "42"}
            ]}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    let msgs = obj["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["role"], "tool");
    assert_eq!(
        msgs[0]["content"],
        json!("42"),
        "text-only array must collapse to a string"
    );
}

#[test]
fn tool_result_multi_text_array_joins_with_double_newline() {
    // Arrange -- two text blocks join into one string.
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "text", "text": "first"},
                {"type": "text", "text": "second"}
            ]}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    let msgs = obj["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["content"], json!("first\n\nsecond"));
}

#[test]
fn tool_result_text_plus_image_joins_text_on_tool_message() {
    // Arrange -- once the image is hoisted, the remaining text-only
    // content collapses to a string like any text-only result.
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "text", "text": "see image"},
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "ZZ=="
                }}
            ]}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    let msgs = obj["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["content"], json!("see image"));
    assert_eq!(msgs[1]["content"][0]["type"], "image_url");
}

#[test]
fn tool_result_string_content_stays_string() {
    // Arrange -- a plain string content is unchanged.
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "already a string"}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    let msgs = obj["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["content"], json!("already a string"));
}

#[test]
fn no_tool_result_user_passes_through() {
    // Arrange
    let messages = json!([
        {"role": "user", "content": "hello"},
        {"role": "assistant", "content": "hi"}
    ]);

    // Act
    let obj = run(messages);

    // Assert -- structure preserved
    let msgs = obj["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["content"], "hello");
    assert_eq!(msgs[1]["role"], "assistant");
}

#[test]
fn tool_result_with_absent_or_non_string_tool_use_id_forwards_it_verbatim() {
    let cases = [
        ("absent", None, None),
        ("empty", Some(json!("")), Some(json!(""))),
        ("non-string", Some(json!(42)), Some(json!(42))),
    ];
    for (name, sent, expected) in cases {
        // Arrange
        let mut block = json!({"type": "tool_result", "content": "orphan"});
        if let Some(sent) = sent {
            block["tool_use_id"] = sent;
        }
        let messages = json!([{"role": "user", "content": [block]}]);

        // Act
        let obj = run_result(messages)
            .unwrap_or_else(|e| panic!("{name}: the tool_result must forward, got {e}"));

        // Assert
        let msgs = obj["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1, "{name}");
        assert_eq!(msgs[0]["role"], "tool", "{name}");
        assert_eq!(msgs[0]["content"], "orphan", "{name}");
        assert_eq!(msgs[0].get("tool_call_id").cloned(), expected, "{name}");
    }
}

#[test]
fn no_messages_is_no_op() {
    // Arrange
    let req = empty_req();
    let mut obj = Map::new();

    // Act
    lift("test", &mut obj, &req, false).unwrap();

    // Assert
    assert!(obj.get("messages").is_none());
}

/// Strict variant of `run_result` for the strict-mode error paths.
fn run_result_strict(messages: Value) -> routectl_core::Result<Map<String, Value>> {
    let req = empty_req();
    let mut obj = Map::new();
    obj.insert("messages".into(), messages);
    lift("test", &mut obj, &req, true)?;
    Ok(obj)
}

/// cache_control on an inner tool_result block (kept in array
/// form by a non-text sibling) must be stripped from the wire,
/// mirroring the top-level content strip.
#[test]
fn inner_block_cache_control_is_stripped() {
    // Arrange -- a text block carrying cache_control plus an unknown
    // block so the array form survives (text-only collapses to a string).
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}},
                {"type": "future_block", "payload": "kept"}
            ]}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    let content = obj["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "text");
    assert!(
        content[0].get("cache_control").is_none(),
        "cache_control must be stripped from the inner block"
    );
}

/// An inner image block with an unrecognized source shape is
/// dropped in lenient mode -- the raw Anthropic image must not reach
/// the wire.
#[test]
fn inner_image_unknown_source_dropped_lenient() {
    // Arrange -- image with an unsupported source type, plus a real
    // image that is hoisted.
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "image", "source": {"type": "weird_unknown_source"}},
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "ZZ=="
                }}
            ]}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert -- only the recognized image survives, lifted to image_url,
    // and nothing of the dropped one stays on the tool message.
    assert_eq!(obj["messages"][0]["content"], json!(""));
    let content = obj["messages"][1]["content"].as_array().unwrap();
    assert_eq!(content.len(), 1, "unknown-source image must be dropped");
    assert_eq!(content[0]["type"], "image_url");
}

/// The url-source-missing-url image is dropped in lenient mode.
#[test]
fn inner_image_url_missing_url_dropped_lenient() {
    // Arrange
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "image", "source": {"type": "url"}},
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "ZZ=="
                }}
            ]}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    assert_eq!(obj["messages"][0]["content"], json!(""));
    let content = obj["messages"][1]["content"].as_array().unwrap();
    assert_eq!(content.len(), 1, "url-missing-url image must be dropped");
    assert_eq!(content[0]["type"], "image_url");
}

/// Strict mode rejects the unrepresentable inner image.
#[test]
fn inner_image_unknown_source_strict_errors() {
    // Arrange
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "image", "source": {"type": "weird_unknown_source"}},
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "ZZ=="
                }}
            ]}
        ]}
    ]);

    // Act
    let res = run_result_strict(messages);

    // Assert
    assert!(
        res.is_err(),
        "strict mode must reject unknown inner image source"
    );
    let msg = format!("{}", res.unwrap_err());
    assert!(msg.contains("strict_translation"));
}

/// A nested document block inside a tool_result is dropped
/// in lenient mode -- it must not reach the wire.
#[test]
fn inner_document_block_dropped_lenient() {
    // Arrange -- document block plus an image that is hoisted.
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "document", "source": {
                    "type": "base64", "media_type": "application/pdf", "data": "AA=="
                }},
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "ZZ=="
                }}
            ]}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert -- document gone from the tool message, image lifted.
    assert_eq!(obj["messages"][0]["content"], json!(""));
    let content = obj["messages"][1]["content"].as_array().unwrap();
    assert_eq!(content.len(), 1, "document block must be dropped");
    assert_eq!(content[0]["type"], "image_url");
}

/// Strict mode rejects a nested document block.
#[test]
fn inner_document_block_strict_errors() {
    // Arrange
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                {"type": "document", "source": {
                    "type": "base64", "media_type": "application/pdf", "data": "AA=="
                }},
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "ZZ=="
                }}
            ]}
        ]}
    ]);

    // Act
    let res = run_result_strict(messages);

    // Assert
    assert!(res.is_err(), "strict mode must reject inner document block");
    let msg = format!("{}", res.unwrap_err());
    assert!(msg.contains("strict_translation"));
    assert!(msg.contains("document"));
}

/// is_error with string content prefixes "Error: ".
#[test]
fn is_error_string_content_prefixed() {
    // Arrange
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "is_error": true,
             "content": "boom"}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    let content = obj["messages"][0]["content"].as_str().unwrap();
    assert!(
        content.starts_with("Error: "),
        "is_error string content must start with 'Error: ', got: {content}"
    );
    assert_eq!(content, "Error: boom");
}

/// is_error with text plus image content keeps the `Error:` marker on
/// the tool message; the image is hoisted unmarked.
#[test]
fn is_error_with_image_marks_tool_message() {
    // Arrange
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "is_error": true,
             "content": [
                {"type": "text", "text": "details"},
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "ZZ=="
                }}
             ]}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    let msgs = obj["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 2, "got: {msgs:?}");
    assert_eq!(msgs[0]["role"], "tool");
    assert_eq!(msgs[0]["content"], json!("Error: details"));
    assert_eq!(msgs[1]["role"], "user");
    assert_eq!(
        msgs[1]["content"],
        json!([{"type": "image_url", "image_url": {"url": "data:image/png;base64,ZZ=="}}])
    );
}

/// An image-only error result keeps a bare `Error: ` tool message.
#[test]
fn is_error_image_only_marks_empty_tool_message() {
    // Arrange
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "is_error": true,
             "content": [
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png", "data": "ZZ=="
                }}
             ]}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    let msgs = obj["messages"].as_array().unwrap();
    assert_eq!(msgs[0]["content"], json!("Error: "));
    assert_eq!(msgs[1]["content"][0]["type"], "image_url");
}

/// is_error with array content that stays an array (a non-text
/// sibling survives) prepends a leading error text block.
#[test]
fn is_error_array_content_prepends_error_block() {
    // Arrange
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "is_error": true,
             "content": [
                {"type": "text", "text": "details"},
                {"type": "future_block", "payload": "kept"}
             ]}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    let content = obj["messages"][0]["content"].as_array().unwrap();
    assert_eq!(content.len(), 3);
    assert_eq!(content[0], json!({"type": "text", "text": "Error: "}));
    assert_eq!(content[1]["text"], "details");
    assert_eq!(content[2]["type"], "future_block");
}

/// Absence of is_error leaves content unchanged.
#[test]
fn no_is_error_leaves_content_unchanged() {
    // Arrange
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
        ]}
    ]);

    // Act
    let obj = run(messages);

    // Assert
    assert_eq!(obj["messages"][0]["content"], json!("ok"));
}

/// The `(openai-compat, class)` counter's current value, read back
/// through the public snapshot.
fn drop_count(class: &str) -> u64 {
    crate::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == "openai-compat" && e.drop_class == class)
        .map_or(0, |e| e.drop_count)
}

/// Run the lenient lift and return the EMITTED WIRE BODY as the string
/// an upstream would receive, plus every captured event. Asserting
/// against the serialized body -- not a typed view of the content array
/// -- is what catches a dropped block that actually rode upstream
/// nested inside some other element's payload.
fn emitted_wire(messages: Value) -> (String, Vec<routectl_testkit::CapturedEvent>) {
    let req = empty_req();
    let mut obj = Map::new();
    obj.insert("messages".into(), messages);
    let events = routectl_testkit::capture_events(|| {
        lift("test", &mut obj, &req, false).expect("lenient lift must succeed");
    });
    let wire = serde_json::to_string(&Value::Object(obj)).expect("wire body serializes");
    (wire, events)
}

/// A tool result carrying `block` alongside a representable image
/// sibling, which is hoisted into the user message after the tool
/// message.
fn tool_result_with(block: Value) -> Value {
    json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [
                block,
                {"type": "image", "source": {
                    "type": "base64", "media_type": "image/png",
                    "data": "TUFSS0VSLUdPT0Qt"
                }}
            ]}
        ]}
    ])
}

/// Assert the three bars for one dropped inner block: the WARN fired
/// with the expected structured `what`, the block's own payload is gone
/// from the EMITTED WIRE BODY, and the representable image sibling
/// survives in that same body. `marker` is a token unique to the
/// fixture so the absence assertion cannot pass by accident.
fn assert_inner_drop(block: Value, marker: &str, expected_what: &str, class: &str) {
    // Act
    let before = drop_count(class);
    let (wire, events) = emitted_wire(tool_result_with(block));
    let after = drop_count(class);

    // Assert 1 -- the WARN fired, naming the tool_result context.
    let warn = events
        .iter()
        .find(|e| e.level == tracing::Level::WARN)
        .unwrap_or_else(|| panic!("the drop must warn, got: {events:?}"));
    assert_eq!(warn.field("provider"), Some("test"));
    assert_eq!(warn.field("context"), Some("tool_result block"));
    assert_eq!(warn.field("what"), Some(expected_what));

    // Assert 2 -- the dropped block's payload is off the emitted wire.
    assert!(
        !wire.contains(marker),
        "the dropped block's payload must not reach the wire, got: {wire}"
    );

    // Assert 3 -- the representable sibling survived, lifted, in that
    // same body.
    assert!(
        wire.contains("data:image/png;base64,TUFSS0VSLUdPT0Qt"),
        "the representable image sibling must survive, got: {wire}"
    );

    assert_eq!(
        after - before,
        1,
        "the drop must be counted exactly once for the request"
    );
}

/// NEGATIVE CONTROL: a nested document block.
#[test]
fn inner_document_block_drops_and_warns() {
    assert_inner_drop(
        json!({"type": "document", "source": {
            "type": "base64", "media_type": "application/pdf",
            "data": "TUFSS0VSLURPQ1VNRU5U"
        }}),
        "TUFSS0VSLURPQ1VNRU5U",
        "document content block (no OpenAI equivalent)",
        "tool_result_document_unrepresentable",
    );
}

/// NEGATIVE CONTROL: a nested image block with an unrecognized source.
#[test]
fn inner_image_unknown_source_drops_and_warns() {
    assert_inner_drop(
        json!({"type": "image", "source": {
            "type": "marker_future_source_kind", "blob": "unused"
        }}),
        "marker_future_source_kind",
        "image block with unsupported source shape (expected base64 or url)",
        "tool_result_image_source_unrepresentable",
    );
}

/// NEGATIVE CONTROL: a nested image block claiming the url source form
/// but carrying no `url`.
#[test]
fn inner_image_url_missing_url_drops_and_warns() {
    assert_inner_drop(
        json!({"type": "image", "source": {
            "type": "url", "href": "marker_wrong_url_key"
        }}),
        "marker_wrong_url_key",
        "image block with url source missing `url`",
        "tool_result_image_source_unrepresentable",
    );
}

/// POSITIVE CONTROL for all three fixtures above: a nested image block
/// with a REPRESENTABLE url source takes the same arm family, must
/// survive on the emitted body, and must warn not at all. Without this,
/// every absence assertion above would pass on a lift that dropped
/// every nested block.
#[test]
fn representable_inner_url_image_survives_without_warning() {
    // Act
    let (wire, events) = emitted_wire(tool_result_with(json!({
        "type": "image",
        "source": {"type": "url", "url": "https://example.com/marker_kept.png"}
    })));

    // Assert
    assert!(
        !events.iter().any(|e| e.level == tracing::Level::WARN),
        "a representable nested image must not warn at all, got: {events:?}"
    );
    assert!(
        wire.contains("https://example.com/marker_kept.png"),
        "the representable nested image must reach the wire, got: {wire}"
    );
}

/// Two dropped nested documents in ONE request are ONE counted drop
/// event, even when they sit in different tool results.
#[test]
fn two_dropped_inner_documents_count_as_one_request_drop() {
    // Arrange
    let doc = json!({"type": "document", "source": {
        "type": "base64", "media_type": "application/pdf", "data": "QQ=="
    }});
    let keeper = json!({"type": "image", "source": {
        "type": "base64", "media_type": "image/png", "data": "Wlo="
    }});
    let messages = json!([
        {"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "t1",
             "content": [doc.clone(), keeper.clone()]},
            {"type": "tool_result", "tool_use_id": "t2",
             "content": [doc, keeper]}
        ]}
    ]);

    // Act
    let before = drop_count("tool_result_document_unrepresentable");
    run(messages);
    let after = drop_count("tool_result_document_unrepresentable");

    // Assert
    assert_eq!(
        after - before,
        1,
        "two dropped blocks in one request must count once"
    );
}
