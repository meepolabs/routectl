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

/// True when `base_url` is an `https` URL with no userinfo, no explicit
/// port, and a host exactly equal (ASCII case-insensitive) to one of `hosts`:
/// the test every first-party credential must pass before it is sent.
///
/// Parsed with `url::Url`, the WHATWG parser the egress uses, so a backslash
/// or `@` smuggle resolves to the host the request would actually reach. The
/// parser lowercases the scheme and drops a port equal to the scheme default,
/// so `HTTPS://` and `:443` are accepted; a trailing-dot host is a different
/// host string and is refused. Invalid URLs and URLs with no host return
/// `false`.
pub fn is_exact_https_host_in(base_url: &str, hosts: &[&str]) -> bool {
    let Ok(url) = url::Url::parse(base_url) else {
        return false;
    };
    let Some(host) = url.host_str() else {
        return false;
    };
    url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.port().is_none()
        && hosts
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(host))
}

#[cfg(test)]
#[path = "https_host_tests.rs"]
mod https_host_tests;
