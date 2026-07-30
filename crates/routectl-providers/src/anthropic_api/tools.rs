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

pub fn translate_tool(td: &ToolDef) -> AnthropicTool {
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
        .map(std::string::ToString::to_string);
    let input_schema = func
        .get("parameters")
        .cloned()
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
    let strict = func.get("strict").and_then(serde_json::Value::as_bool);
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
///   - bare tool-name string `"X"` -> `{"type":"tool","name":"X"}`
///     (CCR / Cursor-compat "force this tool")
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
            _ => Some(serde_json::json!({"type": TOOL_CHOICE_TYPE_TOOL, "name": s})),
        },
        Value::Object(map) => match map.get("type").and_then(|v| v.as_str()) {
            Some("function") => {
                let name = map
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(|n| n.as_str());
                if let Some(n) = name {
                    Some(serde_json::json!({"type": TOOL_CHOICE_TYPE_TOOL, "name": n}))
                } else {
                    tracing::warn!(
                        "tool_choice with type=\"function\" but missing function.name; \
                         passed through as-is and Anthropic will reject it"
                    );
                    Some(tc.clone())
                }
            }
            Some("auto" | "any" | "tool" | "none") => Some(tc.clone()),
            _ => Some(tc.clone()),
        },
        _ => Some(tc.clone()),
    }
}

/// Key the OpenAI dialect uses to request serial tool calls. Rides
/// `provider_extras` on the Anthropic egress (never a canonical
/// `ChatRequest` field) and is consumed here into the Anthropic-native
/// `disable_parallel_tool_use` toggle. See `apply_parallel_tool_use`.
pub(super) const PARALLEL_TOOL_CALLS_KEY: &str = "parallel_tool_calls";

/// Read the OpenAI-dialect `parallel_tool_calls` toggle (a JSON bool)
/// out of `provider_extras`. Returns `None` when the key is absent or
/// not a bool -- the two cases that must NOT drive the inversion.
pub(super) fn parallel_tool_calls_extra(extras: Option<&Value>) -> Option<bool> {
    extras?.as_object()?.get(PARALLEL_TOOL_CALLS_KEY)?.as_bool()
}

