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
    ChatRequest, ContentPart, CustomTool, Error, KnownContentPart, Message, MessageContent,
    ReasoningDetailKind, Result, Role, SystemContent, ToolDef,
};

use super::types::{
    AnthropicContent, AnthropicMessage, AnthropicRequest, AnthropicRole, AnthropicSystem,
    AnthropicSystemBlock, AnthropicTool, ContentBlock, ThinkingConfig,
};

const DEFAULT_MAX_TOKENS: u32 = 4096;
const ANTHROPIC_FORMAT: &str = "anthropic-claude-v1";

/// Proportional budget_tokens as fraction of max_tokens per effort level.
fn effort_ratio(effort: &str) -> f64 {
    match effort {
        "xhigh" => 0.95,
        "high" => 0.80,
        "medium" => 0.50,
        "low" => 0.20,
        "minimal" => 0.10,
        _ => 0.50,
    }
}

fn build_thinking(req: &ChatRequest) -> Option<ThinkingConfig> {
    let r = req.reasoning.as_ref()?;

    if r.enabled == Some(false) {
        return Some(ThinkingConfig::Disabled);
    }
    if let Some(budget) = r.max_tokens {
        return Some(ThinkingConfig::Enabled {
            budget_tokens: budget,
        });
    }
    if let Some(effort) = r.effort.as_deref() {
        if effort == "none" {
            return Some(ThinkingConfig::Disabled);
        }
        let max = req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
        let budget = ((max as f64) * effort_ratio(effort)).max(1.0) as u32;
        return Some(ThinkingConfig::Enabled {
            budget_tokens: budget,
        });
    }
    if r.enabled == Some(true) {
        let max = req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
        let budget = (max / 2).max(1);
        return Some(ThinkingConfig::Enabled {
            budget_tokens: budget,
        });
    }
    None
}

// ---------------------------------------------------------------------------
// System
// ---------------------------------------------------------------------------

