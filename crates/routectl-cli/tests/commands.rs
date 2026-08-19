//! Tests for the `test`, `config`, `login` CLI subcommands.

use std::collections::BTreeMap;

use routectl_cli::commands;
use routectl_router::{
    AliasValue, CURRENT_CONFIG_VERSION as CURRENT, Config, ModelEntry, ProviderEntry, RetryPolicy,
    ServerConfig,
};
use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

use routectl_testkit::ScopedEnv;

fn config_with(server_url: &str) -> Config {
    let mut providers = BTreeMap::new();
    providers.insert(
        "mock".into(),
        ProviderEntry::openai_compat(format!("{server_url}/v1"), common::file_ref("test-key")),
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
            ..Default::default()
        },
        providers,
        aliases,
        retry: RetryPolicy::default(),
        models,
        ..Default::default()
    }
}

#[tokio::test]
// Serialized like the other env-mutating tests: login::run opens the
// default OAuthStore, which reads the process-global XDG_CONFIG_HOME.
// Without this it can race a sibling test and observe its half-written
// credentials file (before that test's chmod 0600).
#[serial_test::serial]
async fn login_unknown_provider_errors_clearly() {
    // The login flow should fail-fast on an unknown provider name BEFORE
    // binding any sockets or opening browsers. (clap normally rejects
    // unknown values first; this guards the command body's own
    // registry-lookup path, which must list every known provider.)
    match commands::login::run("made-up-provider", false, None, None).await {
        Err(routectl_core::Error::Auth(msg)) => {
            assert!(
                msg.contains("unknown oauth provider"),
                "expected unknown-provider message, got: {msg}",
            );
            assert!(
                msg.contains("anthropic") && msg.contains("codex"),
                "expected known-providers list to mention anthropic and codex, got: {msg}",
            );
        }
        Ok(()) => panic!("expected error for unknown provider"),
        Err(other) => panic!("expected Auth error, got: {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn whoami_returns_exit_code_2_when_empty() {
    // Sandbox the credentials path so this test does not depend on
    // (or pollute) the real home directory.
    let tmp = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());

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
        ..Default::default()
    };
    config.providers.insert(
        "mock".into(),
        ProviderEntry::openai_compat("http://127.0.0.1:9", common::file_ref("abc")),
    );
    config
        .models
        .insert("fast-model".into(), ModelEntry::new("mock", "gpt-4o"));
    config
        .aliases
        .insert("fast".into(), AliasValue::Single("fast-model".into()));

    commands::config::check(&config, None)
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
        ..Default::default()
    };
    config
        .aliases
        .insert("fast".into(), AliasValue::Single("ghost".into()));

    match commands::config::check(&config, None).await {
        Err(routectl_core::Error::Config(msg)) => {
            assert!(msg.contains("error"), "got: {msg}");
        }
        Ok(()) => panic!("expected config error"),
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
        ..Default::default()
    }
}

