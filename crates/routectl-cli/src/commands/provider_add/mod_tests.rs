//! Loaded via `#[cfg(test)] #[path = "mod_tests.rs"] mod tests;` in `mod.rs`.

use super::*;

use routectl_auth::SecretRef;
use routectl_router::{CURRENT_CONFIG_VERSION, ProviderEntry};

use super::build::resolve_secret;
use super::capture::{capture_value, execute_pending};
use super::toml_edit::{commit, provider_table};
use crate::commands::provider_env::env_var_for_kind;

/// A minimal valid config at the version this build writes, rendered from
/// the const so the next schema bump needs no fixture edit here.
fn current_base() -> String {
    format!("version = {CURRENT_CONFIG_VERSION}\n{BASE_BODY}")
}

const BASE_BODY: &str = "\
[server]
host = \"127.0.0.1\"
port = 8787

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
";

fn write_config(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("config.toml");
    std::fs::write(&path, body).unwrap();
    path
}

fn args(kind: &str, name: &str) -> ProviderAddArgs {
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
        probe: Some(false),
    }
}

fn set_env(key: &str, val: &str) {
    // SAFETY: env-touching tests are serialized via serial_test, so no
    // other thread reads or writes the process environment concurrently.
    unsafe { std::env::set_var(key, val) };
}

fn unset_env(key: &str) {
    // SAFETY: see set_env.
    unsafe { std::env::remove_var(key) };
}

// -----------------------------------------------------------------
// Secret resolution: the ref STRING is computed without the value; a
// `literal:` secret-ref is refused so no inline key reaches argv/config.
// -----------------------------------------------------------------

#[test]
#[serial_test::serial]
fn resolve_secret_env_yields_scheme_ref_not_the_value() {
    let key = "ROUTECTL_PROVIDER_ADD_RESOLVE_KEY";
    set_env(key, "the-actual-secret-value");

    let mut a = args("anthropic-api", "x");
    a.api_key_env = Some(key.to_string());
    let (ref_str, class, _pending) = resolve_secret(&a, &FakeIo::default()).unwrap();

    assert_eq!(ref_str, format!("env://{key}"));
    assert_eq!(class, "env");
    assert!(
        !ref_str.contains("the-actual-secret-value"),
        "the env value must never appear in the ref string"
    );
    unset_env(key);
}

#[test]
fn resolve_secret_rejects_literal_ref() {
    // `--secret-ref literal:...` is refused: the inline key would land
    // on argv and be persisted in plaintext in config. The error must
    // steer to the safe paths and never echo the key value.
    let mut a = args("openai-compat", "x");
    a.secret_ref = Some("literal:keep-me-exactly".to_string());
    let err = match resolve_secret(&a, &FakeIo::default()) {
        Ok(_) => panic!("a literal: secret-ref must be rejected"),
        Err(e) => e,
    };

    let msg = err.to_string();
    assert!(
        !msg.contains("keep-me-exactly"),
        "rejection must not echo the key value: {msg}"
    );
    assert!(
        msg.contains("--api-key-stdin") && msg.contains("prompt") && msg.contains("env://"),
        "rejection must name the safe paths: {msg}"
    );
}

// -----------------------------------------------------------------
// Happy path: a flag-driven openai-compat add writes a valid v3 block.
// -----------------------------------------------------------------

#[tokio::test]
async fn adds_openai_compat_via_secret_ref() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());

    let mut a = args("openai-compat", "grok");
    a.base_url = Some("https://api.x.example/v1".to_string());
    a.secret_ref = Some("file:///abs/key".to_string());

    let result = run(&path, a).await.expect("add");
    assert_eq!(result, AddResult::Written);

    let text = std::fs::read_to_string(&path).unwrap();
    let config = parse_config(&text).expect("written config parses");
    let entry = config.providers.get("grok").expect("provider present");
    assert_eq!(entry.kind_str(), "openai-compat");
    assert_eq!(entry.api_key_ref(), Some("file:///abs/key"));
    assert!(text.contains("[providers.grok]"), "{text}");
    assert!(text.contains("api_key_ref = \"file:///abs/key\""), "{text}");
}

// -----------------------------------------------------------------
// --api-key-env writes env://VAR; the var VALUE never appears anywhere.
// -----------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn adds_via_api_key_env_without_leaking_the_value() {
    let key = "ROUTECTL_PROVIDER_ADD_TEST_KEY";
    let secret_value = "super-secret-token-value-not-real";
    set_env(key, secret_value);

    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());

    let mut a = args("anthropic-api", "claude");
    a.api_key_env = Some(key.to_string());

    let result = run(&path, a).await.expect("add");
    assert_eq!(result, AddResult::Written);

    let text = std::fs::read_to_string(&path).unwrap();
    let config = parse_config(&text).unwrap();
    let entry = config.providers.get("claude").unwrap();
    assert_eq!(entry.kind_str(), "anthropic-api");
    assert_eq!(entry.api_key_ref(), Some(format!("env://{key}").as_str()));
    assert!(
        !text.contains(secret_value),
        "the env var value must never land in the config"
    );

    unset_env(key);
}

#[tokio::test]
#[serial_test::serial]
async fn key_env_that_is_unset_errors_without_writing() {
    let key = "ROUTECTL_PROVIDER_ADD_UNSET_KEY";
    unset_env(key);

    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let before = std::fs::read(&path).unwrap();

    let mut a = args("anthropic-api", "claude");
    a.api_key_env = Some(key.to_string());

    let err = run(&path, a).await.expect_err("unset env var must error");
    assert!(err.to_string().contains(key), "err: {err}");
    assert_eq!(std::fs::read(&path).unwrap(), before, "must not write");
}

// -----------------------------------------------------------------
// Format preservation: comments + ordering survive the surgical insert.
// -----------------------------------------------------------------

#[tokio::test]
async fn preserves_comments_and_existing_blocks() {
    let dir = tempfile::tempdir().unwrap();
    let body = format!("# operator note\nversion = {CURRENT_CONFIG_VERSION}\n")
        + "\
[server]
host = \"127.0.0.1\"
port = 8787

[providers.fast]
# keep this comment
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"gpt\"
";
    let path = write_config(dir.path(), &body);

    let mut a = args("openai-compat", "grok");
    a.base_url = Some("https://api.x.example/v1".to_string());
    a.secret_ref = Some("file:///abs/key".to_string());

    run(&path, a).await.expect("add");

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("# operator note"), "{text}");
    assert!(text.contains("# keep this comment"), "{text}");
    assert!(
        text.find("# operator note").unwrap() < text.find("[server]").unwrap(),
        "{text}"
    );
    // The pre-existing provider is untouched; the new one is appended.
    assert!(text.contains("[providers.fast]"), "{text}");
    assert!(text.contains("[providers.grok]"), "{text}");
}

