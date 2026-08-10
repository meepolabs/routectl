// Shared fixtures for the `request_tests` module: the config builders (one per
// auth lane), the request/message constructors, and the translate-to-JSON
// helper. `include!`d into `request_tests.rs`, so these compile into THAT
// module and stay in scope for every fragment. All top-level imports live in
// the host `request_tests.rs`; do not add `use` lines here.

// ---------------------------------------------------------------------------
// Test scaffolding
// ---------------------------------------------------------------------------

fn cfg() -> OpenAiResponsesConfig {
    let mut c = OpenAiResponsesConfig::new("openai-responses:test", "literal:test");
    c.account_id = Some("acct-uuid".into());
    c.auth_kind = AuthKind::ChatgptOauth;
    c
}

fn req_with(messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        model: "gpt-5".into(),
        messages: messages.into(),
        ..Default::default()
    }
}

fn user_text(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn assistant_parts(parts: Vec<ContentPart>) -> Message {
    Message {
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Parts(parts),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn assistant_text(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn tool_message(call_id: &str, output: &str) -> Message {
    Message {
        refusal: None,
        role: Role::Tool,
        content: MessageContent::Text(output.into()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: Some(call_id.into()),
        tool_calls: None,
    }
}

/// Convert the request to a JSON Value -- avoids over-tying tests to
/// the struct internals when serde wire shape is what we care about.
fn translate_to_json(cfg: &OpenAiResponsesConfig, req: &ChatRequest) -> Value {
    let r = translate(cfg, req).expect("translate");
    serde_json::to_value(&r).expect("serialize")
}

/// The rendered error a rejected request produces. Panics when the
/// request translates, so a negative assertion can never go vacuous.
fn translate_err(cfg: &OpenAiResponsesConfig, req: &ChatRequest) -> String {
    match translate(cfg, req) {
        Ok(r) => panic!(
            "expected translation to fail, got: {}",
            serde_json::to_value(&r).expect("serialize")
        ),
        Err(e) => e.to_string(),
    }
}

/// A non-chatgpt-oauth config (api-key path) where store stays false by
/// default. Used to exercise the store override + include-forcing logic.
fn cfg_api_key() -> OpenAiResponsesConfig {
    let mut c = OpenAiResponsesConfig::new("openai-responses:test", "literal:test");
    c.auth_kind = AuthKind::ApiKey;
    c
}

/// A bedrock-mantle Responses config. The mantle lane must never persist
/// (`store` forced false regardless of any operator or model override).
fn cfg_bedrock_mantle() -> OpenAiResponsesConfig {
    let mut c = OpenAiResponsesConfig::new("openai-responses:mantle", "literal:bedrock-bearer");
    c.auth_kind = AuthKind::BedrockMantle;
    c
}

fn user_image_base64(media_type: &str, data: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Image {
            source: json!({
                "type": "base64",
                "media_type": media_type,
                "data": data
            }),
            cache_control: None,
        })]),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn user_image_url(url: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Image {
            source: json!({
                "type": "url",
                "url": url
            }),
            cache_control: None,
        })]),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn user_file(file: Value) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::File {
            file,
            cache_control: None,
        })]),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn user_parts(parts: Vec<ContentPart>) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(parts),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn image_part(source: Value) -> ContentPart {
    ContentPart::Known(KnownContentPart::Image {
        source,
        cache_control: None,
    })
}

fn image_url_part(image_url: Value) -> ContentPart {
    ContentPart::Known(KnownContentPart::ImageUrl {
        image_url,
        cache_control: None,
    })
}

fn text_part(text: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::Text {
        text: text.into(),
        citations: None,
        cache_control: None,
    })
}

fn tool_message_parts(call_id: &str, parts: Vec<ContentPart>) -> Message {
    Message {
        refusal: None,
        role: Role::Tool,
        content: MessageContent::Parts(parts),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: Some(call_id.into()),
        tool_calls: None,
    }
}

fn user_text_part_with_cc(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
            text: text.into(),
            citations: None,
            cache_control: Some(CacheControl::ephemeral_5m()),
        })]),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn custom_tool_with_cc(name: &str) -> ToolDef {
    ToolDef::Custom(CustomTool {
        name: name.into(),
        description: None,
        input_schema: json!({"type": "object"}),
        cache_control: Some(CacheControl::ephemeral_5m()),
        defer_loading: None,
        strict: None,
        type_tag: None,
    })
}