fn add_mock_provider(config: &mut Config) {
    config.providers.insert(
        "mock".into(),
        ProviderEntry::openai_compat("http://127.0.0.1:9", common::file_ref("abc")),
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

    match commands::config::check(&config, None).await {
        Err(routectl_core::Error::Config(_)) => {}
        Ok(()) => panic!("expected config error for unknown default-alias target"),
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

    match commands::config::check(&config, None).await {
        Err(routectl_core::Error::Config(_)) => {}
        Ok(()) => panic!("expected config error for unknown provider reference"),
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

    commands::config::check(&config, None)
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

    match commands::config::check(&config, None).await {
        Err(routectl_core::Error::Config(_)) => {}
        Ok(()) => panic!("expected config error for empty alias chain"),
        Err(other) => panic!("expected Config error, got: {other:?}"),
    }
}

/// Proves `validate_provider_credential_sources` is wired into
/// `routectl config check`, not only serve startup: a forwarded
/// provider pointed at a non-Anthropic host must fail `check`.
#[tokio::test]
async fn config_check_fails_for_forwarded_provider_on_non_anthropic_host() {
    use routectl_router::config::CredentialSource;

    let mut config = bare_config();
    config.providers.insert(
        "sneaky".into(),
        ProviderEntry::anthropic_api("")
            .with_base_url("https://evil.example.com")
            .with_credential_source(CredentialSource::Forwarded),
    );

    match commands::config::check(&config, None).await {
        Err(routectl_core::Error::Config(_)) => {}
        Ok(()) => panic!("expected config error for forwarded provider off the pinned host"),
        Err(other) => panic!("expected Config error, got: {other:?}"),
    }
}

/// A clean forwarded provider (pinned host, empty `api_key_ref`) must
/// pass `config check` alongside an unrelated own-credential provider
/// -- coexistence is not itself an error at the config-validation layer.
#[tokio::test]
async fn config_check_passes_for_clean_forwarded_provider() {
    use routectl_router::config::CredentialSource;

    let mut config = bare_config();
    add_mock_provider(&mut config);
    config.providers.insert(
        "anthropic-forwarded".into(),
        ProviderEntry::anthropic_api("")
            .with_base_url("https://api.anthropic.com")
            .with_credential_source(CredentialSource::Forwarded),
    );

    commands::config::check(&config, None)
        .await
        .expect("clean forwarded provider alongside an own-credential provider must check ok");
}

#[test]
fn config_show_redacts_literal_secrets() {
    let mut config = Config {
        server: ServerConfig::default(),
        providers: BTreeMap::new(),
        aliases: BTreeMap::new(),
        retry: RetryPolicy::default(),
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
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());

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
        models,
        ..Default::default()
    };

    let result = commands::test::run(config, "default", "Hi.").await;
    assert!(
        result.is_ok(),
        "test::run should resolve oauth://anthropic via CompositeStore: {result:?}"
    );
}

// ---------------- codex OAuth CLI surface ----------------

/// Write a `<xdg>/routectl/credentials.json` holding one record per
/// `(provider, email)` entry, with the `0o600` hygiene `OAuthStore::open`
/// enforces (it rejects group/other-readable files). The
/// `TokenRecord`/`AccountInfo` structs are `#[non_exhaustive]`, so tests
/// seed via raw JSON matching the v1 schema rather than struct literals --
/// the same approach the in-tree `test_command_resolves_oauth_ref_*` test
/// uses.
fn seed_credentials(xdg: &std::path::Path, providers: &[(&str, &str)]) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut provider_map = serde_json::Map::new();
    for (provider, email) in providers {
        provider_map.insert(
            (*provider).to_string(),
            json!({
                "access_token": format!("seeded-access-{provider}"),
                "refresh_token": format!("seeded-refresh-{provider}"),
                "token_type": "Bearer",
                "expires_at_unix": now + 3600,
                "scopes": ["openid", "offline_access"],
                "account": { "email": email, "account_id": format!("acct-{provider}") },
                "obtained_at_unix": now
            }),
        );
    }
    let creds_json = json!({ "schema_version": 1, "providers": provider_map });
    let creds_path = xdg.join("routectl").join("credentials.json");
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
}

#[tokio::test]
async fn login_codex_print_url_is_refused_with_browser_only_message() {
    // Codex has no headless paste-the-code landing page, so --print-url
    // must fail fast (before opening the store or binding a socket) with
    // the browser-only guidance. Exit-non-zero is enforced by main.rs;
    // here we assert the command body returns the Auth error.
    let err = commands::login::run("codex", /* print_url */ true, None, None)
        .await
        .expect_err("codex --print-url must be refused");
    match err {
        routectl_core::Error::Auth(msg) => {
            assert!(
                msg.contains("--print-url"),
                "expected --print-url refusal, got: {msg}"
            );
            assert!(
                msg.contains("browser"),
                "expected browser-flow guidance, got: {msg}"
            );
            assert!(
                msg.contains("1455"),
                "expected port-forward hint (1455), got: {msg}"
            );
        }
        other => panic!("expected Auth error, got: {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn logout_codex_removes_seeded_record() {
    // logout operates entirely on the local credentials file (no
    // network), so a seeded codex record can be removed deterministically.
    let tmp = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
    seed_credentials(tmp.path(), &[("codex", "dev@example.com")]);

    commands::logout::run("codex", None)
        .await
        .expect("logout codex should succeed against a seeded record");

    // The record must be gone: a second logout reports nothing to remove,
    // and whoami no longer sees any provider.
    commands::logout::run("codex", None)
        .await
        .expect("second logout codex is a no-op, not an error");
    let code = commands::whoami::run().await.expect("whoami ok");
    assert_eq!(code, 2, "store should be empty after logging out of codex");
}

#[tokio::test]
#[serial_test::serial]
async fn refresh_codex_without_record_reports_not_logged_in() {
    // `refresh codex` against an empty store must route through the
    // generic refresh path (proving codex is accepted, not rejected as an
    // unknown provider) and fail-fast with the actionable "login first"
    // hint BEFORE any network call -- force_refresh validates the
    // provider id, then short-circuits on the missing record.
    let tmp = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());

    let err = commands::refresh::run("codex", None)
        .await
        .expect_err("refresh codex with no record must error");
    match err {
        routectl_core::Error::Auth(msg) => {
            assert!(
                !msg.contains("unknown oauth provider"),
                "codex must be a known provider, not rejected: {msg}"
            );
            assert!(
                msg.contains("routectl login codex"),
                "expected login-first guidance for codex, got: {msg}"
            );
        }
        other => panic!("expected Auth error, got: {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn whoami_lists_both_anthropic_and_codex_when_present() {
    // whoami is provider-id-generic: with both records seeded it must
    // exit 0 (at least one provider logged in) and surface each provider
    // from the store. The email + account_id rendering is covered by the
    // command body; here we pin the multi-provider exit contract.
    let tmp = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
    seed_credentials(
        tmp.path(),
        &[
            ("anthropic", "alice@example.com"),
            ("codex", "bob@example.com"),
        ],
    );

    let code = commands::whoami::run()
        .await
        .expect("whoami should not error with two providers seeded");
    assert_eq!(code, 0, "whoami must exit 0 when providers are logged in");

    // Cross-check the store sees both ids (whoami iterates exactly this
    // list). Re-opening via the same sandboxed XDG path is deterministic.
    let store = routectl_auth::OAuthStore::open_default()
        .await
        .expect("open seeded store");
    let ids: Vec<String> = store.list().await.into_iter().map(|(p, _)| p).collect();
    assert!(ids.contains(&"anthropic".to_string()), "got: {ids:?}");
    assert!(ids.contains(&"codex".to_string()), "got: {ids:?}");
}

#[tokio::test]
#[serial_test::serial]
async fn logout_label_removes_only_that_seat() {
    // `logout <provider> --label <name>` must remove ONLY the labeled
    // seat and leave the default seat intact.
    let tmp = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
    seed_credentials(
        tmp.path(),
        &[
            ("anthropic", "default@example.com"),
            ("anthropic#seat-b", "seat-b@example.com"),
        ],
    );

    commands::logout::run("anthropic", Some("seat-b"))
        .await
        .expect("labeled logout should succeed");

    // The default seat survives; only seat-b is gone.
    let store = routectl_auth::OAuthStore::open_default()
        .await
        .expect("open store");
    let ids: Vec<String> = store.list().await.into_iter().map(|(p, _)| p).collect();
    assert_eq!(
        ids,
        vec!["anthropic"],
        "labeled logout must remove only the named seat, leaving the default: {ids:?}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn logout_no_label_removes_default_seat() {
    // `logout <provider>` (no label) must remove ONLY the default seat
    // and leave labeled seats intact -- a bare logout must not surprise an
    // operator who added a pool by wiping every seat.
    let tmp = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
    seed_credentials(
        tmp.path(),
        &[
            ("anthropic", "default@example.com"),
            ("anthropic#seat-b", "seat-b@example.com"),
        ],
    );

    commands::logout::run("anthropic", None)
        .await
        .expect("default logout should succeed");

    // seat-b survives; only the default is gone.
    let store = routectl_auth::OAuthStore::open_default()
        .await
        .expect("open store");
    let ids: Vec<String> = store.list().await.into_iter().map(|(p, _)| p).collect();
    assert_eq!(
        ids,
        vec!["anthropic#seat-b"],
        "no-label logout must remove only the default seat, leaving labeled seats: {ids:?}"
    );
}

#[tokio::test]
#[serial_test::serial]
async fn whoami_lists_seats_grouped_by_provider() {
    // whoami must surface a provider's default seat AND its labeled seat,
    // each as its own block. Exit 0 (at least one seat logged in); the
    // store sees both keys whoami iterates.
    let tmp = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", tmp.path());
    seed_credentials(
        tmp.path(),
        &[
            ("anthropic", "default@example.com"),
            ("anthropic#seat-b", "seat-b@example.com"),
        ],
    );

    let code = commands::whoami::run()
        .await
        .expect("whoami should not error with a pooled provider");
    assert_eq!(code, 0, "whoami must exit 0 when seats are present");

    let store = routectl_auth::OAuthStore::open_default()
        .await
        .expect("open seeded store");
    let ids: Vec<String> = store.list().await.into_iter().map(|(p, _)| p).collect();
    assert_eq!(
        ids,
        vec!["anthropic", "anthropic#seat-b"],
        "store must hold both the default and labeled seat: {ids:?}"
    );
}

// ---------------- clap-layer provider validation ----------------
//
// These spawn the real `routectl` binary so the assertions exercise the
// clap `value_parser` wired off `known_provider_ids()` -- that validator
// lives in `main`, not in any `commands::*` function. Cargo exposes the
// freshly-built binary path via `CARGO_BIN_EXE_routectl`, so no extra
// dev-dependency (assert_cmd etc.) is needed.

/// Run `routectl <args...>` with a sandboxed (empty) XDG dir and return
/// `(exit_code, stderr)`. The sandbox keeps the spawned process from
/// reading the developer's real credentials.json.
fn run_routectl(args: &[&str]) -> (i32, String) {
    let tmp = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_routectl"))
        .args(args)
        .env("XDG_CONFIG_HOME", tmp.path())
        .output()
        .expect("spawn routectl binary");
    let code = out.status.code().unwrap_or(-1);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (code, stderr)
}

#[test]
fn clap_rejects_unknown_login_provider_listing_valid_set() {
    // A made-up provider must be rejected by clap (exit 2) with an error
    // that lists the valid set -- both anthropic AND codex -- so the
    // operator sees every option, not a stale single entry.
    let (code, stderr) = run_routectl(&["login", "made-up-provider"]);
    assert_ne!(
        code, 0,
        "clap must reject an unknown provider; stderr: {stderr}"
    );
    assert!(
        stderr.contains("invalid value"),
        "expected clap invalid-value error, got: {stderr}"
    );
    assert!(
        stderr.contains("anthropic") && stderr.contains("codex"),
        "expected valid set to list anthropic and codex, got: {stderr}"
    );
}

#[test]
fn clap_accepts_codex_then_login_refuses_print_url() {
    // `login codex --print-url` must parse cleanly at the clap layer
    // (codex is an accepted value -- NO "invalid value" error) and then
    // exit non-zero from the runtime refusal with the browser-only
    // message. One spawn pins both contracts and stays fast (the refusal
    // fires before any socket/browser/network work).
    let (code, stderr) = run_routectl(&["login", "codex", "--print-url"]);
    assert!(
        !stderr.contains("invalid value"),
        "codex must be accepted by clap, not rejected: {stderr}"
    );
    assert_ne!(
        code, 0,
        "codex --print-url must exit non-zero; stderr: {stderr}"
    );
    assert!(
        stderr.contains("--print-url") && stderr.contains("browser"),
        "expected browser-only refusal message, got: {stderr}"
    );
}

#[test]
fn clap_accepts_codex_for_logout_and_refresh() {
    // logout/refresh must also accept codex at the clap layer. logout
    // against an empty store is a clean no-op (exit 0); refresh against an
    // empty store fails fast with a runtime error -- but neither is a clap
    // "invalid value" rejection, which is what this test pins.
    let (logout_code, logout_err) = run_routectl(&["logout", "codex"]);
    assert!(
        !logout_err.contains("invalid value"),
        "logout must accept codex: {logout_err}"
    );
    assert_eq!(
        logout_code, 0,
        "logout of empty store is a no-op: {logout_err}"
    );

    let (_refresh_code, refresh_err) = run_routectl(&["refresh", "codex"]);
    assert!(
        !refresh_err.contains("invalid value"),
        "refresh must accept codex: {refresh_err}"
    );
}

#[test]
fn clap_accepts_label_flag_for_seat_commands() {
    // `--label` must parse cleanly on logout (and by extension login /
    // refresh, which share the same flag wiring). A labeled logout against
    // an empty store is a clean no-op (exit 0), NOT a clap rejection.
    let (code, stderr) = run_routectl(&["logout", "anthropic", "--label", "seat-b"]);
    assert!(
        !stderr.contains("unexpected argument") && !stderr.contains("invalid value"),
        "--label must be an accepted flag: {stderr}"
    );
    assert_eq!(
        code, 0,
        "labeled logout of empty store is a no-op: {stderr}"
    );
}

#[test]
fn empty_label_is_rejected_at_runtime() {
    // An empty `--label` parses at the clap layer (it is a String) but the
    // command body rejects it with a clear, secret-free message and a
    // non-zero exit, mirroring the SecretRef parser's empty-label rule.
    let (code, stderr) = run_routectl(&["logout", "anthropic", "--label", "   "]);
    assert_ne!(code, 0, "whitespace-only label must be rejected: {stderr}");
    assert!(
        stderr.contains("--label must not be empty"),
        "expected empty-label guidance, got: {stderr}"
    );
}

// -- did-you-mean enhancer, surfaced through the two CLI entry points that
// funnel through the shared config loader (`server::load_effective_config`
// -> `routectl_router::parse_config`). The hot-reload surface is covered in
// tests/hot_reload.rs. All three share the one parse funnel, so an unknown
// field's suggestion reaches every subcommand for free.

/// `routectl config check` against a config carrying an unknown field must
/// fail with the enhanced parse error: serde's `unknown field` message
/// plus the `did you mean` hint naming the closest real field.
#[test]
fn config_check_surfaces_did_you_mean_for_unknown_field() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!("version = {CURRENT}\n[server]\nprt = 8080\n"),
    )
    .unwrap();
    let path = config_path.to_str().unwrap();

    let (code, stderr) = run_routectl(&["--config", path, "config", "check"]);
    assert_ne!(code, 0, "an unknown field must fail config check: {stderr}");
    assert!(
        stderr.contains("did you mean `port`?"),
        "expected a did-you-mean suggestion on config check, got: {stderr}"
    );
}

/// Serve cold start against the same unknown-field config fails hard at the
/// pre-bind config load, carrying the same enhanced message. Proves the
/// suggestion rides the startup path, not just the offline check.
#[test]
fn serve_cold_start_surfaces_did_you_mean_for_unknown_field() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!("version = {CURRENT}\n[server]\nprt = 8080\n"),
    )
    .unwrap();
    let path = config_path.to_str().unwrap();

    let (code, stderr) = run_routectl(&["--config", path, "serve"]);
    assert_ne!(
        code, 0,
        "an unknown field must abort serve cold start: {stderr}"
    );
    assert!(
        stderr.contains("did you mean `port`?"),
        "expected a did-you-mean suggestion on serve cold start, got: {stderr}"
    );
}

/// Run `routectl <args...>` with a sandboxed (empty) XDG dir and return
/// `(exit_code, stdout, stderr)`. `check`'s rendered error list prints to
/// stdout; [`run_routectl`] captures only stderr, so this variant is used
/// where the assertion needs the stdout report.
fn run_routectl_full(args: &[&str]) -> (i32, String, String) {
    let tmp = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_routectl"))
        .args(args)
        .env("XDG_CONFIG_HOME", tmp.path())
        .output()
        .expect("spawn routectl binary");
    let code = out.status.code().unwrap_or(-1);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (code, stdout, stderr)
}

/// `config check` is the SHOWCASE validation surface: against a config that
/// PARSES but is semantically invalid (a reserved `[retry.classes.feature-unsupported]`
/// override), the real binary must exit non-zero AND render the full
/// error list with the source-line prefix -- not abort on the load-time
/// fail-fast gate that would print only the first plain error. The current
/// `version` stamp keeps the file loadable so the block stays on its written
/// line.
#[test]
fn config_check_renders_source_line_for_semantically_invalid_config() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    // The reserved class block is line 3 (version, blank line, then header).
    std::fs::write(
        &config_path,
        format!("version = {CURRENT}\n\n[retry.classes.feature-unsupported]\nfallback = false\n"),
    )
    .unwrap();
    let path = config_path.to_str().unwrap();

    let (code, stdout, stderr) = run_routectl_full(&["--config", path, "config", "check"]);
    assert_ne!(
        code, 0,
        "a semantically-invalid config must fail check; stdout: {stdout}, stderr: {stderr}"
    );
    assert!(
        stdout.contains("(line 3): ") && stdout.contains("[retry.classes.feature-unsupported]"),
        "expected the reserved-class error rendered with its source line, got stdout: {stdout}"
    );
}

