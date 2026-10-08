//! Cross-lane guard for the OpenAI Responses hosted-MCP tool
//! (`{"type":"mcp", server_label, server_url, authorization, headers}`).
//!
//! The tool carries the caller's remote-server credentials. A Responses
//! upstream receives it verbatim; an Anthropic-shaped body (anthropic-api,
//! Bedrock Invoke) has no equivalent and withholds it, and Bedrock Converse
//! drops every non-function `ToolDef::Other` already (and an object
//! `tool_choice` it cannot read, which covers the hosted-MCP choice). A
//! hosted-MCP `tool_choice` names the withheld tool, so it is withheld too.
//! Each lane is checked on its SERIALIZED body, so a credential surviving
//! under any key is caught.
//!
//! SERIAL GUARD: every test here that sends a hosted-MCP tool through the
//! anthropic-api lane bumps `(anthropic, hosted_mcp_tool_withheld)`, so all of
//! them carry `anthropic_hosted_mcp_tool_withheld`, including the ones that do
//! not read the counter. A test that also sends the tool through Bedrock
//! Converse bumps `(bedrock-converse, builtin_tool_unrepresentable)` and owes
//! `bedrock_converse_builtin_tool_unrepresentable` as well.

use serde_json::{Value, json};

use routectl_core::{ChatRequest, CustomTool, Message, MessageContent, Role, ToolDef};

use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};

const AUTH_SENTINEL: &str = "sentinel-mcp-authorization-7f3a";
const HEADER_SENTINEL: &str = "sentinel-mcp-header-91c2";
const LABEL_SENTINEL: &str = "sentinel-mcp-label";
const URL_SENTINEL: &str = "https://sentinel-mcp.example.invalid/sse";
const FUNCTION_NAME: &str = "get_weather";

fn hosted_mcp_tool() -> Value {
    json!({
        "type": "mcp",
        "server_label": LABEL_SENTINEL,
        "server_url": URL_SENTINEL,
        "authorization": AUTH_SENTINEL,
        "headers": {"X-Api-Key": HEADER_SENTINEL},
        "require_approval": "never"
    })
}

fn function_tool() -> ToolDef {
    ToolDef::Custom(CustomTool {
        name: FUNCTION_NAME.into(),
        description: Some("weather lookup".into()),
        input_schema: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
        cache_control: None,
        defer_loading: None,
        strict: None,
        type_tag: None,
    })
}

fn request_with_tools(tools: Vec<ToolDef>) -> ChatRequest {
    ChatRequest {
        model: "claude-sonnet-4-5".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(64),
        tools: Some(tools),
        ..Default::default()
    }
}

fn mcp_and_function_request() -> ChatRequest {
    request_with_tools(vec![ToolDef::Other(hosted_mcp_tool()), function_tool()])
}

fn bedrock_cfg(api_shape: BedrockApiShape) -> BedrockConfig {
    BedrockConfig {
        id: "bedrock:test".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-haiku-4-5".into(),
        api_shape,
        creds: BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        allowed_betas: Vec::new(),
        additional_model_request_fields: None,
        adaptive_thinking: None,
    }
}

fn anthropic_api_body(req: &ChatRequest) -> String {
    let body =
        crate::anthropic_api::request::normalize("test", req, false, false, None, false, true)
            .expect("anthropic-api body assembles");
    serde_json::to_string(&body).expect("body serializes")
}

fn bedrock_invoke_body(req: &ChatRequest) -> String {
    let body =
        crate::bedrock::invoke::normalize_request(&bedrock_cfg(BedrockApiShape::Invoke), req)
            .expect("bedrock-invoke body assembles");
    serde_json::to_string(&body).expect("body serializes")
}

fn bedrock_converse_body(req: &ChatRequest) -> String {
    let body =
        crate::bedrock::converse::normalize_request(&bedrock_cfg(BedrockApiShape::Converse), req)
            .expect("bedrock-converse body assembles");
    serde_json::to_string(&body).expect("body serializes")
}

