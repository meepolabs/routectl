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

use routectl_core::{ChatRequest, Result};
use routectl_core::{ContentPart, KnownContentPart, MessageContent, Role, ToolDef};

use super::types::{
    Content, FunctionCallPart, FunctionCallingConfig, FunctionDeclaration, FunctionResponsePart,
    GeminiTool, GenerateContentRequest, GenerationConfig, InlineData, Part, SystemInstruction,
    ToolConfig,
};

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
                let mut parts = content_to_parts(provider_id, &msg.content)?;
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
                    parts: vec![Part {
                        text: None,
                        inline_data: None,
                        function_call: None,
                        function_response: Some(FunctionResponsePart {
                            name: tool_name,
                            response: response_content,
                        }),
                    }],
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
                Ok(Some(Part {
                    text: None,
                    inline_data: Some(InlineData {
                        mime_type: mime,
                        data,
                    }),
                    function_call: None,
                    function_response: None,
                }))
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
                            return Ok(Some(Part {
                                text: None,
                                inline_data: Some(InlineData {
                                    mime_type: mime.to_string(),
                                    data: b64.to_string(),
                                }),
                                function_call: None,
                                function_response: None,
                            }));
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
                Ok(Some(Part {
                    text: None,
                    inline_data: None,
                    function_call: Some(FunctionCallPart {
                        name: name.clone(),
                        args: input.clone(),
                    }),
                    function_response: None,
                }))
            }
            KnownContentPart::ToolResult {
                content,
                tool_use_id: _,
                ..
            } => {
                // ToolResult in a parts array -> treat as functionResponse.
                // We carry the content as the response body.
                Ok(Some(Part {
                    text: None,
                    inline_data: None,
                    function_call: None,
                    function_response: Some(FunctionResponsePart {
                        name: String::new(),
                        response: content.clone(),
                    }),
                }))
            }
            KnownContentPart::Thinking { thinking, .. } => {
                // Reasoning content: pass as text in this slice.
                // TODO(slice-2): emit as thought=true Part once thinkingConfig is wired.
                Ok(Some(text_part(thinking.clone())))
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
                Ok(Some(Part {
                    text: None,
                    inline_data: Some(InlineData {
                        mime_type: mime.to_string(),
                        data: data.to_string(),
                    }),
                    function_call: None,
                    function_response: None,
                }))
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
                Ok(Some(Part {
                    text: None,
                    inline_data: Some(InlineData {
                        mime_type: mime.to_string(),
                        data: data.to_string(),
                    }),
                    function_call: None,
                    function_response: None,
                }))
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
    Ok(Some(Part {
        text: None,
        inline_data: None,
        function_call: Some(FunctionCallPart { name, args }),
        function_response: None,
    }))
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
    let has_any = req.temperature.is_some()
        || req.top_p.is_some()
        || req.max_tokens.is_some()
        || req.stop.is_some();

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
    })
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
}
