use serde_json::json;

use crate::ingress::IngressAdapter;
use crate::ingress::anthropic::AnthropicIngress;

use super::*;

#[test]
fn render_response_emits_messages_shape() {
    use routectl_core::{Message, Role, Usage, schema::Choice};
    let resp = ChatResponse {
        id: "msg_01".into(),
        model: "claude-opus-4-7".into(),
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("hi there".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            ..Default::default()
        }),
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    };
    let v = AnthropicIngress.render_response_value(resp).unwrap();
    assert_eq!(v["id"], "msg_01");
    assert_eq!(v["type"], "message");
    assert_eq!(v["role"], "assistant");
    assert_eq!(v["content"][0]["type"], "text");
    assert_eq!(v["content"][0]["text"], "hi there");
    assert_eq!(v["stop_reason"], "end_turn");
    assert_eq!(v["usage"]["input_tokens"], 10);
    assert_eq!(v["usage"]["output_tokens"], 5);
}

/// Ingress-layer serialization pin: the Anthropic ingress renders
/// `resp.model` verbatim into the non-streaming response body. The
/// relabel itself happens upstream in the router (which rewrites
/// `resp.model` to the client-visible label -- requested alias by
/// default, or a per-model `reported_model` override). This test proves
/// only that whatever label the response carries is passed through
/// unchanged into the rendered `model` field; router-integration
/// coverage lives in tests/router.rs and src/router.rs.
#[test]
fn render_response_surfaces_router_model_label_verbatim() {
    use routectl_core::{Message, Role, Usage, schema::Choice};
    // Arrange: a response stamped with a client-visible label.
    let resp = ChatResponse {
        id: "msg_01".into(),
        model: "public-label".into(),
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("hi there".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(Usage::default()),
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    };

    // Act
    let v = AnthropicIngress.render_response_value(resp).unwrap();

    // Assert
    assert_eq!(v["model"], "public-label");
}

/// Bug D (cc-via-* 2026-05-18): openai-responses and anthropic-api
/// non-streaming responses populate BOTH `msg.tool_calls`
/// (OpenAI shape) AND a typed `ContentPart::ToolUse` on
/// `msg.content` for the same upstream function_call. Without
/// dedup, the renderer emits two identical tool_use blocks
/// back-to-back in the Anthropic `content` array. Pin: only ONE
/// tool_use block per call_id, with the parts-native shape
/// preserved.
#[test]
fn render_response_dedupes_tool_use_when_present_in_both_tool_calls_and_parts() {
    use routectl_core::{
        ContentPart, KnownContentPart, Message, MessageContent, Role, Usage, schema::Choice,
    };
    let resp = ChatResponse {
        id: "msg_dup".into(),
        model: "gpt-5".into(),
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![
                    ContentPart::Known(KnownContentPart::Text {
                        text: "I'll compute that".into(),
                        citations: None,
                        cache_control: None,
                    }),
                    ContentPart::Known(KnownContentPart::ToolUse {
                        id: "call_dup".into(),
                        name: "calculator".into(),
                        input: json!({"x": 1, "y": 2}),
                        cache_control: None,
                    }),
                ]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_dup",
                    "type": "function",
                    "function": {
                        "name": "calculator",
                        "arguments": "{\"x\":1,\"y\":2}"
                    }
                })]),
            },
            finish_reason: Some("tool_calls".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 10,
            total_tokens: 15,
            ..Default::default()
        }),
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    };
    let v = AnthropicIngress.render_response_value(resp).unwrap();
    let content = v["content"].as_array().expect("content is array");
    // Count tool_use blocks for the dup id.
    let tool_uses_for_id: Vec<&Value> = content
        .iter()
        .filter(|b| b["type"] == "tool_use" && b["id"] == "call_dup")
        .collect();
    assert_eq!(
        tool_uses_for_id.len(),
        1,
        "expected exactly one tool_use block for call_dup, got content: {content:?}",
    );
    // The surviving block carries Anthropic-native shape (parts source).
    assert_eq!(tool_uses_for_id[0]["name"], "calculator");
    assert_eq!(tool_uses_for_id[0]["input"]["x"], 1);
    assert_eq!(tool_uses_for_id[0]["input"]["y"], 2);
    // Text block also rendered.
    let text_blocks: Vec<&Value> = content.iter().filter(|b| b["type"] == "text").collect();
    assert_eq!(text_blocks.len(), 1);
    assert_eq!(text_blocks[0]["text"], "I'll compute that");
}

