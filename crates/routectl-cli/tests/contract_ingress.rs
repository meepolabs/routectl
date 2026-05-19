//! Contract tests for the ingress layer.
//!
//! Each scenario takes a real client wire body (Anthropic v1/messages
//! shape or OpenAI chat completions shape) and asserts that
//! `IngressAdapter::parse_request` produces the expected canonical
//! `ChatRequest` shape. For scenarios where the two ingresses
//! preserve different native shapes (e.g. `tool_choice`, tool-call
//! history), each ingress's test asserts its own canonical shape
//! rather than the shared `common::scenarios` builder; see the
//! canonical-shape caveat in `common::mod.rs`.
//!
//! See the sibling `contract_egress` tests in `routectl-providers` for
//! the canonical-to-upstream half. Scenario builders are mirrored in
//! both crates: field-level drift (struct shape changes) fails
//! compilation in both files; SEMANTIC drift between the mirrors is
//! not compile-checked, so review the mirror when editing scenarios.

mod common;

use axum::http::HeaderMap;
use routectl_cli::ingress::anthropic::AnthropicIngress;
use routectl_cli::ingress::openai::OpenAiIngress;
use routectl_cli::ingress::IngressAdapter;
use routectl_core::{
    cache_control::CacheControl, content_part::ContentPart, system_content::SystemContent,
    tool_def::ToolDef, KnownContentPart, MessageContent, Role,
};
use serde_json::json;

use common::scenarios;

// =====================================================================
// Scenario 1: system_handling
// =====================================================================
//
// Anthropic ingress: top-level `system: "..."` (string form).
// OpenAI ingress: `Role::System` message inside `messages`, lifted by
// `lift_system_messages` at parse time.
// Both must produce `req.system = SystemContent::Text("...")` with no
// `Role::System` messages left in `req.messages`.

#[test]
fn ingress_anthropic_system_handling() {
    let wire_body = json!({
        "model": "claude-3-opus",
        "system": "You are a helpful assistant.",
        "messages": [
            { "role": "user", "content": "Hello!" }
        ],
        "max_tokens": 1024
    });

    let req = AnthropicIngress
        .parse_request(&HeaderMap::new(), wire_body)
        .expect("anthropic ingress parse");

    let expected = scenarios::scenario_1_system_handling();

    assert_eq!(req.model, expected.model);
    assert_eq!(req.max_tokens, expected.max_tokens);
    assert_eq!(req.messages.len(), 1);
    assert!(matches!(req.messages[0].role, Role::User));

    match req
        .system
        .as_ref()
        .expect("anthropic ingress preserves top-level system")
    {
        SystemContent::Text(s) => assert_eq!(s, "You are a helpful assistant."),
        SystemContent::Blocks(_) => {
            panic!("string-form `system` must deserialize to SystemContent::Text")
        }
    }
}

#[test]
fn ingress_openai_system_handling() {
    let wire_body = json!({
        "model": "claude-3-opus",
        "messages": [
            { "role": "system", "content": "You are a helpful assistant." },
            { "role": "user", "content": "Hello!" }
        ],
        "max_tokens": 1024
    });

    let req = OpenAiIngress
        .parse_request(&HeaderMap::new(), wire_body)
        .expect("openai ingress parse");

    let expected = scenarios::scenario_1_system_handling();

    assert_eq!(req.model, expected.model);
    assert_eq!(req.max_tokens, expected.max_tokens);

    // System message must be lifted out of `messages` and into `req.system`.
    assert_eq!(
        req.messages.len(),
        1,
        "role:system message must be removed from messages array"
    );
    assert!(matches!(req.messages[0].role, Role::User));

    match req
        .system
        .as_ref()
        .expect("openai ingress must lift role:system into req.system")
    {
        SystemContent::Text(s) => assert_eq!(s, "You are a helpful assistant."),
        SystemContent::Blocks(_) => {
            panic!("openai ingress lift must produce SystemContent::Text")
        }
    }
}

// =====================================================================
// Scenario 2: tool_choice_translations
// =====================================================================
//
// Two sub-scenarios pin the canonical shape of `tool_choice` for the
// `auto` and named-function cases. The ingress is intentionally
// passthrough: per-egress translation lives in the providers crate so
// each upstream sees its native wire shape (Anthropic-shape object vs
// OpenAI-shape function pointer). Bug class caught: Bedrock validator
// rejects bare-string `tool_choice`.

