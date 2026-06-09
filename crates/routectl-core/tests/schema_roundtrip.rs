//! Schema round-trip tests. Confirms that the OpenRouter-shape types we
//! serialize match real-world wire formats and don't lose fields.

use routectl_core::{
    cache_control::{Breakpoint, BreakpointPosition},
    upstream_meta::{AnthropicUnifiedQuota, UpstreamMeta},
    CacheControl, ChatChunk, ChatRequest, ChatResponse, ContentPart, KnownContentPart,
    OpaqueSseEvent, Reasoning, ReasoningConfig, ReasoningDetail, ReasoningDetailKind, SystemBlock,
    SystemContent, ToolDef,
};
use serde_json::{json, Value};

fn roundtrip<T>(input: Value) -> T
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let parsed: T = serde_json::from_value(input.clone()).expect("deserialize");
    let serialized = serde_json::to_value(&parsed).expect("serialize");
    // Fields we serialized should be a subset of the input. Every field we accepted
    // should round-trip identically.
    assert_subset(&serialized, &input, "");
    parsed
}

fn assert_subset(actual: &Value, expected: &Value, path: &str) {
    match (actual, expected) {
        (Value::Object(a), Value::Object(e)) => {
            for (k, av) in a {
                let ev = e
                    .get(k)
                    .unwrap_or_else(|| panic!("field `{path}.{k}` serialized but not in input"));
                assert_subset(av, ev, &format!("{path}.{k}"));
            }
        }
        (Value::Array(a), Value::Array(e)) => {
            assert_eq!(a.len(), e.len(), "array length mismatch at `{path}`");
            for (i, (av, ev)) in a.iter().zip(e.iter()).enumerate() {
                assert_subset(av, ev, &format!("{path}[{i}]"));
            }
        }
        (a, e) => assert_eq!(a, e, "value mismatch at `{path}`"),
    }
}

#[test]
fn openai_request_basic() {
    let input = json!({
        "model": "gpt-4o",
        "messages": [
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "Hi"}
        ],
        "temperature": 0.7,
        "max_tokens": 200,
        "stream": false
    });
    let req: ChatRequest = roundtrip(input);
    assert_eq!(req.model, "gpt-4o");
    assert_eq!(req.messages.len(), 2);
    assert_eq!(req.temperature, Some(0.7));
}

#[test]
fn openrouter_request_with_reasoning() {
    let input = json!({
        "model": "anthropic/claude-opus-4-7",
        "messages": [{"role": "user", "content": "Solve this."}],
        "reasoning": {
            "effort": "high",
            "max_tokens": 8000,
            "exclude": false,
            "enabled": true
        },
        "provider_extras": {"transforms": ["middle-out"]}
    });
    let req: ChatRequest = roundtrip(input);
    let r = req.reasoning.as_ref().expect("reasoning present");
    assert_eq!(r.effort.as_deref(), Some("high"));
    assert_eq!(r.max_tokens, Some(8000));
}

#[test]
fn openai_request_with_tools_and_modern_fields() {
    let input = json!({
        "model": "gpt-4o",
        "messages": [{"role": "user", "content": "Use the tool."}],
        "tools": [{
            "type": "function",
            "function": {
                "name": "get_weather",
                "description": "Get weather",
                "parameters": {"type": "object", "properties": {}}
            }
        }],
        "tool_choice": "auto",
        "response_format": {"type": "json_object"},
        "seed": 42,
        "n": 1,
        "logprobs": true,
        "top_logprobs": 5,
        "user": "user-123"
    });
    let req: ChatRequest = roundtrip(input);
    assert_eq!(req.seed, Some(42));
    assert_eq!(req.n, Some(1));
    assert_eq!(req.logprobs, Some(true));
    assert_eq!(req.top_logprobs, Some(5));
    assert_eq!(req.user.as_deref(), Some("user-123"));
}

