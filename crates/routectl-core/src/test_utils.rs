//! Shared canonical-`ChatRequest` / `ChatResponse` builders for the
//! cross-crate contract tests.
//!
//! Each scenario function returns a canonical fixture used as BOTH the
//! assertion target for the ingress tests in `routectl-cli` and the
//! input fixture for the egress tests in `routectl-providers`. Both
//! crates pull these builders in via the `test-utils` feature on
//! `routectl-core` (declared as a dev-dependency), so there is a single
//! source of truth and no hand-maintained mirror.
//!
//! Gating: this module is compiled only under `cfg(test)` or the
//! `test-utils` feature, so the builders never ship in a release build.
//!
//! Canonical-shape caveat: routectl's canonical layer is intentionally
//! pass-through on certain fields (`tool_choice`, OpenAI-vs-Anthropic
//! tool-call representation in history). Each ingress preserves its
//! native wire shape rather than normalizing to one canonical, so the
//! Anthropic and OpenAI ingresses may legitimately produce DIFFERENT
//! canonical `ChatRequest` values from semantically equivalent inputs.
//! Each scenario's doc comment records which ingress shape it
//! represents; ingress tests for the other ingress assert on their own
//! native canonical, not on the shared builder.

#![allow(dead_code)]

use crate::{
    ChatRequest, ChatResponse, Choice, Message, MessageContent, ReasoningDetail,
    ReasoningDetailKind, Role,
    cache_control::CacheControl,
    content_part::{ContentPart, KnownContentPart},
    system_content::{SystemBlock, SystemContent},
    tool_def::{CustomTool, ToolDef},
};
use serde_json::json;