fn responses_body(req: &ChatRequest) -> Value {
    let cfg = crate::openai_responses::OpenAiResponsesConfig::new(
        "openai-responses:test",
        "literal:test",
    );
    let translated =
        crate::openai_responses::request::translate(&cfg, req).expect("responses body translates");
    serde_json::to_value(&translated).expect("body serializes")
}

fn hosted_mcp_withhold_count() -> u64 {
    crate::translation_drop_metrics::translation_policy_action_snapshot()
        .into_iter()
        .find(|e| {
            e.lane == crate::anthropic_api::LANE && e.policy_class == "hosted_mcp_tool_withheld"
        })
        .map_or(0, |e| e.action_count)
}

fn assert_no_mcp_value(lane: &str, body: &str) {
    for sentinel in [AUTH_SENTINEL, HEADER_SENTINEL, LABEL_SENTINEL, URL_SENTINEL] {
        assert!(
            !body.contains(sentinel),
            "{lane}: the hosted-MCP tool's {sentinel:?} reached the wire body: {body}"
        );
    }
}

#[test]
#[serial_test::serial(
    anthropic_hosted_mcp_tool_withheld,
    bedrock_converse_builtin_tool_unrepresentable
)]
fn a_hosted_mcp_tool_never_reaches_an_anthropic_shaped_body_and_the_function_tool_survives() {
    // Arrange
    let req = mcp_and_function_request();

    // Act
    let bodies = [
        ("anthropic-api", anthropic_api_body(&req)),
        ("bedrock-invoke", bedrock_invoke_body(&req)),
        ("bedrock-converse", bedrock_converse_body(&req)),
    ];

    // Assert
    for (lane, body) in &bodies {
        assert_no_mcp_value(lane, body);
        assert!(
            body.contains(FUNCTION_NAME),
            "{lane}: the function tool beside the hosted-MCP tool must still ship: {body}"
        );
    }
}

#[test]
#[serial_test::serial(anthropic_hosted_mcp_tool_withheld)]
fn the_withhold_warns_once_per_request_naming_no_tool_value() {
    // Arrange -- two hosted-MCP tools on one request are one WARN.
    let req = request_with_tools(vec![
        ToolDef::Other(hosted_mcp_tool()),
        ToolDef::Other(hosted_mcp_tool()),
        function_tool(),
    ]);

    // Act
    let events = routectl_testkit::capture_events(|| {
        let _ = anthropic_api_body(&req);
    });

    // Assert
    let warns: Vec<_> = events
        .iter()
        .filter(|e| {
            e.level == tracing::Level::WARN && e.message.contains("hosted-MCP tool withheld")
        })
        .collect();
    assert_eq!(warns.len(), 1, "one WARN per request; got: {events:?}");
    assert_eq!(warns[0].field("count"), Some("2"));
    assert_eq!(warns[0].field("tool_type"), Some("mcp"));
    let rendered = format!("{events:?}");
    assert_no_mcp_value("anthropic-api log", &rendered);
}

#[test]
#[serial_test::serial(anthropic_hosted_mcp_tool_withheld)]
fn an_anthropic_request_carrying_a_hosted_mcp_tool_counts_one_policy_action() {
    // Arrange -- two hosted-MCP tools; the count is per REQUEST.
    let req = request_with_tools(vec![
        ToolDef::Other(hosted_mcp_tool()),
        ToolDef::Other(hosted_mcp_tool()),
    ]);

    // Act
    let before = hosted_mcp_withhold_count();
    let _ = anthropic_api_body(&req);
    let after = hosted_mcp_withhold_count();

    // Assert
    assert_eq!(
        after - before,
        1,
        "one withholding request is one policy action"
    );
}