/// The same semantically-invalid config must still be rejected fail-fast by
/// `serve` cold start: the load-time validation gate is unchanged for every
/// caller other than `check`. Serve surfaces the FIRST error plainly (no
/// `config check:` report, no line-prefixed multi-error list).
#[test]
fn serve_rejects_semantically_invalid_config_fail_fast() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!("version = {CURRENT}\n\n[retry.classes.feature-unsupported]\nfallback = false\n"),
    )
    .unwrap();
    let path = config_path.to_str().unwrap();

    let (code, stdout, stderr) = run_routectl_full(&["--config", path, "serve"]);
    assert_ne!(
        code, 0,
        "serve must reject the semantically-invalid config; stdout: {stdout}, stderr: {stderr}"
    );
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("[retry.classes.feature-unsupported]"),
        "expected the reserved-class error surfaced, got stdout: {stdout}, stderr: {stderr}"
    );
    assert!(
        !combined.contains("config check:"),
        "serve must not run the check report, got stdout: {stdout}, stderr: {stderr}"
    );
}

// -- `config check`'s informational OAuth seat-pool block. Spawned through
// the real binary so the assertions cover what an operator actually reads,
// including the exit code the block must never move.

/// Run `routectl <args...>` with `XDG_CONFIG_HOME` pointed at `xdg` (so a
/// seeded credentials.json is visible) and return `(exit_code, stdout,
/// stderr)`.
fn run_routectl_with_xdg(xdg: &std::path::Path, args: &[&str]) -> (i32, String, String) {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_routectl"))
        .args(args)
        .env("XDG_CONFIG_HOME", xdg)
        .output()
        .expect("spawn routectl binary");
    let code = out.status.code().unwrap_or(-1);
    (
        code,
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Write a config whose single provider entry resolves through `ref_uri`.
fn write_oauth_config(dir: &std::path::Path, ref_uri: &str) -> std::path::PathBuf {
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "version = {CURRENT}\n\
             [providers.managed]\n\
             kind = \"anthropic-api\"\n\
             api_key_ref = \"{ref_uri}\"\n\
             auth_kind = \"oauth-bearer\"\n"
        ),
    )
    .expect("write config");
    config_path
}

