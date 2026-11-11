//! Canonical `ChatRequest` wire-field names that routectl manages.
//!
//! Each egress has its own allow-list for managed keys that operator
//! `provider_extras` / `additional_model_request_fields` may NOT override.
//! The canonical portion -- fields that live on `ChatRequest` itself -- is
//! shared here so the three egresses (openai-compat, anthropic-api, Converse)
//! stay in sync when a new field lands on `ChatRequest`.
//!
//! # What belongs here
//!
//! Any key whose wire name corresponds to a typed field on `ChatRequest`
//! (or that routectl writes from a canonical field, like `stop_sequences`
//! which maps from `req.stop`). Egress-local additions belong in that
//! egress's arm of `routectl-providers`' `managed_keys::is_managed_key`,
//! which calls this fn first and `||`-chains the local extras.
//!
//! # What does NOT belong here
//!
//! Keys that are Converse-native body fields (e.g. `inferenceConfig`,
//! `toolConfig`, `additionalModelResponseFieldPaths`) -- those are
//! Converse-layer keys, not canonical `ChatRequest` fields, and live only
//! in the Converse arm of `managed_keys::is_managed_key`.

/// Returns `true` when `key` is a wire field name that routectl derives
/// directly from `ChatRequest`, or a wire-level alias that routectl owns
/// when shaping outgoing bodies. Egresses should call this as the base
/// check and then `||` their own wire-layer additions.
///
/// The list covers:
/// - Every field on `ChatRequest` (and its known serialization aliases).
/// - Wire-level aliases that routectl emits in egress bodies but that are
///   not literal `ChatRequest` field names:
///   - `max_completion_tokens` -- OpenAI's alias for `max_tokens` on newer
///     models; routectl writes it in place of `max_tokens` for compatible
///     providers.
///   - `stop_sequences` -- Anthropic wire name for `req.stop`; emitted
///     instead of `stop` on Anthropic-shape egresses.
///   - `reasoning_details` -- per-message field echoed on multi-turn
///     assistant messages (not a top-level `ChatRequest` field).
///
/// Extend when a new field is added to `ChatRequest` in
/// `routectl-core/src/schema.rs`, or when a new egress alias is introduced.
pub fn is_canonical_request_key(key: &str) -> bool {
    CANONICAL_REQUEST_KEYS.contains(&key)
}