#[test]
fn ingress_anthropic_tool_choice_auto() {
    let wire_body = json!({
        "model": "claude-3-opus",
        "messages": [
            { "role": "user", "content": "What is the weather?" }
        ],
        "tools": [
            {
                "name": "get_weather",
                "description": "Get the current weather for a location",
                "input_schema": {
                    "type": "object",
                    "properties": {"location": {"type": "string"}},
                    "required": ["location"]
                }
            }
        ],
        "tool_choice": {"type": "auto"},
        "max_tokens": 1024
    });

    let req = AnthropicIngress
        .parse_request(&HeaderMap::new(), wire_body)
        .expect("anthropic ingress parse");

    let expected = scenarios::scenario_2_tool_choice_auto();
    assert_eq!(req.model, expected.model);
    assert_eq!(req.max_tokens, expected.max_tokens);

    let tools = req.tools.as_ref().expect("tools must be preserved");
    assert_eq!(tools.len(), 1);
    match &tools[0] {
        ToolDef::Custom(c) => assert_eq!(c.name, "get_weather"),
        ToolDef::Other(v) => panic!(
            "anthropic ingress must deserialize anthropic-shape tools into Custom, got Other: {v}"
        ),
    }

    // Anthropic-shape tool_choice passes through verbatim.
    assert_eq!(req.tool_choice, Some(json!({"type": "auto"})));
}

#[test]
fn ingress_openai_tool_choice_auto() {
    let wire_body = json!({
        "model": "claude-3-opus",
        "messages": [
            { "role": "user", "content": "What is the weather?" }
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get the current weather for a location",
                    "parameters": {
                        "type": "object",
                        "properties": {"location": {"type": "string"}},
                        "required": ["location"]
                    }
                }
            }
        ],
        "tool_choice": "auto",
        "max_tokens": 1024
    });

    let req = OpenAiIngress
        .parse_request(&HeaderMap::new(), wire_body)
        .expect("openai ingress parse");

    assert_eq!(req.model, "claude-3-opus");
    assert_eq!(req.max_tokens, Some(1024));

    let tools = req.tools.as_ref().expect("tools must be preserved");
    assert_eq!(tools.len(), 1);
    // OpenAI ingress preserves `{type:"function",function:{...}}` as
    // `ToolDef::Other` so the openai-compat egress can emit verbatim
    // and the Anthropic egress's `translate_tool` lifts to Custom.
    match &tools[0] {
        ToolDef::Other(v) => {
            assert_eq!(v["type"], "function");
            assert_eq!(v["function"]["name"], "get_weather");
        }
        ToolDef::Custom(c) => panic!(
            "openai ingress must NOT pre-translate function tools (lossy); got Custom({})",
            c.name
        ),
    }

    // OpenAI-shape bare-string tool_choice passes through verbatim.
    assert_eq!(req.tool_choice, Some(json!("auto")));
}

#[test]
fn ingress_anthropic_tool_choice_named_function() {
    let wire_body = json!({
        "model": "claude-3-opus",
        "messages": [
            { "role": "user", "content": "What is the weather?" }
        ],
        "tools": [
            {
                "name": "get_weather",
                "description": "Get the current weather for a location",
                "input_schema": {
                    "type": "object",
                    "properties": {"location": {"type": "string"}},
                    "required": ["location"]
                }
            }
        ],
        "tool_choice": {"type": "tool", "name": "get_weather"},
        "max_tokens": 1024
    });

    let req = AnthropicIngress
        .parse_request(&HeaderMap::new(), wire_body)
        .expect("anthropic ingress parse");

    assert_eq!(req.model, "claude-3-opus");
    // Anthropic-shape tool_choice passes through verbatim.
    assert_eq!(
        req.tool_choice,
        Some(json!({"type": "tool", "name": "get_weather"}))
    );
}

