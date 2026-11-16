//! Provider factory tests with the default in-process SecretStore.

use routectl_auth::{MemoryStore, SecretRef};
use routectl_core::Error;
use routectl_router::{Auth, Format, ProviderEntry, ReasoningDialect, build_provider};

mod common;

#[tokio::test]
async fn build_openai_compat_resolves_secret() {
    let store: std::sync::Arc<dyn routectl_auth::SecretStore> = std::sync::Arc::new(MemoryStore);
    let entry = ProviderEntry::openai_compat("https://example.com/v1", common::file_ref("sk-abc"));
    let provider = build_provider("test", &entry, store.clone())
        .await
        .expect("build");
    assert_eq!(provider.id(), "openai-compat:test");
}

#[tokio::test]
async fn build_anthropic_api_resolves_secret() {
    let store: std::sync::Arc<dyn routectl_auth::SecretStore> = std::sync::Arc::new(MemoryStore);
    let entry = ProviderEntry::anthropic_api(common::file_ref("sk-ant-abc"));
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
format = "anthropic-messages"
base_url = "https://api2.anthropic.com"
credential = "env://ANTHROPIC_API_KEY"
auth = "x-api-key"
anthropic_version = "2024-05-01"
"#;
    let cfg: routectl_router::Config = toml::from_str(toml_src).expect("parse");
    let entry = cfg.providers.get("anthropic").expect("anthropic entry");
    assert_eq!(entry.format, Format::AnthropicMessages);
    assert_eq!(
        entry.base_url.as_deref(),
        Some("https://api2.anthropic.com")
    );
    assert_eq!(entry.anthropic_version.as_deref(), Some("2024-05-01"));
    assert_eq!(entry.credential, "env://ANTHROPIC_API_KEY");
    assert_eq!(entry.auth, Auth::XApiKey);
}

/// `auth = "bearer"` and `auth = "x-api-key"` each parse to their own
/// [`Auth`] value and survive a serialize / re-parse. Locks in the
/// kebab-case TOML surface.
#[test]
fn anthropic_auth_round_trips_through_toml() {
    let toml_src = r#"
[providers.claude-code]
format = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = "file://<local-path>"
auth = "bearer"

[providers.anthropic-default]
format = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = "env://ANTHROPIC_API_KEY"
auth = "x-api-key"
"#;
    let cfg: routectl_router::Config = toml::from_str(toml_src).expect("parse");

    let claude_code = cfg.providers.get("claude-code").expect("claude-code entry");
    assert_eq!(claude_code.auth, Auth::Bearer);
    let api_key = cfg
        .providers
        .get("anthropic-default")
        .expect("anthropic-default entry");
    assert_eq!(api_key.auth, Auth::XApiKey);

    // Round-trip the other way: serialize and re-parse must preserve the value.
    let reserialized = toml::to_string(&cfg).expect("re-serialize");
    let cfg2: routectl_router::Config = toml::from_str(&reserialized).expect("re-parse");
    assert_eq!(
        cfg2.providers.get("claude-code").unwrap().auth,
        Auth::Bearer
    );
    assert_eq!(
        cfg2.providers.get("anthropic-default").unwrap().auth,
        Auth::XApiKey
    );
}

/// Same test but for the OpenAI-compat shape: a custom `base_url`
/// must survive TOML round-trip. Lots of providers (OpenCode-Go,
/// NIM, llama.cpp) rely on this. v0.6.0 moved `reasoning_dialect` to
/// `[models.X]`; the provider entry no longer carries it.
#[test]
fn openai_compat_custom_base_url_round_trips_through_toml() {
    let toml_src = r#"
[providers.example-deepseek-host]
format = "openai-chat"
base_url = "https://opencode.ai/zen/go/v1"
credential = "env://OPENCODE_GO_API_KEY"
auth = "bearer"

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
    assert_eq!(entry.format, Format::OpenaiChat);
    assert_eq!(
        entry.base_url.as_deref(),
        Some("https://opencode.ai/zen/go/v1")
    );
    let model = cfg.models.get("dsv4").expect("model");
    assert_eq!(model.reasoning_dialect, Some(ReasoningDialect::Deepseek));
}

#[tokio::test]
async fn build_with_missing_env_var_errors() {
    // TODO: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::remove_var("ROUTECTL_TEST_MISSING_KEY") };
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
format = "anthropic-messages"
base_url = "https://api.anthropic.com"
credential = "env://ANTHROPIC_API_KEY"
auth = "x-api-key"
user_agent = "claude-code/1.2.3"

[providers.anthropic.header_extras]
"anthropic-beta" = "context-1m-2025-08-07,prompt-caching-2024-07-31"
"x-custom-trace" = "abc123"
"#;
    let cfg: routectl_router::Config = toml::from_str(toml_src).expect("parse");
    let entry = cfg.providers.get("anthropic").expect("anthropic entry");
    assert_eq!(entry.format, Format::AnthropicMessages);
    assert_eq!(entry.user_agent.as_deref(), Some("claude-code/1.2.3"));
    assert_eq!(
        entry
            .header_extras
            .get("anthropic-beta")
            .map(String::as_str),
        Some("context-1m-2025-08-07,prompt-caching-2024-07-31"),
    );
    assert_eq!(
        entry
            .header_extras
            .get("x-custom-trace")
            .map(String::as_str),
        Some("abc123")
    );
}