#[test]
fn openrouter_response_with_reasoning_details() {
    let input = json!({
        "id": "chatcmpl-abc",
        "model": "anthropic/claude-opus-4-7",
        "created": 1700000000,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "The answer is 42.",
                "reasoning": "First, I considered...",
                "reasoning_details": [{
                    "type": "reasoning.text",
                    "id": "reasoning-1",
                    "format": "anthropic-claude-v1",
                    "index": 0,
                    "text": "Let me work through this...",
                    "signature": "sha256:abc"
                }]
            },
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 50,
            "completion_tokens": 100,
            "total_tokens": 150,
            "reasoning_tokens": 80
        }
    });
    let resp: ChatResponse = roundtrip(input);
    assert_eq!(resp.choices.len(), 1);
    let msg = &resp.choices[0].message;
    assert_eq!(msg.reasoning.as_deref(), Some("First, I considered..."));
    assert_eq!(msg.reasoning_details.len(), 1);
    let detail = &msg.reasoning_details[0];
    assert!(matches!(detail.kind, ReasoningDetailKind::Text));
    assert_eq!(detail.format.as_deref(), Some("anthropic-claude-v1"));
    assert_eq!(detail.id.as_deref(), Some("reasoning-1"));
}

#[test]
fn deepseek_response_legacy_reasoning() {
    // Before normalization: DeepSeek puts reasoning_content next to content.
    // routectl normalizes this away in the provider; here we just verify the
    // schema accepts it via reasoning_details once normalized.
    let input = json!({
        "id": "chatcmpl-ds",
        "model": "deepseek-reasoner",
        "created": 1700000000,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "42",
                "reasoning_details": [{
                    "type": "reasoning.text",
                    "id": "reasoning-ds-0",
                    "format": "deepseek-v1",
                    "index": 0,
                    "text": "Step by step..."
                }]
            },
            "finish_reason": "stop"
        }]
    });
    let resp: ChatResponse = roundtrip(input);
    let detail = &resp.choices[0].message.reasoning_details[0];
    assert_eq!(detail.format.as_deref(), Some("deepseek-v1"));
}

#[test]
fn streaming_chunk_with_reasoning_delta() {
    let input = json!({
        "id": "chatcmpl-stream",
        "model": "deepseek-reasoner",
        "choices": [{
            "index": 0,
            "delta": {
                "reasoning": "First, ",
                "reasoning_details": [{
                    "type": "reasoning.text",
                    "id": "reasoning-stream-0",
                    "format": "deepseek-v1",
                    "index": 0,
                    "text": "First, "
                }]
            }
        }]
    });
    let chunk: ChatChunk = roundtrip(input);
    let delta = &chunk.choices[0].delta;
    assert_eq!(delta.reasoning.as_deref(), Some("First, "));
    assert_eq!(delta.reasoning_details.len(), 1);
}

#[test]
fn anthropic_thinking_signature_preserved_in_reasoning_detail() {
    let detail = ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: Some("anthropic-thinking-0".into()),
        format: Some("anthropic-claude-v1".into()),
        index: Some(0),
        payload: json!({
            "text": "Let me think step by step about this complex problem.",
            "signature": "WaUjzkypQ2mUEVM36O2TxuC06KN8xyfb_ABC"
        }),
    };
    let serialized = serde_json::to_value(&detail).expect("serialize");
    let s = serialized.as_object().unwrap();
    assert_eq!(s["type"], "reasoning.text");
    assert_eq!(s["format"], "anthropic-claude-v1");
    assert_eq!(s["signature"], "WaUjzkypQ2mUEVM36O2TxuC06KN8xyfb_ABC");
    assert!(s["text"].is_string());
}

#[test]
fn reasoning_config_default_is_empty() {
    let cfg = ReasoningConfig::default();
    let s = serde_json::to_value(&cfg).unwrap();
    // skip_serializing_if Option::is_none keeps the JSON minimal
    assert_eq!(s, json!({}));
}

#[test]
fn message_with_no_reasoning_omits_fields() {
    let msg_json = json!({
        "role": "user",
        "content": "Hi"
    });
    let _: routectl_core::Message = roundtrip(msg_json);
}

#[test]
fn reasoning_top_level_serializes_minimally() {
    let r = Reasoning {
        text: Some("trace".into()),
        details: vec![],
    };
    let s = serde_json::to_value(&r).unwrap();
    assert_eq!(s, json!({"text": "trace"}));
}

// ---------------------------------------------------------------------------
// v0.4.0 canonical extension: cache_control + system + tools + anthropic_beta
// ---------------------------------------------------------------------------

