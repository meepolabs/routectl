//! Provider identity-header defaults.
//!
//! One canonical home for the compiled HTTP-fingerprint constants and
//! `(name, value)` default-header builders that ship with routectl so a
//! zero-config operator emits the client fingerprint each upstream
//! associates with a first-party SDK / CLI install:
//!
//!   - `codex` -- the `codex_cli_rs` fingerprint consumed by the
//!     openai-responses egress (ChatgptOauth) and the OAuth refresh
//!     client.
//!   - `anthropic` -- the Claude Code SDK (Stainless) fingerprint
//!     consumed by the anthropic-api egress (OauthBearer).
//!   - `antigravity` -- the Antigravity IDE fingerprint consumed by the
//!     gemini cloud-code egress (bearer token).
//!
//! Each submodule exposes its constants plus the builder the relevant
//! provider calls directly (`default_identity_headers()` for the header
//! sets, a composed User-Agent for the cloud-code lane); nothing else
//! duplicates these literals.

pub mod anthropic;
pub mod antigravity;
pub mod codex;