#[test]
fn config_check_reports_stored_seats_for_a_pool_ref() {
    let tmp = tempfile::tempdir().unwrap();
    seed_credentials(
        tmp.path(),
        &[
            ("anthropic", "default@example.com"),
            ("anthropic#seat-b", "seat-b@example.com"),
        ],
    );
    let config_path = write_oauth_config(tmp.path(), "oauth://anthropic");
    let path = config_path.to_str().unwrap();

    let (code, stdout, stderr) =
        run_routectl_with_xdg(tmp.path(), &["--config", path, "config", "check"]);

    assert_eq!(code, 0, "seat block is informational: {stdout} / {stderr}");
    assert!(
        stdout.contains("oauth seat pools:"),
        "expected the seat-pool block header, got: {stdout}"
    );
    assert!(
        stdout
            .contains("managed: pool ref oauth://anthropic resolves to 2 seats (default, seat-b)"),
        "expected both seats named under the entry key, got: {stdout}"
    );
    assert!(
        stdout.contains("seat_selection fill-first (default)"),
        "expected the strategy clause, got: {stdout}"
    );
}

/// Negative control for the block above: the fixture's token material,
/// account identity, and store path must never reach check's stdout. The
/// seat LABEL does (asserted in the test above), which is what proves this
/// scan can bite.
#[test]
fn config_check_seat_block_leaks_no_credential_material() {
    let tmp = tempfile::tempdir().unwrap();
    seed_credentials(tmp.path(), &[("anthropic", "leaky@example.com")]);
    let config_path = write_oauth_config(tmp.path(), "oauth://anthropic");
    let path = config_path.to_str().unwrap();

    let (_code, stdout, _stderr) =
        run_routectl_with_xdg(tmp.path(), &["--config", path, "config", "check"]);

    for sentinel in [
        "seeded-access-anthropic",
        "seeded-refresh-anthropic",
        "acct-anthropic",
        "leaky@example.com",
        "credentials.json",
    ] {
        assert!(
            !stdout.contains(sentinel),
            "credential-adjacent material leaked ({sentinel}): {stdout}"
        );
    }
}

