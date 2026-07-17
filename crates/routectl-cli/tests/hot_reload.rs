//! Integration tests for the file-watch + SIGHUP hot-reload path.
//!
//! Each test boots a server via `serve_on_listener` against a
//! `tempfile::tempdir`-rooted config.toml and credentials.json so
//! the operator's real `$XDG_CONFIG_HOME` is never touched.
//! Polling-based timing (50 ms intervals up to ~3 s) keeps the
//! suite fast and platform-jitter tolerant; no fixed sleeps.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use routectl_router::Config;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use wiremock::matchers::{header, method, path as wiremock_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

/// Upper bound for polling the reloaded `/v1/models` state after a
/// config rewrite. The reload runs on a spawned server task, so its
/// tracing signal is not observable from the test thread; the observable
/// is the alias table itself, polled at 50 ms until it flips. The bound
/// is generous so a busy CI box under parallel release load cannot flake
/// -- `wait_for_alias` returns the instant the alias appears, so the
/// happy path pays only the real reload latency, never the ceiling.
const RELOAD_WAIT_CEILING: Duration = Duration::from_secs(30);

// -----------------------------------------------------------------
// Test rig
// -----------------------------------------------------------------

/// Single-provider config seeded into the test config.toml. The
/// upstream base is bogus (port 1) -- tests assert on `/v1/models`
/// and `/health`, which never dispatch upstream.
fn config_text_with_alias(alias: &str) -> String {
    let key_ref = common::file_ref("test-key");
    format!(
        r#"
version = 3
[server]
host = "127.0.0.1"
port = 0
strict_translation = false

[providers.fast]
kind = "openai-compat"
base_url = "http://127.0.0.1:1"
api_key_ref = "{key_ref}"

[models.gpt-4o]
provider = "fast"
upstream = "gpt-4o"

[aliases]
{alias} = "gpt-4o"
"#
    )
}

/// Parse the config text so the call site still has an in-memory
/// `Arc<Config>` to hand `serve_on_listener`. Mirrors the production
/// path: read -> toml::from_str -> Arc.
fn parse_config(text: &str) -> Arc<Config> {
    let cfg: Config = toml::from_str(text).expect("test config must parse");
    Arc::new(cfg)
}

/// Boot a server on `127.0.0.1:0`, register `config_path` as the
/// watch target, and return `(base_url, config_path)`. Spawns the
/// server on a background task; tests do not own the join handle
/// (the runtime drops it on exit).
async fn spawn_watched_server(initial_alias: &str) -> (String, PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, config_text_with_alias(initial_alias)).unwrap();

    let config = parse_config(&config_text_with_alias(initial_alias));
    let config = common::isolate_usage_db(config);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");

    let watched_path = config_path.clone();
    tokio::spawn(async move {
        let _ = routectl_cli::server::serve_on_listener(config, listener, Some(watched_path)).await;
    });

    // Tight poll loop: wait for /health to come up before returning.
    let client = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(resp) = client.get(format!("{base_url}/health")).send().await
            && resp.status().is_success()
        {
            return (base_url, config_path, dir);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("test server failed to come up at {base_url}");
}

/// Fetch `/v1/models` and return the sorted list of `id` fields.
/// Used to observe alias-table changes after a hot-reload.
async fn list_model_ids(base_url: &str) -> Vec<String> {
    let resp = reqwest::get(format!("{base_url}/v1/models")).await.unwrap();
    assert!(resp.status().is_success(), "GET /v1/models failed");
    let body: Value = resp.json().await.unwrap();
    let mut ids: Vec<String> = body["data"]
        .as_array()
        .expect("data is an array")
        .iter()
        .map(|e| e["id"].as_str().expect("id is a string").to_string())
        .collect();
    ids.sort();
    ids
}

/// Atomic-rename write: mirrors the production write pattern in
/// `routectl-auth/src/oauth/file_io.rs`. Operators using `vim` with
/// `:set backupcopy=no` or any editor that uses tempfile + rename
/// hit this code path.
fn write_atomic(path: &Path, contents: &[u8]) {
    let parent = path.parent().expect("config path has a parent");
    let tmp = tempfile::Builder::new()
        .prefix(".config.tmp.")
        .suffix(".toml")
        .tempfile_in(parent)
        .unwrap();
    std::fs::write(tmp.path(), contents).unwrap();
    tmp.persist(path).unwrap();
}

