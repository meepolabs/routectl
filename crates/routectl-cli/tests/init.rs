//! Integration boundary floor for `init`: drive the REAL guided-setup command
//! into a temp config on a fresh machine, via both the wizard (a non-interactive
//! [`StubInitIo`]) and the `--yes` flag path, then confirm the learnings-mandated
//! trio -- `config check` passes, the provider factory builds, and the server
//! boots to a live `/health` on the produced config. Plus the idempotence floor
//! (a byte-identical re-run no-op and a partial-failure re-run that duplicates
//! nothing and mints no new secret), the `--scaffold` refusal/output shape, the
//! `--yes` pre-side-effect error, the non-interactive no-hang guarantee, and the
//! forwarded end-to-end path that prompts for no secret.
//!
//! Every config is a TEMP copy; XDG is scoped to a fresh tempdir (isolating the
//! managed secret store and credentials.json) and the usage DB is isolated, so
//! no live config, DB, or credential file is ever touched. Env-mutating tests
//! are `#[serial_test::serial]`, and readiness is polled off `/health` rather
//! than a fixed sleep. Unit-level assertions owned by the init module's own test
//! modules are not duplicated here; this binary is the cross-cutting
//! real-command -> real-config -> real-server-boot verification.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use routectl_auth::{MemoryStore, SecretStore};
use routectl_cli::commands::init::scaffold::scaffold_seed;
use routectl_cli::commands::init::{self, CredentialCapture, InitArgs, InitIo, Offer};
use routectl_cli::commands::provider_add::{self, AddIo, AddResult, ProviderAddArgs};
use routectl_core::Result;
use routectl_router::{Config, build_provider, parse_config};
use tokio::net::TcpListener;

mod common;

/// The upstream model id wired for every provider these tests configure.
const MODEL_ID: &str = "claude-sonnet-4-5";

/// The conventional env var whose presence would add an extra `anthropic-api`
/// offer; unset in every scoped test so detection stays deterministic (a stray
/// value in the developer/CI environment would otherwise select a second
/// provider and make the `--yes` default route ambiguous).
const ANTHROPIC_ENV_VAR: &str = "ANTHROPIC_API_KEY";

// SAFETY: every test that mutates the process environment is annotated
// `#[serial_test::serial]`, so no other thread reads or writes the environment
// concurrently with these calls.
fn set_env(key: &str, val: &str) {
    unsafe { std::env::set_var(key, val) };
}

fn unset_env(key: &str) {
    unsafe { std::env::remove_var(key) };
}

/// Scope `XDG_CONFIG_HOME` at a fresh temp dir for the duration of a test so
/// the managed secret store (`$XDG/routectl/secrets`) and the OAuth store
/// (`$XDG/routectl/credentials.json`) resolve inside the tempdir instead of the
/// developer's real `~/.config/routectl`.
struct XdgScope {
    tmp: tempfile::TempDir,
    prev_anthropic_env: Option<std::ffi::OsString>,
}

impl XdgScope {
    fn creds_path(&self) -> PathBuf {
        self.tmp.path().join("routectl").join("credentials.json")
    }

    fn secrets_dir(&self) -> PathBuf {
        self.tmp.path().join("routectl").join("secrets")
    }

    /// Count the managed secret files currently on disk (an absent dir is zero),
    /// so a re-run can assert it minted no new captured secret.
    fn secret_file_count(&self) -> usize {
        std::fs::read_dir(self.secrets_dir())
            .map_or(0, |rd| rd.filter_map(std::result::Result::ok).count())
    }
}

impl Drop for XdgScope {
    fn drop(&mut self) {
        unset_env("XDG_CONFIG_HOME");
        match self.prev_anthropic_env.take() {
            Some(v) => unsafe { std::env::set_var(ANTHROPIC_ENV_VAR, v) },
            None => unset_env(ANTHROPIC_ENV_VAR),
        }
    }
}

fn scope_xdg() -> XdgScope {
    let tmp = tempfile::tempdir().unwrap();
    let prev_anthropic_env = std::env::var_os(ANTHROPIC_ENV_VAR);
    set_env("XDG_CONFIG_HOME", tmp.path().to_str().unwrap());
    unset_env(ANTHROPIC_ENV_VAR);
    XdgScope {
        tmp,
        prev_anthropic_env,
    }
}

