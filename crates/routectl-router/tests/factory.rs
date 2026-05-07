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
        runtime: Default::default(),
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
        runtime: Default::default(),
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
        runtime: Default::default(),
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
        runtime: Default::default(),
    };
    match build_provider("chatgpt-plus", &entry, &store).await {
        Err(Error::Auth(_)) => {}
        Ok(_) => panic!("expected Err"),
        Err(other) => panic!("expected Error::Auth, got: {other:?}"),
    }
}

/// Defensive test: a custom `base_url` and `anthropic_version` in the
/// TOML config must round-trip cleanly into the Anthropic provider.
/// Locks in that future refactors don't silently re-hardcode the
/// production endpoint or API version.
#[test]
fn anthropic_custom_base_url_and_version_round_trip_through_toml() {
    let toml_src = r#"
[providers.anthropic]
type = "anthropic-api"
api_key_ref = "keychain://routectl/anthropic"
base_url = "https://api2.anthropic.com"
anthropic_version = "2024-05-01"
"#;
    let cfg: routectl_router::Config = toml::from_str(toml_src).expect("parse");
    let entry = cfg.providers.get("anthropic").expect("anthropic entry");
    match entry {
        ProviderEntry::AnthropicApi {
            base_url,
            anthropic_version,
            api_key_ref,
            ..
        } => {
            assert_eq!(base_url, "https://api2.anthropic.com");
            assert_eq!(anthropic_version, "2024-05-01");
            assert_eq!(api_key_ref, "keychain://routectl/anthropic");
        }
        other => panic!("expected AnthropicApi, got {other:?}"),
    }
}

/// Same test but for the OpenAI-compat shape: a custom `base_url`
/// must survive TOML round-trip. Lots of providers (OpenCode-Go,
/// NIM, llama.cpp) rely on this.
#[test]
fn openai_compat_custom_base_url_round_trips_through_toml() {
    let toml_src = r#"
[providers.opencode-go]
type = "openai-compat"
base_url = "https://opencode.ai/zen/go/v1"
api_key_ref = "env://OPENCODE_GO_API_KEY"
reasoning_dialect = "deepseek"
"#;
    let cfg: routectl_router::Config = toml::from_str(toml_src).expect("parse");
    let entry = cfg.providers.get("opencode-go").expect("opencode entry");
    match entry {
        ProviderEntry::OpenaiCompat {
            base_url,
            reasoning_dialect,
            ..
        } => {
            assert_eq!(base_url, "https://opencode.ai/zen/go/v1");
            assert!(matches!(reasoning_dialect, ReasoningDialect::Deepseek));
        }
        other => panic!("expected OpenaiCompat, got {other:?}"),
    }
}

#[tokio::test]
async fn build_with_unknown_secret_errors() {
    let store = MemoryStore::default();
    let entry = ProviderEntry::AnthropicApi {
        api_key_ref: "keychain://routectl/missing".into(),
        base_url: "https://api.anthropic.com".into(),
        anthropic_version: "2023-06-01".into(),
        runtime: Default::default(),
    };
    match build_provider("anthropic", &entry, &store).await {
        Err(Error::Auth(_)) => {}
        Ok(_) => panic!("expected Err"),
        Err(other) => panic!("expected Error::Auth, got: {other:?}"),
    }
}
