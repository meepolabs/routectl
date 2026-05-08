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

/// Set of valid tokens. `None` (or empty) means "no auth required".
///
/// Token comparison is intentionally a per-token constant-time
/// equality so an attacker reaching the listener cannot binary-search
/// a valid token via response-time differences. This matters most
/// when `--unsafe-public` is set (the listener is reachable beyond
/// loopback); on pure loopback the risk is theoretical, but the cost
/// of a constant-time loop is negligible.
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

    fn contains(&self, candidate: &[u8]) -> bool {
        // Iterate every entry rather than short-circuiting -- the
        // length check + constant_time_eq below is what makes
        // comparison resistant to byte-by-byte timing inference.
        // Keeping the loop unbounded over the token list would be
        // overkill and is not a meaningful threat (the set is small
        // and known at startup), so we accept O(N) over the set
        // size.
        let mut hit = false;
        for token in self.tokens.iter() {
            if token.len() == candidate.len() && constant_time_eq(token, candidate) {
                hit = true;
            }
        }
        hit
    }
}

/// Byte-wise constant-time equality. Both inputs MUST be the same
/// length (the caller checks). Avoids `==` on `&[u8]` and
/// `String::==` which short-circuit on first mismatch.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    debug_assert_eq!(a.len(), b.len());
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn extract_x_api_key(headers: &axum::http::HeaderMap) -> Option<&str> {
    headers.get("x-api-key").and_then(|v| v.to_str().ok())
}

fn extract_bearer(headers: &axum::http::HeaderMap) -> Option<&str> {
    let raw = headers.get("authorization")?.to_str().ok()?;
    raw.strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
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
        let body = json!({
            "error": {
                "type": "authentication_error",
                "message": "missing or invalid api key"
            }
        });
        (StatusCode::UNAUTHORIZED, Json(body)).into_response()
    }
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
}