// -----------------------------------------------------------------
// Idempotent re-add: a byte-identical re-add writes nothing.
// -----------------------------------------------------------------

#[tokio::test]
async fn identical_re_add_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());

    let mut first = args("openai-compat", "grok");
    first.base_url = Some("https://api.x.example/v1".to_string());
    first.secret_ref = Some("file:///abs/key".to_string());
    assert_eq!(
        run(&path, first).await.expect("first add"),
        AddResult::Written
    );

    let after_first = std::fs::read(&path).unwrap();

    let mut second = args("openai-compat", "grok");
    second.base_url = Some("https://api.x.example/v1".to_string());
    second.secret_ref = Some("file:///abs/key".to_string());
    assert_eq!(
        run(&path, second).await.expect("re-add"),
        AddResult::NoChange
    );

    assert_eq!(
        std::fs::read(&path).unwrap(),
        after_first,
        "an identical re-add must leave the file byte-identical"
    );
}

// -----------------------------------------------------------------
// Existing name, different block: refused without --overwrite, overwrites
// with it.
// -----------------------------------------------------------------

#[tokio::test]
async fn different_block_on_existing_name_is_refused_without_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let before = std::fs::read(&path).unwrap();

    // "fast" already exists with a different base_url + api_key_ref.
    let mut a = args("openai-compat", "fast");
    a.base_url = Some("https://elsewhere.example/v1".to_string());
    a.secret_ref = Some("file:///abs/key".to_string());

    let err = run(&path, a)
        .await
        .expect_err("must refuse a conflicting overwrite");
    assert!(err.to_string().contains("--overwrite"), "err: {err}");
    assert_eq!(std::fs::read(&path).unwrap(), before, "must not write");
}

#[tokio::test]
async fn overwrite_replaces_an_existing_block() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());

    let mut a = args("openai-compat", "fast");
    a.base_url = Some("https://elsewhere.example/v1".to_string());
    a.secret_ref = Some("file:///abs/key".to_string());
    a.overwrite = true;

    let result = run(&path, a).await.expect("overwrite");
    assert_eq!(result, AddResult::Written);

    let config = parse_config(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let entry = config.providers.get("fast").unwrap();
    assert_eq!(entry.api_key_ref(), Some("file:///abs/key"));
}

#[tokio::test]
async fn overwrite_still_passes_through_the_confirm_gate() {
    // `--overwrite` clears the existing-block refusal but NOT the
    // high-consequence confirmation: with yes=false and a non-terminal stdin the
    // overwrite is declined and the original block is left byte-identical.
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let before = std::fs::read(&path).unwrap();

    let mut a = args("openai-compat", "fast");
    a.base_url = Some("https://elsewhere.example/v1".to_string());
    a.secret_ref = Some("file:///abs/key".to_string());
    a.overwrite = true;
    a.yes = false;

    let result = run(&path, a).await.expect("declining is not an error");
    assert_eq!(result, AddResult::Aborted);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "a declined overwrite must leave the original block untouched"
    );
}

// -----------------------------------------------------------------
// Gate failure: a candidate that fails the shared validator writes
// nothing.
// -----------------------------------------------------------------

#[tokio::test]
async fn candidate_failing_the_gate_writes_nothing() {
    // The base parses but carries a latent semantic error (an alias
    // pointing at an undefined model). It loads far enough for `prev`,
    // but the shared gate re-validates the whole candidate and rejects
    // it -- so `provider add` refuses to write and leaves the file
    // byte-identical, before it ever reaches the confirmation prompt.
    let body = format!("version = {CURRENT_CONFIG_VERSION}\n")
        + "\
[server]
host = \"127.0.0.1\"
port = 8787

[providers.fast]
kind = \"openai-compat\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:test-key\"

[models.gpt]
provider = \"fast\"
upstream = \"gpt-4o\"

[aliases]
default = \"no-such-model\"
";
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &body);
    let before = std::fs::read(&path).unwrap();

    let mut a = args("openai-compat", "grok");
    a.base_url = Some("https://api.x.example/v1".to_string());
    a.secret_ref = Some("file:///abs/key".to_string());

    let err = run(&path, a).await;
    assert!(err.is_err(), "a candidate failing the gate must be refused");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "gate failure must leave the file byte-identical"
    );
}

// -----------------------------------------------------------------
// Missing credential: a key-requiring kind with no secret flag errors
// actionably (and never hangs -- no prompt in this command).
// -----------------------------------------------------------------

#[tokio::test]
async fn missing_secret_source_errors_actionably() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let before = std::fs::read(&path).unwrap();

    let mut a = args("openai-compat", "grok");
    a.base_url = Some("https://api.x.example/v1".to_string());
    // no api_key_env, no secret_ref

    let err = run(&path, a).await.expect_err("must require a credential");
    let msg = err.to_string();
    assert!(
        msg.contains("--api-key-env") && msg.contains("--secret-ref"),
        "{msg}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before, "must not write");
}

#[tokio::test]
async fn adds_gemini_with_default_base_url() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());

    let mut a = args("gemini", "gem");
    a.secret_ref = Some("env://GEMINI_API_KEY".to_string());

    let result = run(&path, a).await.expect("add gemini");
    assert_eq!(result, AddResult::Written);

    let config = parse_config(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let entry = config.providers.get("gem").unwrap();
    assert_eq!(entry.kind_str(), "gemini");
    assert_eq!(entry.api_key_ref(), Some("env://GEMINI_API_KEY"));
}

#[tokio::test]
async fn gemini_rejects_base_url_flag() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let before = std::fs::read(&path).unwrap();

    let mut a = args("gemini", "gem");
    a.base_url = Some("https://example/v1beta".to_string());
    a.secret_ref = Some("env://GEMINI_API_KEY".to_string());

    let err = run(&path, a)
        .await
        .expect_err("gemini must reject --base-url");
    assert!(err.to_string().contains("--base-url"), "err: {err}");
    assert_eq!(std::fs::read(&path).unwrap(), before, "must not write");
}

#[tokio::test]
async fn unsupported_kind_errors_actionably() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());

    let mut a = args("bedrock", "aws");
    a.secret_ref = Some("file:///abs/key".to_string());

    let err = run(&path, a)
        .await
        .expect_err("unsupported kind must error");
    let msg = err.to_string();
    assert!(
        msg.contains("cannot be added with this command"),
        "err: {msg}"
    );
    assert!(
        msg.contains("supported kinds"),
        "err lists the supported set: {msg}"
    );
}

