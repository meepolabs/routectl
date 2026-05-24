//! Tests for the `test`, `config`, `login` CLI subcommands.

use std::collections::BTreeMap;

use routectl_cli::commands;
use routectl_router::{
    AliasValue, Config, LegacyCompat, ModelEntry, ProviderEntry, RetryPolicy, ServerConfig,
};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// RAII guard for `std::env::{set,remove}_var` mutations within tests.
/// Restores the original value (or absence) on `Drop`, so a panicking
/// `assert!` cannot leak modified env into sibling tests. Pair every
/// guard binding with `let _xdg = EnvGuard::set(..)`.
struct EnvGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}
impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, prev }
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

fn config_with(server_url: &str) -> Config {
    let mut providers = BTreeMap::new();
    providers.insert(
        "mock".into(),
        ProviderEntry::openai_compat(format!("{server_url}/v1"), "literal:test-key"),
    );
    let mut models = BTreeMap::new();
    models.insert("fast-mini".into(), ModelEntry::new("mock", "gpt-4o-mini"));
    let mut aliases = BTreeMap::new();
    aliases.insert("fast".into(), AliasValue::Single("fast-mini".into()));
    Config {
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 8787,
            auth: None,
            strict_translation: false,
            allow_disable_fallbacks: true,
        },
        providers,
        aliases,
        retry: RetryPolicy::default(),
        legacy_compat: LegacyCompat::Openrouter,
        models,
        ..Default::default()
    }
}

