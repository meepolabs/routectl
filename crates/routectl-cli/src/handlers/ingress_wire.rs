//! Capture of the client's request body for the live wire check: the raw
//! bytes exactly as posted, the ingress dialect that parsed them and the
//! client's version token, set once on the canonical request right after
//! parsing.

use axum::body::Bytes;
use axum::http::HeaderMap;
use axum::http::header::USER_AGENT;
use routectl_core::identity::anthropic::parse_claude_cli_version;
use routectl_core::wire_check::{IngressDialect, IngressWire};
use routectl_core::{ChatRequest, is_safe_token};

/// Attach the posted body to `req` as its ingress wire. The body is shared
/// by refcount, never copied. A request whose provenance names no ingress
/// dialect is left without one, so it is never checked.
pub(super) fn capture_ingress_wire(headers: &HeaderMap, raw_body: &Bytes, req: &mut ChatRequest) {
    let Some(dialect) = IngressDialect::of_provenance(req.routectl_internal.provenance) else {
        return;
    };
    req.routectl_internal.ingress_wire = Some(IngressWire {
        dialect,
        client_version: client_version(headers),
        bytes: raw_body.clone(),
    });
}

/// The client's version: a stable Claude Code CLI version when the
/// user-agent carries one, else the first `product/version` token of the
/// user-agent that is a safe token, else none.
fn client_version(headers: &HeaderMap) -> Option<String> {
    let user_agent = headers.get(USER_AGENT)?.to_str().ok()?;
    if let Some(version) = parse_claude_cli_version(user_agent) {
        return Some(version.to_string());
    }
    user_agent
        .split_whitespace()
        .find(|token| token.contains('/') && is_safe_token(token))
        .map(str::to_string)
}

#[cfg(test)]
#[path = "ingress_wire_tests.rs"]
mod tests;
