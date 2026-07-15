//! Integration boundary floor for `provider add`: drive the REAL command
//! into a temp v3 config for each credential shape (`env://`, managed
//! `file://`, `oauth://`, `forwarded`), then confirm the learnings-mandated
//! pair -- `config check` passes AND the provider factory builds -- plus a
//! serve-boot `/health` smoke at the feature boundary. Also: the
//! non-interactive no-hang guarantee, comment/section-order preservation on
//! a round-trip add, and a secret-never-leaks scan across the capture paths.
//!
//! Every config is a TEMP copy and both the usage DB and the credential
//! stores (XDG-scoped) are isolated, so no live config, DB, or credential
//! file is ever touched. Unit-level assertions owned by the command's own
//! test module are not duplicated here; this binary is the cross-cutting
//! real-command -> real-config -> real-server-boot verification.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use routectl_auth::{MemoryStore, SecretStore};
use routectl_cli::commands::provider_add::{self, AddIo, AddResult, ProviderAddArgs};
use routectl_cli::server::CompositeStore;
use routectl_core::Result;
use routectl_router::{Config, build_provider, parse_config};
use tokio::net::TcpListener;

mod common;

const V3_BASE: &str = "\
version = 3

[server]
host = \"127.0.0.1\"
port = 8787

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"__API_KEY_REF__\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
";

// SAFETY: every test that mutates the process environment is annotated
// `#[serial_test::serial]`, so no other thread reads or writes the
// environment concurrently with these calls.
fn set_env(key: &str, val: &str) {
    unsafe { std::env::set_var(key, val) };
}

fn unset_env(key: &str) {
    unsafe { std::env::remove_var(key) };
}

/// Scope `XDG_CONFIG_HOME` at a fresh temp dir for the duration of a test so
/// the managed secret store (`$XDG/routectl/secrets`) and the OAuth store
/// (`$XDG/routectl/credentials.json`) resolve inside the tempdir instead of
/// the developer's real `~/.config/routectl`.
struct XdgScope {
    tmp: tempfile::TempDir,
}

impl XdgScope {
    fn creds_path(&self) -> PathBuf {
        self.tmp.path().join("routectl").join("credentials.json")
    }
}

impl Drop for XdgScope {
    fn drop(&mut self) {
        unset_env("XDG_CONFIG_HOME");
    }
}

fn scope_xdg() -> XdgScope {
    let tmp = tempfile::tempdir().unwrap();
    set_env("XDG_CONFIG_HOME", tmp.path().to_str().unwrap());
    XdgScope { tmp }
}

/// A non-interactive [`AddIo`] stub: never a real TTY, stdin, prompt, or
/// browser. `login` provisions a ready token into the XDG-scoped
/// credentials.json, standing in for a completed OAuth login so a later
/// build/boot resolves the credential without a live flow.
#[derive(Default)]
struct StubIo {
    is_tty: bool,
    stdin_value: String,
    prompt_value: String,
    offer_env: bool,
}

#[async_trait]
impl AddIo for StubIo {
    fn stdin_is_terminal(&self) -> bool {
        self.is_tty
    }
    fn read_stdin(&self) -> Result<String> {
        Ok(self.stdin_value.clone())
    }
    fn confirm_env_offer(&self, _var: &str) -> bool {
        self.offer_env
    }
    fn prompt_hidden(&self, _provider_name: &str) -> Result<String> {
        Ok(self.prompt_value.clone())
    }
    async fn login(&self, provider: &str) -> Result<()> {
        provision_login_token(provider);
        Ok(())
    }
}

/// Write a future-expiry OAuth token for `provider` to the XDG-scoped
/// credentials.json (schema v1, 0600 on Unix), simulating a completed
/// login. Mirrors the on-disk shape `OAuthStore` reads.
fn provision_login_token(provider: &str) {
    let xdg = std::env::var("XDG_CONFIG_HOME").expect("XDG_CONFIG_HOME scoped for the test");
    let path = Path::new(&xdg).join("routectl").join("credentials.json");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let creds = serde_json::json!({
        "schema_version": 1,
        "providers": {
            provider: {
                "access_token": format!("provisioned-access-{provider}"),
                "refresh_token": format!("provisioned-refresh-{provider}"),
                "token_type": "Bearer",
                "expires_at_unix": now + 3600,
                "scopes": ["openid", "offline_access"],
                "account": { "email": null, "account_id": format!("acct-{provider}") },
                "obtained_at_unix": now
            }
        }
    });
    std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir creds parent");
    std::fs::write(&path, serde_json::to_vec_pretty(&creds).unwrap()).expect("write creds");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 0600");
    }
}

