//! Shared canonical-ChatRequest builders for egress contract tests.
//!
//! Mirrors `crates/routectl-cli/tests/common/mod.rs`. Each scenario
//! returns a canonical `ChatRequest` that this crate's
//! `contract_egress` tests feed into each provider's
//! `normalize_request` as a snapshot baseline. See the cli crate's
//! mirror for ingress-side documentation, including the
//! canonical-shape caveat about per-ingress preservation of
//! `tool_choice` and tool-call history shapes.
//!
//! Mirror sync: field-level drift between the two `common/mod.rs`
//! files is caught at compile time (struct shape errors). SEMANTIC
//! drift -- divergent field values, different scenario fixtures,
//! out-of-sync doc comments -- has no compile tripwire. Review the
//! mirror file whenever editing a scenario here.
//!
//! When adding a scenario, add it here AND in the cli crate's mirror.

#![allow(dead_code)]

use routectl_core::{
    cache_control::CacheControl,
    content_part::{ContentPart, KnownContentPart},
    system_content::{SystemBlock, SystemContent},
    tool_def::{CustomTool, ToolDef},
    ChatRequest, ChatResponse, Choice, Message, MessageContent, Role,
};
use serde_json::json;

pub fn user_msg(text: &str) -> Message {
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

pub fn assistant_text_msg(text: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// Build a `get_weather` custom tool used by scenarios 2-3.
pub fn get_weather_tool() -> ToolDef {
    ToolDef::Custom(CustomTool {
        name: "get_weather".into(),
        description: Some("Get the current weather for a location".into()),
        input_schema: json!({
            "type": "object",
            "properties": {
                "location": {"type": "string"}
            },
            "required": ["location"]
        }),
        cache_control: None,
        defer_loading: None,
        strict: None,
        type_tag: None,
    })
}

pub mod scenarios {
    use super::*;

    /// Scenario 1: a single user turn with a top-level system prompt.
    /// Mirrors the cli-crate builder of the same name.
    pub fn scenario_1_system_handling() -> ChatRequest {
        ChatRequest {
            model: "claude-3-opus".into(),
            messages: vec![user_msg("Hello!")],
            system: Some(SystemContent::Text("You are a helpful assistant.".into())),
            max_tokens: Some(1024),
            ..Default::default()
        }
    }

    /// Scenario 2 (auto): one custom tool + `tool_choice: "auto"`.
    /// Mirrors the cli-crate builder of the same name.
    pub fn scenario_2_tool_choice_auto() -> ChatRequest {
        ChatRequest {
            model: "claude-3-opus".into(),
            messages: vec![user_msg("What is the weather?")],
            tools: Some(vec![get_weather_tool()]),
            tool_choice: Some(json!("auto")),
            max_tokens: Some(1024),
            ..Default::default()
        }
    }

    /// Scenario 2 (auto, Anthropic-ingress canonical): the
    /// Anthropic-shape object form of `tool_choice`. Mirrors the
    /// cli-crate builder of the same name.
    pub fn scenario_2_tool_choice_auto_anthropic_shape() -> ChatRequest {
        ChatRequest {
            model: "claude-3-opus".into(),
            messages: vec![user_msg("What is the weather?")],
            tools: Some(vec![get_weather_tool()]),
            tool_choice: Some(json!({"type": "auto"})),
            max_tokens: Some(1024),
            ..Default::default()
        }
    }

    /// Scenario 2 (named function): one custom tool + a
    /// `tool_choice` value that pins the model to one specific tool.
    /// Mirrors the cli-crate builder of the same name.
    pub fn scenario_2_tool_choice_named_function() -> ChatRequest {
        ChatRequest {
            model: "claude-3-opus".into(),
            messages: vec![user_msg("What is the weather?")],
            tools: Some(vec![get_weather_tool()]),
            tool_choice: Some(json!({
                "type": "function",
                "function": {"name": "get_weather"}
            })),
            max_tokens: Some(1024),
            ..Default::default()
        }
    }

    /// Scenario 3: a five-message history with a tool round-trip.
    /// Mirrors the cli-crate builder of the same name.
    pub fn scenario_3_multi_turn_with_tool_result() -> ChatRequest {
        let assistant_with_tool_use = Message {
            role: Role::Assistant,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::ToolUse {
                id: "toolu_01".into(),
                name: "get_weather".into(),
                input: json!({"location": "San Francisco"}),
                cache_control: None,
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };
        let tool_result = Message {
            role: Role::Tool,
            content: MessageContent::Text("72F and sunny".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: Some("toolu_01".into()),
            tool_calls: None,
        };
        ChatRequest {
            model: "claude-3-opus".into(),
            messages: vec![
                user_msg("What is the weather?"),
                assistant_with_tool_use,
                tool_result,
                assistant_text_msg("It is currently 72F and sunny in San Francisco."),
                user_msg("And tomorrow?"),
            ],
            tools: Some(vec![get_weather_tool()]),
            max_tokens: Some(1024),
            ..Default::default()
        }
    }

    /// Scenario 4 (end_turn): canonical `ChatResponse` with
    /// `finish_reason: "stop"`. Mirrors the cli-crate builder.
    pub fn scenario_4_response_stop_reason_end_turn() -> ChatResponse {
        ChatResponse {
            id: "msg_end_turn_01".into(),
            model: "claude-3-opus".into(),
            created: 0,
            choices: vec![Choice {
                index: 0,
                message: assistant_text_msg("Hello there!"),
                finish_reason: Some("stop".into()),
            }],
            usage: None,
            routectl_provider: None,
            extras: Default::default(),
        }
    }

    /// Scenario 4 (pause_turn): canonical `ChatResponse` whose
    /// `finish_reason` is the Anthropic-only `pause_turn` value.
    /// Mirrors the cli-crate builder.
    pub fn scenario_4_response_stop_reason_pause_turn() -> ChatResponse {
        ChatResponse {
            id: "msg_pause_turn_01".into(),
            model: "claude-3-opus".into(),
            created: 0,
            choices: vec![Choice {
                index: 0,
                message: assistant_text_msg("Pausing for tool result."),
                finish_reason: Some("pause_turn".into()),
            }],
            usage: None,
            routectl_provider: None,
            extras: Default::default(),
        }
    }

    /// Scenario 5: cache_control set on all four supported positions.
    /// Mirrors the cli-crate builder of the same name.
    pub fn scenario_5_cache_control_positions() -> ChatRequest {
        let user_with_cc_block = Message {
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                text: "Please review the attached document.".into(),
                cache_control: Some(CacheControl::ephemeral_5m()),
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };

        let cached_tool = ToolDef::Custom(CustomTool {
            name: "lookup_docs".into(),
            description: Some("Look up documentation".into()),
            input_schema: json!({
                "type": "object",
                "properties": {"query": {"type": "string"}}
            }),
            cache_control: Some(CacheControl::ephemeral_5m()),
            defer_loading: None,
            strict: None,
            type_tag: None,
        });

        let system_with_cc = SystemContent::Blocks(vec![SystemBlock {
            kind: "text".into(),
            text: "You are an assistant with long instructions.".into(),
            cache_control: Some(CacheControl::ephemeral_5m()),
            citations: None,
        }]);

        ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![user_with_cc_block],
            system: Some(system_with_cc),
            tools: Some(vec![cached_tool]),
            cache_control: Some(CacheControl::ephemeral_5m()),
            max_tokens: Some(1024),
            ..Default::default()
        }
    }
}