#[test]
fn ingress_openai_tool_choice_named_function() {
    let wire_body = json!({
        "model": "claude-3-opus",
        "messages": [
            { "role": "user", "content": "What is the weather?" }
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get the current weather for a location",
                    "parameters": {
                        "type": "object",
                        "properties": {"location": {"type": "string"}},
                        "required": ["location"]
                    }
                }
            }
        ],
        "tool_choice": {"type": "function", "function": {"name": "get_weather"}},
        "max_tokens": 1024
    });

    let req = OpenAiIngress
        .parse_request(&HeaderMap::new(), wire_body)
        .expect("openai ingress parse");

    // OpenAI-shape tool_choice passes through verbatim. This is the
    // canonical shape the builder pins; the openai-compat egress emits
    // it as-is and the Anthropic egress's `translate_tool_choice`
    // rewrites it to `{"type":"tool","name":"get_weather"}`.
    let expected = scenarios::scenario_2_tool_choice_named_function();
    assert_eq!(req.tool_choice, expected.tool_choice);
}

// =====================================================================
// Scenario 3: multi_turn_with_tool_result
// =====================================================================
//
// Five-message history -> user -> assistant tool_use -> tool result ->
// assistant text -> user follow-up. AWS Converse (and Anthropic
// Messages) require strict user/assistant alternation, so a realistic
// flow includes the assistant's text response after the tool result
// before the next user turn. The ingress must preserve role boundaries
// and the `tool_use_id` linkage so each egress can reconstruct its
// native shape. The Anthropic ingress carries the assistant tool_use
// as a `KnownContentPart::ToolUse`; the OpenAI ingress carries the
// assistant tool_use as `msg.tool_calls` on a `Role::Assistant`
// message.

#[test]
fn ingress_anthropic_multi_turn_with_tool_result() {
    let wire_body = json!({
        "model": "claude-3-opus",
        "messages": [
            { "role": "user", "content": "What is the weather?" },
            {
                "role": "assistant",
                "content": [
                    {
                        "type": "tool_use",
                        "id": "toolu_01",
                        "name": "get_weather",
                        "input": {"location": "San Francisco"}
                    }
                ]
            },
            {
                "role": "user",
                "content": [
                    {
                        "type": "tool_result",
                        "tool_use_id": "toolu_01",
                        "content": "72F and sunny"
                    }
                ]
            },
            { "role": "assistant", "content": "It is currently 72F and sunny in San Francisco." },
            { "role": "user", "content": "And tomorrow?" }
        ],
        "tools": [
            {
                "name": "get_weather",
                "description": "Get the current weather for a location",
                "input_schema": {
                    "type": "object",
                    "properties": {"location": {"type": "string"}},
                    "required": ["location"]
                }
            }
        ],
        "max_tokens": 1024
    });

    let req = AnthropicIngress
        .parse_request(&HeaderMap::new(), wire_body)
        .expect("anthropic ingress parse");

    assert_eq!(req.messages.len(), 5);
    assert!(matches!(req.messages[0].role, Role::User));
    assert!(matches!(req.messages[1].role, Role::Assistant));
    assert!(matches!(req.messages[2].role, Role::User));
    assert!(matches!(req.messages[3].role, Role::Assistant));
    assert!(matches!(req.messages[4].role, Role::User));

    // Assistant turn carries a typed ToolUse content part.
    match &req.messages[1].content {
        MessageContent::Parts(parts) => {
            assert_eq!(parts.len(), 1);
            match &parts[0] {
                ContentPart::Known(KnownContentPart::ToolUse {
                    id, name, input, ..
                }) => {
                    assert_eq!(id, "toolu_01");
                    assert_eq!(name, "get_weather");
                    assert_eq!(input["location"], "San Francisco");
                }
                other => panic!("expected ToolUse, got {other:?}"),
            }
        }
        other => panic!("assistant message must carry Parts content, got {other:?}"),
    }

    // Tool result preserved as a typed ToolResult content part keyed
    // by `tool_use_id`.
    match &req.messages[2].content {
        MessageContent::Parts(parts) => match &parts[0] {
            ContentPart::Known(KnownContentPart::ToolResult { tool_use_id, .. }) => {
                assert_eq!(tool_use_id, "toolu_01");
            }
            other => panic!("expected ToolResult, got {other:?}"),
        },
        other => panic!("user tool-result message must carry Parts content, got {other:?}"),
    }

    // Assistant text response between tool_result and the next user
    // turn -- the alternation pin.
    match &req.messages[3].content {
        MessageContent::Text(t) => {
            assert_eq!(t, "It is currently 72F and sunny in San Francisco.")
        }
        other => panic!("message[3] expected Text content (assistant), got {other:?}"),
    }

    // Follow-up user turn: text content must round-trip verbatim.
    // Without this the test would not catch a regression that silently
    // merges or drops trailing messages.
    match &req.messages[4].content {
        MessageContent::Text(t) => assert_eq!(t, "And tomorrow?"),
        other => panic!("message[4] expected Text content (user), got {other:?}"),
    }
}