// -----------------------------------------------------------------
// High-consequence confirm: --yes bypasses; declining leaves the file
// byte-identical.
// -----------------------------------------------------------------

#[tokio::test]
async fn declining_the_confirmation_writes_nothing() {
    // yes=false with a non-terminal stdin -> confirm declines without
    // reading -> abort with the file untouched.
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let before = std::fs::read(&path).unwrap();

    let mut a = args("openai-compat", "grok");
    a.base_url = Some("https://api.x.example/v1".to_string());
    a.secret_ref = Some("file:///abs/key".to_string());
    a.yes = false;

    let result = run(&path, a).await.expect("declining is not an error");
    assert_eq!(result, AddResult::Aborted);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "decline must not write"
    );
}

// -----------------------------------------------------------------
// Audit event: exactly one, with the required fields and NO value / NO
// full ref string.
// -----------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn emits_one_audit_event_without_value_or_full_ref() {
    let key = "ROUTECTL_PROVIDER_ADD_AUDIT_KEY";
    let secret_value = "audit-secret-value-not-real";
    set_env(key, secret_value);

    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());

    let (_res, events) = routectl_testkit::with_capture(async {
        let mut a = args("anthropic-api", "claude");
        a.api_key_env = Some(key.to_string());
        run(&path, a).await.expect("add");
    })
    .await;

    let audit: Vec<_> = events
        .iter()
        .filter(|e| e.field("surface") == Some("cli") && e.field("verb") == Some("provider-add"))
        .collect();
    assert_eq!(audit.len(), 1, "exactly one audit event expected");

    let event = audit[0];
    assert_eq!(event.field("name"), Some("claude"));
    assert_eq!(event.field("kind"), Some("anthropic-api"));
    assert_eq!(
        event.field("credential_source"),
        Some("env"),
        "credential_source is the scheme class only"
    );
    assert!(
        event.field("value").is_none(),
        "the value must never be audited"
    );
    assert!(
        event.field("api_key_ref").is_none(),
        "the full ref must never be audited"
    );

    unset_env(key);
}

// -----------------------------------------------------------------
// Stale-snapshot conflict: the write refuses and the file is unchanged.
// -----------------------------------------------------------------

#[test]
fn stale_snapshot_conflict_leaves_file_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let stale = std::fs::read(&path).unwrap();
    let stale_text = String::from_utf8(stale.clone()).unwrap();

    // Something else rewrote the file after the caller snapshotted it.
    let rewritten = format!("{}# added out of band\n", current_base());
    std::fs::write(&path, &rewritten).unwrap();

    let entry = ProviderEntry::openai_compat("https://api.x.example/v1", "file:///abs/key");
    let block = provider_table(&entry).unwrap();

    let err = commit(&path, &stale, &stale_text, "grok", block)
        .expect_err("a stale snapshot must conflict");
    assert!(err.to_string().contains("changed on disk"), "err: {err}");
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        rewritten,
        "a conflict must leave the on-disk file untouched"
    );
}

// -----------------------------------------------------------------
// Secret-source breadth: stdin capture, TTY guards, interactive prompt,
// env-detect offer, oauth delegation, forwarded, post-capture conflict.
// -----------------------------------------------------------------

/// Configurable [`AddIo`] fake: no real TTY, stdin, prompt, or browser.
/// Interior state uses `Mutex` (not `RefCell`) so the fake is `Send +
/// Sync`, as the `AddIo` supertrait now requires for its async `login`.
struct FakeIo {
    is_tty: bool,
    stdin_value: String,
    stdin_hook: Option<Box<dyn Fn() + Send + Sync>>,
    offer_env: bool,
    prompt_value: String,
    login_ok: bool,
    /// Side effect the stub login performs before returning, standing in
    /// for whatever the real login flow does to the filesystem.
    login_hook: Option<Box<dyn Fn() + Send + Sync>>,
    confirm_probe: bool,
    login_calls: std::sync::Mutex<Vec<String>>,
    stdin_reads: std::sync::Mutex<u32>,
    prompt_calls: std::sync::Mutex<u32>,
}

impl Default for FakeIo {
    fn default() -> Self {
        Self {
            is_tty: false,
            stdin_value: String::new(),
            stdin_hook: None,
            offer_env: false,
            prompt_value: String::new(),
            login_ok: true,
            login_hook: None,
            confirm_probe: false,
            login_calls: std::sync::Mutex::new(Vec::new()),
            stdin_reads: std::sync::Mutex::new(0),
            prompt_calls: std::sync::Mutex::new(0),
        }
    }
}

#[async_trait]
impl AddIo for FakeIo {
    fn stdin_is_terminal(&self) -> bool {
        self.is_tty
    }
    fn read_stdin(&self) -> Result<String> {
        *self.stdin_reads.lock().unwrap() += 1;
        if let Some(hook) = &self.stdin_hook {
            hook();
        }
        Ok(self.stdin_value.clone())
    }
    fn confirm_env_offer(&self, _var: &str) -> bool {
        self.offer_env
    }
    fn prompt_hidden(&self, _provider_name: &str) -> Result<String> {
        *self.prompt_calls.lock().unwrap() += 1;
        Ok(self.prompt_value.clone())
    }
    fn confirm_probe(&self) -> bool {
        self.confirm_probe
    }
    async fn login(&self, provider: &str) -> Result<()> {
        self.login_calls.lock().unwrap().push(provider.to_string());
        if let Some(hook) = &self.login_hook {
            hook();
        }
        if self.login_ok {
            Ok(())
        } else {
            Err(Error::Auth("login failed".into()))
        }
    }
}

/// Point `default_secret_dir` at a temp XDG root so captures land in an
/// isolated store. Returns the guard tempdir (keep it alive) and the
/// secrets dir the store will use.
fn scoped_secret_dir(tmp: &std::path::Path) -> std::path::PathBuf {
    set_env("XDG_CONFIG_HOME", tmp.to_str().unwrap());
    tmp.join("routectl").join("secrets")
}