/// The names [`is_canonical_request_key`] recognizes.
const CANONICAL_REQUEST_KEYS: &[&str] = &[
    // Identifiers and routing.
    "model",
    // Message arrays.
    "messages",
    // System prompt (Anthropic-shape top-level).
    "system",
    // Sampling parameters.
    "temperature",
    "top_p",
    "max_tokens",
    // OpenAI alias for max_tokens on newer models.
    "max_completion_tokens",
    // Stop sequences -- OpenAI wire name.
    "stop",
    // Stop sequences -- Anthropic wire name.
    "stop_sequences",
    "stream",
    "n",
    "seed",
    "logprobs",
    "top_logprobs",
    "logit_bias",
    "presence_penalty",
    "frequency_penalty",
    "user",
    // Tool definitions and selection.
    "tools",
    "tool_choice",
    // OpenAI structured output (JSON mode / json_schema).
    "response_format",
    // Anthropic prompt-cache breakpoint at the top-level body.
    "cache_control",
    // Anthropic beta feature flags.
    "anthropic_beta",
    // Reasoning / thinking config.
    "reasoning",
    // Typed reasoning-details echoed on multi-turn (Anthropic shape).
    "reasoning_details",
    // Escape hatch for long-tail provider knobs; must not be
    // clobbered by a second layer of extras.
    "provider_extras",
    // Chat-template kwargs (vLLM / DashScope).
    "chat_template_kwargs",
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::json;

    use super::{CANONICAL_REQUEST_KEYS, is_canonical_request_key};
    use crate::{
        CacheControl, ChatRequest, CustomTool, Message, MessageContent, ReasoningConfig, Role,
        RoutectlInternal, SystemContent, ToolDef,
    };

    /// Wire names routectl writes or echoes that are not a `ChatRequest`
    /// member's serialized key.
    const WIRE_ALIASES: [&str; 3] = [
        // OpenAI's alias for `max_tokens` on newer models.
        "max_completion_tokens",
        // The Anthropic spelling of `stop`.
        "stop_sequences",
        // Echoed on multi-turn assistant messages.
        "reasoning_details",
    ];

    /// A `ChatRequest` with every member populated, so each serializes. A
    /// struct literal without `..Default::default()`: a member added to
    /// `ChatRequest` fails to compile here until it is populated.
    fn populated_request() -> ChatRequest {
        ChatRequest {
            model: "m".into(),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
                refusal: None,
                extras: serde_json::Map::new(),
            }]
            .into(),
            system: Some(SystemContent::Text("s".into())),
            temperature: Some(0.5),
            top_p: Some(0.5),
            max_tokens: Some(1),
            stop: Some(vec!["x".into()]),
            stream: Some(true),
            n: Some(1),
            seed: Some(1),
            logprobs: Some(true),
            top_logprobs: Some(1),
            logit_bias: Some(json!({})),
            presence_penalty: Some(0.5),
            frequency_penalty: Some(0.5),
            user: Some("u".into()),
            tools: Some(vec![ToolDef::Custom(CustomTool {
                name: "t".into(),
                description: None,
                input_schema: json!({"type": "object"}),
                cache_control: None,
                defer_loading: None,
                strict: None,
                type_tag: None,
                extras: serde_json::Map::new(),
            })]),
            tool_choice: Some(json!("auto")),
            response_format: Some(json!({"type": "text"})),
            cache_control: Some(CacheControl::ephemeral_5m()),
            anthropic_beta: vec!["b".into()],
            reasoning: Some(ReasoningConfig {
                effort: Some("low".into()),
                ..Default::default()
            }),
            chat_template_kwargs: Some(json!({})),
            provider_extras: Some(json!({})),
            routectl_internal: RoutectlInternal::default(),
        }
    }

    /// The predicate covers exactly the keys a populated `ChatRequest`
    /// serializes, plus the wire aliases: no member is missing, and nothing
    /// outside that set is claimed.
    ///
    /// Mutation check: drop one name from `CANONICAL_REQUEST_KEYS` -> red.
    #[test]
    fn the_canonical_keys_are_the_serialized_request_members_plus_wire_aliases() {
        // Arrange
        let wire = serde_json::to_value(populated_request()).expect("serialize");
        let expected: BTreeSet<&str> = wire
            .as_object()
            .expect("a request serializes to an object")
            .keys()
            .map(String::as_str)
            .chain(WIRE_ALIASES)
            .collect();

        // Act
        let claimed: BTreeSet<&str> = CANONICAL_REQUEST_KEYS.iter().copied().collect();

        // Assert
        assert_eq!(claimed, expected);
        assert_eq!(claimed.len(), CANONICAL_REQUEST_KEYS.len(), "no duplicates");
        for key in &expected {
            assert!(is_canonical_request_key(key), "{key}");
        }
    }

    #[test]
    fn long_tail_provider_knobs_are_not_canonical() {
        // Arrange: provider-specific keys that must pass through freely.
        let pass_through = [
            "top_k",
            "service_tier",
            "safety_settings",
            "metadata",
            "container",
            "inference_geo",
            // Converse-layer keys (not canonical body fields):
            "inferenceConfig",
            "toolConfig",
            "additionalModelResponseFieldPaths",
            // Anthropic-API-only top-level keys that are not on ChatRequest:
            "thinking",
            // Anthropic nested output config (structured-output + effort).
            // NOT on ChatRequest; flows through provider_extras -> upstream.
            // Each egress manages it locally when needed (openai-compat
            // blocks it; Anthropic-API and Converse let it through).
            "output_config",
        ];

        // Act + Assert: long-tail keys must return false.
        for key in &pass_through {
            assert!(
                !is_canonical_request_key(key),
                "expected is_canonical_request_key({key:?}) == false but got true"
            );
        }
    }
}
