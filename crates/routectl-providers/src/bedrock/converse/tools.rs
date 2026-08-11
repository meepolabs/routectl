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

use routectl_core::{
    ChatRequest, CustomTool, Result, ToolDef, cache_control::CacheControl, sanitize_for_log,
};

use crate::anthropic_api::request::translate_tool;
use crate::anthropic_api::types::AnthropicTool;

use super::types::{
    CachePoint, ConverseContentBlock, ConverseInputSchema, ConverseMessage, ConverseSpecificTool,
    ConverseToolChoice, ConverseToolDef, ConverseToolSpec, EmptyObject, ToolConfig,
};

/// Reserved name for the dummy tool injected to satisfy Bedrock's
/// tool-history validation. The double-underscore prefix keeps it clear
/// of caller-supplied tool names (a caller tool named exactly this would
/// be a deliberate collision, not an accident).
///
/// Visible across the `converse` module tree so the response lanes can
/// recognize it if the model ever selects it (`toolChoice` is left absent,
/// so Converse defaults to `auto` and selection is possible).
pub(super) const HISTORY_COMPAT_TOOL_NAME: &str = "routectl__history_compat_noop";

/// Translate `req.tools` + `req.tool_choice` into AWS `toolConfig`.
///
/// Returns Ok(None) when there's nothing to send (no tools, or
/// `tool_choice == "none"`); cache_point siblings are interleaved
/// adjacent to their owning tool spec.
///
/// `messages` is the already-translated Converse transcript. When it
/// carries a `toolResult` block but no usable tool defs survive, a single
/// reserved dummy `toolSpec` is injected: Bedrock rejects a request whose
/// transcript carries tool blocks unless `toolConfig` offers at least one
/// tool. The injection stops routectl from omitting a `toolConfig` the
/// wire requires; it does not promise the request then succeeds (an
/// unpaired `toolResult` can still be rejected for pairing reasons).
pub(super) fn build_tool_config(
    id: &str,
    req: &ChatRequest,
    messages: &[ConverseMessage],
) -> Result<Option<ToolConfig>> {
    // Mirror the Anthropic egress: `tool_choice == "none"` strips both
    // tools and tool_choice on the Converse wire too. Converse has no
    // native "none" mode, and shipping tools without tool_choice would
    // let AWS auto-select. Both bare-string `"none"` and the Anthropic-
    // object `{"type":"none"}` shapes must suppress -- AWS Converse
    // defaults to `auto` when `toolChoice` is absent but `tools` is
    // present, so emitting tools-without-toolChoice would let the
    // model call tools the caller forbade. A `"none"` caller explicitly
    // forbade tools, so the dummy backfill never runs under it.
    if is_tool_choice_none(req.tool_choice.as_ref()) {
        return Ok(None);
    }

    let tools: Vec<ConverseToolDef> = match req.tools.as_ref() {
        Some(canonical) => {
            let mut out = Vec::with_capacity(canonical.len());
            for td in canonical {
                append_tool_with_cache_point(id, td, &mut out);
            }
            out
        }
        None => Vec::new(),
    };

    if tools.is_empty() {
        // No usable tool defs survived (none supplied, an empty list, or
        // every entry was an Anthropic builtin that dropped). If the
        // translated transcript still carries a `toolResult`, backfill
        // exactly one reserved dummy so routectl stops omitting a
        // `toolConfig` the wire requires; otherwise absence is the
        // cleaner wire shape.
        if transcript_requires_tool_config(messages) {
            tracing::warn!(
                provider = id,
                "injecting reserved dummy toolSpec: Converse transcript carries a \
                 toolResult but the request offers no tools"
            );
            return Ok(Some(dummy_tool_config()));
        }
        return Ok(None);
    }
    let tool_choice = translate_tool_choice(id, req.tool_choice.as_ref());
    Ok(Some(ToolConfig { tools, tool_choice }))
}

/// True when the translated Converse transcript carries at least one
/// `toolResult` block, which is what makes AWS demand a `toolConfig`.
///
/// Deliberately asymmetric: a lone `toolUse` does NOT qualify. The two
/// rejections are different classes. A `toolResult` without `toolConfig`
/// trips a Converse-API-level missing-required-FIELD check, which
/// supplying a dummy tool repairs. A `toolUse` without its following
/// `toolResult` trips a model-level message-PAIRING check ("The model
/// returned the following errors: ... tool_use ids were found without
/// tool_result blocks"), which no `toolConfig` can repair -- injecting
/// there would be a model-visible mutation with no possible benefit.
/// If AWS ever merges those two validators, this predicate needs
/// revisiting.
fn transcript_requires_tool_config(messages: &[ConverseMessage]) -> bool {
    messages.iter().any(|msg| {
        msg.content
            .iter()
            .any(|block| matches!(block, ConverseContentBlock::ToolResult { .. }))
    })
}

