// Fragment included from the `tests` module in `response.rs`; imports
// live in the host module. Covers the retirable diagnostic that fires
// when a Converse response names the reserved history-compat dummy tool.

const HISTORY_COMPAT_WARN_MESSAGE: &str =
    "converse: model selected the reserved history-compat dummy tool";

fn history_compat_warns(
    captured: &[routectl_testkit::CapturedEvent],
) -> Vec<&routectl_testkit::CapturedEvent> {
    captured
        .iter()
        .filter(|e| e.message == HISTORY_COMPAT_WARN_MESSAGE)
        .collect()
}

fn dummy_tool_use_response(blocks: Value) -> Value {
    json!({
        "output": {"message": {"role": "assistant", "content": blocks}},
        "stopReason": "tool_use"
    })
}

#[test]
fn reserved_dummy_tool_use_emits_one_warn_on_non_streaming_path() {
    // Arrange
    let raw = dummy_tool_use_response(json!([
        {"toolUse": {
            "toolUseId": "tu_secret_id",
            "name": HISTORY_COMPAT_TOOL_NAME,
            "input": {"confidential": "argument-payload"}
        }}
    ]));

    // Act
    let captured = routectl_testkit::capture_events(|| {
        translate("prov-test", &raw).expect("translate must succeed");
    });

    // Assert
    let warns = history_compat_warns(&captured);
    assert_eq!(
        warns.len(),
        1,
        "expected exactly one reserved-dummy WARN; got events: {captured:?}"
    );
    assert_eq!(warns[0].level, tracing::Level::WARN);
    assert_eq!(warns[0].field("provider"), Some("prov-test"));
    assert_eq!(
        warns[0].field("reserved_tool_name"),
        Some(HISTORY_COMPAT_TOOL_NAME)
    );
}

#[test]
fn reserved_dummy_warn_carries_no_tool_id_arguments_or_content_on_non_streaming_path() {
    // Arrange: the upstream block carries an id, arguments, and adjacent
    // assistant text -- none of it may reach the log line.
    let raw = dummy_tool_use_response(json!([
        {"text": "assistant-visible-prose"},
        {"toolUse": {
            "toolUseId": "tu_secret_id",
            "name": HISTORY_COMPAT_TOOL_NAME,
            "input": {"confidential": "argument-payload"}
        }}
    ]));

    // Act
    let captured = routectl_testkit::capture_events(|| {
        translate("prov-test", &raw).expect("translate must succeed");
    });

    // Assert
    let warns = history_compat_warns(&captured);
    assert_eq!(warns.len(), 1);
    let field_names: Vec<&str> = warns[0].fields.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        field_names,
        vec!["provider", "reserved_tool_name"],
        "the WARN must carry only the provider id and the routectl-authored constant"
    );
    let rendered = format!("{} {:?}", warns[0].message, warns[0].fields);
    for leak in [
        "tu_secret_id",
        "confidential",
        "argument-payload",
        "assistant-visible-prose",
    ] {
        assert!(
            !rendered.contains(leak),
            "WARN leaked {leak:?}; rendered: {rendered:?}"
        );
    }
}

#[test]
fn two_reserved_dummy_blocks_in_one_response_emit_one_warn_on_non_streaming_path() {
    // Arrange
    let raw = dummy_tool_use_response(json!([
        {"toolUse": {"toolUseId": "tu_1", "name": HISTORY_COMPAT_TOOL_NAME, "input": {}}},
        {"toolUse": {"toolUseId": "tu_2", "name": HISTORY_COMPAT_TOOL_NAME, "input": {}}}
    ]));

    // Act
    let captured = routectl_testkit::capture_events(|| {
        translate("prov-test", &raw).expect("translate must succeed");
    });

    // Assert: once per upstream response, not once per matching block.
    assert_eq!(history_compat_warns(&captured).len(), 1);
}

#[test]
fn normal_tool_use_emits_no_reserved_dummy_warn_on_non_streaming_path() {
    // Arrange
    let raw = dummy_tool_use_response(json!([
        {"toolUse": {"toolUseId": "tu_42", "name": "get_weather", "input": {"location": "Tokyo"}}}
    ]));

    // Act
    let captured = routectl_testkit::capture_events(|| {
        translate("prov-test", &raw).expect("translate must succeed");
    });

    // Assert
    assert!(history_compat_warns(&captured).is_empty());
}