#[test]
fn ingress_openai_multi_turn_with_tool_result() {
    let wire_body = json!({
        "model": "claude-3-opus",
        "messages": [
            { "role": "user", "content": "What is the weather?" },
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [
                    {
                        "id": "toolu_01",
                        "type": "function",
                        "function": {
                            "name": "get_weather",
                            "arguments": "{\"location\":\"San Francisco\"}"
                        }
                    }
                ]
            },
            {
                "role": "tool",
                "tool_call_id": "toolu_01",
                "content": "72F and sunny"
            },
            { "role": "assistant", "content": "It is currently 72F and sunny in San Francisco." },
            { "role": "user", "content": "And tomorrow?" }
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "Get the current weather for a location",
                    "parameters": {
                        "type": "object",
                        "properties": {"location": {"type": "string"}},
                        "required": ["location"]
                    }
                }
            }
        ],
        "max_tokens": 1024
    });

    let req = OpenAiIngress
        .parse_request(&HeaderMap::new(), wire_body)
        .expect("openai ingress parse");

    assert_eq!(req.messages.len(), 5);
    assert!(matches!(req.messages[0].role, Role::User));
    assert!(matches!(req.messages[1].role, Role::Assistant));
    assert!(matches!(req.messages[2].role, Role::Tool));
    assert!(matches!(req.messages[3].role, Role::Assistant));
    assert!(matches!(req.messages[4].role, Role::User));

    // Assistant turn in OpenAI shape: `content: null` (no text body)
    // and `tool_calls` array carries the function-call shape verbatim.
    // Asserting on each subfield prevents a regression that drops
    // arguments-as-string serialization or strips the `type:"function"`
    // discriminator.
    assert!(
        matches!(req.messages[1].content, MessageContent::Null),
        "assistant tool-call turn must carry MessageContent::Null in OpenAI shape; got {:?}",
        req.messages[1].content
    );
    let tool_calls = req.messages[1]
        .tool_calls
        .as_ref()
        .expect("assistant must carry tool_calls in OpenAI shape");
    assert_eq!(tool_calls.len(), 1);
    assert_eq!(tool_calls[0]["id"], "toolu_01");
    assert_eq!(tool_calls[0]["type"], "function");
    assert_eq!(tool_calls[0]["function"]["name"], "get_weather");
    assert_eq!(
        tool_calls[0]["function"]["arguments"], "{\"location\":\"San Francisco\"}",
        "OpenAI function arguments are wire-serialized as a string, not an object"
    );

    // Tool result on Role::Tool message: linked by `tool_call_id` AND
    // content must round-trip verbatim.
    assert_eq!(
        req.messages[2].tool_call_id.as_deref(),
        Some("toolu_01"),
        "tool_call_id linkage must survive ingress"
    );
    match &req.messages[2].content {
        MessageContent::Text(t) => assert_eq!(t, "72F and sunny"),
        other => panic!("tool result must carry Text content, got {other:?}"),
    }

    // Assistant text response between tool_result and the next user
    // turn -- the alternation pin.
    match &req.messages[3].content {
        MessageContent::Text(t) => {
            assert_eq!(t, "It is currently 72F and sunny in San Francisco.")
        }
        other => panic!("message[3] expected Text content (assistant), got {other:?}"),
    }

    // Follow-up user turn: text content must round-trip verbatim.
    match &req.messages[4].content {
        MessageContent::Text(t) => assert_eq!(t, "And tomorrow?"),
        other => panic!("message[4] expected Text content (user), got {other:?}"),
    }
}

// =====================================================================
// Scenario 4: stop_reason_round_trip
// =====================================================================
//
// Response-side scenario: feed a canonical `ChatResponse` through the
// Anthropic ingress's `render_response` and assert the wire shape
// `stop_reason` survives round-trip for both the OpenAI-mapped value
// (`stop` -> `end_turn`) and the Anthropic-only passthrough
// (`pause_turn`). Only Anthropic ingress is tested because
// openai-compat does not have these stop reasons. Bug class caught:
// Anthropic-only stop reasons clobbered to `end_turn`.