/// Poll `/v1/models` until `id` appears (or `max_wait` elapses).
/// Returns true when found, false on timeout. 50 ms polling cadence
/// keeps the test fast.
async fn wait_for_alias(base_url: &str, id: &str, max_wait: Duration) -> bool {
    let deadline = Instant::now() + max_wait;
    while Instant::now() < deadline {
        let ids = list_model_ids(base_url).await;
        if ids.iter().any(|s| s == id) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Cadence at which `poll_alias_with_restimulus` re-issues its rewrite.
/// Comfortably wider than `DEBOUNCE_MS` (250 ms in file_watch.rs) so each
/// re-write is seen as a distinct event once the watch is armed, and much
/// narrower than `RELOAD_WAIT_CEILING` so a lost first event is re-delivered
/// many times before the ceiling expires.
const RESTIMULUS_INTERVAL: Duration = Duration::from_secs(1);

/// Poll `/v1/models` for `id` while periodically re-issuing an atomic
/// rewrite of `config_path` with `contents`, until the alias surfaces
/// (returns true) or `max_wait` elapses (returns false).
///
/// The watcher's inotify watch is armed on notify's background
/// event-loop thread AFTER `spawn_watcher` returns -- `debouncer.watch()`
/// only enqueues the `add_watch`, it does not block on it (this is why the
/// file_watch.rs unit tests interpose a `SETTLE_MS` sleep before their
/// first mutation). The server answers `/health` before that watch is
/// live, so a single write issued the instant the server comes up can land
/// ahead of the armed watch and be dropped -- there is no fs-event
/// re-delivery, so the reload simply never fires. Under heavy parallel
/// release load that arming window widens past any fixed sleep.
///
/// Re-writing the SAME contents each interval makes the stimulus
/// self-healing without changing observed behavior: the reload path is
/// idempotent (read -> parse -> Arc swap regardless of a self-write
/// filter), so identical contents always yield the identical reloaded
/// alias table. A re-delivered event is a no-op swap; the first event the
/// armed watch actually sees flips the alias.
async fn poll_alias_with_restimulus(
    base_url: &str,
    config_path: &Path,
    contents: &[u8],
    id: &str,
    max_wait: Duration,
) -> bool {
    let deadline = Instant::now() + max_wait;
    write_atomic(config_path, contents);
    let mut last_write = Instant::now();
    while Instant::now() < deadline {
        if list_model_ids(base_url).await.iter().any(|s| s == id) {
            return true;
        }
        if last_write.elapsed() >= RESTIMULUS_INTERVAL {
            write_atomic(config_path, contents);
            last_write = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

// -----------------------------------------------------------------
// Tests
// -----------------------------------------------------------------

/// Writing a new config.toml triggers a hot-reload; the new alias
/// surfaces on `/v1/models`. The stimulus is re-issued on each poll
/// iteration (see `poll_alias_with_restimulus`) because the watcher's
/// inotify watch arms asynchronously after the server is already
/// answering /health -- a single write can race ahead of the armed
/// watch and be dropped with no fs-event re-delivery.
#[tokio::test]
async fn file_write_triggers_config_reload() {
    let (base_url, config_path, _dir) = spawn_watched_server("first-alias").await;

    // Sanity: initial alias visible.
    let initial = list_model_ids(&base_url).await;
    assert!(initial.iter().any(|s| s == "first-alias"));

    // Act + Assert: rewrite with a different alias, re-issuing the
    // (idempotent) rewrite until it surfaces or the ceiling expires.
    assert!(
        poll_alias_with_restimulus(
            &base_url,
            &config_path,
            config_text_with_alias("second-alias").as_bytes(),
            "second-alias",
            RELOAD_WAIT_CEILING,
        )
        .await,
        "second-alias did not appear within {RELOAD_WAIT_CEILING:?} after config reload"
    );
    let ids = list_model_ids(&base_url).await;
    assert!(
        !ids.iter().any(|s| s == "first-alias"),
        "old alias still present after reload: {ids:?}"
    );
}

/// A truncated / structurally-broken TOML rewrite must NOT swap
/// the running config. The existing alias stays live; the broken
/// content is rejected at parse time.
#[tokio::test]
async fn partial_write_keeps_old_config() {
    let (base_url, config_path, _dir) = spawn_watched_server("stable-alias").await;

    // Act: write a truncated TOML (missing closing brace, dangling
    // section header).
    write_atomic(&config_path, b"[server\nhost = \"127.0.0.1\"\nport = 0\n");

    // Give the watcher + reload coordinator time to react. Even
    // after debounce + parse, the old alias must still be present.
    tokio::time::sleep(Duration::from_millis(800)).await;

    let ids = list_model_ids(&base_url).await;
    assert!(
        ids.iter().any(|s| s == "stable-alias"),
        "old alias must remain after a parse-rejected reload: {ids:?}"
    );
}

/// Valid-UTF8 but non-TOML content also must be rejected, with the
/// existing alias preserved. Same surface as `partial_write_keeps_old_config`
/// but exercises the toml::from_str error path with content the
/// reader cannot recover from.
#[tokio::test]
async fn invalid_toml_keeps_old_config() {
    let (base_url, config_path, _dir) = spawn_watched_server("durable-alias").await;

    // Act: garbage that parses as utf-8 but not as TOML.
    write_atomic(&config_path, b"<<<not toml at all>>>\n!@#$%^\n");

    tokio::time::sleep(Duration::from_millis(800)).await;

    let ids = list_model_ids(&base_url).await;
    assert!(
        ids.iter().any(|s| s == "durable-alias"),
        "old alias must remain after an invalid-toml reload attempt: {ids:?}"
    );
}

/// A config text identical to `config_text_with_alias` except it also
/// carries `[retry.classes.feature-unsupported]`, a reserved class that
/// `routectl_router::validate_class_policy` hard-rejects. Syntactically
/// valid TOML -- the rejection is semantic, caught by the startup
/// validators `load_effective_config` runs after parsing.
fn config_text_with_reserved_class_override(alias: &str) -> String {
    let key_ref = common::file_ref("test-key");
    format!(
        r#"
version = 3
[server]
host = "127.0.0.1"
port = 0
strict_translation = false

[retry.classes.feature-unsupported]
retry = 1

[providers.fast]
kind = "openai-compat"
base_url = "http://127.0.0.1:1"
api_key_ref = "{key_ref}"

[models.gpt-4o]
provider = "fast"
upstream = "gpt-4o"

[aliases]
{alias} = "gpt-4o"
"#
    )
}

/// `[retry.classes.feature-unsupported]` is rejected directly at the
/// `load_effective_config` seam -- the same loader the hot-reload path
/// and cold start both run through. Exercising this seam directly
/// (rather than only through the fs-watch + poll path) pins the exact
/// validator that rejects, without depending on watcher timing.
#[test]
fn class_policy_reject_surfaces_through_load_effective_config() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        config_text_with_reserved_class_override("reject-me"),
    )
    .unwrap();

    let err = match routectl_cli::server::load_effective_config(&config_path) {
        Ok(_) => panic!("[retry.classes.feature-unsupported] must be rejected"),
        Err(e) => e,
    };
    assert!(
        err.contains("feature-unsupported") && err.contains("reserved"),
        "error must name the reserved class, got: {err}"
    );
}

/// End-to-end companion to the seam test above: writing a reload
/// candidate carrying `[retry.classes.feature-unsupported]` at the
/// running server's watched config path must NOT swap the live router
/// -- the old alias stays visible on `/v1/models` (and the new
/// candidate's alias never surfaces) exactly like the parse-rejected
/// cases above (`partial_write_keeps_old_config`,
/// `invalid_toml_keeps_old_config`), because `validate_class_policy`
/// runs inside the same `load_effective_config` those reject at. The
/// candidate names a DIFFERENT alias than the running config so a
/// wrongly-applied reload is distinguishable from a correctly-rejected
/// one (both configs are otherwise syntactically valid).
#[tokio::test]
async fn feature_unsupported_class_override_rejected_old_router_stays_live() {
    let (base_url, config_path, _dir) = spawn_watched_server("class-policy-stable").await;

    write_atomic(
        &config_path,
        config_text_with_reserved_class_override("class-policy-rejected").as_bytes(),
    );

    tokio::time::sleep(Duration::from_millis(800)).await;

    let ids = list_model_ids(&base_url).await;
    assert!(
        ids.iter().any(|s| s == "class-policy-stable"),
        "old alias must remain after a class-policy-rejected reload: {ids:?}"
    );
    assert!(
        !ids.iter().any(|s| s == "class-policy-rejected"),
        "the rejected candidate's alias must never surface: {ids:?}"
    );
}

// -----------------------------------------------------------------
// did-you-mean parse-error enhancer on the reload path
// -----------------------------------------------------------------

/// Same base config as `config_text_with_alias` but with an unknown
/// `prt` key inside `[server]` -- a typo of `port`. Structurally valid
/// TOML that `deny_unknown_fields` rejects at parse time, so the
/// enhancer (`routectl_router::parse_config`) appends a `did you mean
/// `port`?` hint to serde's `unknown field` message.
fn config_text_with_unknown_server_field(alias: &str) -> String {
    let key_ref = common::file_ref("test-key");
    format!(
        r#"
version = 3
[server]
host = "127.0.0.1"
port = 0
strict_translation = false
prt = 8080

[providers.fast]
kind = "openai-compat"
base_url = "http://127.0.0.1:1"
api_key_ref = "{key_ref}"

[models.gpt-4o]
provider = "fast"
upstream = "gpt-4o"

[aliases]
{alias} = "gpt-4o"
"#
    )
}

/// The enhanced parse error flows through the SAME `load_effective_config`
/// seam the reload path rejects at: an unknown `[server]` field surfaces
/// serde's `unknown field` message PLUS the `did you mean` hint. Exercising
/// the seam directly pins the enhanced message without depending on watcher
/// timing (companion to the end-to-end test below).
#[test]
fn unknown_field_suggestion_surfaces_through_load_effective_config() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        config_text_with_unknown_server_field("unused"),
    )
    .unwrap();

    let err = match routectl_cli::server::load_effective_config(&config_path) {
        Ok(_) => panic!("an unknown field must be rejected at load"),
        Err(e) => e,
    };
    assert!(
        err.contains("unknown field `prt`") && err.contains("did you mean `port`?"),
        "load error must carry the enhanced did-you-mean message, got: {err}"
    );
}

/// End-to-end companion: writing an unknown-field config at the running
/// server's watched path must NOT swap the live router -- the reload is
/// rejected at parse time (the loader logs a structured warn and keeps the
/// prior config, exactly like `partial_write_keeps_old_config` and the
/// class-policy reject above). The old alias stays visible on `/v1/models`
/// and the candidate's alias never surfaces.
#[tokio::test]
async fn unknown_field_reload_rejected_old_router_stays_live() {
    let (base_url, config_path, _dir) = spawn_watched_server("field-stable").await;

    write_atomic(
        &config_path,
        config_text_with_unknown_server_field("field-rejected").as_bytes(),
    );

    tokio::time::sleep(Duration::from_millis(800)).await;

    let ids = list_model_ids(&base_url).await;
    assert!(
        ids.iter().any(|s| s == "field-stable"),
        "old alias must remain after an unknown-field-rejected reload: {ids:?}"
    );
    assert!(
        !ids.iter().any(|s| s == "field-rejected"),
        "the rejected candidate's alias must never surface: {ids:?}"
    );
}

/// Five sequential rewrites of the config (each with a unique
/// alias) all complete without dead-locking. The final alias is
/// what `/v1/models` returns. Pins the no-filter design's
/// idempotency: each self-triggered reload is at most one
/// extra parse + Arc swap, never an infinite loop.
#[tokio::test]
async fn concurrent_self_write_no_loop() {
    let (base_url, config_path, _dir) = spawn_watched_server("alias-a").await;

    // Act: five sequential rewrites, each forcing a reload.
    for n in 0..5 {
        let alias = format!("alias-{n}");
        write_atomic(&config_path, config_text_with_alias(&alias).as_bytes());
        // Brief settle so the debouncer doesn't coalesce ALL of
        // them into a single reload (which would defeat the
        // "five distinct reloads" invariant).
        tokio::time::sleep(Duration::from_millis(350)).await;
    }

    // Assert: the final alias is visible.
    assert!(
        wait_for_alias(&base_url, "alias-4", Duration::from_secs(5)).await,
        "final alias-4 did not appear within 5s after five sequential reloads"
    );
}

/// SIGHUP combined-path coverage: rewriting `config.toml` and then
/// sending SIGHUP must surface the new alias. Either path (file-watch
/// arm OR SIGHUP arm) can satisfy this assertion -- both fan into the
/// same coordinator and converge on the same Arc swap. SIGHUP-only
/// isolation lives in the unit test
/// `server::tests::sighup_listener_emits_paired_reload_requests_in_isolation`,
/// which drives `run_sighup_listener` against a bare channel with no
/// watcher in the picture. This integration-level test pins the
/// end-to-end happy path of the combined arm.
#[cfg(unix)]
#[tokio::test]
async fn sighup_combined_with_file_rewrite_surfaces_new_config() {
    let (base_url, config_path, _dir) = spawn_watched_server("sighup-pre").await;

    // The fs-event arm may also pick this rewrite up; that is by
    // design (both arms converge on the same coordinator). The
    // SIGHUP-only contract is covered separately by the unit test
    // referenced in the doc comment above.
    write_atomic(
        &config_path,
        config_text_with_alias("sighup-post").as_bytes(),
    );

    // Send SIGHUP to ourselves.
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    kill(Pid::from_raw(std::process::id() as i32), Signal::SIGHUP).expect("send SIGHUP");

    // Assert: the new alias surfaces.
    assert!(
        wait_for_alias(&base_url, "sighup-post", Duration::from_secs(3)).await,
        "sighup-post alias did not appear within 3s after SIGHUP"
    );
}

// -----------------------------------------------------------------
// Credentials hot-reload (oauth:// path)
// -----------------------------------------------------------------

/// RAII guard for `std::env::{set,remove}_var` mutations within tests.
/// Restores the original value (or absence) on `Drop` so a panicking
/// `assert!` cannot leak modified env into sibling tests. Pair every
/// guard binding with `let _xdg = EnvGuard::set(..)`.
struct EnvGuard {
    key: &'static str,
    prev: Option<std::ffi::OsString>,
}
impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        let prev = std::env::var_os(key);
        // TODO: Audit that the environment access only happens in single-threaded code.
        unsafe { std::env::set_var(key, value) };
        Self { key, prev }
    }
}
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.prev.take() {
            // TODO: Audit that the environment access only happens in single-threaded code.
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            // TODO: Audit that the environment access only happens in single-threaded code.
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

/// Atomic-rename write of `credentials.json` matching the production
/// flow in `routectl-auth/src/oauth/file_io.rs::update_under_lock`: write a tempfile
/// in the same parent dir, set 0o600 perms, rename onto the target.
/// `OAuthStore::open` enforces 0o600 hygiene on Unix; without it the
/// reload-from-disk path rejects the file.
fn write_credentials_atomic(creds_path: &Path, access_token: &str) {
    use std::time::{SystemTime, UNIX_EPOCH};

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let creds_json = json!({
        "schema_version": 1,
        "providers": {
            "anthropic": {
                "access_token": access_token,
                "refresh_token": "seeded-refresh-token",
                "token_type": "Bearer",
                "expires_at_unix": now + 3600,
                "scopes": ["user:profile", "user:inference"],
                "account": { "email": null, "account_id": null },
                "obtained_at_unix": now
            }
        }
    });
    let bytes = serde_json::to_vec_pretty(&creds_json).expect("serialize creds");
    let parent = creds_path.parent().expect("creds path has parent");
    std::fs::create_dir_all(parent).expect("mkdir creds parent");

    let tmp = tempfile::Builder::new()
        .prefix(".credentials.tmp.")
        .suffix(".json")
        .tempfile_in(parent)
        .unwrap();
    std::fs::write(tmp.path(), &bytes).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o600))
            .expect("chmod 0600");
    }
    tmp.persist(creds_path).expect("persist creds");
}