/// Counterpart: openai-compat populates ONLY `msg.tool_calls`
/// (parts is empty / Text). The renderer must still emit one
/// tool_use block per call so this code path doesn't regress
/// when the dedup set is empty.
#[test]
fn render_response_emits_tool_use_from_tool_calls_when_parts_has_no_tool_use() {
    use routectl_core::{Message, MessageContent, Role, Usage, schema::Choice};
    let resp = ChatResponse {
        id: "msg_oc".into(),
        model: "qwen-3-coder".into(),
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                // openai-compat doesn't populate parts.ToolUse; content
                // is the model's plain text reply.
                content: MessageContent::Text("running tool now".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_oc",
                    "type": "function",
                    "function": {
                        "name": "ls",
                        "arguments": "{\"path\":\"/tmp\"}"
                    }
                })]),
            },
            finish_reason: Some("tool_calls".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 10,
            total_tokens: 15,
            ..Default::default()
        }),
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    };
    let v = AnthropicIngress.render_response_value(resp).unwrap();
    let content = v["content"].as_array().expect("content is array");
    let tool_uses: Vec<&Value> = content.iter().filter(|b| b["type"] == "tool_use").collect();
    assert_eq!(tool_uses.len(), 1);
    assert_eq!(tool_uses[0]["id"], "call_oc");
    assert_eq!(tool_uses[0]["name"], "ls");
    assert_eq!(tool_uses[0]["input"]["path"], "/tmp");
}

/// Review follow-up to Bug D: the pre-scan must also recognize
/// `ContentPart::Other` entries whose `type_tag` is "tool_use".
/// A future Anthropic sub-field on the tool_use block would
/// cause the deserializer to fall through to Other; without
/// this branch, the dedup HashSet is blind to it and a
/// duplicate emit reappears on the all-Anthropic path.
#[test]
fn render_response_dedupes_tool_use_when_parts_carries_other_typed_tool_use() {
    use routectl_core::{ContentPart, Message, MessageContent, Role, Usage, schema::Choice};
    let mut extras = serde_json::Map::new();
    extras.insert("id".into(), Value::String("call_future".into()));
    extras.insert("name".into(), Value::String("future_tool".into()));
    extras.insert("input".into(), json!({"k": "v"}));
    // Hypothetical future sub-field that breaks
    // KnownContentPart::ToolUse's serde struct.
    extras.insert("future_subfield".into(), Value::Bool(true));
    let resp = ChatResponse {
        id: "msg_future".into(),
        model: "claude-opus-4-7".into(),
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![ContentPart::Other {
                    type_tag: "tool_use".into(),
                    cache_control: None,
                    extras,
                }]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_future",
                    "type": "function",
                    "function": {"name": "future_tool", "arguments": "{\"k\":\"v\"}"}
                })]),
            },
            finish_reason: Some("tool_calls".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 10,
            total_tokens: 15,
            ..Default::default()
        }),
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    };
    let v = AnthropicIngress.render_response_value(resp).unwrap();
    let content = v["content"].as_array().expect("content is array");
    let tool_uses_for_id: Vec<&Value> = content
        .iter()
        .filter(|b| b["type"] == "tool_use" && b["id"] == "call_future")
        .collect();
    assert_eq!(
        tool_uses_for_id.len(),
        1,
        "Other-typed tool_use must dedupe against tool_calls: {content:?}",
    );
}

