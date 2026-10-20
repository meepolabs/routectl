//! The managed-key predicate every extras merge consults.
//!
//! An egress assembles its upstream body from the canonical request, then
//! layers client `provider_extras` or operator extras over it. A key the
//! predicate names is one routectl writes itself, so the merge drops the
//! extras value rather than let it replace the assembled one.
//!
//! Every dialect except the Bedrock Invoke operator path composes the shared
//! canonical list (`routectl_core::is_canonical_request_key`) with its own
//! wire-layer keys. The Invoke operator path is a closed literal set that does
//! not compose the canonical list: widening it changes which configured
//! `additional_model_request_fields` reach the wire.
//!
//! Keys match case-sensitively: the Anthropic and OpenAI upstreams key their
//! request fields by exact name. Gemini's ProtoJSON parser also accepts proto
//! field names (`generation_config` for `generationConfig`); only the
//! lowerCamelCase spelling is named here.

use routectl_core::is_canonical_request_key;

/// The extras merge whose managed set a key is checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// The Anthropic Messages body, built by the anthropic-api egress and
    /// reused by Bedrock Invoke for client `provider_extras`.
    #[cfg(feature = "anthropic-api")]
    AnthropicApi,
    /// Bedrock Invoke operator `additional_model_request_fields`.
    #[cfg(feature = "bedrock")]
    BedrockInvokeOperator,
    /// The Bedrock Converse `additionalModelRequestFields` bag, client and
    /// operator sources alike.
    #[cfg(feature = "bedrock")]
    BedrockConverse,
    /// The openai-compat Chat Completions body.
    #[cfg(feature = "openai-compat")]
    OpenAiCompat,
    /// The Gemini `generateContent` body.
    #[cfg(feature = "gemini")]
    Gemini,
}

/// True when `key` is a top-level field routectl writes for `dialect`, so an
/// extras value under that key must not reach the wire.
pub fn is_managed_key(dialect: Dialect, key: &str) -> bool {
    match dialect {
        #[cfg(feature = "anthropic-api")]
        Dialect::AnthropicApi => {
            is_canonical_request_key(key)
                || matches!(
                    key,
                    // Composed from the client object or `req.reasoning`.
                    "thinking"
                        // Consumed into `tool_choice.disable_parallel_tool_use`;
                        // not a valid top-level Anthropic field.
                        | "parallel_tool_calls"
                )
        }
        #[cfg(feature = "bedrock")]
        Dialect::BedrockInvokeOperator => matches!(
            key,
            "model"
                | "messages"
                | "system"
                | "max_tokens"
                | "thinking"
                | "tools"
                | "tool_choice"
                | "stream"
                | "stop_sequences"
                | "temperature"
                | "top_p"
                | "anthropic_beta"
                | "anthropic_version"
                | "cache_control"
        ),
        #[cfg(feature = "bedrock")]
        Dialect::BedrockConverse => {
            is_canonical_request_key(key)
                || matches!(
                    key,
                    // Bag keys routectl writes from canonical fields.
                    "thinking"
                        | "output_config"
                        // Converse top-level body fields; never valid in the bag.
                        | "inferenceConfig"
                        | "toolConfig"
                        | "additionalModelResponseFieldPaths"
                )
        }
        #[cfg(feature = "openai-compat")]
        Dialect::OpenAiCompat => {
            is_canonical_request_key(key)
                || matches!(
                    key,
                    // Anthropic-shape fields the ingress sweep forwards; strict
                    // openai-compat hosts reject them as unsupported parameters.
                    "output_config"
                        | "metadata"
                        | "context_management"
                        | "context_hint"
                        | "mcp_servers"
                        | "container"
                        | "inference_geo"
                        | "speed"
                        | "diagnostics"
                )
        }
        #[cfg(feature = "gemini")]
        Dialect::Gemini => {
            is_canonical_request_key(key)
                || matches!(
                    key,
                    "contents" | "systemInstruction" | "toolConfig" | "generationConfig"
                )
        }
    }
}

#[cfg(test)]
#[path = "managed_keys_tests.rs"]
mod tests;