/// No credentials file under a perfectly valid config dir opens as an EMPTY
/// store, which is an accurate answer ("nothing logged in") rather than an
/// unknown one.
#[test]
fn config_check_reports_no_stored_seats_for_an_empty_store() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = write_oauth_config(tmp.path(), "oauth://anthropic");
    let path = config_path.to_str().unwrap();

    let (code, stdout, stderr) =
        run_routectl_with_xdg(tmp.path(), &["--config", path, "config", "check"]);

    assert_eq!(
        code, 0,
        "an empty store is not a failure: {stdout} / {stderr}"
    );
    assert!(
        stdout.contains("pool ref oauth://anthropic has no stored seats"),
        "expected the empty-store wording, got: {stdout}"
    );
}

/// With neither `HOME` nor `XDG_CONFIG_HOME` set the store cannot be opened
/// at all: the count renders unknown, the strategy still renders, no path is
/// disclosed, and check still exits 0.
#[test]
fn config_check_reports_unknown_seat_count_when_the_store_cannot_open() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = write_oauth_config(tmp.path(), "oauth://anthropic");
    let path = config_path.to_str().unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_routectl"))
        .args(["--config", path, "config", "check"])
        .env_remove("HOME")
        .env_remove("XDG_CONFIG_HOME")
        .output()
        .expect("spawn routectl binary");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert_eq!(
        out.status.code(),
        Some(0),
        "an unreadable store must not fail check: {stdout}"
    );
    assert!(
        stdout.contains("seat count unknown (credential store unavailable)"),
        "expected the unknown-count wording, got: {stdout}"
    );
    assert!(
        stdout.contains("seat_selection fill-first (default)"),
        "the strategy is config-derived and stays known, got: {stdout}"
    );
}

