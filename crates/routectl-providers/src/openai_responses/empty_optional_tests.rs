//! Which function call arguments the empty-optional filter removes.

use routectl_core::{CustomTool, ToolDef};
use serde_json::{Value, json};

use super::EmptyOptionalFilter;

fn function_tool(name: &str, schema: Value) -> ToolDef {
    ToolDef::Custom(CustomTool {
        name: name.into(),
        description: None,
        input_schema: schema,
        cache_control: None,
        defer_loading: None,
        strict: None,
        type_tag: None,
    })
}

/// A file reader shaped like a coding agent's: one required path, the rest
/// optional.
fn reader() -> ToolDef {
    function_tool(
        "read_file",
        json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
                "pages": {"type": "string"},
                "offset": {"type": "integer"},
                "limit": {"type": "integer"},
                "allow_large": {"type": "boolean"},
                "options": {"type": "object"}
            },
            "required": ["file_path"]
        }),
    )
}

#[test]
fn removes_only_empty_optional_top_level_arguments() {
    // (row, tool, arguments, expected arguments after the filter)
    let rows: [(&str, ToolDef, &str, Value); 7] = [
        (
            "empty optional dropped, other optionals kept",
            reader(),
            r#"{"allow_large":false,"file_path":"/w/a.txt","limit":2000,"offset":0,"pages":""}"#,
            json!({"allow_large": false, "file_path": "/w/a.txt", "limit": 2000, "offset": 0}),
        ),
        (
            "empty required kept",
            reader(),
            r#"{"file_path":"","pages":"1-2"}"#,
            json!({"file_path": "", "pages": "1-2"}),
        ),
        (
            "non-empty optional kept",
            reader(),
            r#"{"file_path":"/w/a.txt","pages":" "}"#,
            json!({"file_path": "/w/a.txt", "pages": " "}),
        ),
        (
            "empty value of an undeclared key kept",
            reader(),
            r#"{"file_path":"/w/a.txt","note":""}"#,
            json!({"file_path": "/w/a.txt", "note": ""}),
        ),
        (
            "nested empty string kept",
            reader(),
            r#"{"file_path":"/w/a.txt","options":{"pages":""}}"#,
            json!({"file_path": "/w/a.txt", "options": {"pages": ""}}),
        ),
        (
            "tool without properties untouched",
            function_tool("free_form", json!({"type": "object"})),
            r#"{"pages":""}"#,
            json!({"pages": ""}),
        ),
        (
            "tool with no required list drops every empty declared key",
            function_tool(
                "search",
                json!({"type": "object", "properties": {"q": {}, "scope": {}}}),
            ),
            r#"{"q":"","scope":"x"}"#,
            json!({"scope": "x"}),
        ),
    ];
    for (row, tool, arguments, expected) in rows {
        // Arrange
        let tools = [tool];
        let name = match &tools[0] {
            ToolDef::Custom(c) => c.name.clone(),
            ToolDef::Other(_) => unreachable!(),
        };
        let mut filter = EmptyOptionalFilter::from_tools(&tools);

        // Act
        let out = filter.apply(&name, arguments);

        // Assert
        let parsed: Value = serde_json::from_str(&out).expect("arguments stay JSON");
        assert_eq!(parsed, expected, "{row}");
    }
}

#[test]
fn unchanged_arguments_pass_through_byte_for_byte() {
    // (row, tool name, arguments): whitespace and key order a re-serialization
    // would normalize, so only a borrow keeps them.
    let rows = [
        (
            "nothing to remove",
            "read_file",
            "{ \"pages\" : \"3\",\n \"file_path\":\"/w\" }",
        ),
        ("unknown tool", "other_tool", "{ \"pages\" : \"\" }"),
        ("not an object", "read_file", "[\"\"]"),
        ("not JSON", "read_file", "{\"pages\": \""),
    ];
    let tools = [reader()];
    for (row, name, arguments) in rows {
        // Arrange
        let mut filter = EmptyOptionalFilter::from_tools(&tools);

        // Act
        let out = filter.apply(name, arguments);

        // Assert
        assert_eq!(out, arguments, "{row}");
        assert!(filter.removed().is_empty(), "{row}");
    }
}

#[test]
fn a_non_function_tool_is_never_filtered() {
    // Arrange: a hosted tool value whose shape happens to carry properties.
    let tools = [ToolDef::Other(json!({
        "type": "web_search", "name": "lookup",
        "parameters": {"type": "object", "properties": {"q": {}}}
    }))];
    let mut filter = EmptyOptionalFilter::from_tools(&tools);

    // Act
    let out = filter.apply("lookup", r#"{"q":""}"#);

    // Assert
    assert_eq!(out, r#"{"q":""}"#);
}

#[test]
fn removed_names_each_call_and_its_count() {
    // Arrange
    let tools = [reader()];
    let mut filter = EmptyOptionalFilter::from_tools(&tools);

    // Act
    filter.apply("read_file", r#"{"file_path":"/w","pages":""}"#);
    filter.apply("read_file", r#"{"file_path":"/w"}"#);
    filter.apply("read_file", r#"{"file_path":"/w","pages":"","options":""}"#);

    // Assert
    assert_eq!(
        filter.removed(),
        [("read_file".to_string(), 1), ("read_file".to_string(), 2)]
    );
}