/// Spawn a watched server with both `config.toml` and `credentials.json`
/// targets armed. Mirrors `spawn_watched_server` but accepts a fully
/// rendered config text (so the caller can wire an arbitrary upstream
/// base URL) and returns only the base URL plus the tempdir holding
/// the config; the credentials path is the caller's responsibility
/// (it lives under the XDG dir the caller seeded).
async fn spawn_server_with_config_text(config_text: &str) -> (String, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("config.toml");
    std::fs::write(&config_path, config_text).unwrap();

    let config = parse_config(config_text);
    let config = common::isolate_usage_db(config);

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}");

    let watched_path = config_path.clone();
    tokio::spawn(async move {
        let _ = routectl_cli::server::serve_on_listener(config, listener, Some(watched_path)).await;
    });

    let client = reqwest::Client::new();
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(resp) = client.get(format!("{base_url}/health")).send().await
            && resp.status().is_success()
        {
            return (base_url, dir);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("test server failed to come up at {base_url}");
}

/// End-to-end credentials hot-reload: seed an `oauth://anthropic`
/// credential under a tempdir XDG, point routectl at a wiremock
/// upstream that matches per-bearer, rewrite `credentials.json` via
/// the same atomic-rename flow `routectl login` / `refresh` use, and
/// prove the next `POST /v1/messages` carries the freshly-rotated
/// bearer to the upstream. Closes the gap left by the prior suite,
/// which only rewrote `config.toml` and inspected `/v1/models`.
///
/// `serial_test::serial` is non-negotiable here: this test mutates
/// `XDG_CONFIG_HOME` and any sibling test reading the env in parallel
/// would see the modified value.
#[tokio::test]
#[serial_test::serial]
async fn credentials_atomic_rewrite_surfaces_new_bearer_on_next_request() {
    // Arrange: isolated XDG + seeded credentials.
    let xdg = tempfile::tempdir().unwrap();
    let _xdg = EnvGuard::set("XDG_CONFIG_HOME", xdg.path());
    let creds_path = xdg.path().join("routectl").join("credentials.json");
    write_credentials_atomic(&creds_path, "first-access-token");

    // Mock Anthropic upstream: one matcher per expected bearer. Both
    // return distinct text bodies so the test can tell which token the
    // upstream actually saw without sniffing the request directly.
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock_path("/v1/messages"))
        .and(header("authorization", "Bearer first-access-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_first",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "first-token"}],
            "model": "claude-sonnet-4-6",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(wiremock_path("/v1/messages"))
        .and(header("authorization", "Bearer second-access-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_second",
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": "second-token"}],
            "model": "claude-sonnet-4-6",
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })))
        .mount(&mock)
        .await;

    // Build a config that resolves api_key_ref via oauth://anthropic
    // and dispatches to the mock upstream.
    let config_text = format!(
        r#"
[server]
host = "127.0.0.1"
port = 0
strict_translation = false

[providers.anthropic_oauth]
kind = "anthropic-api"
base_url = "{}"
api_key_ref = "oauth://anthropic"
auth_kind = "oauth-bearer"

[models.claude]
provider = "anthropic_oauth"
upstream = "claude-sonnet-4-6"

[aliases]
default = "claude"
"#,
        mock.uri()
    );
    let (base_url, _config_dir) = spawn_server_with_config_text(&config_text).await;

    // Act 1: a request before the rewrite must use first-access-token.
    let client = reqwest::Client::new();
    let body = json!({
        "model": "default",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]
    });
    let resp = client
        .post(format!("{base_url}/v1/messages"))
        .json(&body)
        .send()
        .await
        .expect("post messages (first)");
    assert!(
        resp.status().is_success(),
        "expected 200 with seeded first-access-token, got {}",
        resp.status()
    );
    let payload: Value = resp.json().await.expect("json body (first)");
    assert_eq!(
        payload["content"][0]["text"], "first-token",
        "upstream did not see seeded first-access-token: {payload}"
    );

    // Act 2: atomic-rewrite credentials.json with the new token.
    write_credentials_atomic(&creds_path, "second-access-token");

    // Assert: a follow-up request observes the new token within the
    // watcher's debounce + reload window. Poll because the reload is
    // asynchronous; an unmatched bearer surfaces as a wiremock 404,
    // which we treat as "still on the old token, keep waiting".
    let mut succeeded = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let resp = client
            .post(format!("{base_url}/v1/messages"))
            .json(&body)
            .send()
            .await
            .expect("post messages (poll)");
        if resp.status().is_success() {
            let payload: Value = resp.json().await.expect("json body (poll)");
            if payload["content"][0]["text"] == "second-token" {
                succeeded = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        succeeded,
        "credentials hot-reload did not surface second-access-token within 5s"
    );
}