#[tokio::test]
#[serial_test::serial]
async fn api_key_stdin_captures_to_managed_store_and_writes_only_the_ref() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let xdg = tempfile::tempdir().unwrap();
    let secrets = scoped_secret_dir(xdg.path());
    let secret_value = "piped-secret-value-not-real";

    let mut a = args("openai-compat", "grok");
    a.base_url = Some("https://api.x.example/v1".to_string());
    a.api_key_stdin = true;
    let io = FakeIo {
        stdin_value: format!("{secret_value}\n"),
        ..Default::default()
    };

    let result = run_with_io(&path, a, &io).await.expect("stdin capture");
    assert_eq!(result, AddResult::Written);

    let text = std::fs::read_to_string(&path).unwrap();
    let config = parse_config(&text).unwrap();
    let entry = config.providers.get("grok").unwrap();
    let stored_ref = entry.api_key_ref().unwrap();
    assert!(stored_ref.starts_with("file://"), "ref: {stored_ref}");
    assert!(
        !text.contains(secret_value),
        "the piped value must never land in the config"
    );

    // The captured file holds the exact key (trailing newline stripped)
    // and nothing else references the value.
    let captured = std::fs::read_to_string(secrets.join("grok")).unwrap();
    assert_eq!(captured, secret_value);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(secrets.join("grok"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "captured secret must be 0600");
    }
    unset_env("XDG_CONFIG_HOME");
}

#[tokio::test]
#[serial_test::serial]
async fn api_key_stdin_on_a_tty_errors_immediately_without_reading() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let before = std::fs::read(&path).unwrap();

    let mut a = args("openai-compat", "grok");
    a.base_url = Some("https://api.x.example/v1".to_string());
    a.api_key_stdin = true;
    let io = FakeIo {
        is_tty: true,
        ..Default::default()
    };

    let err = run_with_io(&path, a, &io)
        .await
        .expect_err("TTY stdin must error");
    assert!(err.to_string().contains("stdin is a TTY"), "err: {err}");
    assert_eq!(
        *io.stdin_reads.lock().unwrap(),
        0,
        "a TTY stdin must never be read (no hang)"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before, "must not write");
}

#[tokio::test]
#[serial_test::serial]
async fn missing_key_without_tty_errors_actionably_and_never_prompts() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let before = std::fs::read(&path).unwrap();

    let mut a = args("openai-compat", "grok");
    a.base_url = Some("https://api.x.example/v1".to_string());
    let io = FakeIo::default(); // not a TTY, no flags

    let err = run_with_io(&path, a, &io)
        .await
        .expect_err("missing key + no TTY must error");
    let msg = err.to_string();
    assert!(msg.contains("--api-key-env"), "{msg}");
    assert!(msg.contains("--api-key-stdin"), "{msg}");
    assert_eq!(
        *io.prompt_calls.lock().unwrap(),
        0,
        "no interactive prompt when stdin is not a TTY"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before, "must not write");
}

#[tokio::test]
#[serial_test::serial]
async fn interactive_hidden_prompt_captures_when_tty_and_missing() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let xdg = tempfile::tempdir().unwrap();
    let secrets = scoped_secret_dir(xdg.path());
    // No conventional var set, so the env-offer is skipped and the prompt
    // fires. Use a kind whose conventional var we can guarantee is unset.
    let var = env_var_for_kind("openai-compat").unwrap();
    let prev_var = std::env::var(var).ok();
    unset_env(var);

    let mut a = args("openai-compat", "grok");
    a.base_url = Some("https://api.x.example/v1".to_string());
    let io = FakeIo {
        is_tty: true,
        prompt_value: "prompted-key-not-real".to_string(),
        ..Default::default()
    };

    let result = run_with_io(&path, a, &io)
        .await
        .expect("interactive capture");
    assert_eq!(result, AddResult::Written);
    assert_eq!(
        *io.prompt_calls.lock().unwrap(),
        1,
        "the hidden prompt must fire"
    );
    assert_eq!(
        std::fs::read_to_string(secrets.join("grok")).unwrap(),
        "prompted-key-not-real"
    );

    restore_env(var, prev_var);
    unset_env("XDG_CONFIG_HOME");
}

#[tokio::test]
#[serial_test::serial]
async fn interactive_offers_a_resolvable_env_var_and_writes_the_env_ref() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let var = env_var_for_kind("anthropic-api").unwrap();
    let prev_var = std::env::var(var).ok();
    set_env(var, "resolvable-value-not-real");

    let a = args("anthropic-api", "claude");
    let io = FakeIo {
        is_tty: true,
        offer_env: true,
        ..Default::default()
    };

    let result = run_with_io(&path, a, &io)
        .await
        .expect("env-detect offer accepted");
    assert_eq!(result, AddResult::Written);
    assert_eq!(
        *io.prompt_calls.lock().unwrap(),
        0,
        "an accepted env offer must not fall through to the prompt"
    );

    let config = parse_config(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let entry = config.providers.get("claude").unwrap();
    assert_eq!(entry.api_key_ref(), Some(format!("env://{var}").as_str()));

    restore_env(var, prev_var);
}

#[tokio::test]
#[serial_test::serial]
async fn interactive_does_not_offer_an_unresolved_env_var() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let xdg = tempfile::tempdir().unwrap();
    let secrets = scoped_secret_dir(xdg.path());
    let var = env_var_for_kind("anthropic-api").unwrap();
    let prev_var = std::env::var(var).ok();
    unset_env(var);

    let a = args("anthropic-api", "claude");
    // offer_env=true would accept IF asked; the unresolved var must mean
    // it is never offered, so the prompt captures instead.
    let io = FakeIo {
        is_tty: true,
        offer_env: true,
        prompt_value: "fallback-prompt-key".to_string(),
        ..Default::default()
    };

    let result = run_with_io(&path, a, &io).await.expect("prompt fallback");
    assert_eq!(result, AddResult::Written);
    assert_eq!(
        *io.prompt_calls.lock().unwrap(),
        1,
        "prompt must capture instead"
    );
    assert_eq!(
        std::fs::read_to_string(secrets.join("claude")).unwrap(),
        "fallback-prompt-key"
    );

    restore_env(var, prev_var);
    unset_env("XDG_CONFIG_HOME");
}

#[tokio::test]
#[serial_test::serial]
async fn interactive_does_not_offer_an_empty_env_var() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let xdg = tempfile::tempdir().unwrap();
    let secrets = scoped_secret_dir(xdg.path());
    let var = env_var_for_kind("anthropic-api").unwrap();
    let prev_var = std::env::var(var).ok();
    // Set-but-empty must NOT satisfy "resolves non-empty NOW": no offer.
    set_env(var, "");

    let a = args("anthropic-api", "claude");
    let io = FakeIo {
        is_tty: true,
        offer_env: true,
        prompt_value: "prompt-over-empty-var".to_string(),
        ..Default::default()
    };

    let result = run_with_io(&path, a, &io).await.expect("prompt fallback");
    assert_eq!(result, AddResult::Written);
    assert_eq!(
        *io.prompt_calls.lock().unwrap(),
        1,
        "an empty env var is not offered; the prompt captures instead"
    );
    assert_eq!(
        std::fs::read_to_string(secrets.join("claude")).unwrap(),
        "prompt-over-empty-var"
    );

    restore_env(var, prev_var);
    unset_env("XDG_CONFIG_HOME");
}

