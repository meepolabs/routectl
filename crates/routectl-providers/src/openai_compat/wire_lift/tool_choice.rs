//! Lift `req.tool_choice` from Anthropic wire shape to OpenAI wire shape.
//!
//! Anthropic accepts:
//!   "auto" | "none" | "required"         (bare strings, shared with OpenAI)
//!   {"type":"auto"} | {"type":"any"} | {"type":"none"}
//!   {"type":"tool", "name":"X"}
//!
//! OpenAI accepts:
//!   "auto" | "none" | "required"         (bare strings)
//!   {"type":"function", "function":{"name":"X"}}
//!
//! Mapping:
//!   bare strings                     -> passthrough
//!   {"type":"function",...}          -> passthrough (already OpenAI)
//!   {"type":"tool","name":"X"}       -> {"type":"function","function":{"name":"X"}}
//!   {"type":"auto"}                  -> "auto"
//!   {"type":"any"}                   -> "required"
//!   {"type":"none"}                  -> "none"
//!   anything else                    -> warn + drop

use serde_json::Value;
use tracing::warn;

use routectl_core::{ChatRequest, Result};

pub fn lift(
    id: &str,
    obj: &mut serde_json::Map<String, Value>,
    req: &ChatRequest,
) -> Result<()> {
    let tc = match req.tool_choice.as_ref() {
        Some(v) => v,
        None => {
            obj.remove("tool_choice");
            return Ok(());
        }
    };

    let lifted = map_tool_choice(id, tc);
    match lifted {
        Some(v) => {
            obj.insert("tool_choice".to_string(), v);
        }
        None => {
            obj.remove("tool_choice");
        }
    }

    Ok(())
}

/// Map one tool_choice value to OpenAI shape. Returns None when the
/// shape is unrecognized (caller drops the field).
fn map_tool_choice(id: &str, tc: &Value) -> Option<Value> {
    // Bare string: passthrough.
    if let Some(s) = tc.as_str() {
        return Some(Value::String(s.to_string()));
    }

    let obj = tc.as_object()?;
    let kind = obj.get("type").and_then(|t| t.as_str())?;

    match kind {
        // Already OpenAI function-name shape.
        "function" => Some(tc.clone()),

        // Anthropic specific-tool: rewrite to OpenAI function-name object.
        "tool" => {
            let name = match obj.get("name").and_then(|n| n.as_str()) {
                Some(n) => n,
                None => {
                    warn!(
                        provider = id,
                        shape_type = "tool",
                        "openai-compat egress: tool_choice {{type:\"tool\"}} missing or invalid name; dropping field"
                    );
                    return None;
                }
            };
            Some(serde_json::json!({
                "type": "function",
                "function": {"name": name}
            }))
        }

        // Anthropic auto -> OpenAI "auto".
        "auto" => Some(Value::String("auto".to_string())),

        // Anthropic any -> OpenAI "required".
        "any" => Some(Value::String("required".to_string())),

        // Anthropic none -> OpenAI "none".
        "none" => Some(Value::String("none".to_string())),

        // Unknown shape.
        other => {
            warn!(
                provider = id,
                shape = other,
                "openai-compat egress: unrecognized tool_choice shape dropped"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{Message, MessageContent, Role};
    use serde_json::json;

    fn make_req(tool_choice: Option<Value>) -> ChatRequest {
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
            tool_choice,
            ..Default::default()
        }
    }

    fn run(tc: Option<Value>) -> Option<Value> {
        let req = make_req(tc);
        let mut obj = serde_json::Map::new();
        lift("test", &mut obj, &req).unwrap();
        obj.get("tool_choice").cloned()
    }

    #[test]
    fn bare_string_auto_passes_through() {
        // Arrange + Act + Assert
        assert_eq!(run(Some(json!("auto"))), Some(json!("auto")));
    }

    #[test]
    fn bare_string_none_passes_through() {
        assert_eq!(run(Some(json!("none"))), Some(json!("none")));
    }

    #[test]
    fn bare_string_required_passes_through() {
        assert_eq!(run(Some(json!("required"))), Some(json!("required")));
    }

    #[test]
    fn openai_function_object_passes_through() {
        // Arrange
        let tc = json!({"type": "function", "function": {"name": "calculator"}});

        // Act + Assert -- passthrough verbatim
        assert_eq!(run(Some(tc.clone())), Some(tc));
    }

    #[test]
    fn anthropic_tool_type_rewrites_to_openai_function() {
        // Arrange
        let tc = json!({"type": "tool", "name": "calculator"});

        // Act
        let result = run(Some(tc)).unwrap();

        // Assert
        assert_eq!(result["type"], "function");
        assert_eq!(result["function"]["name"], "calculator");
    }

    #[test]
    fn anthropic_auto_object_rewrites_to_string() {
        // Arrange + Act + Assert
        assert_eq!(run(Some(json!({"type": "auto"}))), Some(json!("auto")));
    }

    #[test]
    fn anthropic_any_rewrites_to_required() {
        // Arrange + Act + Assert
        assert_eq!(run(Some(json!({"type": "any"}))), Some(json!("required")));
    }

    #[test]
    fn anthropic_none_object_rewrites_to_string() {
        // Arrange + Act + Assert
        assert_eq!(run(Some(json!({"type": "none"}))), Some(json!("none")));
    }

    #[test]
    fn unknown_shape_is_dropped() {
        // Arrange -- a shape routectl has never seen.
        let tc = json!({"type": "custom_unknown_shape"});

        // Act + Assert -- field absent after lift
        assert_eq!(run(Some(tc)), None);
    }

    #[test]
    fn no_tool_choice_removes_key() {
        // Arrange + Act + Assert
        assert_eq!(run(None), None);
    }
}
