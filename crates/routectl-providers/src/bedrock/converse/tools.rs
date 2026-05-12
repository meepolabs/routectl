//! Canonical `req.tools` + `req.tool_choice` -> Converse `toolConfig`
//! translation.
//!
//! Tool defs ride the `toolConfig.tools` array as a heterogeneous
//! union: each `CustomTool` produces a `{toolSpec}` block, optionally
//! followed by a `{cachePoint}` block when the canonical tool carries
//! a `cache_control` marker. tool_choice maps OpenAI / Anthropic
//! shapes onto the AWS `{auto:{}|any:{}|tool:{name}}` union; missing
//! or empty tool names drop the field entirely (AWS prefers no
//! tool_choice over an invalid one).

use serde_json::Value;

use routectl_core::{cache_control::CacheControl, ChatRequest, CustomTool, Result, ToolDef};

use crate::anthropic_api::request::translate_tool;
use crate::anthropic_api::types::AnthropicTool;

use super::types::{
    CachePoint, ConverseInputSchema, ConverseSpecificTool, ConverseToolChoice, ConverseToolDef,
    ConverseToolSpec, EmptyObject, ToolConfig,
};

/// Translate `req.tools` + `req.tool_choice` into AWS `toolConfig`.
/// Returns Ok(None) when there's nothing to send (no tools, or
/// `tool_choice == "none"`); cache_point siblings are interleaved
/// adjacent to their owning tool spec.
pub(super) fn build_tool_config(_id: &str, req: &ChatRequest) -> Result<Option<ToolConfig>> {
    // Mirror the Anthropic egress: `tool_choice == "none"` strips both
    // tools and tool_choice on the Converse wire too. Converse has no
    // native "none" mode, and shipping tools without tool_choice would
    // let AWS auto-select. Both bare-string `"none"` and the Anthropic-
    // object `{"type":"none"}` shapes must suppress -- AWS Converse
    // defaults to `auto` when `toolChoice` is absent but `tools` is
    // present, so emitting tools-without-toolChoice would let the
    // model call tools the caller forbade.
    let suppress_tools = is_tool_choice_none(req.tool_choice.as_ref());

    let canonical_tools = match (suppress_tools, req.tools.as_ref()) {
        (true, _) | (_, None) => return Ok(None),
        (false, Some(t)) if t.is_empty() => return Ok(None),
        (false, Some(t)) => t,
    };

    let mut tools: Vec<ConverseToolDef> = Vec::with_capacity(canonical_tools.len());
    for td in canonical_tools {
        append_tool_with_cache_point(_id, td, &mut tools);
    }
    if tools.is_empty() {
        // Every tool was an Anthropic builtin; absence is the cleaner
        // wire shape than `tools: []`.
        return Ok(None);
    }
    let tool_choice = translate_tool_choice(_id, req.tool_choice.as_ref());
    Ok(Some(ToolConfig { tools, tool_choice }))
}

/// True when the caller's tool_choice means "do not call tools" --
/// either bare-string `"none"` (OpenAI) or the Anthropic-object form
/// `{"type":"none"}`. Both shapes must suppress the entire toolConfig
/// because Converse has no native "none" mode and ships its own
/// auto-default when `toolChoice` is missing but `tools` is present.
fn is_tool_choice_none(tc: Option<&Value>) -> bool {
    match tc {
        Some(Value::String(s)) => s == "none",
        Some(Value::Object(map)) => {
            map.get("type").and_then(|v| v.as_str()) == Some("none")
        }
        _ => false,
    }
}

/// Append a translated tool spec, then optionally a sibling
/// `{cachePoint}` block. Per AWS docs, `toolConfig.tools` is a union of
/// `{toolSpec}` and `{cachePoint}` entries -- emitting two adjacent
/// items is the wire-correct way to mark a cached tool.
fn append_tool_with_cache_point(id: &str, td: &ToolDef, out: &mut Vec<ConverseToolDef>) {
    let (spec, cache_control) = match td {
        ToolDef::Custom(c) => (custom_tool_to_converse(c), c.cache_control.clone()),
        ToolDef::Other(_) => match translate_tool(td) {
            AnthropicTool::Custom {
                name,
                description,
                input_schema,
                cache_control,
                ..
            } => (
                ConverseToolDef::Spec {
                    tool_spec: ConverseToolSpec {
                        name,
                        description,
                        input_schema: ConverseInputSchema { json: input_schema },
                    },
                },
                cache_control,
            ),
            AnthropicTool::Builtin(_) => {
                tracing::warn!(
                    provider = id,
                    "dropping Anthropic-builtin tool on Converse egress; \
                     no equivalent shape available"
                );
                return;
            }
        },
    };
    out.push(spec);
    if let Some(cc) = cache_control {
        out.push(cache_point_tool_def(&cc));
    }
}

fn cache_point_tool_def(cc: &CacheControl) -> ConverseToolDef {
    ConverseToolDef::CachePoint {
        cache_point: CachePoint::default_with_ttl(Some(cc.effective_ttl().to_string())),
    }
}

