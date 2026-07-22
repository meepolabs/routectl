use super::*;
use crate::anthropic_api::context_management::{
    CLEAR_THINKING_EDIT_TYPE, ThinkingCache, snapshot_to_cache,
};
use routectl_core::{ChatRequest, Message, MessageContent, ReasoningConfig, Role};
use serde_json::json;
use std::num::NonZeroUsize;
use std::sync::{Arc, RwLock};

fn simple_req() -> ChatRequest {
    ChatRequest {
        model: "claude-sonnet-4".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }],
        ..Default::default()
    }
}

fn req_with_cm_extras() -> ChatRequest {
    ChatRequest {
        provider_extras: Some(json!({
            "context_management": {
                "edits": [{"type": CLEAR_THINKING_EDIT_TYPE, "keep": "all"}]
            }
        })),
        ..simple_req()
    }
}

fn req_with_tool_use_history_and_cm() -> ChatRequest {
    // Build a request whose messages contain an assistant turn with
    // tool_calls so translate_messages produces an AnthropicMessage
    // with a ToolUse block -- required for apply_clear_thinking_edit
    // to find qualifying messages.
    ChatRequest {
        model: "claude-sonnet-4".into(),
        max_tokens: Some(4096),
        reasoning: Some(ReasoningConfig {
            enabled: Some(true),
            max_tokens: Some(2048),
            effort: None,
            exclude: None,
        }),
        messages: vec![
            Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("use the calc tool".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("calling calc".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: Some(vec![json!({
                    "id": "toolu_t1",
                    "type": "function",
                    "function": {"name": "calc", "arguments": "{}"}
                })]),
            },
            Message {
                refusal: None,
                role: Role::Tool,
                content: MessageContent::Text("42".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: Some("toolu_t1".into()),
                tool_calls: None,
            },
        ],
        provider_extras: Some(json!({
            "context_management": {
                "edits": [{"type": CLEAR_THINKING_EDIT_TYPE, "keep": "all"}]
            }
        })),
        ..Default::default()
    }
}

fn small_cache(cap: usize) -> RwLock<ThinkingCache> {
    RwLock::new(lru::LruCache::new(NonZeroUsize::new(cap).expect("cap > 0")))
}

/// Adaptive variant of `req_with_tool_use_history_and_cm`: same
/// tool_use history + context_management edits, but with an
/// `effort` set so the adaptive path produces an
/// `output_config.effort` PRE-strip. Used to prove the cache-miss
/// soft-fail strip drops the now-orphan `output_config.effort`,
/// not merely that it is absent-by-default.
fn req_with_tool_use_history_and_cm_adaptive() -> ChatRequest {
    let mut req = req_with_tool_use_history_and_cm();
    req.reasoning = Some(ReasoningConfig {
        enabled: Some(true),
        max_tokens: None,
        effort: Some("high".into()),
        exclude: None,
    });
    req
}

/// When context_management=true, the `context_management` body key
/// (which came from provider_extras) must be stripped before returning.
/// Non-Anthropic upstreams reject unknown top-level body keys.
#[test]
fn normalize_strips_context_management_body_key_when_flag_true() {
    let req = req_with_cm_extras();
    let body = normalize("test", &req, false, &[], true, None).expect("normalize must succeed");
    assert!(
        body.get("context_management").is_none(),
        "context_management body key must be stripped when flag=true; got: {body}"
    );
}

/// When context_management=false, the `context_management` body key
/// must be forwarded verbatim to the upstream (e.g. real Anthropic).
#[test]
fn normalize_keeps_context_management_body_key_when_flag_false() {
    let req = req_with_cm_extras();
    let body = normalize("test", &req, false, &[], false, None).expect("normalize must succeed");
    assert!(
        body.get("context_management").is_some(),
        "context_management body key must survive when flag=false; got: {body}"
    );
}

/// The anthropic-api egress strips the Claude Code billing/attribution
/// system block unconditionally in normalize -- it runs for ALL
/// anthropic-api requests, including those routed to a third-party
/// host (e.g. a `/anthropic`-compatible upstream), so the client
/// fingerprint never reaches a non-Anthropic party. The retained
/// system blocks are forwarded in order.
#[test]
fn normalize_strips_billing_system_block() {
    use routectl_core::{SystemBlock, SystemContent};
    let mut req = simple_req();
    req.system = Some(SystemContent::Blocks(vec![
        SystemBlock {
            kind: "text".into(),
            text: "x-anthropic-billing-header: v=1; fp=secret".into(),
            cache_control: None,
            citations: None,
        },
        SystemBlock {
            kind: "text".into(),
            text: "you are helpful".into(),
            cache_control: None,
            citations: None,
        },
    ]));
    let body = normalize("test", &req, false, &[], false, None).expect("normalize must succeed");
    let sys = body["system"].as_array().expect("system survives as array");
    assert_eq!(
        sys.len(),
        1,
        "billing block must be stripped, leaving only the prompt block; got: {body}"
    );
    assert_eq!(sys[0]["text"], "you are helpful");
}

/// A pure-billing `Text` system collapses to absent: the whole string
/// is the fingerprint-bearing block, so the `system` key is dropped.
#[test]
fn normalize_drops_pure_billing_text_system() {
    use routectl_core::SystemContent;
    let mut req = simple_req();
    req.system = Some(SystemContent::Text(
        "x-anthropic-billing-header: v=1; fp=secret".into(),
    ));
    let body = normalize("test", &req, false, &[], false, None).expect("normalize must succeed");
    assert!(
        body.get("system").is_none() || body["system"].is_null(),
        "pure-billing Text system must collapse to absent; got: {body}"
    );
}

/// Legacy-path defense-in-depth: when `req.system` is None, the billing
/// block carried in a `Role::System` message must still be stripped
/// before it is lifted into the Anthropic `system`. The HTTP ingress
/// always lands system in `req.system` (covered above); this pins the
/// direct-caller / legacy gap so the Claude Code fingerprint never
/// reaches a third-party anthropic-api host via the lift fallback.
#[test]
fn normalize_strips_billing_from_legacy_system_message() {
    use routectl_core::Role;
    let mut req = simple_req();
    // req.system stays None: force the lift_legacy_system fallback.
    req.system = None;
    req.messages = vec![
        Message {
            refusal: None,
            role: Role::System,
            content: MessageContent::Text("x-anthropic-billing-header: v=1; fp=secret".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        },
        Message {
            refusal: None,
            role: Role::System,
            content: MessageContent::Text("you are helpful".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        },
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        },
    ];
    let body = normalize("test", &req, false, &[], false, None).expect("normalize must succeed");
    let sys = body
        .get("system")
        .and_then(|s| s.as_str())
        .expect("legacy lift produces a flat-text system");
    assert!(
        !sys.contains("x-anthropic-billing-header:"),
        "billing block must be stripped from the lifted legacy system; got: {sys:?}"
    );
    assert!(
        sys.contains("you are helpful"),
        "the non-billing system prompt must survive the strip; got: {sys:?}"
    );
}

/// A `Role::System` message whose ONLY content is the billing block
/// must collapse to an absent `system` on the legacy lift path -- the
/// fingerprint is the whole prompt, so nothing lands upstream.
#[test]
fn normalize_drops_pure_billing_legacy_system_message() {
    use routectl_core::Role;
    let mut req = simple_req();
    req.system = None;
    req.messages = vec![
        Message {
            refusal: None,
            role: Role::System,
            content: MessageContent::Text("x-anthropic-billing-header: v=1; fp=secret".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        },
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        },
    ];
    let body = normalize("test", &req, false, &[], false, None).expect("normalize must succeed");
    assert!(
        body.get("system").is_none() || body["system"].is_null(),
        "a pure-billing legacy system must collapse to absent; got: {body}"
    );
}

/// Soft-fail: when context_management=true and the thinking cache has
/// no entry for the qualifying tool_use id (cold-start or TTL eviction),
/// the `thinking` key must be stripped from the outgoing body so the
/// upstream (which does not honour the beta) does not 400.
#[test]
fn normalize_soft_fail_strips_thinking_on_cache_miss() {
    let req = req_with_tool_use_history_and_cm();
    let cache = Arc::new(small_cache(8)); // nothing seeded
    let body = normalize("test", &req, false, &[], true, Some(&cache))
        .expect("normalize must succeed even on cache miss");
    assert!(
        body.get("thinking").is_none(),
        "thinking must be stripped on cache miss; got: {body}"
    );
}

/// Adaptive cache-miss path: the body carries
/// `output_config.effort` PRE-strip (adaptive=true + effort set),
/// and the soft-fail strip removes `thinking`. The late enforcer
/// must then observe that thinking is gone and drop the now-orphan
/// `output_config.effort`. Asserts BOTH thinking absent AND
/// output_config.effort absent -- proving the orphan is dropped,
/// not just absent-by-default.
#[test]
fn normalize_soft_fail_drops_orphan_output_config_effort_on_cache_miss() {
    let req = req_with_tool_use_history_and_cm_adaptive();
    let cache = Arc::new(small_cache(8)); // nothing seeded -> cache miss
    let body = normalize("test", &req, true, &[], true, Some(&cache))
        .expect("normalize must succeed even on cache miss");
    assert!(
        body.get("thinking").is_none(),
        "thinking must be stripped on cache miss; got: {body}"
    );
    assert!(
        body.get("output_config")
            .and_then(|oc| oc.get("effort"))
            .is_none(),
        "orphan output_config.effort must be dropped once thinking is stripped; got: {body}"
    );
}
/// Ordering invariant (cache-miss soft-fail path): thinking is composed
/// -- forcing `temperature=1.0` and dropping `top_p` -- and then the
/// cache-miss soft-fail strips `thinking` from the body. Sampling params
/// must be recomputed from the SOURCE request once thinking is gone:
/// emitted `temperature == req.temperature`, and `top_p` follows the
/// else-branch (absent while temperature is present). Pins the ordering
/// so a future strip pass added without revisiting sampling regresses
/// this test rather than silently shipping the forced 1.0.
#[test]
fn normalize_recomputes_sampling_after_cache_miss_thinking_strip() {
    let mut req = req_with_tool_use_history_and_cm();
    req.temperature = Some(0.2);
    req.top_p = Some(0.9); // caller sent both; temperature wins per else-branch
    let cache = Arc::new(small_cache(8)); // nothing seeded -> cache miss
    let body = normalize("test", &req, false, &[], true, Some(&cache))
        .expect("normalize must succeed even on cache miss");
    assert!(
        body.get("thinking").is_none(),
        "thinking must be stripped on cache miss; got: {body}"
    );
    assert_eq!(
        body.get("temperature").and_then(Value::as_f64),
        Some(0.2),
        "temperature must revert to the caller's value once thinking is stripped; got: {body}"
    );
    assert!(
        body.get("top_p").is_none(),
        "top_p must follow the else-branch (absent while temperature present); got: {body}"
    );
}

/// tool_use id: the `thinking` key must remain in the outgoing body.
#[test]
fn normalize_no_soft_fail_when_cache_hits() {
    let req = req_with_tool_use_history_and_cm();
    let cache = Arc::new(small_cache(8));
    // Seed the cache for the tool_use id used in req_with_tool_use_history_and_cm.
    snapshot_to_cache(
        &cache,
        "test",
        "toolu_t1",
        vec![routectl_core::ReasoningDetail {
            kind: routectl_core::ReasoningDetailKind::Text,
            id: Some("rd-1".into()),
            format: Some(super::super::ANTHROPIC_FORMAT.to_string()),
            index: Some(0),
            payload: json!({"text": "my reasoning", "signature": "sig"}),
        }],
        super::super::context_management::DEFAULT_MAX_THINKING_ENTRY_BYTES,
        super::super::context_management::THINKING_CACHE_TTL,
        "test",
    );
    let body = normalize("test", &req, false, &[], true, Some(&cache))
        .expect("normalize must succeed with cache hit");
    assert!(
        body.get("thinking").is_some(),
        "thinking must NOT be stripped when cache has an entry; got: {body}"
    );
}