#[test]
fn anthropic_request_with_cache_control_on_every_position_round_trips() {
    // Realistic Claude Code-style request: tool def cache, system cache,
    // user message cache. This is the primary round-trip lossless path.
    let input = json!({
        "model": "claude-opus-4-7-20251022",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "text",
                "text": "Use the tool to look up Rust docs.",
                "cache_control": {"type": "ephemeral", "ttl": "5m"}
            }]
        }],
        "system": [{
            "type": "text",
            "text": "You are a Rust expert.",
            "cache_control": {"type": "ephemeral", "ttl": "1h"}
        }],
        "tools": [{
            "name": "lookup_doc",
            "description": "Fetch a Rust crate doc page",
            "input_schema": {"type": "object", "properties": {"crate": {"type": "string"}}},
            "cache_control": {"type": "ephemeral", "ttl": "1h"}
        }],
        "anthropic_beta": ["context-1m-2025-08-07"],
        "max_tokens": 1024
    });
    let req: ChatRequest = roundtrip(input);
    // System lifted to top-level.
    let SystemContent::Blocks(sys_blocks) = req.system.as_ref().expect("system present") else {
        panic!("expected SystemContent::Blocks");
    };
    assert_eq!(sys_blocks.len(), 1);
    assert_eq!(
        sys_blocks[0]
            .cache_control
            .as_ref()
            .unwrap()
            .effective_ttl(),
        "1h"
    );
    // Tool kept its cache_control.
    let tools = req.tools.as_ref().expect("tools present");
    assert_eq!(tools.len(), 1);
    assert!(matches!(&tools[0], ToolDef::Custom(_)));
    assert_eq!(tools[0].cache_control().unwrap().effective_ttl(), "1h");
    // anthropic_beta preserved.
    assert_eq!(
        req.anthropic_beta,
        vec!["context-1m-2025-08-07".to_string()]
    );
}

#[test]
fn top_level_cache_control_round_trips() {
    let input = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "cache_control": {"type": "ephemeral", "ttl": "5m"}
    });
    let req: ChatRequest = roundtrip(input);
    assert_eq!(req.cache_control.as_ref().unwrap().effective_ttl(), "5m");
}

#[test]
fn system_string_form_round_trips() {
    let input = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "system": "You are helpful."
    });
    let req: ChatRequest = roundtrip(input);
    assert!(matches!(req.system, Some(SystemContent::Text(_))));
}

#[test]
fn anthropic_request_with_image_block_round_trips() {
    let input = json!({
        "model": "claude-opus-4-7",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "image",
                "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}
            }, {
                "type": "text",
                "text": "what is this?"
            }]
        }],
        "max_tokens": 100
    });
    let req: ChatRequest = roundtrip(input);
    let msg = &req.messages[0];
    if let routectl_core::MessageContent::Parts(parts) = &msg.content {
        assert_eq!(parts.len(), 2);
        assert!(matches!(
            &parts[0],
            ContentPart::Known(KnownContentPart::Image { .. })
        ));
        assert!(matches!(
            &parts[1],
            ContentPart::Known(KnownContentPart::Text { .. })
        ));
    } else {
        panic!("expected Parts");
    }
}

#[test]
fn unknown_content_block_falls_to_other_and_round_trips() {
    let input = json!({
        "model": "claude-opus-4-7",
        "messages": [{
            "role": "user",
            "content": [{
                "type": "server_tool_use",
                "id": "srvtu_01",
                "name": "web_search",
                "input": {"query": "rust"},
                "cache_control": {"type": "ephemeral", "ttl": "1h"}
            }]
        }],
        "max_tokens": 100
    });
    let req: ChatRequest = roundtrip(input);
    if let routectl_core::MessageContent::Parts(parts) = &req.messages[0].content {
        assert!(matches!(&parts[0], ContentPart::Other { .. }));
        assert_eq!(parts[0].cache_control().unwrap().effective_ttl(), "1h");
        assert_eq!(parts[0].type_tag(), "server_tool_use");
    } else {
        panic!("expected Parts");
    }
}