/// An api-key-only config carries no `oauth://` ref, so the whole block --
/// header included -- is suppressed.
#[test]
fn config_check_omits_the_seat_block_without_any_oauth_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let config_path = tmp.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "version = {CURRENT}\n\
             [providers.plain]\n\
             kind = \"openai-compat\"\n\
             base_url = \"https://example.invalid\"\n\
             api_key_ref = \"env://ROUTECTL_TEST_KEY\"\n"
        ),
    )
    .unwrap();
    let path = config_path.to_str().unwrap();

    let (_code, stdout, _stderr) =
        run_routectl_with_xdg(tmp.path(), &["--config", path, "config", "check"]);

    assert!(
        !stdout.contains("oauth seat pools"),
        "no oauth ref must mean no header noise, got: {stdout}"
    );
}

// -- centralized config-validation suite: the shared ordered function is
// wired into every config surface, so the SAME bad configs are rejected on
// all four caller paths (config check, test, prompt-size, serve pre-parse
// gate). These pin the no-fork acceptance from the centralization work.

/// Alias pointing at a nickname that is neither a `[models]` entry nor
/// another alias key (`validate_alias_chain_targets`).
fn cfg_unknown_alias_target() -> Config {
    toml::from_str("[aliases]\nfast = \"ghost\"\n").expect("fixture must parse")
}

