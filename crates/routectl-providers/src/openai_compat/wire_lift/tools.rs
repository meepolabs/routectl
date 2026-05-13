//! Lift canonical `req.tools` (Vec<ToolDef>) into the OpenAI
//! `{type:"function", function:{name, description, parameters, strict?}}`
//! wire shape.
//!
//! Idempotency: a `ToolDef::Other` that is already OpenAI function-shape
//! (detected via `CustomTool::from_openai_function`) is emitted verbatim
//! so a pass-through of an OpenAI-in request produces byte-identical output.

use serde_json::Value;
use tracing::warn;

use routectl_core::{ChatRequest, CustomTool, Error, Result, ToolDef};

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
                    // Anthropic builtin or unknown shape.
                    let builtin = v
                        .as_object()
                        .and_then(|o| o.get("type"))
                        .and_then(|t| t.as_str())
                        .unwrap_or("unknown");
                    if strict {
                        return Err(Error::Validation(format!(
                            "strict_translation: provider `{id}`: Anthropic builtin / \
                             non-custom tool `{builtin}` cannot be represented on the \
                             OpenAI-compat wire"
                        )));
                    }
                    warn!(
                        provider = id,
                        builtin = builtin,
                        "dropping anthropic-builtin tool on openai-compat egress"
                    );
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
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
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
    fn other_with_anthropic_builtin_warns_and_drops_non_strict() {
        // Arrange -- Anthropic builtin tool, not representable on OpenAI wire.
        let tool = ToolDef::Other(json!({"type": "web_search_20250305", "name": "web_search"}));

        // Act
        let (obj, res) = run(Some(vec![tool]), false);
        res.unwrap();

        // Assert -- tool is dropped, key removed
        assert!(obj.get("tools").is_none(), "builtin tool must be dropped");
    }

    #[test]
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
