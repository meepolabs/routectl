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
    // TRANSLATION-DROP: structural -- a "none" tool_choice is the caller forbidding tool use, so omitting toolConfig delivers their instruction rather than losing it
    if is_tool_choice_none(req.tool_choice.as_ref()) {
        return Ok(None);
    }

    let tools: Vec<ConverseToolDef> = match req.tools.as_ref() {
        Some(canonical) => {
            let mut out = Vec::with_capacity(canonical.len());
            let mut builtin_dropped = false;
            for td in canonical {
                append_tool_with_cache_point(id, td, &mut out, &mut builtin_dropped);
            }
            if builtin_dropped {
                // Once per REQUEST, not once per dropped tool: a request
                // offering three builtins is one drop event against this
                // lane's request-volume denominator.
                crate::translation_drop_metrics::record_translation_drop(
                    "bedrock-converse",
                    "builtin_tool_unrepresentable",
                );
            }
            out
        }
        None => Vec::new(),
    };

    // Translated BEFORE the no-tools early returns, not after: a request whose
    // tool_choice is unrepresentable loses it whether or not a tool def survived,
    // and the population that reaches those returns is precisely the
    // malformed-tool_choice-without-tools case. Counting only the with-tools half
    // made drop_rate() read low for the requests most likely to be broken.
    let tool_choice = translate_tool_choice(id, req.tool_choice.as_ref());

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
        // TRANSLATION-DROP: structural -- no tool def reached this point, so an absent toolConfig is the accurate wire shape; each tool that failed to translate was accounted for at its own arm, and the tool_choice was translated and tallied above this branch
        return Ok(None);
    }
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
///
/// `builtin_dropped` is set when an Anthropic-builtin tool is discarded, so
/// the caller can bump the drop counter once for the whole request rather
/// than once per dropped tool.
fn append_tool_with_cache_point(
    id: &str,
    td: &ToolDef,
    out: &mut Vec<ConverseToolDef>,
    builtin_dropped: &mut bool,
) {
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
            // An Anthropic server-side builtin (web_search, computer_use,
            // ...) is a tool the PROVIDER implements, named by tag with no
            // caller-supplied schema. Converse's `toolConfig.tools` union
            // models only `{toolSpec}` and `{cachePoint}`, so there is no
            // member to translate a builtin onto -- and synthesizing a
            // `toolSpec` for it would offer the model a tool nothing can
            // execute. Lane: bedrock-converse, construction-time
            // translation, cross-dialect by construction. Baked seed
            // verdict per foundations sec 14: deletion stays blocked until
            // this lane's own wire evidence contradicts it.
            // TRANSLATION-DROP: lane=bedrock-converse class=builtin_tool_unrepresentable test=anthropic_builtin_tool_drops_and_bumps_the_drop_counter_once
            AnthropicTool::Builtin(_) => {
                *builtin_dropped = true;
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

/// Per-REQUEST tally for the tool_choice family's drops on this lane.
///
/// Each field is a per-request FLAG, not an occurrence count: a request
/// carries exactly one `tool_choice`, and the two classes are the two
/// distinct operator problems a malformed one presents. The per-arm WARN
/// already carries the offending shape; the counters flushed here answer
/// "how often does this lane lose a caller's tool_choice" against the
/// lane's request-volume denominator.
///
/// The denominator itself is NOT touched here: the Converse egress owns
/// exactly one `record_translation_lane_seen` site, and a second would
/// understate the rate for the whole lane.
#[derive(Default)]
#[must_use = "a tally records nothing until flush() runs"]
struct ToolChoiceDropTally {
    shape_unrepresentable: bool,
    name_missing: bool,
}

impl ToolChoiceDropTally {
    /// Record that the caller's tool_choice named a mode or shape the
    /// Converse `toolChoice` union has no member for.
    const fn record_shape_unrepresentable(&mut self) {
        self.shape_unrepresentable = true;
    }

    /// Record that the caller asked for a specific tool but supplied no
    /// usable name for it.
    const fn record_name_missing(&mut self) {
        self.name_missing = true;
    }

    fn flush(self) {
        if self.shape_unrepresentable {
            crate::translation_drop_metrics::record_translation_drop(
                "bedrock-converse",
                "tool_choice_shape_unrepresentable",
            );
        }
        if self.name_missing {
            crate::translation_drop_metrics::record_translation_drop(
                "bedrock-converse",
                "tool_choice_name_missing",
            );
        }
    }
}

/// Map canonical `tool_choice` Value into AWS's union shape. Accepts
/// bare-string OpenAI shapes ("auto" / "required") and Anthropic-shape
/// objects ({"type":"tool","name":"X"}, {"type":"auto"}, ...) so the
/// Converse egress works for both ingress dialects without translation
/// at the canonical level. Unknown shapes drop with a WARN (let the
/// upstream surface its own error rather than guessing).
///
/// Owns the tally's whole lifetime: this is the one function every
/// request's tool_choice passes through exactly once, and nothing on the
/// path below it is fallible, so the flush cannot be skipped by an early
/// `?` the way a flush placed further out could be.
///
/// That "exactly once" holds only because the caller invokes this ABOVE its
/// no-tools early returns. It previously ran below them, so a malformed
/// tool_choice on a request offering no usable tools was lost with no log and
/// no count -- the half of the population most likely to be malformed.
fn translate_tool_choice(id: &str, tc: Option<&Value>) -> Option<ConverseToolChoice> {
    let mut tally = ToolChoiceDropTally::default();
    let out = translate_tool_choice_tallied(id, tc, &mut tally);
    tally.flush();
    out
}

fn translate_tool_choice_tallied(
    id: &str,
    tc: Option<&Value>,
    tally: &mut ToolChoiceDropTally,
) -> Option<ConverseToolChoice> {
    let tc = tc?;
    match tc {
        Value::String(s) => translate_tool_choice_string(id, s, tally),
        Value::Object(map) => translate_tool_choice_object(id, map, tally),
        _ => None,
    }
}

fn translate_tool_choice_string(
    id: &str,
    s: &str,
    tally: &mut ToolChoiceDropTally,
) -> Option<ConverseToolChoice> {
    match s {
        "auto" => Some(ConverseToolChoice::Auto {
            auto: EmptyObject {},
        }),
        "required" => Some(ConverseToolChoice::Any {
            any: EmptyObject {},
        }),
        "none" => None, // handled at the build_tool_config level
        // A bare-string mode token outside `auto` / `required` / `none` (a
        // future OpenAI spelling, or a client typo). The Converse
        // `toolChoice` union carries no free-form mode member, so there is
        // no shape to emit; guessing at the nearest mode would let the model
        // call tools the caller may have meant to forbid. Lane:
        // bedrock-converse, construction-time translation, cross-dialect by
        // construction. Baked seed verdict: it stands until this lane's own
        // wire evidence contradicts it, and is not eligible for deletion
        // until then.
        // TRANSLATION-DROP: lane=bedrock-converse class=tool_choice_shape_unrepresentable test=converse_unknown_bare_string_tool_choice_drops_and_counts_once
        other => {
            tally.record_shape_unrepresentable();
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
    tally: &mut ToolChoiceDropTally,
) -> Option<ConverseToolChoice> {
    // Converse-shape passthrough first: {"auto":{}} | {"any":{}} |
    // {"tool":{"name":"X"}} -- detect via top-level keys.
    match passthrough_converse_tool_choice(id, map, tally) {
        ConversePassthrough::Translated(c) => Some(c),
        // The object IS a Converse-shape `{"tool":{...}}` entry whose name is
        // unusable, which the passthrough has already reported. Terminal
        // rather than falling through, because the typed translation below
        // would read the same object as an unknown OBJECT SHAPE and report a
        // second, different loss for the one lost tool_choice.
        ConversePassthrough::NameMissing => None,
        ConversePassthrough::NotConverseShape => translate_typed_tool_choice(id, map, tally),
    }
}

/// What the Converse-shape passthrough made of the caller's object. Three
/// outcomes rather than an `Option`, because "this is a Converse `tool`
/// entry I cannot use" and "this is not a Converse shape at all" lead to
/// different places: only the latter may fall through to the typed
/// translation.
enum ConversePassthrough {
    Translated(ConverseToolChoice),
    NameMissing,
    NotConverseShape,
}

fn passthrough_converse_tool_choice(
    id: &str,
    map: &serde_json::Map<String, Value>,
    tally: &mut ToolChoiceDropTally,
) -> ConversePassthrough {
    if map.contains_key("auto") {
        return ConversePassthrough::Translated(ConverseToolChoice::Auto {
            auto: EmptyObject {},
        });
    }
    if map.contains_key("any") {
        return ConversePassthrough::Translated(ConverseToolChoice::Any {
            any: EmptyObject {},
        });
    }
    let Some(tool) = map.get("tool").and_then(|v| v.as_object()) else {
        return ConversePassthrough::NotConverseShape;
    };
    let name = tool.get("name").and_then(|n| n.as_str()).unwrap_or("");
    // A Converse-shape `{"tool":{...}}` entry whose name is absent, empty, or
    // not a string. `toolChoice.tool` requires the name, and AWS rejects the
    // member without it -- while substituting `{any:{}}` would force a
    // DIFFERENT tool than the one the caller named. Lane: bedrock-converse,
    // construction-time translation. Baked seed verdict: it stands until this
    // lane's own wire evidence contradicts it, and is not eligible for
    // deletion until then.
    // TRANSLATION-DROP: lane=bedrock-converse class=tool_choice_name_missing test=converse_passthrough_tool_choice_without_a_name_drops_and_counts_once
    if name.is_empty() {
        // AMBIGUOUS OBJECT: a `{"tool":{...}}` with no usable name that ALSO
        // carries a typed spelling (`{"tool":{}, "type":"tool", "name":"calc"}`)
        // is not a Converse-shape entry this can decide. Before the tally
        // existed, such an object fell through to the typed translation and
        // emitted the caller's named tool; making the name-missing case terminal
        // regressed that to dropping the tool_choice entirely. So defer, and let
        // the typed path own it -- recording nothing here, since nothing is lost
        // when the fall-through succeeds.
        if map.contains_key("type") {
            return ConversePassthrough::NotConverseShape;
        }
        tally.record_name_missing();
        tracing::warn!(
            provider = id,
            shape_type = map
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown"),
            "tool_choice missing or invalid name; dropping field"
        );
        return ConversePassthrough::NameMissing;
    }
    ConversePassthrough::Translated(ConverseToolChoice::Tool {
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
    tally: &mut ToolChoiceDropTally,
) -> Option<ConverseToolChoice> {
    match map.get("type").and_then(|v| v.as_str()) {
        Some("auto") => Some(ConverseToolChoice::Auto {
            auto: EmptyObject {},
        }),
        Some("any" | "required") => Some(ConverseToolChoice::Any {
            any: EmptyObject {},
        }),
        // Same loss as the passthrough's unusable-name arm, reached through
        // the Anthropic spelling: `{"type":"tool"}` with no usable `name`.
        // TRANSLATION-DROP: lane=bedrock-converse class=tool_choice_name_missing test=converse_anthropic_tool_choice_without_a_name_drops_and_counts_once
        Some("tool") => {
            let name = map.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if name.is_empty() {
                tally.record_name_missing();
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
        // Same loss again, reached through the OpenAI spelling:
        // `{"type":"function"}` with no usable `function.name`.
        // TRANSLATION-DROP: lane=bedrock-converse class=tool_choice_name_missing test=converse_openai_tool_choice_without_a_name_drops_and_counts_once
        Some("function") => {
            let name = map
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if name.is_empty() {
                tally.record_name_missing();
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
        // An object matching neither the Converse union nor a typed mode
        // spelling this egress reads. The Converse `toolChoice` union carries
        // only `{auto:{}} | {any:{}} | {tool:{name}}`, so there is no member
        // an unreadable object could become, and guessing at one would let
        // the model call tools on a request whose intent nobody can read.
        // Lane: bedrock-converse, construction-time translation,
        // cross-dialect by construction. Baked seed verdict: it stands until
        // this lane's own wire evidence contradicts it, and is not eligible
        // for deletion until then.
        // TRANSLATION-DROP: lane=bedrock-converse class=tool_choice_shape_unrepresentable test=converse_unknown_tool_choice_object_shape_drops_and_counts_once
        _ => {
            tally.record_shape_unrepresentable();
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

    // -----------------------------------------------------------------
    // The Anthropic-builtin tool drop and its per-request
    // `(bedrock-converse, builtin_tool_unrepresentable)` counter. Log
    // capture here uses `routectl_testkit::capture_events` rather than
    // `logs_contain`: the structured `provider` field is part of what the
    // drop reports, and a substring match on rendered output cannot see it.
    // Serialized on the drop_class's own guard because the counter registry
    // is process-global and this crate's runner is threaded.
    // -----------------------------------------------------------------

    fn builtin_tool() -> ToolDef {
        ToolDef::Other(json!({
            "type": "web_search_20250901",
            "name": "sentinel_builtin_tool",
        }))
    }

    fn builtin_drop_count() -> u64 {
        crate::translation_drop_metrics::translation_drop_snapshot()
            .into_iter()
            .find(|e| {
                e.lane == "bedrock-converse" && e.drop_class == "builtin_tool_unrepresentable"
            })
            .map_or(0, |e| e.drop_count)
    }

    /// The emitted `toolConfig` wire value, so the absence assertion runs
    /// against the serialized body rather than the typed vec.
    fn emitted_tool_config(request: &ChatRequest) -> Value {
        let cfg = build_tool_config(ID, request, &[]).expect("translation ok");
        serde_json::to_value(&cfg).expect("toolConfig must serialize")
    }

    /// NEGATIVE CONTROL. A builtin has no `toolSpec` shape to translate onto,
    /// so it drops; the counter advances once and the representable sibling
    /// still ships.
    #[test]
    #[serial_test::serial(bedrock_converse_builtin_tool_unrepresentable)]
    fn anthropic_builtin_tool_drops_and_bumps_the_drop_counter_once() {
        // Arrange
        let before = builtin_drop_count();
        let request = ChatRequest {
            tools: Some(vec![builtin_tool(), custom_tool()]),
            ..Default::default()
        };

        // Act
        let mut wire = Value::Null;
        let events = routectl_testkit::capture_events(|| {
            wire = emitted_tool_config(&request);
        });
        let after = builtin_drop_count();

        // Assert 1 -- the WARN fired, carrying the provider id as a field.
        let warn = events
            .iter()
            .find(|e| {
                e.level == tracing::Level::WARN
                    && e.message
                        .contains("dropping Anthropic-builtin tool on Converse egress")
            })
            .unwrap_or_else(|| panic!("the builtin drop must warn; got: {events:?}"));
        assert_eq!(warn.field("provider"), Some(ID));

        // Assert 2 -- the builtin's name is absent from the EMITTED WIRE
        // VALUE, not merely from the typed tool vec.
        assert!(
            !wire.to_string().contains("sentinel_builtin_tool"),
            "the builtin must not reach the upstream in any form; emitted toolConfig: {wire}"
        );

        // Assert 3 -- positive control: the representable custom tool
        // survived in that same emitted value.
        assert!(
            wire.to_string().contains("get_weather"),
            "the representable sibling tool must survive; emitted toolConfig: {wire}"
        );

        assert_eq!(
            after - before,
            1,
            "the builtin-drop counter must advance by exactly one for this request"
        );
    }

    /// Two builtins in ONE request is one drop EVENT, not two -- the counter
    /// is bumped once per `build_tool_config` call, which is the placement
    /// this assertion pins.
    #[test]
    #[serial_test::serial(bedrock_converse_builtin_tool_unrepresentable)]
    fn two_builtin_tools_in_one_request_bump_the_drop_counter_once() {
        // Arrange
        let before = builtin_drop_count();
        let request = ChatRequest {
            tools: Some(vec![
                builtin_tool(),
                ToolDef::Other(json!({"type": "computer_20250124", "name": "computer"})),
                custom_tool(),
            ]),
            ..Default::default()
        };

        // Act
        let _ = build_tool_config(ID, &request, &[]).expect("translation ok");
        let after = builtin_drop_count();

        // Assert
        assert_eq!(
            after - before,
            1,
            "two dropped builtins in one request is one drop event, not two"
        );
    }

    /// POSITIVE CONTROL: a request offering only representable custom tools
    /// warns not at all and advances no counter, proving the fixture above
    /// would have surfaced a WARN not actually tied to the builtin.
    #[test]
    #[serial_test::serial(bedrock_converse_builtin_tool_unrepresentable)]
    fn custom_tools_only_advance_no_builtin_drop_counter() {
        // Arrange
        let before = builtin_drop_count();
        let request = ChatRequest {
            tools: Some(vec![custom_tool()]),
            ..Default::default()
        };

        // Act
        let mut wire = Value::Null;
        let events = routectl_testkit::capture_events(|| {
            wire = emitted_tool_config(&request);
        });
        let after = builtin_drop_count();

        // Assert
        assert!(
            wire.to_string().contains("get_weather"),
            "a representable tool must reach the upstream; emitted toolConfig: {wire}"
        );
        assert!(
            !events
                .iter()
                .any(|e| e.level == tracing::Level::WARN && e.message.contains("dropping")),
            "nothing was unrepresentable, so no drop WARN is owed; got: {events:?}"
        );
        assert_eq!(
            after, before,
            "a request with nothing dropped must not advance the counter"
        );
    }

    // -----------------------------------------------------------------
    // The tool_choice family's two drop classes and their per-REQUEST
    // counters. Each fixture drives the whole `build_tool_config` entry
    // point rather than the private arm, because the per-request placement
    // of the flush is part of what is pinned: a counter bumped inside the
    // arm would pass an arm-level test and still be wrong.
    // -----------------------------------------------------------------

    fn tool_choice_drop_count(class: &str) -> u64 {
        crate::translation_drop_metrics::translation_drop_snapshot()
            .into_iter()
            .find(|e| e.lane == "bedrock-converse" && e.drop_class == class)
            .map_or(0, |e| e.drop_count)
    }

    fn shape_drop_count() -> u64 {
        tool_choice_drop_count("tool_choice_shape_unrepresentable")
    }

    fn name_drop_count() -> u64 {
        tool_choice_drop_count("tool_choice_name_missing")
    }

    /// One request offering a representable tool plus the given tool_choice.
    /// Returns the EMITTED WIRE VALUE of the whole `toolConfig` plus every
    /// captured event, so an absence assertion runs against the serialized
    /// body rather than the typed struct.
    fn emitted_with_tool_choice(choice: Value) -> (Value, Vec<routectl_testkit::CapturedEvent>) {
        let request = ChatRequest {
            tools: Some(vec![custom_tool()]),
            tool_choice: Some(choice),
            ..Default::default()
        };
        let mut wire = Value::Null;
        let events = routectl_testkit::capture_events(|| {
            wire = emitted_tool_config(&request);
        });
        (wire, events)
    }

    /// NEGATIVE CONTROL: a bare-string mode token the Converse `toolChoice`
    /// union has no member for drops, warns with the sanitized token, and
    /// counts once. Before this the arm warned and counted nothing.
    #[test]
    #[serial_test::serial(bedrock_converse_tool_choice_shape_unrepresentable)]
    fn converse_unknown_bare_string_tool_choice_drops_and_counts_once() {
        // Arrange
        let before = shape_drop_count();

        // Act
        let (wire, events) = emitted_with_tool_choice(json!("marker_future_mode"));
        let after = shape_drop_count();

        // Assert 1 -- the WARN fired, carrying the provider and the token.
        let warn = events
            .iter()
            .find(|e| {
                e.level == tracing::Level::WARN
                    && e.message.contains("unknown bare-string tool_choice")
            })
            .unwrap_or_else(|| panic!("the drop must warn; got: {events:?}"));
        assert_eq!(warn.field("provider"), Some(ID));

        // Assert 2 -- no toolChoice on the emitted wire value, and no trace
        // of the unusable token riding along.
        assert!(
            !wire.to_string().contains("toolChoice")
                && !wire.to_string().contains("marker_future_mode"),
            "the unusable tool_choice must not reach the upstream; emitted: {wire}"
        );

        // Assert 3 -- positive control: the tools the request offered survived.
        assert!(
            wire.to_string().contains("get_weather"),
            "the request's tools must still ship; emitted: {wire}"
        );

        assert_eq!(
            after - before,
            1,
            "the drop must be counted exactly once for the request"
        );
    }

    /// NEGATIVE CONTROL: an object matching neither the Converse union nor a
    /// typed mode spelling drops and counts on the same class -- one lost
    /// tool_choice is one drop event whichever spelling produced it.
    #[test]
    #[serial_test::serial(bedrock_converse_tool_choice_shape_unrepresentable)]
    fn converse_unknown_tool_choice_object_shape_drops_and_counts_once() {
        // Arrange
        let before = shape_drop_count();

        // Act
        let (wire, events) = emitted_with_tool_choice(json!({"marker_unknown_key": "x"}));
        let after = shape_drop_count();

        // Assert
        assert!(
            events.iter().any(|e| {
                e.level == tracing::Level::WARN
                    && e.message.contains("unknown tool_choice object shape")
            }),
            "the drop must warn; got: {events:?}"
        );
        assert!(
            !wire.to_string().contains("toolChoice"),
            "no toolChoice may be invented for an unreadable object; emitted: {wire}"
        );
        assert!(
            wire.to_string().contains("get_weather"),
            "the request's tools must still ship; emitted: {wire}"
        );
        assert_eq!(after - before, 1);
    }

    /// NEGATIVE CONTROL: the Converse-shape `{"tool":{...}}` passthrough with
    /// no usable name. Counts on the name class, and -- because the object IS
    /// a Converse `tool` entry -- must NOT also be reported as an unreadable
    /// object shape: one lost tool_choice is one drop event, not two on two
    /// classes.
    #[test]
    #[serial_test::serial(
        bedrock_converse_tool_choice_name_missing,
        bedrock_converse_tool_choice_shape_unrepresentable
    )]
    fn converse_passthrough_tool_choice_without_a_name_drops_and_counts_once() {
        // Arrange
        let before_name = name_drop_count();
        let before_shape = shape_drop_count();

        // Act
        let (wire, events) = emitted_with_tool_choice(json!({"tool": {"name": ""}}));
        let after_name = name_drop_count();
        let after_shape = shape_drop_count();

        // Assert 1 -- exactly one WARN, the name one.
        assert_eq!(
            events
                .iter()
                .filter(|e| e.level == tracing::Level::WARN)
                .count(),
            1,
            "one lost tool_choice owes exactly one WARN; got: {events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| e.message.contains("tool_choice missing or invalid name")),
            "the drop must warn about the name; got: {events:?}"
        );

        // Assert 2 -- nothing forced on the emitted wire value. Substituting
        // `{any:{}}` here would force a DIFFERENT tool than the caller named.
        assert!(
            !wire.to_string().contains("toolChoice"),
            "an unnamed forcing choice must not become a forcing one; emitted: {wire}"
        );

        // Assert 3 -- the name class advanced once, and the shape class not
        // at all: the object was recognized as a Converse `tool` entry.
        assert_eq!(after_name - before_name, 1);
        assert_eq!(
            after_shape, before_shape,
            "a recognized Converse tool entry must not also count as an unreadable shape"
        );
    }

    /// NEGATIVE CONTROL: the Anthropic spelling of the same loss.
    #[test]
    #[serial_test::serial(bedrock_converse_tool_choice_name_missing)]
    fn converse_anthropic_tool_choice_without_a_name_drops_and_counts_once() {
        // Arrange
        let before = name_drop_count();

        // Act
        let (wire, events) = emitted_with_tool_choice(json!({"type": "tool"}));
        let after = name_drop_count();

        // Assert
        let warn = events
            .iter()
            .find(|e| {
                e.level == tracing::Level::WARN
                    && e.message.contains("tool_choice missing or invalid name")
            })
            .unwrap_or_else(|| panic!("the drop must warn; got: {events:?}"));
        assert_eq!(warn.field("shape_type"), Some("tool"));
        assert!(!wire.to_string().contains("toolChoice"), "emitted: {wire}");
        assert_eq!(after - before, 1);
    }

    /// NEGATIVE CONTROL: the OpenAI spelling of the same loss.
    #[test]
    #[serial_test::serial(bedrock_converse_tool_choice_name_missing)]
    fn converse_openai_tool_choice_without_a_name_drops_and_counts_once() {
        // Arrange
        let before = name_drop_count();

        // Act
        let (wire, events) = emitted_with_tool_choice(json!({"type": "function", "function": {}}));
        let after = name_drop_count();

        // Assert
        let warn = events
            .iter()
            .find(|e| {
                e.level == tracing::Level::WARN
                    && e.message.contains("tool_choice missing or invalid name")
            })
            .unwrap_or_else(|| panic!("the drop must warn; got: {events:?}"));
        assert_eq!(warn.field("shape_type"), Some("function"));
        assert!(!wire.to_string().contains("toolChoice"), "emitted: {wire}");
        assert_eq!(after - before, 1);
    }

    /// POSITIVE CONTROL for every fixture above: each representable
    /// tool_choice spelling reaches the emitted wire value, warns not at all,
    /// and advances NEITHER counter. Without it the absence assertions above
    /// would pass against an egress that dropped every tool_choice.
    #[test]
    #[serial_test::serial(
        bedrock_converse_tool_choice_name_missing,
        bedrock_converse_tool_choice_shape_unrepresentable
    )]
    fn representable_tool_choices_survive_and_advance_no_counter() {
        for (choice, expected_member) in [
            (json!("auto"), "auto"),
            (json!("required"), "any"),
            (json!({"auto": {}}), "auto"),
            (json!({"any": {}}), "any"),
            (json!({"tool": {"name": "get_weather"}}), "tool"),
            (json!({"type": "auto"}), "auto"),
            (json!({"type": "any"}), "any"),
            (json!({"type": "tool", "name": "get_weather"}), "tool"),
            (
                json!({"type": "function", "function": {"name": "get_weather"}}),
                "tool",
            ),
        ] {
            // Arrange
            let before_name = name_drop_count();
            let before_shape = shape_drop_count();

            // Act
            let (wire, events) = emitted_with_tool_choice(choice.clone());

            // Assert
            assert!(
                !events.iter().any(|e| e.level == tracing::Level::WARN),
                "{choice} is representable and must not warn; got: {events:?}"
            );
            assert_eq!(
                wire["toolChoice"]
                    .as_object()
                    .and_then(|o| o.keys().next().map(String::as_str)),
                Some(expected_member),
                "{choice} must reach the wire as the {expected_member} member; emitted: {wire}"
            );
            assert_eq!(name_drop_count(), before_name, "{choice} counted a drop");
            assert_eq!(shape_drop_count(), before_shape, "{choice} counted a drop");
        }
    }
    #[test]
    #[serial_test::serial(bedrock_converse_tool_choice_shape_unrepresentable)]
    fn an_unrepresentable_tool_choice_counts_even_when_no_tool_def_survives() {
        // The population that reaches build_tool_config's no-tools early returns
        // is precisely the malformed-tool_choice-without-tools case, so counting
        // only the with-tools half made drop_rate() read LOW for the requests most
        // likely to be broken. Measured before the fix: delta 0 without tools, 1
        // with them, for the same tool_choice.
        fn shape_count() -> u64 {
            crate::translation_drop_metrics::translation_drop_snapshot()
                .into_iter()
                .find(|e| {
                    e.lane == "bedrock-converse"
                        && e.drop_class == "tool_choice_shape_unrepresentable"
                })
                .map_or(0, |e| e.drop_count)
        }

        // Arrange: a tool_choice no Converse member can carry, and NO tool defs.
        let request = req(Some(json!("marker_future_mode")));
        let before = shape_count();

        // Act
        let out = build_tool_config(ID, &request, &[]).expect("builds");

        // Assert: absent toolConfig is still the right wire shape, and the lost
        // tool_choice is counted exactly once for the request.
        assert!(
            out.is_none(),
            "no tool def survived, so toolConfig is absent"
        );
        assert_eq!(
            shape_count() - before,
            1,
            "the lost tool_choice must count once even with no tools to force"
        );
    }
    #[test]
    #[serial_test::serial(bedrock_converse_tool_choice_name_missing)]
    fn an_ambiguous_tool_choice_still_reaches_the_typed_translation() {
        // REGRESSION GUARD. An object carrying BOTH an unusable Converse-shape
        // `tool` entry and a usable typed spelling must still emit the caller's
        // named tool. Making the name-missing case terminal dropped it entirely,
        // and no fixture covered the hybrid, so the suite could not see it.
        fn name_missing_count() -> u64 {
            crate::translation_drop_metrics::translation_drop_snapshot()
                .into_iter()
                .find(|e| {
                    e.lane == "bedrock-converse" && e.drop_class == "tool_choice_name_missing"
                })
                .map_or(0, |e| e.drop_count)
        }

        // Arrange
        let mut hybrid = req(Some(json!({"tool": {}, "type": "tool", "name": "calc"})));
        hybrid.tools = Some(vec![custom_tool()]);
        let before = name_missing_count();

        // Act
        let out = build_tool_config(ID, &hybrid, &[]).unwrap().unwrap();

        // Assert -- the named tool rides, and nothing was reported lost.
        assert_eq!(
            serde_json::to_value(&out.tool_choice).unwrap(),
            json!({"tool": {"name": "calc"}}),
            "the typed spelling must still translate"
        );
        assert_eq!(
            name_missing_count() - before,
            0,
            "nothing is lost when the fall-through succeeds, so nothing may be counted"
        );

        // Paired control: a PURE Converse-shape entry with no usable name is
        // still terminal and still counted, so the gate narrowed the decision
        // without disabling it.
        let mut pure = req(Some(json!({"tool": {}})));
        pure.tools = Some(vec![custom_tool()]);
        let before_pure = name_missing_count();
        let out_pure = build_tool_config(ID, &pure, &[]).unwrap().unwrap();
        assert!(
            out_pure.tool_choice.is_none(),
            "an unusable Converse-shape entry still drops"
        );
        assert_eq!(
            name_missing_count() - before_pure,
            1,
            "and still counts once"
        );
    }
}