fn custom_tool_to_converse(c: &CustomTool) -> ConverseToolDef {
    // The canonical CustomTool fields map 1:1 to ConverseToolSpec
    // without any per-shape transform. Routing through
    // anthropic_api::request::translate_custom_tool would only
    // round-trip through AnthropicTool::Custom and back; skip the
    // indirection.
    ConverseToolDef::Spec {
        tool_spec: ConverseToolSpec {
            name: c.name.clone(),
            description: c.description.clone(),
            input_schema: ConverseInputSchema {
                json: c.input_schema.clone(),
            },
        },
    }
}

/// Map canonical `tool_choice` Value into AWS's union shape. Accepts
/// bare-string OpenAI shapes ("auto" / "required") and Anthropic-shape
/// objects ({"type":"tool","name":"X"}, {"type":"auto"}, ...) so the
/// Converse egress works for both ingress dialects without translation
/// at the canonical level. Unknown shapes drop with a WARN (let the
/// upstream surface its own error rather than guessing).
fn translate_tool_choice(id: &str, tc: Option<&Value>) -> Option<ConverseToolChoice> {
    let tc = tc?;
    match tc {
        Value::String(s) => translate_tool_choice_string(id, s),
        Value::Object(map) => translate_tool_choice_object(id, map),
        _ => None,
    }
}

fn translate_tool_choice_string(id: &str, s: &str) -> Option<ConverseToolChoice> {
    match s {
        "auto" => Some(ConverseToolChoice::Auto {
            auto: EmptyObject {},
        }),
        "required" => Some(ConverseToolChoice::Any { any: EmptyObject {} }),
        "none" => None, // handled at the build_tool_config level
        other => {
            tracing::warn!(
                provider = id,
                tool_choice = %other,
                "unknown bare-string tool_choice; dropping on Converse egress"
            );
            None
        }
    }
}

fn translate_tool_choice_object(
    id: &str,
    map: &serde_json::Map<String, Value>,
) -> Option<ConverseToolChoice> {
    // Converse-shape passthrough first: {"auto":{}} | {"any":{}} |
    // {"tool":{"name":"X"}} -- detect via top-level keys.
    if let Some(c) = passthrough_converse_tool_choice(id, map) {
        return Some(c);
    }
    // Fall through to Anthropic / OpenAI shapes that need translation.
    translate_typed_tool_choice(id, map)
}

fn passthrough_converse_tool_choice(
    id: &str,
    map: &serde_json::Map<String, Value>,
) -> Option<ConverseToolChoice> {
    if map.contains_key("auto") {
        return Some(ConverseToolChoice::Auto {
            auto: EmptyObject {},
        });
    }
    if map.contains_key("any") {
        return Some(ConverseToolChoice::Any { any: EmptyObject {} });
    }
    let tool = map.get("tool").and_then(|v| v.as_object())?;
    let name = tool.get("name").and_then(|n| n.as_str()).unwrap_or("");
    if name.is_empty() {
        tracing::warn!(
            provider = id,
            shape_type = map.get("type").and_then(|v| v.as_str()).unwrap_or("unknown"),
            "tool_choice missing or invalid name; dropping field"
        );
        return None;
    }
    Some(ConverseToolChoice::Tool {
        tool: ConverseSpecificTool {
            name: name.to_string(),
        },
    })
}

/// Anthropic-shape: {"type":"auto"|"any"|"tool","name"?}.
/// OpenAI-shape: {"type":"function","function":{"name"}}.
fn translate_typed_tool_choice(
    id: &str,
    map: &serde_json::Map<String, Value>,
) -> Option<ConverseToolChoice> {
    match map.get("type").and_then(|v| v.as_str()) {
        Some("auto") => Some(ConverseToolChoice::Auto {
            auto: EmptyObject {},
        }),
        Some("any") | Some("required") => {
            Some(ConverseToolChoice::Any { any: EmptyObject {} })
        }
        Some("tool") => {
            let name = map.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() {
                tracing::warn!(
                    provider = id,
                    shape_type = "tool",
                    "tool_choice missing or invalid name; dropping field"
                );
                return None;
            }
            Some(ConverseToolChoice::Tool {
                tool: ConverseSpecificTool {
                    name: name.to_string(),
                },
            })
        }
        Some("function") => {
            let name = map
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if name.is_empty() {
                tracing::warn!(
                    provider = id,
                    shape_type = "function",
                    "tool_choice missing or invalid name; dropping field"
                );
                return None;
            }
            Some(ConverseToolChoice::Tool {
                tool: ConverseSpecificTool {
                    name: name.to_string(),
                },
            })
        }
        _ => {
            tracing::warn!(
                provider = id,
                "unknown tool_choice object shape; dropping on Converse egress"
            );
            None
        }
    }
}

/// Expose tool-level cache_control so the orchestrator's breakpoint
/// validator can include them in the prefix order. Iterates
/// `req.tools` and yields `(position-relative-index, control)` for any
/// tool that carries a marker.
pub(super) fn collect_tool_cache_controls(req: &ChatRequest) -> Vec<CacheControl> {
    let Some(tools) = req.tools.as_ref() else {
        return Vec::new();
    };
    let mut out: Vec<CacheControl> = Vec::new();
    for td in tools {
        let cc = match td {
            ToolDef::Custom(c) => c.cache_control.clone(),
            ToolDef::Other(_) => match translate_tool(td) {
                AnthropicTool::Custom { cache_control, .. } => cache_control,
                AnthropicTool::Builtin(_) => None,
            },
        };
        if let Some(c) = cc {
            out.push(c);
        }
    }
    out
}