#[test]
#[serial_test::serial(anthropic_hosted_mcp_tool_withheld)]
fn a_bedrock_invoke_withhold_is_not_credited_to_the_anthropic_lane() {
    // Arrange
    let req = mcp_and_function_request();

    // Act
    let before = hosted_mcp_withhold_count();
    let body = bedrock_invoke_body(&req);
    let after = hosted_mcp_withhold_count();

    // Assert -- the tool is still withheld on Invoke, but counted nowhere.
    assert_no_mcp_value("bedrock-invoke", &body);
    assert_eq!(after, before, "an Invoke request belongs to another lane");
}

#[test]
#[serial_test::serial(anthropic_hosted_mcp_tool_withheld)]
fn a_background_probe_carrying_a_hosted_mcp_tool_counts_nothing() {
    // Arrange
    let mut probe = mcp_and_function_request();
    probe.routectl_internal.background_probe = true;

    // Act
    let before = hosted_mcp_withhold_count();
    let body = anthropic_api_body(&probe);
    let after = hosted_mcp_withhold_count();

    // Assert -- withheld from the wire all the same.
    assert_no_mcp_value("anthropic-api probe", &body);
    assert_eq!(
        after, before,
        "routectl's own probe traffic is not client traffic"
    );
}

#[test]
fn an_anthropic_builtin_still_forwards_verbatim() {
    // Arrange -- positive control: an Anthropic server tool rides
    // `ToolDef::Other` exactly like the hosted-MCP tool does.
    let builtin = json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 3});
    let req = request_with_tools(vec![ToolDef::Other(builtin.clone()), function_tool()]);

    // Act
    let anthropic: Value = serde_json::from_str(&anthropic_api_body(&req)).expect("json");
    let invoke: Value = serde_json::from_str(&bedrock_invoke_body(&req)).expect("json");

    // Assert
    for (lane, body) in [("anthropic-api", anthropic), ("bedrock-invoke", invoke)] {
        let tools = body["tools"].as_array().expect("tools array");
        assert_eq!(tools.len(), 2, "{lane}: {body}");
        assert_eq!(tools[0], builtin, "{lane}: the builtin forwards verbatim");
        assert_eq!(tools[1]["name"], FUNCTION_NAME, "{lane}: {body}");
    }
}

#[test]
fn an_anthropic_mcp_toolset_is_not_mistaken_for_the_responses_hosted_tool() {
    // Arrange -- Anthropic's own MCP connector tool type.
    let toolset = json!({"type": "mcp_toolset", "mcp_server_name": "docs"});
    let req = request_with_tools(vec![ToolDef::Other(toolset.clone())]);

    // Act
    let body: Value = serde_json::from_str(&anthropic_api_body(&req)).expect("json");

    // Assert
    assert_eq!(body["tools"], json!([toolset]));
}