/// Fold the OpenAI-dialect `parallel_tool_calls` toggle into the
/// Anthropic-native `disable_parallel_tool_use` field on the translated
/// `tool_choice` object.
///
/// Only `parallel == Some(false)` is load-bearing: OpenAI clients that
/// disable parallel tool calls expect at most one tool call, and without
/// the inversion Anthropic may return several. It maps to
/// `disable_parallel_tool_use: true`. When no explicit `tool_choice`
/// survived translation but tools are on the wire, synthesize
/// `{"type":"auto","disable_parallel_tool_use":true}` so the toggle has a
/// carrier (Anthropic defaults tool selection to `auto`).
///
/// `Some(true)` and absent leave the object untouched: `true` is
/// Anthropic's own default (the field is omitted), and an absent toggle
/// must not clobber a native / round-trip `disable_parallel_tool_use`
/// already present on the object. The raw `parallel_tool_calls` key is
/// stripped from the Anthropic wire separately (managed-key path).
pub(super) fn apply_parallel_tool_use(
    provider_id: &str,
    tool_choice: Option<Value>,
    parallel: Option<bool>,
    has_wire_tools: bool,
) -> Option<Value> {
    if parallel != Some(false) {
        return tool_choice;
    }
    match tool_choice {
        Some(mut tc) => {
            if let Some(obj) = tc.as_object_mut() {
                obj.insert("disable_parallel_tool_use".into(), Value::Bool(true));
                tracing::debug!(
                    provider = provider_id,
                    "parallel_tool_calls=false -> disable_parallel_tool_use=true \
                     on Anthropic tool_choice"
                );
            }
            Some(tc)
        }
        None if has_wire_tools => {
            tracing::debug!(
                provider = provider_id,
                "parallel_tool_calls=false with tools and no explicit tool_choice \
                 -> synthesized auto tool_choice with disable_parallel_tool_use=true"
            );
            Some(json!({"type": "auto", "disable_parallel_tool_use": true}))
        }
        None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_parallel_tool_use, parallel_tool_calls_extra, translate_tool_choice};
    use serde_json::json;

    // --- bare tool-name string -> {type:tool, name} ---

    #[test]
    fn bare_tool_name_string_becomes_tool_object() {
        let tc = json!("get_weather");
        let out = translate_tool_choice(Some(&tc), true).unwrap();
        assert_eq!(out, json!({"type": "tool", "name": "get_weather"}));
    }

    #[test]
    fn bare_string_auto_required_none_unchanged() {
        assert_eq!(
            translate_tool_choice(Some(&json!("auto")), true).unwrap(),
            json!({"type": "auto"})
        );
        assert_eq!(
            translate_tool_choice(Some(&json!("required")), true).unwrap(),
            json!({"type": "any"})
        );
        // "none" with tools present drops the field entirely.
        assert!(translate_tool_choice(Some(&json!("none")), true).is_none());
    }

    #[test]
    fn nested_function_object_still_maps_to_tool() {
        let tc = json!({"type": "function", "function": {"name": "calc"}});
        let out = translate_tool_choice(Some(&tc), true).unwrap();
        assert_eq!(out, json!({"type": "tool", "name": "calc"}));
    }

    // --- parallel_tool_calls extraction ---

    #[test]
    fn parallel_extra_reads_bool() {
        assert_eq!(
            parallel_tool_calls_extra(Some(&json!({"parallel_tool_calls": false}))),
            Some(false)
        );
        assert_eq!(
            parallel_tool_calls_extra(Some(&json!({"parallel_tool_calls": true}))),
            Some(true)
        );
    }

    #[test]
    fn parallel_extra_none_when_absent_or_not_bool() {
        assert_eq!(parallel_tool_calls_extra(None), None);
        assert_eq!(parallel_tool_calls_extra(Some(&json!({"other": 1}))), None);
        assert_eq!(
            parallel_tool_calls_extra(Some(&json!({"parallel_tool_calls": "false"}))),
            None
        );
    }

    // --- apply_parallel_tool_use inversion ---

    #[test]
    fn parallel_false_sets_disable_on_existing_choice() {
        let tc = Some(json!({"type": "tool", "name": "calc"}));
        let out = apply_parallel_tool_use("p", tc, Some(false), true).unwrap();
        assert_eq!(out["type"], "tool");
        assert_eq!(out["name"], "calc");
        assert_eq!(out["disable_parallel_tool_use"], true);
    }

    #[test]
    fn parallel_false_synthesizes_auto_when_no_choice_but_tools() {
        let out = apply_parallel_tool_use("p", None, Some(false), true).unwrap();
        assert_eq!(
            out,
            json!({"type": "auto", "disable_parallel_tool_use": true})
        );
    }

    #[test]
    fn parallel_false_no_synthesis_without_tools() {
        assert!(apply_parallel_tool_use("p", None, Some(false), false).is_none());
    }

    #[test]
    fn parallel_true_omits_disable_field() {
        let tc = Some(json!({"type": "auto"}));
        let out = apply_parallel_tool_use("p", tc, Some(true), true).unwrap();
        assert!(out.get("disable_parallel_tool_use").is_none());
        assert_eq!(out, json!({"type": "auto"}));
    }

    #[test]
    fn parallel_absent_leaves_native_disable_untouched() {
        // An Anthropic round-trip carried disable_parallel_tool_use:false;
        // an absent toggle must not overwrite it.
        let tc = Some(json!({"type": "auto", "disable_parallel_tool_use": false}));
        let out = apply_parallel_tool_use("p", tc, None, true).unwrap();
        assert_eq!(out["disable_parallel_tool_use"], false);
    }
}