/// A non-interactive [`InitIo`] stub: never a real TTY, stdin, prompt, or
/// browser. The wizard-choice seams return preconfigured answers; the inherited
/// [`AddIo`] `login` provisions a ready token into the XDG-scoped
/// credentials.json, standing in for a completed OAuth login so a later
/// build/boot resolves the credential without a live flow. `login_calls` and
/// `prompt_hidden_calls` record whether a secret side effect ever fired.
struct StubInitIo {
    scaffold: bool,
    offer_selection: Vec<usize>,
    model_id: Option<String>,
    default_route: Option<String>,
    ack: bool,
    is_tty: bool,
    stdin_value: String,
    offer_env: bool,
    prompt_value: String,
    credential_capture: CredentialCapture,
    login_calls: Mutex<u32>,
    prompt_hidden_calls: Mutex<u32>,
}

impl Default for StubInitIo {
    fn default() -> Self {
        Self {
            scaffold: false,
            offer_selection: Vec::new(),
            model_id: None,
            default_route: None,
            ack: true,
            is_tty: false,
            stdin_value: String::new(),
            offer_env: false,
            prompt_value: String::new(),
            credential_capture: CredentialCapture::Skip,
            login_calls: Mutex::new(0),
            prompt_hidden_calls: Mutex::new(0),
        }
    }
}

#[async_trait]
impl AddIo for StubInitIo {
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
        *self.prompt_hidden_calls.lock().unwrap() += 1;
        Ok(self.prompt_value.clone())
    }
    fn confirm_probe(&self) -> bool {
        false
    }
    async fn login(&self, provider: &str) -> Result<()> {
        *self.login_calls.lock().unwrap() += 1;
        provision_login_token(provider);
        Ok(())
    }
}

impl InitIo for StubInitIo {
    fn choose_scaffold_or_wizard(&self) -> bool {
        self.scaffold
    }
    fn select_offers(&self, _offers: &[Offer]) -> Vec<usize> {
        self.offer_selection.clone()
    }
    fn prompt_model_id(&self, _p: &str, _k: &str, _h: &str) -> Option<String> {
        self.model_id.clone()
    }
    fn choose_default_route(&self, _candidates: &[String]) -> Option<String> {
        self.default_route.clone()
    }
    fn confirm_wizard_ack(&self) -> bool {
        self.ack
    }
    fn offer_credential_capture(&self) -> CredentialCapture {
        self.credential_capture
    }
}

/// Write a future-expiry OAuth token for `provider` to the XDG-scoped
/// credentials.json (schema v1, 0600 on Unix), simulating a completed login.
/// Mirrors the on-disk shape `OAuthStore` reads.
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

fn init_args(scaffold: bool, yes: bool, default_model: Option<&str>, forwarded: bool) -> InitArgs {
    InitArgs {
        scaffold,
        yes,
        default_model: default_model.map(str::to_string),
        forwarded,
        probe: Some(false),
    }
}

/// The `provider add` args init composes for an oauth offer: the login sentinel
/// kind and a name matching the oauth id, everything else defaulted (init always
/// passes `yes: true` to its provider-add steps).
fn oauth_add_args(name: &str) -> ProviderAddArgs {
    ProviderAddArgs {
        kind: "anthropic".to_string(),
        name: name.to_string(),
        base_url: None,
        api_key_env: None,
        secret_ref: None,
        api_key_stdin: false,
        credential_source: None,
        overwrite: false,
        yes: true,
        probe: Some(false),
    }
}

/// Count `[providers.<name>]` block headers as exact lines, so a substring of a
/// longer provider key (`anthropic` vs `anthropic-forwarded`) is never
/// miscounted -- the re-init contract asserts EXACTLY one.
fn count_provider_block(text: &str, name: &str) -> usize {
    let header = format!("[providers.{name}]");
    text.lines().filter(|line| line.trim() == header).count()
}