pub fn user_msg(text: &str) -> Message {
    Message {
        refusal: None,
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
        refusal: None,
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
    use super::{
        CacheControl, ChatRequest, ChatResponse, Choice, ContentPart, CustomTool, KnownContentPart,
        Message, MessageContent, ReasoningDetail, ReasoningDetailKind, Role, SystemBlock,
        SystemContent, ToolDef, assistant_text_msg, get_weather_tool, json, user_msg,
    };

    /// Scenario 1: a single user turn with a top-level system prompt.
    ///
    /// Anthropic ingress arrives with `system: "..."` (top-level
    /// string). OpenAI ingress arrives with a `Role::System` message
    /// inside `messages`; `lift_system_messages` hoists it into
    /// canonical `req.system`. Both flows must produce this exact
    /// canonical shape.
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
    ///
    /// Canonical-shape note: this builder represents the OpenAI-ingress
    /// canonical (bare string). The Anthropic ingress preserves its
    /// native object shape `{"type":"auto"}` -- see the sibling
    /// [`scenario_2_tool_choice_auto_anthropic_shape`] builder for
    /// that canonical, used by the matching egress tests so both
    /// canonical paths reach a snapshot.
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

    /// Scenario 2 (auto, Anthropic-ingress canonical): the same intent
    /// as [`scenario_2_tool_choice_auto`] but with the Anthropic-shape
    /// object form of `tool_choice`. Pinning the egress side of this
    /// canonical closes the gap where bare-string-only egress coverage
    /// left the Anthropic ingress -> Anthropic egress and Anthropic
    /// ingress -> openai-compat egress paths uncontract-tested.
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
    ///
    /// Canonical carries the OpenAI-shape object so the openai-compat
    /// egress can passthrough; the Anthropic egress's
    /// `translate_tool_choice` rewrites to
    /// `{"type":"tool","name":"get_weather"}`.
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
    ///
    /// Messages: user question -> assistant `tool_use` -> tool result
    /// -> assistant text response -> user follow-up.
    ///
    /// Alternation note: AWS Converse (and Anthropic Messages) require
    /// strict user/assistant alternation. After a `Role::Tool` message
    /// (which lowers to a user-role tool_result on the wire), the next
    /// non-tool message MUST be an assistant turn before another user
    /// message can land. The earlier four-message form (user ->
    /// assistant -> tool -> user) caused Bedrock-Converse to emit two
    /// adjacent `role:"user"` messages and would 400 on AWS in
    /// production; this five-message form mirrors what real Claude
    /// Code multi-turn flows actually send.
    ///
    /// Canonical-shape note: this builder represents the
    /// Anthropic-ingress canonical -- the assistant turn carries
    /// `ContentPart::Known(KnownContentPart::ToolUse{...})` on
    /// `message.content` (NOT on `message.tool_calls`) and the
    /// tool-result turn is a `Role::Tool` message with `tool_call_id`
    /// linking back to `ToolUse.id`. The OpenAI ingress preserves a
    /// different shape (assistant `tool_calls` array + `Role::Tool`
    /// message; no ToolUse content part), and the OpenAI ingress test
    /// for this scenario asserts the OpenAI-shape canonical directly.
    /// The egress tests feed this Anthropic-ingress canonical; both
    /// the Anthropic egress (preserves typed blocks) and the
    /// openai-compat egress (lowers to `tool_calls` + `role:"tool"`)
    /// must accept it.
    pub fn scenario_3_multi_turn_with_tool_result() -> ChatRequest {
        let assistant_with_tool_use = Message {
            refusal: None,
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
            refusal: None,
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
    /// `finish_reason: "stop"` -- the OpenAI mapping of Anthropic's
    /// `end_turn`. The Anthropic ingress's `render_response` must
    /// round-trip this back to `stop_reason: "end_turn"`.
    pub fn scenario_4_response_stop_reason_end_turn() -> ChatResponse {
        ChatResponse {
            id: "msg_end_turn_01".into(),
            model: "claude-3-opus".into(),
            created: 0,
            choices: vec![Choice {
                logprobs: None,
                index: 0,
                message: assistant_text_msg("Hello there!"),
                finish_reason: Some("stop".into()),
                matched_stop_sequence: None,
            }],
            usage: None,
            routectl_provider: None,
            extras: Default::default(),
            upstream_meta: None,
        }
    }

    /// Scenario 4 (pause_turn): canonical `ChatResponse` whose
    /// `finish_reason` is the Anthropic-only `pause_turn` value
    /// (passthrough preserved by `map_stop_reason` for non-overlap
    /// stop reasons). The Anthropic ingress's `render_response` must
    /// emit `stop_reason: "pause_turn"` -- NOT clobber it to
    /// `end_turn`.
    pub fn scenario_4_response_stop_reason_pause_turn() -> ChatResponse {
        ChatResponse {
            id: "msg_pause_turn_01".into(),
            model: "claude-3-opus".into(),
            created: 0,
            choices: vec![Choice {
                logprobs: None,
                index: 0,
                message: assistant_text_msg("Pausing for tool result."),
                finish_reason: Some("pause_turn".into()),
                matched_stop_sequence: None,
            }],
            usage: None,
            routectl_provider: None,
            extras: Default::default(),
            upstream_meta: None,
        }
    }

    /// Scenario 5: cache_control set on all four supported positions.
    ///
    /// (1) top-level (auto-cache), (2) a system block, (3) a tool
    /// definition, (4) a user message content block. All four must
    /// survive the Anthropic-in / Anthropic-out path verbatim; the
    /// openai-compat egress must silently drop every position.
    ///
    /// Ordering is constrained: cache prefix is tools -> system ->
    /// messages -> top-level, and 1h breakpoints must precede 5m.
    /// Pick a uniform `ephemeral_5m` so the validator accepts the
    /// four breakpoints together.
    pub fn scenario_5_cache_control_positions() -> ChatRequest {
        let user_with_cc_block = Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Text {
                text: "Please review the attached document.".into(),
                citations: None,
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

    /// Scenario 10: assistant history carries `reasoning_details`
    /// with the Anthropic `thinking` shape (format
    /// `anthropic-claude-v1`, payload `{text, signature}`).
    ///
    /// Multi-turn replay: a follow-up user message arrives after
    /// the assistant turn that included a thinking block. The
    /// Anthropic egress MUST emit the assistant turn with a
    /// `thinking` content block carrying both the text and the
    /// `signature` field. Anthropic 400s on `thinking` blocks
    /// missing `signature`; the contract test pins the round-trip
    /// so a regression in signature emission breaks CI rather than
    /// silently breaking every Claude 4.5 multi-turn after a
    /// tool-only thinking turn.
    pub fn scenario_10_reasoning_details_signature_replay() -> ChatRequest {
        let assistant_with_thinking = Message {
            refusal: None,
            role: Role::Assistant,
            content: MessageContent::Text("Sure, here is the answer: 42.".into()),
            reasoning: None,
            reasoning_details: vec![ReasoningDetail {
                kind: ReasoningDetailKind::Text,
                id: None,
                format: Some("anthropic-claude-v1".into()),
                index: Some(0),
                payload: json!({
                    "text": "The user is asking about the answer. 6 * 7 is 42.",
                    "signature": "sig_pretend_this_is_a_real_anthropic_signature_blob"
                }),
            }],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        };

        ChatRequest {
            model: "claude-opus-4-7".into(),
            messages: vec![
                user_msg("What is 6 times 7?"),
                assistant_with_thinking,
                user_msg("And 6 times 8?"),
            ],
            max_tokens: Some(1024),
            ..Default::default()
        }
    }

    /// Scenario 11: canonical `ChatResponse` with
    /// `matched_stop_sequence` set. Mirrors what Anthropic-shape
    /// egresses lift from the upstream wire `stop_sequence` field and
    /// what the openai-compat egress's suffix-match heuristic recovers
    /// from a `req.stop` list. The Anthropic ingress's `render_response`
    /// must emit `stop_reason:"stop_sequence"` + `stop_sequence:"<value>"`
    /// instead of the lossy `end_turn` mapping it would otherwise apply
    /// to the canonical `finish_reason:"stop"`. Bug class: structured-output
    /// flows mis-rendered as `end_turn`.
    pub fn scenario_11_response_matched_stop_sequence() -> ChatResponse {
        ChatResponse {
            id: "msg_stop_seq_01".into(),
            model: "claude-3-opus".into(),
            created: 0,
            choices: vec![Choice {
                logprobs: None,
                index: 0,
                message: assistant_text_msg("Here is the structured answer."),
                finish_reason: Some("stop".into()),
                matched_stop_sequence: Some("</answer>".into()),
            }],
            usage: None,
            routectl_provider: None,
            extras: Default::default(),
            upstream_meta: None,
        }
    }
}
