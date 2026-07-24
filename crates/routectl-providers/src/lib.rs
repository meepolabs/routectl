#![deny(rustdoc::broken_intra_doc_links)]
#![warn(missing_docs)]
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
//! "use adaptive thinking for Opus 4.7+") live in `model_profile` as a
//! single declarative table. Only the openai-compat egress reads it
//! today, so it is gated on that feature to stay dead-code-free in lean
//! single-feature builds.

#[cfg(feature = "openai-compat")]
pub(crate) mod model_profile;

pub(crate) mod http_client;

// Shared single-shot reachability probe for the HTTP-based egresses
// (`Provider::probe`). Gated on the three OpenAI/Anthropic-shape
// providers that share the GET-a-free-models-list mechanic; bedrock
// probes its credential chain directly and needs nothing here.
#[cfg(any(
    feature = "openai-compat",
    feature = "anthropic-api",
    feature = "openai-responses"
))]
pub(crate) mod probe;

// Shared effort-clamping helper for OpenAI-shape egresses. Unconditional
// (no feature gate) because both openai-compat and openai-responses egresses
// use it and both pull in reqwest anyway. `pub` so `routectl-router` can
// import `VALID_EFFORT_TOKENS` (the single source of truth for the valid
// effort vocabulary).
pub mod effort;

// Shared helpers for the Bedrock mantle lanes. The URL builders and the
// service-scope constant are pure and dependency-free, so they stay
// unconditional (`routectl-router` derives lane base URLs from a region
// without pulling in the AWS SDK); only the `sign` wrapper, which reaches
// into the bedrock signer, is gated on `bedrock`.
pub mod mantle;

// Shared, lazily-gated dir-2 / dir-3 header-trace helpers. Unconditional
// like `http_client` (both lean on `reqwest`, which any provider feature
// pulls in); every provider calls into it, so there is no dead code in a
// feature-gated build.
pub(crate) mod header_trace;

// Shared WARN emitter for upstream HTTP failures. Folds the
// 401/403-vs-else auth-warn split into one place so every egress emits
// the same message wording and field shape. Gated on the egress
// features that produce upstream errors (every provider does).
#[cfg(any(
    feature = "openai-compat",
    feature = "anthropic-api",
    feature = "bedrock",
    feature = "openai-responses",
    feature = "gemini"
))]
pub(crate) mod upstream_log;

// Shared parser for the standard HTTP `Retry-After` response header.
// Gated on the provider features that bring in `reqwest` + `chrono`
// (every provider does). Crate-internal: provider egresses call this
// directly, and the router only ever sees the parsed `Duration` via
// `Error::Upstream` -- it never reaches the module by path.
#[cfg(any(
    feature = "openai-compat",
    feature = "anthropic-api",
    feature = "bedrock",
    feature = "openai-responses",
    feature = "gemini"
))]
pub(crate) mod retry_after;

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
// block before any egress forwards it to an upstream. Used by every
// provider egress (openai-compat, bedrock, openai-responses, anthropic-api).
#[cfg(any(
    feature = "openai-compat",
    feature = "bedrock",
    feature = "openai-responses",
    feature = "anthropic-api"
))]
pub(crate) mod system_filter;

// Body re-signer for the billing-header checksum. When routectl mutates
// the canonical body on the egress path (effort injection, tool-id
// sanitize, signature strip), any checksum the ingress client computed
// is invalidated. This module re-signs the existing billing block
// in-place so the bytes transmitted match an upstream recompute.
#[cfg(feature = "anthropic-api")]
pub(crate) mod claude_signing;

// Shared Anthropic `error.type` -> synthetic HTTP status mapping. Both
// the native Anthropic SSE path and the Bedrock-Converse eventstream map
// in-stream error events through this one table so they classify
// identically to the sync path. Gated on the two anthropic-vocabulary
// egresses (crate-level, NOT bedrock-only, so the lean anthropic-api
// build sees it); a lean openai-compat-only build carries no dead code.
#[cfg(any(feature = "anthropic-api", feature = "bedrock"))]
pub(crate) mod anthropic_error;

#[cfg(feature = "openai-compat")]
pub mod openai_compat;

#[cfg(feature = "anthropic-api")]
pub mod anthropic_api;

#[cfg(feature = "bedrock")]
pub mod bedrock;

#[cfg(feature = "openai-responses")]
pub mod openai_responses;

#[cfg(feature = "gemini")]
pub mod gemini;
