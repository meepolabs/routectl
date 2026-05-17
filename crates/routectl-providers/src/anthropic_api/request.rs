//! Request normalization: routectl shape -> Anthropic wire format.
//!
//! v0.4.0: rewritten to consume the typed canonical (ContentPart,
//! SystemContent, ToolDef) so cache_control round-trips end-to-end on
//! the Anthropic-in / Anthropic-out and Anthropic-in / Bedrock-Invoke-out
//! paths. Forward-compat: ContentPart::Other and ToolDef::Other pass
//! through verbatim, so a new Anthropic block or builtin tool ships
//! without code edits here.
//!
//! Translation rules:
//! - `req.system` is read directly into the wire `system` field (Text or
//!   Blocks). Backwards-compatible fallback: when `req.system` is None,
//!   any Role::System messages in `req.messages` get lifted (today's
//!   behavior) so direct callers without an ingress aren't broken.
//! - User content is translated typed-block-by-typed-block. Unknown
//!   blocks pass through via ContentPart::Other -> ContentBlock::Other.
//! - Assistant content with reasoning_details (multi-turn tool-use)
//!   continues to require a signature on each thinking block.
//! - Tool message: the canonical Tool role becomes a user message with
//!   a tool_result block, same as today.
//! - Tools: ToolDef::Custom -> AnthropicTool::Custom (cache_control,
//!   defer_loading, strict, optional type_tag); ToolDef::Other ->
//!   AnthropicTool::Builtin (passthrough Value).
//! - Top-level cache_control and anthropic_beta are set on the body.
//! - cache_control::validate runs before serialization (debug_assert
//!   only; keeps non-debug builds fast).

use serde_json::{json, Value};

use routectl_core::cache_control::{self, Breakpoint, BreakpointPosition};
use routectl_core::{
    is_canonical_request_key, ChatRequest, ContentPart, CustomTool, Error, KnownContentPart,
    Message, MessageContent, ReasoningDetail, ReasoningDetailKind, Result, Role, SystemContent,
    ToolDef,
};

use super::parts::{parse_image_url_source, strip_text_after_tool_use};
use super::types::{
    AnthropicContent, AnthropicMessage, AnthropicRequest, AnthropicRole, AnthropicSystem,
    AnthropicSystemBlock, AnthropicTool, ContentBlock, OutputConfig, ThinkingConfig,
};

const DEFAULT_MAX_TOKENS: u32 = 4096;
const ANTHROPIC_FORMAT: &str = "anthropic-claude-v1";

/// Proportional budget_tokens as fraction of max_tokens per effort level.
/// Only consulted on the legacy `ThinkingConfig::Enabled` path -- the
/// adaptive-thinking path passes `effort` through verbatim into
/// `output_config.effort` and never calls this.
fn effort_ratio(effort: &str) -> f64 {
    match effort {
        // `max` arrived with the Opus 4.7+ adaptive thinking shape,
        // but the legacy `Enabled { budget_tokens }` path may still
        // see it on a non-adaptive provider. 0.99 leaves 1% of
        // max_tokens for the visible response so the request is
        // accepted; in practice operators who want `max` should set
        // `adaptive_thinking = true` on the provider.
        "max" => 0.99,
        "xhigh" => 0.95,
        "high" => 0.80,
        "medium" => 0.50,
        "low" => 0.20,
        "minimal" => 0.10,
        _ => 0.50,
    }
}

/// Effort string to use for top-level `output_config.effort`. Returns
/// `req.reasoning.effort` verbatim when set; falls back to "medium"
/// otherwise (Anthropic requires the field when adaptive thinking is
/// active and validates the string).
fn derive_effort(req: &ChatRequest) -> String {
    req.reasoning
        .as_ref()
        .and_then(|r| r.effort.clone())
        .unwrap_or_else(|| "medium".to_string())
}

/// Decide which `ThinkingConfig` variant (if any) to emit. The
/// `adaptive` flag selects the wire shape: when `true` AND thinking
/// would otherwise be `Enabled`, returns `Adaptive` instead (the
/// caller pairs that with a top-level `output_config`); when `false`,
/// returns the legacy `Enabled { budget_tokens }` shape; `Disabled`
/// is always returned verbatim regardless of the flag.
///
/// Note on `max_tokens` + adaptive: Anthropic's adaptive thinking wire
/// shape has no field for an explicit budget -- the model picks its
/// own from the effort string. If a caller sets both
/// `reasoning.max_tokens` AND the provider has `adaptive_thinking =
/// true`, the budget is dropped (with a tracing::warn at the call
/// site). The caller's effort string still travels to
/// `output_config.effort`.
pub(crate) fn build_thinking(req: &ChatRequest, adaptive: bool) -> Option<ThinkingConfig> {
    let r = req.reasoning.as_ref()?;

    if r.enabled == Some(false) {
        return Some(ThinkingConfig::Disabled);
    }
    if r.effort.as_deref() == Some("none") {
        return Some(ThinkingConfig::Disabled);
    }

    // Did the caller actually ask for thinking? Any of: explicit
    // enabled=true, a budget, an effort string (other than "none").
    let thinking_active = r.enabled == Some(true)
        || r.max_tokens.is_some()
        || r.effort.as_deref().is_some_and(|e| e != "none");
    if !thinking_active {
        return None;
    }

    if adaptive {
        // Opus 4.7+ wire shape. budget_tokens is gone; effort moves
        // to top-level output_config (handled by build_output_config).
        // If the caller set both an explicit budget AND
        // adaptive_thinking is on, the budget gets dropped because
        // there's no wire field for it. Warn so an operator who set
        // both fields routinely (e.g. a client library that always
        // sends `reasoning.max_tokens`) can see the discard in logs
        // and adjust to using `effort` instead.
        if r.max_tokens.is_some() {
            tracing::warn!(
                budget_tokens = r.max_tokens,
                "reasoning.max_tokens dropped on adaptive thinking path; \
                 Anthropic's adaptive shape has no budget field -- \
                 the model picks its own budget from output_config.effort. \
                 Set reasoning.effort to steer instead."
            );
        }
        return Some(ThinkingConfig::Adaptive);
    }

    // Legacy wire shape: Enabled { budget_tokens }. Translate the
    // canonical signal (explicit budget > effort > enabled=true).
    if let Some(budget) = r.max_tokens {
        return Some(ThinkingConfig::Enabled {
            budget_tokens: budget,
        });
    }
    if let Some(effort) = r.effort.as_deref() {
        let max = req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
        let budget = ((max as f64) * effort_ratio(effort)).max(1.0) as u32;
        return Some(ThinkingConfig::Enabled {
            budget_tokens: budget,
        });
    }
    // r.enabled == Some(true) without budget or effort.
    let max = req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let budget = (max / 2).max(1);
    Some(ThinkingConfig::Enabled {
        budget_tokens: budget,
    })
}