fn write_config(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("config.toml");
    let body = body.replace("__API_KEY_REF__", &common::file_ref("test-key"));
    std::fs::write(&path, body).unwrap();
    path
}

fn base_args(kind: &str, name: &str) -> ProviderAddArgs {
    ProviderAddArgs {
        kind: kind.to_string(),
        name: name.to_string(),
        base_url: None,
        api_key_env: None,
        secret_ref: None,
        api_key_stdin: false,
        credential_source: None,
        overwrite: false,
        yes: true,
    }
}

/// Drive the real `provider add` command, assert it wrote, then parse the
/// resulting config back off disk. Returns the parsed config plus its raw
/// text (threaded into `config check` and the preservation assertions).
async fn add_and_parse(path: &Path, args: ProviderAddArgs, io: &dyn AddIo) -> (Config, String) {
    let result = provider_add::run_with_io(path, args, io)
        .await
        .expect("provider add must succeed");
    assert_eq!(result, AddResult::Written);
    let text = std::fs::read_to_string(path).unwrap();
    let config = parse_config(&text).expect("written config parses");
    (config, text)
}

/// Bind an ephemeral port, boot the server on `config` (usage DB isolated),
/// and return the base URL once `/health` responds. Polls generously rather
/// than sleeping a fixed readiness tick, so a slow boot does not race the
/// smoke (the pattern the config-migrate boot smokes use).
async fn boot_and_await_health(config: Arc<Config>) -> String {
    let config = common::isolate_usage_db(config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");

    tokio::spawn(async move {
        routectl_cli::server::serve_on_listener(config, listener, None)
            .await
            .expect("server failed to serve");
    });

    for _ in 0..100 {
        if let Ok(resp) = reqwest::get(format!("{base}/health")).await
            && resp.status().is_success()
        {
            return base;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("server did not become healthy in time");
}

/// The learnings-mandated per-shape floor: `config check` passes, the
/// provider factory builds over the added entry, and the server boots to a
/// live `/health` on the resulting config.
async fn assert_boundary(config: Config, text: &str, name: &str, store: Arc<dyn SecretStore>) {
    routectl_cli::commands::config::check(&config, Some(text))
        .await
        .expect("config check must pass on the added provider");

    {
        let entry = config
            .providers
            .get(name)
            .expect("added provider present in config");
        build_provider(name, entry, store)
            .await
            .expect("provider factory must build over the added entry");
    }

    let base = boot_and_await_health(Arc::new(config)).await;
    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

// ---------------------------------------------------------------------
// Per credential shape: check + build + serve-boot.
// ---------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn env_shape_checks_builds_and_boots() {
    let _xdg = scope_xdg();
    let key = "ROUTECTL_PROVIDER_ADD_IT_ENV_KEY";
    set_env(key, "env-shape-secret-value-not-real");

    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), V3_BASE);

    let mut a = base_args("anthropic-api", "claude");
    a.api_key_env = Some(key.to_string());
    let (config, text) = add_and_parse(&path, a, &StubIo::default()).await;

    assert_eq!(
        config.providers.get("claude").unwrap().api_key_ref(),
        Some(format!("env://{key}").as_str())
    );

    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    assert_boundary(config, &text, "claude", store).await;

    unset_env(key);
}

#[tokio::test]
#[serial_test::serial]
async fn managed_file_shape_checks_builds_and_boots() {
    let _xdg = scope_xdg();
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), V3_BASE);

    let mut a = base_args("openai-compat", "grok");
    a.base_url = Some("https://api.x.example/v1".to_string());
    a.api_key_stdin = true;
    let io = StubIo {
        stdin_value: "file-shape-secret-value-not-real\n".to_string(),
        ..Default::default()
    };
    let (config, text) = add_and_parse(&path, a, &io).await;

    let stored = config.providers.get("grok").unwrap().api_key_ref().unwrap();
    assert!(stored.starts_with("file://"), "managed ref: {stored}");

    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    assert_boundary(config, &text, "grok", store).await;
}

#[tokio::test]
#[serial_test::serial]
async fn oauth_shape_checks_builds_and_boots() {
    let xdg = scope_xdg();
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), V3_BASE);

    // `--kind anthropic` delegates to the login flow; the stub provisions a
    // token into the scoped credentials.json so no live browser is needed.
    let (config, text) = add_and_parse(
        &path,
        base_args("anthropic", "claude-sub"),
        &StubIo::default(),
    )
    .await;

    assert_eq!(
        config.providers.get("claude-sub").unwrap().api_key_ref(),
        Some("oauth://anthropic")
    );

    let store: Arc<dyn SecretStore> =
        Arc::new(CompositeStore::open_at(xdg.creds_path()).await.unwrap());
    assert_boundary(config, &text, "claude-sub", store).await;
}

