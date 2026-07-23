use super::*;
use crate::server::test_support::isolate_usage_db;
use routectl_auth::MemoryStore;

#[tokio::test]
async fn build_router_twice_from_same_secrets_handle_succeeds() {
    // Hot-reload smoke test: rebuild a Router twice from the
    // same `Arc<dyn SecretStore>` handle. Pinning the no-panic
    // contract -- a regression that drops the per-provider
    // single-flight refresh mutex on rebuild would either error
    // on the second build or surface a dangling Arc here.
    use routectl_auth::MemoryStore;
    use routectl_router::Config;
    use std::sync::Arc;

    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let config = Arc::new(Config::default());

    let r1 = build_router_from_config(config.clone(), secrets.clone())
        .await
        .expect("first router build");
    let r2 = build_router_from_config(config.clone(), secrets.clone())
        .await
        .expect("second router build");

    // Sanity: each call returns a fresh Router but they share
    // the same Config (Arc-shared at construction time).
    assert!(Arc::ptr_eq(&r1.config, &config));
    assert!(Arc::ptr_eq(&r2.config, &config));
}

/// `validate_provider_credential_sources` is wired into
/// `build_router_from_config_with_overlay` itself (not only reachable
/// via the separate `commands::config::check` call to the same
/// validator) -- a forwarded provider pointed at a non-Anthropic host
/// must fail the router build, i.e. serve startup and hot reload,
/// which both call this builder directly.
#[tokio::test]
async fn build_router_from_config_rejects_forwarded_provider_on_non_anthropic_host() {
    use routectl_router::ProviderEntry;
    use routectl_router::config::CredentialSource;

    let mut config = Config::default();
    let _usage_dir = isolate_usage_db(&mut config);
    config.providers.insert(
        "sneaky".into(),
        ProviderEntry::anthropic_api("")
            .with_base_url("https://evil.example.com")
            .with_credential_source(CredentialSource::Forwarded),
    );
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());

    // `Router` is not `Debug`, so match rather than `expect_err`.
    let err = match build_router_from_config(Arc::new(config), secrets).await {
        Ok(_) => panic!("forwarded provider off the pinned host must fail the router build"),
        Err(e) => e,
    };
    assert!(matches!(err, Error::Config(_)), "got: {err:?}");
}