// -----------------------------------------------------------------
// Per-class retry-cap change applies post-reload
// -----------------------------------------------------------------

/// Config text wiring a single `anthropic-api` provider at `upstream_uri`
/// with a `[retry.classes.rate-limited]` override of `retry_cap`. Zero
/// backoff/jitter keeps a same-provider retry loop from adding latency
/// the test would otherwise have to sleep through.
fn config_text_with_rate_limited_cap(upstream_uri: &str, retry_cap: u32) -> String {
    let key_ref = common::file_ref("test-key");
    format!(
        r#"
version = 3
[server]
host = "127.0.0.1"
port = 0
strict_translation = false

[retry]
initial_backoff_ms = 0
jitter_ms = 0

[retry.classes.rate-limited]
retry = {retry_cap}

[providers.up]
kind = "anthropic-api"
base_url = "{upstream_uri}"
api_key_ref = "{key_ref}"

[models.claude]
provider = "up"
upstream = "claude-sonnet-4-6"

[aliases]
default = "claude"
"#
    )
}

/// Send one `POST /v1/messages` and return how many additional requests
/// the mock upstream received as a result -- the direct behavioral
/// signal for how many times the router dispatched to the SAME provider
/// for this one client call (the retry-cap-for-class this test pins).
async fn same_provider_dispatch_count(
    client: &reqwest::Client,
    base_url: &str,
    mock: &MockServer,
) -> usize {
    let before = mock
        .received_requests()
        .await
        .expect("recording enabled")
        .len();
    let body = json!({
        "model": "default",
        // Above `probe_max_tokens` (default 1) so the request is not
        // treated as an availability probe -- a probe fast-fails a 429
        // with no retry regardless of the configured class cap.
        "max_tokens": 50,
        "messages": [{"role": "user", "content": "hi"}]
    });
    let _ = client
        .post(format!("{base_url}/v1/messages"))
        .json(&body)
        .send()
        .await
        .expect("post messages");
    let after = mock
        .received_requests()
        .await
        .expect("recording enabled")
        .len();
    after - before
}

