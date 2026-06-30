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

use serde_json::Value;

use routectl_core::{ChatRequest, ReasoningDetail, Result};
use routectl_core::{ContentPart, KnownContentPart, MessageContent, Role, ToolDef};

use super::types::{
    Content, FunctionCallPart, FunctionCallingConfig, FunctionDeclaration, FunctionResponsePart,
    GeminiTool, GenerateContentRequest, GenerationConfig, InlineData, Part, SystemInstruction,
    ThinkingConfig, ToolConfig,
};
use super::GEMINI_FORMAT;

/// Build a `GenerateContentRequest` from a canonical `ChatRequest`.
///
/// The config's `id` is used only for error attribution.
pub(crate) fn translate(provider_id: &str, req: &ChatRequest) -> Result<GenerateContentRequest> {
    let system_instruction = build_system_instruction(req);
    let contents = build_contents(provider_id, req)?;
    let (tools, tool_config) = build_tools_and_config(req);
    let generation_config = build_generation_config(req);

    Ok(GenerateContentRequest {
        contents,
        system_instruction,
        tools,
        tool_config,
        generation_config,
    })
}

// ---------------------------------------------------------------------------
// System instruction
// ---------------------------------------------------------------------------

fn build_system_instruction(req: &ChatRequest) -> Option<SystemInstruction> {
    let mut texts: Vec<String> = Vec::new();

    // Collect system-role messages.
    for msg in &req.messages {
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

    for msg in &req.messages {
        match msg.role {
            Role::System => {
                // Lifted into systemInstruction above; skip here.
            }
            Role::User => {
                let parts = content_to_parts(provider_id, &msg.content)?;
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
                parts.extend(content_to_parts(provider_id, &msg.content)?);
                // Assistant tool_calls -> functionCall parts.
                if let Some(tool_calls_raw) = &msg.tool_calls {
                    for tc in tool_calls_raw {
                        if let Some(p) = tool_call_to_function_call_part(provider_id, tc)? {
                            parts.push(p);
                        }
                    }
                }
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
                let tool_name = msg.name.clone().unwrap_or_default();
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

fn content_to_parts(provider_id: &str, content: &MessageContent) -> Result<Vec<Part>> {
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
                if let Some(part) = content_part_to_part(provider_id, p)? {
                    out.push(part);
                }
            }
            Ok(out)
        }
        MessageContent::Null => Ok(Vec::new()),
    }
}

fn content_part_to_part(provider_id: &str, part: &ContentPart) -> Result<Option<Part>> {
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
                if let Some(stripped) = url.strip_prefix("data:") {
                    if let Some(semi) = stripped.find(';') {
                        let mime = &stripped[..semi];
                        let rest = &stripped[semi + 1..];
                        if let Some(b64) = rest.strip_prefix("base64,") {
                            return Ok(Some(inline_data_part(InlineData {
                                mime_type: mime.to_string(),
                                data: b64.to_string(),
                            })));
                        }
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
                tool_use_id: _,
                ..
            } => {
                // ToolResult in a parts array -> treat as functionResponse.
                // We carry the content as the response body.
                Ok(Some(function_response_part(FunctionResponsePart {
                    name: String::new(),
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
    let func = match tc.get("function") {
        Some(f) => f,
        None => {
            tracing::debug!(
                provider = %provider_id,
                "gemini: tool_call missing 'function' field; skipping"
            );
            return Ok(None);
        }
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
    Ok(Some(function_call_part(FunctionCallPart { name, args })))
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

fn build_tools_and_config(req: &ChatRequest) -> (Option<Vec<GeminiTool>>, Option<ToolConfig>) {
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
                    parameters: Some(custom.input_schema.clone()),
                });
            }
            ToolDef::Other(v) => {
                // OpenAI-shape: {type:"function", function:{name,description,parameters}}
                let func = v.get("function").unwrap_or(v);
                let name = func
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let description = func
                    .get("description")
                    .and_then(Value::as_str)
                    .map(|s| s.to_string());
                let parameters = func.get("parameters").cloned();
                declarations.push(FunctionDeclaration {
                    name,
                    description,
                    parameters,
                });
            }
        }
    }

    let tools = if declarations.is_empty() {
        None
    } else {
        Some(vec![GeminiTool {
            function_declarations: declarations,
        }])
    };

    let tool_config = build_tool_config(req.tool_choice.as_ref());
    (tools, tool_config)
}

fn build_tool_config(tool_choice: Option<&Value>) -> Option<ToolConfig> {
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

/// Map the canonical `reasoning` controls to Gemini's `thinkingConfig`.
///
///   - `enabled: Some(false)`         -> None (reasoning explicitly off)
///   - explicit `max_tokens` (budget) -> that budget verbatim
///   - explicit `effort`              -> budget via the effort table
///   - reasoning present otherwise    -> dynamic budget (-1)
///
/// `include_thoughts` is true whenever thinking is on and not excluded,
/// so thought summaries stream back and map to canonical reasoning.
fn build_thinking_config(req: &ChatRequest) -> Option<ThinkingConfig> {
    let reasoning = req.reasoning.as_ref()?;
    if reasoning.enabled == Some(false) {
        return None;
    }

    let thinking_budget = if let Some(budget) = reasoning.max_tokens {
        Some(budget as i32)
    } else if let Some(effort) = reasoning.effort.as_deref() {
        crate::effort::budget_from_level(effort).map(|b| b as i32)
    } else {
        Some(THINKING_BUDGET_DYNAMIC)
    };

    let include_thoughts = reasoning.exclude != Some(true);

    Some(ThinkingConfig {
        thinking_budget,
        include_thoughts: Some(include_thoughts),
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

fn text_part(text: String) -> Part {
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
fn thought_part(text: String, signature: Option<String>) -> Part {
    Part {
        text: Some(text),
        inline_data: None,
        function_call: None,
        function_response: None,
        thought: Some(true),
        thought_signature: signature,
    }
}

fn inline_data_part(inline_data: InlineData) -> Part {
    Part {
        text: None,
        inline_data: Some(inline_data),
        function_call: None,
        function_response: None,
        thought: None,
        thought_signature: None,
    }
}

fn function_call_part(call: FunctionCallPart) -> Part {
    Part {
        text: None,
        inline_data: None,
        function_call: Some(call),
        function_response: None,
        thought: None,
        thought_signature: None,
    }
}

fn function_response_part(response: FunctionResponsePart) -> Part {
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
                .map(|s| s.to_string());
            Some(thought_part(text.to_string(), signature))
        })
        .collect()
}

/// Shallow-merge canonical `provider_extras` (the router's dispatch-time
/// merge of provider + model `payload_extras`) into the outgoing Gemini
/// body so operator-supplied knobs like `safetySettings`, `topK`, or
/// extra `generationConfig` fields reach the wire. Entries that collide
/// with a routectl-managed canonical key are dropped with a WARN so an
/// operator cannot clobber the assembled `contents` / `tools` / etc.
pub(crate) fn merge_payload_extras(provider_id: &str, body: &mut Value, extras: &Value) {
    let (Some(body_obj), Some(extra_obj)) = (body.as_object_mut(), extras.as_object()) else {
        return;
    };
    for (k, v) in extra_obj {
        if routectl_core::is_canonical_request_key(k) {
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
    let ext = filename
        .rfind('.')
        .map(|i| &filename[i + 1..])
        .unwrap_or("");
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
    use routectl_core::{ChatRequest, Message, MessageContent, Role};
    use serde_json::json;

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
            messages: vec![make_user("hello")],
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
            ],
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
            messages: vec![make_system("you are helpful"), make_user("hi")],
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
            }],
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
            }],
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
    fn tools_become_function_declarations() {
        use routectl_core::{CustomTool, ToolDef};
        let req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![make_user("call something")],
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
    fn tool_choice_none_maps_to_mode_none() {
        use routectl_core::{CustomTool, ToolDef};
        let req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![make_user("hi")],
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
            messages: vec![make_user("hi")],
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
            messages: vec![make_user("hi")],
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
        assert!(translate("gemini:test", &req)
            .expect("translate")
            .generation_config
            .is_none());
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

    #[test]
    fn response_format_json_schema_maps_to_response_schema() {
        let req = ChatRequest {
            model: "gemini-2.5-pro".into(),
            messages: vec![make_user("hi")],
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
            messages: vec![make_user("hi")],
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
            messages: vec![make_user("q"), assistant],
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
}