fn default_alias_of(config: &Config) -> Option<String> {
    config
        .aliases
        .get("default")
        .and_then(|value| value.nicknames().next())
        .map(str::to_string)
}

/// Bind an ephemeral port, boot the server on `config` (usage DB isolated), and
/// return the base URL once `/health` responds. Polls generously rather than
/// sleeping a fixed readiness tick, so a slow boot does not race the smoke.
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

/// The learnings-mandated per-config floor: `config check` passes, the provider
/// factory builds over the wired provider, and the server boots to a live
/// `/health` on the produced config.
async fn assert_config_boots(text: &str, provider_name: &str, store: Arc<dyn SecretStore>) {
    let config = parse_config(text).expect("produced config parses");
    routectl_cli::commands::config::check(&config, Some(text))
        .await
        .expect("config check must pass on the produced config");

    {
        let entry = config
            .providers
            .get(provider_name)
            .expect("wired provider present in config");
        build_provider(provider_name, entry, store)
            .await
            .expect("provider factory must build over the wired entry");
    }

    let base = boot_and_await_health(Arc::new(config)).await;
    let resp = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
}

// ---------------------------------------------------------------------
// Fresh machine -> a routed, bootable config via BOTH the wizard and the
// `--yes` flag path. The forwarded offer is the deterministic vehicle: it wires
// with no secret, no login, and no managed-store touch, so a fresh empty XDG
// needs no seeded credential.
// ---------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn wizard_path_yields_a_routed_config_that_checks_builds_and_boots() {
    let _xdg = scope_xdg();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");

    let stub = StubInitIo {
        offer_selection: vec![0],
        model_id: Some(MODEL_ID.to_string()),
        default_route: Some("anthropic-forwarded".to_string()),
        ..Default::default()
    };
    init::run_with_io(&path, init_args(false, false, None, true), &stub)
        .await
        .expect("the guided wizard writes a routed config");

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("[providers.anthropic-forwarded]"), "{text}");
    assert_eq!(
        default_alias_of(&parse_config(&text).unwrap()).as_deref(),
        Some("anthropic-forwarded"),
    );

    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    assert_config_boots(&text, "anthropic-forwarded", store).await;
}

#[tokio::test]
#[serial_test::serial]
async fn yes_path_yields_a_routed_config_that_checks_builds_and_boots() {
    let _xdg = scope_xdg();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");

    init::run_with_io(
        &path,
        init_args(false, true, Some(MODEL_ID), true),
        &StubInitIo::default(),
    )
    .await
    .expect("the --yes flag path writes a routed config");

    let text = std::fs::read_to_string(&path).unwrap();
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    assert_config_boots(&text, "anthropic-forwarded", store).await;
}

#[tokio::test]
#[serial_test::serial]
async fn wizard_and_yes_paths_produce_a_byte_identical_config() {
    let _xdg = scope_xdg();

    let wizard_dir = tempfile::tempdir().unwrap();
    let wizard_path = wizard_dir.path().join("config.toml");
    let stub = StubInitIo {
        offer_selection: vec![0],
        model_id: Some(MODEL_ID.to_string()),
        default_route: Some("anthropic-forwarded".to_string()),
        ..Default::default()
    };
    init::run_with_io(&wizard_path, init_args(false, false, None, true), &stub)
        .await
        .expect("wizard run");

    let yes_dir = tempfile::tempdir().unwrap();
    let yes_path = yes_dir.path().join("config.toml");
    init::run_with_io(
        &yes_path,
        init_args(false, true, Some(MODEL_ID), true),
        &StubInitIo::default(),
    )
    .await
    .expect("--yes run");

    assert_eq!(
        std::fs::read(&wizard_path).unwrap(),
        std::fs::read(&yes_path).unwrap(),
        "the wizard and --yes paths must produce a byte-identical config",
    );
}

