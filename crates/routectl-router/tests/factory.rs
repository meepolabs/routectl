//! Provider factory tests with the in-memory SecretStore.

use std::collections::BTreeMap;

use routectl_auth::{MemoryStore, SecretRef, SecretStore};
use routectl_core::Error;
use routectl_router::{build_provider, ProviderEntry, ReasoningDialect};

fn store_with_key(uri: &str, value: &str) -> MemoryStore {
    let store = MemoryStore::default();
    let secret_ref = SecretRef::parse(uri).expect("parse");
    futures::executor::block_on(async {
        store.set(&secret_ref, value).await.expect("set");
    });
    store
}

#[tokio::test]
async fn build_openai_compat_resolves_secret() {
    let store = store_with_key("keychain://routectl/test", "sk-abc");
    let entry = ProviderEntry::OpenaiCompat {
        base_url: "https://example.com/v1".into(),
        api_key_ref: "keychain://routectl/test".into(),
        extra_headers: BTreeMap::new(),
        default_extras: None,
        reasoning_dialect: ReasoningDialect::Openai,
    };
    let provider = build_provider("test", &entry, &store).await.expect("build");
    assert_eq!(provider.id(), "openai-compat:test");
}

#[tokio::test]
async fn build_anthropic_api_resolves_secret() {
    let store = store_with_key("keychain://routectl/anthropic", "sk-ant-abc");
    let entry = ProviderEntry::AnthropicApi {
        api_key_ref: "keychain://routectl/anthropic".into(),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
    };
    let provider = build_provider("anthropic", &entry, &store).await.expect("build");
    assert_eq!(provider.id(), "anthropic-api:anthropic");
}

#[tokio::test]
async fn build_claude_cookie_returns_not_enabled() {
    let store = MemoryStore::default();
    let entry = ProviderEntry::ClaudeCookie {
        session_ref: "keychain://routectl/claude".into(),
        organization_id: None,
    };
    match build_provider("claude-pro", &entry, &store).await {
        Err(Error::Auth(msg)) => {
            assert!(msg.contains("claude-cookie"), "got: {msg}");
            assert!(msg.contains("not enabled"), "got: {msg}");
        }
        Ok(_) => panic!("expected Err"),
        Err(other) => panic!("expected Error::Auth, got: {other:?}"),
    }
}

#[tokio::test]
async fn build_chatgpt_cookie_returns_not_enabled() {
    let store = MemoryStore::default();
    let entry = ProviderEntry::ChatgptCookie {
        session_ref: "keychain://routectl/chatgpt".into(),
    };
    match build_provider("chatgpt-plus", &entry, &store).await {
        Err(Error::Auth(_)) => {}
        Ok(_) => panic!("expected Err"),
        Err(other) => panic!("expected Error::Auth, got: {other:?}"),
    }
}

#[tokio::test]
async fn build_with_unknown_secret_errors() {
    let store = MemoryStore::default();
    let entry = ProviderEntry::AnthropicApi {
        api_key_ref: "keychain://routectl/missing".into(),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
    };
    match build_provider("anthropic", &entry, &store).await {
        Err(Error::Auth(_)) => {}
        Ok(_) => panic!("expected Err"),
        Err(other) => panic!("expected Error::Auth, got: {other:?}"),
    }
}
