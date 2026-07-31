//! Canonical -> Gemini `generateContent` request body translation.
//!
//! Translates a `ChatRequest` into a `GenerateContentRequest`.
//! Gemini's message model differs from OpenAI/Anthropic:
//!
//!   - System messages: collected into `systemInstruction.parts` (no role).
//!   - User messages: `contents` entry with role "user".
//!   - Assistant messages: `contents` entry with role "model".
//!   - Tool result messages: a user-turn `contents` entry with role "user"
//!     carrying `functionResponse` parts. Gemini receives tool results as a
//!     user turn (not a separate "tool" role).

use std::collections::HashMap;

use serde_json::Value;

use routectl_core::cache_control::{BreakpointPosition, CacheBreakpointSource};
use routectl_core::{ChatRequest, ReasoningDetail, Result};
use routectl_core::{ContentPart, KnownContentPart, MessageContent, Role, ToolDef};

use super::GEMINI_FORMAT;
use super::schema::clean_schema;
use super::types::{
    Content, FunctionCallPart, FunctionCallingConfig, FunctionDeclaration, FunctionResponsePart,
    GeminiTool, GenerateContentRequest, GenerationConfig, InlineData, Part, SystemInstruction,
    ThinkingConfig, ToolConfig,
};

/// Build a `GenerateContentRequest` from a canonical `ChatRequest`.
///
/// The config's `id` is used only for error attribution.
pub fn translate(provider_id: &str, req: &ChatRequest) -> Result<GenerateContentRequest> {
    warn_dropped_cache_control(provider_id, req);
    // Responses-dialect reasoning context/mode has no Gemini home (it is
    // dropped as a managed key in merge_payload_extras); WARN once so the
    // loss isn't silent.
    crate::responses_reasoning_guard::warn_dropped_reasoning_dialect(provider_id, req);
    let system_instruction = build_system_instruction(req);
    let contents = build_contents(provider_id, req)?;
    let (tools, tool_config) = build_tools_and_config(provider_id, req);
    let generation_config = build_generation_config(req);

    Ok(GenerateContentRequest {
        contents,
        system_instruction,
        tools,
        tool_config,
        generation_config,
    })
}

/// Cache-prefix surfaces carrying a caller `cache_control` marker that the
/// Gemini egress drops. Gemini uses implicit prefix caching (automatic, with
/// no caller-controllable breakpoint surface), so every supplied marker is
/// dropped -- this only names which surfaces carried one. Pure function of
/// `req`: no logging, no mutation, so the detection can be unit-tested
/// directly. Unlike the openai-responses egress, `system` is NOT excluded --
/// Gemini's system-instruction assembly drops the marker without its own log,
/// so reporting it here is the only breadcrumb the operator gets.
fn dropped_cache_surfaces(req: &ChatRequest) -> Vec<&'static str> {
    let mut surfaces: Vec<&'static str> = Vec::new();
    for bp in req.cache_breakpoints() {
        let name = match bp.position {
            BreakpointPosition::Tools => "tools",
            BreakpointPosition::System => "system",
            BreakpointPosition::Messages => "messages",
            BreakpointPosition::TopLevel => "top-level",
        };
        if !surfaces.contains(&name) {
            surfaces.push(name);
        }
    }
    surfaces
}

/// Emit one WARN naming every cache-prefix surface carrying a caller
/// `cache_control` marker that the Gemini egress drops. Matches the
/// openai-compat / openai-responses egress convention so an operator routing
/// cache-hinted traffic to a Gemini target sees the same breadcrumb on this
/// leg as on the others. Logs only the surface name(s) + a count: no message
/// content, no bodies, no secrets.
fn warn_dropped_cache_control(provider_id: &str, req: &ChatRequest) {
    let surfaces = dropped_cache_surfaces(req);
    if surfaces.is_empty() {
        return;
    }
    tracing::warn!(
        provider = %provider_id,
        dropped_surfaces = ?surfaces,
        dropped_count = surfaces.len(),
        "gemini egress: cache_control dropped (Gemini uses implicit prefix \
         caching, no breakpoint surface)"
    );
}

// ---------------------------------------------------------------------------
// System instruction
// ---------------------------------------------------------------------------

fn build_system_instruction(req: &ChatRequest) -> Option<SystemInstruction> {
    let mut texts: Vec<String> = Vec::new();

    // Collect system-role messages.
    for msg in &*req.messages {
        if !matches!(msg.role, Role::System) {
            continue;
        }
        match &msg.content {
            MessageContent::Text(t) => texts.push(t.clone()),
            MessageContent::Parts(parts) => {
                for p in parts {
                    if let Some(t) = extract_text_from_part(p) {
                        texts.push(t);
                    }
                }
            }
            MessageContent::Null => {}
        }
    }

    // Lift top-level `system` field if present (Anthropic ingress path).
    use routectl_core::SystemContent;
    if let Some(system) = &req.system {
        match system {
            SystemContent::Text(t) => texts.push(t.clone()),
            SystemContent::Blocks(blocks) => {
                for block in blocks {
                    if !block.text.is_empty() {
                        texts.push(block.text.clone());
                    }
                }
            }
        }
    }

    if texts.is_empty() {
        return None;
    }
    Some(SystemInstruction {
        parts: texts.into_iter().map(text_part).collect(),
    })
}

// ---------------------------------------------------------------------------
// Contents array
// ---------------------------------------------------------------------------

fn build_contents(provider_id: &str, req: &ChatRequest) -> Result<Vec<Content>> {
    let mut contents: Vec<Content> = Vec::new();
    let tool_name_by_id = build_tool_call_name_index(req);

    for msg in &*req.messages {
        match msg.role {
            Role::System => {
                // Lifted into systemInstruction above; skip here.
            }
            Role::User => {
                let parts = content_to_parts(provider_id, &msg.content, &tool_name_by_id)?;
                if !parts.is_empty() {
                    contents.push(Content {
                        role: "user".into(),
                        parts,
                    });
                }
            }
            Role::Assistant => {
                // Reasoning replay: Gemini-origin thinking is echoed back
                // as thought parts (carrying the thoughtSignature) ahead of
                // the visible output, so the model can continue its prior
                // chain-of-thought. Foreign-provider reasoning is skipped.
                let mut parts = reasoning_details_to_thought_parts(&msg.reasoning_details);
                // A functionCall can reach this turn from either tool-call
                // shape: an Anthropic-canonical `ToolUse` content-part (handled
                // in `content_to_parts`) or the OpenAI-shape `tool_calls` array.
                parts.extend(content_to_parts(
                    provider_id,
                    &msg.content,
                    &tool_name_by_id,
                )?);
                if let Some(tool_calls_raw) = &msg.tool_calls {
                    for tc in tool_calls_raw {
                        if let Some(p) = tool_call_to_function_call_part(provider_id, tc)? {
                            parts.push(p);
                        }
                    }
                }
                inject_skip_signature_sentinel(&req.model, &mut parts);
                if !parts.is_empty() {
                    contents.push(Content {
                        role: "model".into(),
                        parts,
                    });
                }
            }
            Role::Tool => {
                // Tool results come back as a user-turn functionResponse.
                // Gemini does not have a separate "tool" role -- the
                // tool result is a user turn carrying functionResponse parts.
                // Gemini correlates functionResponse -> functionCall BY NAME,
                // but OpenAI tool-role / Anthropic tool_result messages carry
                // only a correlation id, not the tool name. Recover the name
                // from the prior assistant tool call keyed on that id.
                let tool_name = recover_tool_name(
                    provider_id,
                    msg.tool_call_id.as_deref(),
                    &tool_name_by_id,
                    msg.name.as_deref(),
                );
                let response_content = match &msg.content {
                    MessageContent::Text(t) => {
                        serde_json::json!({"content": t})
                    }
                    MessageContent::Parts(parts) => {
                        let texts: Vec<String> =
                            parts.iter().filter_map(extract_text_from_part).collect();
                        serde_json::json!({"content": texts.join("\n")})
                    }
                    MessageContent::Null => serde_json::json!({}),
                };
                contents.push(Content {
                    role: "user".into(),
                    parts: vec![function_response_part(FunctionResponsePart {
                        name: tool_name,
                        response: response_content,
                    })],
                });
            }
        }
    }

    Ok(contents)
}

/// Build a `correlation-id -> tool-name` index over every tool call in the
/// request. Gemini keys `functionResponse` on the tool NAME, but cross-dialect
/// tool loops (OpenAI tool-role, Anthropic tool_result) carry only the
/// correlation id on the result message. Both assistant tool-call shapes are
/// indexed: the OpenAI `tool_calls` array (`{id, function:{name}}`) and
/// Anthropic `tool_use` content blocks (`{id, name}`).
fn build_tool_call_name_index(req: &ChatRequest) -> HashMap<String, String> {
    let mut index: HashMap<String, String> = HashMap::new();
    for msg in &*req.messages {
        if !matches!(msg.role, Role::Assistant) {
            continue;
        }
        if let Some(tool_calls) = &msg.tool_calls {
            for tc in tool_calls {
                let id = tc.get("id").and_then(Value::as_str);
                let name = tc
                    .get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str);
                if let (Some(id), Some(name)) = (id, name)
                    && !id.is_empty()
                    && !name.is_empty()
                {
                    index
                        .entry(id.to_string())
                        .or_insert_with(|| name.to_string());
                }
            }
        }
        if let MessageContent::Parts(parts) = &msg.content {
            for part in parts {
                if let ContentPart::Known(KnownContentPart::ToolUse { id, name, .. }) = part
                    && !id.is_empty()
                    && !name.is_empty()
                {
                    index.entry(id.clone()).or_insert_with(|| name.clone());
                }
            }
        }
    }
    index
}