// ---------------------------------------------------------------------
// Empty-offer credential capture: a credential-less fresh machine no longer
// dead-ends at the missing-route error. The api-key branch drives `provider
// add`'s existing hidden prompt end to end (XDG scoped, so the managed secret
// store writes inside the tempdir), captures a `file://`-backed credential,
// and reaches a routed config -- the branch's real orchestration path, not
// just its planned args.
// ---------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn empty_offer_api_key_capture_wires_a_file_backed_provider() {
    let xdg = scope_xdg();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");

    let stub = StubInitIo {
        credential_capture: CredentialCapture::ApiKey,
        is_tty: true,
        prompt_value: "sk-ant-not-a-real-key".to_string(),
        offer_selection: vec![0],
        model_id: Some(MODEL_ID.to_string()),
        default_route: Some("anthropic".to_string()),
        ..Default::default()
    };
    init::run_with_io(&path, init_args(false, false, None, false), &stub)
        .await
        .expect("the empty-offer api-key capture writes a routed config");

    assert_eq!(
        *stub.prompt_hidden_calls.lock().unwrap(),
        1,
        "the credential is captured through the existing hidden prompt",
    );
    assert_eq!(
        *stub.login_calls.lock().unwrap(),
        0,
        "the api-key branch runs no oauth login",
    );
    assert_eq!(
        xdg.secret_file_count(),
        1,
        "exactly one managed secret is written to the scoped store",
    );

    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(count_provider_block(&text, "anthropic"), 1, "{text}");
    assert!(
        text.contains("file://"),
        "the captured key lands as a managed file ref: {text}",
    );
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    assert_config_boots(&text, "anthropic", store).await;
}

// ---------------------------------------------------------------------
// Multi-offer boundary: with two detected offers (an oauth login and the
// forwarded lane), the wizard wires them in a deterministic order and two
// independent runs produce a byte-identical config -- the command-boundary
// ordering guarantee, exercised end to end rather than only in the detect unit
// tests.
// ---------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn multiple_offers_wire_in_a_deterministic_order() {
    let run_once = || async {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        provision_login_token("anthropic");
        let stub = StubInitIo {
            offer_selection: vec![0, 1],
            model_id: Some(MODEL_ID.to_string()),
            default_route: Some("anthropic".to_string()),
            ..Default::default()
        };
        init::run_with_io(&path, init_args(false, false, None, true), &stub)
            .await
            .expect("wizard wires both offers");
        (dir, std::fs::read_to_string(&path).unwrap())
    };

    let _xdg = scope_xdg();
    let (_first_dir, first) = run_once().await;
    let (_second_dir, second) = run_once().await;

    let anthropic = first
        .find("[models.anthropic]")
        .expect("oauth model block present");
    let forwarded = first
        .find("[models.anthropic-forwarded]")
        .expect("forwarded model block present");
    assert!(
        anthropic < forwarded,
        "offers wire in sorted order (anthropic before anthropic-forwarded):\n{first}",
    );
    assert_eq!(
        first, second,
        "two independent multi-offer runs produce a byte-identical config",
    );
}

// ---------------------------------------------------------------------
// Re-run on a produced config is a byte-identical no-op walk-through.
// ---------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn rerun_on_a_produced_config_is_a_byte_identical_no_op() {
    let _xdg = scope_xdg();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");

    init::run_with_io(
        &path,
        init_args(false, true, Some(MODEL_ID), true),
        &StubInitIo::default(),
    )
    .await
    .expect("first init");
    let after_first = std::fs::read(&path).unwrap();

    init::run_with_io(
        &path,
        init_args(false, true, Some(MODEL_ID), true),
        &StubInitIo::default(),
    )
    .await
    .expect("re-run walk-through");

    assert_eq!(
        std::fs::read(&path).unwrap(),
        after_first,
        "re-running init on a complete config is a byte-identical no-op",
    );
}

