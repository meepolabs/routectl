// Fragment included from the `tests` module hosted by `eventstream.rs`;
// imports live in the host module. Covers the retirable diagnostic that
// fires when a ConverseStream names the reserved history-compat dummy
// tool in a `contentBlockStart`.

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

fn dummy_block_start_payload(index: u32, tool_use_id: &str, name: &str) -> String {
    format!(
        r#"{{"contentBlockIndex":{index},"start":{{"toolUse":{{"toolUseId":"{tool_use_id}","name":"{name}"}}}}}}"#
    )
}

#[test]
fn reserved_dummy_tool_use_emits_one_warn_on_streaming_path() {
    // Arrange
    let mut state = ConverseStreamState::default();
    let payload = dummy_block_start_payload(0, "tu_secret_id", HISTORY_COMPAT_TOOL_NAME);

    // Act
    let captured = routectl_testkit::capture_events(|| {
        let _ = run("contentBlockStart", &payload, &mut state);
    });

    // Assert
    let warns = history_compat_warns(&captured);
    assert_eq!(
        warns.len(),
        1,
        "expected exactly one reserved-dummy WARN; got events: {captured:?}"
    );
    assert_eq!(warns[0].level, tracing::Level::WARN);
    assert_eq!(warns[0].field("provider"), Some("test"));
    assert_eq!(
        warns[0].field("reserved_tool_name"),
        Some(HISTORY_COMPAT_TOOL_NAME)
    );
}

#[test]
fn reserved_dummy_warn_carries_no_tool_id_arguments_or_content_on_streaming_path() {
    // Arrange: the stream carries a tool id, argument deltas, and text --
    // none of it may reach the log line.
    let mut state = ConverseStreamState::default();
    let start = dummy_block_start_payload(0, "tu_secret_id", HISTORY_COMPAT_TOOL_NAME);

    // Act
    let captured = routectl_testkit::capture_events(|| {
        let _ = run("contentBlockStart", &start, &mut state);
        let _ = run(
            "contentBlockDelta",
            r#"{"contentBlockIndex":0,"delta":{"toolUse":{"input":"{\"confidential\":\"argument-payload\"}"}}}"#,
            &mut state,
        );
        let _ = run(
            "contentBlockDelta",
            r#"{"contentBlockIndex":1,"delta":{"text":"assistant-visible-prose"}}"#,
            &mut state,
        );
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
fn two_reserved_dummy_blocks_in_one_stream_emit_one_warn() {
    // Arrange
    let mut state = ConverseStreamState::default();
    let first = dummy_block_start_payload(0, "tu_1", HISTORY_COMPAT_TOOL_NAME);
    let second = dummy_block_start_payload(1, "tu_2", HISTORY_COMPAT_TOOL_NAME);

    // Act
    let captured = routectl_testkit::capture_events(|| {
        let _ = run("contentBlockStart", &first, &mut state);
        let _ = run("contentBlockStart", &second, &mut state);
    });

    // Assert: once per upstream response, not once per matching block.
    assert_eq!(history_compat_warns(&captured).len(), 1);
}

#[test]
fn normal_tool_use_emits_no_reserved_dummy_warn_on_streaming_path() {
    // Arrange
    let mut state = ConverseStreamState::default();
    let payload = dummy_block_start_payload(0, "tu_42", "get_weather");

    // Act
    let captured = routectl_testkit::capture_events(|| {
        let _ = run("contentBlockStart", &payload, &mut state);
    });

    // Assert
    assert!(history_compat_warns(&captured).is_empty());
}
