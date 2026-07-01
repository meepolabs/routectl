//! Canonical `req.tools` + `req.tool_choice` -> Anthropic wire
//! translation.
//!
//! `translate_tool` maps a `ToolDef` onto an `AnthropicTool`: typed
//! `CustomTool` carries cache_control / defer_loading / strict / type
//! tag through; a legacy OpenAI-shape `{type:"function",function:{...}}`
//! arriving via `ToolDef::Other` is rewritten to `AnthropicTool::Custom`
//! (so callers that bypass the OpenAI ingress still get a working body);
//! anything else passes through verbatim as `AnthropicTool::Builtin`.
//! `translate_tool_choice` maps OpenAI / Anthropic tool_choice shapes
//! onto the Anthropic `{type:auto|any|tool}` form. `translate_tool` is
//! `pub(crate)` so the Bedrock Converse egress can reuse it.

use serde_json::{Value, json};

use routectl_core::{CustomTool, ToolDef};

use super::types::AnthropicTool;

/// Anthropic `tool_choice.type` values that force tool use. Pairing
/// either with `thinking` causes a 400 (extended-thinking docs).
pub(super) const TOOL_CHOICE_TYPE_ANY: &str = "any";
pub(super) const TOOL_CHOICE_TYPE_TOOL: &str = "tool";

fn translate_custom_tool(c: &CustomTool) -> AnthropicTool {
    AnthropicTool::Custom {
        name: c.name.clone(),
        description: c.description.clone(),
        input_schema: c.input_schema.clone(),
        cache_control: c.cache_control.clone(),
        defer_loading: c.defer_loading,
        strict: c.strict,
        type_tag: c.type_tag.clone(),
    }
}

pub(crate) fn translate_tool(td: &ToolDef) -> AnthropicTool {
    match td {
        ToolDef::Custom(c) => translate_custom_tool(c),
        ToolDef::Other(v) => {
            // Backwards-compat: a legacy OpenAI-shape tool
            // `{type: "function", function: {name, description, parameters}}`
            // arriving via ToolDef::Other gets translated to
            // AnthropicTool::Custom so callers that bypass the OpenAI
            // ingress still get a working Anthropic body. Anything else
            // (Anthropic builtins, server-side, future shapes) passes
            // through verbatim as Builtin.
            if let Some(custom) = openai_function_to_custom(v) {
                custom
            } else {
                AnthropicTool::Builtin(v.clone())
            }
        }
    }
}

fn openai_function_to_custom(v: &Value) -> Option<AnthropicTool> {
    let obj = v.as_object()?;
    let is_function = obj.get("type").and_then(|t| t.as_str()) == Some("function");
    if !is_function {
        return None;
    }
    let func = obj.get("function")?.as_object()?;
    let name = func.get("name")?.as_str()?.to_string();
    let description = func
        .get("description")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let input_schema = func
        .get("parameters")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    let strict = func.get("strict").and_then(|v| v.as_bool());
    Some(AnthropicTool::Custom {
        name,
        description,
        input_schema,
        cache_control: None,
        defer_loading: None,
        strict,
        type_tag: None,
    })
}

/// Translate canonical `tool_choice` values into the Anthropic-shape
/// object the Messages API requires.
///
/// Mapping:
///   - bare `"auto"` -> `{"type":"auto"}`
///   - bare `"required"` -> `{"type":"any"}`
///   - bare `"none"` -> field dropped; the caller must also drop
///     `tools` (otherwise Anthropic defaults to `auto` and may call
///     them, silently flipping the caller's "do not call tools" intent)
///   - OpenAI `{"type":"function","function":{"name":X}}` ->
///     `{"type":"tool","name":X}`
///   - already-Anthropic shape -> passthrough
///   - anything else -> passthrough (let the upstream decide)
pub(super) fn translate_tool_choice(tc: Option<&Value>, has_tools: bool) -> Option<Value> {
    let tc = tc?;
    match tc {
        Value::String(s) => match s.as_str() {
            "auto" => Some(serde_json::json!({"type":"auto"})),
            "required" => Some(serde_json::json!({"type": TOOL_CHOICE_TYPE_ANY})),
            "none" => {
                if has_tools {
                    tracing::warn!(
                        "tool_choice=\"none\" with tools present: routectl drops both fields so \
                         Anthropic cannot auto-select (Anthropic has no native equivalent of \
                         OpenAI's \"none\")"
                    );
                }
                None
            }
            _ => Some(tc.clone()),
        },
        Value::Object(map) => match map.get("type").and_then(|v| v.as_str()) {
            Some("function") => {
                let name = map
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str());
                match name {
                    Some(n) => Some(serde_json::json!({"type": TOOL_CHOICE_TYPE_TOOL, "name": n})),
                    None => {
                        tracing::warn!(
                            "tool_choice with type=\"function\" but missing function.name; \
                             passed through as-is and Anthropic will reject it"
                        );
                        Some(tc.clone())
                    }
                }
            }
            Some("auto") | Some("any") | Some("tool") | Some("none") => Some(tc.clone()),
            _ => Some(tc.clone()),
        },
        _ => Some(tc.clone()),
    }
}