#[test]
fn a_responses_egress_forwards_the_hosted_mcp_tool_byte_for_byte() {
    // Arrange
    let mcp = hosted_mcp_tool();
    let req = mcp_and_function_request();

    // Act
    let body = responses_body(&req);

    // Assert
    let tools = body["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 2, "{body}");
    assert_eq!(
        serde_json::to_string(&tools[0]).expect("serializes"),
        serde_json::to_string(&mcp).expect("serializes"),
        "a same-dialect upstream receives the hosted-MCP tool unchanged"
    );
    assert_eq!(tools[1]["name"], FUNCTION_NAME, "{body}");
}

const CHOICE_NAME_SENTINEL: &str = "sentinel-mcp-choice-tool";

fn hosted_mcp_tool_choice() -> Value {
    json!({"type": "mcp", "server_label": LABEL_SENTINEL, "name": CHOICE_NAME_SENTINEL})
}

fn with_tool_choice(mut req: ChatRequest, tool_choice: Value) -> ChatRequest {
    req.tool_choice = Some(tool_choice);
    req
}

fn anthropic_shaped_bodies(req: &ChatRequest) -> [(&'static str, Value); 2] {
    let parse = |body: String| serde_json::from_str::<Value>(&body).expect("json");
    [
        ("anthropic-api", parse(anthropic_api_body(req))),
        ("bedrock-invoke", parse(bedrock_invoke_body(req))),
    ]
}

#[test]
#[serial_test::serial(anthropic_hosted_mcp_tool_withheld)]
fn a_hosted_mcp_tool_choice_becomes_auto_and_names_nothing_on_an_anthropic_shaped_body() {
    // Arrange
    let req = with_tool_choice(mcp_and_function_request(), hosted_mcp_tool_choice());

    // Act
    let bodies = anthropic_shaped_bodies(&req);

    // Assert
    for (lane, body) in &bodies {
        assert_eq!(
            body["tool_choice"],
            json!({"type": "auto"}),
            "{lane}: {body}"
        );
        let rendered = body.to_string();
        assert_no_mcp_value(lane, &rendered);
        assert!(
            !rendered.contains(CHOICE_NAME_SENTINEL),
            "{lane}: the hosted-MCP tool_choice name reached the wire body: {rendered}"
        );
    }
}

#[test]
#[serial_test::serial(anthropic_hosted_mcp_tool_withheld)]
fn a_hosted_mcp_tool_choice_with_no_tool_left_on_the_wire_is_omitted() {
    // Arrange -- the only tool is the withheld one, so there is nothing for
    // even an `auto` choice to select.
    let req = with_tool_choice(
        request_with_tools(vec![ToolDef::Other(hosted_mcp_tool())]),
        hosted_mcp_tool_choice(),
    );

    // Act
    let bodies = anthropic_shaped_bodies(&req);

    // Assert
    for (lane, body) in &bodies {
        assert!(body.get("tool_choice").is_none(), "{lane}: {body}");
        assert!(
            !body.to_string().contains(CHOICE_NAME_SENTINEL),
            "{lane}: {body}"
        );
    }
}

#[test]
#[serial_test::serial(anthropic_hosted_mcp_tool_withheld)]
fn anthropic_tool_choices_beside_a_withheld_hosted_mcp_tool_are_unchanged() {
    for choice in [
        json!({"type": "tool", "name": FUNCTION_NAME}),
        json!({"type": "any"}),
    ] {
        // Arrange -- positive control: the mcp tool is still withheld, but a
        // choice that is not the hosted-MCP shape rides through as before.
        let req = with_tool_choice(mcp_and_function_request(), choice.clone());

        // Act
        let bodies = anthropic_shaped_bodies(&req);

        // Assert
        for (lane, body) in &bodies {
            assert_eq!(body["tool_choice"], choice, "{lane}: {body}");
        }
    }
}

#[test]
#[serial_test::serial(anthropic_hosted_mcp_tool_withheld)]
fn a_hosted_mcp_tool_choice_alone_counts_one_policy_action_and_warns_without_values() {
    // Arrange -- no hosted-MCP tool, so only the choice site can record.
    let req = with_tool_choice(
        request_with_tools(vec![function_tool()]),
        hosted_mcp_tool_choice(),
    );

    // Act
    let before = hosted_mcp_withhold_count();
    let events = routectl_testkit::capture_events(|| {
        let _ = anthropic_api_body(&req);
    });
    let after = hosted_mcp_withhold_count();

    // Assert
    assert_eq!(
        after - before,
        1,
        "the withheld choice is one policy action"
    );
    let warns: Vec<_> = events
        .iter()
        .filter(|e| {
            e.level == tracing::Level::WARN && e.message.contains("hosted-MCP tool withheld")
        })
        .collect();
    assert_eq!(warns.len(), 1, "one WARN per request; got: {events:?}");
    assert_eq!(warns[0].field("tool_choice_withheld"), Some("true"));
    assert_eq!(warns[0].field("count"), Some("0"));
    let rendered = format!("{events:?}");
    assert!(!rendered.contains(CHOICE_NAME_SENTINEL), "{rendered}");
    assert!(!rendered.contains(LABEL_SENTINEL), "{rendered}");
}