#[test]
fn anthropic_builtin_tool_passes_through_other() {
    let input = json!({
        "model": "claude-opus-4-7",
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{
            "type": "bash_20250124",
            "name": "bash",
            "cache_control": {"type": "ephemeral"}
        }],
        "max_tokens": 100
    });
    let req: ChatRequest = roundtrip(input);
    let tools = req.tools.as_ref().unwrap();
    assert!(matches!(&tools[0], ToolDef::Other(_)));
    assert!(tools[0].cache_control().is_some());
}

#[test]
fn usage_with_cache_stats_round_trips() {
    let input = json!({
        "id": "msg_01",
        "model": "claude-opus-4-7",
        "created": 1700000000,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hi"},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 50,
            "completion_tokens": 10,
            "total_tokens": 60,
            "cache_creation_input_tokens": 4096,
            "cache_read_input_tokens": 8192,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 2048,
                "ephemeral_1h_input_tokens": 2048
            }
        }
    });
    let resp: ChatResponse = roundtrip(input);
    let u = resp.usage.as_ref().unwrap();
    assert_eq!(u.cache_creation_input_tokens, Some(4096));
    assert_eq!(u.cache_read_input_tokens, Some(8192));
    let cc = u.cache_creation.as_ref().unwrap();
    assert_eq!(cc.ephemeral_5m_input_tokens, Some(2048));
    assert_eq!(cc.ephemeral_1h_input_tokens, Some(2048));
}

#[test]
fn chunk_with_streaming_usage_round_trips() {
    let input = json!({
        "id": "chatcmpl-stream",
        "model": "claude-opus-4-7",
        "choices": [],
        "usage": {
            "completion_tokens": 100,
            "cache_read_input_tokens": 4096
        }
    });
    let chunk: ChatChunk = roundtrip(input);
    let u = chunk.usage.as_ref().unwrap();
    assert_eq!(u.completion_tokens, Some(100));
    assert_eq!(u.cache_read_input_tokens, Some(4096));
}

#[test]
fn cache_control_validate_enforces_ttl_ordering_via_canonical() {
    // Build a sequence that mirrors what an Anthropic ingress would
    // collect across positions and ensure the validator catches the
    // 5m-then-1h ordering bug.
    let five = CacheControl::ephemeral_5m();
    let one = CacheControl::ephemeral_1h();
    let bps = vec![
        Breakpoint {
            position: BreakpointPosition::Tools,
            control: &five,
        },
        Breakpoint {
            position: BreakpointPosition::System,
            control: &one,
        },
    ];
    let err = routectl_core::cache_control::validate(&bps).unwrap_err();
    assert!(err.to_string().contains("after a 5m"));
}

#[test]
fn cache_control_validate_accepts_max_breakpoints() {
    // Helper: build a breakpoint list of size N.
    fn bps(n: usize, cc: &CacheControl) -> Vec<Breakpoint<'_>> {
        (0..n)
            .map(|_| Breakpoint {
                position: BreakpointPosition::Messages,
                control: cc,
            })
            .collect()
    }
    let cc = CacheControl::ephemeral_5m();
    routectl_core::cache_control::validate(&bps(4, &cc)).unwrap();
    assert!(routectl_core::cache_control::validate(&bps(5, &cc)).is_err());
}

#[test]
fn system_block_helper_constructs_minimally() {
    // Ensure SystemBlock can be built programmatically without spelling
    // out every optional field. This is the path used by the OpenAI
    // ingress when lifting Role::System messages.
    let block = SystemBlock {
        kind: "text".into(),
        text: "you are helpful".into(),
        cache_control: None,
        citations: None,
    };
    let v = serde_json::to_value(&block).unwrap();
    assert_eq!(v["type"], "text");
    assert_eq!(v["text"], "you are helpful");
    assert!(!v.as_object().unwrap().contains_key("cache_control"));
}

// ---------------------------------------------------------------------------
// ChatChunk.opaque_events: skip-serialized carrier for opaque SSE bytes.
// Anthropic egress writes; Anthropic ingress reads. Never on the wire.
// ---------------------------------------------------------------------------