/// Review follow-up to Bug D / monotonicity: the pre-scan must
/// NOT incorrectly dedupe an Other-typed block that lacks an
/// `id` extras field. Without the id, we cannot prove the
/// parts version is the same call as a tool_calls entry; emit
/// both rather than mis-dropping the tool_calls one.
#[test]
fn render_response_does_not_dedupe_other_tool_use_when_id_missing() {
    use routectl_core::{ContentPart, Message, MessageContent, Role, Usage, schema::Choice};
    let mut extras = serde_json::Map::new();
    // No `id` field on the Other block.
    extras.insert("name".into(), Value::String("anon".into()));
    let resp = ChatResponse {
        id: "msg_anon".into(),
        model: "claude-opus-4-7".into(),
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Parts(vec![ContentPart::Other {
                    type_tag: "tool_use".into(),
                    cache_control: None,
                    extras,
                }]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_oc",
                    "type": "function",
                    "function": {"name": "ls", "arguments": "{}"}
                })]),
            },
            finish_reason: Some("tool_calls".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 10,
            total_tokens: 15,
            ..Default::default()
        }),
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    };
    let v = AnthropicIngress.render_response_value(resp).unwrap();
    let content = v["content"].as_array().expect("content is array");
    // tool_calls entry still emits even though parts has an Other tool_use
    // (the parts block is also rendered as-is in the parts iteration).
    let tool_uses: Vec<&Value> = content.iter().filter(|b| b["type"] == "tool_use").collect();
    assert!(
        tool_uses.iter().any(|b| b["id"] == "call_oc"),
        "tool_calls entry must still emit when Other has no id: {content:?}",
    );
}

/// Real Anthropic 400s on `signature: null` mid-conversation when the
/// next provider in a switch is claude-sonnet replaying a prior turn's
/// thinking block produced by a non-Anthropic upstream. A canonical
/// `ReasoningDetail` whose payload has no `signature` key (or whose
/// signature is null) must render to a thinking block with NO
/// `signature` key on the wire -- not `signature: null`.
#[test]
fn render_response_omits_signature_key_when_payload_has_none() {
    use routectl_core::{
        Message, MessageContent, ReasoningDetail, ReasoningDetailKind, Role, Usage, schema::Choice,
    };
    let resp = ChatResponse {
        id: "msg_no_sig".into(),
        model: "deepseek-v4-pro".into(),
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("answer".into()),
                reasoning: None,
                reasoning_details: vec![ReasoningDetail {
                    kind: ReasoningDetailKind::Text,
                    id: Some("rd_1".into()),
                    format: Some("deepseek-v1".into()),
                    index: Some(0),
                    payload: json!({"text": "let me think"}),
                }],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 10,
            total_tokens: 15,
            ..Default::default()
        }),
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    };
    let v = AnthropicIngress.render_response_value(resp).unwrap();
    let content = v["content"].as_array().expect("content is array");
    let thinking = content
        .iter()
        .find(|b| b["type"] == "thinking")
        .expect("thinking block present");
    assert_eq!(thinking["thinking"], "let me think");
    let obj = thinking
        .as_object()
        .expect("thinking block is a JSON object");
    assert!(
        !obj.contains_key("signature"),
        "no signature key when payload has none, got: {thinking}"
    );
}

/// Counterpart: an Anthropic-shape detail with a non-null signature
/// must round-trip the signature verbatim. Pins that the omit logic
/// only fires on absent / null signatures.
#[test]
fn render_response_emits_signature_verbatim_when_payload_has_one() {
    use routectl_core::{
        Message, MessageContent, ReasoningDetail, ReasoningDetailKind, Role, Usage, schema::Choice,
    };
    let resp = ChatResponse {
        id: "msg_signed".into(),
        model: "claude-opus-4-7".into(),
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("answer".into()),
                reasoning: None,
                reasoning_details: vec![ReasoningDetail {
                    kind: ReasoningDetailKind::Text,
                    id: Some("rd_1".into()),
                    format: Some("anthropic-claude-v1".into()),
                    index: Some(0),
                    payload: json!({"text": "let me think", "signature": "abc123"}),
                }],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 10,
            total_tokens: 15,
            ..Default::default()
        }),
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    };
    let v = AnthropicIngress.render_response_value(resp).unwrap();
    let content = v["content"].as_array().expect("content is array");
    let thinking = content
        .iter()
        .find(|b| b["type"] == "thinking")
        .expect("thinking block present");
    assert_eq!(thinking["signature"], "abc123");
}

