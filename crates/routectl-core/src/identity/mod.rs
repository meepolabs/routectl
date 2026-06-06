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
//!
//! Both submodules expose their constants plus a
//! `default_identity_headers()` builder the relevant provider calls
//! directly; nothing else duplicates these literals.

pub mod anthropic;
pub mod codex;