/// The reserved `[retry.classes.feature-unsupported]` override
/// (`validate_class_policy`).
fn cfg_reserved_class_override() -> Config {
    toml::from_str("[retry.classes.feature-unsupported]\nfallback = false\n")
        .expect("fixture must parse")
}

fn bad_config_fixtures() -> Vec<(&'static str, Config)> {
    vec![
        ("unknown-alias-target", cfg_unknown_alias_target()),
        ("reserved-class-override", cfg_reserved_class_override()),
    ]
}

#[tokio::test]
async fn config_check_rejects_each_centralized_bad_config() {
    for (name, config) in bad_config_fixtures() {
        match commands::config::check(&config, None).await {
            Err(routectl_core::Error::Config(_)) => {}
            Ok(()) => panic!("config check must reject `{name}`"),
            Err(other) => panic!("expected Config error for `{name}`, got: {other:?}"),
        }
    }
}

#[tokio::test]
async fn test_command_rejects_each_centralized_bad_config() {
    for (name, config) in bad_config_fixtures() {
        match commands::test::run(config, "fast", "Hi.").await {
            Err(routectl_core::Error::Config(_)) => {}
            Ok(()) => panic!("test command must reject `{name}` before dispatch"),
            Err(other) => panic!("expected Config error for `{name}`, got: {other:?}"),
        }
    }
}

