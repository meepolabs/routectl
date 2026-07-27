//! The factory provider-loop boundary (`build_resolved_models`) installs
//! the configured codex identity before constructing providers, so the
//! knob reaches the wire on every path that routes through it (serve,
//! reload, `routectl test`, doctor) -- not just the serve builder.
//!
//! `resolved_identity` is a set-once process-global, so this lives in its
//! own test binary with a SINGLE test.

use std::sync::Arc;

use routectl_auth::{MemoryStore, SecretStore};
use routectl_router::{BuildOptions, Config, build_resolved_models};

#[tokio::test]
async fn build_resolved_models_installs_configured_codex_version() {
    // Arrange: a config whose openai-responses provider pins a codex
    // version. The identity install runs at the START of
    // build_resolved_models (before any provider construction), so the
    // assertion holds even if a provider build were to fail.
    let config: Config = toml::from_str(
        "[providers.cx]\nkind = \"openai-responses\"\napi_key_ref = \"literal:sk-test\"\n\
         auth_kind = \"api-key\"\ncodex_version = \"3.3.3-build\"\n\
         [models.m]\nprovider = \"cx\"\nupstream = \"gpt-5\"\n",
    )
    .expect("fixture parses");
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);

    // Act
    let _ = build_resolved_models(&config, store, BuildOptions::default()).await;

    // Assert: every codex fingerprint surface now reads the configured
    // version -- UA (egress client default + OAuth refresh) and the egress
    // `version` header both derive from this one resolved identity.
    let identity = routectl_core::identity::codex::resolved_identity();
    assert_eq!(identity.version(), "3.3.3-build");
    assert!(
        identity.user_agent().contains("3.3.3-build"),
        "resolved UA must carry the configured version: {}",
        identity.user_agent(),
    );
    assert_eq!(
        routectl_core::identity::codex::codex_user_agent(),
        identity.user_agent(),
        "the free-function wrapper must return the resolved UA",
    );
}
