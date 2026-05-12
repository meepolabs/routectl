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
//! which maps from `req.stop`). Egress-local additions belong in each
//! egress's own `is_*_managed_key` function, which should call this fn
//! first and `||`-chain any local extras.
//!
//! # What does NOT belong here
//!
//! Keys that are Converse-native body fields (e.g. `inferenceConfig`,
//! `toolConfig`, `additionalModelResponseFieldPaths`) -- those are
//! Converse-layer keys, not canonical `ChatRequest` fields, and live only
//! in `bedrock/converse/extras.rs::is_converse_managed_key`.

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
    matches!(
        key,
        // Identifiers and routing.
        "model"
            // Message arrays.
            | "messages"
            // System prompt (Anthropic-shape top-level).
            | "system"
            // Sampling parameters.
            | "temperature"
            | "top_p"
            | "max_tokens"
            // OpenAI alias for max_tokens on newer models.
            | "max_completion_tokens"
            // Stop sequences -- OpenAI wire name.
            | "stop"
            // Stop sequences -- Anthropic wire name.
            | "stop_sequences"
            | "stream"
            | "n"
            | "seed"
            | "logprobs"
            | "top_logprobs"
            | "logit_bias"
            | "presence_penalty"
            | "frequency_penalty"
            | "user"
            // Tool definitions and selection.
            | "tools"
            | "tool_choice"
            // OpenAI structured output (JSON mode / json_schema).
            | "response_format"
            // Anthropic prompt-cache breakpoint at the top-level body.
            | "cache_control"
            // Anthropic beta feature flags.
            | "anthropic_beta"
            // Reasoning / thinking config.
            | "reasoning"
            // Typed reasoning-details echoed on multi-turn (Anthropic shape).
            | "reasoning_details"
            // Escape hatch for long-tail provider knobs; must not be
            // clobbered by a second layer of extras.
            | "provider_extras"
            // Chat-template kwargs (vLLM / DashScope).
            | "chat_template_kwargs"
    )
}

#[cfg(test)]
mod tests {
    use super::is_canonical_request_key;

    /// Every serde wire-name on `ChatRequest` must be recognized.
    /// Update this list whenever `schema.rs::ChatRequest` gains or
    /// renames a field.
    #[test]
    fn all_chat_request_wire_fields_are_recognized() {
        // Arrange: exhaustive list of ChatRequest wire-field names
        // (the key each field serializes to on the JSON wire).
        let canonical_fields = [
            "model",
            "messages",
            "system",
            "temperature",
            "top_p",
            "max_tokens",
            "stop",
            "stream",
            "n",
            "seed",
            "logprobs",
            "top_logprobs",
            "logit_bias",
            "presence_penalty",
            "frequency_penalty",
            "user",
            "tools",
            "tool_choice",
            "response_format",
            "cache_control",
            "anthropic_beta",
            "reasoning",
            "chat_template_kwargs",
            "provider_extras",
            // Additional aliases routectl uses across egresses:
            "max_completion_tokens",
            "stop_sequences",
            "reasoning_details",
        ];

        // Act + Assert: each field name must return true.
        for field in &canonical_fields {
            assert!(
                is_canonical_request_key(field),
                "expected is_canonical_request_key({field:?}) == true but got false"
            );
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
