//! Listener-side authentication middleware.
//!
//! When `[server.auth].tokens` is non-empty, every request must carry
//! a matching `x-api-key` or `Authorization: Bearer <token>` header.
//! This is purely a listener gate -- upstream credentials come from
//! the per-provider config and are never derived from inbound auth
//! (no credential bridging, no token storage). The token set is
//! resolved at startup via `SecretStore` and held in memory.
//!
//! Local-loopback default: when `tokens` is empty (or
//! `[server.auth]` is absent), the middleware is bypassed entirely.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Json, Response};
use serde_json::json;
use subtle::ConstantTimeEq;

use crate::ingress::ErrorEnvelopeShape;

/// Set of valid tokens. `None` (or empty) means "no auth required".
///
/// Token comparison uses `subtle::ConstantTimeEq` so an attacker
/// reaching the listener cannot binary-search a valid token's value
/// via response-time differences. Per-token `[u8]::ct_eq` is
/// constant-time in the token CONTENTS; it does short-circuit on a
/// length mismatch (subtle-documented), which can leak a configured
/// token's LENGTH but never its value. The outer loop never
/// early-returns on a match -- it accumulates hits across all tokens
/// so it does not leak which entry matched (or that any did). This
/// matters most when `--unsafe-public` is set (listener
/// reachable beyond loopback); on pure loopback the risk is
/// theoretical, but the cost of the constant-time loop is negligible.
#[derive(Debug, Default, Clone)]
pub struct TokenSet {
    tokens: Arc<Vec<Vec<u8>>>,
}