#[test]
fn chatchunk_wire_shape_unchanged_after_opaque_events() {
    // Pin the contract: a default ChatChunk's wire JSON must not contain
    // an `opaque_events` key. Adding the field is invisible to library
    // consumers and OpenAI-shape ingresses.
    let before = serde_json::to_value(ChatChunk::default()).unwrap();
    assert!(before.get("opaque_events").is_none());

    // And a chunk with opaque_events populated must also serialize without
    // the key (the field is `#[serde(skip)]`).
    let chunk = ChatChunk {
        opaque_events: vec![OpaqueSseEvent::ContentBlockStop { upstream_index: 0 }],
        ..Default::default()
    };
    let serialized = serde_json::to_value(&chunk).unwrap();
    assert!(serialized.get("opaque_events").is_none());
}

#[test]
fn chatchunk_opaque_events_round_trip_drops_to_empty() {
    // Round-trip contract: serializing a ChatChunk with non-empty
    // opaque_events and deserializing back yields opaque_events = [].
    // The carrier is in-process only; ser/de erases it. This is by
    // design so OpenAI-shape ingresses never see Anthropic-only blocks.
    let chunk = ChatChunk {
        id: "chunk-1".into(),
        opaque_events: vec![
            OpaqueSseEvent::ContentBlockStart {
                upstream_index: 2,
                type_tag: "server_tool_use".into(),
                raw_data: b"{\"type\":\"server_tool_use\",\"id\":\"x\"}".to_vec(),
            },
            OpaqueSseEvent::ContentBlockDelta {
                upstream_index: 2,
                raw_delta: b"{\"type\":\"input_json_delta\"}".to_vec(),
            },
            OpaqueSseEvent::ContentBlockStop { upstream_index: 2 },
        ],
        ..Default::default()
    };
    let json = serde_json::to_string(&chunk).unwrap();
    let restored: ChatChunk = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.id, "chunk-1");
    assert!(restored.opaque_events.is_empty());
}

// ---------------------------------------------------------------------------
// ChatResponse/ChatChunk.upstream_meta: skip-serialized transport-internal
// quota/overage carrier. Anthropic egress writes; usage-accounting reads.
// Never on the client-facing wire.
// ---------------------------------------------------------------------------

fn sample_upstream_meta() -> UpstreamMeta {
    // AnthropicUnifiedQuota is `#[non_exhaustive]`: an out-of-crate
    // consumer mutates fields on the default value rather than using a
    // struct expression.
    let mut quota = AnthropicUnifiedQuota::default();
    quota.status = Some("allowed".into());
    quota.representative_claim = Some("overage".into());
    UpstreamMeta::from_anthropic_unified(quota)
}

#[test]
fn chatresponse_upstream_meta_never_serializes() {
    // Arrange: a default response has no upstream_meta key, and a
    // populated upstream_meta must also be omitted (the field is
    // `#[serde(skip)]`).
    let default_json = serde_json::to_value(ChatResponse::default()).unwrap();
    assert!(default_json.get("upstream_meta").is_none());

    let resp = ChatResponse {
        upstream_meta: Some(sample_upstream_meta()),
        ..Default::default()
    };

    // Act
    let serialized = serde_json::to_value(&resp).unwrap();

    // Assert
    assert!(
        serialized.get("upstream_meta").is_none(),
        "upstream_meta must never reach the wire: {serialized}"
    );
}

#[test]
fn chatchunk_upstream_meta_never_serializes() {
    // Arrange
    let chunk = ChatChunk {
        upstream_meta: Some(sample_upstream_meta()),
        ..Default::default()
    };

    // Act
    let serialized = serde_json::to_value(&chunk).unwrap();

    // Assert
    assert!(
        serialized.get("upstream_meta").is_none(),
        "upstream_meta must never reach the wire: {serialized}"
    );
}

#[test]
fn chatresponse_upstream_meta_round_trip_drops_to_none() {
    // Round-trip contract: serializing a ChatResponse with a populated
    // upstream_meta and deserializing back yields upstream_meta = None.
    // The carrier is in-process only; ser/de erases it.
    let resp = ChatResponse {
        id: "resp-1".into(),
        upstream_meta: Some(sample_upstream_meta()),
        ..Default::default()
    };
    let json = serde_json::to_string(&resp).unwrap();
    let restored: ChatResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.id, "resp-1");
    assert!(restored.upstream_meta.is_none());
}