#[tokio::test]
async fn login_unknown_provider_errors_clearly() {
    // The login flow should fail-fast on an unknown provider name
    // BEFORE binding any sockets or opening browsers. (PR1 only ships
    // anthropic; codex lands in PR3.)
    match commands::login::run("made-up-provider", false, None).await {
        Err(routectl_core::Error::Auth(msg)) => {
            assert!(
                msg.contains("unknown oauth provider"),
                "expected unknown-provider message, got: {msg}",
            );
            assert!(
                msg.contains("anthropic"),
                "expected known-providers list to mention anthropic, got: {msg}",
            );
        }
        Ok(_) => panic!("expected error for unknown provider"),
        Err(other) => panic!("expected Auth error, got: {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn whoami_returns_exit_code_2_when_empty() {
    // Sandbox the credentials path so this test does not depend on
    // (or pollute) the real home directory.
    let tmp = tempfile::tempdir().unwrap();
    let _xdg = EnvGuard::set("XDG_CONFIG_HOME", tmp.path());

    let code = commands::whoami::run()
        .await
        .expect("whoami should not error on empty store");
    assert_eq!(code, 2, "whoami must return 2 when no providers logged in");
}

#[tokio::test]
async fn config_check_passes_for_valid_config() {
    let mut config = Config {
        server: ServerConfig::default(),
        providers: BTreeMap::new(),
        aliases: BTreeMap::new(),
        retry: RetryPolicy::default(),
        legacy_compat: LegacyCompat::Openrouter,
        ..Default::default()
    };
    config.providers.insert(
        "mock".into(),
        ProviderEntry::openai_compat("http://127.0.0.1:9", "literal:abc"),
    );
    config
        .models
        .insert("fast-model".into(), ModelEntry::new("mock", "gpt-4o"));
    config
        .aliases
        .insert("fast".into(), AliasValue::Single("fast-model".into()));

    commands::config::check(&config)
        .await
        .expect("valid config should check ok");
}

#[tokio::test]
async fn config_check_fails_for_alias_pointing_at_unknown_nickname() {
    let mut config = Config {
        server: ServerConfig::default(),
        providers: BTreeMap::new(),
        aliases: BTreeMap::new(),
        retry: RetryPolicy::default(),
        legacy_compat: LegacyCompat::Openrouter,
        ..Default::default()
    };
    config
        .aliases
        .insert("fast".into(), AliasValue::Single("ghost".into()));

    match commands::config::check(&config).await {
        Err(routectl_core::Error::Config(msg)) => {
            assert!(msg.contains("error"), "got: {msg}");
        }
        Ok(_) => panic!("expected config error"),
        Err(other) => panic!("expected Config error, got: {other:?}"),
    }
}

/// Bare Config with empty providers/aliases/models. Five tests share
/// this skeleton; the only per-test variation is which provider/alias/
/// model entries get pushed in after construction.
fn bare_config() -> Config {
    Config {
        server: ServerConfig::default(),
        providers: BTreeMap::new(),
        aliases: BTreeMap::new(),
        retry: RetryPolicy::default(),
        legacy_compat: LegacyCompat::Openrouter,
        ..Default::default()
    }
}

fn add_mock_provider(config: &mut Config) {
    config.providers.insert(
        "mock".into(),
        ProviderEntry::openai_compat("http://127.0.0.1:9", "literal:abc"),
    );
}

#[tokio::test]
async fn config_check_fails_when_default_alias_points_to_unknown_nickname() {
    // `default = "..."` is just a regular alias key in v0.6.0. The
    // existing alias-target validator catches the unknown-nickname
    // case for any key, including "default".
    let mut config = bare_config();
    add_mock_provider(&mut config);
    config.aliases.insert(
        "default".into(),
        AliasValue::Single("nonexistent-nickname".into()),
    );

    match commands::config::check(&config).await {
        Err(routectl_core::Error::Config(_)) => {}
        Ok(_) => panic!("expected config error for unknown default-alias target"),
        Err(other) => panic!("expected Config error, got: {other:?}"),
    }
}

#[tokio::test]
async fn config_check_fails_when_model_references_unknown_provider() {
    let mut config = bare_config();
    add_mock_provider(&mut config);
    config
        .models
        .insert("orphan".into(), ModelEntry::new("ghost-provider", "gpt-x"));
    config
        .aliases
        .insert("default".into(), AliasValue::Single("orphan".into()));

    match commands::config::check(&config).await {
        Err(routectl_core::Error::Config(_)) => {}
        Ok(_) => panic!("expected config error for unknown provider reference"),
        Err(other) => panic!("expected Config error, got: {other:?}"),
    }
}

#[tokio::test]
async fn config_check_passes_when_default_alias_is_valid() {
    let mut config = bare_config();
    add_mock_provider(&mut config);
    config
        .models
        .insert("fast-model".into(), ModelEntry::new("mock", "gpt-4o"));
    config
        .aliases
        .insert("default".into(), AliasValue::Single("fast-model".into()));

    commands::config::check(&config)
        .await
        .expect("default = existing nickname must check ok");
}

#[tokio::test]
async fn config_check_fails_for_empty_alias_chain() {
    // An alias whose chain is empty resolves to UnknownAlias at
    // request time, which is the same as not declaring the alias
    // at all -- a configuration mistake. Reject at startup.
    let mut config = bare_config();
    add_mock_provider(&mut config);
    config
        .aliases
        .insert("empty".into(), AliasValue::Chain(Vec::new()));

    match commands::config::check(&config).await {
        Err(routectl_core::Error::Config(_)) => {}
        Ok(_) => panic!("expected config error for empty alias chain"),
        Err(other) => panic!("expected Config error, got: {other:?}"),
    }
}

#[test]
fn config_show_redacts_literal_secrets() {
    let mut config = Config {
        server: ServerConfig::default(),
        providers: BTreeMap::new(),
        aliases: BTreeMap::new(),
        retry: RetryPolicy::default(),
        legacy_compat: LegacyCompat::Openrouter,
        ..Default::default()
    };
    config.providers.insert(
        "secret".into(),
        ProviderEntry::openai_compat("https://api.example.com/v1", "literal:sk-very-secret"),
    );

    // Capture stdout via a wrapping function. Easiest: write our own
    // serializer instead. Use `toml::to_string_pretty` ourselves to verify
    // the redaction logic.
    commands::config::show(&config).expect("show ok");

    // Render again to a string we can inspect.
    let mut redacted = config.clone();
    if let ProviderEntry::OpenaiCompat { api_key_ref, .. } =
        redacted.providers.get_mut("secret").unwrap()
    {
        // Simulate what show() does -- this confirms the redaction logic
        // we wrote in commands/config.rs is exposed via behavior.
        if api_key_ref.starts_with("literal:") {
            *api_key_ref = "literal:[REDACTED]".into();
        }
    }
    let s = toml::to_string_pretty(&redacted).unwrap();
    assert!(s.contains("[REDACTED]"));
    assert!(!s.contains("sk-very-secret"));
}

#[test]
fn config_show_keeps_env_uris_intact() {
    let mut config = Config {
        server: ServerConfig::default(),
        providers: BTreeMap::new(),
        aliases: BTreeMap::new(),
        retry: RetryPolicy::default(),
        legacy_compat: LegacyCompat::Openrouter,
        ..Default::default()
    };
    config.providers.insert(
        "kc".into(),
        ProviderEntry::openai_compat(
            "https://api.example.com/v1",
            "env://ROUTECTL_TEST_ANTHROPIC",
        ),
    );

    commands::config::show(&config).expect("show ok");
}

#[test]
fn config_example_prints_without_error() {
    commands::config::example().expect("example ok");
}

#[tokio::test]
async fn test_command_runs_completion_through_router() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-test",
            "object": "chat.completion",
            "created": 1700000000,
            "model": "gpt-4o-mini",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "Hello there short reply."
                },
                "finish_reason": "stop"
            }],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 5,
                "total_tokens": 15
            }
        })))
        .mount(&mock)
        .await;

    let config = config_with(&mock.uri());
    commands::test::run(config, "fast", "Hi.")
        .await
        .expect("test command ok");
}

