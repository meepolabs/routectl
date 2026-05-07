//! Provider implementations.
//!
//! Each provider is feature-gated so binaries can opt in. v0.1 ships with
//! `openai-compat` + `anthropic-api` enabled by default; cookie-auth providers
//! are scaffolded but require explicit opt-in.

#[cfg(feature = "openai-compat")]
pub mod openai_compat;

#[cfg(feature = "anthropic-api")]
pub mod anthropic_api;

#[cfg(feature = "claude-cookie")]
pub mod claude_cookie;

#[cfg(feature = "chatgpt-cookie")]
pub mod chatgpt_cookie;
