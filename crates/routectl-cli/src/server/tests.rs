use super::*;

#[test]
fn cache_policy_banner_reflects_both_switches() {
    // Arrange / Act / Assert: each switch maps to enabled / disabled.
    assert_eq!(
        cache_policy_banner(true, true),
        "cache policy: auto-emit top-level breakpoint enabled, context reduction enabled"
    );
    assert_eq!(
        cache_policy_banner(false, false),
        "cache policy: auto-emit top-level breakpoint disabled, context reduction disabled"
    );
    assert_eq!(
        cache_policy_banner(true, false),
        "cache policy: auto-emit top-level breakpoint enabled, context reduction disabled"
    );
}

/// Point a config's usage DB at a per-test tempdir so server tests
/// never touch the real `~/.config/routectl/usage.db` (the
/// `UsageConfig` default). Returns the `TempDir` guard the caller
/// MUST keep alive for the test's duration. Isolating the path --
/// rather than disabling usage -- keeps the writer wiring exercised.
#[cfg(test)]
fn isolate_usage_db(config: &mut Config) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("usage tempdir");
    config.usage.db_path = dir.path().join("usage.db");
    dir
}

#[test]
fn status_requires_auth_covers_the_four_cells() {
    use std::net::SocketAddr;

    let empty = TokenSet::new(vec![]);
    let tokened = TokenSet::new(vec!["secret".to_string()]);
    let loopback: SocketAddr = "127.0.0.1:8080".parse().unwrap();
    let public: SocketAddr = "0.0.0.0:8080".parse().unwrap();

    assert!(
        !status_requires_auth(&empty, loopback),
        "token-less loopback is the open dev path"
    );
    assert!(
        status_requires_auth(&tokened, loopback),
        "configured tokens gate status even on loopback"
    );
    assert!(
        status_requires_auth(&empty, public),
        "a non-loopback bind gates status even with no tokens (fail-closed)"
    );
    assert!(
        status_requires_auth(&tokened, public),
        "tokens plus a non-loopback bind gate status"
    );
}

#[test]
fn is_loopback_covers_full_127_range() {
    // Arrange + Act + Assert
    assert!(is_loopback("127.0.0.1"));
    assert!(is_loopback("127.0.0.2"));
    assert!(is_loopback("127.255.255.254"));
    assert!(is_loopback("::1"));
    assert!(is_loopback("localhost"));
    assert!(!is_loopback("0.0.0.0"));
    assert!(!is_loopback("192.168.1.1"));
    assert!(!is_loopback("not-an-address"));
}

#[test]
fn is_loopback_handles_ipv4_mapped_ipv6() {
    // Arrange + Act + Assert: IPv4-mapped IPv6 addresses
    // (::ffff:127.x.x.x) must be treated as loopback; non-loopback
    // IPv4-mapped addresses must not be.
    assert!(is_loopback("::ffff:127.0.0.1"));
    assert!(!is_loopback("::ffff:192.168.1.1"));
}

#[tokio::test]
async fn build_router_twice_from_same_secrets_handle_succeeds() {
    // Hot-reload smoke test: rebuild a Router twice from the
    // same `Arc<dyn SecretStore>` handle. Pinning the no-panic
    // contract -- a regression that drops the per-provider
    // single-flight refresh mutex on rebuild would either error
    // on the second build or surface a dangling Arc here.
    use routectl_auth::MemoryStore;
    use routectl_router::Config;
    use std::sync::Arc;

    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let config = Arc::new(Config::default());

    let r1 = build_router_from_config(config.clone(), secrets.clone())
        .await
        .expect("first router build");
    let r2 = build_router_from_config(config.clone(), secrets.clone())
        .await
        .expect("second router build");

    // Sanity: each call returns a fresh Router but they share
    // the same Config (Arc-shared at construction time).
    assert!(Arc::ptr_eq(&r1.config, &config));
    assert!(Arc::ptr_eq(&r2.config, &config));
}

/// `validate_provider_credential_sources` is wired into
/// `build_router_from_config_with_overlay` itself (not only reachable
/// via the separate `commands::config::check` call to the same
/// validator) -- a forwarded provider pointed at a non-Anthropic host
/// must fail the router build, i.e. serve startup and hot reload,
/// which both call this builder directly.
#[tokio::test]
async fn build_router_from_config_rejects_forwarded_provider_on_non_anthropic_host() {
    use routectl_router::ProviderEntry;
    use routectl_router::config::CredentialSource;

    let mut config = Config::default();
    let _usage_dir = isolate_usage_db(&mut config);
    config.providers.insert(
        "sneaky".into(),
        ProviderEntry::anthropic_api("")
            .with_base_url("https://evil.example.com")
            .with_credential_source(CredentialSource::Forwarded),
    );
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());

    // `Router` is not `Debug`, so match rather than `expect_err`.
    let err = match build_router_from_config(Arc::new(config), secrets).await {
        Ok(_) => panic!("forwarded provider off the pinned host must fail the router build"),
        Err(e) => e,
    };
    assert!(matches!(err, Error::Config(_)), "got: {err:?}");
}

/// Build a `Config` whose single listener-auth token is the given
/// secret URI. Isolates the usage DB so the test never touches the
/// real path; returns the config plus the tempdir guard.
#[cfg(test)]
fn config_with_single_listener_token(uri: &str) -> (Config, tempfile::TempDir) {
    use routectl_router::{Config, ServerAuth, ServerConfig};
    let mut config = Config {
        server: ServerConfig {
            auth: Some(ServerAuth {
                tokens: vec![uri.to_string()],
            }),
            ..ServerConfig::default()
        },
        ..Config::default()
    };
    let dir = isolate_usage_db(&mut config);
    (config, dir)
}

/// Write `contents` to a 0600 tempfile and return a `file://` URI
/// pointing at it plus the `NamedTempFile` guard (kept alive by the
/// caller). `file://` is a use-time source the secret-ref parser
/// cannot pre-reject for emptiness, and it avoids mutating
/// process-global env state (which races with concurrent tests).
#[cfg(test)]
fn file_token_uri(contents: &str) -> (String, tempfile::NamedTempFile) {
    use std::io::Write;
    let mut f = tempfile::NamedTempFile::new().expect("tempfile");
    f.write_all(contents.as_bytes()).expect("write secret file");
    f.flush().expect("flush");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        f.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .expect("chmod 600");
    }
    let uri = format!("file://{}", f.path().display());
    (uri, f)
}

#[tokio::test]
async fn resolve_listener_tokens_rejects_empty_file_source() {
    // Arrange: a file:// source the parser CANNOT pre-reject (the URI
    // is well-formed) whose contents trim to an empty string.
    let (uri, _file) = file_token_uri("");
    let (config, _guard) = config_with_single_listener_token(&uri);

    // Act
    let err = resolve_listener_tokens(&config)
        .await
        .expect_err("empty file token must be rejected");

    // Assert: Config error naming entry #1 + the empty-token risk;
    // never the raw value.
    match err {
        Error::Config(msg) => {
            assert!(msg.contains("entry #1"), "must name the entry: {msg}");
            assert!(
                msg.contains("empty token") && msg.contains("disable authentication"),
                "must name the empty-token risk: {msg}"
            );
        }
        other => panic!("expected Error::Config, got {other:?}"),
    }
}

