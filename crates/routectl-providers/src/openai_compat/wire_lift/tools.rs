//! Lift canonical `req.tools` (`Vec<ToolDef>`) into the OpenAI
//! `{type:"function", function:{name, description, parameters, strict?}}`
//! wire shape.
//!
//! Idempotency: a `ToolDef::Other` that is already OpenAI function-shape
//! (detected via `CustomTool::from_openai_function`) is emitted verbatim
//! so a pass-through of an OpenAI-in request produces byte-identical output.

use serde_json::Value;

use routectl_core::{ChatRequest, CustomTool, Result, ToolDef};

use super::reject_or_drop_unrepresentable;

pub fn lift(
    id: &str,
    obj: &mut serde_json::Map<String, Value>,
    req: &ChatRequest,
    strict: bool,
) -> Result<()> {
    let tools = match req.tools.as_ref() {
        Some(t) if !t.is_empty() => t,
        _ => {
            obj.remove("tools");
            return Ok(());
        }
    };

    let mut lifted: Vec<Value> = Vec::with_capacity(tools.len());
    let mut dropped_non_custom = false;
    for tool in tools {
        match tool {
            ToolDef::Custom(c) => {
                lifted.push(custom_to_openai(c));
            }
            ToolDef::Other(v) => {
                if CustomTool::from_openai_function(v).is_some() {
                    // Already OpenAI function shape -- pass through verbatim.
                    lifted.push(v.clone());
                } else {
                    // Cross-dialect translation lane: an Anthropic builtin
                    // (`web_search_*`, `bash_*`, `code_execution_*`) or an
                    // otherwise-unmodeled tool shape reaching the
                    // OpenAI-compat egress. Drop rather than forward -- the
                    // OpenAI `tools` array admits only
                    // `{type:"function", function:{...}}`, and a builtin is
                    // a SERVER-SIDE capability of the Anthropic API rather
                    // than a schema an OpenAI-compat host could execute, so
                    // no translation exists. Forwarding it verbatim 400s a
                    // strict host on the unknown `type`. Baked seed verdict:
                    // it stands until this lane's own wire evidence
                    // contradicts it, and is not eligible for deletion
                    // until then.
                    // TRANSLATION-DROP: lane=openai-compat class=non_custom_tool_unrepresentable test=anthropic_builtin_tool_drops_and_warns
                    let builtin = v
                        .as_object()
                        .and_then(|o| o.get("type"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("unknown");
                    let tool_name = v
                        .as_object()
                        .and_then(|o| o.get("name"))
                        .and_then(|n| n.as_str())
                        .unwrap_or("");
                    dropped_non_custom = true;
                    reject_or_drop_unrepresentable(
                        id,
                        strict,
                        &format!("tool `{tool_name}`"),
                        &format!("Anthropic builtin / non-custom tool `{builtin}`"),
                    )?;
                    // Do not push -- tool is dropped.
                }
            }
        }
    }

    if lifted.is_empty() {
        obj.remove("tools");
    } else {
        obj.insert("tools".to_string(), Value::Array(lifted));
    }

    // One counted drop event per REQUEST, at this lift's single exit: a
    // request declaring three builtins is one loss an operator acts on,
    // not three. Strict mode never arrives -- the arm above returned Err.
    if dropped_non_custom {
        crate::translation_drop_metrics::record_translation_drop(
            "openai-compat",
            "non_custom_tool_unrepresentable",
        );
    }

    Ok(())
}

fn custom_to_openai(c: &CustomTool) -> Value {
    let mut func = serde_json::Map::new();
    func.insert("name".to_string(), Value::String(c.name.clone()));
    if let Some(desc) = &c.description {
        func.insert("description".to_string(), Value::String(desc.clone()));
    }
    func.insert("parameters".to_string(), c.input_schema.clone());
    if let Some(strict) = c.strict {
        func.insert("strict".to_string(), Value::Bool(strict));
    }
    serde_json::json!({
        "type": "function",
        "function": Value::Object(func)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{Message, MessageContent, Role};
    use serde_json::json;

    fn make_req(tools: Option<Vec<ToolDef>>) -> ChatRequest {
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
            tools,
            ..Default::default()
        }
    }

    fn run(
        tools: Option<Vec<ToolDef>>,
        strict: bool,
    ) -> (serde_json::Map<String, Value>, Result<()>) {
        let req = make_req(tools);
        let mut obj = serde_json::Map::new();
        let result = lift("test", &mut obj, &req, strict);
        (obj, result)
    }

    #[test]
    fn custom_tool_lifts_to_function_shape() {
        // Arrange
        let tool = ToolDef::Custom(CustomTool {
            name: "calculator".into(),
            description: Some("do math".into()),
            input_schema: json!({"type": "object", "properties": {"expr": {"type": "string"}}, "required": ["expr"]}),
            cache_control: None,
            defer_loading: None,
            strict: None,
            type_tag: None,
        });

        // Act
        let (obj, res) = run(Some(vec![tool]), false);
        res.unwrap();

        // Assert
        let tools = obj["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], "function");
        assert_eq!(tools[0]["function"]["name"], "calculator");
        assert_eq!(tools[0]["function"]["description"], "do math");
        assert_eq!(
            tools[0]["function"]["parameters"]["properties"]["expr"]["type"],
            "string"
        );
        assert!(
            tools[0].get("input_schema").is_none(),
            "Anthropic input_schema must not leak"
        );
    }

    #[test]
    fn custom_tool_with_strict_emits_strict_field() {
        // Arrange
        let tool = ToolDef::Custom(CustomTool {
            name: "fn".into(),
            description: None,
            input_schema: json!({"type": "object", "properties": {}}),
            cache_control: None,
            defer_loading: None,
            strict: Some(true),
            type_tag: None,
        });

        // Act
        let (obj, res) = run(Some(vec![tool]), false);
        res.unwrap();

        // Assert
        assert_eq!(obj["tools"][0]["function"]["strict"], true);
    }

    #[test]
    fn other_with_openai_shape_passes_through_verbatim() {
        // Arrange -- a ToolDef::Other that is already OpenAI function-shape.
        let v = json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }
        });
        let tool = ToolDef::Other(v.clone());

        // Act
        let (obj, res) = run(Some(vec![tool]), false);
        res.unwrap();

        // Assert -- byte-identical passthrough
        assert_eq!(obj["tools"][0], v);
    }

    #[test]
    #[serial_test::serial(openai_compat_non_custom_tool_unrepresentable)]
    fn other_with_anthropic_builtin_warns_and_drops_non_strict() {
        // Arrange -- Anthropic builtin tool, not representable on OpenAI wire.
        let tool = ToolDef::Other(json!({"type": "web_search_20250305", "name": "web_search"}));

        // Act
        let (obj, res) = run(Some(vec![tool]), false);
        res.unwrap();

        // Assert -- tool is dropped, key removed
        assert!(obj.get("tools").is_none(), "builtin tool must be dropped");
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
    /// an upstream would receive, plus every captured event.
    fn emitted_wire(tools: Vec<ToolDef>) -> (String, Vec<routectl_testkit::CapturedEvent>) {
        let req = make_req(Some(tools));
        let mut obj = serde_json::Map::new();
        let events = routectl_testkit::capture_events(|| {
            lift("test", &mut obj, &req, false).expect("lenient lift must succeed");
        });
        let wire = serde_json::to_string(&Value::Object(obj)).expect("wire body serializes");
        (wire, events)
    }

    /// NEGATIVE CONTROL: an Anthropic builtin tool drops, warns with its
    /// structured fields, and none of its shape reaches the emitted wire
    /// body -- while a representable custom sibling declared in the same
    /// request rides through in that same body.
    #[test]
    #[serial_test::serial(openai_compat_non_custom_tool_unrepresentable)]
    fn anthropic_builtin_tool_drops_and_warns() {
        // Arrange -- one builtin plus one representable custom tool.
        let builtin = ToolDef::Other(json!({
            "type": "marker_builtin_kind", "name": "marker_builtin_name"
        }));
        let custom = ToolDef::Custom(CustomTool {
            name: "marker_surviving_custom".into(),
            description: None,
            input_schema: json!({"type": "object", "properties": {}}),
            cache_control: None,
            defer_loading: None,
            strict: None,
            type_tag: None,
        });

        // Act
        let before = drop_count("non_custom_tool_unrepresentable");
        let (wire, events) = emitted_wire(vec![builtin, custom]);
        let after = drop_count("non_custom_tool_unrepresentable");

        // Assert 1 -- the drop warned, naming the tool and the shape.
        let warn = events
            .iter()
            .find(|e| e.level == tracing::Level::WARN)
            .unwrap_or_else(|| panic!("the drop must warn, got: {events:?}"));
        assert_eq!(warn.field("provider"), Some("test"));
        assert_eq!(warn.field("context"), Some("tool `marker_builtin_name`"));
        assert_eq!(
            warn.field("what"),
            Some("Anthropic builtin / non-custom tool `marker_builtin_kind`")
        );

        // Assert 2 -- no trace of the builtin reached the emitted body, in
        // any key. A builtin re-serialized inside some other tool's payload
        // would still 400 the upstream.
        assert!(
            !wire.contains("marker_builtin_kind") && !wire.contains("marker_builtin_name"),
            "the builtin must not reach the wire in any form, got: {wire}"
        );

        // Assert 3 -- the representable sibling survived in that same body.
        assert!(
            wire.contains("marker_surviving_custom"),
            "the representable custom tool must survive, got: {wire}"
        );

        assert_eq!(
            after - before,
            1,
            "the drop must be counted exactly once for the request"
        );
    }

    /// POSITIVE CONTROL: a `ToolDef::Other` that is ALREADY OpenAI
    /// function-shape takes the same `Other` arm and must NOT drop or warn.
    /// Without this, the fixture above would pass on a lift that dropped
    /// every `Other` variant indiscriminately.
    #[test]
    fn openai_shape_other_tool_survives_without_warning() {
        // Arrange
        let tool = ToolDef::Other(json!({
            "type": "function",
            "function": {"name": "marker_openai_shape_tool", "parameters": {"type": "object"}}
        }));

        // Act
        let (wire, events) = emitted_wire(vec![tool]);

        // Assert
        assert!(
            !events.iter().any(|e| e.level == tracing::Level::WARN),
            "an already-OpenAI-shape tool must not warn at all, got: {events:?}"
        );
        assert!(
            wire.contains("marker_openai_shape_tool"),
            "the OpenAI-shape tool must reach the wire verbatim, got: {wire}"
        );
    }

    /// Two dropped builtins in ONE request are ONE counted drop event.
    #[test]
    #[serial_test::serial(openai_compat_non_custom_tool_unrepresentable)]
    fn two_dropped_builtins_count_as_one_request_drop() {
        // Arrange
        let tools = vec![
            ToolDef::Other(json!({"type": "web_search_20250305", "name": "a"})),
            ToolDef::Other(json!({"type": "bash_20250124", "name": "b"})),
        ];

        // Act
        let before = drop_count("non_custom_tool_unrepresentable");
        let (_obj, res) = run(Some(tools), false);
        res.unwrap();
        let after = drop_count("non_custom_tool_unrepresentable");

        // Assert
        assert_eq!(
            after - before,
            1,
            "two dropped tools in one request must count once"
        );
    }

    #[test]
    #[serial_test::serial(openai_compat_non_custom_tool_unrepresentable)]
    fn other_with_anthropic_builtin_strict_returns_err() {
        // Arrange
        let tool = ToolDef::Other(json!({"type": "web_search_20250305", "name": "web_search"}));

        // Act
        let (_obj, res) = run(Some(vec![tool]), true);

        // Assert
        assert!(res.is_err(), "strict mode must reject builtin tools");
        let msg = format!("{}", res.unwrap_err());
        assert!(
            msg.contains("strict_translation"),
            "error must mention strict_translation"
        );
    }

    #[test]
    fn empty_tools_removes_key() {
        // Arrange -- explicitly empty tools vec
        let (obj, res) = run(Some(vec![]), false);
        res.unwrap();

        // Assert
        assert!(obj.get("tools").is_none(), "empty tools must remove key");
    }

    #[test]
    fn no_tools_removes_key() {
        // Arrange
        let (obj, res) = run(None, false);
        res.unwrap();

        // Assert
        assert!(obj.get("tools").is_none());
    }

    #[test]
    fn custom_tool_cache_control_is_not_emitted() {
        // Arrange -- a Custom tool with cache_control set; it must not leak
        // into the OpenAI function shape because cache_control is Anthropic-only.
        let tool = ToolDef::Custom(CustomTool {
            name: "search".into(),
            description: Some("search the web".into()),
            input_schema: json!({"type": "object", "properties": {}}),
            cache_control: Some(routectl_core::CacheControl::ephemeral_5m()),
            defer_loading: None,
            strict: None,
            type_tag: None,
        });

        // Act
        let (obj, res) = run(Some(vec![tool]), false);
        res.unwrap();

        // Assert -- function object has no cache_control field
        let func = &obj["tools"][0]["function"];
        assert!(
            func.get("cache_control").is_none(),
            "cache_control must not appear in the OpenAI function shape"
        );
    }
}