/// Summary-kind details (OpenAI Responses per-step summaries) must
/// follow the same contract: no `signature` key on the wire when the
/// payload doesn't carry one.
#[test]
fn render_response_summary_kind_omits_signature_key_when_absent() {
    use routectl_core::{
        Message, MessageContent, ReasoningDetail, ReasoningDetailKind, Role, Usage, schema::Choice,
    };
    let resp = ChatResponse {
        id: "msg_summary".into(),
        model: "gpt-5".into(),
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("answer".into()),
                reasoning: None,
                reasoning_details: vec![ReasoningDetail {
                    kind: ReasoningDetailKind::Summary,
                    id: Some("rd_1".into()),
                    format: Some("openai-responses-v1".into()),
                    index: Some(0),
                    payload: json!({"text": "step summary"}),
                }],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 10,
            total_tokens: 15,
            ..Default::default()
        }),
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    };
    let v = AnthropicIngress.render_response_value(resp).unwrap();
    let content = v["content"].as_array().expect("content is array");
    let thinking = content
        .iter()
        .find(|b| b["type"] == "thinking")
        .expect("Summary thinking block present");
    assert_eq!(thinking["thinking"], "step summary");
    let obj = thinking
        .as_object()
        .expect("thinking block is a JSON object");
    assert!(
        !obj.contains_key("signature"),
        "no signature key on Summary thinking when payload has none, got: {thinking}"
    );
}

/// An unrecognized detail kind must get the identical best-effort
/// forward as Summary: surface `payload.text` as a thinking block
/// rather than dropping it, since this render path forwards what can
/// be displayed. Paired with the Summary test above (same code path,
/// recognized kind) as the positive control.
#[test]
fn render_response_unrecognized_kind_falls_back_to_thinking_block() {
    use routectl_core::{
        Message, MessageContent, ReasoningDetail, ReasoningDetailKind, Role, Usage, schema::Choice,
    };
    let resp = ChatResponse {
        id: "msg_unrecognized".into(),
        model: "gpt-5".into(),
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("answer".into()),
                reasoning: None,
                reasoning_details: vec![ReasoningDetail {
                    kind: ReasoningDetailKind::Other("future.kind".to_string()),
                    id: Some("rd_2".into()),
                    format: Some("openai-responses-v1".into()),
                    index: Some(0),
                    payload: json!({"text": "a future shape"}),
                }],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 10,
            total_tokens: 15,
            ..Default::default()
        }),
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    };
    let v = AnthropicIngress.render_response_value(resp).unwrap();
    let content = v["content"].as_array().expect("content is array");
    let thinking = content
        .iter()
        .find(|b| b["type"] == "thinking")
        .expect("unrecognized-kind detail must still produce a thinking block");
    assert_eq!(thinking["thinking"], "a future shape");
}

