//! Auth-wiring tests (TokenSource delegation + Debug redaction).

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A `TokenSource` that counts `on_auth_failure` invocations so we
/// can assert the provider delegates to it. `token()` returns a
/// fixed value; the counter proves the delegation wiring.
#[derive(Default)]
struct CountingTokenSource {
    on_auth_failure_calls: AtomicUsize,
}

impl std::fmt::Debug for CountingTokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CountingTokenSource").finish()
    }
}

#[async_trait]
impl TokenSource for CountingTokenSource {
    async fn token(&self) -> Result<String> {
        Ok("counting-jwt".into())
    }

    async fn on_auth_failure(&self) -> Result<()> {
        self.on_auth_failure_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// `Provider::on_auth_failure` must delegate to the underlying
/// `TokenSource::on_auth_failure` so an `oauth://` source can
/// force-refresh. Verified by a fake source that counts calls.
#[tokio::test]
async fn on_auth_failure_delegates_to_token_source() {
    // Arrange
    let source = Arc::new(CountingTokenSource::default());
    let mut cfg = OpenAiResponsesConfig::new("openai-responses:test", "unused");
    cfg.auth = source.clone();
    let provider = OpenAiResponsesProvider::new(cfg);

    // Act
    provider
        .on_auth_failure()
        .await
        .expect("on_auth_failure ok");
    provider
        .on_auth_failure()
        .await
        .expect("on_auth_failure ok");

    // Assert: each Provider-level call reached the token source.
    assert_eq!(source.on_auth_failure_calls.load(Ordering::SeqCst), 2);
}

/// Debug for `OpenAiResponsesConfig` must redact the auth source:
/// the inner token must never appear, and a `[REDACTED]` marker
/// must be present in its place.
#[test]
fn config_debug_redacts_auth_token() {
    // Arrange
    let cfg = OpenAiResponsesConfig::new("openai-responses:test", "super-secret-jwt");

    // Act
    let dbg = format!("{cfg:?}");

    // Assert
    assert!(
        !dbg.contains("super-secret-jwt"),
        "Debug must not leak the auth token; got: {dbg}"
    );
    assert!(
        dbg.contains("[REDACTED]"),
        "Debug must mark the auth field redacted; got: {dbg}"
    );
}