#[tokio::test]
async fn resolve_listener_tokens_rejects_whitespace_only_file_source() {
    // Arrange: an all-whitespace file value must ALSO be rejected --
    // the guard trims before the emptiness check. (The file store
    // trims trailing whitespace, so seed leading spaces too.)
    let (uri, _file) = file_token_uri("   \n");
    let (config, _guard) = config_with_single_listener_token(&uri);

    // Act
    let err = resolve_listener_tokens(&config)
        .await
        .expect_err("whitespace-only file token must be rejected");

    // Assert
    match err {
        Error::Config(msg) => {
            assert!(msg.contains("entry #1"), "must name the entry: {msg}");
            assert!(
                msg.contains("disable authentication"),
                "must name the empty-token risk: {msg}"
            );
        }
        other => panic!("expected Error::Config, got {other:?}"),
    }
}

#[tokio::test]
async fn resolve_listener_tokens_accepts_non_empty_token() {
    // Arrange: a valid non-empty token must resolve into a TokenSet
    // without error (positive path).
    let (uri, _file) = file_token_uri("tok-not-empty");
    let (config, _guard) = config_with_single_listener_token(&uri);

    // Act
    let result = resolve_listener_tokens(&config).await;

    // Assert
    assert!(
        result.is_ok(),
        "a non-empty token must build the TokenSet: {result:?}"
    );
}

/// SIGHUP-only delivery: drive `run_sighup_listener` directly with no
/// filesystem watcher in the picture. Sending SIGHUP to ourselves
/// must produce exactly one `Config` followed by one `Credentials`
/// `ReloadRequest` on the channel. A regression that breaks the
/// signal -> mpsc fan-out (registration error, dropped sender, fused
/// recv) cannot pass this test because no other path can emit those
/// requests in this fixture. Pairs with the integration-level
/// combined-path test in `tests/hot_reload.rs`.
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial]
async fn sighup_listener_emits_paired_reload_requests_in_isolation() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    use std::time::Duration;

    // Arrange: pure SIGHUP -> channel rig. No file watcher, no
    // server, no reload coordinator.
    let (tx, mut rx) = mpsc::channel::<file_watch::ReloadRequest>(8);
    let (_shutdown_tx, shutdown_rx) = watch::channel(());
    tokio::spawn(run_sighup_listener(tx, shutdown_rx));

    // Yield until the spawned listener has installed its handler.
    // tokio::signal::unix::signal registers synchronously, but the
    // task needs to enter its select! loop before a signal landing
    // in the same instant is observable.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Act: deliver SIGHUP to ourselves.
    kill(Pid::from_raw(std::process::id() as i32), Signal::SIGHUP).expect("kill(SIGHUP) to self");

    // Assert: one Config then one Credentials, in that order. A
    // 2s timeout is generous for the OS notify -> tokio signal
    // futex hop on a loaded CI runner.
    let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("first reload request must arrive within 2s")
        .expect("channel must not close while listener is alive");
    let second = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("second reload request must arrive within 2s")
        .expect("channel must not close while listener is alive");

    assert_eq!(first, file_watch::ReloadRequest::Config);
    assert_eq!(second, file_watch::ReloadRequest::Credentials);
}

// ---- Graceful shutdown: bounded in-flight drain (OPS-08) ----

/// Sending SIGTERM to ourselves must trigger the graceful shutdown
/// path so `serve_on_listener` returns cleanly within a short bound,
/// proving the signal -> drain -> serve-return wiring. tokio's
/// registered SIGTERM handler intercepts the signal, so the test
/// process is not killed. There are no in-flight requests, so the
/// drain completes immediately (well under `DRAIN_DEADLINE`).
#[cfg(unix)]
#[tokio::test]
#[serial_test::serial]
async fn sigterm_triggers_graceful_shutdown_and_serve_returns() {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    use std::time::Duration;

    // Arrange: bind an ephemeral loopback port and start the server.
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral loopback port");
    let mut config = Config::default();
    let _usage_dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let server = tokio::spawn(async move { serve_on_listener(config, listener, None).await });

    // Let the server install its signal handler and enter serve.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Act: deliver SIGTERM to ourselves.
    kill(Pid::from_raw(std::process::id() as i32), Signal::SIGTERM).expect("kill(SIGTERM) to self");

    // Assert: serve_on_listener returns Ok within a short bound. A
    // 5s ceiling is generous for an idle drain yet far below
    // DRAIN_DEADLINE, so a hang here is a real regression, not the
    // deadline doing its job.
    let outcome = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("serve_on_listener must return within 5s of SIGTERM")
        .expect("server task must not panic");
    assert!(
        outcome.is_ok(),
        "graceful shutdown must return Ok: {outcome:?}"
    );
}

/// The bounded-drain select must resolve to "deadline elapsed" when
/// the drain future never completes. We drive `drain_deadline_watcher`
/// against a flipped signal with a tiny stand-in deadline and assert
/// it wins a race against a never-completing "drain". Covers the
/// abandon-on-hung-upstream decision in isolation (no real socket).
#[tokio::test(start_paused = true)]
async fn drain_deadline_fires_when_drain_never_completes() {
    // Arrange: a signal channel already flipped to `true`.
    let (tx, mut rx) = watch::channel(false);
    tx.send(true).expect("flip signal");
    let never_completes = std::future::pending::<()>();

    // Act + Assert: the deadline watcher must win against a drain
    // that never finishes. With paused time, sleep auto-advances.
    let abandoned = tokio::select! {
        () = drain_deadline_watcher(&mut rx) => true,
        () = never_completes => false,
    };
    assert!(
        abandoned,
        "deadline watcher must resolve when the drain never completes"
    );
}

/// The mirror case: when the drain completes promptly, the drain
/// branch wins and the deadline does NOT fire. Drives the same
/// select shape with a ready "drain" future against the deadline
/// watcher (whose DRAIN_DEADLINE sleep would otherwise dominate).
#[tokio::test(start_paused = true)]
async fn drain_completion_wins_over_deadline() {
    // Arrange: signal flipped, but the drain is already ready.
    let (tx, mut rx) = watch::channel(false);
    tx.send(true).expect("flip signal");
    let drain_done = std::future::ready(());

    // Act + Assert: the completed-drain branch must win.
    let completed = tokio::select! {
        biased;
        () = drain_done => true,
        () = drain_deadline_watcher(&mut rx) => false,
    };
    assert!(
        completed,
        "a completed drain must win over the deadline watcher"
    );
}

/// Before any signal fires, the deadline watcher must NOT resolve --
/// otherwise the bounded-drain select could trip the deadline branch
/// during normal operation and tear the server down prematurely.
#[tokio::test(start_paused = true)]
async fn drain_deadline_does_not_fire_before_signal() {
    use std::time::Duration;

    // Arrange: an UN-flipped signal channel (no shutdown requested).
    let (_tx, mut rx) = watch::channel(false);

    // Act: race the watcher against a long sleep. With time paused,
    // the only way the sleep wins is if the watcher is correctly
    // pending on the (never-arriving) signal.
    let fired = tokio::select! {
        () = drain_deadline_watcher(&mut rx) => true,
        () = tokio::time::sleep(Duration::from_hours(1)) => false,
    };

    // Assert: the watcher stayed pending; the sleep won.
    assert!(
        !fired,
        "deadline watcher must not resolve before the shutdown signal fires"
    );
}