#[tokio::test]
async fn forwarded_anthropic_api_adds_without_a_secret_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());

    let mut a = args("anthropic-api", "fwd");
    a.credential_source = Some("forwarded".to_string());
    let io = FakeIo::default();

    let result = run_with_io(&path, a, &io).await.expect("forwarded add");
    assert_eq!(result, AddResult::Written);
    assert_eq!(
        *io.prompt_calls.lock().unwrap(),
        0,
        "forwarded prompts for nothing"
    );

    let text = std::fs::read_to_string(&path).unwrap();
    let config = parse_config(&text).unwrap();
    let entry = config.providers.get("fwd").unwrap();
    assert_eq!(entry.kind_str(), "anthropic-api");
    assert_eq!(
        entry.api_key_ref(),
        Some(""),
        "a forwarded provider carries no configured credential"
    );
    assert!(text.contains("credential_source = \"forwarded\""), "{text}");
    assert!(
        text.contains("api.anthropic.com"),
        "base URL pinned: {text}"
    );
}

#[tokio::test]
async fn forwarded_on_a_non_anthropic_kind_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let before = std::fs::read(&path).unwrap();

    let mut a = args("openai-compat", "x");
    a.base_url = Some("https://api.x.example/v1".to_string());
    a.credential_source = Some("forwarded".to_string());
    let io = FakeIo::default();

    let err = run_with_io(&path, a, &io)
        .await
        .expect_err("forwarded is anthropic-api only");
    assert!(err.to_string().contains("anthropic-api"), "err: {err}");
    assert_eq!(std::fs::read(&path).unwrap(), before, "must not write");
}

#[tokio::test]
async fn forwarded_with_a_secret_flag_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let before = std::fs::read(&path).unwrap();

    let mut a = args("anthropic-api", "fwd");
    a.credential_source = Some("forwarded".to_string());
    a.secret_ref = Some("file:///abs/key".to_string());
    let io = FakeIo::default();

    let err = run_with_io(&path, a, &io)
        .await
        .expect_err("forwarded must not combine with a secret-source flag");
    assert!(
        err.to_string().contains("captures no credential"),
        "err: {err}"
    );
    assert_eq!(std::fs::read(&path).unwrap(), before, "must not write");
}

#[tokio::test]
async fn oauth_backed_kind_rejects_base_url_flag() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let before = std::fs::read(&path).unwrap();

    let mut a = args("anthropic", "claude-sub");
    a.base_url = Some("https://api.anthropic.com/v1".to_string());
    let io = FakeIo::default();

    let err = run_with_io(&path, a, &io)
        .await
        .expect_err("an oauth-backed kind must reject --base-url");
    assert!(err.to_string().contains("--base-url"), "err: {err}");
    assert_eq!(std::fs::read(&path).unwrap(), before, "must not write");
}

#[tokio::test]
async fn oauth_backed_kind_delegates_to_login_and_writes_the_oauth_ref() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());

    let a = args("anthropic", "claude-sub");
    let io = FakeIo::default(); // login_ok = true (already-logged-in seam)

    let result = run_with_io(&path, a, &io).await.expect("oauth add");
    assert_eq!(result, AddResult::Written);
    assert_eq!(
        *io.login_calls.lock().unwrap(),
        vec!["anthropic".to_string()],
        "the login flow must be delegated exactly once for `anthropic`"
    );

    let config = parse_config(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let entry = config.providers.get("claude-sub").unwrap();
    assert_eq!(entry.kind_str(), "anthropic-api");
    assert_eq!(entry.api_key_ref(), Some("oauth://anthropic"));
}

#[tokio::test]
async fn oauth_login_failure_aborts_before_the_config_write() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let before = std::fs::read(&path).unwrap();

    let a = args("anthropic", "claude-sub");
    let io = FakeIo {
        login_ok: false,
        ..Default::default()
    };

    let err = run_with_io(&path, a, &io)
        .await
        .expect_err("a failed login must abort");
    assert!(err.to_string().contains("login failed"), "err: {err}");
    assert_eq!(std::fs::read(&path).unwrap(), before, "must not write");
}

#[tokio::test]
#[serial_test::serial]
async fn declined_confirm_captures_no_secret_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let before = std::fs::read(&path).unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let secrets = scoped_secret_dir(xdg.path());

    let mut a = args("openai-compat", "grok");
    a.base_url = Some("https://api.x.example/v1".to_string());
    a.api_key_stdin = true;
    a.yes = false; // non-terminal stdin -> confirm declines
    let io = FakeIo {
        stdin_value: "declined-key-not-real".to_string(),
        ..Default::default()
    };

    let result = run_with_io(&path, a, &io)
        .await
        .expect("declining is not an error");
    assert_eq!(result, AddResult::Aborted);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "must not write config"
    );
    assert!(
        !secrets.join("grok").exists(),
        "a declined confirm must capture no secret file"
    );
    unset_env("XDG_CONFIG_HOME");
}

#[tokio::test]
#[serial_test::serial]
async fn post_capture_config_conflict_persists_secret_and_reports_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let xdg = tempfile::tempdir().unwrap();
    let secrets = scoped_secret_dir(xdg.path());

    // The fake rewrites config.toml the moment stdin is read -- i.e.
    // AFTER `run` snapshotted it but before the locked commit -- so the
    // commit sees a changed file and conflicts, with the capture already
    // done.
    let conflict_path = path.clone();
    let mut a = args("openai-compat", "grok");
    a.base_url = Some("https://api.x.example/v1".to_string());
    a.api_key_stdin = true;
    let io = FakeIo {
        stdin_value: "captured-then-conflict".to_string(),
        stdin_hook: Some(Box::new(move || {
            std::fs::write(&conflict_path, format!("{}# out of band\n", current_base())).unwrap();
        })),
        ..Default::default()
    };

    let err = run_with_io(&path, a, &io)
        .await
        .expect_err("a post-capture conflict must error");
    let msg = err.to_string();
    assert!(
        msg.contains("captured"),
        "recovery names the capture: {msg}"
    );
    assert!(msg.contains("re-run"), "recovery says re-run: {msg}");
    assert!(
        secrets.join("grok").exists(),
        "the captured secret must persist across a config-write conflict"
    );
    unset_env("XDG_CONFIG_HOME");
}

