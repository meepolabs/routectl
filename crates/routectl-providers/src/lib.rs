//! Provider implementations.
//!
//! The default build includes `openai-compat`, `anthropic-api`, and
//! `bedrock`. To build this providers library without the AWS SDK
//! dependency tree:
//!
//!   cargo check -p routectl-providers --no-default-features \
//!     --features openai-compat,anthropic-api
//!
//! Per-model quirks (e.g. "drop temperature for o3-mini",
//! "use adaptive thinking for Opus 4.7+") live in [`model_profile`] as a
//! single declarative table consumed by every provider.

pub(crate) mod model_profile;

pub(crate) mod http_client;

// Shared effort-clamping helper for OpenAI-shape egresses. Unconditional
// (no feature gate) because both openai-compat and openai-responses egresses
// use it and both pull in reqwest anyway. `pub` so `routectl-router` can
// import `VALID_EFFORT_TOKENS` (the single source of truth for the valid
// effort vocabulary).
pub mod effort;

// Shared, lazily-gated dir-2 / dir-3 header-trace helpers. Unconditional
// like `http_client` (both lean on `reqwest`, which any provider feature
// pulls in); every provider calls into it, so there is no dead code in a
// feature-gated build.
pub(crate) mod header_trace;

// Shared parser for the standard HTTP `Retry-After` response header.
// Gated on the provider features that bring in `reqwest` + `chrono`
// (every provider does); the router consumes the parsed `Duration` via
// `Error::Upstream`, but provider egresses call this directly.
#[cfg(any(
    feature = "openai-compat",
    feature = "anthropic-api",
    feature = "bedrock",
    feature = "openai-responses"
))]
pub mod retry_after;

// Shared tool-call id charset sanitizer. Anthropic and Bedrock Converse
// require `tool_use.id` to match `^[a-zA-Z0-9_-]+$`; an OpenAI-origin id
// with `.`/`:`/`/` 400s the upstream. Applied at every id-emit site and
// every tool_result correlation site so a sanitized id and its result
// stay equal. Gated on anthropic-api / bedrock (the egresses that enforce
// the charset) so a lean openai-compat-only build carries no dead code.
#[cfg(any(
    feature = "anthropic-api",
    feature = "bedrock",
    feature = "openai-responses"
))]
pub(crate) mod tool_id;

// Shared parse step for OpenAI-shape `Message.tool_calls` entries. The
// bedrock-converse and openai-responses egresses re-emit those calls as
// their native tool-use items so a following tool_result turn is not
// orphaned upstream; both consume this helper. Gated on those two
// features so a lean build (openai-compat + anthropic-api only) doesn't
// carry dead code -- the anthropic-api egress keeps its own inline parse
// to stay byte-identical on the empty-id path.
#[cfg(any(feature = "bedrock", feature = "openai-responses"))]
pub(crate) mod tool_calls;

// Shared filter that drops the Claude Code billing/attribution system
// block before a NON-Anthropic egress forwards it. Gated on the egresses
// that strip it (openai-compat, bedrock); the anthropic-api egress keeps
// forwarding the block and does not consume this module.
#[cfg(any(feature = "openai-compat", feature = "bedrock"))]
pub(crate) mod system_filter;

// Body re-signer for the billing-header checksum. When routectl mutates
// the canonical body on the egress path (effort injection, tool-id
// sanitize, signature strip), any checksum the ingress client computed
// is invalidated. This module re-signs the existing billing block
// in-place so the bytes transmitted match an upstream recompute.
#[cfg(feature = "anthropic-api")]
pub(crate) mod claude_signing;

#[cfg(feature = "openai-compat")]
pub mod openai_compat;

#[cfg(feature = "anthropic-api")]
pub mod anthropic_api;

#[cfg(feature = "bedrock")]
pub mod bedrock;

#[cfg(feature = "openai-responses")]
pub mod openai_responses;