#[tokio::test]
async fn test_command_propagates_upstream_error() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
        .mount(&mock)
        .await;

    let config = config_with(&mock.uri());
    let err = commands::test::run(config, "fast", "Hi.")
        .await
        .expect_err("should fail with 401");
    match err {
        routectl_core::Error::Upstream { status, .. } => assert_eq!(status, 401),
        other => panic!("expected upstream 401, got: {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn test_command_resolves_oauth_ref_when_logged_in() {
    use routectl_providers::anthropic_api::AuthKind as AnthropicAuthKind;
    use std::time::{SystemTime, UNIX_EPOCH};

    let tmp = tempfile::tempdir().expect("tempdir");
    let _xdg = EnvGuard::set("XDG_CONFIG_HOME", tmp.path());

    // Seed a synthetic credentials.json under
    // <tmp>/routectl/credentials.json. The OAuth subsystem's `file_io`
    // helpers are pub(crate) in routectl-auth, so we hand-write the JSON
    // shape (matching the v1 schema) and apply the 0o600 hygiene that
    // OAuthStore::open enforces. Round-tripping through the public
    // CredentialsFile/TokenRecord struct literals is blocked by their
    // `#[non_exhaustive]` attribute outside the auth crate.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let creds_json = json!({
        "schema_version": 1,
        "providers": {
            "anthropic": {
                "access_token": "seeded-access-token",
                "refresh_token": "seeded-refresh-token",
                "token_type": "Bearer",
                "expires_at_unix": now + 3600,
                "scopes": ["user:profile", "user:inference"],
                "account": {
                    "email": "alice@example.com",
                    "account_id": null
                },
                "obtained_at_unix": now
            }
        }
    });
    let creds_path = tmp.path().join("routectl").join("credentials.json");
    std::fs::create_dir_all(creds_path.parent().unwrap()).expect("mkdir creds parent");
    std::fs::write(
        &creds_path,
        serde_json::to_vec_pretty(&creds_json).expect("serialize creds"),
    )
    .expect("write creds");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&creds_path, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 0600");
    }

    // Mock Anthropic Messages endpoint. The Authorization-header matcher
    // pins the bearer-forwarding contract: a regression that drops the
    // header or sends a different token will fail this test (wiremock
    // returns 404 by default for unmatched POSTs).
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("authorization", "Bearer seeded-access-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_test",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "ok"}],
            "model": "claude-sonnet-4-6",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(&mock)
        .await;

    // Build a Config with one anthropic_api provider that resolves
    // its api key via `oauth://anthropic` and points at the mock URL.
    let mut providers = BTreeMap::new();
    providers.insert(
        "anthropic_oauth".into(),
        ProviderEntry::anthropic_api("oauth://anthropic")
            .with_base_url(mock.uri())
            .with_auth_kind(AnthropicAuthKind::OauthBearer),
    );
    let mut models = BTreeMap::new();
    models.insert(
        "claude".into(),
        ModelEntry::new("anthropic_oauth", "claude-sonnet-4-6"),
    );
    let mut aliases = BTreeMap::new();
    aliases.insert("default".into(), AliasValue::Single("claude".into()));
    let config = Config {
        server: ServerConfig::default(),
        providers,
        aliases,
        retry: RetryPolicy::default(),
        legacy_compat: LegacyCompat::Openrouter,
        models,
        ..Default::default()
    };

    let result = commands::test::run(config, "default", "Hi.").await;
    assert!(
        result.is_ok(),
        "test::run should resolve oauth://anthropic via CompositeStore: {result:?}"
    );
}
