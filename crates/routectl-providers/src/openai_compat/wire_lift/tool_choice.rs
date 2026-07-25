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

use super::reject_or_drop_unrepresentable;

pub fn lift(
    id: &str,
    obj: &mut serde_json::Map<String, Value>,
    req: &ChatRequest,
    strict: bool,
) -> Result<()> {
    let tc = if let Some(v) = req.tool_choice.as_ref() {
        v
    } else {
        obj.remove("tool_choice");
        return Ok(());
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

    // A forcing tool_choice with no tools to force is unrepresentable:
    // OpenAI hosts 400 on `tool_choice:"required"` (or a named function)
    // when `tools` is empty or absent. `tools` is read from the WIRE
    // (`obj["tools"]`), which is the post-tools-lift state because the
    // tools step runs before tool_choice in LIFT_STEPS. In lenient mode
    // we drop the forcing tool_choice and warn; strict rejects.
    if wire_tools_empty(obj) && tool_choice_is_forcing(obj.get("tool_choice")) {
        reject_or_drop_unrepresentable(
            id,
            strict,
            "tool_choice",
            "forcing tool_choice with no tools to force",
        )?;
        obj.remove("tool_choice");
    }

    Ok(())
}

/// True when the wire body carries no usable `tools` array (absent or
/// empty). The tools lift runs first and removes the key when no tool
/// survives, so an empty/absent `obj["tools"]` is the authoritative
/// "nothing to force" signal.
fn wire_tools_empty(obj: &serde_json::Map<String, Value>) -> bool {
    match obj.get("tools") {
        Some(Value::Array(a)) => a.is_empty(),
        Some(_) => false,
        None => true,
    }
}

/// True when the (post-map) wire tool_choice forces a tool call:
/// the bare string `"required"` or an OpenAI `{type:"function", ...}`
/// object. `"auto"` / `"none"` are not forcing. `map_tool_choice` nests
/// a named tool under `function`, so no forcing output carries a bare
/// top-level `name` key.
fn tool_choice_is_forcing(tc: Option<&Value>) -> bool {
    match tc {
        Some(Value::String(s)) => s == "required",
        Some(Value::Object(o)) => {
            matches!(o.get("type").and_then(|t| t.as_str()), Some("function"))
        }
        _ => false,
    }
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
            let name = if let Some(n) = obj.get("name").and_then(|n| n.as_str()) {
                n
            } else {
                warn!(
                    provider = id,
                    shape_type = "tool",
                    "openai-compat egress: tool_choice {{type:\"tool\"}} missing or invalid name; dropping field"
                );
                return None;
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
            tool_choice,
            ..Default::default()
        }
    }

    fn run(tc: Option<Value>) -> Option<Value> {
        let req = make_req(tc);
        let mut obj = serde_json::Map::new();
        lift("test", &mut obj, &req, false).unwrap();
        obj.get("tool_choice").cloned()
    }

    /// Variant of `run` that seeds the wire body with a non-empty `tools`
    /// array so a forcing tool_choice has something to force (the forcing-choice
    /// guard only fires when wire tools are empty/absent).
    fn run_with_tools(tc: Option<Value>) -> Option<Value> {
        let req = make_req(tc);
        let mut obj = serde_json::Map::new();
        obj.insert(
            "tools".into(),
            json!([{"type": "function", "function": {"name": "f"}}]),
        );
        lift("test", &mut obj, &req, false).unwrap();
        obj.get("tool_choice").cloned()
    }

    /// Strict variant that seeds empty wire tools and surfaces the Result.
    fn run_strict_empty_tools(tc: Option<Value>) -> Result<Option<Value>> {
        let req = make_req(tc);
        let mut obj = serde_json::Map::new();
        obj.insert("tools".into(), json!([]));
        lift("test", &mut obj, &req, true)?;
        Ok(obj.get("tool_choice").cloned())
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
        // `required` is forcing, so the forcing-choice guard would drop it unless
        // the wire carries tools. Seed tools so the choice survives.
        let req = make_req(Some(json!("required")));
        let mut obj = serde_json::Map::new();
        obj.insert(
            "tools".into(),
            json!([{"type": "function", "function": {"name": "f"}}]),
        );
        lift("test", &mut obj, &req, false).unwrap();

        assert_eq!(
            obj.get("tool_choice"),
            Some(&json!("required")),
            "forcing tool_choice must survive when tools are present"
        );
        assert!(
            obj.get("tools").is_some(),
            "seeded tools must still be on the wire"
        );
    }

    #[test]
    fn openai_function_object_passes_through() {
        // Arrange -- forcing function object; seed tools so the forcing-choice guard
        // does not drop it.
        let tc = json!({"type": "function", "function": {"name": "calculator"}});
        let req = make_req(Some(tc.clone()));
        let mut obj = serde_json::Map::new();
        obj.insert(
            "tools".into(),
            json!([{"type": "function", "function": {"name": "f"}}]),
        );

        // Act
        lift("test", &mut obj, &req, false).unwrap();

        // Assert -- passthrough verbatim and the guard did not fire.
        assert_eq!(obj.get("tool_choice"), Some(&tc));
        assert!(obj.get("tools").is_some(), "seeded tools must survive");
    }

    #[test]
    fn anthropic_tool_type_rewrites_to_openai_function() {
        // Arrange -- forcing named tool; seed tools so the forcing-choice guard does
        // not drop the rewritten choice.
        let tc = json!({"type": "tool", "name": "calculator"});
        let req = make_req(Some(tc));
        let mut obj = serde_json::Map::new();
        obj.insert(
            "tools".into(),
            json!([{"type": "function", "function": {"name": "f"}}]),
        );

        // Act
        lift("test", &mut obj, &req, false).unwrap();
        let result = obj.get("tool_choice").cloned().unwrap();

        // Assert
        assert_eq!(result["type"], "function");
        assert_eq!(result["function"]["name"], "calculator");
        assert!(obj.get("tools").is_some(), "seeded tools must survive");
    }

    #[test]
    fn anthropic_auto_object_rewrites_to_string() {
        // Arrange + Act + Assert
        assert_eq!(run(Some(json!({"type": "auto"}))), Some(json!("auto")));
    }

    #[test]
    fn anthropic_any_rewrites_to_required() {
        // Arrange -- `any` maps to the forcing `required`; seed tools so the
        // forcing-choice guard does not drop it.
        let req = make_req(Some(json!({"type": "any"})));
        let mut obj = serde_json::Map::new();
        obj.insert(
            "tools".into(),
            json!([{"type": "function", "function": {"name": "f"}}]),
        );

        // Act
        lift("test", &mut obj, &req, false).unwrap();

        // Assert
        assert_eq!(obj.get("tool_choice"), Some(&json!("required")));
        assert!(obj.get("tools").is_some(), "seeded tools must survive");
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

    /// A forcing tool_choice ({type:"any"} -> "required") with
    /// no tools on the wire is unrepresentable; lenient mode drops it.
    #[test]
    fn forcing_tool_choice_without_tools_dropped_lenient() {
        // Arrange -- empty wire tools + a forcing Anthropic `any` choice.
        let req = make_req(Some(json!({"type": "any"})));
        let mut obj = serde_json::Map::new();
        obj.insert("tools".into(), json!([]));

        // Act
        lift("test", &mut obj, &req, false).unwrap();

        // Assert -- tool_choice removed (cannot force with no tools).
        assert!(
            obj.get("tool_choice").is_none(),
            "forcing tool_choice with empty tools must be dropped"
        );
    }

    /// The same forcing-without-tools case errors under strict.
    #[test]
    fn forcing_tool_choice_without_tools_strict_errors() {
        // Act
        let res = run_strict_empty_tools(Some(json!({"type": "any"})));

        // Assert
        assert!(
            res.is_err(),
            "strict mode must reject forcing tc without tools"
        );
        let msg = format!("{}", res.unwrap_err());
        assert!(msg.contains("strict_translation"));
    }

    /// "auto" is not forcing -- it survives even with no tools.
    #[test]
    fn auto_tool_choice_without_tools_untouched() {
        // Arrange
        let req = make_req(Some(json!("auto")));
        let mut obj = serde_json::Map::new();

        // Act
        lift("test", &mut obj, &req, false).unwrap();

        // Assert
        assert_eq!(obj.get("tool_choice"), Some(&json!("auto")));
    }

    /// A forcing choice WITH tools present is untouched.
    #[test]
    fn forcing_tool_choice_with_tools_untouched() {
        // Arrange + Act + Assert
        assert_eq!(
            run_with_tools(Some(json!("required"))),
            Some(json!("required"))
        );
    }
}
