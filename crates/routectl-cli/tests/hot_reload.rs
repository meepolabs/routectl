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
use serde_json::{json, Value};
use tokio::net::TcpListener;
use wiremock::matchers::{header, method, path as wiremock_path};
use wiremock::{Mock, MockServer, ResponseTemplate};

mod common;

// -----------------------------------------------------------------
// Test rig
// -----------------------------------------------------------------

/// Single-provider config seeded into the test config.toml. The
/// upstream base is bogus (port 1) -- tests assert on `/v1/models`
/// and `/health`, which never dispatch upstream.
fn config_text_with_alias(alias: &str) -> String {
    format!(
        r#"
[server]
host = "127.0.0.1"
port = 0
strict_translation = false

[providers.fast]
kind = "openai-compat"
base_url = "http://127.0.0.1:1"
api_key_ref = "literal:test-key"

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
        if let Ok(resp) = client.get(format!("{base_url}/health")).send().await {
            if resp.status().is_success() {
                return (base_url, config_path, dir);
            }
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

// -----------------------------------------------------------------
// Tests
// -----------------------------------------------------------------

/// Writing a new config.toml triggers a hot-reload; the new alias
/// surfaces on `/v1/models` within a few hundred ms.
#[tokio::test]
async fn file_write_triggers_config_reload() {
    let (base_url, config_path, _dir) = spawn_watched_server("first-alias").await;

    // Sanity: initial alias visible.
    let initial = list_model_ids(&base_url).await;
    assert!(initial.iter().any(|s| s == "first-alias"));

    // Act: rewrite with a different alias.
    write_atomic(
        &config_path,
        config_text_with_alias("second-alias").as_bytes(),
    );

    // Assert: the new alias surfaces; the old one disappears.
    assert!(
        wait_for_alias(&base_url, "second-alias", Duration::from_secs(3)).await,
        "second-alias did not appear within 3s after config reload"
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
    use nix::sys::signal::{kill, Signal};
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
/// flow in `routectl-auth/src/oauth/file_io.rs::save`: write a tempfile
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
        if let Ok(resp) = client.get(format!("{base_url}/health")).send().await {
            if resp.status().is_success() {
                return (base_url, dir);
            }
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