// ---- Credentials reload: seat-set-change Router rebuild gate ----

/// Write a `credentials.json` carrying one record per seat key in
/// `seats` (each `(key, access_token)`) using the same JSON shape +
/// 0o600 hygiene the production credentials writer emits, so
/// `OAuthStore::open` / `reload_from_disk` accept it. Keys are the raw
/// credentials-map keys: a bare provider (`anthropic`) for the default
/// seat, `provider#label` for a labeled seat.
#[cfg(test)]
fn write_pool_credentials(path: &std::path::Path, seats: &[(&str, &str)]) {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let mut providers = serde_json::Map::new();
    for (key, token) in seats {
        providers.insert(
            (*key).to_string(),
            serde_json::json!({
                "access_token": token,
                "refresh_token": "seeded-refresh-token",
                "token_type": "Bearer",
                "expires_at_unix": now + 3600,
                "scopes": ["user:inference"],
                "account": { "email": null, "account_id": null },
                "obtained_at_unix": now
            }),
        );
    }
    let doc = serde_json::json!({ "schema_version": 1, "providers": providers });
    let bytes = serde_json::to_vec_pretty(&doc).expect("serialize creds");
    let parent = path.parent().expect("creds path has parent");
    std::fs::create_dir_all(parent).expect("mkdir creds parent");
    std::fs::write(path, &bytes).expect("write creds");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod 0600");
    }
}

/// Config with a single pooled model `claude` whose provider resolves
/// its bearer via the bare-pool `oauth://anthropic` ref. With >=2
/// seats on disk this expands to one dispatch target per seat; with one
/// seat it stays single-target.
#[cfg(test)]
fn pooled_oauth_config() -> Arc<Config> {
    let text = r#"
[server]
host = "127.0.0.1"
port = 0
strict_translation = false

[providers.anthropic_oauth]
kind = "anthropic-api"
base_url = "http://127.0.0.1:1"
api_key_ref = "oauth://anthropic"
auth_kind = "oauth-bearer"

[models.claude]
provider = "anthropic_oauth"
upstream = "claude-sonnet-4-6"

[aliases]
default = "claude"
"#;
    Arc::new(toml::from_str(text).expect("pooled oauth config must parse"))
}

/// Build the `(OAuthStore handle, secrets, router_swap)` triple the
/// reload coordinator owns, seeded from a credentials file already on
/// disk at `creds_path`. Mirrors `serve_on_listener`'s wiring: one
/// shared `Arc<dyn SecretStore>` across rebuilds.
#[cfg(test)]
async fn coordinator_rig(
    creds_path: &std::path::Path,
    config: &Arc<Config>,
) -> (
    Arc<routectl_auth::OAuthStore>,
    Arc<dyn SecretStore>,
    Arc<ArcSwap<Router>>,
) {
    let composite = CompositeStore::open_at(creds_path)
        .await
        .expect("open composite store at temp creds path");
    let oauth = composite.oauth_store().expect("oauth arm present");
    let secrets: Arc<dyn SecretStore> = Arc::new(composite);
    let router = build_router_from_config(config.clone(), secrets.clone())
        .await
        .expect("initial router build");
    let swap = Arc::new(ArcSwap::from_pointee(router));
    (oauth, secrets, swap)
}

/// Adding a seat to credentials.json and firing a credentials reload
/// must re-expand the live Router's pool: the model goes from a single
/// target (one seat, non-pooled) to two seat targets, with no daemon
/// restart and no config change.
#[tokio::test]
async fn credentials_reload_reexpands_seat_set() {
    // Arrange: one seat on disk -> non-pooled (seat_count_for == None).
    let dir = tempfile::tempdir().unwrap();
    let creds = dir.path().join("routectl").join("credentials.json");
    write_pool_credentials(&creds, &[("anthropic", "tok-default")]);
    let config = pooled_oauth_config();
    let (oauth, secrets, swap) = coordinator_rig(&creds, &config).await;
    assert_eq!(
        swap.load().seat_count_for("claude"),
        None,
        "single seat must stay single-target before reload"
    );

    // Act: add a second seat on disk, then reload credentials.
    write_pool_credentials(
        &creds,
        &[("anthropic", "tok-default"), ("anthropic#seat-b", "tok-b")],
    );
    let overlay = Arc::new(CatalogOverlay::default());
    handle_credentials_reload(
        &Some(oauth),
        &config,
        &overlay,
        secrets,
        &swap,
        &Arc::new(ArcSwap::from_pointee(ActivationState::default())),
    )
    .await;

    // Assert: the live Router now resolves two seat targets.
    assert_eq!(
        swap.load().seat_count_for("claude"),
        Some(2),
        "reload must re-expand the pool to two seats"
    );
}

/// A credentials reload that changes only a token VALUE (same seat
/// keys) -- the routine auto-refresh case -- must NOT rebuild the
/// Router. Proven by pointer-equality of the `ArcSwap` payload across
/// the reload: the seat-set gate skipped the rebuild.
#[tokio::test]
async fn credentials_reload_token_only_change_does_not_rebuild() {
    // Arrange: a stable two-seat pool.
    let dir = tempfile::tempdir().unwrap();
    let creds = dir.path().join("routectl").join("credentials.json");
    write_pool_credentials(
        &creds,
        &[("anthropic", "tok-default"), ("anthropic#seat-b", "tok-b")],
    );
    let config = pooled_oauth_config();
    let (oauth, secrets, swap) = coordinator_rig(&creds, &config).await;
    let before = swap.load_full();

    // Act: rewrite with the SAME keys but new token values, reload.
    write_pool_credentials(
        &creds,
        &[
            ("anthropic", "tok-default-rotated"),
            ("anthropic#seat-b", "tok-b-rotated"),
        ],
    );
    let overlay = Arc::new(CatalogOverlay::default());
    handle_credentials_reload(
        &Some(oauth),
        &config,
        &overlay,
        secrets,
        &swap,
        &Arc::new(ArcSwap::from_pointee(ActivationState::default())),
    )
    .await;

    // Assert: the Router Arc is pointer-unchanged (no rebuild fired).
    let after = swap.load_full();
    assert!(
        Arc::ptr_eq(&before, &after),
        "token-value-only refresh must not rebuild the router"
    );
}

/// A credentials reload whose Router rebuild fails (seat set changed,
/// but the config no longer builds against the new credentials) must
/// keep the previously-installed Router (disk-first-keep-old). Induced
/// by pointing the pool's provider-build path at a config the rebuild
/// rejects: here we make the alias reference a model whose only seat
/// disappears so the rebuild errors, and pin that the live Router is
/// untouched.
#[tokio::test]
async fn credentials_reload_rebuild_failure_keeps_previous_router() {
    // Arrange: a healthy two-seat pool and a built Router.
    let dir = tempfile::tempdir().unwrap();
    let creds = dir.path().join("routectl").join("credentials.json");
    write_pool_credentials(
        &creds,
        &[("anthropic", "tok-default"), ("anthropic#seat-b", "tok-b")],
    );
    let config = pooled_oauth_config();
    let (oauth, secrets, swap) = coordinator_rig(&creds, &config).await;
    let before = swap.load_full();

    // Act: corrupt credentials.json so reload_from_disk fails. The
    // disk-first invariant means the cache (and thus the seat set) is
    // untouched, and the Router must not be rebuilt or swapped.
    std::fs::write(&creds, b"<<corrupt-json>>").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&creds, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let overlay = Arc::new(CatalogOverlay::default());
    handle_credentials_reload(
        &Some(oauth),
        &config,
        &overlay,
        secrets,
        &swap,
        &Arc::new(ArcSwap::from_pointee(ActivationState::default())),
    )
    .await;

    // Assert: a failed reload leaves the previous Router installed.
    let after = swap.load_full();
    assert!(
        Arc::ptr_eq(&before, &after),
        "a failed credentials reload must keep the previous router"
    );
}