impl TokenSet {
    pub fn new(tokens: Vec<String>) -> Self {
        Self {
            tokens: Arc::new(tokens.into_iter().map(String::into_bytes).collect()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Check the request headers for a valid token. Accepts both
    /// `x-api-key: <token>` and `Authorization: Bearer <token>`.
    pub fn check(&self, headers: &axum::http::HeaderMap) -> bool {
        if self.tokens.is_empty() {
            return true;
        }
        if let Some(t) = extract_x_api_key(headers) {
            if self.contains(t.as_bytes()) {
                return true;
            }
        }
        if let Some(t) = extract_bearer(headers) {
            if self.contains(t.as_bytes()) {
                return true;
            }
        }
        false
    }

    /// Constant-time membership test. Iterates every token and
    /// accumulates a hit via `subtle::ConstantTimeEq`, with no early
    /// return on first match. `[u8]::ct_eq` short-circuits on a length
    /// mismatch (subtle-documented), so per-token timing can leak a
    /// configured token's length but never its contents. The outer
    /// loop always runs `self.tokens.len()` iterations, so it does not
    /// leak whether or where a match occurred.
    fn contains(&self, candidate: &[u8]) -> bool {
        let mut hit: u8 = 0;
        for token in self.tokens.iter() {
            // ct_eq returns Choice(1) on equal, Choice(0) on unequal.
            // unwrap_u8() extracts that as 1 or 0.
            // Tokens of different lengths always produce 0 (no match).
            hit |= token.as_slice().ct_eq(candidate).unwrap_u8();
        }
        hit != 0
    }
}

fn extract_x_api_key(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers.get("x-api-key").and_then(|v| v.to_str().ok())
}

fn extract_bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    let raw = headers.get("authorization")?.to_str().ok()?;
    // Auth schemes are case-insensitive per RFC 7235 sec 2.1; a
    // header like `Authorization: BEARER sk-...` from a paranoid
    // client must not be rejected. Compare the scheme portion
    // case-insensitively without re-allocating the whole string
    // by walking only the first 7 chars (`bearer `).
    let (scheme, rest) = raw.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        Some(rest)
    } else {
        None
    }
}

/// Returns `true` if the `Authorization` header carries a Bearer
/// token (scheme is "bearer", case-insensitive, per RFC 7235 sec 2.1).
/// Used by the auth rejection log to record whether the client even
/// attempted Bearer auth.
fn has_bearer_header(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split_once(' '))
        .map(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .unwrap_or(false)
}

/// Axum middleware. Mounted only when `TokenSet` is non-empty (the
/// server skips the layer entirely otherwise so loopback dev is
/// unchanged).
pub async fn auth_layer(
    State(tokens): State<Arc<TokenSet>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    if tokens.check(req.headers()) {
        next.run(req).await
    } else {
        // Log the SHAPE of the rejected request (which auth header
        // the client sent, if any) so operators can distinguish
        // "client never authenticated" from "client sent the wrong
        // token". Never log the supplied value or any prefix --
        // even a few bytes of a leaked secret materially helps an
        // attacker.
        let path = req.uri().path();
        let has_x_api_key = req.headers().contains_key("x-api-key");
        let has_bearer = has_bearer_header(req.headers());
        tracing::warn!(
            route = %path,
            has_x_api_key,
            has_bearer,
            "listener auth rejected",
        );
        // Path-aware envelope: the auth layer fires BEFORE any
        // ingress is selected, but the inbound path tells us which
        // dialect the caller speaks. claude-code parses Anthropic
        // shape on /v1/messages*; OpenAI SDKs parse OpenAI shape on
        // /v1/chat/completions*. Anything else stays on the OpenAI
        // shape (the historical default; covers /v1/models and
        // future routes).
        let shape = envelope_shape_for_path(path);
        unauthorized_response(shape)
    }
}

/// Pick the error-envelope shape based on the inbound URL path.
/// Exact-match against the routes that speak Anthropic Messages today;
/// everything else (incl. `/v1/chat/completions`, `/v1/models`, future
/// routes, and any same-prefix attacker path like
/// `/v1/messages-x`) gets the OpenAI shape. New Anthropic routes MUST
/// be added here explicitly -- prefix matching would silently catch
/// non-Messages paths.
fn envelope_shape_for_path(path: &str) -> ErrorEnvelopeShape {
    match path {
        "/v1/messages" | "/v1/messages/count_tokens" => ErrorEnvelopeShape::Anthropic,
        _ => ErrorEnvelopeShape::OpenAi,
    }
}

/// Render a 401 unauthorized response with the dialect-specific
/// envelope. The auth layer fires before any handler so we render
/// the envelope inline rather than reusing `ingress_handle::map_error`
/// (no `Error` value to map; just a fixed authentication error).
fn unauthorized_response(shape: ErrorEnvelopeShape) -> Response {
    let body = match shape {
        ErrorEnvelopeShape::OpenAi => json!({
            "error": {
                "type": "authentication_error",
                "message": "missing or invalid api key",
            }
        }),
        ErrorEnvelopeShape::Anthropic => json!({
            "type": "error",
            "error": {
                "type": "authentication_error",
                "message": "missing or invalid api key",
            }
        }),
    };
    (StatusCode::UNAUTHORIZED, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderMap;

    fn hm(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn envelope_shape_for_path_exact_matches_anthropic_routes() {
        // Real Anthropic Messages routes get the Anthropic envelope.
        assert_eq!(
            envelope_shape_for_path("/v1/messages"),
            ErrorEnvelopeShape::Anthropic
        );
        assert_eq!(
            envelope_shape_for_path("/v1/messages/count_tokens"),
            ErrorEnvelopeShape::Anthropic
        );
    }

    #[test]
    fn envelope_shape_for_path_prefix_collisions_default_to_openai() {
        // Defense-in-depth: a path that merely shares the /v1/messages
        // prefix but is NOT a registered Anthropic route falls through
        // to the OpenAI envelope. Forces future routes to be added to
        // the match arm explicitly.
        for path in &[
            "/v1/messages-something-malicious",
            "/v1/messages_underscore",
            "/v1/messagesfoo",
            "/v1/messages/extra/path",
            "/v1/chat/completions",
            "/v1/models",
            "/",
            "",
        ] {
            assert_eq!(
                envelope_shape_for_path(path),
                ErrorEnvelopeShape::OpenAi,
                "path `{path}` should default to OpenAI envelope",
            );
        }
    }

    #[test]
    fn empty_token_set_lets_everything_through() {
        let ts = TokenSet::default();
        assert!(ts.check(&hm(&[])));
        assert!(ts.check(&hm(&[("x-api-key", "anything")])));
    }

    #[test]
    fn x_api_key_match_passes() {
        let ts = TokenSet::new(vec!["sk-ok".into()]);
        assert!(ts.check(&hm(&[("x-api-key", "sk-ok")])));
    }

    #[test]
    fn authorization_bearer_match_passes() {
        let ts = TokenSet::new(vec!["sk-ok".into()]);
        assert!(ts.check(&hm(&[("authorization", "Bearer sk-ok")])));
    }

    #[test]
    fn lowercase_bearer_prefix_passes() {
        let ts = TokenSet::new(vec!["sk-ok".into()]);
        assert!(ts.check(&hm(&[("authorization", "bearer sk-ok")])));
    }

    #[test]
    fn wrong_token_rejected() {
        let ts = TokenSet::new(vec!["sk-good".into()]);
        assert!(!ts.check(&hm(&[("x-api-key", "sk-bad")])));
        assert!(!ts.check(&hm(&[("authorization", "Bearer sk-bad")])));
    }

    #[test]
    fn missing_credentials_rejected_when_required() {
        let ts = TokenSet::new(vec!["sk-good".into()]);
        assert!(!ts.check(&hm(&[])));
    }

    #[test]
    fn non_bearer_authorization_scheme_ignored() {
        let ts = TokenSet::new(vec!["sk-good".into()]);
        assert!(!ts.check(&hm(&[("authorization", "Basic c2s6Zm9v")])));
    }

    // Fix 1: different-length candidate returns false, no panic.
    #[test]
    fn different_length_candidate_returns_false() {
        // Arrange: token set with a known-length token.
        let ts = TokenSet::new(vec!["sk-long-token".into()]);

        // Act + Assert: shorter and longer candidates must both fail
        // cleanly without panicking.
        assert!(!ts.check(&hm(&[("x-api-key", "short")])));
        assert!(!ts.check(&hm(&[("x-api-key", "sk-long-token-extra")])));
    }

    // Fix 2: uppercase BEARER scheme must produce has_bearer=true.
    #[test]
    fn has_bearer_header_is_case_insensitive() {
        // Arrange
        let upper = hm(&[("authorization", "BEARER sk-x")]);
        let mixed = hm(&[("authorization", "Bearer sk-x")]);
        let lower = hm(&[("authorization", "bearer sk-x")]);
        let basic = hm(&[("authorization", "Basic dXNlcjpwYXNz")]);
        let empty = hm(&[]);

        // Act + Assert
        assert!(has_bearer_header(&upper), "BEARER should be recognized");
        assert!(has_bearer_header(&mixed), "Bearer should be recognized");
        assert!(has_bearer_header(&lower), "bearer should be recognized");
        assert!(!has_bearer_header(&basic), "Basic scheme should not match");
        assert!(!has_bearer_header(&empty), "absent header should be false");
    }
}
