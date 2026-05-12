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

#[cfg(feature = "openai-compat")]
pub mod openai_compat;

#[cfg(feature = "anthropic-api")]
pub mod anthropic_api;

#[cfg(feature = "bedrock")]
pub mod bedrock;