/// Resolve the Gemini `functionResponse.name` for a tool-result message.
/// Prefer a name recovered from the prior tool call keyed on the correlation
/// id; fall back to any name the ingress carried; last, an empty name with a
/// WARN (Gemini cannot correlate a nameless functionResponse).
fn recover_tool_name(
    provider_id: &str,
    correlation_id: Option<&str>,
    tool_name_by_id: &HashMap<String, String>,
    carried_name: Option<&str>,
) -> String {
    if let Some(id) = correlation_id
        && let Some(name) = tool_name_by_id.get(id)
    {
        return name.clone();
    }
    if let Some(name) = carried_name.filter(|n| !n.is_empty()) {
        return name.to_string();
    }
    tracing::warn!(
        provider = %provider_id,
        correlation_id = %correlation_id.unwrap_or_default(),
        "gemini: could not recover tool name for functionResponse; Gemini may fail to correlate"
    );
    String::new()
}

fn content_to_parts(
    provider_id: &str,
    content: &MessageContent,
    tool_name_by_id: &HashMap<String, String>,
) -> Result<Vec<Part>> {
    match content {
        MessageContent::Text(t) => {
            if t.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(vec![text_part(t.clone())])
            }
        }
        MessageContent::Parts(parts) => {
            let mut out = Vec::new();
            for p in parts {
                if let Some(part) = content_part_to_part(provider_id, p, tool_name_by_id)? {
                    out.push(part);
                }
            }
            Ok(out)
        }
        MessageContent::Null => Ok(Vec::new()),
    }
}