/// Spec-drift fix: when canonical Usage has no cache fields (None),
/// the non-streaming render must omit `cache_creation_input_tokens`,
/// `cache_read_input_tokens`, and `cache_creation` from the wire
/// rather than emitting them as JSON null. Emitting null for these
/// fields diverges from api.anthropic.com's own response shape and
/// can confuse downstream consumers that treat null and absent
/// differently (e.g. token-counting dashboards that sum cache fields).
#[test]
fn render_response_omits_absent_cache_fields_from_usage() {
    use routectl_core::{Message, MessageContent, Role, Usage, schema::Choice};
    let resp = ChatResponse {
        id: "msg_no_cache".into(),
        model: "claude-opus-4-7".into(),
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("hello".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(Usage {
            prompt_tokens: 20,
            completion_tokens: 8,
            total_tokens: 28,
            // All cache fields are None -- no cache activity.
            cache_creation_input_tokens: None,
            cache_read_input_tokens: None,
            cache_creation: None,
            ..Default::default()
        }),
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    };
    let v = AnthropicIngress.render_response_value(resp).unwrap();
    let usage = v["usage"].as_object().expect("usage is present");

    // Typed fields that ARE present must be emitted.
    assert_eq!(v["usage"]["input_tokens"], 20);
    assert_eq!(v["usage"]["output_tokens"], 8);

    // Cache fields that are None must NOT appear at all -- not even as null.
    assert!(
        !usage.contains_key("cache_creation_input_tokens"),
        "cache_creation_input_tokens must be absent when None, got usage: {usage:?}"
    );
    assert!(
        !usage.contains_key("cache_read_input_tokens"),
        "cache_read_input_tokens must be absent when None, got usage: {usage:?}"
    );
    assert!(
        !usage.contains_key("cache_creation"),
        "cache_creation must be absent when None, got usage: {usage:?}"
    );
}

/// Counterpart: when cache fields ARE present they must still be emitted
/// with the correct values.
#[test]
fn render_response_emits_cache_fields_when_present() {
    use routectl_core::{
        Message, MessageContent, Role, Usage,
        schema::{CacheCreation, Choice},
    };
    let resp = ChatResponse {
        id: "msg_cache".into(),
        model: "claude-opus-4-7".into(),
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("cached".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(Usage {
            // prompt_tokens = raw(100) + cache_create(50) + cache_read(30) = 180
            prompt_tokens: 180,
            completion_tokens: 10,
            total_tokens: 190,
            cache_creation_input_tokens: Some(50),
            cache_read_input_tokens: Some(30),
            cache_creation: Some(CacheCreation {
                ephemeral_5m_input_tokens: Some(50),
                ephemeral_1h_input_tokens: None,
            }),
            ..Default::default()
        }),
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    };
    let v = AnthropicIngress.render_response_value(resp).unwrap();
    // Raw input = 180 - 50 - 30 = 100.
    assert_eq!(v["usage"]["input_tokens"], 100);
    assert_eq!(v["usage"]["output_tokens"], 10);
    assert_eq!(v["usage"]["cache_creation_input_tokens"], 50);
    assert_eq!(v["usage"]["cache_read_input_tokens"], 30);
    let cc = v["usage"]["cache_creation"]
        .as_object()
        .expect("cache_creation present");
    assert_eq!(cc["ephemeral_5m_input_tokens"], 50);
    // ephemeral_1h is None -- must be absent, not null.
    assert!(
        !cc.contains_key("ephemeral_1h_input_tokens"),
        "absent ephemeral_1h_input_tokens must be omitted, got cc: {cc:?}"
    );
}

#[test]
fn content_filter_finish_renders_refusal_stop_reason() {
    use routectl_core::{Message, Role, Usage, schema::Choice};
    let resp = ChatResponse {
        id: "msg_cf".into(),
        model: "gpt-5".into(),
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("redacted".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            finish_reason: Some("content_filter".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 2,
            total_tokens: 7,
            ..Default::default()
        }),
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    };
    let v = AnthropicIngress.render_response_value(resp).unwrap();
    assert_eq!(v["stop_reason"], "refusal");
}

#[test]
fn anthropic_native_pause_turn_finish_round_trips_unchanged() {
    // An Anthropic-native stop_reason with no OpenAI analogue must
    // survive the reverse mapping verbatim via the catchall arm.
    assert_eq!(openai_finish_to_anthropic_stop("pause_turn"), "pause_turn");
}

fn render_single_tool_call(arguments: &str) -> Value {
    use routectl_core::{Message, MessageContent, Role, Usage, schema::Choice};
    let resp = ChatResponse {
        id: "msg_args".into(),
        model: "qwen-3-coder".into(),
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text(String::new()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "call_args",
                    "type": "function",
                    "function": {
                        "name": "do_thing",
                        "arguments": arguments
                    }
                })]),
            },
            finish_reason: Some("tool_calls".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(Usage {
            prompt_tokens: 1,
            completion_tokens: 1,
            total_tokens: 2,
            ..Default::default()
        }),
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    };
    AnthropicIngress.render_response_value(resp).unwrap()
}