#[test]
fn ingress_anthropic_render_stop_reason_end_turn() {
    let resp = scenarios::scenario_4_response_stop_reason_end_turn();

    let wire = AnthropicIngress
        .render_response(resp)
        .expect("anthropic ingress render");

    assert_eq!(wire["stop_reason"], "end_turn");
    assert_eq!(wire["id"], "msg_end_turn_01");
    assert_eq!(wire["role"], "assistant");
    assert_eq!(wire["type"], "message");
}

#[test]
fn ingress_anthropic_render_stop_reason_pause_turn() {
    let resp = scenarios::scenario_4_response_stop_reason_pause_turn();

    let wire = AnthropicIngress
        .render_response(resp)
        .expect("anthropic ingress render");

    // Anthropic-only stop reasons must passthrough verbatim --
    // they must NOT be clobbered to `end_turn`. Pre-fix the
    // legacy mapping would lose `pause_turn`, breaking
    // claude-code's per-stop-reason error handling.
    assert_eq!(
        wire["stop_reason"], "pause_turn",
        "pause_turn must passthrough verbatim, not clobber to end_turn"
    );
    assert_eq!(wire["id"], "msg_pause_turn_01");
}

// =====================================================================
// Scenario 5: cache_control_positions
// =====================================================================
//
// cache_control is set on all four supported positions on the wire
// (top-level, system block, tool definition, message content block).
// The Anthropic ingress must preserve every position verbatim into
// canonical. The OpenAI ingress is NOT tested for scenario 5 --
// cache_control is Anthropic-only on the wire and OpenAI clients
// never send it. Bug class caught: cache_control silently dropped at
// the ingress seam (would break the cc-via-anthropic-oauth path).

#[test]
fn ingress_anthropic_cache_control_positions() {
    let wire_body = json!({
        "model": "claude-opus-4-7",
        "system": [
            {
                "type": "text",
                "text": "You are an assistant with long instructions.",
                "cache_control": {"type": "ephemeral", "ttl": "5m"}
            }
        ],
        "messages": [
            {
                "role": "user",
                "content": [
                    {
                        "type": "text",
                        "text": "Please review the attached document.",
                        "cache_control": {"type": "ephemeral", "ttl": "5m"}
                    }
                ]
            }
        ],
        "tools": [
            {
                "name": "lookup_docs",
                "description": "Look up documentation",
                "input_schema": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}}
                },
                "cache_control": {"type": "ephemeral", "ttl": "5m"}
            }
        ],
        "cache_control": {"type": "ephemeral", "ttl": "5m"},
        "max_tokens": 1024
    });

    let req = AnthropicIngress
        .parse_request(&HeaderMap::new(), wire_body)
        .expect("anthropic ingress parse");

    // Position 1: top-level cache_control.
    let top = req
        .cache_control
        .as_ref()
        .expect("top-level cache_control must survive ingress");
    assert_eq!(top, &CacheControl::ephemeral_5m());

    // Position 2: system block cache_control.
    match req.system.as_ref().expect("system must be present") {
        SystemContent::Blocks(blocks) => {
            assert_eq!(blocks.len(), 1);
            let cc = blocks[0]
                .cache_control
                .as_ref()
                .expect("system block cache_control must survive ingress");
            assert_eq!(cc, &CacheControl::ephemeral_5m());
        }
        SystemContent::Text(_) => panic!("system with array form must deserialize to Blocks"),
    }

    // Position 3: tool definition cache_control.
    let tools = req.tools.as_ref().expect("tools must be preserved");
    match &tools[0] {
        ToolDef::Custom(c) => {
            let cc = c
                .cache_control
                .as_ref()
                .expect("tool cache_control must survive ingress");
            assert_eq!(cc, &CacheControl::ephemeral_5m());
        }
        ToolDef::Other(v) => panic!("custom tool must deserialize to ToolDef::Custom, got: {v}"),
    }

    // Position 4: message content block cache_control.
    match &req.messages[0].content {
        MessageContent::Parts(parts) => match &parts[0] {
            ContentPart::Known(KnownContentPart::Text { cache_control, .. }) => {
                let cc = cache_control
                    .as_ref()
                    .expect("user text block cache_control must survive ingress");
                assert_eq!(cc, &CacheControl::ephemeral_5m());
            }
            other => panic!("expected Text content part, got {other:?}"),
        },
        other => panic!("user message must carry Parts content, got {other:?}"),
    }
}
