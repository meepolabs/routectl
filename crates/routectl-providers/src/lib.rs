//! Provider implementations.
//!
//! The default build includes `openai-compat`, `anthropic-api`, and
//! `bedrock`. To build a lean binary without the AWS SDK dependency
//! tree:
//!
//!   cargo build --release --no-default-features \
//!     --features openai-compat,anthropic-api
//!
//! Per-model quirks (e.g. "drop temperature for o3-mini",
//! "use adaptive thinking for Opus 4.7+") live in [`model_profile`] as a
//! single declarative table consumed by every provider.

pub mod model_profile;

pub(crate) mod http_client;

// Shared effort-clamping helper for OpenAI-shape egresses. Unconditional
// (no feature gate) because both openai-compat and openai-responses egresses
// use it and both pull in reqwest anyway.
pub(crate) mod effort;

// Shared, lazily-gated dir-2 / dir-3 header-trace helpers. Unconditional
// like `http_client` (both lean on `reqwest`, which any provider feature
// pulls in); every provider calls into it, so there is no dead code in a
// feature-gated build.
pub(crate) mod header_trace;

#[cfg(feature = "openai-compat")]
pub mod openai_compat;

#[cfg(feature = "anthropic-api")]
pub mod anthropic_api;

#[cfg(feature = "bedrock")]
pub mod bedrock;

#[cfg(feature = "openai-responses")]
pub mod openai_responses;
