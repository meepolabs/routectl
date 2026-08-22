//! Integration tests for the file-watch + SIGHUP hot-reload path.
//!
//! Each test boots a server via `serve_on_listener` against a
//! `tempfile::tempdir`-rooted config.toml and credentials.json so
//! the operator's real `$XDG_CONFIG_HOME` is never touched.
//! Polling-based timing (50 ms intervals under generous ceilings)
//! keeps the suite fast and platform-jitter tolerant; a fs-event
//! stimulus is always re-issued on the restimulus cadence rather
//! than written once and slept on (see `poll_alias_with_restimulus`).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use routectl_router::{CURRENT_CONFIG_VERSION as CURRENT, Config};
use routectl_testkit::ScopedEnv;
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
/// -- `poll_alias_with_restimulus` returns the instant the alias appears, so
/// the happy path pays only the real reload latency, never the ceiling.
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
version = {CURRENT}
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
/// A write to a watched path is NOT reliably delivered to the watcher, and
/// the loss is not confined to a startup window. The best-understood case is
/// watch arming: the inotify watch is armed on notify's background
/// event-loop thread AFTER `spawn_watcher` returns -- `debouncer.watch()`
/// only enqueues the `add_watch`, it does not block on it (this is why the
/// file_watch.rs unit tests interpose a `SETTLE_MS` sleep before their first
/// mutation), and the server answers `/health` before that watch is live. But
/// a single write has also been observed never delivered LONG AFTER the watch
/// was provably armed -- with an earlier marker config on the same path
/// already reloaded. Atomic-rename delivery through debouncing is best-effort
/// at every point in a server's life, not just at boot.
///
/// There is no fs-event re-delivery, so a dropped event means the reload
/// simply never fires: a bare write plus a fixed sleep can observe NOTHING
/// and still pass whenever the assertion's expected outcome coincides with
/// "no reload happened". Re-issuing the write is therefore the only reliable
/// stimulus, and a test whose expected outcome IS "nothing changes" needs a
/// separate proof that the watch is delivering (see the both-sides marker
/// guard in `failed_reload_keeps_old_reduction_state`).
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

/// `config_text_with_alias` plus a second `marker` alias sharing the same
/// model. The marker is a watch-delivery proof for tests whose candidate must
/// be REJECTED: a VALID config carrying it lands on the watched path before
/// and after the rejected window, so the alias table itself shows the reload
/// pipeline was processing writes to that path throughout.
fn config_text_with_alias_and_marker(alias: &str, marker: &str) -> String {
    format!("{}{marker} = \"gpt-4o\"\n", config_text_with_alias(alias))
}

/// Land a VALID config publishing `stable` plus `marker` and wait for `marker`
/// to surface on `/v1/models`.
///
/// This is the half a rejected-reload test cannot do without: "the candidate
/// was declined" and "the write was never delivered" produce the identical
/// observation, so the watch has to be proven live independently. Run on BOTH
/// sides of the rejected window, it bounds the window with demonstrated
/// delivery rather than assuming it.
async fn prove_watch_delivering(base_url: &str, config_path: &Path, stable: &str, marker: &str) {
    let armed = config_text_with_alias_and_marker(stable, marker);
    assert!(
        poll_alias_with_restimulus(
            base_url,
            config_path,
            armed.as_bytes(),
            marker,
            RELOAD_WAIT_CEILING,
        )
        .await,
        "the watch is not delivering: marker alias `{marker}` did not surface within {RELOAD_WAIT_CEILING:?}",
    );
}

/// How long a rejected-candidate test holds its candidate on disk, re-issuing
/// the write on the `RESTIMULUS_INTERVAL` cadence. Spans several intervals so
/// the rejected write is delivered repeatedly rather than once, and each
/// delivery is checked.
const REJECT_HOLD_WINDOW: Duration = Duration::from_secs(4);