#[test]
fn empty_tool_call_arguments_render_input_as_empty_object() {
    let v = render_single_tool_call("");
    let content = v["content"].as_array().expect("content is array");
    let tu = content
        .iter()
        .find(|b| b["type"] == "tool_use")
        .expect("tool_use block present");
    assert!(
        tu["input"].is_object(),
        "input must be an object, got: {:?}",
        tu["input"]
    );
    assert_eq!(tu["input"], json!({}));
}

#[test]
fn non_json_tool_call_arguments_render_input_as_empty_object() {
    let v = render_single_tool_call("notjson");
    let content = v["content"].as_array().expect("content is array");
    let tu = content
        .iter()
        .find(|b| b["type"] == "tool_use")
        .expect("tool_use block present");
    assert!(
        tu["input"].is_object(),
        "input must be an object, got: {:?}",
        tu["input"]
    );
    assert_eq!(tu["input"], json!({}));
}

#[test]
fn valid_tool_call_arguments_render_input_unchanged() {
    let v = render_single_tool_call("{\"x\":1}");
    let content = v["content"].as_array().expect("content is array");
    let tu = content
        .iter()
        .find(|b| b["type"] == "tool_use")
        .expect("tool_use block present");
    assert_eq!(tu["input"], json!({"x": 1}));
}

/// `upstream_meta` is routectl's transport-internal carrier name. The
/// typed field is `#[serde(skip)]`, but `ChatResponse.extras` is
/// `#[serde(flatten)]`, so an upstream Anthropic body carrying a
/// top-level key literally named `upstream_meta` would land in `extras`
/// and otherwise be re-emitted by the forward-compat loop. Pin that the
/// Anthropic render drops the reserved name so it never reaches a client.
#[test]
fn render_response_drops_reserved_upstream_meta_key_from_extras() {
    use routectl_core::{Message, MessageContent, Role, Usage, schema::Choice};
    let mut extras = serde_json::Map::new();
    extras.insert("upstream_meta".into(), json!({"leaked": "should not ship"}));
    let resp = ChatResponse {
        id: "msg_reserved".into(),
        model: "claude-opus-4-7".into(),
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        usage: Some(Usage {
            prompt_tokens: 5,
            completion_tokens: 2,
            total_tokens: 7,
            ..Default::default()
        }),
        routectl_provider: None,
        extras,
        upstream_meta: None,
    };
    let v = AnthropicIngress.render_response_value(resp).unwrap();
    let obj = v.as_object().expect("rendered body is a JSON object");
    assert!(
        !obj.contains_key("upstream_meta"),
        "reserved upstream_meta key must never reach the rendered body, got: {obj:?}"
    );
}

/// Build a canonical response carrying exactly one `Encrypted`
/// reasoning detail, so the flatten-to-`redacted_thinking` path can be
/// exercised with an arbitrary `(format, id, blob)`.
fn response_with_encrypted_detail(
    format: Option<&str>,
    id: Option<&str>,
    blob: &str,
) -> routectl_core::ChatResponse {
    use routectl_core::{
        Message, MessageContent, ReasoningDetail, ReasoningDetailKind, Role, schema::Choice,
    };
    ChatResponse {
        id: "msg_enc".into(),
        model: "claude-opus-4-7".into(),
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("answer".into()),
                reasoning: None,
                reasoning_details: vec![ReasoningDetail {
                    kind: ReasoningDetailKind::Encrypted,
                    id: id.map(Into::into),
                    format: format.map(Into::into),
                    index: Some(0),
                    payload: json!({"encrypted_content": blob}),
                }],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        usage: None,
        routectl_provider: None,
        extras: Default::default(),
        upstream_meta: None,
    }
}

/// Extract the single `redacted_thinking.data` string from a rendered
/// Anthropic response body.
fn redacted_thinking_data(v: &Value) -> String {
    v["content"]
        .as_array()
        .expect("content is array")
        .iter()
        .find(|b| b["type"] == "redacted_thinking")
        .expect("redacted_thinking block present")["data"]
        .as_str()
        .expect("data is a string")
        .to_string()
}