/// A current-version config (so `config set`'s raw version preflight accepts it)
/// carrying one provider, one model, and a single alias. `config set` edits
/// this in place through the shared write primitive; the running server's
/// file watcher then hot-reloads the atomic rename.
fn v3_config_with_alias(alias: &str) -> String {
    let key_ref = common::file_ref("test-key");
    format!(
        r#"version = 3

[server]
host = "127.0.0.1"
port = 0
strict_translation = false

[providers.fast]
kind = "openai-compat"
base_url = "http://127.0.0.1:1"
api_key_ref = "{key_ref}"

[models.gpt-4o]
provider = "fast"
upstream = "gpt-4o"

[aliases]
{alias} = "gpt-4o"
"#
    )
}

/// A valid `config set` edits `config.toml` through the shared write
/// primitive; the running server's file watcher picks up the atomic rename
/// and hot-reloads, so the newly-added alias surfaces on `/v1/models`.
#[tokio::test]
async fn config_set_valid_edit_hot_reloads() {
    let (base_url, dir) = spawn_server_with_config_text(&v3_config_with_alias("set-before")).await;
    let config_path = dir.path().join("config.toml");

    // Sanity: only the seeded alias is present.
    let initial = list_model_ids(&base_url).await;
    assert!(initial.iter().any(|s| s == "set-before"), "{initial:?}");

    // Act: add a new alias via the write pipeline (yes bypasses any prompt).
    routectl_cli::commands::config_edit::run(
        &config_path,
        "aliases.set-after",
        routectl_cli::commands::config_edit::EditKind::Set("gpt-4o".to_string()),
        true,
    )
    .expect("valid set must succeed");

    // Assert: the watcher hot-reloads and the new alias surfaces.
    assert!(
        wait_for_alias(&base_url, "set-after", Duration::from_secs(3)).await,
        "config set did not hot-reload the new alias within 3s"
    );
}

