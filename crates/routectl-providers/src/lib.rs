//! Provider implementations.
//!
//! Each provider is feature-gated so binaries can opt in. v0.1 ships with
//! `openai-compat` + `anthropic-api` enabled by default; cookie-auth providers
//! are scaffolded but require explicit opt-in.
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

#[cfg(feature = "claude-cookie")]
pub mod claude_cookie;

#[cfg(feature = "chatgpt-cookie")]
pub mod chatgpt_cookie;