#[tokio::test]
#[serial_test::serial]
async fn forwarded_shape_checks_builds_and_boots() {
    let _xdg = scope_xdg();
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), V3_BASE);

    let mut a = base_args("anthropic-api", "fwd");
    a.credential_source = Some("forwarded".to_string());
    let (config, text) = add_and_parse(&path, a, &StubIo::default()).await;

    assert_eq!(
        config.providers.get("fwd").unwrap().api_key_ref(),
        Some(""),
        "a forwarded provider carries no configured credential"
    );

    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    assert_boundary(config, &text, "fwd", store).await;
}

// ---------------------------------------------------------------------
// Non-interactive parity: every input is a flag, and a missing-required
// input with no TTY exits nonzero within a bounded time (never hangs).
// ---------------------------------------------------------------------

#[test]
fn every_provider_add_input_is_expressible_as_a_flag() {
    let bin = env!("CARGO_BIN_EXE_routectl");
    let out = std::process::Command::new(bin)
        .args(["provider", "add", "--help"])
        .output()
        .expect("run `provider add --help`");
    assert!(out.status.success(), "help must exit 0");
    let help = String::from_utf8_lossy(&out.stdout);
    for flag in [
        "--kind",
        "--name",
        "--base-url",
        "--api-key-env",
        "--secret-ref",
        "--api-key-stdin",
        "--credential-source",
        "--overwrite",
        "--yes",
    ] {
        assert!(help.contains(flag), "help must document {flag}:\n{help}");
    }
}

#[test]
fn missing_required_input_without_tty_errors_within_bounded_time() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), V3_BASE);

    // Run on a worker thread and collect the outcome through a channel with a
    // bounded receive: a hang (e.g. a regression that blocked on stdin) never
    // sends, so `recv_timeout` fails the test instead of stalling forever.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut a = base_args("openai-compat", "grok");
        a.base_url = Some("https://api.x.example/v1".to_string());
        let io = StubIo::default(); // not a TTY, no secret source
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let outcome = rt.block_on(provider_add::run_with_io(&path, a, &io));
        let _ = tx.send(outcome.err().map(|e| e.to_string()));
    });

    let outcome = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("provider add must not hang on a non-interactive missing-input invocation");
    let msg = outcome.expect("a missing required credential with no TTY must exit nonzero");
    assert!(msg.contains("--api-key-env"), "actionable message: {msg}");
    assert!(msg.contains("--api-key-stdin"), "actionable message: {msg}");
}

// ---------------------------------------------------------------------
// Format preservation: a round-trip add keeps pre-existing comments and
// section ordering intact.
// ---------------------------------------------------------------------

#[tokio::test]
async fn adding_a_provider_preserves_comments_and_section_order() {
    let body = "\
# top-of-file operator note
version = 3

[aliases]
default = \"gpt\"

[server]
host = \"127.0.0.1\"
port = 8787

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[providers.fast]
# keep this inline note
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"
";
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), body);

    let mut a = base_args("openai-compat", "grok");
    a.base_url = Some("https://api.x.example/v1".to_string());
    a.secret_ref = Some("file:///abs/key".to_string());
    let (_config, text) = add_and_parse(&path, a, &StubIo::default()).await;

    assert!(text.contains("# top-of-file operator note"), "{text}");
    assert!(text.contains("# keep this inline note"), "{text}");

    let idx = |needle: &str| {
        text.find(needle)
            .unwrap_or_else(|| panic!("missing `{needle}` in:\n{text}"))
    };
    assert!(
        idx("# top-of-file operator note") < idx("[aliases]"),
        "{text}"
    );
    assert!(idx("[aliases]") < idx("[server]"), "{text}");
    assert!(idx("[server]") < idx("[models.gpt]"), "{text}");
    assert!(idx("[models.gpt]") < idx("[providers.fast]"), "{text}");
    assert!(text.contains("[providers.grok]"), "{text}");
    assert!(
        idx("[providers.fast]") < idx("[providers.grok]"),
        "the new provider appends after the existing one: {text}"
    );
}