/// A `config set` whose candidate fails the shared validation gate writes
/// NOTHING: the file stays byte-identical and the running server never sees
/// a reload (the old alias remains, no candidate alias appears).
#[tokio::test]
async fn config_set_failed_candidate_makes_no_watcher_visible_write() {
    let (base_url, dir) = spawn_server_with_config_text(&v3_config_with_alias("stable")).await;
    let config_path = dir.path().join("config.toml");
    let before = std::fs::read(&config_path).unwrap();

    // A non-numeric value for a numeric field fails the re-parse gate.
    let err = routectl_cli::commands::config_edit::run(
        &config_path,
        "server.port",
        routectl_cli::commands::config_edit::EditKind::Set("not-a-port".to_string()),
        true,
    );
    assert!(err.is_err(), "an invalid candidate must be refused");

    // The file is byte-identical: nothing the watcher could observe changed.
    assert_eq!(std::fs::read(&config_path).unwrap(), before);

    // Give the watcher a beat; the old alias must still be live.
    tokio::time::sleep(Duration::from_millis(400)).await;
    let ids = list_model_ids(&base_url).await;
    assert!(
        ids.iter().any(|s| s == "stable"),
        "old alias must remain: {ids:?}"
    );
}

/// A `[retry.classes.rate-limited]` cap raised across a reload takes
/// effect on the REBUILT router: a single client request dispatched
/// against an upstream that always returns 429 hits that upstream
/// exactly `retry_cap` times (the same-provider retry loop stops once
/// `attempts_made` reaches the class's resolved cap). Observed through
/// real dispatch rather than through `routectl-testkit` capture, which
/// only sees events on the SAME task -- irrelevant here since the
/// server runs its own spawned tasks.
#[tokio::test]
async fn retry_classes_cap_change_applies_after_reload() {
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock_path("/v1/messages"))
        .respond_with(ResponseTemplate::new(429).set_body_json(json!({
            "type": "error",
            "error": {"type": "rate_limit_error", "message": "rate limited"}
        })))
        .mount(&mock)
        .await;

    let (base_url, dir) =
        spawn_server_with_config_text(&config_text_with_rate_limited_cap(&mock.uri(), 0)).await;
    let config_path = dir.path().join("config.toml");
    let client = reqwest::Client::new();

    // Sanity: retry = 0 means the provider is dispatched exactly once.
    let before_reload = same_provider_dispatch_count(&client, &base_url, &mock).await;
    assert_eq!(
        before_reload, 1,
        "retry.classes.rate-limited.retry = 0 must dispatch exactly once"
    );

    // Act: reload with the cap raised to 2.
    write_atomic(
        &config_path,
        config_text_with_rate_limited_cap(&mock.uri(), 2).as_bytes(),
    );

    // Assert: poll a single client call at a time until it observes 2
    // upstream hits -- proof the rebuilt router resolved the raised cap.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut observed = before_reload;
    while Instant::now() < deadline {
        observed = same_provider_dispatch_count(&client, &base_url, &mock).await;
        if observed == 2 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        observed, 2,
        "raised retry.classes.rate-limited cap did not apply within 5s after reload"
    );
}