/// Convert canonical `SystemContent` to wire `AnthropicSystem`. Preserves
/// per-block cache_control and citations.
fn translate_system(s: &SystemContent) -> AnthropicSystem {
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
fn lift_legacy_system(messages: &[Message]) -> Option<AnthropicSystem> {
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

fn translate_tool(td: &ToolDef) -> AnthropicTool {
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

// ---------------------------------------------------------------------------
// Content blocks
// ---------------------------------------------------------------------------

/// Translate one canonical ContentPart into a wire ContentBlock.
/// Forward-compat: ContentPart::Other passes through verbatim as
/// ContentBlock::Other so the Anthropic-in / Anthropic-out path keeps
/// working when Anthropic ships a new block type.
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
        // OpenAI-shape ImageUrl translates to an Anthropic image block
        // when the URL is HTTPS-direct. base64 data URIs are a separate
        // shape and we don't synthesize them here -- callers should use
        // KnownContentPart::Image for that.
        KnownContentPart::ImageUrl {
            image_url,
            cache_control,
        } => {
            let url = image_url.get("url").cloned().unwrap_or(Value::Null);
            ContentBlock::Image {
                source: json!({"type": "url", "url": url}),
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
    if msg.reasoning_details.is_empty() {
        // No multi-turn reasoning to thread back; fall through to the
        // generic content translation (Text or Parts).
        return Ok(translate_simple_content(&msg.content));
    }

    let mut blocks: Vec<ContentBlock> = Vec::new();

    // Emit reasoning blocks first (in index order).
    let mut details = msg.reasoning_details.clone();
    details.sort_by_key(|d| d.index.unwrap_or(0));

    for detail in &details {
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
                    .ok_or_else(|| {
                        Error::normalize_request(
                            id,
                            "thinking block missing signature in reasoning_details",
                        )
                    })?
                    .to_string();
                blocks.push(ContentBlock::Thinking {
                    thinking,
                    signature,
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

    // Append text or parts from message content. For a Text body, emit
    // a single Text block; for typed Parts, translate each.
    match &msg.content {
        MessageContent::Text(t) if !t.is_empty() => blocks.push(ContentBlock::Text {
            text: t.clone(),
            cache_control: None,
        }),
        MessageContent::Text(_) | MessageContent::Null => {}
        MessageContent::Parts(parts) => {
            for p in parts {
                blocks.push(translate_content_part(p));
            }
        }
    }

    Ok(AnthropicContent::Blocks(blocks))
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

pub fn normalize(id: &str, req: &ChatRequest) -> Result<Value> {
    let max_tokens = req.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS);
    let thinking = build_thinking(req);

    // System: prefer req.system (canonical); fall back to lifting any
    // Role::System messages for direct callers that bypass an ingress.
    let system = req
        .system
        .as_ref()
        .map(translate_system)
        .or_else(|| lift_legacy_system(&req.messages));

    // Translate non-System messages.
    let mut anthropic_messages: Vec<AnthropicMessage> = Vec::new();
    for msg in &req.messages {
        match msg.role {
            Role::System => {
                // Already handled via req.system / lift_legacy_system.
                // Drop here (do not duplicate in the messages array).
            }
            Role::User => {
                anthropic_messages.push(AnthropicMessage {
                    role: AnthropicRole::User,
                    content: translate_simple_content(&msg.content),
                });
            }
            Role::Assistant => {
                let content = build_assistant_content(id, msg)?;
                anthropic_messages.push(AnthropicMessage {
                    role: AnthropicRole::Assistant,
                    content,
                });
            }
            Role::Tool => {
                anthropic_messages.push(build_tool_message(msg));
            }
        }
    }

    let tools = req
        .tools
        .as_ref()
        .map(|ts| ts.iter().map(translate_tool).collect::<Vec<_>>());

    // Anthropic requires temperature = 1.0 when thinking is enabled.
    let temperature = match &thinking {
        Some(ThinkingConfig::Enabled { .. }) => Some(1.0f64),
        _ => req.temperature,
    };

    let ar = AnthropicRequest {
        model: req.model.clone(),
        messages: anthropic_messages,
        max_tokens,
        system,
        thinking,
        temperature,
        top_p: req.top_p,
        stop_sequences: req.stop.clone(),
        stream: None, // caller sets this
        tools,
        tool_choice: req.tool_choice.clone(),
        cache_control: req.cache_control.clone(),
        anthropic_beta: req.anthropic_beta.clone(),
    };

    // Catch breakpoint-cap and TTL-ordering bugs in CI before they
    // reach upstream. Debug-assert keeps non-debug builds fast; when
    // the ingress runs validate at parse time, debug-only is enough
    // here as a defense in depth.
    // Belt-and-braces: validate in release too. The Anthropic ingress
    // already runs this at parse time; running it again here catches
    // direct callers (library users without an ingress) and protects
    // upstream from cap/ordering violations regardless of build mode.
    validate_breakpoints(&ar)?;

    let mut body =
        serde_json::to_value(&ar).map_err(|e| Error::normalize_request(id, e.to_string()))?;

    // Merge provider_extras last (caller wins). The merge has an
    // allow-list: routectl-managed top-level keys cannot be stomped
    // by a malicious or careless `provider_extras` value. This was
    // an architecture-review finding (MEDIUM-1) -- without the
    // allow-list, a request with `provider_extras = {"messages":
    // [{"role":"user","content":"INJECTED"}]}` would replace the
    // assembled messages array.
    if let Some(extras) = req.provider_extras.as_ref() {
        if let (Some(obj), Some(extra_obj)) = (body.as_object_mut(), extras.as_object()) {
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
    }

    Ok(body)
}

/// Top-level Anthropic body keys constructed by routectl that
/// `provider_extras` is NOT permitted to override. Anthropic-only
/// extras like `top_k`, `service_tier`, `output_config`, `container`,
/// `inference_geo` are still allowed through (they're how the ingress
/// forwards request fields canonical doesn't know about).
fn is_routectl_managed_key(key: &str) -> bool {
    matches!(
        key,
        "model"
            | "messages"
            | "system"
            | "max_tokens"
            | "thinking"
            | "tools"
            | "tool_choice"
            | "stream"
            | "stop_sequences"
            | "temperature"
            | "top_p"
            | "anthropic_beta"
            | "cache_control"
    )
}