// ---------------------------------------------------------------------
// Audit parity: an oauth-backed add resolves `--kind anthropic` to an
// `anthropic-api` block, so the audit event must report the resolved kind
// that lands on disk, not the CLI-supplied one.
// ---------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn oauth_add_audits_the_resolved_kind() {
    let _xdg = scope_xdg();
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), V3_BASE);

    let (_r, events) = routectl_testkit::with_capture(async {
        provider_add::run_with_io(
            &path,
            base_args("anthropic", "claude-sub"),
            &StubIo::default(),
        )
        .await
        .expect("oauth add");
    })
    .await;

    let audit: Vec<_> = events
        .iter()
        .filter(|e| e.field("surface") == Some("cli") && e.field("verb") == Some("provider-add"))
        .collect();
    assert_eq!(
        audit.len(),
        1,
        "exactly one provider-add audit event expected"
    );
    assert_eq!(
        audit[0].field("kind"),
        Some("anthropic-api"),
        "the audit kind must match the on-disk `anthropic-api` block, not the CLI `anthropic`"
    );

    // The block actually written carries the same resolved kind, confirming
    // the audit event and the config agree.
    let config = parse_config(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        config.providers.get("claude-sub").unwrap().kind_str(),
        "anthropic-api"
    );
}

// ---------------------------------------------------------------------
// Secret never leaks: across the env / stdin / prompt capture paths the
// secret value never reaches a tracing event's message or fields.
// ---------------------------------------------------------------------

fn assert_no_leak(events: &[routectl_testkit::CapturedEvent], secret: &str) {
    let saw_audit = events
        .iter()
        .any(|e| e.field("verb") == Some("provider-add"));
    assert!(saw_audit, "the add must emit its audit event under capture");

    for e in events {
        assert!(
            !e.message.contains(secret),
            "secret leaked into a tracing message: {}",
            e.message
        );
        for (k, v) in &e.fields {
            assert!(
                !v.contains(secret),
                "secret leaked into tracing field `{k}`: {v}"
            );
        }
    }
}

#[tokio::test]
#[serial_test::serial]
async fn no_capture_path_leaks_the_secret_to_tracing() {
    let _xdg = scope_xdg();

    // env://
    let key = "ROUTECTL_PROVIDER_ADD_IT_LEAK_ENV";
    let env_secret = "leak-env-secret-value-not-real";
    set_env(key, env_secret);
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), V3_BASE);
    let (_r, events) = routectl_testkit::with_capture(async {
        let mut a = base_args("anthropic-api", "claude");
        a.api_key_env = Some(key.to_string());
        provider_add::run_with_io(&path, a, &StubIo::default())
            .await
            .expect("env add");
    })
    .await;
    assert_no_leak(&events, env_secret);
    unset_env(key);

    // stdin -> managed file://
    let stdin_secret = "leak-stdin-secret-value-not-real";
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), V3_BASE);
    let (_r, events) = routectl_testkit::with_capture(async {
        let mut a = base_args("openai-compat", "grok");
        a.base_url = Some("https://api.x.example/v1".to_string());
        a.api_key_stdin = true;
        let io = StubIo {
            stdin_value: format!("{stdin_secret}\n"),
            ..Default::default()
        };
        provider_add::run_with_io(&path, a, &io)
            .await
            .expect("stdin add");
    })
    .await;
    assert_no_leak(&events, stdin_secret);

    // hidden prompt -> managed file://
    let prompt_secret = "leak-prompt-secret-value-not-real";
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), V3_BASE);
    let (_r, events) = routectl_testkit::with_capture(async {
        let mut a = base_args("openai-compat", "grok2");
        a.base_url = Some("https://api.x.example/v1".to_string());
        let io = StubIo {
            is_tty: true,
            prompt_value: prompt_secret.to_string(),
            ..Default::default()
        };
        provider_add::run_with_io(&path, a, &io)
            .await
            .expect("prompt add");
    })
    .await;
    assert_no_leak(&events, prompt_secret);

    // --secret-ref carrying an inline `literal:` value is now rejected outright
    // (an inline key on argv is a leak vector by construction). The rejection
    // path must still never surface the value in a tracing event.
    let literal_secret = "leak-literal-secret-value-not-real";
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), V3_BASE);
    let (result, events) = routectl_testkit::with_capture(async {
        let mut a = base_args("openai-compat", "grok3");
        a.base_url = Some("https://api.x.example/v1".to_string());
        a.secret_ref = Some(format!("literal:{literal_secret}"));
        provider_add::run_with_io(&path, a, &StubIo::default()).await
    })
    .await;
    assert!(
        result.is_err(),
        "an inline literal: secret-ref must be rejected"
    );
    for e in &events {
        assert!(
            !e.message.contains(literal_secret),
            "secret leaked into a tracing message: {}",
            e.message
        );
        for (k, v) in &e.fields {
            assert!(
                !v.contains(literal_secret),
                "secret leaked into tracing field `{k}`: {v}"
            );
        }
    }
}
