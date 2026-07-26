//! Capability canary builders and the baked probe profile.
//!
//! Each builder authors a minimal request that exercises exactly one
//! capability and derives the matching [`DetectorContext`] as a struct
//! literal, so a canary response classifies through the SAME
//! `routectl_router::detect` path a live response would. The builders are
//! pure: they touch no network, config, clock, or registry.
//!
//! The token ceiling and per-capability canary counts are baked into the
//! single [`PROBE_PROFILE_V1`] const. There is no flag, env, or config
//! path that raises them: both request building and the cost estimate read
//! this one value, so a refactor cannot silently widen a probe's blast
//! radius.

use routectl_core::{
    CacheControl, ChatRequest, ContentPart, KnownContentPart, Message, MessageContent,
    ReasoningConfig, Role, ToolDef,
};
use routectl_router::DetectorContext;
use serde_json::json;

/// The baked, immutable probe profile: the token ceiling every canary
/// request carries and the number of canary calls each capability lane
/// costs. Consumed by both request building and the cost estimate; there
/// is no override path, so the exact-value unit tests pin every field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeProfileV1 {
    /// Baked completion-token ceiling applied to every canary request.
    /// Sized to admit a full minimum extended-thinking block plus a small
    /// visible JSON answer, while capping a runaway completion.
    pub max_tokens: u32,
    /// Canary calls the structured-output lane costs.
    pub structured_output_canaries: u8,
    /// Canary calls the web-search lane costs.
    pub web_search_canaries: u8,
    /// Canary calls the prompt-caching lane costs (prime + read).
    pub prompt_caching_canaries: u8,
    /// Canary calls the thinking lane costs.
    pub thinking_canaries: u8,
}

/// THE baked probe profile. No flag/env/config path mutates these values;
/// every consumer reads this const.
pub const PROBE_PROFILE_V1: ProbeProfileV1 = ProbeProfileV1 {
    max_tokens: 1536,
    structured_output_canaries: 1,
    web_search_canaries: 1,
    prompt_caching_canaries: 2,
    thinking_canaries: 1,
};

/// Extended-thinking budget requested by the thinking canary. Set to the
/// minimum a provider will accept; the profile ceiling
/// ([`ProbeProfileV1::max_tokens`]) is chosen to sit strictly above it so
/// the completion still has room for a short visible answer.
const THINKING_BUDGET_TOKENS: u32 = 1024;

/// One filler sentence repeated to build a cacheable prompt prefix. The
/// text is inert: it exists only to give the caching canary a prefix long
/// enough for a provider to cache.
const CACHE_PREFIX_UNIT: &str = "This block exists only to fill the prompt cache prefix. ";

/// How many times [`CACHE_PREFIX_UNIT`] is repeated to form the cached
/// block. Large enough that the prefix clears a provider's minimum
/// cacheable size.
const CACHE_PREFIX_REPEATS: usize = 128;

/// The single required top-level property the structured-output canary's
/// schema declares. The canary owns this schema, so the shallow
/// required-key check is exact for it.
const STRUCTURED_OUTPUT_KEY: &str = "answer";

/// A single-capability canary: one request plus the detector context that
/// classifies its response.
#[derive(Debug, Clone)]
pub struct Canary {
    /// The request to dispatch.
    pub request: ChatRequest,
    /// The context the response is classified through.
    pub context: DetectorContext,
}

/// The prompt-caching canary: an ordered pair of requests. `prime` writes
/// the cache prefix; `read` reuses the identical prefix so a cache read
/// can be observed. Both share one detector context.
#[derive(Debug, Clone)]
pub struct CachingCanary {
    /// First call: writes the cache prefix.
    pub prime: ChatRequest,
    /// Second call: reuses the identical prefix to read the cache.
    pub read: ChatRequest,
    /// The context both responses are classified through.
    pub context: DetectorContext,
}