#[tokio::test]
#[serial_test::serial]
async fn captured_value_never_appears_in_tracing_events() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let xdg = tempfile::tempdir().unwrap();
    let _secrets = scoped_secret_dir(xdg.path());
    let secret_value = "tracing-secret-value-not-real";

    let (_res, events) = routectl_testkit::with_capture(async {
        let mut a = args("openai-compat", "grok");
        a.base_url = Some("https://api.x.example/v1".to_string());
        a.api_key_stdin = true;
        let io = FakeIo {
            stdin_value: secret_value.to_string(),
            ..Default::default()
        };
        run_with_io(&path, a, &io).await.expect("stdin capture");
    })
    .await;

    for event in &events {
        for field in ["credential_source", "value", "api_key_ref", "message"] {
            if let Some(v) = event.field(field) {
                assert!(
                    !v.contains(secret_value),
                    "the secret value leaked into tracing field `{field}`: {v}"
                );
            }
        }
    }
    // And the recorded credential class is the scheme only.
    let audit: Vec<_> = events
        .iter()
        .filter(|e| e.field("verb") == Some("provider-add"))
        .collect();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].field("credential_source"), Some("file"));
    unset_env("XDG_CONFIG_HOME");
}

// -----------------------------------------------------------------
// Canonicalize-once: the store is opened during the capture phase and
// carried through PendingSecret::File, so a symlinked ancestor swapped
// between capture and execute cannot redirect the put -- it lands under
// the ORIGINAL canonical base the precomputed ref already points at.
// -----------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
#[serial_test::serial]
async fn symlinked_ancestor_swap_between_phases_lands_at_original_base() {
    // Arrange: XDG_CONFIG_HOME points through a symlink at `real_a`, so
    // the store's default dir resolves under `real_a`.
    let root = tempfile::tempdir().unwrap();
    let real_a = root.path().join("real-a");
    let real_b = root.path().join("real-b");
    std::fs::create_dir_all(&real_a).unwrap();
    std::fs::create_dir_all(&real_b).unwrap();
    let link = root.path().join("xdg-link");
    std::os::unix::fs::symlink(&real_a, &link).unwrap();
    set_env("XDG_CONFIG_HOME", link.to_str().unwrap());

    let a = args("openai-compat", "grok");

    // Act (capture phase): opens the store, canonicalizes through the
    // symlink, and computes the ref off the resolved base.
    let (ref_str, class, pending) =
        capture_value(&a, "value-not-real".to_string()).expect("capture");
    assert_eq!(class, "file");
    let expected = std::fs::canonicalize(&real_a)
        .unwrap()
        .join("routectl")
        .join("secrets")
        .join("grok");
    assert_eq!(
        ref_str,
        SecretRef::File(expected.clone()).to_string(),
        "the precomputed ref points at the original canonical base"
    );

    // Swap the symlinked ancestor to `real_b` BETWEEN the two phases.
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&real_b, &link).unwrap();

    // Act (execute phase): the put reuses the store canonicalized at
    // capture time, not the freshly-swapped symlink target.
    execute_pending(pending, &FakeIo::default())
        .await
        .expect("put");

    // Assert: the secret landed under the ORIGINAL canonical base,
    // matching the precomputed ref -- never under the swapped target.
    assert_eq!(
        std::fs::read_to_string(&expected).unwrap(),
        "value-not-real",
        "put must land at the precomputed ref path"
    );
    let swapped = std::fs::canonicalize(&real_b)
        .unwrap()
        .join("routectl")
        .join("secrets")
        .join("grok");
    assert!(
        !swapped.exists(),
        "the swapped symlink target must never receive the secret"
    );
    unset_env("XDG_CONFIG_HOME");
}

// -----------------------------------------------------------------
// File-backed credential rotation: a fresh piped key on an existing
// provider rewrites the managed secret even though the config block
// (a name-derived `file://` ref) is byte-identical. Non-file pendings
// keep their idempotent no-op.
// -----------------------------------------------------------------

/// Add a file-backed provider named `grok` with `value` as its piped key,
/// asserting the initial add wrote. Returns the config bytes on disk after
/// the add so a later rotation can prove config.toml is untouched.
async fn seed_file_backed_grok(path: &std::path::Path, value: &str) -> Vec<u8> {
    let mut a = args("openai-compat", "grok");
    a.base_url = Some("https://api.x.example/v1".to_string());
    a.api_key_stdin = true;
    let io = FakeIo {
        stdin_value: format!("{value}\n"),
        ..Default::default()
    };
    assert_eq!(
        run_with_io(path, a, &io).await.expect("initial add"),
        AddResult::Written
    );
    std::fs::read(path).unwrap()
}

#[tokio::test]
#[serial_test::serial]
async fn fresh_piped_key_rewrites_secret_and_reports_rotated() {
    // Arrange: an existing file-backed provider whose managed secret holds
    // the original key.
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let xdg = tempfile::tempdir().unwrap();
    let secrets = scoped_secret_dir(xdg.path());
    let config_before = seed_file_backed_grok(&path, "original-key-not-real").await;
    assert_eq!(
        std::fs::read_to_string(secrets.join("grok")).unwrap(),
        "original-key-not-real"
    );

    // Act: re-add the SAME provider with a NEW piped key + --overwrite. The
    // block normalizes byte-identically (the ref is name-derived), so
    // config.toml is untouched -- but the managed secret must rotate.
    let mut rotate = args("openai-compat", "grok");
    rotate.base_url = Some("https://api.x.example/v1".to_string());
    rotate.api_key_stdin = true;
    rotate.overwrite = true;
    let io = FakeIo {
        stdin_value: "rotated-key-not-real\n".to_string(),
        ..Default::default()
    };
    let result = run_with_io(&path, rotate, &io)
        .await
        .expect("rotation must succeed");

    // Assert: the outcome is Rotated, the secret now holds the NEW value at
    // 0600, and config.toml is byte-identical to before the rotation.
    assert_eq!(result, AddResult::Rotated);
    assert_eq!(
        std::fs::read_to_string(secrets.join("grok")).unwrap(),
        "rotated-key-not-real",
        "the managed secret file must hold the rotated value"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        config_before,
        "a rotation must leave config.toml byte-identical"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(secrets.join("grok"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "the rotated secret must stay 0600");
    }
    unset_env("XDG_CONFIG_HOME");
}

#[tokio::test]
#[serial_test::serial]
async fn fresh_piped_key_on_existing_provider_requires_overwrite() {
    // Arrange: an existing file-backed provider.
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let xdg = tempfile::tempdir().unwrap();
    let secrets = scoped_secret_dir(xdg.path());
    let config_before = seed_file_backed_grok(&path, "original-key-not-real").await;

    // Act: a fresh capture WITHOUT --overwrite on the existing provider.
    let mut rotate = args("openai-compat", "grok");
    rotate.base_url = Some("https://api.x.example/v1".to_string());
    rotate.api_key_stdin = true;
    let io = FakeIo {
        stdin_value: "would-rotate-not-real\n".to_string(),
        ..Default::default()
    };
    let err = run_with_io(&path, rotate, &io)
        .await
        .expect_err("a fresh capture on an existing provider must require --overwrite");

    // Assert: the error names the flag; the secret and config are untouched.
    assert!(err.to_string().contains("--overwrite"), "err: {err}");
    assert_eq!(
        std::fs::read_to_string(secrets.join("grok")).unwrap(),
        "original-key-not-real",
        "a refused rotation must not touch the existing secret"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        config_before,
        "a refused rotation must not write config"
    );
    unset_env("XDG_CONFIG_HOME");
}

#[tokio::test]
#[serial_test::serial]
async fn env_ref_identical_re_add_stays_no_change() {
    // A non-file pending (env ref) that normalizes to the identical block
    // stays an idempotent no-op: the rotation change must not regress
    // re-init safety for flag-driven refs.
    let key = "ROUTECTL_PROVIDER_ADD_IDEMPOTENT_ENV";
    set_env(key, "present-value-not-real");
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());

    let mut first = args("anthropic-api", "claude");
    first.api_key_env = Some(key.to_string());
    assert_eq!(
        run(&path, first).await.expect("first add"),
        AddResult::Written
    );
    let after_first = std::fs::read(&path).unwrap();

    let mut second = args("anthropic-api", "claude");
    second.api_key_env = Some(key.to_string());
    assert_eq!(
        run(&path, second).await.expect("re-add"),
        AddResult::NoChange,
        "an identical env-ref re-add stays a no-op"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        after_first,
        "an identical env-ref re-add must leave config byte-identical"
    );
    unset_env(key);
}