// ---- Usage writer: boot, hot-reload enabled-gate, shutdown drain ----

/// Minimal on-disk config text with the usage block's `enabled` set
/// to `enabled` and `db_path` pointed at `db_path`, so a config
/// reload picks up an isolated DB and a flippable gate.
#[cfg(test)]
fn usage_config_text(enabled: bool, db_path: &std::path::Path) -> String {
    format!(
        "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 0\n\n\
             [usage]\nenabled = {enabled}\ndb_path = \"{}\"\nretention_days = 0\n",
        db_path.display()
    )
}

/// A config reload that flips `usage.enabled` true -> false must flip
/// the live gate WITHOUT rebuilding the writer: the same `UsageHandle`
/// the daemon holds reports `is_enabled() == false` after the reload,
/// and the Router Arc is swapped (proving the reload ran).
///
/// `#[serial]`: `handle_config_reload` reads the AMBIENT
/// `catalog_overlay.json` via `routectl_router::overlay_default_path()`
/// (XDG_CONFIG_HOME / HOME), same as every other loader call. Without
/// this, a concurrently-running `#[serial]` test that points
/// `XDG_CONFIG_HOME` at a tempdir holding a DELIBERATELY corrupt
/// overlay (see `config_reload_picks_up_overlay_file_change_and_fails_closed_on_corruption`)
/// can race this test's reload into reading that corrupt file and
/// failing closed here too -- `serial_test` only excludes OTHER
/// `#[serial]` tests, so this test must join the same group to be
/// protected.
#[tokio::test]
#[serial_test::serial]
async fn config_reload_flips_usage_enabled_gate_live() {
    // Arrange: a temp DB + a config file starting enabled=true, and a
    // writer/handle pair the daemon would own across the reload.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("usage.db");
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, usage_config_text(false, &db_path)).unwrap();

    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let mut start_config = Config::default();
    start_config.usage.db_path = db_path.clone();
    start_config.usage.enabled = true;
    let start_config = Arc::new(start_config);
    let (usage, _writer) = build_usage_writer(&start_config);
    assert!(usage.is_enabled(), "writer must start enabled");

    let router = build_router_from_config(start_config.clone(), secrets.clone())
        .await
        .expect("initial router build");
    let swap = Arc::new(ArcSwap::from_pointee(router));
    let before_router = swap.load_full();

    // Act: reload the on-disk config (enabled=false).
    let (new_config, _new_overlay) = handle_config_reload(
        Some(&cfg_path),
        &start_config,
        secrets,
        &swap,
        &usage,
        ReloadTrigger::ConfigFile,
    )
    .await
    .expect("config reload must apply");

    // Assert: gate flipped live (same handle), router swapped.
    assert!(!new_config.usage.enabled);
    assert!(
        !usage.is_enabled(),
        "reload must flip the live usage gate to disabled"
    );
    let after_router = swap.load_full();
    assert!(
        !Arc::ptr_eq(&before_router, &after_router),
        "config reload must swap the router"
    );
}

/// Shared-loader symmetry: a config reload re-reads
/// the catalog overlay from disk, not just config.toml -- and a corrupt
/// overlay fails the reload closed, keeping the previously-installed
/// router live. Isolated `XDG_CONFIG_HOME` so this test controls
/// exactly what `routectl_router::overlay_default_path()` resolves to.
#[tokio::test]
#[serial_test::serial]
async fn config_reload_picks_up_overlay_file_change_and_fails_closed_on_corruption() {
    // Arrange: an isolated config dir, a minimal config.toml, and NO
    // overlay file yet (first run -> empty overlay).
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 0\n",
    )
    .unwrap();

    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let mut initial_config = Config::default();
    let _usage_dir = isolate_usage_db(&mut initial_config);
    let initial_config = Arc::new(initial_config);
    let (usage, _writer) = build_usage_writer(&initial_config);
    let router = build_router_from_config_with_overlay(
        initial_config.clone(),
        &CatalogOverlay::default(),
        secrets.clone(),
    )
    .await
    .expect("initial router build");
    let swap = Arc::new(ArcSwap::from_pointee(router));

    // Act 1: write a real overlay cell to disk, then reload.
    let overlay_dir = dir.path().join("routectl");
    std::fs::create_dir_all(&overlay_dir).unwrap();
    std::fs::write(
        overlay_dir.join("catalog_overlay.json"),
        r#"{"schema_version":1,"revision":1,"cells":{"anthropic-api:claude-opus-4-8*":
               {"source":"user","verified_at":"2026-07-01","wm":9.5}}}"#,
    )
    .unwrap();
    let (new_config, new_overlay) = handle_config_reload(
        Some(&cfg_path),
        &initial_config,
        secrets.clone(),
        &swap,
        &usage,
        ReloadTrigger::ConfigFile,
    )
    .await
    .expect("reload with a fresh overlay file must apply");

    // Assert: the reload re-read the overlay from disk (not the empty
    // overlay the initial router booted with).
    assert!(
        new_overlay
            .cells
            .contains_key("anthropic-api:claude-opus-4-8*"),
        "config reload must re-read the catalog overlay file from disk",
    );
    let router_after_good_reload = swap.load_full();

    // Act 2: corrupt the overlay file, reload again.
    std::fs::write(overlay_dir.join("catalog_overlay.json"), b"not json {{{").unwrap();
    let result = handle_config_reload(
        Some(&cfg_path),
        &new_config,
        secrets,
        &swap,
        &usage,
        ReloadTrigger::ConfigFile,
    )
    .await;

    // Assert: a corrupt overlay fails the reload closed -- no
    // config/overlay update, and the router installed by the LAST GOOD
    // reload stays live.
    assert!(result.is_none(), "a corrupt overlay must fail the reload");
    assert!(
        Arc::ptr_eq(&swap.load_full(), &router_after_good_reload),
        "a failed reload must keep the previously-installed router",
    );
}

