//! Source for an authentication token (bearer string, API key, etc.).
//!
//! Providers that need a credential per request hold an `Arc<dyn
//! TokenSource>` rather than a baked-in `String`. The default impl
//! (`StaticToken`) caches a value resolved once at construction --
//! semantically equivalent to the pre-v0.7 `api_key: String` field.
//! Providers that route through routectl-managed OAuth can be given
//! a different impl (constructed in `routectl-router`'s factory)
//! that re-resolves through `SecretStore::get` per request, so token
//! rotation in the credentials store is picked up live without
//! restart.
//!
//! The trait lives in `routectl-core` -- not `routectl-auth` -- so
//! `routectl-providers` can depend on it without inheriting the
//! `SecretRef`/`SecretStore` surface (which is auth-policy, not
//! provider concern). The bedrock module already maintains this
//! separation; OAuth follows the same pattern.

use async_trait::async_trait;

use crate::Result;

/// Provides the current bearer/API-key token for a provider's
/// outbound HTTP requests. Implementations may cache, refresh, or
/// re-read on every call -- the contract is "give me the token to
/// send right now".
#[async_trait]
pub trait TokenSource: Send + Sync + std::fmt::Debug {
    /// Return the current token. Called once per upstream request,
    /// so an OAuth-managed source can re-read from disk + refresh
    /// here without ingress contamination.
    async fn token(&self) -> Result<String>;
}

/// Cached-string token. Used for `env://`, `file://`, and `literal:`
/// secret refs (resolved once at provider construction time, never
/// re-read). Cheap to clone; cheap to hold under `Arc`.
pub struct StaticToken {
    value: String,
}

impl StaticToken {
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
        }
    }
}

impl std::fmt::Debug for StaticToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never expose the token in Debug output. The provider's
        // own redaction is the second line of defense, but a
        // well-behaved TokenSource never lets the value escape.
        f.debug_struct("StaticToken")
            .field("value", &"[REDACTED]")
            .finish()
    }
}

#[async_trait]
impl TokenSource for StaticToken {
    async fn token(&self) -> Result<String> {
        Ok(self.value.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn static_token_returns_value() {
        let s: Arc<dyn TokenSource> = Arc::new(StaticToken::new("sk-foo"));
        assert_eq!(s.token().await.unwrap(), "sk-foo");
    }

    #[test]
    fn static_token_debug_redacts() {
        let s = StaticToken::new("sk-secret-123");
        let dbg = format!("{s:?}");
        assert!(!dbg.contains("sk-secret-123"));
        assert!(dbg.contains("REDACTED"));
    }

    #[tokio::test]
    async fn static_token_can_be_cloned_through_arc() {
        let s: Arc<dyn TokenSource> = Arc::new(StaticToken::new("k"));
        let s2 = s.clone();
        assert_eq!(s.token().await.unwrap(), s2.token().await.unwrap());
    }
}