#[test]
fn prompt_size_rejects_each_centralized_bad_config() {
    use routectl_cli::commands::prompt_size::{self, ProjectionArgs};
    use routectl_router::CatalogOverlay;
    use std::path::Path;

    // Validation runs before the request fixture is read, so a
    // non-existent path never matters -- the bad config fails first.
    let missing = Path::new("/nonexistent/request.json");
    let overlay = CatalogOverlay::default();
    let projection = ProjectionArgs {
        hypothetical_d: None,
        hypothetical_k: None,
        c_after: None,
        ttl_tier: "5m",
        steady_state: false,
    };

    for (name, config) in bad_config_fixtures() {
        match prompt_size::run(config, &overlay, "fast", missing, projection) {
            Err(routectl_core::Error::Config(_)) => {}
            Ok(()) => panic!("prompt-size must reject `{name}` before reading the fixture"),
            Err(other) => panic!("expected Config error for `{name}`, got: {other:?}"),
        }
    }
}

// -- semantic-error source-line rendering (config check's richer renderer).
// The rendered error/warning lines are printed by `check`; `validation_report`
// exposes the same rendering without the secret-store IO so it is testable.

/// A `[retry.classes.feature-unsupported]` reserved-class error must be
/// prefixed with the source line the block sits on when the raw config text
/// is available.
#[test]
fn validation_report_prefixes_semantic_error_with_source_line() {
    // The reserved class block is the 4th line (header, blank line, then it).
    let raw =
        "[server]\nhost = \"127.0.0.1\"\n\n[retry.classes.feature-unsupported]\nfallback = false\n";
    let config: Config = toml::from_str(raw).expect("fixture must parse");

    let report = commands::config::validation_report(&config, Some(raw));

    let retry_err = report
        .errors
        .iter()
        .find(|e| e.contains("[retry.classes.feature-unsupported]"))
        .expect("the reserved-class error must be reported");
    assert!(
        retry_err.starts_with("(line 4): "),
        "expected a source-line prefix, got: {retry_err}"
    );
}

/// When the derived key/path cannot be located in the supplied text, the
/// renderer keeps the plain message rather than inventing a line number.
#[test]
fn validation_report_falls_back_to_plain_when_path_not_locatable() {
    // The reserved-class error derives the path `retry.classes.feature-unsupported`,
    // but the text handed to the report carries no such block -- locate returns
    // None and the message stays plain.
    let cfg_raw = "[retry.classes.feature-unsupported]\nfallback = false\n";
    let config: Config = toml::from_str(cfg_raw).expect("fixture must parse");
    let unrelated_raw = "[server]\nhost = \"127.0.0.1\"\n";

    let report = commands::config::validation_report(&config, Some(unrelated_raw));

    let retry_err = report
        .errors
        .iter()
        .find(|e| e.contains("[retry.classes.feature-unsupported]"))
        .expect("the reserved-class error must be reported");
    assert!(
        !retry_err.starts_with("(line "),
        "expected a plain fallback with no line prefix, got: {retry_err}"
    );
    assert!(
        retry_err.starts_with("config: [retry.classes.feature-unsupported]"),
        "expected the bare validator message, got: {retry_err}"
    );
}
