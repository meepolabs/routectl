//! Canonical `req.tools` + `req.tool_choice` -> Responses
//! `tools` / `tool_choice` translation.
//!
//! `ToolDef::Custom` -> flat Responses shape `{type, name, description?,
//! parameters, strict?}`. `ToolDef::Other` passes through verbatim so
//! Anthropic builtins / future shapes ride the egress without code edits.
//!
//! tool_choice mapping:
//!   - `"auto"` / `"required"` / `"none"` -> bare-string passthrough
//!   - named function (OpenAI object shape, Anthropic-shape, or any
//!     `{"name"}`-bearing object) -> flat Responses shape
//!     `{"type":"function","name":"X"}` (smoke 2026-05-12 confirmed the
//!     nested chat-completions shape is rejected with
//!     "Unknown parameter: 'tool_choice.function'").

use serde_json::{Value, json};

use routectl_core::{ChatRequest, ToolDef};

use super::types::{ResponsesFunctionTag, ResponsesTool};

/// Translate `req.tools` into the Responses `tools` array. Returns an
/// empty Vec when no tools are configured -- the parent
/// `ResponsesRequest` skips serializing the field when empty.
pub(super) fn translate_tools(req: &ChatRequest) -> Vec<ResponsesTool> {
    let Some(tools) = req.tools.as_ref() else {
        return Vec::new();
    };
    let mut out: Vec<ResponsesTool> = Vec::with_capacity(tools.len());
    for td in tools {
        match td {
            ToolDef::Custom(c) => {
                // Flat Responses shape: {type, name, description?, parameters, strict?}
                // The chat-completions nested shape ({type, function:{name,...}}) is
                // rejected by the chatgpt-oauth backend with
                // "Missing required parameter: 'tools[0].name'" (smoke 2026-05-12).
                out.push(ResponsesTool::Function {
                    kind: ResponsesFunctionTag::Function,
                    name: c.name.clone(),
                    description: c.description.clone(),
                    parameters: c.input_schema.clone(),
                    strict: c.strict,
                });
            }
            ToolDef::Other(v) => {
                // Forward-compat passthrough. Anthropic builtins and
                // future shapes ride here unchanged so the Responses
                // server can surface its own error if it doesn't
                // accept them.
                out.push(ResponsesTool::Other(v.clone()));
            }
        }
    }
    out
}

/// Translate `req.tool_choice` to the Responses-shape value. Returns
/// None when canonical has no tool_choice. Bare-string shapes
/// (`auto`/`required`/`none`) pass through verbatim; OpenAI / Anthropic
/// named-function shapes collapse to flat Responses shape
/// `{"type":"function","name":"X"}`.
pub(super) fn translate_tool_choice(tc: Option<&Value>) -> Option<Value> {
    let tc = tc?;
    match tc {
        Value::String(s) => translate_tool_choice_string(s),
        Value::Object(map) => translate_tool_choice_object(map),
        _ => None,
    }
}

fn translate_tool_choice_string(s: &str) -> Option<Value> {
    match s {
        "auto" | "required" | "none" => Some(Value::String(s.to_string())),
        other => {
            tracing::warn!(
                tool_choice = %other,
                "unknown bare-string tool_choice; dropping on Responses egress"
            );
            None
        }
    }
}

/// Object shapes recognized:
///   - OpenAI: `{"type":"function","function":{"name":"X"}}`
///   - Anthropic: `{"type":"tool","name":"X"}` (and `{"type":"auto"|"any"}`)
///   - Generic: any object with a `name` (or nested `function.name`)
///     string -> emit named-function shape.
fn translate_tool_choice_object(map: &serde_json::Map<String, Value>) -> Option<Value> {
    // Anthropic-shape `{"type":"auto"|"any"}` -> string equivalents.
    match map.get("type").and_then(|v| v.as_str()) {
        Some("auto") => return Some(Value::String("auto".into())),
        Some("any") | Some("required") => return Some(Value::String("required".into())),
        Some("none") => return Some(Value::String("none".into())),
        _ => {}
    }

    let name = extract_tool_name(map)?;
    if name.is_empty() {
        tracing::warn!("tool_choice missing or invalid name; dropping field on Responses egress");
        return None;
    }
    // Flat Responses shape: {"type":"function","name":"X"}
    // The chat-completions nested shape ({"type":"function","function":{"name":"X"}})
    // is rejected by the chatgpt-oauth backend with
    // "Unknown parameter: 'tool_choice.function'" (smoke 2026-05-12).
    Some(json!({
        "type": "function",
        "name": name
    }))
}

fn extract_tool_name(map: &serde_json::Map<String, Value>) -> Option<String> {
    // OpenAI shape: `{"type":"function","function":{"name":"X"}}`.
    if let Some(name) = map
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(|n| n.as_str())
    {
        return Some(name.to_string());
    }
    // Anthropic shape: `{"type":"tool","name":"X"}`. Falls back to any
    // top-level `name`.
    if let Some(name) = map.get("name").and_then(|n| n.as_str()) {
        return Some(name.to_string());
    }
    None
}