/// Pin: `build_resolved_models` threads the originating `oauth://`
/// SecretRef onto each ResolvedModel so the 401 self-heal hook can
/// dispatch back through the OAuth store. MemoryStore parses the
/// URI fine; we never call `.token()` here, just inspect the
/// retained `auth_secret_ref`.
#[tokio::test]
async fn resolved_model_retains_oauth_secret_ref_for_anthropic() {
    use routectl_router::{BuildOptions, Config, ModelEntry, build_resolved_models};
    use std::collections::BTreeMap;

    let store: std::sync::Arc<dyn routectl_auth::SecretStore> = std::sync::Arc::new(MemoryStore);
    let mut providers: BTreeMap<String, ProviderEntry> = BTreeMap::new();
    providers.insert(
        "anthropic".into(),
        ProviderEntry::anthropic_api("oauth://anthropic").with_auth(Auth::Bearer),
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
            provider: "anthropic".into(),
            label: None,
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
    use routectl_router::{BuildOptions, Config, ModelEntry, build_resolved_models};
    use std::collections::BTreeMap;

    // Set the env var so MemoryStore's env:// arm can resolve at
    // build time. The retained `auth_secret_ref` is parsed from the
    // URI string regardless.
    // TODO: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::set_var("ROUTECTL_TEST_OPENAI_COMPAT_KEY", "sk-test-value") };

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
    // TODO: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::remove_var("ROUTECTL_TEST_OPENAI_COMPAT_KEY") };

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
    use routectl_router::{Config, build_provider};

    #[tokio::test]
    async fn factory_builds_openai_responses_chatgpt_oauth_provider() {
        // Arrange
        let api_key_ref = crate::common::file_ref("test-jwt");
        let account_id_ref = crate::common::file_ref("acct-uuid");
        let toml_src = format!(
            r#"
[providers.gpt]
format = "openai-responses"
base_url = "https://chatgpt.com/backend-api/codex"
credential = "{api_key_ref}"
auth = "bearer"
account_id_ref = "{account_id_ref}"
"#
        );
        let cfg: Config = toml::from_str(&toml_src).expect("parse");
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
format = "openai-responses"
base_url = "https://chatgpt.com/backend-api/codex"
credential = "env://ROUTECTL_FACTORY_TEST_UNSET_JWT"
auth = "bearer"
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
                assert!(msg.contains("ChatGPT Codex host"), "msg: {msg}");
                assert!(msg.contains("account_id_ref"), "msg: {msg}");
            }
            Ok(_) => panic!("expected Err, got Ok"),
            Err(other) => panic!("expected Error::Config, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn factory_builds_openai_responses_api_key_provider() {
        // Arrange: api-key surface, no account_id_ref, default base_url.
        let api_key_ref = crate::common::file_ref("sk-test-123");
        let toml_src = format!(
            r#"
[providers.gpt-api]
format = "openai-responses"
base_url = "https://api.openai.com/v1"
credential = "{api_key_ref}"
auth = "bearer"
"#
        );
        let cfg: Config = toml::from_str(&toml_src).expect("parse");
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
format = "openai-responses"
base_url = "https://api.openai.com/v1"
credential = "env://ROUTECTL_FACTORY_TEST_UNSET_KEY"
auth = "bearer"
account_id_ref = "env://ROUTECTL_FACTORY_TEST_UNSET_ACCT"
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
                assert!(msg.contains("ChatGPT Codex host"), "msg: {msg}");
            }
            Ok(_) => panic!("expected Err, got Ok"),
            Err(other) => panic!("expected Error::Config, got {other:?}"),
        }
    }

    /// The Responses dialect follows the destination: the Codex host speaks
    /// the ChatGPT dialect, the OpenAI API host does not.
    #[test]
    fn openai_responses_dialect_follows_the_codex_host() {
        // Arrange
        let toml_src = r#"
[providers.gpt]
format = "openai-responses"
base_url = "https://chatgpt.com/backend-api/codex"
credential = "env://ROUTECTL_FACTORY_TEST_UNSET_JWT"
auth = "bearer"
account_id_ref = "env://ROUTECTL_FACTORY_TEST_UNSET_ACCT"

[providers.gpt-api]
format = "openai-responses"
base_url = "https://api.openai.com/v1"
credential = "env://ROUTECTL_FACTORY_TEST_UNSET_KEY"
auth = "bearer"
"#;
        let cfg: Config = toml::from_str(toml_src).expect("parse");

        // Assert
        assert!(cfg.providers["gpt"].is_chatgpt_oauth_responses());
        assert!(!cfg.providers["gpt-api"].is_chatgpt_oauth_responses());
    }
}