#[cfg(unix)]
#[tokio::test]
#[serial_test::serial]
async fn rotation_secret_write_failure_leaves_old_secret_intact() {
    use std::os::unix::fs::PermissionsExt;

    // Arrange: an existing file-backed provider whose secret holds a known
    // value.
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let xdg = tempfile::tempdir().unwrap();
    let secrets = scoped_secret_dir(xdg.path());
    let config_before = seed_file_backed_grok(&path, "durable-old-key-not-real").await;

    // Make the store directory read-only so a fresh capture cannot write.
    std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o500)).unwrap();
    // If this environment can still create files in a 0500 dir (e.g. as
    // root), the capture would not fail -- restore perms and skip rather
    // than assert a guarantee the environment does not provide.
    if std::fs::File::create(secrets.join(".probe")).is_ok() {
        let _ = std::fs::remove_file(secrets.join(".probe"));
        std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o700)).unwrap();
        unset_env("XDG_CONFIG_HOME");
        return;
    }

    // Act: attempt a rotation with --overwrite; the capture write must fail.
    let mut rotate = args("openai-compat", "grok");
    rotate.base_url = Some("https://api.x.example/v1".to_string());
    rotate.api_key_stdin = true;
    rotate.overwrite = true;
    let io = FakeIo {
        stdin_value: "never-lands-not-real\n".to_string(),
        ..Default::default()
    };
    let result = run_with_io(&path, rotate, &io).await;

    // Restore perms before asserting so the tempdir can be read/cleaned.
    std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o700)).unwrap();

    // Assert: the rotation errors, the OLD secret is intact, and config is
    // untouched -- a failed rotation never destroys the live credential.
    assert!(
        result.is_err(),
        "an unwritable store must fail the rotation"
    );
    assert_eq!(
        std::fs::read_to_string(secrets.join("grok")).unwrap(),
        "durable-old-key-not-real",
        "a failed rotation must leave the old secret intact"
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        config_before,
        "a failed rotation must not write config"
    );
    unset_env("XDG_CONFIG_HOME");
}

#[tokio::test]
#[serial_test::serial]
async fn rotation_emits_one_audit_event_config_unchanged_without_value_or_ref() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let xdg = tempfile::tempdir().unwrap();
    let secrets = scoped_secret_dir(xdg.path());
    let secret_value = "rotation-audit-secret-not-real";
    // Seed the existing provider OUTSIDE the capture so only the rotation's
    // event is observed.
    seed_file_backed_grok(&path, "seed-key-not-real").await;

    let (_res, events) = routectl_testkit::with_capture(async {
        let mut rotate = args("openai-compat", "grok");
        rotate.base_url = Some("https://api.x.example/v1".to_string());
        rotate.api_key_stdin = true;
        rotate.overwrite = true;
        let io = FakeIo {
            stdin_value: secret_value.to_string(),
            ..Default::default()
        };
        assert_eq!(
            run_with_io(&path, rotate, &io).await.expect("rotation"),
            AddResult::Rotated
        );
    })
    .await;

    let audit: Vec<_> = events
        .iter()
        .filter(|e| e.field("surface") == Some("cli") && e.field("verb") == Some("provider-add"))
        .collect();
    assert_eq!(audit.len(), 1, "exactly one rotation audit event expected");

    let event = audit[0];
    assert_eq!(event.field("name"), Some("grok"));
    assert_eq!(event.field("kind"), Some("openai-compat"));
    assert_eq!(
        event.field("credential_source"),
        Some("file"),
        "a rotation captures a managed file secret"
    );
    assert_eq!(
        event.field("config_changed"),
        Some("false"),
        "a rotation leaves config.toml unchanged"
    );
    assert!(
        event.field("value").is_none(),
        "the value must never be audited"
    );
    assert!(
        event.field("api_key_ref").is_none(),
        "the full ref must never be audited"
    );
    // The captured value must not leak into any event's message or fields.
    for e in &events {
        assert!(
            !e.message.contains(secret_value),
            "secret leaked into a message: {}",
            e.message
        );
        for (k, v) in &e.fields {
            assert!(
                !v.contains(secret_value),
                "secret leaked into field `{k}`: {v}"
            );
        }
    }
    // Sanity: the secret really did rotate on disk.
    assert_eq!(
        std::fs::read_to_string(secrets.join("grok")).unwrap(),
        secret_value
    );
    unset_env("XDG_CONFIG_HOME");
}

#[test]
fn rotation_reports_the_pinned_operator_message() {
    // The rotation line is an operator-scriptable contract: pin the exact
    // literal so a downstream `grep` never breaks silently.
    assert_eq!(ROTATED_MESSAGE, "credential rotated; config unchanged");
}

// -----------------------------------------------------------------
// Post-add capability-probe offer: a forced probe against an unreachable
// lane fails, and the just-committed provider block must survive on disk --
// the probe writes only to the capability ledger and never rolls back the
// add. The failing probe also mints no capability events.
// -----------------------------------------------------------------

