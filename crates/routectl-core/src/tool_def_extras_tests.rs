use serde_json::{Map, Value, json};

use super::*;

fn custom(input: Value) -> CustomTool {
    match serde_json::from_value::<ToolDef>(input).unwrap() {
        ToolDef::Custom(tool) => tool,
        ToolDef::Other(other) => panic!("expected Custom, got {other}"),
    }
}

#[test]
fn custom_tool_typed_members_equal_its_serialized_keys_plus_wire_spellings() {
    // Arrange
    let tool = CustomTool {
        name: "n".into(),
        description: Some("d".into()),
        input_schema: json!({}),
        cache_control: Some(CacheControl::ephemeral_5m()),
        defer_loading: Some(true),
        strict: Some(true),
        type_tag: Some("custom".into()),
        extras: Map::new(),
    };

    // Act
    let wire = serde_json::to_value(&tool).unwrap();

    // Assert: the serialized keys plus the accepted `inputSchema` alias.
    let mut serialized: Vec<&str> = wire
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    serialized.push("inputSchema");
    let mut typed = CUSTOM_TOOL_MEMBERS.to_vec();
    serialized.sort_unstable();
    typed.sort_unstable();
    assert_eq!(typed, serialized);
}

#[test]
fn custom_tool_leftover_members_land_in_extras() {
    // (row, input, expected extras)
    let rows = [
        (
            "caller",
            json!({"name": "t", "caller": {"type": "direct"}}),
            json!({"caller": {"type": "direct"}}),
        ),
        (
            "non_ascii_value",
            json!({"type": "custom", "name": "t", "note": "caf\u{e9}"}),
            json!({"note": "caf\u{e9}"}),
        ),
        ("empty_map", json!({"name": "t", "strict": true}), json!({})),
        (
            "reserved_name_collision",
            json!({"name": "typed", "extras": {"name": "nested"}}),
            json!({"extras": {"name": "nested"}}),
        ),
        (
            "camel_case_alias_is_typed",
            json!({"name": "t", "inputSchema": {"type": "object"}}),
            json!({}),
        ),
        (
            "lifted_bare_function_is_consumed",
            json!({"function": {"name": "t", "parameters": {}}, "eager_input_streaming": true}),
            json!({"eager_input_streaming": true}),
        ),
        (
            "unlifted_function_object_is_extra",
            json!({"name": "outer", "function": {"name": "inner"}}),
            json!({"function": {"name": "inner"}}),
        ),
    ];
    for (row, input, expected) in rows {
        // Act
        let tool = custom(input);

        // Assert
        assert_eq!(Value::Object(tool.extras.clone()), expected, "row `{row}`");
        let wire = serde_json::to_value(&tool).unwrap();
        for key in expected.as_object().unwrap().keys() {
            assert!(wire.get(key).is_none(), "row `{row}` emitted `{key}`");
        }
    }
}

#[test]
fn custom_tool_typed_name_stays_typed_beside_a_colliding_extra() {
    // Arrange
    let input = json!({"name": "typed", "extras": {"name": "nested"}, "x": 1});

    // Act
    let tool = custom(input);

    // Assert
    assert_eq!(tool.name, "typed");
    assert!(!tool.extras.contains_key("name"));
}

#[test]
fn custom_tool_parse_errors_are_unchanged_by_capture() {
    for (input, needle) in [
        (
            json!({"description": "no name", "caller": 1}),
            "missing field `name`",
        ),
        (json!({"name": 7, "caller": 1}), "invalid type"),
        (
            json!({"name": "t", "input_schema": {}, "inputSchema": {}, "caller": 1}),
            "duplicate field",
        ),
    ] {
        // Act
        let err = serde_json::from_value::<ToolDef>(input.clone()).unwrap_err();

        // Assert
        assert!(err.to_string().contains(needle), "{input}: {err}");
    }
}

#[test]
fn other_tools_keep_every_member_verbatim() {
    // Arrange
    let input = json!({"type": "bash_20250124", "name": "bash", "caller": 1});

    // Act
    let tool: ToolDef = serde_json::from_value(input.clone()).unwrap();

    // Assert
    assert!(matches!(tool, ToolDef::Other(ref v) if *v == input));
}