/// Pair `ThinkingConfig::Adaptive` with a top-level `output_config`.
/// Returns `Some(OutputConfig)` only when the thinking variant is
/// `Adaptive`; otherwise `None`.
fn build_output_config(
    req: &ChatRequest,
    thinking: &Option<ThinkingConfig>,
) -> Option<OutputConfig> {
    if matches!(thinking, Some(ThinkingConfig::Adaptive)) {
        Some(OutputConfig {
            effort: derive_effort(req),
        })
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// System
// ---------------------------------------------------------------------------

/// Convert canonical `SystemContent` to wire `AnthropicSystem`. Preserves
/// per-block cache_control and citations.
pub(crate) fn translate_system(s: &SystemContent) -> AnthropicSystem {
    match s {
        SystemContent::Text(t) => AnthropicSystem::Text(t.clone()),
        SystemContent::Blocks(blocks) => AnthropicSystem::Blocks(
            blocks
                .iter()
                .map(|b| AnthropicSystemBlock {
                    kind: b.kind.clone(),
                    text: b.text.clone(),
                    cache_control: b.cache_control.clone(),
                    citations: b.citations.clone(),
                })
                .collect(),
        ),
    }
}

/// Backwards-compat fallback: lift Role::System messages out of the
/// messages array into a flat AnthropicSystem::Text. Used only when
/// `req.system` is None. Returns None when no System messages are
/// present, or when all System messages contain only non-text content
/// (Parts without text blocks, Null) -- avoids emitting a meaningless
/// `system: ""` upstream and the extra newlines from joining blanks.
///
/// `pub(crate)` so the Bedrock Converse egress can reuse the same
/// legacy-shape fallback (single source of truth).
pub(crate) fn lift_legacy_system(messages: &[Message]) -> Option<AnthropicSystem> {
    let texts: Vec<String> = messages
        .iter()
        .filter(|m| matches!(m.role, Role::System))
        .filter_map(|m| match &m.content {
            MessageContent::Text(t) => {
                let trimmed = t.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(t.clone())
                }
            }
            MessageContent::Parts(parts) => {
                // Pick out text content from typed parts. Image/Document/etc.
                // in a System message are not meaningful for the flat-text
                // lift and would have been dropped by the egress anyway.
                let collected: Vec<String> = parts
                    .iter()
                    .filter_map(|p| match p {
                        routectl_core::ContentPart::Known(
                            routectl_core::KnownContentPart::Text { text, .. },
                        ) => {
                            let trimmed = text.trim();
                            if trimmed.is_empty() {
                                None
                            } else {
                                Some(text.clone())
                            }
                        }
                        _ => None,
                    })
                    .collect();
                if collected.is_empty() {
                    None
                } else {
                    Some(collected.join("\n"))
                }
            }
            MessageContent::Null => None,
        })
        .collect();
    if texts.is_empty() {
        None
    } else {
        Some(AnthropicSystem::Text(texts.join("\n")))
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

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
fn translate_tool_choice(tc: Option<&Value>, has_tools: bool) -> Option<Value> {
    let tc = tc?;
    match tc {
        Value::String(s) => match s.as_str() {
            "auto" => Some(serde_json::json!({"type":"auto"})),
            "required" => Some(serde_json::json!({"type":"any"})),
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
                    Some(n) => Some(serde_json::json!({"type":"tool","name":n})),
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

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

/// Translate one canonical ContentPart into a wire ContentBlock.
/// Forward-compat: ContentPart::Other passes through verbatim as
/// ContentBlock::Other so the Anthropic-in / Anthropic-out path keeps
/// working when Anthropic ships a new block type.
/// Walk the canonical `ChatRequest` and reject malformed multi-turn
/// shapes that the translation helpers would otherwise paper over
/// with empty-string fallbacks. Anthropic's wire requires:
///
/// - Every `Thinking` content part carries a `signature` for replay.
///   Without it Anthropic / Bedrock 400 with a confusing error;
///   surfacing the missing signature here gives operators the
///   precise field to fix.
/// - Every tool_result message (canonical `Role::Tool`) carries a
///   `tool_call_id` matching the preceding `tool_use.id`. Without
///   it Anthropic 400 with "tool_use ids were found without
///   tool_result blocks immediately after" or a similar error
///   that doesn't name the bad message.
fn validate_replay_invariants(id: &str, req: &ChatRequest) -> Result<()> {
    for (i, msg) in req.messages.iter().enumerate() {
        if matches!(msg.role, Role::Tool) && msg.tool_call_id.as_deref().unwrap_or("").is_empty() {
            return Err(Error::normalize_request(
                id,
                format!(
                    "messages[{i}] is a tool_result (Role::Tool) without tool_call_id; \
                     Anthropic requires the id of the tool_use this is answering",
                ),
            ));
        }
        if let MessageContent::Parts(parts) = &msg.content {
            for (j, p) in parts.iter().enumerate() {
                if let ContentPart::Known(KnownContentPart::Thinking { signature, .. }) = p {
                    if signature.as_deref().unwrap_or("").is_empty() {
                        return Err(Error::normalize_request(
                            id,
                            format!(
                                "messages[{i}].content[{j}] is a thinking block without \
                                 signature; Anthropic requires the upstream-supplied \
                                 signature to replay thinking on a multi-turn request",
                            ),
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

fn translate_content_part(p: &ContentPart) -> ContentBlock {
    match p {
        ContentPart::Known(k) => translate_known_part(k),
        ContentPart::Other {
            type_tag,
            cache_control,
            extras,
        } => ContentBlock::Other {
            type_tag: type_tag.clone(),
            cache_control: cache_control.clone(),
            extras: extras.clone(),
        },
    }
}

fn translate_known_part(k: &KnownContentPart) -> ContentBlock {
    match k {
        KnownContentPart::Text {
            text,
            cache_control,
        } => ContentBlock::Text {
            text: text.clone(),
            cache_control: cache_control.clone(),
        },
        KnownContentPart::Image {
            source,
            cache_control,
        } => ContentBlock::Image {
            source: source.clone(),
            cache_control: cache_control.clone(),
        },
        // OpenAI-shape ImageUrl translates to an Anthropic image
        // block. Two URL shapes need different Anthropic source forms:
        //
        //   - HTTPS direct  ->  {type: "url", url: "..."}
        //   - data: URI     ->  {type: "base64", media_type: "...", data: "..."}
        //
        // Bedrock + Anthropic API both reject data: URIs in the URL
        // source form ("URL sources are not supported"); they require
        // the base64 source. OpenAI multimodal clients (claude-code's
        // OpenAI-compat fallback, vanilla OpenAI SDK, etc.) embed
        // images via `data:image/<fmt>;base64,<payload>`, so we parse
        // the data: prefix here and rewrite. Anything else
        // (https://, gs://, malformed) flows through as URL source --
        // upstream will surface a clean error if it isn't supported.
        KnownContentPart::ImageUrl {
            image_url,
            cache_control,
        } => {
            let url = image_url.get("url").and_then(|v| v.as_str()).unwrap_or("");
            let source = parse_image_url_source(url);
            ContentBlock::Image {
                source,
                cache_control: cache_control.clone(),
            }
        }
        KnownContentPart::Document {
            source,
            title,
            citations,
            cache_control,
        } => ContentBlock::Document {
            source: source.clone(),
            title: title.clone(),
            citations: citations.clone(),
            cache_control: cache_control.clone(),
        },
        KnownContentPart::ToolUse {
            id,
            name,
            input,
            cache_control,
        } => ContentBlock::ToolUse {
            id: id.clone(),
            name: name.clone(),
            input: input.clone(),
            cache_control: cache_control.clone(),
        },
        KnownContentPart::ToolResult {
            tool_use_id,
            content,
            is_error,
            cache_control,
        } => ContentBlock::ToolResult {
            tool_use_id: tool_use_id.clone(),
            content: content.clone(),
            cache_control: cache_control.clone(),
            is_error: *is_error,
        },
        KnownContentPart::Thinking {
            thinking,
            signature,
        } => ContentBlock::Thinking {
            thinking: thinking.clone(),
            // Wire requires signature; absent on canonical means we fall
            // back to empty. Multi-turn callers should always set this;
            // build_assistant_content errors when reasoning_details lack
            // a signature.
            signature: signature.clone().unwrap_or_default(),
            cache_control: None,
        },
        KnownContentPart::RedactedThinking { data } => ContentBlock::RedactedThinking {
            data: data.clone(),
            cache_control: None,
        },
    }
}

/// Reconstruct an Anthropic content array for an assistant message that
/// carries reasoning_details (tool-use continuity). thinking blocks with
/// signatures must be passed back verbatim.
fn build_assistant_content(id: &str, msg: &Message) -> Result<AnthropicContent> {
    let has_tool_calls = msg
        .tool_calls
        .as_ref()
        .map(|tc| !tc.is_empty())
        .unwrap_or(false);
    if msg.reasoning_details.is_empty() && !has_tool_calls {
        // No multi-turn reasoning to thread back AND no OpenAI-shape
        // tool_calls field to re-emit; fall through to the generic
        // content translation (Text or Parts), but strip trailing
        // text-after-tool_use first (see helper docstring).
        return Ok(translate_assistant_simple_content(&msg.content));
    }

    let mut blocks = emit_reasoning_blocks(id, &msg.reasoning_details)?;
    append_assistant_message_blocks(&mut blocks, &msg.content);
    if let Some(tool_calls) = msg.tool_calls.as_ref() {
        emit_tool_use_blocks_from_calls(id, tool_calls, &mut blocks)?;
    }
    Ok(AnthropicContent::Blocks(blocks))
}

/// Re-emit OpenAI-shape `tool_calls` (the canonical
/// representation produced by `walk_content_blocks` on the
/// response side) as Anthropic `ContentBlock::ToolUse` entries
/// for multi-turn replay. Without this, an OpenAI-ingress
/// request whose assistant history carries `tool_calls` -- or a
/// caller that echoes a canonical Message returned by routectl
/// straight back as a multi-turn turn -- would silently drop the
/// tool_use blocks, and the next user turn's `tool_result` would
/// fail upstream with "tool_use ids were found without
/// preceding tool_use blocks".
///
/// OpenAI shape: `{id, type: "function", function: {name, arguments}}`
/// where `arguments` is a JSON-encoded STRING. Anthropic shape:
/// `ContentBlock::ToolUse { id, name, input: Value }` where
/// `input` is the parsed JSON object. We attempt parsing first
/// and fall back to wrapping the raw string under
/// `{"_arguments": "..."}` so the upstream can return a useful
/// error rather than us silently producing a malformed body.
fn emit_tool_use_blocks_from_calls(
    id: &str,
    tool_calls: &[Value],
    blocks: &mut Vec<ContentBlock>,
) -> Result<()> {
    for call in tool_calls {
        let tool_id = call
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let function = call.get("function");
        let name = function
            .and_then(|f| f.get("name"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let arguments_raw = function
            .and_then(|f| f.get("arguments"))
            .and_then(|v| v.as_str())
            .unwrap_or("{}");
        let input = if arguments_raw.is_empty() {
            json!({})
        } else {
            serde_json::from_str(arguments_raw).unwrap_or_else(|e| {
                tracing::warn!(
                    provider = id,
                    tool_id = %tool_id,
                    error = %e,
                    "tool_call.arguments not valid JSON; wrapping under _arguments for upstream",
                );
                json!({ "_arguments": arguments_raw })
            })
        };
        blocks.push(ContentBlock::ToolUse {
            id: tool_id,
            name,
            input,
            cache_control: None,
        });
    }
    Ok(())
}

/// Translate `reasoning_details` into Anthropic `Thinking` /
/// `RedactedThinking` blocks for echo on a multi-turn assistant turn.
/// Index-ordered so an upstream that re-orders reasoning blocks
/// doesn't surprise the downstream signature check. Anthropic rejects
/// a `Thinking` block on echo without the `signature` field; when a
/// detail's signature is missing or empty (Anthropic 4.5 occasionally
/// omits `signature_delta` on tool-only thinking turns), the detail
/// is logged at WARN and skipped so replay doesn't 400 on a
/// guaranteed-malformed echo. WARN level (not DEBUG) so operators
/// see the partial echo and can correlate with upstream cache misses
/// or quality drift -- mixed signed/unsigned histories lose ordering
/// fidelity. See CLAUDE.md "Anthropic streaming reasoning replay".
fn emit_reasoning_blocks(id: &str, details: &[ReasoningDetail]) -> Result<Vec<ContentBlock>> {
    let mut sorted = details.to_vec();
    sorted.sort_by_key(|d| d.index.unwrap_or(0));

    let mut blocks: Vec<ContentBlock> = Vec::with_capacity(sorted.len());
    let mut skipped_unsigned: Vec<Option<u32>> = Vec::new();
    for detail in &sorted {
        match detail.kind {
            ReasoningDetailKind::Text => {
                if detail.format.as_deref() != Some(ANTHROPIC_FORMAT) {
                    continue;
                }
                let thinking = detail
                    .payload
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let signature = detail
                    .payload
                    .get("signature")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if signature.is_empty() {
                    // Anthropic 400s on a Thinking block without a
                    // signature; skipping is better than a hard fail.
                    // Aggregate the WARN per-call (Claude 4.5 multi-
                    // block thinking turns can pile up several skipped
                    // entries and per-detail WARN would flood the log).
                    skipped_unsigned.push(detail.index);
                    continue;
                }
                blocks.push(ContentBlock::Thinking {
                    thinking,
                    signature: signature.to_string(),
                    cache_control: None,
                });
            }
            ReasoningDetailKind::Encrypted => {
                if detail.format.as_deref() != Some(ANTHROPIC_FORMAT) {
                    continue;
                }
                let data = detail
                    .payload
                    .get("data")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                blocks.push(ContentBlock::RedactedThinking {
                    data,
                    cache_control: None,
                });
            }
            ReasoningDetailKind::Summary => {
                // Not an Anthropic block; skip.
            }
        }
    }
    if !skipped_unsigned.is_empty() {
        tracing::warn!(
            provider = id,
            skipped_count = skipped_unsigned.len(),
            skipped_indices = ?skipped_unsigned,
            "skipping Thinking blocks on replay: signature missing or empty \
             (multi-block thinking history is now partially echoed; \
             see CLAUDE.md \"Anthropic streaming reasoning replay\" residual)"
        );
    }
    Ok(blocks)
}

/// Append the assistant message's text/parts content AFTER the
/// reasoning blocks already pushed. For Text, emits a single Text
/// block (skipped on empty/Null since reasoning-only assistant turns
/// are valid). For Parts, translates each block (after stripping
/// trailing text-after-tool_use, which both Bedrock and Anthropic
/// reject with "tool_use ids were found without tool_result blocks
/// immediately after").
fn append_assistant_message_blocks(blocks: &mut Vec<ContentBlock>, content: &MessageContent) {
    match content {
        MessageContent::Text(t) if !t.is_empty() => blocks.push(ContentBlock::Text {
            text: t.clone(),
            cache_control: None,
        }),
        MessageContent::Text(_) | MessageContent::Null => {}
        MessageContent::Parts(parts) => {
            let cleaned = strip_text_after_tool_use(parts);
            for p in cleaned.iter() {
                blocks.push(translate_content_part(p));
            }
        }
    }
}

/// Assistant-message variant of `translate_simple_content` that strips
/// trailing text-after-tool_use before per-part translation. Called
/// only from `build_assistant_content`. Text/Null arms delegate to
/// `translate_simple_content` so the two stay in lockstep -- only the
/// `Parts` arm needs the strip.
fn translate_assistant_simple_content(c: &MessageContent) -> AnthropicContent {
    match c {
        MessageContent::Parts(parts) => {
            let cleaned = strip_text_after_tool_use(parts);
            AnthropicContent::Blocks(cleaned.iter().map(translate_content_part).collect())
        }
        // Text/Null arms are identical to `translate_simple_content`;
        // delegate to keep them in one place.
        _ => translate_simple_content(c),
    }
}

/// Translate plain message content (no multi-turn reasoning context).
/// Text -> AnthropicContent::Text (cheaper wire form). Parts ->
/// AnthropicContent::Blocks via per-part translation.
fn translate_simple_content(c: &MessageContent) -> AnthropicContent {
    match c {
        MessageContent::Text(t) => AnthropicContent::Text(t.clone()),
        MessageContent::Null => AnthropicContent::Text(String::new()),
        MessageContent::Parts(parts) => {
            AnthropicContent::Blocks(parts.iter().map(translate_content_part).collect())
        }
    }
}

// ---------------------------------------------------------------------------
// Tool-role messages
// ---------------------------------------------------------------------------

fn build_tool_message(msg: &Message) -> AnthropicMessage {
    let tool_use_id = msg.tool_call_id.clone().unwrap_or_default();
    // Anthropic tool_result.content accepts either a string or an array
    // of content blocks. We honor whichever shape the canonical message
    // carries.
    let content_val = match &msg.content {
        MessageContent::Text(t) => Value::String(t.clone()),
        MessageContent::Parts(parts) => Value::Array(
            parts
                .iter()
                .map(|p| serde_json::to_value(translate_content_part(p)).unwrap_or(Value::Null))
                .collect(),
        ),
        MessageContent::Null => Value::Null,
    };
    AnthropicMessage {
        role: AnthropicRole::User,
        content: AnthropicContent::Blocks(vec![ContentBlock::ToolResult {
            tool_use_id,
            content: content_val,
            cache_control: None,
            is_error: None,
        }]),
    }
}

// ---------------------------------------------------------------------------
// cache_control validation
// ---------------------------------------------------------------------------

/// Walk all positions of an AnthropicRequest and call
/// `cache_control::validate` against the collected breakpoint sequence.
/// Catches 1h-after-5m ordering violations and 5+ breakpoint counts
/// before they reach upstream.
fn validate_breakpoints(ar: &AnthropicRequest) -> Result<()> {
    let mut bps: Vec<Breakpoint<'_>> = Vec::new();

    // Owned cache_control values pulled out of `AnthropicTool::Builtin`'s
    // raw JSON. Lives here so the Breakpoint slice below can reference
    // them without lifetime issues. Indexed by position in `ar.tools`.
    let builtin_tool_ccs: Vec<Option<routectl_core::CacheControl>> = ar
        .tools
        .as_ref()
        .map(|tools| {
            tools
                .iter()
                .map(|t| match t {
                    AnthropicTool::Builtin(v) => v
                        .as_object()
                        .and_then(|o| o.get("cache_control"))
                        .and_then(|cc| {
                            serde_json::from_value::<routectl_core::CacheControl>(cc.clone()).ok()
                        }),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    // Tools come first in the cache prefix.
    if let Some(tools) = &ar.tools {
        for (i, t) in tools.iter().enumerate() {
            if let Some(cc) = anthropic_tool_cache_control(t) {
                bps.push(Breakpoint {
                    position: BreakpointPosition::Tools,
                    control: cc,
                });
            } else if let Some(cc) = builtin_tool_ccs.get(i).and_then(|o| o.as_ref()) {
                bps.push(Breakpoint {
                    position: BreakpointPosition::Tools,
                    control: cc,
                });
            }
        }
    }

    // Then system blocks.
    if let Some(AnthropicSystem::Blocks(blocks)) = &ar.system {
        for b in blocks {
            if let Some(cc) = b.cache_control.as_ref() {
                bps.push(Breakpoint {
                    position: BreakpointPosition::System,
                    control: cc,
                });
            }
        }
    }

    // Then messages.
    for m in &ar.messages {
        if let AnthropicContent::Blocks(blocks) = &m.content {
            for b in blocks {
                if let Some(cc) = content_block_cache_control(b) {
                    bps.push(Breakpoint {
                        position: BreakpointPosition::Messages,
                        control: cc,
                    });
                }
            }
        }
    }

    // Top-level auto-cache marker.
    if let Some(cc) = ar.cache_control.as_ref() {
        bps.push(Breakpoint {
            position: BreakpointPosition::TopLevel,
            control: cc,
        });
    }

    cache_control::validate(&bps)
}

fn content_block_cache_control(b: &ContentBlock) -> Option<&routectl_core::CacheControl> {
    match b {
        ContentBlock::Text { cache_control, .. }
        | ContentBlock::Image { cache_control, .. }
        | ContentBlock::Document { cache_control, .. }
        | ContentBlock::Thinking { cache_control, .. }
        | ContentBlock::RedactedThinking { cache_control, .. }
        | ContentBlock::ToolUse { cache_control, .. }
        | ContentBlock::ToolResult { cache_control, .. }
        | ContentBlock::Other { cache_control, .. } => cache_control.as_ref(),
    }
}

fn anthropic_tool_cache_control(t: &AnthropicTool) -> Option<&routectl_core::CacheControl> {
    match t {
        AnthropicTool::Custom { cache_control, .. } => cache_control.as_ref(),
        AnthropicTool::Builtin(_) => None,
    }
}

// ---------------------------------------------------------------------------
// Top-level normalize
// ---------------------------------------------------------------------------

/// Filter `req.anthropic_beta` against the operator-supplied
/// `allowed_betas` list. Empty allowlist = pass-through (default).
/// Otherwise, drop entries not in the list at DEBUG so operators
/// triaging unexpected behavior can see WHICH flags got removed.
/// Mirrors the Bedrock-egress `filter_bedrock_betas` shape.
fn filter_anthropic_betas(
    provider_id: &str,
    requested: &[String],
    allowed: &[String],
) -> Vec<String> {
    if allowed.is_empty() {
        return requested.to_vec();
    }
    let mut kept = Vec::with_capacity(requested.len());
    for flag in requested {
        if allowed.iter().any(|a| a == flag) {
            kept.push(flag.clone());
        } else {
            tracing::debug!(
                provider = provider_id,
                flag = %flag,
                "dropping beta flag not in operator-supplied [providers.X] allowed_betas"
            );
        }
    }
    kept
}

pub fn normalize(
    id: &str,
    req: &ChatRequest,
    adaptive_thinking: bool,
    allowed_betas: &[String],
) -> Result<Value> {
    // Anthropic's wire requires (a) every Thinking block carry a
    // `signature` for multi-turn, (b) every tool_result carry the
    // `tool_use_id` of the tool_use it answers. Validate up front so
    // routectl doesn't emit empty-string fallbacks that 400 vaguely
    // upstream.
    validate_replay_invariants(id, req)?;

    let max_tokens = req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let thinking = build_thinking(req, adaptive_thinking);
    let output_config = build_output_config(req, &thinking);

    // Prefer canonical req.system; fall back to lifting Role::System
    // messages for direct callers that bypass an ingress.
    let system = req
        .system
        .as_ref()
        .map(translate_system)
        .or_else(|| lift_legacy_system(&req.messages));

    let anthropic_messages = translate_messages(id, &req.messages)?;

    // tool_choice="none" forbids tool use; Anthropic has no native
    // equivalent for the bare-string OpenAI form, so strip BOTH the
    // field and the tools list. The Anthropic-shape `{"type":"none"}`
    // object form passes through above and Anthropic suppresses tool
    // use server-side, so it doesn't need the extra strip.
    let suppress_tools = matches!(
        req.tool_choice.as_ref(),
        Some(Value::String(s)) if s == "none"
    );
    let has_tools = req.tools.as_ref().is_some_and(|t| !t.is_empty());
    let tools = if suppress_tools {
        None
    } else {
        req.tools
            .as_ref()
            .map(|ts| ts.iter().map(translate_tool).collect::<Vec<_>>())
    };

    // Anthropic requires temperature=1.0 when thinking is enabled
    // (legacy and adaptive both): no alternative-continuation sampling
    // while spending reasoning budget.
    let temperature = match &thinking {
        Some(ThinkingConfig::Enabled { .. }) | Some(ThinkingConfig::Adaptive) => Some(1.0f64),
        _ => req.temperature,
    };

    let ar = AnthropicRequest {
        model: req.model.clone(),
        messages: anthropic_messages,
        max_tokens,
        system,
        thinking,
        output_config,
        temperature,
        top_p: req.top_p,
        stop_sequences: req.stop.clone(),
        stream: None, // caller sets this
        tools,
        tool_choice: translate_tool_choice(req.tool_choice.as_ref(), has_tools),
        cache_control: req.cache_control.clone(),
        anthropic_beta: filter_anthropic_betas(id, &req.anthropic_beta, allowed_betas),
    };

    // Belt-and-braces: validate in release too. The Anthropic ingress
    // already runs this at parse time; running it again here catches
    // direct callers (library users without an ingress) and protects
    // upstream from cap/ordering violations regardless of build mode.
    validate_breakpoints(&ar)?;

    let mut body =
        serde_json::to_value(&ar).map_err(|e| Error::normalize_request(id, e.to_string()))?;

    merge_provider_extras(id, &mut body, req.provider_extras.as_ref());
    Ok(body)
}

/// Iterate the canonical messages and produce the Anthropic-shaped
/// per-role list. System messages are intentionally dropped here --
/// they're already lifted into `req.system` (canonical) or by
/// `lift_legacy_system` for direct callers without an ingress, so
/// re-emitting them as messages would duplicate.
fn translate_messages(id: &str, messages: &[Message]) -> Result<Vec<AnthropicMessage>> {
    let mut out: Vec<AnthropicMessage> = Vec::with_capacity(messages.len());
    for msg in messages {
        match msg.role {
            Role::System => {
                // Already handled via req.system / lift_legacy_system.
                // Drop here (do not duplicate in the messages array).
            }
            Role::User => out.push(AnthropicMessage {
                role: AnthropicRole::User,
                content: translate_simple_content(&msg.content),
            }),
            Role::Assistant => out.push(AnthropicMessage {
                role: AnthropicRole::Assistant,
                content: build_assistant_content(id, msg)?,
            }),
            Role::Tool => out.push(build_tool_message(msg)),
        }
    }
    Ok(out)
}

/// Merge `provider_extras` into the assembled body. Caller-supplied
/// keys win EXCEPT for routectl-managed top-level keys (see
/// `is_routectl_managed_key`); those are dropped with a WARN log so a
/// malicious or careless `provider_extras = {"messages": [...]}` can't
/// replace the assembled messages array. This was an architecture-review
/// finding (MEDIUM-1).
fn merge_provider_extras(id: &str, body: &mut Value, extras: Option<&Value>) {
    let Some(extras) = extras else { return };
    let (Some(obj), Some(extra_obj)) = (body.as_object_mut(), extras.as_object()) else {
        return;
    };
    for (k, v) in extra_obj {
        if is_routectl_managed_key(k) {
            tracing::warn!(
                provider = id,
                key = %k,
                "provider_extras attempted to override routectl-managed key; dropped"
            );
            continue;
        }
        obj.insert(k.clone(), v.clone());
    }
}

/// Top-level Anthropic body keys that routectl owns. Delegates to the
/// shared canonical list and adds Anthropic-API-specific keys that are
/// not on `ChatRequest` but are written by this egress from canonical
/// fields:
/// - `thinking`  -- translated from `req.reasoning` by `build_thinking`;
///   not a raw ChatRequest field but routectl writes it.
fn is_routectl_managed_key(key: &str) -> bool {
    is_canonical_request_key(key)
        || matches!(
            key,
            // Anthropic-API-specific managed keys not on ChatRequest:
            // `thinking` is built from req.reasoning by this egress.
            "thinking"
        )
}

#[cfg(test)]
mod allowlist_tests {
    use super::*;
    use routectl_core::{ChatRequest, Message, Role};
    use serde_json::json;

    fn req_with_betas(betas: Vec<String>) -> ChatRequest {
        ChatRequest {
            model: "claude-sonnet-4-5".into(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            max_tokens: Some(64),
            anthropic_beta: betas,
            ..Default::default()
        }
    }

    /// Pin: empty allowlist = pass-through. Default behavior, no
    /// operator surprise on upgrade.
    #[test]
    fn empty_allowlist_passes_all_betas() {
        let req = req_with_betas(vec![
            "context-1m-2025-08-07".into(),
            "prompt-caching-2024-07-31".into(),
        ]);
        let body = normalize("p", &req, false, &[]).unwrap();
        assert_eq!(
            body["anthropic_beta"],
            json!(["context-1m-2025-08-07", "prompt-caching-2024-07-31"])
        );
    }

    /// Pin: non-empty allowlist drops entries not in the list.
    #[test]
    fn non_empty_allowlist_drops_unknown() {
        let req = req_with_betas(vec![
            "context-1m-2025-08-07".into(),
            "secret-experimental-flag".into(),
            "prompt-caching-2024-07-31".into(),
        ]);
        let allowed = vec![
            "context-1m-2025-08-07".to_string(),
            "prompt-caching-2024-07-31".to_string(),
        ];
        let body = normalize("p", &req, false, &allowed).unwrap();
        // Order preserved, unknown flag dropped.
        assert_eq!(
            body["anthropic_beta"],
            json!(["context-1m-2025-08-07", "prompt-caching-2024-07-31"])
        );
    }

    /// Pin: every requested beta is rejected when none are on the
    /// allowlist. The wire field is either absent or an empty array;
    /// both mean "no betas reach upstream" and either serialization
    /// is acceptable.
    #[test]
    fn allowlist_can_drop_all_requested() {
        let req = req_with_betas(vec!["totally-unknown".into()]);
        let allowed = vec!["context-1m-2025-08-07".to_string()];
        let body = normalize("p", &req, false, &allowed).unwrap();
        let got = &body["anthropic_beta"];
        assert!(
            got.is_null() || got.as_array().map(|a| a.is_empty()).unwrap_or(false),
            "expected absent or empty array, got: {got}"
        );
    }
}

#[cfg(test)]
mod multi_turn_tool_use_tests {
    use super::*;
    use routectl_core::{ChatRequest, Message, Role};
    use serde_json::json;

    fn user_msg(text: &str) -> Message {
        Message {
            role: Role::User,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn assistant_msg(text: &str, tool_calls: Option<Vec<Value>>) -> Message {
        Message {
            role: Role::Assistant,
            content: MessageContent::Text(text.into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls,
        }
    }

    /// On a multi-turn assistant turn, `Message.tool_calls` (the
    /// canonical OpenAI-shape representation produced by
    /// `walk_content_blocks` on the response side) must be re-emitted
    /// as Anthropic `ContentBlock::ToolUse` entries. Without this,
    /// echoing a canonical Message back through the Anthropic egress
    /// drops the tool_use blocks and the next user `tool_result` turn
    /// fails upstream with "tool_use ids were found without preceding
    /// tool_use blocks".
    #[test]
    fn assistant_message_with_tool_calls_emits_tool_use_blocks() {
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![
                user_msg("calculate 2+2"),
                assistant_msg(
                    "Let me calculate.",
                    Some(vec![json!({
                        "id": "toolu_abc123",
                        "type": "function",
                        "function": {
                            "name": "calc",
                            "arguments": "{\"expr\":\"2+2\"}",
                        }
                    })]),
                ),
            ],
            ..Default::default()
        };

        let body = normalize("test-anthropic", &req, false, &[]).unwrap();
        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        let assistant = messages
            .iter()
            .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
            .expect("assistant message must be present");
        let blocks = assistant
            .get("content")
            .and_then(|v| v.as_array())
            .expect("assistant content must be Blocks form when tool_calls present");

        let tool_use = blocks
            .iter()
            .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
            .expect("assistant must carry a tool_use block on multi-turn replay");
        assert_eq!(tool_use["id"], "toolu_abc123");
        assert_eq!(tool_use["name"], "calc");
        assert_eq!(tool_use["input"], json!({"expr": "2+2"}));
    }

    #[test]
    fn tool_message_without_tool_call_id_is_rejected() {
        // Anthropic requires `tool_result` to reference the
        // `tool_use.id` it answers. An empty / missing
        // `tool_call_id` on a Role::Tool message used to fall
        // through as `unwrap_or_default()` (empty string) and
        // upstream returned a vague 400. Reject locally with a
        // precise NormalizeRequest error.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![Message {
                role: Role::Tool,
                content: MessageContent::Text("result content".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            ..Default::default()
        };
        let err = normalize("test-anthropic", &req, false, &[]).unwrap_err();
        assert!(
            err.to_string().contains("tool_call_id"),
            "must mention tool_call_id; got: {err}"
        );
    }

    #[test]
    fn thinking_part_without_signature_is_rejected() {
        // KnownContentPart::Thinking with `signature: None` previously
        // emitted an empty-string signature to upstream which fails
        // multi-turn replay with a vague Anthropic 400. Reject
        // locally with a precise NormalizeRequest error.
        use routectl_core::{ContentPart, KnownContentPart};
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![Message {
                role: Role::Assistant,
                content: MessageContent::Parts(vec![ContentPart::Known(
                    KnownContentPart::Thinking {
                        thinking: "let me think".into(),
                        signature: None,
                    },
                )]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }],
            ..Default::default()
        };
        let err = normalize("test-anthropic", &req, false, &[]).unwrap_err();
        assert!(
            err.to_string().contains("thinking block without signature"),
            "must mention signature; got: {err}"
        );
    }

    #[test]
    fn assistant_tool_call_with_unparseable_arguments_wraps_under_underscore() {
        // Defensive fallback: a tool_call.arguments string that
        // isn't valid JSON shouldn't silently produce a malformed
        // upstream body. We wrap under {"_arguments": "..."} and
        // emit a WARN, so the upstream returns a useful error.
        let req = ChatRequest {
            model: "claude-sonnet-4".into(),
            messages: vec![assistant_msg(
                "",
                Some(vec![json!({
                    "id": "toolu_xyz",
                    "type": "function",
                    "function": {"name": "calc", "arguments": "this is not json"}
                })]),
            )],
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[]).unwrap();
        let messages = body.get("messages").and_then(|v| v.as_array()).unwrap();
        let assistant = messages
            .iter()
            .find(|m| m.get("role").and_then(|v| v.as_str()) == Some("assistant"))
            .unwrap();
        let blocks = assistant.get("content").and_then(|v| v.as_array()).unwrap();
        let tool_use = blocks
            .iter()
            .find(|b| b.get("type").and_then(|v| v.as_str()) == Some("tool_use"))
            .unwrap();
        assert_eq!(tool_use["input"], json!({"_arguments": "this is not json"}));
    }

    /// FX-1: with `adaptive_thinking = true`, the wire shape is the
    /// Opus 4.7+ form -- `thinking: {type:"adaptive"}` (no
    /// `budget_tokens`) plus a top-level `output_config: {effort:...}`
    /// carrying the canonical `reasoning.effort` string verbatim.
    #[test]
    fn adaptive_thinking_emits_adaptive_shape_with_output_config() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1024),
            reasoning: Some(ReasoningConfig {
                effort: Some("xhigh".into()),
                max_tokens: Some(8000),
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, true, &[]).unwrap();

        // thinking serializes to {"type":"adaptive"} -- no budget_tokens.
        let thinking = body.get("thinking").expect("thinking field present");
        assert_eq!(thinking["type"], "adaptive");
        assert!(
            thinking.get("budget_tokens").is_none(),
            "adaptive shape must not carry budget_tokens, got {thinking:?}"
        );

        // output_config carries the effort verbatim.
        let oc = body.get("output_config").expect("output_config present");
        assert_eq!(oc["effort"], "xhigh");

        // Anthropic requires temperature == 1.0 with thinking active --
        // both Enabled and Adaptive variants trigger the same constraint.
        assert_eq!(body["temperature"], 1.0);
    }

    /// FX-1: with `adaptive_thinking = false` (or absent), the wire
    /// shape is the legacy `Enabled { budget_tokens }` form. Older
    /// Claude models (4.5/4.6 family) still want this shape and would
    /// 400 on the adaptive form.
    #[test]
    fn legacy_thinking_unchanged_when_flag_false() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-opus-4-6".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1024),
            reasoning: Some(ReasoningConfig {
                effort: Some("high".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[]).unwrap();

        let thinking = body.get("thinking").expect("thinking field present");
        assert_eq!(thinking["type"], "enabled");
        // budget_tokens = max_tokens (1024) * effort_ratio("high")=0.80 = 819
        assert_eq!(thinking["budget_tokens"], 819);

        // No output_config on the legacy path.
        assert!(
            body.get("output_config").is_none(),
            "legacy shape must not emit output_config, got {body:?}"
        );

        assert_eq!(body["temperature"], 1.0);
    }

    /// FX-1: `effort = "max"` on the legacy path maps to a near-total
    /// budget (max_tokens * 0.99). Adaptive path passes "max"
    /// verbatim into `output_config.effort` and never calls
    /// `effort_ratio`. This test pins the legacy mapping so a
    /// non-adaptive provider receiving `max` from the canonical
    /// surface still produces a serializable body.
    #[test]
    fn effort_max_maps_to_99_percent_legacy_path() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-sonnet-4-6".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1000),
            reasoning: Some(ReasoningConfig {
                effort: Some("max".into()),
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, false, &[]).unwrap();
        let thinking = body.get("thinking").unwrap();
        assert_eq!(thinking["type"], "enabled");
        // 1000 * 0.99 = 990
        assert_eq!(thinking["budget_tokens"], 990);
    }

    /// FX-1: `reasoning.effort = "none"` produces `Disabled` on both
    /// paths. The adaptive flag does not coerce a Disabled into an
    /// Adaptive -- if the caller said no thinking, we honor it.
    #[test]
    fn disabled_thinking_unchanged_under_adaptive_flag() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(512),
            reasoning: Some(ReasoningConfig {
                effort: Some("none".into()),
                max_tokens: None,
                exclude: None,
                enabled: None,
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, true, &[]).unwrap();
        let thinking = body.get("thinking").unwrap();
        assert_eq!(thinking["type"], "disabled");
        assert!(body.get("output_config").is_none());
    }

    /// FX-1: the barefoot adaptive case -- `reasoning.enabled = true`
    /// with no effort and no budget. Adaptive shape applies; effort
    /// defaults to "medium". This is the only path where
    /// `derive_effort` returns the fallback string, so we pin it
    /// explicitly. (Without this test the default would silently
    /// drift if anyone changed `derive_effort`.)
    #[test]
    fn adaptive_thinking_defaults_effort_to_medium_when_unset() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1024),
            reasoning: Some(ReasoningConfig {
                effort: None,
                max_tokens: None,
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, true, &[]).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "medium");
    }

    /// FX-1: when `adaptive_thinking = true` AND the caller sets an
    /// explicit `reasoning.max_tokens`, the budget is dropped (the
    /// adaptive wire shape has no field for it) and a tracing::warn
    /// fires at normalize time. We can't easily assert the warn in a
    /// unit test without `tracing-test`, but we CAN pin that the
    /// resulting body is the adaptive shape with the caller's
    /// effort string (or "medium" fallback), with no budget_tokens
    /// leaking into the wire.
    #[test]
    fn adaptive_thinking_drops_max_tokens_silently() {
        use routectl_core::ReasoningConfig;
        let req = ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_msg("hi")],
            max_tokens: Some(1024),
            reasoning: Some(ReasoningConfig {
                effort: Some("low".into()),
                max_tokens: Some(8000),
                exclude: None,
                enabled: Some(true),
            }),
            ..Default::default()
        };
        let body = normalize("test-anthropic", &req, true, &[]).unwrap();
        assert_eq!(body["thinking"]["type"], "adaptive");
        // budget_tokens MUST NOT leak into the adaptive shape.
        assert!(
            body["thinking"].get("budget_tokens").is_none(),
            "adaptive shape must not carry budget_tokens, got {body:?}"
        );
        // The caller's effort string survives even though the budget
        // was dropped.
        assert_eq!(body["output_config"]["effort"], "low");
    }

    /// Tool-choice translation lives in the egress (different upstreams
    /// want different shapes; the OpenAI ingress passes wire `tool_choice`
    /// through verbatim). Pin the canonical -> Anthropic mapping for
    /// every shape we expect callers to send.
    #[test]
    fn tool_choice_string_auto_translates_to_anthropic_object() {
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            tool_choice: Some(json!("auto")),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[]).unwrap();
        assert_eq!(body["tool_choice"], json!({"type":"auto"}));
    }

    #[test]
    fn tool_choice_string_required_translates_to_any() {
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            tool_choice: Some(json!("required")),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[]).unwrap();
        assert_eq!(body["tool_choice"], json!({"type":"any"}));
    }

    #[test]
    fn tool_choice_string_none_drops_field() {
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            tool_choice: Some(json!("none")),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[]).unwrap();
        assert!(
            body.get("tool_choice").is_none() || body["tool_choice"].is_null(),
            "expected tool_choice dropped, got: {body:?}"
        );
        assert!(
            body.get("tools").is_none() || body["tools"].is_null(),
            "expected no tools field when caller sent neither tools nor tool_choice"
        );
    }

    /// `tool_choice = "none"` plus `tools` present must drop BOTH on the
    /// Anthropic wire. Anthropic has no native "none" -- if we send the
    /// tools but no tool_choice, Anthropic defaults to auto-select and
    /// the caller's "do not call tools" intent silently flips to "auto".
    #[test]
    fn tool_choice_none_with_tools_strips_tools_too() {
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            tool_choice: Some(json!("none")),
            tools: Some(vec![routectl_core::ToolDef::Custom(
                routectl_core::CustomTool {
                    name: "get_weather".into(),
                    description: Some("weather lookup".into()),
                    input_schema: json!({"type":"object"}),
                    cache_control: None,
                    defer_loading: None,
                    strict: None,
                    type_tag: None,
                },
            )]),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[]).unwrap();
        assert!(
            body.get("tool_choice").is_none() || body["tool_choice"].is_null(),
            "expected tool_choice dropped, got: {body:?}"
        );
        assert!(
            body.get("tools").is_none() || body["tools"].is_null(),
            "expected tools dropped alongside tool_choice=none, got: {body:?}"
        );
    }

    #[test]
    fn tool_choice_function_object_translates_to_anthropic_tool() {
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            tool_choice: Some(json!({"type":"function","function":{"name":"get_weather"}})),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[]).unwrap();
        assert_eq!(
            body["tool_choice"],
            json!({"type":"tool","name":"get_weather"})
        );
    }

    /// Anthropic-shape tool_choice (e.g. from claude-code via Anthropic
    /// ingress) must passthrough verbatim. Without this, the Anthropic
    /// ingress -> Anthropic egress path would double-translate and
    /// silently corrupt the field.
    #[test]
    fn tool_choice_already_anthropic_shape_passes_through_verbatim() {
        for tc in [
            json!({"type":"auto"}),
            json!({"type":"any"}),
            json!({"type":"tool","name":"X"}),
            json!({"type":"none"}),
        ] {
            let req = ChatRequest {
                model: "claude-sonnet-4-5-20250929".into(),
                messages: vec![user_msg("hi")],
                tool_choice: Some(tc.clone()),
                ..Default::default()
            };
            let body = normalize("test", &req, false, &[]).unwrap();
            assert_eq!(body["tool_choice"], tc, "expected passthrough for {tc:?}");
        }
    }

    /// Unknown shapes are not coerced; let the upstream surface its
    /// own error. The OpenAI ingress still passes them through the
    /// canonical body, so the egress sees them here.
    #[test]
    fn tool_choice_unknown_object_passes_through_verbatim() {
        let weird = json!({"type":"some_future_mode","extra":"bag"});
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            tool_choice: Some(weird.clone()),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[]).unwrap();
        assert_eq!(body["tool_choice"], weird);
    }

    /// `output_config` arriving via `provider_extras` (the path used
    /// by the Anthropic ingress for structured-output requests) is
    /// merged into the upstream body so `output_config.format` reaches
    /// api.anthropic.com unchanged. The egress doesn't need a
    /// dedicated field for this -- the provider_extras allow-list
    /// already lets `output_config` through.
    #[test]
    fn structured_output_format_merges_from_provider_extras() {
        let req = ChatRequest {
            model: "claude-sonnet-4-5-20250929".into(),
            messages: vec![user_msg("hi")],
            provider_extras: Some(json!({
                "output_config": {
                    "format": {
                        "type": "json_schema",
                        "schema": {"type": "object"}
                    }
                }
            })),
            ..Default::default()
        };
        let body = normalize("test", &req, false, &[]).unwrap();
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert_eq!(body["output_config"]["format"]["schema"]["type"], "object");
    }
}