/// The Anthropic wire carries neither the artifact's item id nor its
/// scheme, so a Responses-family blob flattened here would come back
/// next turn indistinguishable from a native `redacted_thinking` and
/// could no longer be replayed onto the lane that issued it. It must go
/// out self-describing, with the original blob bytes untouched.
#[test]
fn render_response_wraps_a_foreign_scheme_encrypted_blob_with_its_scheme_and_id() {
    // Arrange
    const BLOB: &str = "rsn_OPAQUE-PROVIDER-PAYLOAD-9f31";
    let resp = response_with_encrypted_detail(
        Some(routectl_core::BEDROCK_MANTLE),
        Some("rs_abc123"),
        BLOB,
    );

    // Act
    let v = AnthropicIngress.render_response_value(resp).unwrap();
    let data = redacted_thinking_data(&v);

    // Assert
    let (scheme, id, blob) =
        routectl_core::reasoning_envelope::unwrap(&data).expect("wire data is an envelope");
    assert_eq!(scheme, routectl_core::BEDROCK_MANTLE);
    assert_eq!(id, Some("rs_abc123"));
    assert_eq!(blob, BLOB, "inner blob bytes must be unchanged");
}

/// An Anthropic-family blob must reach the wire BYTE-VERBATIM. Its
/// native signature is what makes same-model replay work on that lane;
/// wrapping it would corrupt a mechanism that works today.
#[test]
fn render_response_emits_an_anthropic_family_encrypted_blob_byte_verbatim() {
    // Arrange
    const BLOB: &str = "ErkBCkYIBRgCKkDd-ANTHROPIC-NATIVE-SIGNATURE";
    let resp = response_with_encrypted_detail(
        Some(crate::ingress::anthropic::ANTHROPIC_FORMAT),
        Some("rd_1"),
        BLOB,
    );

    // Act
    let v = AnthropicIngress.render_response_value(resp).unwrap();
    let data = redacted_thinking_data(&v);

    // Assert
    assert_eq!(data, BLOB, "Anthropic-sourced blob must not be wrapped");
}

/// An artifact with no recoverable id still wraps, id-less: one lane
/// family validates content and ignores the id entirely, so the scheme
/// alone keeps it replayable.
#[test]
fn render_response_wraps_an_id_less_foreign_blob_so_its_scheme_survives() {
    // Arrange
    const BLOB: &str = "smry_OPAQUE-PROVIDER-PAYLOAD-4c02";
    let resp = response_with_encrypted_detail(Some(routectl_core::CODEX_OAUTH), None, BLOB);

    // Act
    let v = AnthropicIngress.render_response_value(resp).unwrap();
    let data = redacted_thinking_data(&v);

    // Assert
    let (scheme, id, blob) =
        routectl_core::reasoning_envelope::unwrap(&data).expect("id-less envelope round-trips");
    assert_eq!(scheme, routectl_core::CODEX_OAUTH);
    assert_eq!(id, None);
    assert_eq!(blob, BLOB);
}

/// A detail with no format tag at all is not a Responses-family
/// artifact, so it takes the verbatim path rather than being wrapped
/// under a guessed scheme.
#[test]
fn render_response_emits_an_untagged_encrypted_blob_byte_verbatim() {
    // Arrange
    const BLOB: &str = "UNTAGGED-OPAQUE-PAYLOAD-1a77";
    let resp = response_with_encrypted_detail(None, Some("rd_9"), BLOB);

    // Act
    let v = AnthropicIngress.render_response_value(resp).unwrap();
    let data = redacted_thinking_data(&v);

    // Assert
    assert_eq!(data, BLOB, "an untagged blob must not be wrapped");
}

/// An empty blob carries nothing to replay, so wrapping it would only
/// produce an envelope the decode side rejects.
#[test]
fn render_response_leaves_an_empty_encrypted_blob_unwrapped() {
    // Arrange
    let resp =
        response_with_encrypted_detail(Some(routectl_core::BEDROCK_MANTLE), Some("rs_empty"), "");

    // Act
    let v = AnthropicIngress.render_response_value(resp).unwrap();
    let data = redacted_thinking_data(&v);

    // Assert
    assert_eq!(data, "", "an empty blob must not become an envelope");
}