// ---------------------------------------------------------------------
// Partial-failure re-run: a provider landed and its credential was captured on
// a first run that never reached the models/aliases write. A second init
// completes the setup WITHOUT duplicating the provider block or minting a new
// secret artifact.
//
// Init has no managed `file://` offer path (its offer sources are oauth / env /
// forwarded only); the oauth credentials.json is the ONLY persistent secret
// artifact init produces, so it is the vehicle that makes "no new secret file"
// a meaningful assertion (a forwarded-only run would mint nothing, leaving that
// check vacuous). The partial state is staged with init's OWN building blocks --
// `scaffold_seed` then the same `provider add` step init's apply loop runs --
// which is exactly the on-disk state a crash between the provider loop and the
// final write leaves behind.
// ---------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn partial_failure_rerun_completes_without_duplicating_or_reminting() {
    let xdg = scope_xdg();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");

    // Stage the crash-between-steps state: seed + one oauth provider block, with
    // the credential captured to credentials.json, but no models/aliases yet.
    scaffold_seed(&path).expect("seed the fresh config");
    let stub = StubInitIo::default();
    let staged = provider_add::run_with_io(&path, oauth_add_args("anthropic"), &stub)
        .await
        .expect("stage the provider that landed before the crash");
    assert_eq!(staged, AddResult::Written);
    assert_eq!(
        *stub.login_calls.lock().unwrap(),
        1,
        "staging captured once"
    );

    let staged_creds = std::fs::read(xdg.creds_path()).expect("staged credential on disk");
    let staged_secret_files = xdg.secret_file_count();
    assert_eq!(
        count_provider_block(&std::fs::read_to_string(&path).unwrap(), "anthropic"),
        1,
    );

    // Re-run init: it re-detects the oauth offer, re-runs the (now idempotent)
    // provider add as a no-op, and lands the final models/aliases write.
    init::run_with_io(&path, init_args(false, true, Some(MODEL_ID), false), &stub)
        .await
        .expect("the re-run completes the setup");

    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        count_provider_block(&text, "anthropic"),
        1,
        "the provider block appears exactly once after the re-run:\n{text}",
    );
    let config = parse_config(&text).expect("completed config parses");
    routectl_cli::commands::config::check(&config, Some(&text))
        .await
        .expect("the completed config passes check");
    assert_eq!(
        default_alias_of(&config).as_deref(),
        Some("anthropic"),
        "the re-run lands the routing the first run missed",
    );

    assert_eq!(
        *stub.login_calls.lock().unwrap(),
        1,
        "the idempotent re-run runs NO second login",
    );
    assert_eq!(
        xdg.secret_file_count(),
        staged_secret_files,
        "the re-run mints no additional managed secret file",
    );
    assert_eq!(
        std::fs::read(xdg.creds_path()).unwrap(),
        staged_creds,
        "the captured credential is reused byte-for-byte, not re-minted",
    );
}

// ---------------------------------------------------------------------
// Scaffold fast-path: refused on an existing config; its output is
// structurally valid (required sections present, never golden bytes). The
// shipped starter carries placeholder secret refs the operator fills in, so
// the assertion is structural -- the write already cleared the write-time gate.
// ---------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn scaffold_output_checks_and_carries_the_required_sections() {
    let _xdg = scope_xdg();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");

    init::run_with_io(
        &path,
        init_args(true, false, None, false),
        &StubInitIo::default(),
    )
    .await
    .expect("scaffold fast-path writes a starter config");

    let text = std::fs::read_to_string(&path).unwrap();
    let config = parse_config(&text).expect("scaffolded config parses");
    assert!(
        !config.providers.is_empty(),
        "[providers.*] present: {text}"
    );
    assert!(!config.models.is_empty(), "[models.*] present: {text}");
    assert!(!config.aliases.is_empty(), "[aliases] present: {text}");
    assert!(!config.server.host.is_empty(), "[server] present: {text}");
}

#[tokio::test]
#[serial_test::serial]
async fn scaffold_refuses_an_existing_config_and_leaves_it_byte_identical() {
    let _xdg = scope_xdg();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let existing = "version = 3\n[server]\nhost = \"10.0.0.1\"\n";
    std::fs::write(&path, existing).unwrap();

    let err = init::run_with_io(
        &path,
        init_args(true, false, None, false),
        &StubInitIo::default(),
    )
    .await
    .expect_err("scaffold must refuse an existing config");

    assert!(err.to_string().contains("already exists"), "err: {err}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        existing,
        "a refused scaffold leaves the existing config byte-identical",
    );
}