// -----------------------------------------------------------------
// Start-and-degrade on a broken credentials.json + hot-reload recovery
// -----------------------------------------------------------------

/// Seed a structurally-corrupt `credentials.json` at 0o600 under the XDG
/// creds path. `OAuthStore::open_or_degraded` brings the oauth arm up
/// PRESENT-but-DEGRADED (start-and-degrade) rather than failing serve
/// startup or dropping the arm. 0o600 keeps the ONLY defect the
/// unparseable content -- not a world-readable perms rejection -- so the
/// recovery signal is unambiguous. Written before serve starts so the
/// store observes it as broken at open time (a missing file would open
/// clean).
fn write_corrupt_credentials(creds_path: &Path) {
    let parent = creds_path.parent().expect("creds path has parent");
    std::fs::create_dir_all(parent).expect("mkdir creds parent");
    std::fs::write(creds_path, b"{ this is not valid credentials json <<<")
        .expect("write corrupt creds");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(creds_path, std::fs::Permissions::from_mode(0o600))
            .expect("chmod 0600");
    }
}

/// A minimal Anthropic Messages success body whose single text block is
/// `text` -- lets a mock upstream tag which credential path it served.
fn anthropic_ok_body(text: &str) -> Value {
    json!({
        "id": "msg_ok",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": text}],
        "model": "claude-sonnet-4-6",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1}
    })
}

/// POST one non-streaming `/v1/messages` routed by `model` (an alias) and
/// return the `(status, parsed-body)` pair. A non-JSON body yields
/// `Value::Null` so callers poll on rather than panic.
async fn post_message(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
) -> (reqwest::StatusCode, Value) {
    let body = json!({
        "model": model,
        "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]
    });
    let resp = client
        .post(format!("{base_url}/v1/messages"))
        .json(&body)
        .send()
        .await
        .expect("post /v1/messages");
    let status = resp.status();
    let payload: Value = resp.json().await.unwrap_or(Value::Null);
    (status, payload)
}

/// The client-observable signature of a credential-resolution failure:
/// HTTP 503 plus the generic sanitized message. The TRUE cause is logged
/// server-side at ERROR (`auth error suppressed in HTTP response`), but
/// that log fires on a spawned server task that thread-local tracing
/// capture cannot see -- so the client body is the reliable observable.
fn is_credential_resolution_failure(status: reqwest::StatusCode, body: &Value) -> bool {
    status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        && body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("server-side credential resolution failed"))
}