/// The reserved dummy tool config: one `toolSpec` with a do-not-call
/// description and an empty-object input schema. `tool_choice` is left
/// absent so Converse defaults to `auto` -- the model MAY ignore the
/// dummy; a forcing choice would compel a nonsensical call.
fn dummy_tool_config() -> ToolConfig {
    ToolConfig {
        tools: vec![ConverseToolDef::Spec {
            tool_spec: ConverseToolSpec {
                name: HISTORY_COMPAT_TOOL_NAME.to_string(),
                description: Some("history compatibility only; do not call".to_string()),
                input_schema: ConverseInputSchema {
                    json: serde_json::json!({"type": "object", "properties": {}}),
                },
            },
        }],
        tool_choice: None,
    }
}

/// True when the caller's tool_choice means "do not call tools" --
/// either bare-string `"none"` (OpenAI) or the Anthropic-object form
/// `{"type":"none"}`. Both shapes must suppress the entire toolConfig
/// because Converse has no native "none" mode and ships its own
/// auto-default when `toolChoice` is missing but `tools` is present.
fn is_tool_choice_none(tc: Option<&Value>) -> bool {
    match tc {
        Some(Value::String(s)) => s == "none",
        Some(Value::Object(map)) => map.get("type").and_then(|v| v.as_str()) == Some("none"),
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
    // anthropic_api::tools::translate_custom_tool would only
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
        "required" => Some(ConverseToolChoice::Any {
            any: EmptyObject {},
        }),
        "none" => None, // handled at the build_tool_config level
        other => {
            tracing::warn!(
                provider = id,
                tool_choice = %sanitize_for_log(other),
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
        return Some(ConverseToolChoice::Any {
            any: EmptyObject {},
        });
    }
    let tool = map.get("tool").and_then(|v| v.as_object())?;
    let name = tool.get("name").and_then(|n| n.as_str()).unwrap_or("");
    if name.is_empty() {
        tracing::warn!(
            provider = id,
            shape_type = map
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown"),
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
        Some("any" | "required") => Some(ConverseToolChoice::Any {
            any: EmptyObject {},
        }),
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

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::CustomTool;
    use serde_json::json;
    use tracing_test::traced_test;

    use super::super::types::{ConverseMessage, ConverseToolResult, ConverseToolUse};

    const ID: &str = "bedrock:test-converse";

    fn tool_use_msg() -> ConverseMessage {
        ConverseMessage {
            role: "assistant".to_string(),
            content: vec![ConverseContentBlock::ToolUse {
                tool_use: ConverseToolUse {
                    tool_use_id: "tu_1".to_string(),
                    name: "calc".to_string(),
                    input: json!({"expr": "2+2"}),
                },
            }],
        }
    }

    fn tool_result_msg() -> ConverseMessage {
        ConverseMessage {
            role: "user".to_string(),
            content: vec![ConverseContentBlock::ToolResult {
                tool_result: ConverseToolResult {
                    tool_use_id: "tu_1".to_string(),
                    content: vec![],
                    status: None,
                },
            }],
        }
    }

    fn plain_msg() -> ConverseMessage {
        ConverseMessage {
            role: "user".to_string(),
            content: vec![ConverseContentBlock::Text {
                text: "hello".to_string(),
            }],
        }
    }

    fn wire_history() -> Vec<ConverseMessage> {
        vec![tool_use_msg(), tool_result_msg()]
    }

    fn req(tool_choice: Option<Value>) -> ChatRequest {
        ChatRequest {
            tool_choice,
            ..Default::default()
        }
    }

    fn custom_tool() -> ToolDef {
        ToolDef::Custom(CustomTool {
            name: "get_weather".to_string(),
            description: None,
            input_schema: json!({"type": "object"}),
            cache_control: None,
            defer_loading: None,
            strict: None,
            type_tag: None,
        })
    }

    #[test]
    fn injects_dummy_when_wire_history_but_no_tools() {
        // Arrange
        let request = req(None);
        let messages = wire_history();

        // Act
        let cfg = build_tool_config(ID, &request, &messages).unwrap().unwrap();

        // Assert: exactly one dummy toolSpec, auto/absent tool_choice.
        assert_eq!(cfg.tools.len(), 1);
        assert!(cfg.tool_choice.is_none(), "dummy must not force tool use");
        let ConverseToolDef::Spec { tool_spec } = &cfg.tools[0] else {
            panic!("expected a toolSpec entry");
        };
        assert_eq!(tool_spec.name, HISTORY_COMPAT_TOOL_NAME);
        assert_eq!(
            tool_spec.description.as_deref(),
            Some("history compatibility only; do not call")
        );
        assert_eq!(
            tool_spec.input_schema.json,
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn dummy_serializes_to_expected_wire_shape() {
        // Arrange
        let cfg = build_tool_config(ID, &req(None), &wire_history())
            .unwrap()
            .unwrap();

        // Act
        let v = serde_json::to_value(&cfg).unwrap();

        // Assert
        assert_eq!(
            v,
            json!({
                "tools": [{
                    "toolSpec": {
                        "name": HISTORY_COMPAT_TOOL_NAME,
                        "description": "history compatibility only; do not call",
                        "inputSchema": {"json": {"type": "object", "properties": {}}}
                    }
                }]
            })
        );
    }

    #[test]
    fn no_dummy_when_real_tools_present() {
        // Arrange: real tools plus wire history -- real tools win.
        let request = ChatRequest {
            tools: Some(vec![custom_tool()]),
            ..Default::default()
        };

        // Act
        let cfg = build_tool_config(ID, &request, &wire_history())
            .unwrap()
            .unwrap();

        // Assert
        assert_eq!(cfg.tools.len(), 1);
        let ConverseToolDef::Spec { tool_spec } = &cfg.tools[0] else {
            panic!("expected a toolSpec entry");
        };
        assert_eq!(tool_spec.name, "get_weather");
    }

    #[test]
    fn no_dummy_when_tool_choice_none_bare() {
        let cfg = build_tool_config(ID, &req(Some(json!("none"))), &wire_history()).unwrap();
        assert!(cfg.is_none(), "bare-string none suppresses the dummy");
    }

    #[test]
    fn no_dummy_when_tool_choice_none_object() {
        let cfg =
            build_tool_config(ID, &req(Some(json!({"type": "none"}))), &wire_history()).unwrap();
        assert!(cfg.is_none(), "object-shape none suppresses the dummy");
    }

    #[test]
    fn no_dummy_when_no_wire_history() {
        // Arrange: no tools, and a transcript with no tool blocks.
        let messages = vec![plain_msg()];

        // Act
        let cfg = build_tool_config(ID, &req(None), &messages).unwrap();

        // Assert
        assert!(cfg.is_none(), "no history means no false-positive dummy");
    }

    #[test]
    fn no_dummy_when_only_tool_use_present() {
        // Arrange: a lone toolUse with no matching toolResult must not fire
        // the model-visible backfill. That shape is rejected by a
        // model-level pairing check ("tool_use ids were found without
        // tool_result blocks"), which a dummy toolConfig cannot repair, so
        // injecting one would mutate the request for no benefit.
        let messages = vec![tool_use_msg(), plain_msg()];

        // Act
        let cfg = build_tool_config(ID, &req(None), &messages).unwrap();

        // Assert
        assert!(cfg.is_none());
    }

    #[test]
    fn injects_dummy_when_only_tool_result_present() {
        // Arrange: a lone toolResult is what makes AWS demand a toolConfig
        // (a Converse-level missing-required-field rejection), so routectl
        // must stop omitting one.
        let messages = vec![plain_msg(), tool_result_msg()];

        // Act
        let cfg = build_tool_config(ID, &req(None), &messages)
            .unwrap()
            .unwrap();

        // Assert
        assert_eq!(cfg.tools.len(), 1);
        assert!(cfg.tool_choice.is_none(), "dummy must not force tool use");
        let ConverseToolDef::Spec { tool_spec } = &cfg.tools[0] else {
            panic!("expected a toolSpec entry");
        };
        assert_eq!(tool_spec.name, HISTORY_COMPAT_TOOL_NAME);
    }

    #[test]
    fn no_dummy_when_tool_result_and_tool_choice_none() {
        // KNOWN UNREPAIRED SHAPE. A toolResult with `tool_choice: "none"`
        // still gets no toolConfig, so AWS still rejects it. Repairing it
        // would mean shipping a tool the caller explicitly forbade --
        // Converse has no native "none" mode, so a present `tools` array
        // defaults to auto-selection. Violating stated caller intent is
        // worse than the rejection.
        let messages = vec![plain_msg(), tool_result_msg()];

        for choice in [json!("none"), json!({"type": "none"})] {
            let cfg = build_tool_config(ID, &req(Some(choice)), &messages).unwrap();
            assert!(cfg.is_none(), "none must keep the shape unrepaired");
        }
    }

    #[traced_test]
    #[test]
    fn warns_on_dummy_injection() {
        // Act
        let _ = build_tool_config(ID, &req(None), &wire_history()).unwrap();

        // Assert: a WARN fires, carrying the provider id and no tool args.
        assert!(logs_contain("injecting reserved dummy toolSpec"));
    }

    #[traced_test]
    #[test]
    fn warns_on_dummy_injection_for_lone_tool_result() {
        // Act
        let messages = vec![plain_msg(), tool_result_msg()];
        let _ = build_tool_config(ID, &req(None), &messages).unwrap();

        // Assert: the newly-covered path is never a silent mutation.
        assert!(logs_contain("injecting reserved dummy toolSpec"));
    }
}
