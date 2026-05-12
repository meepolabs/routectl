//! Provider factory tests with the default in-process SecretStore.

use routectl_auth::MemoryStore;
use routectl_core::Error;
use routectl_router::{build_provider, ProviderEntry, ReasoningDialect};

#[tokio::test]
async fn build_openai_compat_resolves_secret() {
    let store = MemoryStore;
    let entry = ProviderEntry::openai_compat("https://example.com/v1", "literal:sk-abc")
        .with_reasoning_dialect(ReasoningDialect::Openai);
    let provider = build_provider("test", &entry, &store).await.expect("build");
    assert_eq!(provider.id(), "openai-compat:test");
}

#[tokio::test]
async fn build_anthropic_api_resolves_secret() {
    let store = MemoryStore;
    let entry = ProviderEntry::anthropic_api("literal:sk-ant-abc");
    let provider = build_provider("anthropic", &entry, &store)
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
    let store = MemoryStore;
    let entry = ProviderEntry::anthropic_api("env://ROUTECTL_TEST_MISSING_KEY");
    match build_provider("anthropic", &entry, &store).await {
        Err(Error::Auth(msg)) => {
            assert!(msg.contains("not set"), "got: {msg}");
        }
        Ok(_) => panic!("expected Err"),
        Err(other) => panic!("expected Error::Auth, got: {other:?}"),
    }
}

/// TOML round-trip for Anthropic `extra_headers` and `user_agent`.
#[test]
fn anthropic_extra_headers_and_user_agent_round_trip_through_toml() {
    let toml_src = r#"
[providers.anthropic]
type = "anthropic-api"
api_key_ref = "env://ANTHROPIC_API_KEY"
user_agent = "claude-code/1.2.3"

[providers.anthropic.extra_headers]
"anthropic-beta" = "context-1m-2025-08-07,prompt-caching-2024-07-31"
"x-custom-trace" = "abc123"
"#;
    let cfg: routectl_router::Config = toml::from_str(toml_src).expect("parse");
    let entry = cfg.providers.get("anthropic").expect("anthropic entry");
    match entry {
        ProviderEntry::AnthropicApi {
            extra_headers,
            user_agent,
            ..
        } => {
            assert_eq!(user_agent.as_deref(), Some("claude-code/1.2.3"));
            assert_eq!(
                extra_headers.get("anthropic-beta").map(String::as_str),
                Some("context-1m-2025-08-07,prompt-caching-2024-07-31"),
            );
            assert_eq!(
                extra_headers.get("x-custom-trace").map(String::as_str),
                Some("abc123")
            );
        }
        other => panic!("expected AnthropicApi, got {other:?}"),
    }
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
type = "openai-responses"
api_key_ref = "literal:test-jwt"
account_id_ref = "literal:acct-uuid"
auth_kind = "chatgpt-oauth"
"#;
        let cfg: Config = toml::from_str(toml_src).expect("parse");
        let entry = cfg.providers.get("gpt").expect("gpt entry");
        let store = MemoryStore;

        // Act
        let provider = build_provider("gpt", entry, &store).await.expect("build");

        // Assert
        assert_eq!(provider.id(), "openai-responses:gpt");
    }

    #[tokio::test]
    async fn factory_rejects_chatgpt_oauth_without_account_id_ref() {
        // Arrange
        let toml_src = r#"
[providers.gpt]
type = "openai-responses"
api_key_ref = "literal:test-jwt"
auth_kind = "chatgpt-oauth"
"#;
        let cfg: Config = toml::from_str(toml_src).expect("parse");
        let entry = cfg.providers.get("gpt").expect("gpt entry");
        let store = MemoryStore;

        // Act
        let result = build_provider("gpt", entry, &store).await;

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
    async fn factory_rejects_api_key_with_account_id_ref() {
        // Arrange
        let toml_src = r#"
[providers.gpt-api]
type = "openai-responses"
api_key_ref = "literal:sk-test"
account_id_ref = "literal:acct-uuid"
auth_kind = "api-key"
"#;
        let cfg: Config = toml::from_str(toml_src).expect("parse");
        let entry = cfg.providers.get("gpt-api").expect("gpt-api entry");
        let store = MemoryStore;

        // Act
        let result = build_provider("gpt-api", entry, &store).await;

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
type = "openai-responses"
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
type = "bedrock"
region = "us-west-2"
model_id = "us.anthropic.claude-opus-4-7"
api_shape = "invoke"
user_agent = "claude-code/1.2.3"
anthropic_beta = ["context-1m-2025-08-07", "prompt-caching-2024-07-31"]
creds = { kind = "bearer-key", key_ref = "file:///home/me/.config/routectl/bedrock.key" }

[providers.bedrock_anthropic.extra_headers]
"x-trace-id" = "abc"
"#;
        let cfg: Config = toml::from_str(toml_src).expect("parse");
        let entry = cfg.providers.get("bedrock_anthropic").expect("entry");
        match entry {
            ProviderEntry::Bedrock {
                region,
                model_id,
                api_shape,
                user_agent,
                anthropic_beta,
                extra_headers,
                creds,
                ..
            } => {
                assert_eq!(region, "us-west-2");
                assert_eq!(model_id, "us.anthropic.claude-opus-4-7");
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
                    extra_headers.get("x-trace-id").map(String::as_str),
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
type = "bedrock"
region = "us-west-2"
model_id = "anthropic.claude-haiku-4-5"

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
type = "bedrock"
region = "us-west-2"
model_id = "us.anthropic.claude-opus-4-7"
api_shape = "converse"
creds = { kind = "profile", name = "isengard-cecelia" }
"#;
        let cfg: Config = toml::from_str(toml_src).expect("parse");
        match cfg.providers.get("bedrock_profile").unwrap() {
            ProviderEntry::Bedrock {
                creds, api_shape, ..
            } => {
                assert_eq!(*api_shape, BedrockApiShapeConfig::Converse);
                match creds {
                    BedrockCredsConfig::Profile { name } => {
                        assert_eq!(name, "isengard-cecelia");
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
type = "bedrock"
region = "us-west-2"
model_id = "us.anthropic.claude-opus-4-7"
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