/// Structured-output canary: a strict `json_schema` output format with one
/// required top-level property. A schema-conforming response verifies the
/// capability; a non-conforming clean response suspects its absence.
pub fn structured_output_canary(model: &str) -> Canary {
    let request = ChatRequest {
        model: model.to_string(),
        messages: vec![user_text(
            "Reply with only a JSON object containing the answer property.",
        )]
        .into(),
        max_tokens: Some(PROBE_PROFILE_V1.max_tokens),
        provider_extras: Some(json!({
            "output_config": {
                "format": {
                    "type": "json_schema",
                    "name": "probe_answer",
                    "schema": {
                        "type": "object",
                        "properties": { STRUCTURED_OUTPUT_KEY: { "type": "string" } },
                        "required": [STRUCTURED_OUTPUT_KEY],
                        "additionalProperties": false
                    }
                }
            }
        })),
        ..Default::default()
    };
    let context = DetectorContext {
        strict_output_requested: true,
        requested_schema_required_keys: vec![STRUCTURED_OUTPUT_KEY.to_string()],
        ..Default::default()
    };
    Canary { request, context }
}

/// Web-search canary: a single search forced via `tool_choice`. A clean
/// response carrying no search evidence is a suspected absence.
pub fn web_search_canary(model: &str) -> Canary {
    let request = ChatRequest {
        model: model.to_string(),
        messages: vec![user_text(
            "Search the web for one current fact and answer in one short sentence.",
        )]
        .into(),
        max_tokens: Some(PROBE_PROFILE_V1.max_tokens),
        tools: Some(vec![ToolDef::Other(json!({
            "type": "web_search",
            "name": "web_search"
        }))]),
        tool_choice: Some(json!({ "type": "tool", "name": "web_search" })),
        ..Default::default()
    };
    let context = DetectorContext {
        forced_web_search: true,
        ..Default::default()
    };
    Canary { request, context }
}

/// Prompt-caching canary: an ordered prime/read pair sharing one cacheable
/// prefix carrying a `cache_control` marker.
pub fn prompt_caching_canary(model: &str) -> CachingCanary {
    let prime = caching_request(model);
    let read = caching_request(model);
    let context = DetectorContext {
        cache_requested: true,
        ..Default::default()
    };
    CachingCanary {
        prime,
        read,
        context,
    }
}

/// Thinking canary: extended reasoning requested with the minimum budget.
/// Reasoning blocks or reasoning-token usage verify the capability.
pub fn thinking_canary(model: &str) -> Canary {
    let request = ChatRequest {
        model: model.to_string(),
        messages: vec![user_text(
            "Think step by step, then give a one-line final answer.",
        )]
        .into(),
        max_tokens: Some(PROBE_PROFILE_V1.max_tokens),
        reasoning: Some(ReasoningConfig {
            effort: None,
            max_tokens: Some(THINKING_BUDGET_TOKENS),
            exclude: None,
            enabled: Some(true),
        }),
        ..Default::default()
    };
    let context = DetectorContext {
        reasoning_requested: true,
        ..Default::default()
    };
    Canary { request, context }
}

/// One caching request: a user turn whose first content block is the
/// cacheable prefix (marked with `cache_control`) followed by a short
/// instruction. The prime and read builds are byte-identical so the read
/// reuses the primed prefix.
fn caching_request(model: &str) -> ChatRequest {
    let prefix = CACHE_PREFIX_UNIT.repeat(CACHE_PREFIX_REPEATS);
    ChatRequest {
        model: model.to_string(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Parts(vec![
                ContentPart::Known(KnownContentPart::Text {
                    text: prefix,
                    citations: None,
                    cache_control: Some(CacheControl::ephemeral_5m()),
                }),
                ContentPart::Known(KnownContentPart::Text {
                    text: "Reply with the single word ok.".to_string(),
                    citations: None,
                    cache_control: None,
                }),
            ]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        }]
        .into(),
        max_tokens: Some(PROBE_PROFILE_V1.max_tokens),
        ..Default::default()
    }
}

/// A minimal `Role::User` turn carrying flat text.
fn user_text(text: &str) -> Message {
    Message {
        role: Role::User,
        content: MessageContent::Text(text.to_string()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    }
}

#[cfg(test)]
#[path = "canary_tests.rs"]
mod canary_tests;