/// Re-issue an atomic rewrite of `config_path` with a candidate that must be
/// REJECTED, on the `RESTIMULUS_INTERVAL` cadence for `window`, failing the
/// moment the live alias table shows the reload was applied: `never` is an
/// alias the candidate would publish if accepted, `keep` an alias only the
/// PRIOR config publishes.
///
/// The inverse of `poll_alias_with_restimulus`: that one waits for an alias a
/// valid candidate should publish, this one holds a candidate that must never
/// change the live table and watches for any sign that it did. Same re-issuing
/// rationale -- a single write can be dropped with no fs-event re-delivery, so
/// repeated identical writes are what make the stimulus reliable rather than
/// best-effort. Pair it with `prove_watch_delivering` on both sides: repeated
/// delivery alone does not prove any of the writes landed.
async fn hold_rejected_candidate(
    base_url: &str,
    config_path: &Path,
    contents: &[u8],
    never: Option<&str>,
    keep: Option<&str>,
    window: Duration,
) {
    let deadline = Instant::now() + window;
    write_atomic(config_path, contents);
    let mut last_write = Instant::now();
    while Instant::now() < deadline {
        let ids = list_model_ids(base_url).await;
        if let Some(never) = never {
            assert!(
                !ids.iter().any(|s| s == never),
                "the rejected candidate's alias `{never}` surfaced, so the reload was APPLIED: {ids:?}",
            );
        }
        if let Some(keep) = keep {
            assert!(
                ids.iter().any(|s| s == keep),
                "the prior config's alias `{keep}` vanished, so the rejected reload took effect: {ids:?}",
            );
        }
        if last_write.elapsed() >= RESTIMULUS_INTERVAL {
            write_atomic(config_path, contents);
            last_write = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

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
///
/// Because the expected outcome is "nothing changes", a dropped write would
/// satisfy the assertion vacuously. So the watch is proven delivering on both
/// sides of the rejected window (`prove_watch_delivering`) and the broken
/// content is re-issued on the restimulus cadence throughout it, with the
/// surviving alias re-checked on every delivery.
#[tokio::test]
async fn partial_write_keeps_old_config() {
    let (base_url, config_path, _dir) = spawn_watched_server("stable-alias").await;

    prove_watch_delivering(&base_url, &config_path, "stable-alias", "partial-armed").await;

    // Act: hold a truncated TOML (missing closing brace, dangling section
    // header) on disk across several restimulus cycles. Even after debounce +
    // parse, the old alias must still be present on every delivery.
    hold_rejected_candidate(
        &base_url,
        &config_path,
        b"[server\nhost = \"127.0.0.1\"\nport = 0\n",
        None,
        Some("stable-alias"),
        REJECT_HOLD_WINDOW,
    )
    .await;

    let ids = list_model_ids(&base_url).await;
    assert!(
        ids.iter().any(|s| s == "stable-alias"),
        "old alias must remain after a parse-rejected reload: {ids:?}"
    );

    prove_watch_delivering(&base_url, &config_path, "stable-alias", "partial-post").await;
}

/// Valid-UTF8 but non-TOML content also must be rejected, with the
/// existing alias preserved. Same surface as `partial_write_keeps_old_config`
/// but exercises the toml::from_str error path with content the
/// reader cannot recover from. Same both-sides delivery proof, for the same
/// reason: "rejected" and "never delivered" look identical from outside.
#[tokio::test]
async fn invalid_toml_keeps_old_config() {
    let (base_url, config_path, _dir) = spawn_watched_server("durable-alias").await;

    prove_watch_delivering(&base_url, &config_path, "durable-alias", "invalid-armed").await;

    // Act: hold garbage that parses as utf-8 but not as TOML.
    hold_rejected_candidate(
        &base_url,
        &config_path,
        b"<<<not toml at all>>>\n!@#$%^\n",
        None,
        Some("durable-alias"),
        REJECT_HOLD_WINDOW,
    )
    .await;

    let ids = list_model_ids(&base_url).await;
    assert!(
        ids.iter().any(|s| s == "durable-alias"),
        "old alias must remain after an invalid-toml reload attempt: {ids:?}"
    );

    prove_watch_delivering(&base_url, &config_path, "durable-alias", "invalid-post").await;
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
version = {CURRENT}
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
///
/// Delivery is proven on both sides of the rejected window
/// (`prove_watch_delivering`) and the candidate is re-issued on the restimulus
/// cadence throughout it -- without that, a write the watcher never saw would
/// satisfy "the old alias is still live" for entirely the wrong reason.
#[tokio::test]
async fn feature_unsupported_class_override_rejected_old_router_stays_live() {
    let (base_url, config_path, _dir) = spawn_watched_server("class-policy-stable").await;

    prove_watch_delivering(
        &base_url,
        &config_path,
        "class-policy-stable",
        "class-policy-armed",
    )
    .await;

    hold_rejected_candidate(
        &base_url,
        &config_path,
        config_text_with_reserved_class_override("class-policy-rejected").as_bytes(),
        Some("class-policy-rejected"),
        Some("class-policy-stable"),
        REJECT_HOLD_WINDOW,
    )
    .await;

    let ids = list_model_ids(&base_url).await;
    assert!(
        ids.iter().any(|s| s == "class-policy-stable"),
        "old alias must remain after a class-policy-rejected reload: {ids:?}"
    );
    assert!(
        !ids.iter().any(|s| s == "class-policy-rejected"),
        "the rejected candidate's alias must never surface: {ids:?}"
    );

    prove_watch_delivering(
        &base_url,
        &config_path,
        "class-policy-stable",
        "class-policy-post",
    )
    .await;
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
version = {CURRENT}
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
/// and the candidate's alias never surfaces. Delivery is proven on both sides
/// of the rejected window and the candidate is re-issued on the restimulus
/// cadence throughout it, so an undelivered write cannot pass for a rejection.
#[tokio::test]
async fn unknown_field_reload_rejected_old_router_stays_live() {
    let (base_url, config_path, _dir) = spawn_watched_server("field-stable").await;

    prove_watch_delivering(&base_url, &config_path, "field-stable", "field-armed").await;

    hold_rejected_candidate(
        &base_url,
        &config_path,
        config_text_with_unknown_server_field("field-rejected").as_bytes(),
        Some("field-rejected"),
        Some("field-stable"),
        REJECT_HOLD_WINDOW,
    )
    .await;

    let ids = list_model_ids(&base_url).await;
    assert!(
        ids.iter().any(|s| s == "field-stable"),
        "old alias must remain after an unknown-field-rejected reload: {ids:?}"
    );
    assert!(
        !ids.iter().any(|s| s == "field-rejected"),
        "the rejected candidate's alias must never surface: {ids:?}"
    );

    prove_watch_delivering(&base_url, &config_path, "field-stable", "field-post").await;
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

    // Assert: the final alias is visible. The final rewrite is re-issued on
    // the restimulus cadence (identical bytes, idempotent) because a lone
    // atomic rename is not reliably delivered to the watcher -- the loop above
    // is the behavior under test, but its LAST event is the one this assertion
    // depends on.
    assert!(
        poll_alias_with_restimulus(
            &base_url,
            &config_path,
            config_text_with_alias("alias-4").as_bytes(),
            "alias-4",
            RELOAD_WAIT_CEILING,
        )
        .await,
        "final alias-4 did not appear within {RELOAD_WAIT_CEILING:?} after five sequential reloads"
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
    let post = config_text_with_alias("sighup-post");
    write_atomic(&config_path, post.as_bytes());

    // Send SIGHUP to ourselves.
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    kill(Pid::from_raw(std::process::id() as i32), Signal::SIGHUP).expect("send SIGHUP");

    // Assert: the new alias surfaces. The rewrite is re-issued on the
    // restimulus cadence (identical bytes, idempotent) so the fs arm still
    // converges if the lone rename above was never delivered -- the combined
    // arm is what is under test, not one specific arm winning the race.
    assert!(
        poll_alias_with_restimulus(
            &base_url,
            &config_path,
            post.as_bytes(),
            "sighup-post",
            RELOAD_WAIT_CEILING,
        )
        .await,
        "sighup-post alias did not appear within {RELOAD_WAIT_CEILING:?} after SIGHUP"
    );
}

// -----------------------------------------------------------------
// Credentials hot-reload (oauth:// path)
// -----------------------------------------------------------------

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
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", xdg.path());
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
version = {CURRENT}
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
        r#"version = {CURRENT}

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

    // Assert: the watcher hot-reloads and the new alias surfaces. The edit
    // above is the behavior under test, but its single atomic rename can race
    // ahead of the asynchronously-armed inotify watch and be dropped with no
    // fs-event re-delivery; re-issue the identical post-edit bytes on an
    // interval (idempotent for the reload) until the alias surfaces.
    let edited = std::fs::read(&config_path).expect("read edited config");
    assert!(
        poll_alias_with_restimulus(
            &base_url,
            &config_path,
            &edited,
            "set-after",
            RELOAD_WAIT_CEILING,
        )
        .await,
        "config set did not hot-reload the new alias within {RELOAD_WAIT_CEILING:?}"
    );
}

/// A `config set` whose candidate fails the shared validation gate writes
/// NOTHING: the file stays byte-identical and the running server never sees
/// a reload (the old alias remains, no candidate alias appears).
///
/// The watch is proven delivering FIRST, so "the old alias is still live" is
/// evidence about the failed edit rather than about a watcher that was never
/// going to report anything. That marker write also establishes the byte
/// baseline this test compares against.
#[tokio::test]
async fn config_set_failed_candidate_makes_no_watcher_visible_write() {
    let (base_url, dir) = spawn_server_with_config_text(&v3_config_with_alias("stable")).await;
    let config_path = dir.path().join("config.toml");

    prove_watch_delivering(&base_url, &config_path, "stable", "set-fail-armed").await;
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

    // Act + Assert: reload with the cap raised to 2, then poll a single client
    // call at a time until it observes 2 upstream hits -- proof the rebuilt
    // router resolved the raised cap. The rewrite is re-issued on an interval
    // (idempotent -- identical bytes always rebuild the same cap) because the
    // single atomic rename can race ahead of the asynchronously-armed inotify
    // watch and be dropped with no fs-event re-delivery.
    let raised = config_text_with_rate_limited_cap(&mock.uri(), 2);
    write_atomic(&config_path, raised.as_bytes());
    let mut last_write = Instant::now();
    let deadline = Instant::now() + RELOAD_WAIT_CEILING;
    let mut observed = before_reload;
    while Instant::now() < deadline {
        observed = same_provider_dispatch_count(&client, &base_url, &mock).await;
        if observed == 2 {
            break;
        }
        if last_write.elapsed() >= RESTIMULUS_INTERVAL {
            write_atomic(&config_path, raised.as_bytes());
            last_write = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        observed, 2,
        "raised retry.classes.rate-limited cap did not apply within {RELOAD_WAIT_CEILING:?} after reload"
    );
}

// -----------------------------------------------------------------
// Catalog-overlay reload surfaces on /v1/models
// -----------------------------------------------------------------

/// Nickname and upstream of the single model in
/// `overlay_window_config`. The upstream matches no baked catalog cell, so
/// its window is unconfirmed until the overlay supplies one -- which makes
/// "the key appeared" an unambiguous signal that the overlay went live.
const OVERLAY_MODEL: &str = "windowed";
const OVERLAY_UPSTREAM: &str = "no-such-model-in-any-catalog-cell";

/// The window the overlay cell publishes for `OVERLAY_UPSTREAM`.
const OVERLAY_WINDOW: u32 = 321_000;

/// Single-model config whose upstream has no baked catalog row, so the
/// model's context window comes from the overlay alone.
fn overlay_window_config() -> String {
    let key_ref = common::file_ref("test-key");
    format!(
        r#"
version = {CURRENT}
[server]
host = "127.0.0.1"
port = 0
strict_translation = false

[providers.anthropic]
kind = "anthropic-api"
api_key_ref = "{key_ref}"

[models.{OVERLAY_MODEL}]
provider = "anthropic"
upstream = "{OVERLAY_UPSTREAM}"
"#
    )
}

/// Atomic-rename write of a `catalog_overlay.json` publishing
/// `max_context_tokens = window` for `OVERLAY_UPSTREAM`. Mirrors
/// `catalog_overlay::save`'s write pattern (tempfile in the same parent,
/// rename onto the target), which is what the watcher is tuned for.
fn write_overlay_atomic(overlay_path: &Path, window: u32, revision: u64) {
    let overlay = json!({
        "schema_version": 1,
        "revision": revision,
        "cells": {
            format!("anthropic-api:{OVERLAY_UPSTREAM}"): {
                "source": "user",
                "verified_at": "2026-08-21",
                "max_context_tokens": window,
            }
        }
    });
    let parent = overlay_path.parent().expect("overlay path has a parent");
    std::fs::create_dir_all(parent).expect("mkdir overlay parent");
    let tmp = tempfile::Builder::new()
        .prefix(".catalog_overlay.tmp.")
        .suffix(".json")
        .tempfile_in(parent)
        .unwrap();
    std::fs::write(
        tmp.path(),
        serde_json::to_vec_pretty(&overlay).expect("serialize overlay"),
    )
    .unwrap();
    tmp.persist(overlay_path).expect("persist overlay");
}

/// The `context_length` of the `/v1/models` entry for `id`, or `None` when
/// the entry omits the key.
async fn context_length_of(base_url: &str, id: &str) -> Option<u64> {
    let resp = reqwest::get(format!("{base_url}/v1/models")).await.unwrap();
    assert!(resp.status().is_success(), "GET /v1/models failed");
    let body: Value = resp.json().await.unwrap();
    body["data"]
        .as_array()
        .expect("data is an array")
        .iter()
        .find(|e| e["id"] == id)
        .expect("the configured model must be listed")
        .get("context_length")
        .and_then(Value::as_u64)
}

/// An overlay written at the watched `catalog_overlay.json` path goes live
/// on `/v1/models`: the second request sees the overlay's window where the
/// first saw no `context_length` at all.
///
/// The pre-write observation is load-bearing: it proves the key's later
/// appearance is the reload, not a value the baked table was already
/// supplying. The write is re-issued on the restimulus cadence with a
/// BUMPED revision each time, because a lone atomic rename is not reliably
/// delivered to the watcher (see `poll_alias_with_restimulus`) -- every
/// revision publishes the same window, so a re-delivered event is a no-op
/// for the assertion.
///
/// `serial_test::serial` is non-negotiable: the overlay lives at a
/// process-global `XDG_CONFIG_HOME`-derived path, so this test both pins and
/// writes under that env var.
#[tokio::test]
#[serial_test::serial]
async fn catalog_overlay_write_surfaces_new_context_length_after_reload() {
    let xdg = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", xdg.path());
    let overlay_path = routectl_router::overlay_default_path();
    // The watcher registers the overlay's PARENT directory, so that
    // directory has to exist before serve installs the watch -- on a real
    // install it always does (the config lives there); this fixture's
    // config lives in its own tempdir instead.
    std::fs::create_dir_all(overlay_path.parent().unwrap()).unwrap();

    let (base_url, _dir) = spawn_server_with_config_text(&overlay_window_config()).await;

    // Before: no baked cell matches this upstream, so the key is absent.
    assert_eq!(
        context_length_of(&base_url, OVERLAY_MODEL).await,
        None,
        "the fixture upstream must start with an unconfirmed window",
    );

    // Act + Assert: publish the overlay and poll until the window surfaces.
    let deadline = Instant::now() + RELOAD_WAIT_CEILING;
    let mut revision = 1;
    write_overlay_atomic(&overlay_path, OVERLAY_WINDOW, revision);
    let mut last_write = Instant::now();
    let mut observed = None;
    while Instant::now() < deadline {
        observed = context_length_of(&base_url, OVERLAY_MODEL).await;
        if observed == Some(u64::from(OVERLAY_WINDOW)) {
            break;
        }
        if last_write.elapsed() >= RESTIMULUS_INTERVAL {
            revision += 1;
            write_overlay_atomic(&overlay_path, OVERLAY_WINDOW, revision);
            last_write = Instant::now();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        observed,
        Some(u64::from(OVERLAY_WINDOW)),
        "the overlay's window did not surface on /v1/models within {RELOAD_WAIT_CEILING:?}",
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
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", xdg.path());
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

// -----------------------------------------------------------------
// Learned-capability kill-switch flip E2E
// -----------------------------------------------------------------

/// A byte-accurate openai-compat `unsupported_parameter` 400 whose
/// `/error/param` is `web_search` -- the capability the probe request carries
/// as a built-in tool `type`. The request-membership gate admits the negative
/// because the request derived that same key from the tool; the router learns
/// a negative for the rejecting target and de-prioritizes it.
const WEB_SEARCH_UNSUPPORTED_400: &str = r#"{"error":{"message":"Unsupported parameter: 'web_search' is not supported with this model.","type":"invalid_request_error","param":"web_search","code":"unsupported_parameter"}}"#;

/// A minimal, valid openai chat-completion success body for the clean tail
/// upstream.
fn openai_ok_body() -> Value {
    json!({
        "id": "cmpl-test",
        "object": "chat.completion",
        "created": 1,
        "model": "upstream-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
}

/// Two openai-compat providers behind wiremock (A rejects the probed
/// `web_search` feature, B succeeds) plus the learned-capability chain
/// alias. `enabled` flips the `[capability]` kill switch; `marker_alias`, when
/// set, adds a second alias whose appearance on `/v1/models` signals the
/// reloaded config went live.
fn kill_switch_config(
    a_url: &str,
    b_url: &str,
    enabled: bool,
    marker_alias: Option<&str>,
) -> String {
    let key_ref = common::file_ref("test-key");
    let marker = marker_alias.map_or_else(String::new, |a| format!("{a} = \"m_b\"\n"));
    format!(
        r#"
version = {CURRENT}
[server]
host = "127.0.0.1"
port = 0
strict_translation = false

[providers.prov_a]
kind = "openai-compat"
base_url = "{a_url}"
api_key_ref = "{key_ref}"

[providers.prov_b]
kind = "openai-compat"
base_url = "{b_url}"
api_key_ref = "{key_ref}"

[models.m_a]
provider = "prov_a"
upstream = "upstream-model"

[models.m_b]
provider = "prov_b"
upstream = "upstream-model"

[capability]
enabled = {enabled}
decay_hours = 48

[aliases]
probe-chain = ["m_a", "m_b"]
{marker}"#
    )
}

/// An openai upstream mounted at `POST /chat/completions` that always rejects
/// with the `response_format` unsupported-parameter 400.
async fn probe_upstream_reject() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock_path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(400)
                .insert_header("content-type", "application/json")
                .set_body_string(WEB_SEARCH_UNSUPPORTED_400),
        )
        .mount(&server)
        .await;
    server
}

/// An openai upstream mounted at `POST /chat/completions` that always succeeds.
async fn probe_upstream_ok() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock_path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(openai_ok_body()))
        .mount(&server)
        .await;
    server
}

/// Number of requests this upstream has received.
async fn mock_hits(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .map_or(0, |reqs| reqs.len())
}

/// POST a `/v1/chat/completions` request against `alias` carrying a
/// `web_search` built-in tool so `derive_feature_keys` yields `web_search`,
/// the capability target A rejects with a resolvable 400. Returns the
/// client-visible status.
async fn post_web_search_probe(
    client: &reqwest::Client,
    base_url: &str,
    alias: &str,
) -> reqwest::StatusCode {
    let body = json!({
        "model": alias,
        "messages": [{"role": "user", "content": "hi"}],
        "tools": [{"type": "web_search", "name": "t"}]
    });
    let resp = client
        .post(format!("{base_url}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("post /v1/chat/completions");
    resp.status()
}

/// The `state_key`s of the learned negatives currently resident in the live
/// router, read through the read-only `/status/health` panel.
async fn learned_negative_keys(base_url: &str) -> Vec<String> {
    let resp = reqwest::get(format!("{base_url}/status/health"))
        .await
        .expect("GET /status/health");
    assert!(resp.status().is_success(), "GET /status/health failed");
    let body: Value = resp.json().await.expect("status/health body is JSON");
    body["data"]["learned_negatives"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|e| e["state_key"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Poll until a learned negative for `state_key` becomes resident, re-issuing
/// the web_search probe each iteration so a self-identifying negative
/// is learned. Returns true once it surfaces, false on timeout.
async fn wait_for_learned_negative(
    client: &reqwest::Client,
    base_url: &str,
    alias: &str,
    state_key: &str,
    max_wait: Duration,
) -> bool {
    let deadline = Instant::now() + max_wait;
    while Instant::now() < deadline {
        let _ = post_web_search_probe(client, base_url, alias).await;
        if learned_negative_keys(base_url)
            .await
            .iter()
            .any(|k| k == state_key)
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// The learned-capability kill switch (`[capability] enabled`) flips OFF across
/// a hot-reload, and the learned registry carries over the reload.
///
/// Enabled-state behavior is OBSERVED FIRST (a learned negative is established
/// and A is de-prioritized so a second probe never dials it) so the test
/// asserts a real transition, not a state that was already flipped. The flip
/// is then driven by an idempotent restimulus loop
/// (`poll_alias_with_restimulus`) that atomically rewrites the SAME disabled
/// config on an interval while polling a behavior signal (the marker alias on
/// `/v1/models`) at 50ms -- never a single write, because the watcher's inotify
/// watch arms asynchronously after `/health` is already answering, so a single
/// write can race ahead of the armed watch and be silently lost.
///
/// After the flip the disabled router is live: A is dialed again (route-away is
/// off), AND the learned negative is STILL resident on `/status/health` --
/// `Router::carry_over_learned_from` imported it across the reload. That import
/// path (`LearnedCapabilityRegistry::import_entries`) resets every carried
/// entry's `in_flight` slot to false, so a re-probe that was in flight at
/// reload time can never latch across the swap (the reload cross-check for the
/// unreached-admission fix).
///
/// Overlay isolation is load-bearing for the carry-over assertion:
/// `carry_over_learned_from` CLEARS the learned registry whenever the catalog
/// overlay revision changes across a reload. A reload re-reads the overlay from
/// the process-global `XDG_CONFIG_HOME` path, while the test-only boot installs
/// an empty overlay (revision 0). Pinning `XDG_CONFIG_HOME` at an isolated
/// empty tree makes every reload resolve the same missing-file overlay
/// (revision 0 as well), so boot and reload share a revision and the flip
/// carries the registry over rather than invalidating it. Without the pin, a
/// sibling test toggling `XDG_CONFIG_HOME` mid-run flips the resolved overlay
/// revision under this test and the carry-over is silently cleared.
/// `serial_test::serial` is non-negotiable: this test mutates the
/// process-global `XDG_CONFIG_HOME`, so a sibling test reading or writing it in
/// parallel would see -- or stomp -- the pinned value.
#[tokio::test]
#[serial_test::serial]
async fn capability_kill_switch_flips_off_and_registry_carries_over_reload() {
    // Pin XDG at an isolated empty tree so every reload's overlay read resolves
    // to the same revision-0 (missing) overlay the boot installed -- otherwise
    // an overlay-revision change across the flip clears the learned registry.
    let xdg = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", xdg.path());

    // Arrange: A rejects the probed feature; B is the clean tail.
    let a = probe_upstream_reject().await;
    let b = probe_upstream_ok().await;
    let enabled_cfg = kill_switch_config(&a.uri(), &b.uri(), true, None);
    let (base_url, dir) = spawn_server_with_config_text(&enabled_cfg).await;
    let config_path = dir.path().join("config.toml");
    let client = reqwest::Client::new();

    // Observe enabled-state behavior FIRST: a self-identifying negative for A
    // is learned (kill switch ON), established before the flip.
    assert!(
        wait_for_learned_negative(
            &client,
            &base_url,
            "probe-chain",
            "m_a",
            RELOAD_WAIT_CEILING
        )
        .await,
        "with the kill switch ON, target A must learn a web_search negative",
    );
    let a_hits_after_learn = mock_hits(&a).await;
    assert!(
        a_hits_after_learn >= 1,
        "A was dialed at least once to learn"
    );

    // A is now de-prioritized: a further probe is served by the clean tail B
    // without dialing A again (route-away, the enabled-state behavior).
    let status = post_web_search_probe(&client, &base_url, "probe-chain").await;
    assert!(status.is_success(), "the clean tail B serves the probe");
    assert_eq!(
        mock_hits(&a).await,
        a_hits_after_learn,
        "with the kill switch ON, a learned negative de-prioritizes A (not re-dialed)",
    );

    // Act: flip the kill switch OFF via an idempotent restimulus loop, polling
    // the marker alias on /v1/models until the reloaded config goes live.
    let disabled_cfg = kill_switch_config(&a.uri(), &b.uri(), false, Some("kill-switch-off"));
    assert!(
        poll_alias_with_restimulus(
            &base_url,
            &config_path,
            disabled_cfg.as_bytes(),
            "kill-switch-off",
            RELOAD_WAIT_CEILING,
        )
        .await,
        "the disabled config did not go live within {RELOAD_WAIT_CEILING:?}",
    );

    // Assert reload carry-over: the learned negative is STILL resident after
    // the reload (imported by carry_over_learned_from, in_flight reset false).
    assert!(
        learned_negative_keys(&base_url)
            .await
            .iter()
            .any(|k| k == "m_a"),
        "the learned negative must carry over the reload (imported live)",
    );

    // Behavior flip confirmation: with the kill switch OFF, route-away is gone
    // -- A is dialed again on the next probe.
    let a_hits_before = mock_hits(&a).await;
    let status = post_web_search_probe(&client, &base_url, "probe-chain").await;
    assert!(
        status.is_success(),
        "B still serves the probe after the flip"
    );
    assert!(
        mock_hits(&a).await > a_hits_before,
        "with the kill switch OFF, the learned negative no longer de-prioritizes A",
    );
}

// -----------------------------------------------------------------
// Context-reduction kill switch (`[reduction] enabled`) flip E2E
// -----------------------------------------------------------------

/// The pretty (whitespace-laden) JSON document the probe request ships as a
/// stringified `function.arguments`. The reducer's ONLY legal effect on this
/// request is compacting exactly this string, which is what makes a
/// full-body byte comparison across the flip a sound oracle.
const PRETTY_ARGUMENTS: &str = "{\n  \"query\": \"rust\",\n  \"limit\": 10\n}";

/// What `PRETTY_ARGUMENTS` minifies to. Written out rather than derived so a
/// silent change in the minifier cannot move both sides of the assertion.
const COMPACT_ARGUMENTS: &str = "{\"query\":\"rust\",\"limit\":10}";

/// Single-provider config with `[reduction] enabled` set explicitly.
///
/// The value is ALWAYS written, never left to the schema default, so the
/// test states the enablement it asserts on and stays correct whichever way
/// the shipped default points. `marker_alias`, when set, adds a second alias
/// whose appearance on `/v1/models` signals the reloaded config went live.
fn reduction_config(upstream_uri: &str, enabled: bool, marker_alias: Option<&str>) -> String {
    let key_ref = common::file_ref("test-key");
    let marker = marker_alias.map_or_else(String::new, |a| format!("{a} = \"m_up\"\n"));
    format!(
        r#"
version = {CURRENT}
[server]
host = "127.0.0.1"
port = 0
strict_translation = false

[reduction]
enabled = {enabled}

[providers.up]
kind = "openai-compat"
base_url = "{upstream_uri}"
api_key_ref = "{key_ref}"

[models.m_up]
provider = "up"
upstream = "upstream-model"

[aliases]
reduce-me = "m_up"
{marker}"#
    )
}

/// Same shape as `reduction_config` but with an unknown `[server]` field
/// (`prt`, a typo of `port`) so `deny_unknown_fields` rejects the candidate
/// at parse time -- the reload never applies and the running router keeps
/// whatever `[reduction] enabled` it was built with.
///
/// `marker_alias` is the rejection ORACLE: it is an alias the candidate would
/// publish if it were ever accepted. Because the candidate cannot parse, that
/// alias must never surface on `/v1/models` no matter how many times the
/// write is delivered.
fn reduction_config_unparseable(upstream_uri: &str, enabled: bool, marker_alias: &str) -> String {
    let key_ref = common::file_ref("test-key");
    format!(
        r#"
version = {CURRENT}
[server]
host = "127.0.0.1"
port = 0
strict_translation = false
prt = 8080

[reduction]
enabled = {enabled}

[providers.up]
kind = "openai-compat"
base_url = "{upstream_uri}"
api_key_ref = "{key_ref}"

[models.m_up]
provider = "up"
upstream = "upstream-model"

[aliases]
reduce-me = "m_up"
{marker_alias} = "m_up"
"#
    )
}

/// Two-entry fallback chain (`chain = ["m_a", "m_b"]`) with `[reduction]
/// enabled` set explicitly. A is dialed first and fails; B is the tail that
/// serves the request. `max_attempts = 1` plus `[retry.classes.server-error]
/// retry = 0` keeps A to exactly ONE dial, so a slow A costs its delay once.
fn reduction_chain_config(a_url: &str, b_url: &str, enabled: bool, marker_alias: &str) -> String {
    let key_ref = common::file_ref("test-key");
    format!(
        r#"
version = {CURRENT}
[server]
host = "127.0.0.1"
port = 0
strict_translation = false

[retry]
max_attempts = 1
initial_backoff_ms = 0
jitter_ms = 0

[retry.classes.server-error]
retry = 0

[reduction]
enabled = {enabled}

[providers.prov_a]
kind = "openai-compat"
base_url = "{a_url}"
api_key_ref = "{key_ref}"

[providers.prov_b]
kind = "openai-compat"
base_url = "{b_url}"
api_key_ref = "{key_ref}"

[models.m_a]
provider = "prov_a"
upstream = "upstream-model"

[models.m_b]
provider = "prov_b"
upstream = "upstream-model"

[aliases]
chain = ["m_a", "m_b"]
{marker_alias} = "m_b"
"#
    )
}

/// POST one `/v1/chat/completions` carrying an assistant turn whose
/// stringified `function.arguments` is `PRETTY_ARGUMENTS` -- the reducer's
/// target. No `cache_control` anywhere, so the whole message list is the
/// mutable tail. `max_tokens` is above `probe_max_tokens` (default 1) so the
/// request is never treated as an availability probe (a probe fast-fails
/// instead of walking the fallback chain).
async fn post_reduction_probe(
    client: &reqwest::Client,
    base_url: &str,
    alias: &str,
) -> reqwest::StatusCode {
    let body = json!({
        "model": alias,
        "max_tokens": 50,
        "messages": [
            {"role": "user", "content": "hi"},
            {"role": "assistant", "tool_calls": [{
                "id": "call_1",
                "type": "function",
                "function": {"name": "search", "arguments": PRETTY_ARGUMENTS}
            }]}
        ]
    });
    let resp = client
        .post(format!("{base_url}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("post /v1/chat/completions");
    resp.status()
}

/// The raw body bytes of the LAST request this upstream received, as a
/// string. The egress body is what the reduction assertions are about, so
/// the oracle is the transmitted bytes rather than any router-side signal.
async fn last_request_body(server: &MockServer) -> String {
    let requests = server
        .received_requests()
        .await
        .expect("recording enabled on the mock upstream");
    let last = requests.last().expect("upstream received a request");
    String::from_utf8(last.body.clone()).expect("upstream body is utf-8")
}

/// The stringified `function.arguments` of the first tool_call in the first
/// message that carries one, read out of a transmitted egress body.
fn egress_tool_call_arguments(body: &str) -> String {
    let parsed: Value = serde_json::from_str(body).expect("egress body is JSON");
    parsed["messages"]
        .as_array()
        .expect("egress body carries a messages array")
        .iter()
        .find_map(|m| m.get("tool_calls"))
        .and_then(|calls| calls.get(0))
        .and_then(|call| call.get("function"))
        .and_then(|f| f.get("arguments"))
        .and_then(Value::as_str)
        .expect("egress body carries a stringified function.arguments")
        .to_string()
}

/// An openai upstream at `POST /chat/completions` that always succeeds,
/// after `delay` (zero for an immediate answer). The delay is a tokio sleep
/// inside wiremock, so it holds the request open without blocking the
/// current-thread test runtime.
async fn reduction_upstream_ok(delay: Duration) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock_path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(openai_ok_body())
                .set_delay(delay),
        )
        .mount(&server)
        .await;
    server
}

/// An openai upstream at `POST /chat/completions` that answers 500 after
/// `delay` -- a `server-error` class, which falls back to the chain tail.
async fn reduction_upstream_slow_500(delay: Duration) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock_path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(500)
                .set_body_json(json!({"error": {"message": "boom"}}))
                .set_delay(delay),
        )
        .mount(&server)
        .await;
    server
}

/// Flipping `[reduction] enabled` to false live is the kill switch: the very
/// next admitted request egresses BYTE-IDENTICAL passthrough bytes.
///
/// Enabled-state behavior is OBSERVED FIRST (the pretty `function.arguments`
/// reaches the upstream compacted) so the test asserts a real transition,
/// not a state that was already off. The flip is then driven by the
/// idempotent restimulus loop (`poll_alias_with_restimulus`) rather than a
/// single write, because the watcher's inotify watch arms asynchronously
/// after `/health` already answers, so a lone write can race ahead of the
/// armed watch and be silently lost.
///
/// The post-flip oracle is the FULL egress body, not just the arguments
/// field: substituting the compact string back into the pre-flip body must
/// reproduce the post-flip body byte-for-byte. That pins passthrough as
/// "nothing else about the transmitted bytes moved either", which an
/// arguments-only comparison cannot show.
#[tokio::test]
async fn reduction_kill_switch_flip_yields_byte_identical_passthrough() {
    // Arrange: reduction ON, one upstream that records what it receives.
    let up = reduction_upstream_ok(Duration::ZERO).await;
    let (base_url, dir) =
        spawn_server_with_config_text(&reduction_config(&up.uri(), true, None)).await;
    let config_path = dir.path().join("config.toml");
    let client = reqwest::Client::new();

    // Observe enabled-state behavior FIRST: the pretty arguments arrive
    // upstream compacted.
    let status = post_reduction_probe(&client, &base_url, "reduce-me").await;
    assert!(status.is_success(), "the upstream serves the probe");
    let reduced_body = last_request_body(&up).await;
    assert_eq!(
        egress_tool_call_arguments(&reduced_body),
        COMPACT_ARGUMENTS,
        "with reduction ON the pretty arguments must reach the upstream compacted",
    );

    // Act: flip the kill switch OFF, polling the marker alias until the
    // reloaded config is live.
    let disabled = reduction_config(&up.uri(), false, Some("reduction-off"));
    assert!(
        poll_alias_with_restimulus(
            &base_url,
            &config_path,
            disabled.as_bytes(),
            "reduction-off",
            RELOAD_WAIT_CEILING,
        )
        .await,
        "the reduction-disabled config did not go live within {RELOAD_WAIT_CEILING:?}",
    );

    // Assert: the next admitted request passes the pretty arguments through
    // verbatim, and the transmitted bytes differ from the reduced ones in
    // exactly that one string and nothing else.
    let status = post_reduction_probe(&client, &base_url, "reduce-me").await;
    assert!(
        status.is_success(),
        "the upstream serves the post-flip probe"
    );
    let passthrough_body = last_request_body(&up).await;
    assert_eq!(
        egress_tool_call_arguments(&passthrough_body),
        PRETTY_ARGUMENTS,
        "with reduction OFF the pretty arguments must egress byte-identical",
    );
    let compact_encoded =
        serde_json::to_string(COMPACT_ARGUMENTS).expect("encode compact arguments");
    let pretty_encoded = serde_json::to_string(PRETTY_ARGUMENTS).expect("encode pretty arguments");
    assert_eq!(
        reduced_body.replace(&compact_encoded, &pretty_encoded),
        passthrough_body,
        "the kill switch must change ONLY the minified string, nothing else in the egress bytes",
    );
}

/// How long the first chain entry holds the in-flight request open. The
/// pinning window: the reduction-disabling reload must land INSIDE it, and
/// the fallback preparation for the chain tail happens at its end. Wide
/// enough that a reload on a loaded box lands with room to spare.
const PIN_HOLD: Duration = Duration::from_secs(15);

/// Ceiling for the flip to go live while the request above is held. Bounded
/// strictly below `PIN_HOLD` so the ordering the test claims is enforced
/// rather than assumed: if the reload were slower than this the test fails
/// loudly instead of passing for the wrong reason (a fallback that happened
/// before the swap would satisfy an old-state assertion vacuously).
const PIN_FLIP_CEILING: Duration = Duration::from_secs(6);

/// In-flight semantics: a request admitted BEFORE the swap uses the OLD
/// router snapshot for its WHOLE fallback chain, so a reduction flip landing
/// mid-request does not change bytes already in that request's chain -- even
/// for a chain entry prepared after the swap.
///
/// The rig makes that observable: chain entry A holds the request open for
/// `PIN_HOLD` and then 500s (a `server-error`, which falls back); the kill
/// switch is flipped OFF while A holds; the chain tail B is prepared and
/// dialed only AFTER the swap. B's received bytes must still be REDUCED --
/// the pinned old snapshot governs.
///
/// Ordering is enforced, not assumed: A is confirmed dialed (the request is
/// admitted and pinned) before the flip is written, and the flip is required
/// to go live within `PIN_FLIP_CEILING` -- strictly less than `PIN_HOLD`, so
/// the swap provably precedes B's preparation.
#[tokio::test]
async fn request_pinned_before_the_swap_keeps_old_reduction_state() {
    // Arrange: reduction ON; A holds then fails, B is the clean tail.
    let a = reduction_upstream_slow_500(PIN_HOLD).await;
    let b = reduction_upstream_ok(Duration::ZERO).await;
    let (base_url, dir) = spawn_server_with_config_text(&reduction_chain_config(
        &a.uri(),
        &b.uri(),
        true,
        "pinned-off",
    ))
    .await;
    let config_path = dir.path().join("config.toml");
    let client = reqwest::Client::new();

    // Act 1: issue the request on a background task; it will sit in A for
    // PIN_HOLD before falling back to B.
    let in_flight = {
        let client = client.clone();
        let base_url = base_url.clone();
        tokio::spawn(async move { post_reduction_probe(&client, &base_url, "chain").await })
    };

    // Rendezvous: A has been dialed, so the request is admitted and its
    // router snapshot is pinned. Only now is the flip written.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && mock_hits(&a).await == 0 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        mock_hits(&a).await,
        1,
        "the request must be admitted and dialed at chain entry A before the flip",
    );

    // Act 2: flip the kill switch OFF and require it live well inside the
    // window A is holding the request open for.
    let disabled = reduction_chain_config(&a.uri(), &b.uri(), false, "pinned-off");
    assert!(
        poll_alias_with_restimulus(
            &base_url,
            &config_path,
            disabled.as_bytes(),
            "pinned-off",
            PIN_FLIP_CEILING,
        )
        .await,
        "the flip had to go live within {PIN_FLIP_CEILING:?} for the pinning ordering to hold",
    );

    // Assert: the in-flight request completes off the chain tail, and the
    // bytes B received are still REDUCED -- the pinned old snapshot governed
    // a fallback entry prepared after the swap.
    let status = in_flight.await.expect("in-flight request task");
    assert!(
        status.is_success(),
        "the chain tail B must serve the held request, got {status}",
    );
    assert_eq!(mock_hits(&b).await, 1, "B served exactly the held request");
    assert_eq!(
        egress_tool_call_arguments(&last_request_body(&b).await),
        COMPACT_ARGUMENTS,
        "a request pinned before the swap must keep old reduction state for its whole chain",
    );

    // Cross-check that the flip really is live for requests admitted AFTER
    // the swap -- otherwise the assertion above could hold because the flip
    // never applied at all.
    let status = post_reduction_probe(&client, &base_url, "chain").await;
    assert!(status.is_success(), "the post-swap request is served");
    assert_eq!(
        egress_tool_call_arguments(&last_request_body(&b).await),
        PRETTY_ARGUMENTS,
        "a request admitted after the swap must see the disabled reduction state",
    );
}

/// A reload candidate that would disable reduction but FAILS validation
/// leaves the live state untouched: the running router keeps reduction ON
/// and the next request still egresses compacted bytes. Same rejection seam
/// as `unknown_field_reload_rejected_old_router_stays_live` -- an unknown
/// `[server]` field -- asserted on the reduction behavior instead of the
/// alias table.
///
/// Non-vacuity, three guards, because a never-observed write and a declined
/// reload are otherwise indistinguishable from the outside:
///
/// 1. Reduction is OBSERVED live first (the pretty `function.arguments`
///    reaches the upstream compacted), so this asserts a real transition
///    rather than a state that was already off.
/// 2. The watch is proven ARMED AND DELIVERING on both sides of the rejected
///    window, by landing a valid marker config before and after it. A dropped
///    event cannot explain a green run: the pipeline demonstrably processes
///    writes to this path across the window.
/// 3. The rejected candidate carries a marker alias it WOULD publish if
///    accepted, and `hold_rejected_candidate` re-issues the write on the
///    restimulus cadence while asserting that alias never surfaces. The
///    candidate is therefore delivered repeatedly, and every delivery is
///    declined.
///
/// What this level cannot observe is the validation-failure LOG line: the
/// reload runs on a spawned server task, which thread-local tracing capture
/// does not reach. That half is pinned deterministically by
/// `server::reload_tests::unparseable_candidate_logs_its_rejection_and_keeps_reduction_on`,
/// which drives `handle_config_reload` directly under capture.
#[tokio::test]
async fn failed_reload_keeps_old_reduction_state() {
    // Arrange: reduction ON.
    let up = reduction_upstream_ok(Duration::ZERO).await;
    let (base_url, dir) =
        spawn_server_with_config_text(&reduction_config(&up.uri(), true, None)).await;
    let config_path = dir.path().join("config.toml");
    let client = reqwest::Client::new();

    // Guard 2 (first half): land a valid marker config. Its alias surfacing
    // proves the inotify watch is armed and a reload really does fire on a
    // write to this path.
    let armed = reduction_config(&up.uri(), true, Some("watch-armed"));
    assert!(
        poll_alias_with_restimulus(
            &base_url,
            &config_path,
            armed.as_bytes(),
            "watch-armed",
            RELOAD_WAIT_CEILING,
        )
        .await,
        "the watch never armed: the marker config did not go live within {RELOAD_WAIT_CEILING:?}",
    );

    // Guard 1: reduction is live in the router this reload will be rejected
    // against.
    let status = post_reduction_probe(&client, &base_url, "reduce-me").await;
    assert!(
        status.is_success(),
        "the upstream serves the baseline probe"
    );
    assert_eq!(
        egress_tool_call_arguments(&last_request_body(&up).await),
        COMPACT_ARGUMENTS,
        "reduction must be live before the rejected reload is written",
    );

    // Act + guard 3: hold the reduction-disabling, unparseable candidate on
    // disk across several restimulus cycles. `reject-marker` is the alias it
    // would publish if the reload were ever applied.
    hold_rejected_candidate(
        &base_url,
        &config_path,
        reduction_config_unparseable(&up.uri(), false, "reject-marker").as_bytes(),
        Some("reject-marker"),
        Some("reduce-me"),
        REJECT_HOLD_WINDOW,
    )
    .await;

    // Assert: the rejected candidate never took effect -- reduction is still
    // ON, so the pretty arguments still reach the upstream compacted.
    let status = post_reduction_probe(&client, &base_url, "reduce-me").await;
    assert!(status.is_success(), "the upstream serves the probe");
    assert_eq!(
        egress_tool_call_arguments(&last_request_body(&up).await),
        COMPACT_ARGUMENTS,
        "a rejected reload must keep the previous reduction state live",
    );

    // Guard 2 (second half): a valid marker config still goes live, so the
    // reload pipeline was processing writes to this path throughout -- the
    // green run above is a rejection, not a dead watcher.
    let after = reduction_config(&up.uri(), true, Some("post-reject"));
    assert!(
        poll_alias_with_restimulus(
            &base_url,
            &config_path,
            after.as_bytes(),
            "post-reject",
            RELOAD_WAIT_CEILING,
        )
        .await,
        "no reload landed after the rejected candidate, so the watcher was not delivering",
    );
}
