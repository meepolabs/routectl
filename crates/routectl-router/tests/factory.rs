//! Provider factory tests with the default in-process SecretStore.

use routectl_auth::MemoryStore;
use routectl_core::Error;
use routectl_router::{build_provider, ProviderEntry, ReasoningDialect};

#[tokio::test]
async fn build_openai_compat_resolves_secret() {
    let store = MemoryStore::default();
    let entry = ProviderEntry::openai_compat("https://example.com/v1", "literal:sk-abc")
        .with_reasoning_dialect(ReasoningDialect::Openai);
    let provider = build_provider("test", &entry, &store).await.expect("build");
    assert_eq!(provider.id(), "openai-compat:test");
}

#[tokio::test]
async fn build_anthropic_api_resolves_secret() {
    let store = MemoryStore::default();
    let entry = ProviderEntry::anthropic_api("literal:sk-ant-abc");
    let provider = build_provider("anthropic", &entry, &store)
        .await
        .expect("build");
    assert_eq!(provider.id(), "anthropic-api:anthropic");
}

#[tokio::test]
async fn build_claude_cookie_returns_not_enabled() {
    let store = MemoryStore::default();
    let entry = ProviderEntry::claude_cookie("literal:fake-cookie");
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
    let entry = ProviderEntry::chatgpt_cookie("literal:fake-cookie");

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
api_key_ref = "env://ANTHROPIC_API_KEY"
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
            auth_kind,
            ..
        } => {
            assert_eq!(base_url, "https://api2.anthropic.com");
            assert_eq!(anthropic_version, "2024-05-01");
            assert_eq!(api_key_ref, "env://ANTHROPIC_API_KEY");
            // No `auth_kind` line in the TOML -> default api-key.
            assert_eq!(
                *auth_kind,
                routectl_providers::anthropic_api::AuthKind::ApiKey
            );
        }
        other => panic!("expected AnthropicApi, got {other:?}"),
    }
}

/// `auth_kind = "oauth-bearer"` round-trips into `AuthKind::OauthBearer`,
/// and the absence of the field defaults to `AuthKind::ApiKey`. Locks
/// in the kebab-case TOML surface.
#[test]
fn anthropic_auth_kind_round_trips_through_toml() {
    use routectl_providers::anthropic_api::AuthKind;

    let toml_src = r#"
[providers.claude-code]
type = "anthropic-api"
api_key_ref = "file:///home/me/.secrets/claude-code-oauth"
auth_kind = "oauth-bearer"

[providers.anthropic-default]
type = "anthropic-api"
api_key_ref = "env://ANTHROPIC_API_KEY"
"#;
    let cfg: routectl_router::Config = toml::from_str(toml_src).expect("parse");

    match cfg.providers.get("claude-code").expect("claude-code entry") {
        ProviderEntry::AnthropicApi { auth_kind, .. } => {
            assert_eq!(*auth_kind, AuthKind::OauthBearer);
        }
        other => panic!("expected AnthropicApi, got {other:?}"),
    }

    match cfg
        .providers
        .get("anthropic-default")
        .expect("anthropic-default entry")
    {
        ProviderEntry::AnthropicApi { auth_kind, .. } => {
            assert_eq!(*auth_kind, AuthKind::ApiKey);
        }
        other => panic!("expected AnthropicApi, got {other:?}"),
    }

    // Round-trip the other way: serialize and re-parse must preserve the value.
    let reserialized = toml::to_string(&cfg).expect("re-serialize");
    let cfg2: routectl_router::Config = toml::from_str(&reserialized).expect("re-parse");
    match cfg2.providers.get("claude-code").unwrap() {
        ProviderEntry::AnthropicApi { auth_kind, .. } => {
            assert_eq!(*auth_kind, AuthKind::OauthBearer);
        }
        _ => unreachable!(),
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
async fn build_with_missing_env_var_errors() {
    std::env::remove_var("ROUTECTL_TEST_MISSING_KEY");
    let store = MemoryStore::default();
    let entry = ProviderEntry::anthropic_api("env://ROUTECTL_TEST_MISSING_KEY");
    match build_provider("anthropic", &entry, &store).await {
        Err(Error::Auth(msg)) => {
            assert!(msg.contains("not set"), "got: {msg}");
        }
        Ok(_) => panic!("expected Err"),
        Err(other) => panic!("expected Error::Auth, got: {other:?}"),
    }
}