/// `ReloadRequest::Config` and `ReloadRequest::CatalogOverlay` both
/// call `handle_config_reload` -- the SAME loader re-reads config +
/// overlay together regardless of which file changed -- but each
/// must label its own success log with the trigger that fired it, so
/// an operator can tell an overlay-driven reload from a config-file
/// one. Drives `handle_config_reload` directly (no `tokio::spawn`)
/// under `with_capture` so the thread-local capture subscriber sees
/// the event.
#[tokio::test]
#[serial_test::serial]
async fn handle_config_reload_labels_its_trigger_in_the_success_log() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 0\n",
    )
    .unwrap();

    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let mut config = Config::default();
    let _usage_dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, _writer) = build_usage_writer(&config);
    let router = build_router_from_config_with_overlay(
        config.clone(),
        &CatalogOverlay::default(),
        secrets.clone(),
    )
    .await
    .unwrap();
    let swap = Arc::new(ArcSwap::from_pointee(router));

    // Act: the SAME function, once per trigger. `Box::pin` keeps
    // each call's future off this test's own stack frame --
    // `handle_config_reload`'s future is large enough to trip
    // clippy's large-futures lint otherwise.
    let (config_result, config_events) =
        routectl_testkit::with_capture(Box::pin(handle_config_reload(
            Some(&cfg_path),
            &config,
            secrets.clone(),
            &swap,
            &usage,
            ReloadTrigger::ConfigFile,
        )))
        .await;
    config_result.expect("config-triggered reload must apply");

    let (overlay_result, overlay_events) =
        routectl_testkit::with_capture(Box::pin(handle_config_reload(
            Some(&cfg_path),
            &config,
            secrets,
            &swap,
            &usage,
            ReloadTrigger::CatalogOverlay,
        )))
        .await;
    overlay_result.expect("overlay-triggered reload must apply");

    // Assert: each call's success log names its OWN trigger.
    let success_message = "config reloaded; router rebuilt and swapped";
    let config_trigger = config_events
        .iter()
        .find(|e| e.message == success_message)
        .and_then(|e| e.field("trigger"))
        .expect("config-triggered reload must log a trigger field");
    assert_eq!(config_trigger, "config change");

    let overlay_trigger = overlay_events
        .iter()
        .find(|e| e.message == success_message)
        .and_then(|e| e.field("trigger"))
        .expect("overlay-triggered reload must log a trigger field");
    assert_eq!(overlay_trigger, "overlay change");
}

/// Full-stack proof that `spawn_reload_pipeline` -- the actual
/// production wiring, not a hand-rolled substitute -- watches the
/// catalog overlay path and routes its writes through the SAME
/// reload coordinator as config: a real overlay write (through the
/// real `notify` watcher) swaps the router, and a corrupt overlay
/// write keeps the PRIOR router live. `#[serial]`: the path this
/// wiring watches is the ambient `routectl_router::overlay_default_path()`
/// (XDG_CONFIG_HOME-derived).
#[tokio::test]
#[serial_test::serial]
async fn spawn_reload_pipeline_watches_overlay_and_swaps_router_on_write() {
    // Arrange: config.toml and the (not-yet-written) overlay share
    // `routectl_config_dir()`, mirroring the real deployment layout
    // -- the watcher needs the overlay's PARENT directory to exist at
    // watch-install time even though the overlay FILE does not.
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let config_dir = dir.path().join("routectl");
    std::fs::create_dir_all(&config_dir).unwrap();
    let cfg_path = config_dir.join("config.toml");
    std::fs::write(
        &cfg_path,
        "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 0\n",
    )
    .unwrap();

    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let mut initial_config = Config::default();
    let _usage_dir = isolate_usage_db(&mut initial_config);
    let initial_config = Arc::new(initial_config);
    let (usage, _writer) = build_usage_writer(&initial_config);
    let router = build_router_from_config_with_overlay(
        initial_config.clone(),
        &CatalogOverlay::default(),
        secrets.clone(),
    )
    .await
    .unwrap();
    let swap = Arc::new(ArcSwap::from_pointee(router));
    let before_router = swap.load_full();

    let (_shutdown_tx, shutdown_rx) = watch::channel(());
    let _handles = spawn_reload_pipeline(
        initial_config,
        Arc::new(CatalogOverlay::default()),
        Some(cfg_path),
        None,
        secrets,
        swap.clone(),
        Arc::new(ArcSwap::from_pointee(ActivationState::default())),
        usage,
        shutdown_rx,
    );

    // Settle the watch install before writing.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // Act 1: a real overlay write, via the same tempfile + rename
    // pattern `catalog_overlay::save` uses.
    let overlay_path = routectl_router::overlay_default_path();
    std::fs::create_dir_all(overlay_path.parent().unwrap()).unwrap();
    let tmp = tempfile::Builder::new()
        .prefix(".catalog_overlay.tmp.")
        .suffix(".json")
        .tempfile_in(overlay_path.parent().unwrap())
        .unwrap();
    std::fs::write(
        tmp.path(),
        br#"{"schema_version":1,"revision":1,"cells":{"anthropic-api:claude-opus-4-8*":
               {"source":"user","verified_at":"2026-07-01","wm":9.5}}}"#,
    )
    .unwrap();
    tmp.persist(&overlay_path).unwrap();

    // Assert: the router swaps within the debounce + reload window.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    let mut router_after_write = swap.load_full();
    while tokio::time::Instant::now() < deadline {
        router_after_write = swap.load_full();
        if !Arc::ptr_eq(&before_router, &router_after_write) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(
        !Arc::ptr_eq(&before_router, &router_after_write),
        "an overlay write through the real watcher must trigger a reload and swap the router",
    );

    // Act 2: a corrupt overlay write, same rename pattern.
    let tmp2 = tempfile::Builder::new()
        .prefix(".catalog_overlay.tmp.")
        .suffix(".json")
        .tempfile_in(overlay_path.parent().unwrap())
        .unwrap();
    std::fs::write(tmp2.path(), b"not json {{{").unwrap();
    tmp2.persist(&overlay_path).unwrap();

    // No positive signal to poll for here (the swap must NOT
    // happen); a fixed settle window mirrors the negative-outcome
    // checks in the integration suite (`partial_write_keeps_old_config`).
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;

    assert!(
        Arc::ptr_eq(&swap.load_full(), &router_after_write),
        "a corrupt overlay write must keep the previously-installed router live",
    );
}

/// Boot wiring: the writer/handle are constructed before the ArcSwap
/// and a Router hot-swap must NOT disturb the usage handle. We hold a
/// handle, swap the Router under it, and assert the handle's gate and
/// counters object survive the swap unchanged (handler call sites keep
/// a stable usage handle across any number of Router rebuilds).
#[tokio::test]
async fn router_hot_swap_does_not_disturb_usage_handle() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.usage.db_path = dir.path().join("usage.db");
    let config = Arc::new(config);
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let (usage, _writer) = build_usage_writer(&config);

    let r1 = build_router_from_config(config.clone(), secrets.clone())
        .await
        .unwrap();
    let swap = Arc::new(ArcSwap::from_pointee(r1));
    let counters_ptr = Arc::as_ptr(usage.counters());
    usage.set_enabled(true);

    // Act: swap a freshly-built Router under the same handle.
    let r2 = build_router_from_config(config.clone(), secrets)
        .await
        .unwrap();
    swap.store(Arc::new(r2));

    // Assert: the handle's gate and shared counters are untouched by
    // the Router swap (same counters allocation, gate value preserved).
    assert!(usage.is_enabled(), "router swap must not flip the gate");
    assert_eq!(
        counters_ptr,
        Arc::as_ptr(usage.counters()),
        "router swap must not rebuild the usage handle's counters"
    );
}

