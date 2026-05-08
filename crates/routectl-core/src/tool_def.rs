//! Typed tool definitions on the request.
//!
//! The hub stores a tool def in one of two shapes:
//!
//! - `ToolDef::Custom(CustomTool)` -- canonical Anthropic-shape custom
//!   tool with first-class `cache_control`, `defer_loading`, and
//!   `strict`. The OpenAI ingress translates `{type: "function",
//!   function: {...}}` into this variant at parse time (see
//!   `lift_openai_function_tools` in `crates/routectl-cli/src/
//!   ingress/openai.rs`, which uses `CustomTool::from_openai_function`)
//!   so all egresses see a single representation for the hot-path
//!   case. Library callers that bypass the ingress can rely on the
//!   `anthropic-api` egress's belt-and-braces translation of the same
//!   shape.
//! - `ToolDef::Other(Value)` -- forward-compat catchall. Anthropic
//!   built-in tools (`bash_*`, `code_execution_*`, `web_search_*`),
//!   server-side tools, and future shapes pass through verbatim. The
//!   Anthropic and Bedrock-Invoke egresses re-emit this Value as-is;
//!   OpenAI-compat egress drops with a `tracing::warn!` (or rejects
//!   under `strict_translation`).
//!
//! Discrimination on the wire: the `type` field decides. Absent or
//! `"custom"` -> `Custom`. Anything else -> `Other`. This avoids
//! `name`-based heuristics that would falsely absorb builtin tools
//! (which also carry `name`) into the typed variant.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

use crate::cache_control::CacheControl;

/// Tool definition variants. See module docs.
#[derive(Debug, Clone)]
pub enum ToolDef {
    Custom(CustomTool),
    Other(Value),
}

/// Anthropic-shape custom tool. `input_schema` defaults to an empty
/// object schema so a minimal `{name}` tool round-trips.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomTool {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default = "empty_object_schema")]
    pub input_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defer_loading: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strict: Option<bool>,
    /// Optional `type` discriminant. Anthropic accepts `"custom"` or
    /// absence; we round-trip whichever the wire used.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub type_tag: Option<String>,
}

fn empty_object_schema() -> Value {
    serde_json::json!({"type": "object", "properties": {}})
}

impl CustomTool {
    /// If `v` is the OpenAI tool wire shape (`{type: "function", function:
    /// {name, description?, parameters?, strict?}}`), translate it into a
    /// canonical `CustomTool`. Returns `None` for any other shape so the
    /// caller can fall through to `ToolDef::Other` for builtin / unknown
    /// tool types.
    ///
    /// Used by the OpenAI ingress (`crates/routectl-cli/src/ingress/
    /// openai.rs`) at parse time so all egresses see the canonical
    /// representation. Direct callers that bypass an ingress can rely on
    /// the `anthropic-api` egress's belt-and-braces translation of the
    /// same shape.
    pub fn from_openai_function(v: &Value) -> Option<CustomTool> {
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
            .unwrap_or_else(empty_object_schema);
        let strict = func.get("strict").and_then(|v| v.as_bool());
        Some(CustomTool {
            name,
            description,
            input_schema,
            cache_control: None,
            defer_loading: None,
            strict,
            type_tag: None,
        })
    }
}

impl ToolDef {
    /// Cache_control if the tool def carries one. The validator uses this
    /// to count breakpoints. Owned because for the `Other` variant the
    /// marker lives inside an arbitrary `Value` and is parsed on demand.
    pub fn cache_control(&self) -> Option<CacheControl> {
        match self {
            ToolDef::Custom(c) => c.cache_control.clone(),
            ToolDef::Other(v) => v
                .as_object()
                .and_then(|o| o.get("cache_control"))
                .and_then(|cc| serde_json::from_value::<CacheControl>(cc.clone()).ok()),
        }
    }
}

impl Serialize for ToolDef {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            ToolDef::Custom(c) => c.serialize(serializer),
            ToolDef::Other(v) => v.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for ToolDef {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let type_field = value
            .as_object()
            .and_then(|o| o.get("type"))
            .and_then(|t| t.as_str());
        match type_field {
            // Absent or "custom" -> typed Custom variant. We deserialize
            // from the same Value (rather than re-serializing) to keep
            // unknown fields silently ignored, matching today's behavior
            // for ChatRequest as a whole.
            None | Some("custom") => serde_json::from_value::<CustomTool>(value)
                .map(ToolDef::Custom)
                .map_err(serde::de::Error::custom),
            // Builtin or unknown discriminator -> opaque passthrough.
            Some(_) => Ok(ToolDef::Other(value)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn anthropic_custom_tool_round_trips() {
        let v = json!({
            "name": "calculator",
            "description": "do math",
            "input_schema": {"type": "object", "properties": {"a": {"type": "number"}}},
            "cache_control": {"type": "ephemeral", "ttl": "1h"}
        });
        let td: ToolDef = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(&td, ToolDef::Custom(_)));
        assert_eq!(td.cache_control().unwrap().effective_ttl(), "1h");
        assert_eq!(serde_json::to_value(&td).unwrap(), v);
    }

    #[test]
    fn explicit_type_custom_round_trips() {
        let v = json!({
            "type": "custom",
            "name": "calc",
            "input_schema": {"type": "object"}
        });
        let td: ToolDef = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(&td, ToolDef::Custom(_)));
        assert_eq!(serde_json::to_value(&td).unwrap(), v);
    }

    #[test]
    fn openai_function_tool_falls_to_other() {
        let v = json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get weather",
                "parameters": {"type": "object"}
            }
        });
        let td: ToolDef = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(&td, ToolDef::Other(_)));
        assert_eq!(serde_json::to_value(&td).unwrap(), v);
    }

    #[test]
    fn anthropic_builtin_tool_falls_to_other() {
        let v = json!({
            "type": "bash_20250124",
            "name": "bash"
        });
        let td: ToolDef = serde_json::from_value(v.clone()).unwrap();
        assert!(matches!(&td, ToolDef::Other(_)));
        assert_eq!(serde_json::to_value(&td).unwrap(), v);
    }

    #[test]
    fn minimal_custom_tool_uses_default_input_schema() {
        let v = json!({"name": "noop"});
        let td: ToolDef = serde_json::from_value(v).unwrap();
        if let ToolDef::Custom(c) = td {
            assert_eq!(c.name, "noop");
            assert_eq!(c.input_schema["type"], "object");
        } else {
            panic!("expected Custom variant");
        }
    }

    #[test]
    fn cache_control_extracts_from_other_variant() {
        let v = json!({
            "type": "web_search_20250901",
            "name": "search",
            "cache_control": {"type": "ephemeral"}
        });
        let td: ToolDef = serde_json::from_value(v).unwrap();
        assert!(td.cache_control().is_some());
    }
}