fn content_part_to_part(
    provider_id: &str,
    part: &ContentPart,
    tool_name_by_id: &HashMap<String, String>,
) -> Result<Option<Part>> {
    match part {
        ContentPart::Known(known) => match known {
            KnownContentPart::Text { text, .. } => Ok(Some(text_part(text.clone()))),
            KnownContentPart::Image { source, .. } => {
                // Anthropic image source: {type:"base64", media_type, data}
                // Map to inlineData.
                let mime = source
                    .get("media_type")
                    .and_then(Value::as_str)
                    .unwrap_or("image/jpeg")
                    .to_string();
                let data = source
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                Ok(Some(inline_data_part(InlineData {
                    mime_type: mime,
                    data,
                })))
            }
            KnownContentPart::ImageUrl { image_url, .. } => {
                // OpenAI image_url: {url} -- Gemini inlineData needs base64.
                // Gemini also supports urls via fileData. For now emit text
                // with the URL as a best-effort passthrough; Gemini does not
                // natively accept arbitrary remote URLs in inlineData.
                let url = image_url
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                // If it's a data URI (data:mime/type;base64,...), extract.
                if let Some(stripped) = url.strip_prefix("data:")
                    && let Some(semi) = stripped.find(';')
                {
                    let mime = &stripped[..semi];
                    let rest = &stripped[semi + 1..];
                    if let Some(b64) = rest.strip_prefix("base64,") {
                        return Ok(Some(inline_data_part(InlineData {
                            mime_type: mime.to_string(),
                            data: b64.to_string(),
                        })));
                    }
                }
                // Non-data URL: pass as text (best-effort).
                Ok(Some(text_part(url.to_string())))
            }
            KnownContentPart::ToolUse {
                id: _, name, input, ..
            } => {
                // Assistant ToolUse block -> functionCall part.
                Ok(Some(function_call_part(FunctionCallPart {
                    name: name.clone(),
                    args: input.clone(),
                })))
            }
            KnownContentPart::ToolResult {
                content,
                tool_use_id,
                ..
            } => {
                // ToolResult in a parts array -> functionResponse. Gemini
                // correlates by name, which the block does not carry; recover
                // it from the prior tool call keyed on tool_use_id.
                let name = recover_tool_name(
                    provider_id,
                    Some(tool_use_id.as_str()),
                    tool_name_by_id,
                    None,
                );
                Ok(Some(function_response_part(FunctionResponsePart {
                    name,
                    response: content.clone(),
                })))
            }
            KnownContentPart::Thinking {
                thinking,
                signature,
            } => {
                // Assistant reasoning replayed back as a thought part so
                // the model can continue its chain-of-thought. The
                // signature is Gemini's `thoughtSignature` from a prior
                // turn (when this reasoning originated from Gemini).
                Ok(Some(thought_part(thinking.clone(), signature.clone())))
            }
            KnownContentPart::RedactedThinking { .. } => {
                // Redacted blocks have no text we can forward; drop silently.
                Ok(None)
            }
            KnownContentPart::File { file, .. } => {
                // OpenAI file block: try to extract inline base64 content.
                let filename = file
                    .get("file")
                    .and_then(|f| f.get("filename"))
                    .and_then(Value::as_str)
                    .unwrap_or("file");
                let data = file
                    .get("file")
                    .and_then(|f| f.get("file_data"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let mime = mime_from_filename(filename);
                Ok(Some(inline_data_part(InlineData {
                    mime_type: mime.to_string(),
                    data: data.to_string(),
                })))
            }
            KnownContentPart::Document { source, .. } => {
                // Anthropic document: extract text or base64.
                let text = source.get("text").and_then(Value::as_str);
                if let Some(t) = text {
                    return Ok(Some(text_part(t.to_string())));
                }
                let data = source
                    .get("data")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let mime = source
                    .get("media_type")
                    .and_then(Value::as_str)
                    .unwrap_or("application/pdf");
                Ok(Some(inline_data_part(InlineData {
                    mime_type: mime.to_string(),
                    data: data.to_string(),
                })))
            }
        },
        ContentPart::Other {
            type_tag, extras, ..
        } => {
            // Unknown block type: log and skip to keep the request valid.
            tracing::debug!(
                provider = %provider_id,
                type_tag = %type_tag,
                "gemini: skipping unknown content block type"
            );
            let _ = extras;
            Ok(None)
        }
    }
}

fn extract_text_from_part(part: &ContentPart) -> Option<String> {
    match part {
        ContentPart::Known(KnownContentPart::Text { text, .. }) => Some(text.clone()),
        _ => None,
    }
}

/// Convert an OpenAI-shape tool_call JSON value to a `functionCall` Part.
///
/// The canonical `Message.tool_calls` field carries `Vec<Value>` in the
/// OpenAI shape: `{id, type:"function", function:{name, arguments}}`.
fn tool_call_to_function_call_part(provider_id: &str, tc: &Value) -> Result<Option<Part>> {
    let func = if let Some(f) = tc.get("function") {
        f
    } else {
        tracing::debug!(
            provider = %provider_id,
            "gemini: tool_call missing 'function' field; skipping"
        );
        return Ok(None);
    };
    let name = func
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // `arguments` is a JSON string in the OpenAI shape; parse it into an object.
    let args_str = func
        .get("arguments")
        .and_then(Value::as_str)
        .unwrap_or("{}");
    let args: Value = serde_json::from_str(args_str).unwrap_or_else(|_| {
        tracing::debug!(
            provider = %provider_id,
            fn_name = %name,
            "gemini: could not parse tool_call arguments as JSON; using empty object"
        );
        serde_json::json!({})
    });
    // A native-Gemini tool turn round-trips a real thoughtSignature that the
    // response translator captured directly onto the tool_call value. Preserve
    // it verbatim so replay continues the model's chain-of-thought; foreign
    // history has none, and the caller may inject the skip-validation sentinel.
    let thought_signature = tc
        .get("thought_signature")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(std::string::ToString::to_string);
    let mut part = function_call_part(FunctionCallPart { name, args });
    part.thought_signature = thought_signature;
    Ok(Some(part))
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// Sentinel `thoughtSignature` that tells Gemini-3+ to skip signature
/// validation on a replayed `functionCall` whose tool history originated from a
/// non-Gemini provider (no genuine signature to preserve). Byte-identical to
/// the value the tensorzero and adk-python gateways inject.
const SKIP_THOUGHT_SIGNATURE_VALIDATOR: &str = "skip_thought_signature_validator";

/// Inject the skip-validation sentinel onto the FIRST `functionCall` part of a
/// replayed model turn when the target is Gemini-3+ and that part carries no
/// genuine captured signature.
///
/// Gemini-3+ validates a `thoughtSignature` on every replayed `functionCall`.
/// Native-Gemini history carries a real signature (captured by the response
/// translator onto the tool_call it round-trips); foreign history -- whether it
/// arrived as an OpenAI-shape `tool_calls` array or an Anthropic-canonical
/// `ToolUse` content-part -- carries none, so a single sentinel on the first
/// functionCall of the turn lets Gemini accept the unsigned foreign call.
/// Parallel calls after the first do not need it. Gemini-2 must NOT get the
/// sentinel -- a synthetic signature there opens a new reject path. Operates on
/// the fully-assembled parts so BOTH tool-call sources are covered by one pass.
fn inject_skip_signature_sentinel(model: &str, parts: &mut [Part]) {
    if gemini_generation(model).is_none_or(|g| g < THINKING_LEVEL_MIN_GENERATION) {
        return;
    }
    if let Some(first_fc) = parts.iter_mut().find(|p| p.function_call.is_some())
        && first_fc.thought_signature.is_none()
    {
        first_fc.thought_signature = Some(SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string());
    }
}

fn build_tools_and_config(
    provider_id: &str,
    req: &ChatRequest,
) -> (Option<Vec<GeminiTool>>, Option<ToolConfig>) {
    let tool_defs = match &req.tools {
        Some(t) if !t.is_empty() => t,
        _ => return (None, None),
    };

    let mut declarations: Vec<FunctionDeclaration> = Vec::new();
    for def in tool_defs {
        match def {
            ToolDef::Custom(custom) => {
                declarations.push(FunctionDeclaration {
                    name: custom.name.clone(),
                    description: custom.description.clone(),
                    parameters: Some(clean_schema(&custom.input_schema)),
                });
            }
            ToolDef::Other(v) => {
                // OpenAI-shape: {type:"function", function:{name,description,parameters}}
                let func = v.get("function").unwrap_or(v);
                let name = func.get("name").and_then(Value::as_str).unwrap_or_default();
                if name.is_empty() {
                    // Hosted / unknown tool shape (web_search, file_search,
                    // codex namespaces) carries no usable function name. Gemini
                    // rejects unknown tool shapes, so forwarding verbatim would
                    // turn a silent drop into a loud 400; an empty-named
                    // declaration is a silently broken tool the model may call.
                    // Skip with a structured WARN, mirroring openai-compat.
                    let kind = v.get("type").and_then(Value::as_str).unwrap_or("unknown");
                    tracing::warn!(
                        provider = %provider_id,
                        tool_type = %kind,
                        "gemini: skipping tool def with no usable function name; \
                         hosted / unknown tool shapes are not representable"
                    );
                    continue;
                }
                let description = func
                    .get("description")
                    .and_then(Value::as_str)
                    .map(std::string::ToString::to_string);
                let parameters = func.get("parameters").map(clean_schema);
                declarations.push(FunctionDeclaration {
                    name: name.to_string(),
                    description,
                    parameters,
                });
            }
        }
    }

    if declarations.is_empty() {
        // No usable declarations survive (e.g. every ToolDef was a nameless
        // hosted-tool shape). Emitting a `toolConfig` with no `tools` makes
        // Gemini reject the request ("Function calling config is set without
        // function_declarations"), so omit BOTH. Warn only when a tool_choice
        // was set, since that is the intent being dropped.
        if req.tool_choice.is_some() {
            tracing::warn!(
                provider = %provider_id,
                "gemini: no tool declarations survived; dropping tool_choice \
                 (a toolConfig with no functionDeclarations is rejected)"
            );
        }
        return (None, None);
    }

    // Reconcile the choice against the declarations that actually survived.
    let surviving_names: Vec<&str> = declarations.iter().map(|d| d.name.as_str()).collect();
    let tool_config = build_tool_config(provider_id, req.tool_choice.as_ref(), &surviving_names);
    let tools = Some(vec![GeminiTool {
        function_declarations: declarations,
    }]);
    (tools, tool_config)
}

fn build_tool_config(
    provider_id: &str,
    tool_choice: Option<&Value>,
    surviving_names: &[&str],
) -> Option<ToolConfig> {
    let choice = tool_choice?;

    let mode = if choice.is_string() {
        match choice.as_str().unwrap_or("auto") {
            "none" => "NONE",
            "required" => "ANY",
            _ => "AUTO",
        }
    } else if let Some(obj) = choice.as_object() {
        // {type:"function", function:{name}} shape
        if obj.get("type").and_then(Value::as_str) == Some("function") {
            "ANY"
        } else {
            "AUTO"
        }
    } else {
        "AUTO"
    };

    let allowed_function_names = if mode == "ANY" {
        if let Some(func) = choice.get("function") {
            func.get("name")
                .and_then(Value::as_str)
                .map(|n| vec![n.to_string()])
        } else {
            None
        }
    } else {
        None
    };

    // A forced tool_choice that names a tool with no surviving declaration
    // would point Gemini at a function it never received. Drop the forcing
    // (omit toolConfig, letting the model default to AUTO over the surviving
    // tools) rather than emit a config Gemini rejects.
    if let Some(names) = &allowed_function_names
        && names.iter().any(|n| !surviving_names.contains(&n.as_str()))
    {
        tracing::warn!(
            provider = %provider_id,
            forced = ?names,
            "gemini: tool_choice forced a tool with no surviving declaration; \
             dropping the forcing"
        );
        return None;
    }

    Some(ToolConfig {
        function_calling_config: FunctionCallingConfig {
            mode: mode.to_string(),
            allowed_function_names,
        },
    })
}

// ---------------------------------------------------------------------------
// GenerationConfig
// ---------------------------------------------------------------------------

fn build_generation_config(req: &ChatRequest) -> Option<GenerationConfig> {
    let thinking_config = build_thinking_config(req);
    let (response_mime_type, response_schema) = build_response_format(req);

    let has_any = req.temperature.is_some()
        || req.top_p.is_some()
        || req.max_tokens.is_some()
        || req.stop.is_some()
        || thinking_config.is_some()
        || response_mime_type.is_some();

    if !has_any {
        return None;
    }

    // top_k is not in the canonical schema; only emit via provider_extras.
    Some(GenerationConfig {
        temperature: req.temperature,
        top_p: req.top_p,
        top_k: None,
        max_output_tokens: req.max_tokens,
        stop_sequences: req.stop.clone(),
        response_mime_type,
        response_schema,
        thinking_config,
    })
}

/// Dynamic-budget sentinel: tells Gemini to size the thinking budget
/// itself. Used when reasoning is enabled without an explicit budget or
/// effort level.
const THINKING_BUDGET_DYNAMIC: i32 = -1;

/// First Gemini generation that replaced the numeric `thinkingBudget`
/// with the qualitative `thinkingLevel` string in the wire oneof.
const THINKING_LEVEL_MIN_GENERATION: u32 = 3;

/// Parse the major generation number a Gemini model id expresses, e.g.
/// `2` from `gemini-2.5-pro`, `3` from `gemini-3.5-flash` or
/// `models/gemini-3.1-pro-preview`. Returns `None` when the id carries no
/// `gemini-<n>` version segment (unversioned or foreign ids), so callers
/// fall back to the legacy budget path. Reads only what the catalog id
/// already expresses -- no hardcoded model list.
fn gemini_generation(model: &str) -> Option<u32> {
    let after = model.rsplit("gemini-").next()?;
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<u32>().ok()
}

/// True when the model's generation uses `thinkingLevel` (Gemini-3+)
/// rather than the numeric `thinkingBudget`. Unknown / unversioned ids
/// default to the legacy budget path.
fn uses_thinking_level(model: &str) -> bool {
    gemini_generation(model).is_some_and(|g| g >= THINKING_LEVEL_MIN_GENERATION)
}

/// Map a canonical effort token (or a budget-derived level) to the
/// Gemini-3 `thinkingLevel` vocabulary (`minimal` | `low` | `medium` |
/// `high`). The six canonical tokens collapse onto Gemini's four: `xhigh`
/// and `max` saturate to `high`; `none` maps to the lowest `minimal`.
/// Returns `None` for any token outside the canonical set so the caller
/// omits the field and lets the model apply its default -- mirroring how
/// the budget path drops an unrecognized effort.
fn thinking_level_from_effort(effort: &str) -> Option<&'static str> {
    match effort {
        "none" | "minimal" => Some("minimal"),
        "low" => Some("low"),
        "medium" => Some("medium"),
        "high" | "xhigh" | "max" => Some("high"),
        _ => None,
    }
}

/// Map the canonical `reasoning` controls to Gemini's `thinkingConfig`.
///
/// Selects the oneof arm by model generation:
///   - Gemini-3+ -> `thinkingLevel` (string)
///   - older     -> `thinkingBudget` (numeric)
///
/// Within each arm:
///   - `enabled: Some(false)`         -> None (reasoning explicitly off)
///   - explicit `max_tokens` (budget) -> budget verbatim / budget-derived level
///   - explicit `effort`              -> level via the effort table
///   - reasoning present otherwise    -> dynamic budget (-1) / omitted level
///
/// `include_thoughts` is true whenever thinking is on and not excluded,
/// so thought summaries stream back and map to canonical reasoning.
/// Exactly one of `thinking_budget` / `thinking_level` is ever populated.
fn build_thinking_config(req: &ChatRequest) -> Option<ThinkingConfig> {
    let reasoning = req.reasoning.as_ref()?;
    if reasoning.enabled == Some(false) {
        return None;
    }

    let include_thoughts = Some(reasoning.exclude != Some(true));

    if uses_thinking_level(&req.model) {
        let thinking_level = if let Some(budget) = reasoning.max_tokens {
            // Map an explicit numeric budget onto Gemini-3's level scale:
            // budget -> canonical token -> Gemini level.
            thinking_level_from_effort(crate::effort::level_from_budget(budget))
        } else if let Some(effort) = reasoning.effort.as_deref() {
            thinking_level_from_effort(effort)
        } else {
            None
        };
        return Some(ThinkingConfig {
            thinking_budget: None,
            thinking_level: thinking_level.map(str::to_string),
            include_thoughts,
        });
    }

    let thinking_budget = if let Some(budget) = reasoning.max_tokens {
        // Gemini's thinkingBudget is i32; clamp rather than cast so a
        // pathological u32 cannot wrap into a negative sentinel
        // (-1 = dynamic, 0 = disabled).
        Some(i32::try_from(budget).unwrap_or(i32::MAX))
    } else if let Some(effort) = reasoning.effort.as_deref() {
        crate::effort::budget_from_level(effort).map(|b| i32::try_from(b).unwrap_or(i32::MAX))
    } else {
        Some(THINKING_BUDGET_DYNAMIC)
    };

    Some(ThinkingConfig {
        thinking_budget,
        thinking_level: None,
        include_thoughts,
    })
}

/// Map the canonical OpenAI-shape `response_format` to Gemini's
/// `responseMimeType` + `responseSchema`. Returns `(mime, schema)`:
///
/// - `{type:"json_schema", json_schema:{schema}}` -> `("application/json", Some(schema))`
/// - `{type:"json_object"}` -> `("application/json", None)`
/// - anything else / absent -> `(None, None)`
fn build_response_format(req: &ChatRequest) -> (Option<String>, Option<Value>) {
    let format = match req.response_format.as_ref() {
        Some(f) => f,
        None => return (None, None),
    };
    let kind = format.get("type").and_then(Value::as_str);
    match kind {
        Some("json_schema") => {
            let schema = format
                .get("json_schema")
                .and_then(|js| js.get("schema"))
                .cloned();
            (Some("application/json".to_string()), schema)
        }
        Some("json_object") => (Some("application/json".to_string()), None),
        _ => (None, None),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const fn text_part(text: String) -> Part {
    Part {
        text: Some(text),
        inline_data: None,
        function_call: None,
        function_response: None,
        thought: None,
        thought_signature: None,
    }
}

/// A thinking part replayed back to the model on a follow-up turn. The
/// `signature` is the opaque `thoughtSignature` Gemini handed back on a
/// prior turn; the text is the reasoning summary.
const fn thought_part(text: String, signature: Option<String>) -> Part {
    Part {
        text: Some(text),
        inline_data: None,
        function_call: None,
        function_response: None,
        thought: Some(true),
        thought_signature: signature,
    }
}

const fn inline_data_part(inline_data: InlineData) -> Part {
    Part {
        text: None,
        inline_data: Some(inline_data),
        function_call: None,
        function_response: None,
        thought: None,
        thought_signature: None,
    }
}

const fn function_call_part(call: FunctionCallPart) -> Part {
    Part {
        text: None,
        inline_data: None,
        function_call: Some(call),
        function_response: None,
        thought: None,
        thought_signature: None,
    }
}

const fn function_response_part(response: FunctionResponsePart) -> Part {
    Part {
        text: None,
        inline_data: None,
        function_call: None,
        function_response: Some(response),
        thought: None,
        thought_signature: None,
    }
}

/// Replay assistant reasoning_details as Gemini thought parts. Only
/// details tagged with `GEMINI_FORMAT` are echoed back: their
/// `payload.text` is the thinking summary and `payload.thought_signature`
/// is the opaque token Gemini requires verbatim for chain-of-thought
/// continuity. Foreign-provider reasoning (e.g. Anthropic, OpenAI) is
/// skipped -- replaying it without a matching Gemini signature would not
/// continue the model's reasoning and risks an upstream reject.
fn reasoning_details_to_thought_parts(details: &[ReasoningDetail]) -> Vec<Part> {
    details
        .iter()
        .filter(|d| d.format.as_deref() == Some(GEMINI_FORMAT))
        .filter_map(|d| {
            let text = d.payload.get("text").and_then(Value::as_str)?;
            let signature = d
                .payload
                .get("thought_signature")
                .and_then(Value::as_str)
                .map(std::string::ToString::to_string);
            Some(thought_part(text.to_string(), signature))
        })
        .collect()
}

/// True when `key` is a request field routectl assembles itself and an
/// operator/client must not be able to clobber via `payload_extras`.
/// Chains the canonical-request-key set (model, messages, tools,
/// response_format, reasoning, ...) with the Gemini wire-owned top-level
/// keys this translator writes. Unknown top-level request keys are
/// forwarded into `provider_extras` by both ingresses, so without the
/// Gemini-key guard a client could smuggle a raw `contents` /
/// `generationConfig` block that replaces the routectl-normalized body.
fn is_gemini_managed_key(key: &str) -> bool {
    routectl_core::is_canonical_request_key(key)
        || matches!(
            key,
            "contents" | "systemInstruction" | "toolConfig" | "generationConfig"
        )
}

/// Shallow-merge canonical `provider_extras` (the router's dispatch-time
/// merge of provider + model `payload_extras`) into the outgoing Gemini
/// body so operator-supplied top-level knobs like `safetySettings` reach
/// the wire. Entries that collide with a routectl-managed key (canonical
/// fields or a Gemini wire-owned key per [`is_gemini_managed_key`]) are
/// dropped with a WARN so neither an operator nor a client smuggling
/// extras through an ingress can clobber the assembled `contents` /
/// `generationConfig` / `tools` / etc.
pub fn merge_payload_extras(provider_id: &str, body: &mut Value, extras: &Value) {
    let (Some(body_obj), Some(extra_obj)) = (body.as_object_mut(), extras.as_object()) else {
        return;
    };
    for (k, v) in extra_obj {
        if is_gemini_managed_key(k) {
            tracing::warn!(
                provider = %provider_id,
                key = %k,
                "gemini: payload_extras attempted to override routectl-managed key; dropped"
            );
            continue;
        }
        body_obj.insert(k.clone(), v.clone());
    }
}

fn mime_from_filename(filename: &str) -> &'static str {
    let ext = filename.rfind('.').map_or("", |i| &filename[i + 1..]);
    match ext.to_lowercase().as_str() {
        "pdf" => "application/pdf",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_core::{CacheControl, ChatRequest, Message, MessageContent, Role};
    use routectl_core::{SystemBlock, SystemContent};
    use serde_json::json;
    use tracing_test::traced_test;

    fn make_user(text: &str) -> Message {
        Message {
            role: Role::User,
            content: MessageContent::Text(text.into()),
            refusal: None,
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn make_assistant(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: MessageContent::Text(text.into()),
            refusal: None,
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn make_system(text: &str) -> Message {
        Message {
            role: Role::System,
            content: MessageContent::Text(text.into()),
            refusal: None,
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }
    }

    fn base_req() -> ChatRequest {
        ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![make_user("hello")].into(),
            ..Default::default()
        }
    }

    #[test]
    fn simple_text_message_maps_to_user_content() {
        let req = base_req();
        let body = translate("gemini:test", &req).expect("translate ok");

        assert_eq!(body.contents.len(), 1);
        assert_eq!(body.contents[0].role, "user");
        assert_eq!(body.contents[0].parts[0].text.as_deref(), Some("hello"));
        assert!(body.system_instruction.is_none());
    }

    #[test]
    fn multi_turn_user_model_roles() {
        let req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![
                make_user("hi"),
                make_assistant("hello back"),
                make_user("thanks"),
            ]
            .into(),
            ..Default::default()
        };
        let body = translate("gemini:test", &req).expect("translate ok");

        assert_eq!(body.contents.len(), 3);
        assert_eq!(body.contents[0].role, "user");
        assert_eq!(body.contents[1].role, "model");
        assert_eq!(body.contents[2].role, "user");
    }

    #[test]
    fn system_message_lifted_into_system_instruction() {
        let req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![make_system("you are helpful"), make_user("hi")].into(),
            ..Default::default()
        };
        let body = translate("gemini:test", &req).expect("translate ok");

        // System message stays out of contents[].
        assert_eq!(body.contents.len(), 1);
        let si = body.system_instruction.expect("system_instruction present");
        assert_eq!(si.parts[0].text.as_deref(), Some("you are helpful"));
    }

    #[test]
    fn assistant_tool_calls_become_function_call_parts() {
        let req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![Message {
                role: Role::Assistant,
                content: MessageContent::Null,
                refusal: None,
                reasoning: None,
                reasoning_details: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}
                })]),
            }]
            .into(),
            ..Default::default()
        };
        let body = translate("gemini:test", &req).expect("translate ok");

        assert_eq!(body.contents.len(), 1);
        assert_eq!(body.contents[0].role, "model");
        let fc = body.contents[0].parts[0]
            .function_call
            .as_ref()
            .expect("function_call part");
        assert_eq!(fc.name, "get_weather");
        assert_eq!(fc.args["city"], "Paris");
    }

    #[test]
    fn tool_result_becomes_function_response_user_turn() {
        let req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![Message {
                role: Role::Tool,
                content: MessageContent::Text("sunny".into()),
                refusal: None,
                reasoning: None,
                reasoning_details: Vec::new(),
                name: Some("get_weather".into()),
                tool_call_id: Some("call_1".into()),
                tool_calls: None,
            }]
            .into(),
            ..Default::default()
        };
        let body = translate("gemini:test", &req).expect("translate ok");

        assert_eq!(body.contents.len(), 1);
        assert_eq!(body.contents[0].role, "user");
        let fr = body.contents[0].parts[0]
            .function_response
            .as_ref()
            .expect("function_response part");
        assert_eq!(fr.name, "get_weather");
    }

    #[test]
    fn nameless_tool_result_recovers_name_from_prior_openai_tool_call() {
        // Arrange: an OpenAI-shape assistant tool_call carrying the name +
        // id, followed by a tool-role result that omits the name (only the
        // tool_call_id survives cross-dialect).
        let assistant = Message {
            role: Role::Assistant,
            content: MessageContent::Null,
            refusal: None,
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![json!({
                "id": "call_42",
                "type": "function",
                "function": {"name": "get_weather", "arguments": "{}"}
            })]),
        };
        let tool_result = Message {
            role: Role::Tool,
            content: MessageContent::Text("sunny".into()),
            refusal: None,
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: Some("call_42".into()),
            tool_calls: None,
        };
        let req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![make_user("weather?"), assistant, tool_result].into(),
            ..Default::default()
        };

        // Act
        let body = translate("gemini:test", &req).expect("translate ok");

        // Assert: the functionResponse recovered the name by id match.
        let tool_turn = body.contents.last().expect("tool result turn");
        let fr = tool_turn.parts[0]
            .function_response
            .as_ref()
            .expect("function_response part");
        assert_eq!(fr.name, "get_weather");
    }

    #[test]
    fn nameless_tool_result_recovers_name_from_prior_anthropic_tool_use() {
        use routectl_core::{ContentPart, KnownContentPart};
        // Arrange: an Anthropic-shape assistant tool_use block (id + name)
        // followed by a tool-role result keyed only on tool_call_id.
        let assistant = Message {
            role: Role::Assistant,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ToolUse {
                id: "toolu_7".into(),
                name: "lookup".into(),
                input: json!({}),
                cache_control: None,
            })]),
            refusal: None,
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };
        let tool_result = Message {
            role: Role::Tool,
            content: MessageContent::Text("42".into()),
            refusal: None,
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: Some("toolu_7".into()),
            tool_calls: None,
        };
        let req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![make_user("q"), assistant, tool_result].into(),
            ..Default::default()
        };

        // Act
        let body = translate("gemini:test", &req).expect("translate ok");

        // Assert
        let tool_turn = body.contents.last().expect("tool result turn");
        let fr = tool_turn.parts[0]
            .function_response
            .as_ref()
            .expect("function_response part");
        assert_eq!(fr.name, "lookup");
    }

    #[test]
    fn tool_result_falls_back_to_empty_name_when_no_match() {
        // Arrange: a tool result whose id matches no prior tool call and
        // carries no name -- Gemini cannot correlate; name stays empty.
        let tool_result = Message {
            role: Role::Tool,
            content: MessageContent::Text("orphan".into()),
            refusal: None,
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: Some("unknown_id".into()),
            tool_calls: None,
        };
        let req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![tool_result].into(),
            ..Default::default()
        };

        // Act
        let body = translate("gemini:test", &req).expect("translate ok");

        // Assert
        let fr = body.contents[0].parts[0]
            .function_response
            .as_ref()
            .expect("function_response part");
        assert_eq!(fr.name, "");
    }

    #[test]
    fn tool_result_prefers_recovered_name_over_carried_name() {
        // Arrange: id match wins over any name the ingress happened to carry.
        let assistant = Message {
            role: Role::Assistant,
            content: MessageContent::Null,
            refusal: None,
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: Some(vec![json!({
                "id": "call_9",
                "type": "function",
                "function": {"name": "canonical_name", "arguments": "{}"}
            })]),
        };
        let tool_result = Message {
            role: Role::Tool,
            content: MessageContent::Text("x".into()),
            refusal: None,
            reasoning: None,
            reasoning_details: Vec::new(),
            name: Some("stale_name".into()),
            tool_call_id: Some("call_9".into()),
            tool_calls: None,
        };
        let req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![make_user("q"), assistant, tool_result].into(),
            ..Default::default()
        };

        // Act
        let body = translate("gemini:test", &req).expect("translate ok");

        // Assert
        let fr = body.contents.last().unwrap().parts[0]
            .function_response
            .as_ref()
            .expect("function_response part");
        assert_eq!(fr.name, "canonical_name");
    }

    #[test]
    fn tools_become_function_declarations() {
        use routectl_core::{CustomTool, ToolDef};
        let req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![make_user("call something")].into(),
            tools: Some(vec![ToolDef::Custom(CustomTool {
                name: "my_fn".into(),
                description: Some("does stuff".into()),
                input_schema: json!({"type":"object","properties":{}}),
                cache_control: None,
                defer_loading: None,
                strict: None,
                type_tag: None,
            })]),
            ..Default::default()
        };
        let body = translate("gemini:test", &req).expect("translate ok");

        let tools = body.tools.expect("tools present");
        assert_eq!(tools[0].function_declarations[0].name, "my_fn");
        assert_eq!(
            tools[0].function_declarations[0].description.as_deref(),
            Some("does stuff")
        );
    }

    #[test]
    fn custom_tool_parameters_are_gemini_cleaned() {
        use routectl_core::{CustomTool, ToolDef};
        // Arrange: a caller schema carrying constructs Gemini rejects raw.
        let req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![make_user("go")].into(),
            tools: Some(vec![ToolDef::Custom(CustomTool {
                name: "f".into(),
                description: None,
                input_schema: json!({
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "mode": {"oneOf": [{"type": "string"}]},
                        "count": {"type": ["integer", "null"]}
                    }
                }),
                cache_control: None,
                defer_loading: None,
                strict: None,
                type_tag: None,
            })]),
            ..Default::default()
        };

        // Act
        let body = translate("gemini:test", &req).expect("translate ok");

        // Assert: the emitted parameters are in the Gemini subset.
        let params = body.tools.expect("tools")[0].function_declarations[0]
            .parameters
            .clone()
            .expect("parameters");
        assert!(params.get("$schema").is_none());
        assert!(params.get("additionalProperties").is_none());
        assert_eq!(params["type"], "OBJECT");
        assert!(params["properties"]["mode"].get("oneOf").is_none());
        assert_eq!(params["properties"]["mode"]["anyOf"][0]["type"], "STRING");
        assert_eq!(params["properties"]["count"]["type"], "INTEGER");
        assert_eq!(params["properties"]["count"]["nullable"], true);
    }

    // ---------------------------------------------------------------------------
    // nameless ToolDef::Other is skipped-with-warn, never emitted
    // ---------------------------------------------------------------------------

    fn req_with_tools(tools: Vec<routectl_core::ToolDef>) -> ChatRequest {
        ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![make_user("go")].into(),
            tools: Some(tools),
            ..Default::default()
        }
    }

    #[traced_test]
    #[test]
    fn nameless_other_tool_is_skipped_with_warn_not_emitted() {
        use routectl_core::ToolDef;
        // Arrange: a hosted-tool shape carrying no usable function name.
        let req = req_with_tools(vec![ToolDef::Other(json!({"type": "web_search"}))]);

        // Act
        let body = translate("gemini:test", &req).expect("translate ok");

        // Assert: no tools emitted (the only def was skipped), and a WARN fired.
        assert!(
            body.tools.is_none(),
            "a nameless tool def must not produce an empty-named declaration"
        );
        assert!(
            logs_contain("skipping tool def with no usable function name"),
            "the skip must be surfaced with a structured WARN"
        );
    }

    #[test]
    fn nameless_other_tool_does_not_starve_named_siblings() {
        use routectl_core::{CustomTool, ToolDef};
        // Arrange: a nameless hosted tool alongside a named Other and a Custom.
        let req = req_with_tools(vec![
            ToolDef::Other(json!({"type": "file_search"})),
            ToolDef::Other(json!({
                "type": "function",
                "function": {"name": "lookup", "description": "d", "parameters": {"type": "object"}}
            })),
            ToolDef::Custom(CustomTool {
                name: "calc".into(),
                description: None,
                input_schema: json!({"type": "object", "properties": {}}),
                cache_control: None,
                defer_loading: None,
                strict: None,
                type_tag: None,
            }),
        ]);

        // Act
        let body = translate("gemini:test", &req).expect("translate ok");

        // Assert: only the two named tools survive; the nameless one is gone.
        let decls = &body.tools.expect("tools present")[0].function_declarations;
        let names: Vec<&str> = decls.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["lookup", "calc"]);
        assert!(
            !names.iter().any(|n| n.is_empty()),
            "no empty-named declaration may be emitted"
        );
    }

    #[test]
    fn named_other_tool_still_emits_declaration_unchanged() {
        use routectl_core::ToolDef;
        // Arrange: a native OpenAI function-shape ToolDef::Other.
        let req = req_with_tools(vec![ToolDef::Other(json!({
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "look up weather",
                "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}
            }
        }))]);

        // Act
        let body = translate("gemini:test", &req).expect("translate ok");

        // Assert: the function-shape tool is preserved as a declaration.
        let decl = &body.tools.expect("tools present")[0].function_declarations[0];
        assert_eq!(decl.name, "get_weather");
        assert_eq!(decl.description.as_deref(), Some("look up weather"));
        assert!(decl.parameters.is_some());
    }

    // ---------------------------------------------------------------------------
    // skip_thought_signature_validator sentinel + provenance
    // ---------------------------------------------------------------------------

    fn assistant_with_tool_calls(model: &str, tool_calls: Vec<Value>) -> ChatRequest {
        ChatRequest {
            model: model.into(),
            messages: vec![Message {
                role: Role::Assistant,
                content: MessageContent::Null,
                refusal: None,
                reasoning: None,
                reasoning_details: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: Some(tool_calls),
            }]
            .into(),
            ..Default::default()
        }
    }

    fn model_turn_function_parts(body: &GenerateContentRequest) -> Vec<&Part> {
        body.contents
            .iter()
            .find(|c| c.role == "model")
            .expect("model turn")
            .parts
            .iter()
            .filter(|p| p.function_call.is_some())
            .collect()
    }

    fn foreign_tool_call(name: &str) -> Value {
        json!({
            "id": format!("call_{name}"),
            "type": "function",
            "function": {"name": name, "arguments": "{}"}
        })
    }

    #[test]
    fn gemini2_foreign_tool_history_gets_no_sentinel() {
        // Arrange: foreign tool-call history replayed to a Gemini-2 target.
        let req = assistant_with_tool_calls("gemini-2.5-pro", vec![foreign_tool_call("f")]);

        // Act
        let body = translate("gemini:test", &req).expect("translate ok");

        // Assert: no sentinel -- a synthetic signature on Gemini-2 risks a new
        // reject path.
        let parts = model_turn_function_parts(&body);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].thought_signature, None);
    }

    #[test]
    fn gemini3_foreign_tool_history_gets_sentinel_on_first_call_only() {
        // Arrange: two parallel foreign tool calls in one turn, Gemini-3 target.
        let req = assistant_with_tool_calls(
            "gemini-3.5-flash",
            vec![foreign_tool_call("a"), foreign_tool_call("b")],
        );

        // Act
        let body = translate("gemini:test", &req).expect("translate ok");

        // Assert: only the first functionCall carries the sentinel.
        let parts = model_turn_function_parts(&body);
        assert_eq!(parts.len(), 2);
        assert_eq!(
            parts[0].thought_signature.as_deref(),
            Some(SKIP_THOUGHT_SIGNATURE_VALIDATOR)
        );
        assert_eq!(
            parts[1].thought_signature, None,
            "parallel calls after the first do not get the sentinel"
        );
    }

    #[test]
    fn gemini3_single_foreign_tool_call_gets_sentinel() {
        // Arrange: a single foreign tool call, Gemini-3 target.
        let req = assistant_with_tool_calls("gemini-3.5-flash", vec![foreign_tool_call("solo")]);

        // Act
        let body = translate("gemini:test", &req).expect("translate ok");

        // Assert
        let parts = model_turn_function_parts(&body);
        assert_eq!(parts.len(), 1);
        assert_eq!(
            parts[0].thought_signature.as_deref(),
            Some(SKIP_THOUGHT_SIGNATURE_VALIDATOR)
        );
    }

    #[test]
    fn gemini3_native_signature_preserved_not_overwritten_by_sentinel() {
        // Arrange: a native-Gemini tool turn whose signature was captured onto
        // the tool_call by the response translator (round-tripped end to end):
        // build a Gemini response with a functionCall part carrying a real
        // thoughtSignature, translate it to canonical, and replay the resulting
        // tool_calls to a Gemini-3 target.
        use crate::gemini::response;
        use crate::gemini::types::{
            Candidate, GenerateContentResponse, ResponseContent, ResponseFunctionCall, ResponsePart,
        };

        let resp = GenerateContentResponse {
            candidates: vec![Candidate {
                content: Some(ResponseContent {
                    parts: vec![ResponsePart {
                        text: None,
                        function_call: Some(ResponseFunctionCall {
                            name: "native_fn".into(),
                            args: json!({"x": 1}),
                        }),
                        thought_signature: Some("real-sig-123".into()),
                        ..Default::default()
                    }],
                    role: Some("model".into()),
                }),
                finish_reason: Some("STOP".into()),
                index: 0,
            }],
            usage_metadata: None,
            model_version: None,
            response_id: Some("r".into()),
            prompt_feedback: None,
        };
        let canonical = response::translate("gemini:test", resp).expect("response translate");
        let tool_calls = canonical.choices[0]
            .message
            .tool_calls
            .clone()
            .expect("tool_calls captured");

        // Act: replay that captured history to a Gemini-3 target.
        let req = assistant_with_tool_calls("gemini-3.5-flash", tool_calls);
        let body = translate("gemini:test", &req).expect("translate ok");

        // Assert: the genuine signature survives verbatim; the sentinel never
        // overwrites it.
        let parts = model_turn_function_parts(&body);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].thought_signature.as_deref(), Some("real-sig-123"));
    }

    fn anthropic_tooluse_turn(model: &str) -> ChatRequest {
        use routectl_core::{ContentPart, KnownContentPart};
        ChatRequest {
            model: model.into(),
            messages: vec![Message {
                role: Role::Assistant,
                content: MessageContent::Parts(vec![ContentPart::Known(
                    KnownContentPart::ToolUse {
                        id: "toolu_1".into(),
                        name: "get_weather".into(),
                        input: json!({"city": "Paris"}),
                        cache_control: None,
                    },
                )]),
                refusal: None,
                reasoning: None,
                reasoning_details: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }]
            .into(),
            ..Default::default()
        }
    }

    #[test]
    fn gemini3_anthropic_tooluse_content_part_gets_sentinel() {
        // Anthropic-canonical assistant turns carry tool calls as ToolUse
        // content-parts with `tool_calls: None`. That functionCall is foreign
        // to Gemini and must get the skip-validation sentinel on Gemini-3.
        let req = anthropic_tooluse_turn("gemini-3.5-flash");
        let body = translate("gemini:test", &req).expect("translate ok");

        let parts = model_turn_function_parts(&body);
        assert_eq!(parts.len(), 1);
        assert_eq!(
            parts[0].function_call.as_ref().expect("fc").name,
            "get_weather"
        );
        assert_eq!(
            parts[0].thought_signature.as_deref(),
            Some(SKIP_THOUGHT_SIGNATURE_VALIDATOR)
        );
    }

    #[test]
    fn gemini2_anthropic_tooluse_content_part_gets_no_sentinel() {
        // The same foreign ToolUse content-part replayed to a Gemini-2 target
        // must NOT get a synthetic signature.
        let req = anthropic_tooluse_turn("gemini-2.5-pro");
        let body = translate("gemini:test", &req).expect("translate ok");

        let parts = model_turn_function_parts(&body);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].thought_signature, None);
    }

    #[test]
    fn gemini3_native_signed_first_call_preserved_when_mixed_with_content() {
        // A turn mixing a visible-text content part with a native-Gemini signed
        // tool_call: the genuine signature on the first functionCall survives
        // and the skip-validation sentinel never overwrites it.
        let signed_call = json!({
            "id": "call_native",
            "type": "function",
            "function": {"name": "native_fn", "arguments": "{}"},
            "thought_signature": "real-sig-xyz"
        });
        let req = ChatRequest {
            model: "gemini-3.5-flash".into(),
            messages: vec![Message {
                role: Role::Assistant,
                content: MessageContent::Text("here goes".into()),
                refusal: None,
                reasoning: None,
                reasoning_details: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![signed_call]),
            }]
            .into(),
            ..Default::default()
        };
        let body = translate("gemini:test", &req).expect("translate ok");

        let parts = model_turn_function_parts(&body);
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0].thought_signature.as_deref(), Some("real-sig-xyz"));
    }

    // ---------------------------------------------------------------------------
    // toolConfig is never emitted without surviving declarations
    // ---------------------------------------------------------------------------

    fn nameless_tool_req_with_choice(choice: Value) -> ChatRequest {
        use routectl_core::ToolDef;
        ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![make_user("go")].into(),
            tools: Some(vec![ToolDef::Other(json!({"type": "web_search"}))]),
            tool_choice: Some(choice),
            ..Default::default()
        }
    }

    #[test]
    fn all_tools_skipped_with_auto_choice_omits_tool_config() {
        let req = nameless_tool_req_with_choice(json!("auto"));
        let body = translate("gemini:test", &req).expect("translate ok");
        assert!(body.tools.is_none());
        assert!(
            body.tool_config.is_none(),
            "a toolConfig with no functionDeclarations is rejected by Gemini"
        );
    }

    #[test]
    fn all_tools_skipped_with_required_choice_omits_tool_config() {
        let req = nameless_tool_req_with_choice(json!("required"));
        let body = translate("gemini:test", &req).expect("translate ok");
        assert!(body.tools.is_none());
        assert!(
            body.tool_config.is_none(),
            "required must not force ANY-mode with no declarations"
        );
    }

    #[test]
    fn all_tools_skipped_with_named_choice_omits_tool_config() {
        let req = nameless_tool_req_with_choice(
            json!({"type": "function", "function": {"name": "web_search"}}),
        );
        let body = translate("gemini:test", &req).expect("translate ok");
        assert!(body.tools.is_none());
        assert!(body.tool_config.is_none());
    }

    fn single_custom_tool_req(choice: Value) -> ChatRequest {
        use routectl_core::{CustomTool, ToolDef};
        ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![make_user("go")].into(),
            tools: Some(vec![ToolDef::Custom(CustomTool {
                name: "calc".into(),
                description: None,
                input_schema: json!({"type": "object", "properties": {}}),
                cache_control: None,
                defer_loading: None,
                strict: None,
                type_tag: None,
            })]),
            tool_choice: Some(choice),
            ..Default::default()
        }
    }

    #[test]
    fn forcing_a_skipped_tool_drops_the_forcing_but_keeps_survivor() {
        // A survivor exists, but the forced name names a tool that is not among
        // the surviving declarations: emit the survivor, drop the forcing.
        let req = single_custom_tool_req(json!({"type": "function", "function": {"name": "gone"}}));
        let body = translate("gemini:test", &req).expect("translate ok");
        assert!(body.tools.is_some(), "the surviving tool is still emitted");
        assert!(
            body.tool_config.is_none(),
            "forcing a tool with no surviving declaration must be dropped"
        );
    }

    #[test]
    fn forcing_a_surviving_tool_keeps_the_config() {
        // Guard against over-dropping: a forced name that IS among survivors
        // still produces ANY-mode with allowedFunctionNames.
        let req = single_custom_tool_req(json!({"type": "function", "function": {"name": "calc"}}));
        let body = translate("gemini:test", &req).expect("translate ok");
        let tc = body.tool_config.expect("tool_config present");
        assert_eq!(tc.function_calling_config.mode, "ANY");
        assert_eq!(
            tc.function_calling_config.allowed_function_names.as_deref(),
            Some(["calc".to_string()].as_slice())
        );
    }

    #[test]
    fn tool_choice_none_maps_to_mode_none() {
        use routectl_core::{CustomTool, ToolDef};
        let req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![make_user("hi")].into(),
            tools: Some(vec![ToolDef::Custom(CustomTool {
                name: "f".into(),
                description: None,
                input_schema: json!({"type":"object","properties":{}}),
                cache_control: None,
                defer_loading: None,
                strict: None,
                type_tag: None,
            })]),
            tool_choice: Some(json!("none")),
            ..Default::default()
        };
        let body = translate("gemini:test", &req).expect("translate ok");

        let tc = body.tool_config.expect("tool_config");
        assert_eq!(tc.function_calling_config.mode, "NONE");
    }

    #[test]
    fn generation_config_fields_populate_correctly() {
        let req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![make_user("hi")].into(),
            temperature: Some(0.7),
            top_p: Some(0.9),
            max_tokens: Some(512),
            stop: Some(vec!["END".into()]),
            ..Default::default()
        };
        let body = translate("gemini:test", &req).expect("translate ok");

        let gc = body.generation_config.expect("generation_config");
        assert_eq!(gc.temperature, Some(0.7));
        assert_eq!(gc.top_p, Some(0.9));
        assert_eq!(gc.max_output_tokens, Some(512));
        assert_eq!(
            gc.stop_sequences.as_deref(),
            Some(["END".to_string()].as_slice())
        );
    }

    fn req_with_reasoning(r: routectl_core::ReasoningConfig) -> ChatRequest {
        ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![make_user("hi")].into(),
            reasoning: Some(r),
            ..Default::default()
        }
    }

    #[test]
    fn thinking_config_from_effort_uses_budget_table() {
        let req = req_with_reasoning(routectl_core::ReasoningConfig {
            effort: Some("high".into()),
            ..Default::default()
        });
        let tc = translate("gemini:test", &req)
            .expect("translate")
            .generation_config
            .expect("generation_config")
            .thinking_config
            .expect("thinking_config");
        // "high" -> 24576 from the shared effort table.
        assert_eq!(tc.thinking_budget, Some(24576));
        assert_eq!(tc.include_thoughts, Some(true));
    }

    #[test]
    fn thinking_config_from_explicit_budget() {
        let req = req_with_reasoning(routectl_core::ReasoningConfig {
            max_tokens: Some(2048),
            ..Default::default()
        });
        let tc = translate("gemini:test", &req)
            .expect("translate")
            .generation_config
            .expect("generation_config")
            .thinking_config
            .expect("thinking_config");
        assert_eq!(tc.thinking_budget, Some(2048));
    }

    #[test]
    fn thinking_disabled_when_reasoning_enabled_false() {
        let req = req_with_reasoning(routectl_core::ReasoningConfig {
            enabled: Some(false),
            ..Default::default()
        });
        // No other generationConfig knobs set -> the whole block is None.
        assert!(
            translate("gemini:test", &req)
                .expect("translate")
                .generation_config
                .is_none()
        );
    }

    #[test]
    fn thinking_exclude_sets_include_thoughts_false() {
        let req = req_with_reasoning(routectl_core::ReasoningConfig {
            max_tokens: Some(100),
            exclude: Some(true),
            ..Default::default()
        });
        let tc = translate("gemini:test", &req)
            .expect("translate")
            .generation_config
            .expect("generation_config")
            .thinking_config
            .expect("thinking_config");
        assert_eq!(tc.include_thoughts, Some(false));
    }

    // ---------------------------------------------------------------------------
    // thinkingLevel vs thinkingBudget by model generation (Gemini-3 oneof)
    // ---------------------------------------------------------------------------

    fn req_gen3_reasoning(r: routectl_core::ReasoningConfig) -> ChatRequest {
        ChatRequest {
            model: "gemini-3.5-flash".into(),
            messages: vec![make_user("hi")].into(),
            reasoning: Some(r),
            ..Default::default()
        }
    }

    #[test]
    fn gemini3_model_emits_thinking_level_not_budget() {
        // Arrange: a Gemini-3 model with an effort level.
        let req = req_gen3_reasoning(routectl_core::ReasoningConfig {
            effort: Some("high".into()),
            ..Default::default()
        });

        // Act
        let tc = translate("gemini:test", &req)
            .expect("translate")
            .generation_config
            .expect("generation_config")
            .thinking_config
            .expect("thinking_config");

        // Assert: level set, numeric budget absent.
        assert_eq!(tc.thinking_level.as_deref(), Some("high"));
        assert_eq!(tc.thinking_budget, None);
    }

    #[test]
    fn older_gemini_model_still_emits_thinking_budget_not_level() {
        // Arrange: a Gemini-2.5 model with the same effort.
        let req = req_with_reasoning(routectl_core::ReasoningConfig {
            effort: Some("high".into()),
            ..Default::default()
        });

        // Act
        let tc = translate("gemini:test", &req)
            .expect("translate")
            .generation_config
            .expect("generation_config")
            .thinking_config
            .expect("thinking_config");

        // Assert: numeric budget set, level absent.
        assert_eq!(tc.thinking_budget, Some(24576));
        assert_eq!(tc.thinking_level, None);
    }

    #[test]
    fn thinking_config_oneof_serializes_exactly_one_arm() {
        // Arrange: a Gemini-3 request whose thinkingConfig must carry the
        // level arm only, never the numeric budget.
        let req = req_gen3_reasoning(routectl_core::ReasoningConfig {
            effort: Some("medium".into()),
            ..Default::default()
        });

        // Act: serialize the assembled body to the wire shape.
        let body = translate("gemini:test", &req).expect("translate");
        let value = serde_json::to_value(&body).expect("serialize");
        let thinking = &value["generationConfig"]["thinkingConfig"];

        // Assert: thinkingLevel present, thinkingBudget absent -- the oneof
        // is never double-populated on the wire.
        assert_eq!(thinking["thinkingLevel"], "medium");
        assert!(
            thinking.get("thinkingBudget").is_none(),
            "budget arm must not serialize alongside level: {thinking}"
        );
    }

    #[test]
    fn gemini3_explicit_budget_maps_to_level() {
        // Arrange: a Gemini-3 model given a numeric budget. Gemini-3 takes
        // no numeric budget, so it is mapped onto the level scale (2048
        // falls in the medium band).
        let req = req_gen3_reasoning(routectl_core::ReasoningConfig {
            max_tokens: Some(2048),
            ..Default::default()
        });

        // Act
        let tc = translate("gemini:test", &req)
            .expect("translate")
            .generation_config
            .expect("generation_config")
            .thinking_config
            .expect("thinking_config");

        // Assert
        assert_eq!(tc.thinking_level.as_deref(), Some("medium"));
        assert_eq!(tc.thinking_budget, None);
    }

    #[test]
    fn gemini3_saturates_xhigh_and_max_to_high() {
        for effort in ["xhigh", "max"] {
            let req = req_gen3_reasoning(routectl_core::ReasoningConfig {
                effort: Some(effort.into()),
                ..Default::default()
            });
            let tc = translate("gemini:test", &req)
                .expect("translate")
                .generation_config
                .expect("generation_config")
                .thinking_config
                .expect("thinking_config");
            assert_eq!(
                tc.thinking_level.as_deref(),
                Some("high"),
                "{effort} must saturate to high"
            );
            assert_eq!(tc.thinking_budget, None);
        }
    }

    #[test]
    fn gemini3_dynamic_reasoning_omits_both_arms() {
        // Arrange: reasoning present but neither effort nor budget given.
        // Gemini-3 has no dynamic-level sentinel, so both arms are omitted
        // and the model applies its own default.
        let req = req_gen3_reasoning(routectl_core::ReasoningConfig::default());

        // Act
        let tc = translate("gemini:test", &req)
            .expect("translate")
            .generation_config
            .expect("generation_config")
            .thinking_config
            .expect("thinking_config");

        // Assert
        assert_eq!(tc.thinking_level, None);
        assert_eq!(tc.thinking_budget, None);
        assert_eq!(tc.include_thoughts, Some(true));
    }

    #[test]
    fn gemini_generation_parsed_generically_from_id() {
        // Bare parser: reads the generation the catalog id expresses, no
        // hardcoded model list.
        assert_eq!(gemini_generation("gemini-2.5-pro"), Some(2));
        assert_eq!(gemini_generation("gemini-3.5-flash"), Some(3));
        assert_eq!(gemini_generation("models/gemini-3.1-pro-preview"), Some(3));
        assert_eq!(gemini_generation("gemini-pro"), None);
        assert!(uses_thinking_level("gemini-3.5-flash"));
        assert!(!uses_thinking_level("gemini-2.5-pro"));
        assert!(!uses_thinking_level("gemini-pro"));
    }

    #[test]
    fn response_format_json_schema_maps_to_response_schema() {
        let req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![make_user("hi")].into(),
            response_format: Some(json!({
                "type": "json_schema",
                "json_schema": {"schema": {"type": "object", "properties": {}}}
            })),
            ..Default::default()
        };
        let gc = translate("gemini:test", &req)
            .expect("translate")
            .generation_config
            .expect("generation_config");
        assert_eq!(gc.response_mime_type.as_deref(), Some("application/json"));
        assert_eq!(gc.response_schema.expect("schema")["type"], "object");
    }

    #[test]
    fn response_format_json_object_sets_mime_without_schema() {
        let req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![make_user("hi")].into(),
            response_format: Some(json!({"type": "json_object"})),
            ..Default::default()
        };
        let gc = translate("gemini:test", &req)
            .expect("translate")
            .generation_config
            .expect("generation_config");
        assert_eq!(gc.response_mime_type.as_deref(), Some("application/json"));
        assert!(gc.response_schema.is_none());
    }

    #[test]
    fn gemini_reasoning_details_replayed_as_thought_parts() {
        // One Gemini-origin detail (replayed) + one foreign detail (skipped).
        let assistant = Message {
            role: Role::Assistant,
            content: MessageContent::Text("the answer".into()),
            refusal: None,
            reasoning: None,
            reasoning_details: vec![
                routectl_core::ReasoningDetail {
                    kind: routectl_core::ReasoningDetailKind::Text,
                    id: None,
                    format: Some(crate::gemini::GEMINI_FORMAT.to_string()),
                    index: Some(0),
                    payload: json!({"text": "prior thought", "thought_signature": "sig9"}),
                },
                routectl_core::ReasoningDetail {
                    kind: routectl_core::ReasoningDetailKind::Text,
                    id: None,
                    format: Some("anthropic-v1".to_string()),
                    index: Some(0),
                    payload: json!({"text": "foreign reasoning"}),
                },
            ],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };
        let req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![make_user("q"), assistant].into(),
            ..Default::default()
        };
        let body = translate("gemini:test", &req).expect("translate");
        let model_turn = body
            .contents
            .iter()
            .find(|c| c.role == "model")
            .expect("model turn");
        let thoughts: Vec<&Part> = model_turn
            .parts
            .iter()
            .filter(|p| p.thought == Some(true))
            .collect();
        assert_eq!(thoughts.len(), 1, "only the Gemini-origin detail replays");
        assert_eq!(thoughts[0].text.as_deref(), Some("prior thought"));
        assert_eq!(thoughts[0].thought_signature.as_deref(), Some("sig9"));
        // The thought part precedes the visible answer text.
        assert_eq!(
            model_turn.parts.last().unwrap().text.as_deref(),
            Some("the answer")
        );
    }

    #[test]
    #[traced_test]
    fn responses_reasoning_context_mode_dropped_and_warns() {
        // A Responses-ingress request carrying reasoning context/mode routed
        // to the Gemini egress does NOT emit them and warns once.
        let mut req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![make_user("hi")].into(),
            ..Default::default()
        };
        req.provider_extras = Some(json!({"reasoning": {"context": "all_turns", "mode": "pro"}}));

        let r = translate("gemini:test", &req).expect("translate");
        let body = serde_json::to_value(&r).unwrap();
        // Merge the remainder as the real path does; managed key -> dropped.
        let mut body = body;
        merge_payload_extras(
            "gemini:test",
            &mut body,
            req.provider_extras.as_ref().unwrap(),
        );

        assert!(body.get("reasoning").is_none());
        assert!(body.get("context").is_none());
        assert!(body.get("mode").is_none());
        assert!(logs_contain("reasoning context/mode dropped"));
    }

    #[test]
    fn payload_extras_merges_non_canonical_key() {
        let mut body = json!({"contents": []});
        let extras = json!({"safetySettings": [{"category": "HARM_CATEGORY_HATE_SPEECH"}]});
        merge_payload_extras("gemini:test", &mut body, &extras);
        assert!(
            body.get("safetySettings").is_some(),
            "safetySettings must merge in"
        );
    }

    #[test]
    fn payload_extras_drops_canonical_managed_key() {
        // `tools` is a routectl-managed canonical key (and a real Gemini
        // body key) -- payload_extras must not be allowed to clobber it.
        let mut body = json!({"contents": [], "tools": [{"functionDeclarations": []}]});
        let extras = json!({"tools": "operator-clobber", "safetySettings": []});
        merge_payload_extras("gemini:test", &mut body, &extras);
        assert!(
            body["tools"].is_array(),
            "assembled tools must be preserved"
        );
        assert!(body.get("safetySettings").is_some());
    }

    #[test]
    fn payload_extras_cannot_clobber_gemini_structural_keys() {
        // contents / generationConfig are Gemini wire-owned keys (NOT
        // canonical names), but a client can smuggle them via provider_extras
        // through an ingress. They must be dropped, not allowed to replace
        // the routectl-assembled body.
        let mut body = json!({
            "contents": [{"role": "user", "parts": [{"text": "real"}]}],
            "generationConfig": {"temperature": 0.5}
        });
        let extras = json!({
            "contents": "client-clobber",
            "generationConfig": {"temperature": 9.9},
            "systemInstruction": {"parts": [{"text": "evil"}]},
            "toolConfig": "x"
        });
        merge_payload_extras("gemini:test", &mut body, &extras);
        assert!(body["contents"].is_array(), "assembled contents preserved");
        assert_eq!(
            body["generationConfig"]["temperature"], 0.5,
            "assembled generationConfig must not be replaced"
        );
        assert!(
            body.get("systemInstruction").is_none(),
            "smuggled systemInstruction must be dropped"
        );
        assert!(body.get("toolConfig").is_none());
    }

    // ---------------------------------------------------------------------------
    // cache_control drop-with-warn (Gemini has no caller breakpoint surface)
    // ---------------------------------------------------------------------------

    #[test]
    fn dropped_cache_surfaces_names_every_carrier() {
        // Arrange: a top-level marker AND a system-block marker.
        let mut req = base_req();
        req.cache_control = Some(CacheControl::ephemeral_5m());
        req.system = Some(SystemContent::Blocks(vec![SystemBlock {
            kind: "text".into(),
            text: "sys".into(),
            cache_control: Some(CacheControl::ephemeral_5m()),
            citations: None,
        }]));

        // Act
        let surfaces = dropped_cache_surfaces(&req);

        // Assert: both surfaces reported (Gemini logs system too, unlike
        // the Responses egress).
        assert!(surfaces.contains(&"top-level"), "got: {surfaces:?}");
        assert!(surfaces.contains(&"system"), "got: {surfaces:?}");
    }

    #[test]
    fn dropped_cache_surfaces_empty_for_clean_request() {
        // Arrange: no cache_control anywhere.
        let req = base_req();

        // Act + Assert
        assert!(dropped_cache_surfaces(&req).is_empty());
    }

    #[traced_test]
    #[test]
    fn warn_fires_for_cache_control_bearing_request() {
        // Arrange: a request carrying a top-level cache_control breakpoint.
        let mut req = base_req();
        req.cache_control = Some(CacheControl::ephemeral_5m());

        // Act
        let _ = translate("gemini:test", &req).expect("translate ok");

        // Assert: the drop diagnostic fired, consistent with the other
        // cache-less egresses.
        assert!(
            logs_contain("cache_control dropped"),
            "drop diagnostic must fire when cache_control is present"
        );
    }

    #[traced_test]
    #[test]
    fn no_warn_for_clean_request() {
        // Arrange: no cache_control -> no diagnostic.
        let req = base_req();

        // Act
        let _ = translate("gemini:test", &req).expect("translate ok");

        // Assert
        assert!(
            !logs_contain("cache_control dropped"),
            "no drop diagnostic without a cache_control marker"
        );
    }
}