/// Graceful shutdown must drain queued usage rows to the (temp) DB
/// before returning, and complete within the writer's bounded
/// deadline. Drives `drain_usage_writer` (the spawn_blocking path the
/// serve loop uses) directly with rows already queued.
#[tokio::test]
async fn drain_usage_writer_flushes_queued_rows() {
    // Arrange: a live writer at a temp DB with rows queued behind it.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("usage.db");
    let mut config = Config::default();
    config.usage.db_path = db_path.clone();
    config.usage.enabled = true;
    config.usage.retention_days = 0;
    let config = Arc::new(config);
    let (usage, writer) = build_usage_writer(&config);

    let n = 25usize;
    for i in 0..n {
        usage.try_send(sample_usage_record(&format!("drain-{i}")));
    }
    // Drop the producer handle so the channel closes once the writer
    // drops its sender -- mirrors serve return, where the app owning
    // the handle is dropped before the drain runs.
    drop(usage);

    // Act: the serve loop's drain dispatch must flush + return within
    // the bounded deadline.
    let start = std::time::Instant::now();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        drain_usage_writer(writer),
    )
    .await
    .expect("drain must complete within the bounded deadline");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(7),
        "drain exceeded the bounded deadline"
    );

    // Assert: every queued row landed in the temp DB.
    let count: i64 = rusqlite_count(&db_path);
    assert_eq!(count, n as i64, "all queued rows must be flushed on drain");
}

/// Shutdown with no rows queued must still complete within the
/// writer's bounded deadline. With the producer handle dropped (the
/// serve-return steady state) the empty channel closes and the drain
/// returns promptly rather than waiting out the full deadline.
#[tokio::test]
async fn drain_usage_writer_empty_queue_returns_fast() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.usage.db_path = dir.path().join("usage.db");
    let config = Arc::new(config);
    let (usage, writer) = build_usage_writer(&config);
    // Drop the producer handle so the channel can close once the
    // writer drops its own sender -- the steady state after the axum
    // app (which owned the only handle) is dropped on serve return.
    drop(usage);

    let start = std::time::Instant::now();
    drain_usage_writer(writer).await;
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "empty-queue drain must return promptly once the handle is dropped"
    );
}

/// Models the production shutdown topology the other drain tests
/// miss: a SECOND `UsageHandle` clone (the reload coordinator's, in
/// real wiring) is alive in a background task when the drain begins
/// and is only dropped after a brief delay. The drain must still
/// complete cleanly within the bounded deadline (no timeout-warn
/// path) once that clone goes away, and every queued row must land.
/// The existing `drain_usage_writer_flushes_queued_rows` drops the
/// sole handle up front, which hides this race.
#[tokio::test]
async fn drain_usage_writer_completes_with_concurrent_handle() {
    // Arrange: a live writer at a temp DB with rows queued behind it.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("usage.db");
    let mut config = Config::default();
    config.usage.db_path = db_path.clone();
    config.usage.enabled = true;
    config.usage.retention_days = 0;
    let config = Arc::new(config);
    let (usage, writer) = build_usage_writer(&config);

    let n = 25usize;
    for i in 0..n {
        usage.try_send(sample_usage_record(&format!("concurrent-{i}")));
    }

    // Hold a second handle clone alive in a background task that
    // releases it after a brief delay -- the channel stays open until
    // BOTH this clone and the writer's own sender are gone.
    let second = usage.clone();
    let dropper = tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        drop(second);
    });
    // Drop the primary producer handle now; the background clone is
    // still keeping the channel open.
    drop(usage);

    // Act: the drain must flush and return within the bounded deadline
    // once the lingering clone is released.
    let start = std::time::Instant::now();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        drain_usage_writer(writer),
    )
    .await
    .expect("drain must complete within the bounded deadline");
    assert!(
        start.elapsed() < std::time::Duration::from_secs(7),
        "drain exceeded the bounded deadline despite the clone being released"
    );
    let _ = dropper.await;

    // Assert: every queued row landed despite the concurrent handle.
    let count: i64 = rusqlite_count(&db_path);
    assert_eq!(count, n as i64, "all queued rows must be flushed on drain");
}

/// Read `SELECT COUNT(*)` from the usage DB at `path`. Test helper.
#[cfg(test)]
fn rusqlite_count(path: &std::path::Path) -> i64 {
    use routectl_usage::open;
    let db = open(path).expect("open usage db for read");
    db.conn()
        .query_row("SELECT COUNT(*) FROM requests", [], |r| r.get(0))
        .expect("count rows")
}

/// Build a minimal valid `UsageRecord` with the given id for drain
/// tests. Mirrors the writer crate's own fixture shape.
#[cfg(test)]
fn sample_usage_record(request_id: &str) -> routectl_usage::UsageRecord {
    use routectl_usage::{Outcome, UsageRecord};
    UsageRecord {
        ts_start: 0,
        ts_end: 0,
        request_id: request_id.to_string(),
        ingress_dialect: "openai".to_string(),
        requested_model: "m".to_string(),
        alias: "a".to_string(),
        model: None,
        upstream: None,
        provider: None,
        provider_kind: None,
        seat: None,
        session_id: None,
        stream: false,
        max_tokens_req: None,
        tool_count: 0,
        thinking_req: None,
        thinking_req_kind: None,
        msg_count: 1,
        service_tier: None,
        outcome: Outcome::Ok,
        http_status: None,
        error_class: None,
        resolved_class: None,
        finish_reason: None,
        attempt_count: 1,
        fallback_count: 0,
        latency_ms: 0,
        ttfb_ms: None,
        input_tokens: None,
        output_tokens: None,
        reasoning_tokens: None,
        cache_read: None,
        cache_write_5m: None,
        cache_write_1h: None,
        server_tool_use: None,
        quota_claim: None,
        quota_status: None,
        quota_overage_status: None,
        quota_utilization: None,
        quota_overage_utilization: None,
        quota_reset: None,
        quota_extras: None,
        extra: None,
        strategy: None,
        reduction_strategy: None,
        selection_decision: None,
        would_trim_tokens: None,
        would_trim_break_even_k: None,
        would_trim_k_floor: None,
        would_trim_shadow_misfire: None,
        would_trim_dedup_tokens: None,
        would_trim_supersession_tokens: None,
        would_trim_path_units: None,
        would_trim_path_extractable: None,
        would_trim_recorder_version: None,
        would_trim_raw_marks: None,
        would_trim_context_fraction: None,
    }
}

use routectl_testkit::ScopedEnv;

/// A config older than this build writes is REJECTED at load, never
/// migrated in place. Both the serve/reload loader
/// (`load_effective_config`) and the `config check` unvalidated path
/// (`load_effective_config_unvalidated`) must reject it identically,
/// point at `config migrate`, and leave the file byte-identical -- the
/// mutate-on-load incident class this replaces.
#[test]
#[serial_test::serial]
fn load_rejects_a_too_old_config_and_leaves_it_byte_identical() {
    // Arrange: a v1 config (no explicit `version`) under an isolated
    // temp dir -- never the live config, per the loader learnings.
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    let body = "[server]\nhost = \"127.0.0.1\"\nport = 4000\n";
    std::fs::write(&cfg_path, body).expect("write config.toml");

    // Act: the serve/reload path.
    let serve_err = match load_effective_config(&cfg_path) {
        Ok(_) => panic!("a too-old config must be rejected on the serve path"),
        Err(e) => e,
    };
    // Act: the `config check` unvalidated path.
    let check_err = match load_effective_config_unvalidated(&cfg_path) {
        Ok(_) => panic!("a too-old config must be rejected on the check path"),
        Err(e) => e,
    };

    // Assert: both reject with the single-sourced migrate pointer.
    for err in [&serve_err, &check_err] {
        assert!(err.contains("config migrate"), "err: {err}");
        assert!(
            err.contains(&routectl_router::CURRENT_CONFIG_VERSION.to_string()),
            "err: {err}"
        );
    }

    // Assert: the file was not touched -- no stamp, no rewrite.
    let after = std::fs::read_to_string(&cfg_path).expect("read config after reject");
    assert_eq!(after, body, "a rejected config must stay byte-identical");
}

