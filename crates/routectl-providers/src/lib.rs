#![deny(rustdoc::broken_intra_doc_links)]
#![warn(missing_docs)]
//! Provider implementations.
//!
//! At least one provider feature (`openai-compat`, `anthropic-api`,
//! `openai-responses`, `gemini`, `bedrock`) must be enabled; a build with
//! none is rejected at compile time.
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

// A build with no provider feature is not a supported target: this crate
// exists to hold provider implementations, and every provider feature is
// what pulls in `reqwest`. Without this guard the failure surfaces as ten
// unresolved-import errors in `http_client` / `header_trace` instead of the
// actual requirement.
#[cfg(not(any(
    feature = "openai-compat",
    feature = "anthropic-api",
    feature = "openai-responses",
    feature = "gemini",
    feature = "bedrock"
)))]
compile_error!(
    "routectl-providers requires at least one provider feature \
     (openai-compat, anthropic-api, openai-responses, gemini, bedrock). \
     For the leanest supported build use: \
     --no-default-features --features openai-compat,anthropic-api"
);

#[cfg(feature = "openai-compat")]
pub(crate) mod model_profile;

// Shared leak-guard: WARN once per request naming which of the canonical
// sampling knobs (`n`, `seed`, `logprobs`, `top_logprobs`, `logit_bias`,
// `presence_penalty`, `frequency_penalty`) an egress received but cannot
// translate; each caller passes the subset it does honor. Gated on the
// egresses that call in; openai-compat forwards them under their canonical
// names and never calls in.
#[cfg(any(
    feature = "anthropic-api",
    feature = "bedrock",
    feature = "gemini",
    feature = "openai-responses"
))]
pub(crate) mod sampling_drop_guard;

// Reqwest-backed, so gated on "any provider feature" rather than left
// unconditional: a build with no provider has no `reqwest`, and this gate is
// what lets the crate-level `compile_error!` above be the only error reported.
#[cfg(any(
    feature = "openai-compat",
    feature = "anthropic-api",
    feature = "openai-responses",
    feature = "gemini",
    feature = "bedrock"
))]
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

// Shared effort-clamping helper for OpenAI-shape egresses. Unconditional (no
// feature gate) because its ungated surface is dependency-free and is exported
// to `routectl-router`, which imports `VALID_EFFORT_TOKENS` (the single source
// of truth for the valid effort vocabulary) regardless of which provider
// features this crate was built with.
pub mod effort;

// Shared helpers for the Bedrock mantle lanes. The URL builders and the
// service-scope constant are pure and dependency-free, so they stay
// unconditional (`routectl-router` derives lane base URLs from a region
// without pulling in the AWS SDK); only the `sign` wrapper, which reaches
// into the bedrock signer, is gated on `bedrock`.
pub mod mantle;

// Shared, lazily-gated dir-2 / dir-3 header-trace helpers. Gated like
// `http_client` (both lean on `reqwest`, which any provider feature pulls
// in); every provider calls into it, so there is no dead code in a
// feature-gated build.
#[cfg(any(
    feature = "openai-compat",
    feature = "anthropic-api",
    feature = "openai-responses",
    feature = "gemini",
    feature = "bedrock"
))]
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

// Shared lifter for the upstream provider correlation id (`x-request-id` /
// `x-oai-request-id` / `cf-ray`) off a response header map. Same feature
// gate + crate-internal visibility as `retry_after`: provider egresses call
// it directly at the error seam, and the router only ever sees the lifted
// id via `Error::Upstream`.
#[cfg(any(
    feature = "openai-compat",
    feature = "anthropic-api",
    feature = "bedrock",
    feature = "openai-responses",
    feature = "gemini"
))]
pub(crate) mod upstream_request_id;

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

// Shared redaction + token lift for AWS/Bedrock upstream error envelopes. A
// 403 AccessDenied body names the caller principal ARN, account id, and
// resource ARN; the single classifier here keeps that out of both the
// client-facing message and the log line. Used by the native bedrock lane,
// the anthropic-api mantle lift, AND both OpenAI readers (which lift AWS
// tokens as a fallback when a mantle upstream returns a flat AWS envelope
// instead of the native error shape), so it is crate-level -- every lane
// that can front a mantle (Bedrock) upstream sees it, and no lane links the
// AWS SDK just to redact an error body.
#[cfg(any(
    feature = "anthropic-api",
    feature = "bedrock",
    feature = "openai-compat",
    feature = "openai-responses"
))]
pub(crate) mod aws_error;

// The AWS exception-discriminator vocabulary, re-exported for the downstream
// crates that gate on a `__type` / `x-amzn-errortype` token. Every consumer
// compares through `aws_exception_type_is` (or the already-stripped
// `upstream_type` the lift lands) so the namespaced wire form
// (`com.amazon.coral.validate#ValidationException`) can never be silently
// missed by an exact match against the bare name. The rest of `aws_error`
// stays crate-private: the redaction and lift entry points are lane-internal.
#[cfg(any(
    feature = "anthropic-api",
    feature = "bedrock",
    feature = "openai-compat",
    feature = "openai-responses"
))]
pub use aws_error::{VALIDATION_EXCEPTION_TYPE, aws_exception_type_is};

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

// Cross-lane guard for the opening-role-chunk contract. Needs all four
// role-emitting egress lanes compiled in.
#[cfg(all(
    test,
    feature = "gemini",
    feature = "openai-responses",
    feature = "anthropic-api",
    feature = "bedrock"
))]
mod streaming_role_parity_tests;