#[tokio::test]
#[serial_test::serial]
async fn failing_post_add_probe_leaves_the_provider_block_intact() {
    let dir = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let _secrets = scoped_secret_dir(xdg.path());

    // A migrated ledger so the probe's read-write open succeeds and the run
    // reaches an actual dispatch (which then fails on the unreachable lane).
    let db_path = dir.path().join("usage.db");
    drop(routectl_usage::open(&db_path).expect("create migrated ledger"));

    let key = "ROUTECTL_PROVIDER_ADD_PROBE_ENV_KEY";
    set_env(key, "probe-key-not-real");

    // A model already routes to `grok`, so the probe can scope. `grok` starts
    // at one unreachable base; the add rewrites it to a different unreachable
    // base, so the write is a real `Written` outcome.
    let body = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\n\
         [server]\nhost = \"127.0.0.1\"\nport = 8787\n\n\
         [usage]\ndb_path = \"{}\"\n\n\
         [providers.grok]\nkind = \"openai-compat\"\n\
         base_url = \"http://127.0.0.1:2\"\napi_key_ref = \"literal:test-key\"\n\n\
         [models.gpt]\nprovider = \"grok\"\nupstream = \"grok-2\"\n\n\
         [aliases]\ndefault = \"gpt\"\n",
        db_path.display()
    );
    let path = write_config(dir.path(), &body);

    let mut a = args("openai-compat", "grok");
    a.base_url = Some("http://127.0.0.1:1".to_string());
    a.secret_ref = Some(format!("env://{key}"));
    a.overwrite = true;
    a.probe = Some(true); // force the probe non-interactively

    let result = run(&path, a)
        .await
        .expect("the add succeeds even though the probe fails");
    assert_eq!(result, AddResult::Written);

    // The provider block persists with the new base URL: a failing probe never
    // rolls the committed add back.
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("[providers.grok]"), "{text}");
    assert!(
        text.contains("base_url = \"http://127.0.0.1:1\""),
        "the rewritten provider block must persist: {text}"
    );

    // A failing probe writes no capability events.
    let db = routectl_usage::open_rw(&db_path).expect("reopen ledger");
    let events =
        routectl_usage::read_capability_events_after(db.conn(), 0, 100).expect("read events");
    assert!(
        events.is_empty(),
        "a failing probe must mint no capability events"
    );

    unset_env(key);
    unset_env("XDG_CONFIG_HOME");
}

fn restore_env(key: &str, prev: Option<String>) {
    match prev {
        Some(v) => set_env(key, &v),
        None => unset_env(key),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn post_add_probe_offer_skips_when_no_model_routes_to_the_provider() {
    // probe = None means the offer is consulted, but a freshly added provider
    // that no model routes to yet resolves to no lane, so the offer is silently
    // skipped: the add succeeds and the confirm seam is never consulted (a
    // consent of `true` here would still not dispatch).
    let dir = tempfile::tempdir().unwrap();
    let xdg = tempfile::tempdir().unwrap();
    let _secrets = scoped_secret_dir(xdg.path());
    let path = write_config(dir.path(), &current_base());

    let mut a = args("openai-compat", "grok");
    a.base_url = Some("https://api.x.example/v1".to_string());
    a.secret_ref = Some("file:///abs/key".to_string());
    a.probe = None;
    let io = FakeIo {
        confirm_probe: true,
        ..Default::default()
    };

    let result = run_with_io(&path, a, &io).await.expect("add");
    assert_eq!(result, AddResult::Written);
    // V3_BASE routes `gpt` -> `fast`, so `grok` has no lane to probe.
    let config = parse_config(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(config.providers.contains_key("grok"));
    unset_env("XDG_CONFIG_HOME");
}

// -----------------------------------------------------------------
// The oauth arm writes exactly ONE block, because the delegated login
// touches no config file. Nothing about this has a compile signal: the
// defect it guards surfaces only as a confusing snapshot conflict.
// -----------------------------------------------------------------

/// The MECHANISM: this command's snapshot is captured before the login and
/// its commit compares against those exact bytes, so a login that writes
/// config in between makes the add conflict. Driven here by a stub login
/// that writes -- which is precisely what the auto-surface would do if the
/// production seam passed anything but `ConfigSurface::Skip`.
#[tokio::test]
async fn a_config_writing_login_would_make_the_oauth_arm_conflict_on_its_own_snapshot() {
    // Arrange: the stub login appends a byte to config.toml, standing in
    // for the login auto-surface committing its own delta.
    let dir = tempfile::tempdir().unwrap();
    let path = write_config(dir.path(), &current_base());
    let racing_path = path.clone();
    let io = FakeIo {
        login_hook: Some(Box::new(move || {
            let mut text = std::fs::read_to_string(&racing_path).unwrap();
            text.push_str("\n# a config-writing login landed here\n");
            std::fs::write(&racing_path, text).unwrap();
        })),
        ..Default::default()
    };

    // Act
    let err = run_with_io(&path, args("anthropic", "claude-sub"), &io)
        .await
        .expect_err("a login that writes config invalidates this command's snapshot");

    // Assert: the conflict, reported through the post-side-effect recovery
    // wording rather than a bare failure.
    let msg = err.to_string();
    assert!(
        msg.contains("changed on disk"),
        "expected a snapshot conflict, got: {msg}"
    );
    // And the entry the add existed to write is absent: one command, one
    // write, and this one wrote nothing.
    let config = parse_config(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert!(
        !config.providers.contains_key("claude-sub"),
        "the conflicting add must write no block"
    );
}

/// The WIRING: the production seam must pass `Skip`. There is no compile
/// signal for this (both variants type-check), and no test can drive the
/// real oauth flow, so the guard is lexical over this module's own
/// production source -- the one place `login::run` is called from here.
///
/// The whole file is scanned deliberately, with no `#[cfg(test)]` cut: this
/// module's tests are a SIDECAR (`#[path = "mod_tests.rs"] mod tests;`), so
/// none of their text is in `mod.rs` at all, while cutting on the first
/// `#[cfg(test)]` would truncate at that declaration -- which sits ABOVE
/// `RealAddIo` and would leave the guard scanning nothing that matters.
#[test]
fn the_production_login_seam_never_arms_the_config_auto_surface() {
    let production = include_str!("mod.rs");

    assert!(
        production.contains("ConfigSurface::Skip"),
        "RealAddIo::login must pass ConfigSurface::Skip"
    );
    assert!(
        !production.contains("ConfigSurface::Auto"),
        "arming the auto-surface here makes every oauth `provider add` conflict on its \
         own byte snapshot"
    );
}