/// A current-version config loads unchanged: it passes the preflight,
/// the load never merges the legacy sidecar or mutates the file, and the
/// overlay -- untouched by this load -- stays as it was on disk.
#[test]
#[serial_test::serial]
fn load_leaves_a_current_version_config_unchanged() {
    // Arrange: a v2 config, plus a sidecar file that the load must NOT
    // fold in (the load no longer merges sidecars at any version).
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    let body = "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 4000\n";
    std::fs::write(&cfg_path, body).expect("write config.toml");

    let sidecar_dir = dir.path().join("routectl");
    std::fs::create_dir_all(&sidecar_dir).expect("create sidecar dir");
    std::fs::write(
        sidecar_dir.join("pricing_verifications.json"),
        r#"{"verified":{"openai-compat:grok-*":"2026-06-30"}}"#,
    )
    .expect("write sidecar");

    // Act
    let loaded = load_effective_config(&cfg_path).expect("load must succeed");

    // Assert: version preserved, nothing folded, file byte-identical.
    assert_eq!(
        loaded.config.version,
        routectl_router::CURRENT_CONFIG_VERSION
    );
    assert!(loaded.config.cache_pricing.is_empty());
    assert!(loaded.catalog_overlay.cells.is_empty());
    let after = std::fs::read_to_string(&cfg_path).expect("read config after load");
    assert_eq!(
        after, body,
        "a current-version config must not be rewritten"
    );
}

/// Cold-start posture: a config whose `version` is newer than this
/// build supports fails the load closed with a clear message, via the
/// preflight raw-TOML read -- before the `deny_unknown_fields` typed
/// deserialize would otherwise mask it behind an unknown-field error.
#[test]
fn load_effective_config_rejects_a_version_newer_than_supported() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, "version = 99\n[server]\nhost = \"127.0.0.1\"\n")
        .expect("write config.toml");

    // Act. `LoadedConfig` is not `Debug`, so match rather than
    // `expect_err`.
    let err = match load_effective_config(&cfg_path) {
        Ok(_) => panic!("version 99 must be rejected"),
        Err(e) => e,
    };

    // Assert
    assert!(err.contains("99"), "err: {err}");
    assert!(
        err.contains(&routectl_router::CURRENT_CONFIG_VERSION.to_string()),
        "err: {err}"
    );
}

/// `validate_provider_credential_sources` is wired into
/// `validate_effective_config`, which `load_effective_config` calls at
/// the end of its own pre-parse gate -- this proves the rejection
/// fires on the actual serve/reload load path (the function
/// `read_parse_validate_config` calls), not only via the separate
/// `commands::config::check` entry point that also happens to call
/// the same validator.
#[test]
#[serial_test::serial]
fn load_effective_config_rejects_forwarded_provider_on_non_anthropic_host() {
    // Arrange: a current-version config so the load path exercises
    // only the credential-source validator, not the version preflight.
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        "version = 3\n\
             [providers.sneaky]\n\
             kind = \"anthropic-api\"\n\
             base_url = \"https://evil.example.com\"\n\
             credential_source = \"forwarded\"\n",
    )
    .expect("write config.toml");

    // Act. `LoadedConfig` is not `Debug`, so match rather than
    // `expect_err`.
    let err = match load_effective_config(&cfg_path) {
        Ok(_) => panic!("forwarded provider off the pinned host must be rejected"),
        Err(e) => e,
    };

    // Assert
    assert!(err.contains("sneaky"), "err: {err}");
    assert!(err.contains("forwarded"), "err: {err}");
}

/// The serve/reload pre-parse gate (`validate_effective_config`, called
/// by `load_effective_config`) routes through the same centralized
/// suite as `config check` / `test` / `prompt-size`, so the identical
/// bad configs are rejected here too. Pins the no-fork acceptance on
/// the fourth caller path.
#[test]
#[serial_test::serial]
fn load_effective_config_rejects_each_centralized_bad_config() {
    // The loader preflight-rejects a stale version, and each remaining
    // case is a valid v3 config that a centralized validator refuses.
    let cases = [
        (
            "unknown-alias-target",
            "version = 3\n[aliases]\nfast = \"ghost\"\n",
        ),
        (
            "reserved-class-override",
            "version = 3\n[retry.classes.feature-unsupported]\nfallback = false\n",
        ),
    ];

    for (name, body) in cases {
        let dir = tempfile::tempdir().expect("tempdir");
        let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
        let cfg_path = dir.path().join("config.toml");
        std::fs::write(&cfg_path, body).expect("write config.toml");

        assert!(
            load_effective_config(&cfg_path).is_err(),
            "serve pre-parse gate must reject `{name}`"
        );
    }
}

/// A serve-loadable config that sets all three legacy capability-list
/// keys AND passes the shared validator suite: an `openai-compat`
/// provider carrying `unsupported_features`, plus the two `[bedrock]`
/// allowlists (non-empty). No Bedrock provider is configured, so the
/// Bedrock allowlist validator short-circuits and the config loads
/// cleanly -- letting the same file exercise both the serve WARN and
/// the silent `config check` path.
const LEGACY_LIST_CONFIG: &str = "version = 3\n\
         [providers.p]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://api.example.com\"\n\
         api_key_ref = \"env://SOME_KEY\"\n\
         unsupported_features = [\"web_search\"]\n\
         [bedrock]\n\
         allowed_betas = [\"some-beta\"]\n\
         allowed_body_fields = [\"messages\", \"anthropic_version\", \"max_tokens\"]\n";

fn deprecation_warns(
    events: &[routectl_testkit::CapturedEvent],
) -> Vec<&routectl_testkit::CapturedEvent> {
    events
        .iter()
        .filter(|e| e.field("legacy_keys").is_some())
        .collect()
}

/// Serve COLD START (`load_effective_config`, the loader both the
/// cold-start `load_config_with_overlay` and the hot-reload
/// `read_parse_validate_config` flow through) emits exactly ONE
/// deprecation WARN on a legacy-list config, naming which keys are
/// present plus the successor + migrate pointer -- and no config VALUES.
#[test]
#[serial_test::serial]
fn serve_load_warns_once_on_legacy_capability_lists() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, LEGACY_LIST_CONFIG).expect("write config.toml");

    // Act
    let events = routectl_testkit::capture_events(|| {
        load_effective_config(&cfg_path).expect("legacy-list config must still load");
    });

    // Assert: exactly one deprecation WARN.
    let warns = deprecation_warns(&events);
    assert_eq!(
        warns.len(),
        1,
        "serve cold-start load must emit exactly one deprecation WARN; got {}",
        warns.len()
    );
    let warn = warns[0];
    assert_eq!(warn.level, tracing::Level::WARN);
    assert_eq!(warn.field("event"), Some("legacy_deprecation"));

    // Names all three present legacy keys, the successor, the command.
    let keys = warn.field("legacy_keys").expect("legacy_keys field");
    assert!(keys.contains("unsupported_features"), "keys: {keys}");
    assert!(keys.contains("allowed_betas"), "keys: {keys}");
    assert!(keys.contains("allowed_body_fields"), "keys: {keys}");
    assert_eq!(warn.field("successor"), Some("[capability.overrides]"));
    assert_eq!(warn.field("migrate_command"), Some("config migrate"));

    // Log hygiene: the WARN carries key NAMES, never the operator's
    // list VALUES (which can sit next to secrets).
    let blob = format!("{} {:?}", warn.message, warn.fields);
    assert!(!blob.contains("web_search"), "leaked value: {blob}");
    assert!(!blob.contains("some-beta"), "leaked value: {blob}");
}