#[cfg(feature = "bedrock")]
mod bedrock_tests {
    use routectl_router::{Auth, Cloud, Config, Format, ProviderEntry};

    #[test]
    fn bedrock_invoke_with_bearer_key_round_trips() {
        let toml_src = r#"
[providers.bedrock_anthropic]
format = "bedrock-invoke"
cloud = { name = "aws", region = "us-west-2" }
credential = "file://<local-path>"
auth = "bearer"
user_agent = "claude-code/1.2.3"

[providers.bedrock_anthropic.header_extras]
"x-trace-id" = "abc"
"anthropic-beta" = "context-1m-2025-08-07,prompt-caching-2024-07-31"
"#;
        let cfg: Config = toml::from_str(toml_src).expect("parse");
        let entry = cfg.providers.get("bedrock_anthropic").expect("entry");
        assert_eq!(entry.format, Format::BedrockInvoke);
        assert_eq!(
            entry.cloud,
            Some(Cloud::Aws {
                region: "us-west-2".into()
            })
        );
        assert_eq!(entry.base_url, None);
        assert_eq!(entry.user_agent.as_deref(), Some("claude-code/1.2.3"));
        assert_eq!(
            entry.header_extras.get("x-trace-id").map(String::as_str),
            Some("abc")
        );
        assert_eq!(
            entry
                .header_extras
                .get("anthropic-beta")
                .map(String::as_str),
            Some("context-1m-2025-08-07,prompt-caching-2024-07-31")
        );
        assert_eq!(entry.credential, "file://<local-path>");
        assert_eq!(entry.auth, Auth::Bearer);
    }

    /// Static AWS keys are not a config form: an entry that still carries
    /// the static-keys `creds` table is refused, while the same entry on
    /// the flat keys parses.
    #[test]
    fn bedrock_static_creds_table_is_refused() {
        let flat = r#"
[providers.bedrock_static]
format = "bedrock-invoke"
cloud = { name = "aws", region = "us-west-2" }
credential = "aws://chain"
auth = "aws-sigv4"
"#;
        let static_keys = format!(
            "{flat}\n[providers.bedrock_static.creds]\nkind = \"static\"\n\
             access_key_ref = \"env://AWS_ACCESS_KEY_ID\"\n\
             secret_key_ref = \"env://AWS_SECRET_ACCESS_KEY\"\n\
             session_token_ref = \"env://AWS_SESSION_TOKEN\"\n"
        );

        let accepted: Config = toml::from_str(flat).expect("the flat entry parses");
        let refused = toml::from_str::<Config>(&static_keys);

        assert_eq!(
            accepted.providers["bedrock_static"].credential,
            "aws://chain"
        );
        let err = refused
            .expect_err("a creds table is not an entry key")
            .to_string();
        assert!(err.contains("creds"), "names the refused key: {err}");
    }

    #[test]
    fn bedrock_profile_creds_round_trip() {
        let toml_src = r#"
[providers.bedrock_profile]
format = "bedrock-converse"
cloud = { name = "aws", region = "us-west-2" }
credential = "aws://profile/bedrock-prod"
auth = "aws-sigv4"
"#;
        let cfg: Config = toml::from_str(toml_src).expect("parse");
        let entry = cfg.providers.get("bedrock_profile").unwrap();
        assert_eq!(entry.format, Format::BedrockConverse);
        assert_eq!(entry.credential, "aws://profile/bedrock-prod");
        assert_eq!(entry.auth, Auth::AwsSigv4);
    }

    #[test]
    fn bedrock_default_chain_round_trips() {
        let toml_src = r#"
[providers.bedrock_chain]
format = "bedrock-invoke"
cloud = { name = "aws", region = "us-west-2" }
credential = "aws://chain"
auth = "aws-sigv4"
"#;
        let cfg: Config = toml::from_str(toml_src).expect("parse");
        let entry = cfg.providers.get("bedrock_chain").unwrap();
        assert_eq!(entry.format, Format::BedrockInvoke);
        assert_eq!(entry.credential, "aws://chain");
    }

    #[test]
    fn bedrock_redact_secrets_clears_literals_only() {
        let entry = |credential: &str| {
            ProviderEntry::cloud_aws(Format::BedrockInvoke, "us-west-2", credential, Auth::Bearer)
        };
        let mut literal = entry("literal:testkey-actual-xyz");
        let mut pointer = entry("env://AWS_BEARER_TOKEN_BEDROCK");
        let mut chain = ProviderEntry::cloud_aws(
            Format::BedrockInvoke,
            "us-west-2",
            "aws://chain",
            Auth::AwsSigv4,
        );

        literal.redact_secrets();
        pointer.redact_secrets();
        chain.redact_secrets();

        assert_eq!(literal.credential, "literal:[REDACTED]");
        assert_eq!(pointer.credential, "env://AWS_BEARER_TOKEN_BEDROCK");
        assert_eq!(chain.credential, "aws://chain");
    }
}