// ---------------------------------------------------------------------
// `--yes` with a missing model id errors actionably BEFORE any side effect.
// ---------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn yes_with_a_missing_model_id_errors_before_any_write() {
    let _xdg = scope_xdg();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");

    let err = init::run_with_io(
        &path,
        init_args(false, true, None, true),
        &StubInitIo::default(),
    )
    .await
    .expect_err("a missing model id under --yes must error");

    assert!(
        err.to_string().contains("--default-model"),
        "the error is actionable: {err}",
    );
    assert!(
        !path.exists(),
        "no partial write precedes the actionable error",
    );
}

// ---------------------------------------------------------------------
// Forwarded end to end: no secret prompt, no login, no captured credential --
// the produced config carries the `credential_source = "forwarded"` provider
// with an empty credential.
// ---------------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn forwarded_path_prompts_for_no_secret_and_writes_an_empty_credential() {
    let xdg = scope_xdg();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");

    let stub = StubInitIo::default();
    init::run_with_io(&path, init_args(false, true, Some(MODEL_ID), true), &stub)
        .await
        .expect("forwarded wiring");

    assert_eq!(
        *stub.login_calls.lock().unwrap(),
        0,
        "forwarded runs no login"
    );
    assert_eq!(
        *stub.prompt_hidden_calls.lock().unwrap(),
        0,
        "forwarded prompts for no secret",
    );
    assert_eq!(
        xdg.secret_file_count(),
        0,
        "forwarded captures no managed secret file",
    );
    assert!(
        !xdg.creds_path().exists(),
        "forwarded mints no oauth credential",
    );

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("credential_source = \"forwarded\""), "{text}");
    let config = parse_config(&text).unwrap();
    assert_eq!(
        config
            .providers
            .get("anthropic-forwarded")
            .unwrap()
            .api_key_ref(),
        Some(""),
        "a forwarded provider carries no configured credential",
    );
}

// ---------------------------------------------------------------------
// Command surface: every flag is documented, and a non-interactive
// missing-value invocation of the real binary terminates (never hangs).
// ---------------------------------------------------------------------

#[test]
fn init_help_documents_the_flag_surface() {
    let bin = env!("CARGO_BIN_EXE_routectl");
    let out = Command::new(bin)
        .args(["init", "--help"])
        .output()
        .expect("run `init --help`");
    assert!(out.status.success(), "help must exit 0");
    let help = String::from_utf8_lossy(&out.stdout);
    for flag in ["--scaffold", "--yes", "--default-model", "--forwarded"] {
        assert!(help.contains(flag), "help must document {flag}:\n{help}");
    }
}

#[test]
fn non_interactive_missing_value_run_terminates_without_hanging() {
    let bin = env!("CARGO_BIN_EXE_routectl");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("config.toml");
    let xdg = tempfile::tempdir().unwrap();

    // Fresh empty XDG (no oauth creds) + no offerable env var means the wizard
    // reaches an empty offer list. With stdin closed it cannot capture a
    // credential either, so it must print the actionable next step and exit --
    // never block on a prompt, and never surface the raw missing-route error.
    // The bounded receive fails the test on a hang instead of stalling forever.
    let (tx, rx) = std::sync::mpsc::channel();
    let path_arg = path.to_str().unwrap().to_string();
    let xdg_arg = xdg.path().to_str().unwrap().to_string();
    std::thread::spawn(move || {
        let out = Command::new(bin)
            .args(["--config", &path_arg, "init"])
            .env("XDG_CONFIG_HOME", &xdg_arg)
            .env_remove(ANTHROPIC_ENV_VAR)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        let _ = tx.send(out);
    });

    let out = rx
        .recv_timeout(Duration::from_secs(20))
        .expect("init must not hang on a non-interactive missing-value invocation")
        .expect("the init subprocess ran");
    let stdout = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "a credential-less non-interactive run exits cleanly with guidance, not a routing error",
    );
    assert!(
        stdout.contains("routectl login anthropic") && stdout.contains(ANTHROPIC_ENV_VAR),
        "the actionable next step names both setup paths:\n{stdout}",
    );
    assert!(
        !stdout.to_ascii_lowercase().contains("default route"),
        "the raw missing-route error must never surface:\n{stdout}",
    );
    assert!(!path.exists(), "the run wrote no config");
}