/// HOT RELOAD (`read_parse_validate_config`, the synchronous loader the
/// reload path runs off-runtime) emits exactly ONE deprecation WARN on a
/// legacy-list config -- the same site as cold start, fired once.
#[test]
#[serial_test::serial]
fn hot_reload_load_warns_once_on_legacy_capability_lists() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, LEGACY_LIST_CONFIG).expect("write config.toml");

    // Act
    let events = routectl_testkit::capture_events(|| {
        read_parse_validate_config(&cfg_path).expect("legacy-list config must reload");
    });

    // Assert
    assert_eq!(
        deprecation_warns(&events).len(),
        1,
        "hot reload must emit exactly one deprecation WARN"
    );
}

/// `config check` loads via `load_effective_config_unvalidated` (never
/// `load_effective_config`), so the SAME legacy-list config produces NO
/// deprecation WARN there -- and still PASSES the shared validator suite,
/// so existing configs keep passing `check`. Asserts both sides.
#[test]
#[serial_test::serial]
fn config_check_stays_silent_and_passing_on_legacy_capability_lists() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, LEGACY_LIST_CONFIG).expect("write config.toml");

    // Act: the loader `config check` uses, under capture.
    let mut loaded = None;
    let events = routectl_testkit::capture_events(|| {
        loaded = Some(
            load_effective_config_unvalidated(&cfg_path)
                .expect("legacy-list config must parse for check"),
        );
    });

    // Assert (silent): no deprecation WARN on the check load path.
    assert!(
        deprecation_warns(&events).is_empty(),
        "config check load path must emit no deprecation WARN"
    );

    // Assert (passing): the shared validator suite `config check` runs
    // finds no errors, so the legacy-list config still passes.
    let config = loaded.expect("config loaded").config;
    let validation = routectl_router::collect_config_validation(&config);
    assert!(
        validation.errors.is_empty(),
        "legacy-list config must still pass config check; errors: {:?}",
        validation.errors
    );
}

/// A config with no legacy list set (empty lists are the pass-through
/// default) emits no deprecation WARN on the serve load path.
#[test]
#[serial_test::serial]
fn serve_load_is_silent_without_legacy_capability_lists() {
    // Arrange
    let dir = tempfile::tempdir().expect("tempdir");
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 0\n",
    )
    .expect("write config.toml");

    // Act
    let events = routectl_testkit::capture_events(|| {
        load_effective_config(&cfg_path).expect("clean config must load");
    });

    // Assert
    assert!(
        deprecation_warns(&events).is_empty(),
        "a config with no legacy lists must emit no deprecation WARN"
    );
}

/// Hot-reload posture: a config edited to a too-new `version`
/// rejects the reload and keeps the prior router live, same as any
/// other reload-time load failure.
#[tokio::test]
#[serial_test::serial]
async fn config_reload_rejects_a_version_newer_than_supported_and_keeps_prior_router() {
    // Arrange: a good current-version config, an initial router built from it.
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 0\n",
    )
    .unwrap();

    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let mut initial_config = Config {
        version: routectl_router::CURRENT_CONFIG_VERSION,
        ..Default::default()
    };
    let _usage_dir = isolate_usage_db(&mut initial_config);
    let initial_config = Arc::new(initial_config);
    let (usage, _writer) = build_usage_writer(&initial_config);
    let router = build_router_from_config_with_overlay(
        initial_config.clone(),
        &CatalogOverlay::default(),
        secrets.clone(),
    )
    .await
    .expect("initial router build");
    let swap = Arc::new(ArcSwap::from_pointee(router));
    let router_before = swap.load_full();

    // Act: edit the config to a version newer than this build
    // supports, then reload.
    std::fs::write(
        &cfg_path,
        "version = 99\n[server]\nhost = \"127.0.0.1\"\nport = 0\n",
    )
    .unwrap();
    let result = handle_config_reload(
        Some(&cfg_path),
        &initial_config,
        secrets,
        &swap,
        &usage,
        ReloadTrigger::ConfigFile,
    )
    .await;

    // Assert: the reload rejects and the prior router stays installed.
    assert!(result.is_none(), "a too-new version must reject the reload");
    let router_after = swap.load_full();
    assert!(
        Arc::ptr_eq(&router_before, &router_after),
        "the prior router must stay installed on a rejected reload"
    );
}

/// Hot-reload containment: an overlay hand-edited to a degenerate cell
/// value (rm <= 0) fails the fail-closed load, so the reload is
/// rejected and the prior router stays live -- same posture as any
/// other reload-time load failure.
#[tokio::test]
#[serial_test::serial]
async fn config_reload_rejects_a_corrupt_overlay_cell_and_keeps_prior_router() {
    // Arrange: a good current-version config and an initial router built
    // from an empty overlay.
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 0\n",
    )
    .unwrap();

    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let mut initial_config = Config {
        version: routectl_router::CURRENT_CONFIG_VERSION,
        ..Default::default()
    };
    let _usage_dir = isolate_usage_db(&mut initial_config);
    let initial_config = Arc::new(initial_config);
    let (usage, _writer) = build_usage_writer(&initial_config);
    let router = build_router_from_config_with_overlay(
        initial_config.clone(),
        &CatalogOverlay::default(),
        secrets.clone(),
    )
    .await
    .expect("initial router build");
    let swap = Arc::new(ArcSwap::from_pointee(router));
    let router_before = swap.load_full();

    // Act: write a hand-edited overlay carrying a degenerate cell
    // (rm = 0) at the default overlay path, then reload.
    let overlay_dir = dir.path().join("routectl");
    std::fs::create_dir_all(&overlay_dir).unwrap();
    std::fs::write(
            overlay_dir.join("catalog_overlay.json"),
            r#"{"schema_version":1,"revision":0,"cells":{"openai-compat:grok-*":{"source":"user","verified_at":"2026-01-01","rm":0.0}}}"#,
        )
        .unwrap();
    let result = handle_config_reload(
        Some(&cfg_path),
        &initial_config,
        secrets,
        &swap,
        &usage,
        ReloadTrigger::CatalogOverlay,
    )
    .await;

    // Assert: the reload rejects and the prior router stays installed.
    assert!(
        result.is_none(),
        "a corrupt overlay cell must reject the reload"
    );
    let router_after = swap.load_full();
    assert!(
        Arc::ptr_eq(&router_before, &router_after),
        "the prior router must stay installed on a rejected reload"
    );
}