/// Poll the `oauth://`-backed alias until it is OBSERVABLY degraded
/// (credential-resolution failure) or `max_wait` elapses. Establishing the
/// degraded steady state BEFORE the recovery write is the first half of the
/// watcher-arming guard: it proves the test is asserting a real transition,
/// not a state that was already recovered.
async fn wait_for_degraded_oauth(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    max_wait: Duration,
) -> bool {
    let deadline = Instant::now() + max_wait;
    while Instant::now() < deadline {
        let (status, body) = post_message(client, base_url, model).await;
        if is_credential_resolution_failure(status, &body) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Poll the `oauth://`-backed alias for recovery (a 200 carrying
/// `recovered_marker`) while periodically re-issuing an atomic rewrite of a
/// VALID `credentials.json` at `creds_path`, until recovery is observed
/// (true) or `max_wait` elapses (false).
///
/// The credentials file-watch arms asynchronously on notify's background
/// thread AFTER serve is already answering `/health` (see
/// `poll_alias_with_restimulus` for the full mechanism), so a single
/// recovery write can race ahead of the armed watch and be dropped with no
/// fs-event re-delivery. Re-writing the SAME token each interval is
/// idempotent for the assertion -- `OAuthStore::reload_from_disk` resolves
/// the identical bearer regardless of how many times the record is
/// re-read -- so a re-delivered event is a harmless cache refresh and the
/// first event the armed watch actually sees clears the degrade marker.
async fn wait_for_oauth_recovery_with_restimulus(
    client: &reqwest::Client,
    base_url: &str,
    model: &str,
    creds_path: &Path,
    access_token: &str,
    recovered_marker: &str,
    max_wait: Duration,
) -> bool {
    let deadline = Instant::now() + max_wait;
    write_credentials_atomic(creds_path, access_token);
    let mut last_write = Instant::now();
    while Instant::now() < deadline {
        let (status, body) = post_message(client, base_url, model).await;
        if status.is_success() && body["content"][0]["text"] == recovered_marker {
            return true;
        }
        if last_write.elapsed() >= RESTIMULUS_INTERVAL {
            write_credentials_atomic(creds_path, access_token);
            last_write = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Feature-boundary smoke, automated: `serve` must START despite a broken
/// `credentials.json` (start-and-degrade), keep serving `file://`-backed
/// aliases while the `oauth://` arm is degraded, and RECOVER the oauth arm
/// when a valid credentials file is written -- all in the SAME running
/// daemon, no restart. The automated analogue of the manual runtime smoke
/// this surface already passed.
///
/// True-cause observability: the sanitized cause of the oauth failure is
/// logged server-side at ERROR, but that log fires on a spawned server task
/// and thread-local tracing capture does not reach spawned tasks -- so the
/// reliable observable is the CLIENT body (`is_credential_resolution_failure`:
/// 503 + the generic `server-side credential resolution failed`). The test
/// asserts the generic client message AND that serve stays up (health +
/// file:// alias keep serving), which is the full observable contract for
/// start-and-degrade.
///
/// Watcher-arming race guard (two halves): the initial degraded state is
/// OBSERVED first (`wait_for_degraded_oauth`) BEFORE the recovery write, so
/// a lost first event cannot make the test claim a recovery it never
/// triggered; and the recovery write is re-issued on an interval
/// (`wait_for_oauth_recovery_with_restimulus`, idempotent) with a generous
/// ceiling, so an event dropped before the credentials watch armed is
/// re-delivered many times before the ceiling expires.
///
/// `serial_test::serial` is non-negotiable: this test mutates
/// `XDG_CONFIG_HOME` and any sibling test reading the env in parallel would
/// see the modified value.
#[tokio::test]
#[serial_test::serial]
async fn serve_starts_degraded_on_broken_credentials_then_hot_reloads_recovery() {
    // Arrange: isolated XDG holding a CORRUPT credentials.json.
    let xdg = tempfile::tempdir().unwrap();
    let _xdg = EnvGuard::set("XDG_CONFIG_HOME", xdg.path());
    let creds_path = xdg.path().join("routectl").join("credentials.json");
    write_corrupt_credentials(&creds_path);

    // Mock upstream: one matcher per credential path. The file:// static
    // key stamps `x-api-key`; the recovered oauth seat stamps
    // `authorization: Bearer`. Distinct bodies so the test can tell which
    // credential path the upstream actually served.
    let mock = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock_path("/v1/messages"))
        .and(header("x-api-key", "file-secret-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(anthropic_ok_body("file-ok")))
        .mount(&mock)
        .await;
    Mock::given(method("POST"))
        .and(wiremock_path("/v1/messages"))
        .and(header("authorization", "Bearer recovered-access-token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(anthropic_ok_body("oauth-recovered")),
        )
        .mount(&mock)
        .await;

    // Two anthropic-api providers sharing the mock upstream: one resolves
    // via oauth://anthropic (degraded at boot), one via a file:// static
    // key (unaffected). The oauth base_url never dispatches while degraded.
    let file_key_ref = common::file_ref("file-secret-key");
    let config_text = format!(
        r#"
[server]
host = "127.0.0.1"
port = 0
strict_translation = false

[providers.anthropic_oauth]
kind = "anthropic-api"
base_url = "{uri}"
api_key_ref = "oauth://anthropic"
auth_kind = "oauth-bearer"

[providers.static_file]
kind = "anthropic-api"
base_url = "{uri}"
api_key_ref = "{file_key_ref}"

[models.claude_oauth]
provider = "anthropic_oauth"
upstream = "claude-sonnet-4-6"

[models.claude_file]
provider = "static_file"
upstream = "claude-sonnet-4-6"

[aliases]
oauthalias = "claude_oauth"
filealias = "claude_file"
"#,
        uri = mock.uri(),
    );

    // Act: serve must COME UP despite the broken credentials file. The
    // daemon runs on an in-process tokio task, so its PID is the test
    // process throughout -- captured here to pin the no-restart invariant.
    let pid_before = std::process::id();
    let (base_url, _config_dir) = spawn_server_with_config_text(&config_text).await;
    let client = reqwest::Client::new();

    // Assert (a): health answers -- serve started, start-and-degrade.
    let health = client
        .get(format!("{base_url}/health"))
        .send()
        .await
        .expect("get /health");
    assert!(
        health.status().is_success(),
        "serve must answer /health despite a broken credentials.json, got {}",
        health.status()
    );

    // Assert (b): the oauth:// alias is OBSERVABLY degraded -- 503 + the
    // generic credential-resolution message. Polled so the degraded steady
    // state is confirmed BEFORE the recovery write is issued.
    assert!(
        wait_for_degraded_oauth(&client, &base_url, "oauthalias", Duration::from_secs(5)).await,
        "oauth:// alias must fail credential resolution while credentials.json is broken"
    );

    // Assert (c): the file:// alias still resolves its secret and reaches
    // upstream -- file:// is unaffected while the oauth arm is degraded.
    let (file_status, file_body) = post_message(&client, &base_url, "filealias").await;
    assert!(
        file_status.is_success(),
        "file:// alias must serve while oauth is degraded, got {file_status}: {file_body}"
    );
    assert_eq!(
        file_body["content"][0]["text"], "file-ok",
        "file:// alias must reach upstream with its resolved secret: {file_body}"
    );

    // Act 2 + Assert (recovery): write a VALID credentials.json atomically
    // (fresh anthropic seat) and poll for recovery. The write is re-issued
    // on an interval to defeat the async watcher-arming race; recovery is a
    // 200 carrying the oauth mock's marker (proof the recovered bearer was
    // resolved AND sent upstream), never a restart.
    assert!(
        wait_for_oauth_recovery_with_restimulus(
            &client,
            &base_url,
            "oauthalias",
            &creds_path,
            "recovered-access-token",
            "oauth-recovered",
            RELOAD_WAIT_CEILING,
        )
        .await,
        "oauth:// alias did not recover within {RELOAD_WAIT_CEILING:?} after a valid credentials.json was written"
    );

    // The recovery happened in the SAME process: the daemon was never
    // re-execed. In-process spawn makes this structural; the assert pins it
    // so a future harness change to a subprocess model cannot quietly break
    // the no-restart contract.
    assert_eq!(
        std::process::id(),
        pid_before,
        "credentials recovery must happen in the same daemon process, no restart"
    );
}
