//! Provider factory tests with the default in-process SecretStore.

use routectl_auth::{MemoryStore, SecretRef};
use routectl_core::Error;
use routectl_router::{build_provider, ProviderEntry, ReasoningDialect};

#[tokio::test]
async fn build_openai_compat_resolves_secret() {
    let store: std::sync::Arc<dyn routectl_auth::SecretStore> = std::sync::Arc::new(MemoryStore);
    let entry = ProviderEntry::openai_compat("https://example.com/v1", "literal:sk-abc");
    let provider = build_provider("test", &entry, store.clone())
        .await
        .expect("build");
    assert_eq!(provider.id(), "openai-compat:test");
}

#[tokio::test]
async fn build_anthropic_api_resolves_secret() {
    let store: std::sync::Arc<dyn routectl_auth::SecretStore> = std::sync::Arc::new(MemoryStore);
    let entry = ProviderEntry::anthropic_api("literal:sk-ant-abc");
    let provider = build_provider("anthropic", &entry, store.clone())
        .await
        .expect("build");
    assert_eq!(provider.id(), "anthropic-api:anthropic");
}

/// Defensive test: a custom `base_url` and `anthropic_version` in the
/// TOML config must round-trip cleanly into the Anthropic provider.
/// Locks in that future refactors don't silently re-hardcode the
/// production endpoint or API version.
#[test]
fn anthropic_custom_base_url_and_version_round_trip_through_toml() {
    let toml_src = r#"
[providers.anthropic]
kind = "anthropic-api"
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
kind = "anthropic-api"
api_key_ref = "file:///home/me/.secrets/claude-code-oauth"
auth_kind = "oauth-bearer"

[providers.anthropic-default]
kind = "anthropic-api"
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
/// NIM, llama.cpp) rely on this. v0.6.0 moved `reasoning_dialect` to
/// `[models.X]`; the provider entry no longer carries it.
#[test]
fn openai_compat_custom_base_url_round_trips_through_toml() {
    let toml_src = r#"
[providers.example-deepseek-host]
kind = "openai-compat"
base_url = "https://opencode.ai/zen/go/v1"
api_key_ref = "env://OPENCODE_GO_API_KEY"

[models.dsv4]
provider = "example-deepseek-host"
upstream = "deepseek-v4-pro"
reasoning_dialect = "deepseek"
"#;
    let cfg: routectl_router::Config = toml::from_str(toml_src).expect("parse");
    let entry = cfg
        .providers
        .get("example-deepseek-host")
        .expect("opencode entry");
    match entry {
        ProviderEntry::OpenaiCompat { base_url, .. } => {
            assert_eq!(base_url, "https://opencode.ai/zen/go/v1");
        }
        other => panic!("expected OpenaiCompat, got {other:?}"),
    }
    let model = cfg.models.get("dsv4").expect("model");
    assert_eq!(model.reasoning_dialect, Some(ReasoningDialect::Deepseek));
}

#[tokio::test]
async fn build_with_missing_env_var_errors() {
    std::env::remove_var("ROUTECTL_TEST_MISSING_KEY");
    let store: std::sync::Arc<dyn routectl_auth::SecretStore> = std::sync::Arc::new(MemoryStore);
    let entry = ProviderEntry::anthropic_api("env://ROUTECTL_TEST_MISSING_KEY");
    match build_provider("anthropic", &entry, store.clone()).await {
        Err(Error::Auth(msg)) => {
            assert!(msg.contains("not set"), "got: {msg}");
        }
        Ok(_) => panic!("expected Err"),
        Err(other) => panic!("expected Error::Auth, got: {other:?}"),
    }
}

/// TOML round-trip for Anthropic `header_extras` and `user_agent`
/// (renamed from `extra_headers` in v0.6.0).
#[test]
fn anthropic_header_extras_and_user_agent_round_trip_through_toml() {
    let toml_src = r#"
[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "env://ANTHROPIC_API_KEY"
user_agent = "claude-code/1.2.3"

[providers.anthropic.header_extras]
"anthropic-beta" = "context-1m-2025-08-07,prompt-caching-2024-07-31"
"x-custom-trace" = "abc123"
"#;
    let cfg: routectl_router::Config = toml::from_str(toml_src).expect("parse");
    let entry = cfg.providers.get("anthropic").expect("anthropic entry");
    match entry {
        ProviderEntry::AnthropicApi {
            header_extras,
            user_agent,
            ..
        } => {
            assert_eq!(user_agent.as_deref(), Some("claude-code/1.2.3"));
            assert_eq!(
                header_extras.get("anthropic-beta").map(String::as_str),
                Some("context-1m-2025-08-07,prompt-caching-2024-07-31"),
            );
            assert_eq!(
                header_extras.get("x-custom-trace").map(String::as_str),
                Some("abc123")
            );
        }
        other => panic!("expected AnthropicApi, got {other:?}"),
    }
}

/// Pin: `build_resolved_models` threads the originating `oauth://`
/// SecretRef onto each ResolvedModel so the 401 self-heal hook can
/// dispatch back through the OAuth store. MemoryStore parses the
/// URI fine; we never call `.token()` here, just inspect the
/// retained `auth_secret_ref`.
#[tokio::test]
async fn resolved_model_retains_oauth_secret_ref_for_anthropic() {
    use routectl_router::{build_resolved_models, BuildOptions, Config, ModelEntry};
    use std::collections::BTreeMap;

    let store: std::sync::Arc<dyn routectl_auth::SecretStore> = std::sync::Arc::new(MemoryStore);
    let mut providers: BTreeMap<String, ProviderEntry> = BTreeMap::new();
    providers.insert(
        "anthropic".into(),
        ProviderEntry::anthropic_api("oauth://anthropic"),
    );
    let mut models: BTreeMap<String, ModelEntry> = BTreeMap::new();
    models.insert(
        "claude".into(),
        ModelEntry::new("anthropic", "claude-sonnet-4-6"),
    );
    let cfg = Config {
        providers,
        models,
        ..Config::default()
    };

    let (resolved, failed) = build_resolved_models(&cfg, store, BuildOptions::default())
        .await
        .expect("build_resolved_models");
    assert!(failed.is_empty(), "expected no failures: {failed:?}");
    let m = resolved.get("claude").expect("claude entry");
    assert_eq!(
        m.auth_secret_ref,
        Some(SecretRef::OAuth {
            provider: "anthropic".into()
        })
    );
}

/// Pin: openai-compat entries with `env://` refs also have their
/// originating SecretRef retained on the ResolvedModel. The env var
/// itself doesn't need to exist for this test -- the URI is parsed
/// (not resolved) for `auth_secret_ref` propagation. The provider
/// build itself uses the literal value via the MemoryStore literal
/// shortcut, so we use `literal:k` for the actual build but the
/// retained ref reflects the original URI.
#[tokio::test]
#[serial_test::serial]
async fn resolved_model_retains_env_secret_ref_for_openai_compat() {
    use routectl_router::{build_resolved_models, BuildOptions, Config, ModelEntry};
    use std::collections::BTreeMap;

    // Set the env var so MemoryStore's env:// arm can resolve at
    // build time. The retained `auth_secret_ref` is parsed from the
    // URI string regardless.
    std::env::set_var("ROUTECTL_TEST_OPENAI_COMPAT_KEY", "sk-test-value");

    let store: std::sync::Arc<dyn routectl_auth::SecretStore> = std::sync::Arc::new(MemoryStore);
    let mut providers: BTreeMap<String, ProviderEntry> = BTreeMap::new();
    providers.insert(
        "host".into(),
        ProviderEntry::openai_compat(
            "https://example.com/v1",
            "env://ROUTECTL_TEST_OPENAI_COMPAT_KEY",
        ),
    );
    let mut models: BTreeMap<String, ModelEntry> = BTreeMap::new();
    models.insert("m".into(), ModelEntry::new("host", "some-model"));
    let cfg = Config {
        providers,
        models,
        ..Config::default()
    };

    let (resolved, failed) = build_resolved_models(&cfg, store, BuildOptions::default())
        .await
        .expect("build_resolved_models");
    std::env::remove_var("ROUTECTL_TEST_OPENAI_COMPAT_KEY");

    assert!(failed.is_empty(), "expected no failures: {failed:?}");
    let m = resolved.get("m").expect("m entry");
    assert_eq!(
        m.auth_secret_ref,
        Some(SecretRef::Env("ROUTECTL_TEST_OPENAI_COMPAT_KEY".into()))
    );
}

#[cfg(feature = "openai-responses")]
mod openai_responses_tests {
    use routectl_auth::MemoryStore;
    use routectl_core::Error;
    use routectl_router::{build_provider, Config, ProviderEntry};

    #[tokio::test]
    async fn factory_builds_openai_responses_chatgpt_oauth_provider() {
        // Arrange
        let toml_src = r#"
[providers.gpt]
kind = "openai-responses"
api_key_ref = "literal:test-jwt"
account_id_ref = "literal:acct-uuid"
auth_kind = "chatgpt-oauth"
"#;
        let cfg: Config = toml::from_str(toml_src).expect("parse");
        let entry = cfg.providers.get("gpt").expect("gpt entry");
        let store: std::sync::Arc<dyn routectl_auth::SecretStore> =
            std::sync::Arc::new(MemoryStore);

        // Act
        let provider = build_provider("gpt", entry, store.clone())
            .await
            .expect("build");

        // Assert
        assert_eq!(provider.id(), "openai-responses:gpt");
    }

    #[tokio::test]
    async fn factory_rejects_chatgpt_oauth_without_account_id_ref() {
        // Arrange
        let toml_src = r#"
[providers.gpt]
kind = "openai-responses"
api_key_ref = "literal:test-jwt"
auth_kind = "chatgpt-oauth"
"#;
        let cfg: Config = toml::from_str(toml_src).expect("parse");
        let entry = cfg.providers.get("gpt").expect("gpt entry");
        let store: std::sync::Arc<dyn routectl_auth::SecretStore> =
            std::sync::Arc::new(MemoryStore);

        // Act
        let result = build_provider("gpt", entry, store.clone()).await;

        // Assert
        match result {
            Err(Error::Config(msg)) => {
                assert!(msg.contains("chatgpt-oauth"), "msg: {msg}");
                assert!(msg.contains("account_id_ref"), "msg: {msg}");
            }
            Ok(_) => panic!("expected Err, got Ok"),
            Err(other) => panic!("expected Error::Config, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn factory_builds_openai_responses_api_key_provider() {
        // Arrange: api-key surface, no account_id_ref, default base_url.
        let toml_src = r#"
[providers.gpt-api]
kind = "openai-responses"
api_key_ref = "literal:sk-test-123"
auth_kind = "api-key"
"#;
        let cfg: Config = toml::from_str(toml_src).expect("parse");
        let entry = cfg.providers.get("gpt-api").expect("gpt-api entry");
        let store: std::sync::Arc<dyn routectl_auth::SecretStore> =
            std::sync::Arc::new(MemoryStore);

        // Act
        let provider = build_provider("gpt-api", entry, store.clone())
            .await
            .expect("build");

        // Assert
        assert_eq!(provider.id(), "openai-responses:gpt-api");
    }

    #[tokio::test]
    async fn factory_rejects_api_key_with_account_id_ref() {
        // Arrange
        let toml_src = r#"
[providers.gpt-api]
kind = "openai-responses"
api_key_ref = "literal:sk-test"
account_id_ref = "literal:acct-uuid"
auth_kind = "api-key"
"#;
        let cfg: Config = toml::from_str(toml_src).expect("parse");
        let entry = cfg.providers.get("gpt-api").expect("gpt-api entry");
        let store: std::sync::Arc<dyn routectl_auth::SecretStore> =
            std::sync::Arc::new(MemoryStore);

        // Act
        let result = build_provider("gpt-api", entry, store.clone()).await;

        // Assert
        match result {
            Err(Error::Config(msg)) => {
                assert!(msg.contains("account_id_ref"), "msg: {msg}");
                assert!(msg.contains("chatgpt-oauth"), "msg: {msg}");
            }
            Ok(_) => panic!("expected Err, got Ok"),
            Err(other) => panic!("expected Error::Config, got {other:?}"),
        }
    }

    #[test]
    fn openai_responses_default_auth_kind_is_chatgpt_oauth() {
        use routectl_providers::openai_responses::AuthKind;

        // Arrange: auth_kind omitted -> default.
        let toml_src = r#"
[providers.gpt]
kind = "openai-responses"
api_key_ref = "literal:test-jwt"
account_id_ref = "literal:acct-uuid"
"#;
        let cfg: Config = toml::from_str(toml_src).expect("parse");

        // Assert
        match cfg.providers.get("gpt").unwrap() {
            ProviderEntry::OpenaiResponses { auth_kind, .. } => {
                assert_eq!(*auth_kind, AuthKind::ChatgptOauth);
            }
            other => panic!("expected OpenaiResponses, got {other:?}"),
        }
    }
}

#[cfg(feature = "bedrock")]
mod bedrock_tests {
    use routectl_router::{BedrockApiShapeConfig, BedrockCredsConfig, Config, ProviderEntry};

    #[test]
    fn bedrock_invoke_with_bearer_key_round_trips() {
        let toml_src = r#"
[providers.bedrock_anthropic]
kind = "bedrock"
region = "us-west-2"
api_shape = "invoke"
user_agent = "claude-code/1.2.3"
anthropic_beta = ["context-1m-2025-08-07", "prompt-caching-2024-07-31"]
creds = { kind = "bearer-key", key_ref = "file:///home/me/.config/routectl/bedrock.key" }

[providers.bedrock_anthropic.header_extras]
"x-trace-id" = "abc"
"#;
        let cfg: Config = toml::from_str(toml_src).expect("parse");
        let entry = cfg.providers.get("bedrock_anthropic").expect("entry");
        match entry {
            ProviderEntry::Bedrock {
                region,
                api_shape,
                user_agent,
                anthropic_beta,
                header_extras,
                creds,
                ..
            } => {
                assert_eq!(region, "us-west-2");
                assert_eq!(*api_shape, BedrockApiShapeConfig::Invoke);
                assert_eq!(user_agent.as_deref(), Some("claude-code/1.2.3"));
                assert_eq!(
                    anthropic_beta,
                    &[
                        "context-1m-2025-08-07".to_string(),
                        "prompt-caching-2024-07-31".to_string(),
                    ]
                );
                assert_eq!(
                    header_extras.get("x-trace-id").map(String::as_str),
                    Some("abc")
                );
                match creds {
                    BedrockCredsConfig::BearerKey { key_ref } => {
                        assert_eq!(key_ref, "file:///home/me/.config/routectl/bedrock.key");
                    }
                    other => panic!("expected BearerKey, got {other:?}"),
                }
            }
            other => panic!("expected Bedrock, got {other:?}"),
        }
    }

    #[test]
    fn bedrock_static_creds_round_trip_with_optional_session_token() {
        // TOML inline tables must be single-line, so we use a sub-table
        // for `creds` here. Both syntaxes serialize to the same struct.
        let toml_src = r#"
[providers.bedrock_static]
kind = "bedrock"
region = "us-west-2"

[providers.bedrock_static.creds]
kind = "static"
access_key_ref = "env://AWS_ACCESS_KEY_ID"
secret_key_ref = "env://AWS_SECRET_ACCESS_KEY"
session_token_ref = "env://AWS_SESSION_TOKEN"
"#;
        let cfg: Config = toml::from_str(toml_src).expect("parse");
        match cfg.providers.get("bedrock_static").unwrap() {
            ProviderEntry::Bedrock {
                creds, api_shape, ..
            } => {
                // Default api_shape when omitted -> Invoke.
                assert_eq!(*api_shape, BedrockApiShapeConfig::Invoke);
                match creds {
                    BedrockCredsConfig::Static {
                        access_key_ref,
                        secret_key_ref,
                        session_token_ref,
                    } => {
                        assert_eq!(access_key_ref, "env://AWS_ACCESS_KEY_ID");
                        assert_eq!(secret_key_ref, "env://AWS_SECRET_ACCESS_KEY");
                        assert_eq!(
                            session_token_ref.as_deref(),
                            Some("env://AWS_SESSION_TOKEN"),
                        );
                    }
                    other => panic!("expected Static, got {other:?}"),
                }
            }
            other => panic!("expected Bedrock, got {other:?}"),
        }
    }

    #[test]
    fn bedrock_profile_creds_round_trip() {
        let toml_src = r#"
[providers.bedrock_profile]
kind = "bedrock"
region = "us-west-2"
api_shape = "converse"
creds = { kind = "profile", name = "bedrock-prod" }
"#;
        let cfg: Config = toml::from_str(toml_src).expect("parse");
        match cfg.providers.get("bedrock_profile").unwrap() {
            ProviderEntry::Bedrock {
                creds, api_shape, ..
            } => {
                assert_eq!(*api_shape, BedrockApiShapeConfig::Converse);
                match creds {
                    BedrockCredsConfig::Profile { name } => {
                        assert_eq!(name, "bedrock-prod");
                    }
                    other => panic!("expected Profile, got {other:?}"),
                }
            }
            other => panic!("expected Bedrock, got {other:?}"),
        }
    }

    #[test]
    fn bedrock_default_chain_round_trips() {
        let toml_src = r#"
[providers.bedrock_chain]
kind = "bedrock"
region = "us-west-2"
creds = { kind = "default-chain" }
"#;
        let cfg: Config = toml::from_str(toml_src).expect("parse");
        match cfg.providers.get("bedrock_chain").unwrap() {
            ProviderEntry::Bedrock { creds, .. } => {
                assert!(matches!(creds, BedrockCredsConfig::DefaultChain));
            }
            other => panic!("expected Bedrock, got {other:?}"),
        }
    }

    #[test]
    fn bedrock_redact_secrets_clears_literals_only() {
        let mut creds = BedrockCredsConfig::Static {
            access_key_ref: "literal:AKIAACTUAL".into(),
            secret_key_ref: "env://AWS_SECRET_ACCESS_KEY".into(),
            session_token_ref: Some("literal:abc123".into()),
        };
        creds.redact();
        match creds {
            BedrockCredsConfig::Static {
                access_key_ref,
                secret_key_ref,
                session_token_ref,
            } => {
                assert_eq!(access_key_ref, "literal:[REDACTED]");
                assert_eq!(secret_key_ref, "env://AWS_SECRET_ACCESS_KEY");
                assert_eq!(session_token_ref.as_deref(), Some("literal:[REDACTED]"));
            }
            _ => unreachable!(),
        }
    }
}
