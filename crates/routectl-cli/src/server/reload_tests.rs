use super::*;
use crate::server::serve::build_usage_writer;
use crate::server::test_support::isolate_usage_db;
use routectl_testkit::ScopedEnv;

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

/// A credentials-only reload rebuilds the Router off the CURRENT (unchanged)
/// overlay, so the replacement must carry that overlay through -- both the
/// revision stamp the capability boundary compares and the retained generation
/// the status read side derives the in-effect view from. Dropping either would
/// silently reset the operator's overlay to empty on a routine seat change.
#[tokio::test]
async fn credentials_reload_seat_change_carries_the_overlay_onto_the_new_router() {
    // Arrange: one seat on disk, and a live Router built against a real
    // overlay rather than the empty default.
    let dir = tempfile::tempdir().unwrap();
    let creds = dir.path().join("routectl").join("credentials.json");
    write_pool_credentials(&creds, &[("anthropic", "tok-default")]);
    let config = pooled_oauth_config();
    let overlay: Arc<CatalogOverlay> = Arc::new(
        serde_json::from_str(
            r#"{"schema_version":1,"revision":5,"cells":{"anthropic-api:claude-sonnet-4-6*":
                 {"source":"user","verified_at":"2026-07-01","wm":9.5}}}"#,
        )
        .expect("valid overlay"),
    );
    let composite = CompositeStore::open_at(&creds)
        .await
        .expect("open composite store at temp creds path");
    let oauth = composite.oauth_store().expect("oauth arm present");
    let secrets: Arc<dyn SecretStore> = Arc::new(composite);
    let router = build_router_from_config_with_overlay(config.clone(), &overlay, secrets.clone())
        .await
        .expect("initial router build");
    let swap = Arc::new(ArcSwap::from_pointee(router));
    let before = swap.load_full();
    assert_eq!(before.overlay_revision(), 5);

    // Act: add a second seat on disk (a real seat-set change), then reload.
    write_pool_credentials(
        &creds,
        &[("anthropic", "tok-default"), ("anthropic#seat-b", "tok-b")],
    );
    handle_credentials_reload(
        &Some(oauth),
        &config,
        &overlay,
        secrets,
        &swap,
        &Arc::new(ArcSwap::from_pointee(ActivationState::default())),
    )
    .await;

    // Assert: the rebuild happened, and the replacement carries the same
    // overlay generation -- never the empty default.
    let after = swap.load_full();
    assert!(
        !Arc::ptr_eq(&before, &after),
        "a seat-set change must rebuild and swap the router"
    );
    assert_eq!(
        after.overlay_revision(),
        5,
        "the replacement must keep the accepted overlay's revision stamp"
    );
    assert!(
        after
            .catalog_overlay()
            .cells
            .contains_key("anthropic-api:claude-sonnet-4-6*"),
        "the replacement must retain the accepted overlay itself, not an empty one"
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
        &Arc::default(),
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
    let router =
        build_router_from_config_with_overlay(config.clone(), &Arc::default(), secrets.clone())
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
        &Arc::default(),
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
        crate::handlers::status::DaemonMeta::for_test(),
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
        &Arc::default(),
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

// ---- Hot-reload capability tombstone: revision-change replay boundary ----

/// Count the tombstone rows persisted in the ledger at `path`.
#[cfg(test)]
fn tombstone_count(path: &std::path::Path) -> i64 {
    let db = routectl_usage::open(path).expect("open ledger");
    db.conn()
        .query_row(
            "SELECT COUNT(*) FROM capability_events WHERE verdict = 'tombstone'",
            [],
            |row| row.get(0),
        )
        .expect("count tombstones")
}

/// A config/overlay reload that advances the overlay revision must enqueue
/// EXACTLY ONE tombstone stamped the NEW revision -- the replay boundary that
/// keeps post-reload negatives replayable after a restart. Drives
/// `handle_config_reload` directly against a real, enabled usage writer, then
/// drains it and inspects the ledger. `#[serial]`: the loader reads the
/// ambient `overlay_default_path()` (XDG_CONFIG_HOME-derived).
#[tokio::test]
#[serial_test::serial]
async fn config_reload_revision_change_enqueues_one_new_revision_tombstone() {
    // Arrange: isolated config dir, a config.toml with usage enabled at a temp
    // DB, and an initial router built from an empty overlay (revision 0).
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let db_path = dir.path().join("usage.db");
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, usage_config_text(true, &db_path)).unwrap();

    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let mut start_config = Config::default();
    start_config.usage.db_path = db_path.clone();
    start_config.usage.enabled = true;
    let start_config = Arc::new(start_config);
    let (usage, writer) = build_usage_writer(&start_config);

    let router = build_router_from_config_with_overlay(
        start_config.clone(),
        &Arc::default(),
        secrets.clone(),
    )
    .await
    .expect("initial router build");
    let swap = Arc::new(ArcSwap::from_pointee(router));
    let boot_overlay_revision = swap.load().overlay_revision();

    // Act: write an overlay file at revision 1, advancing the overlay
    // revision, then reload.
    let overlay_dir = dir.path().join("routectl");
    std::fs::create_dir_all(&overlay_dir).unwrap();
    std::fs::write(
        overlay_dir.join("catalog_overlay.json"),
        r#"{"schema_version":1,"revision":1,"cells":{"anthropic-api:claude-opus-4-8*":
               {"source":"user","verified_at":"2026-07-01","wm":9.5}}}"#,
    )
    .unwrap();
    handle_config_reload(
        Some(&cfg_path),
        &start_config,
        secrets,
        &swap,
        &usage,
        ReloadTrigger::CatalogOverlay,
    )
    .await
    .expect("overlay reload must apply");

    // Precondition: the reload actually advanced the overlay revision.
    let new_overlay_revision = swap.load().overlay_revision();
    assert_ne!(
        boot_overlay_revision, new_overlay_revision,
        "the overlay write must advance the overlay revision"
    );

    // Drain the writer, then inspect the ledger.
    drop(usage);
    writer.shutdown();

    assert_eq!(
        tombstone_count(&db_path),
        1,
        "a revision-changing reload must enqueue exactly one tombstone"
    );
    let db = routectl_usage::open(&db_path).expect("reopen ledger");
    let boundary = routectl_usage::latest_tombstone(db.conn())
        .expect("read tombstone")
        .expect("a reload tombstone exists");
    assert_eq!(
        boundary.overlay_revision,
        Some(i64::try_from(new_overlay_revision).unwrap()),
        "the tombstone must be stamped the NEW overlay revision"
    );
    assert_eq!(
        boundary.catalog_version,
        Some(i64::from(swap.load().catalog_version())),
        "the tombstone must carry this reload's catalog version"
    );
}

/// A reload that leaves the catalog version and overlay revision unchanged
/// (here: no overlay file, config identical) must enqueue NO tombstone -- the
/// replay boundary only advances on a real revision change. `#[serial]`: the
/// loader reads the ambient `overlay_default_path()`.
#[tokio::test]
#[serial_test::serial]
async fn config_reload_without_revision_change_enqueues_no_tombstone() {
    // Arrange: isolated config dir with NO overlay file (revision stays 0),
    // usage enabled at a temp DB, and an initial router off an empty overlay.
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let db_path = dir.path().join("usage.db");
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, usage_config_text(true, &db_path)).unwrap();

    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let mut start_config = Config::default();
    start_config.usage.db_path = db_path.clone();
    start_config.usage.enabled = true;
    let start_config = Arc::new(start_config);
    let (usage, writer) = build_usage_writer(&start_config);

    let router = build_router_from_config_with_overlay(
        start_config.clone(),
        &Arc::default(),
        secrets.clone(),
    )
    .await
    .expect("initial router build");
    let swap = Arc::new(ArcSwap::from_pointee(router));
    let before = swap.load_full();

    // Act: reload the unchanged config against the same (absent) overlay.
    handle_config_reload(
        Some(&cfg_path),
        &start_config,
        secrets,
        &swap,
        &usage,
        ReloadTrigger::ConfigFile,
    )
    .await
    .expect("config reload must apply");

    // The reload still swapped the router (proving it ran), but neither
    // revision moved.
    let after = swap.load_full();
    assert!(
        !Arc::ptr_eq(&before, &after),
        "the reload must have run and swapped the router"
    );
    assert_eq!(before.catalog_version(), after.catalog_version());
    assert_eq!(before.overlay_revision(), after.overlay_revision());

    // Drain the writer: no tombstone must have been enqueued.
    drop(usage);
    writer.shutdown();
    assert_eq!(
        tombstone_count(&db_path),
        0,
        "an unchanged-revision reload must enqueue no tombstone"
    );
}
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
        &Arc::default(),
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

/// Minimal on-disk config text with `[reduction] enabled` set explicitly, so
/// a reload's reduction value is stated rather than inherited from the schema
/// default.
#[cfg(test)]
fn reduction_config_text(enabled: bool) -> String {
    format!(
        "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 0\n\n\
         [reduction]\nenabled = {enabled}\n"
    )
}

/// A reload that FLIPS `[reduction] enabled` stamps the before/after pair on
/// the success line, and a reload that leaves it alone omits both fields --
/// the operator's proof that a kill-switch flip actually landed, without
/// adding noise to every ordinary reload.
///
/// Drives `handle_config_reload` directly (no `tokio::spawn`) under
/// `with_capture` so the thread-local capture subscriber sees the event.
/// `#[serial]`: the loader re-reads the ambient `catalog_overlay.json` via
/// `routectl_router::overlay_default_path()`, so it joins the same
/// XDG-pinning group as its siblings above.
#[tokio::test]
#[serial_test::serial]
async fn reduction_flip_is_stamped_on_the_reload_success_log() {
    // Arrange: a live config with reduction ON and an on-disk candidate
    // that turns it OFF.
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, reduction_config_text(false)).unwrap();

    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let mut config = Config::default();
    let _usage_dir = isolate_usage_db(&mut config);
    config.reduction.enabled = true;
    let config = Arc::new(config);
    let (usage, _writer) = build_usage_writer(&config);
    let router =
        build_router_from_config_with_overlay(config.clone(), &Arc::default(), secrets.clone())
            .await
            .unwrap();
    let swap = Arc::new(ArcSwap::from_pointee(router));

    // Act 1: reload against the flipping candidate. `Box::pin` keeps the
    // future off this test's stack frame (clippy's large-futures lint).
    let (flip_result, flip_events) =
        routectl_testkit::with_capture(Box::pin(handle_config_reload(
            Some(&cfg_path),
            &config,
            secrets.clone(),
            &swap,
            &usage,
            ReloadTrigger::ConfigFile,
        )))
        .await;
    let (flipped_config, _) = flip_result.expect("the flipping reload must apply");
    assert!(!flipped_config.reduction.enabled);

    // Act 2: reload again from the ALREADY-flipped config -- same file, so
    // the value does not change this time.
    let (steady_result, steady_events) =
        routectl_testkit::with_capture(Box::pin(handle_config_reload(
            Some(&cfg_path),
            &flipped_config,
            secrets,
            &swap,
            &usage,
            ReloadTrigger::ConfigFile,
        )))
        .await;
    steady_result.expect("the no-change reload must apply");

    // Assert: the flip stamped both fields; the no-change reload stamped
    // neither, on the SAME success message.
    let success_message = "config reloaded; router rebuilt and swapped";
    let flip_line = flip_events
        .iter()
        .find(|e| e.message == success_message)
        .expect("the flipping reload must log the success line");
    assert_eq!(flip_line.field("reduction_enabled_before"), Some("true"));
    assert_eq!(flip_line.field("reduction_enabled_after"), Some("false"));

    let steady_line = steady_events
        .iter()
        .find(|e| e.message == success_message)
        .expect("the no-change reload must log the success line");
    assert_eq!(
        steady_line.field("reduction_enabled_before"),
        None,
        "an unchanged reduction value must not stamp the transition fields"
    );
    assert_eq!(steady_line.field("reduction_enabled_after"), None);
}

/// Minimal on-disk config text with `[cache] k_gated_emission` set explicitly,
/// so a reload's value for the break-even emission gate is stated rather than
/// inherited from the schema default (which is `false`).
#[cfg(test)]
fn k_gated_emission_config_text(enabled: bool) -> String {
    format!(
        "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 0\n\n\
         [cache]\nk_gated_emission = {enabled}\n"
    )
}

/// A reload that FLIPS `[cache] k_gated_emission` stamps its own before/after
/// pair on the success line, and a reload that leaves it alone omits both --
/// the same contract the reduction kill switch carries, for the switch that
/// governs whether break-even-gated suppression withholds cache markers.
///
/// The reduction pair must stay absent throughout: the two switches are
/// independent failure domains, so one flipping must not stamp the other's
/// fields.
///
/// Drives `handle_config_reload` directly (no `tokio::spawn`) under
/// `with_capture` so the thread-local capture subscriber sees the event.
/// `#[serial]`: the loader re-reads the ambient `catalog_overlay.json` via
/// `routectl_router::overlay_default_path()`, so it joins the same
/// XDG-pinning group as its siblings above.
#[tokio::test]
#[serial_test::serial]
async fn k_gated_emission_flip_is_stamped_on_the_reload_success_log() {
    // Arrange: a live config with the gate OFF (the shipped default) and an
    // on-disk candidate that turns it ON.
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, k_gated_emission_config_text(true)).unwrap();

    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let mut config = Config::default();
    let _usage_dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    assert!(
        !config.cache.k_gated_emission,
        "the shipped default must be off, so the flip below is a real transition"
    );
    let (usage, _writer) = build_usage_writer(&config);
    let router =
        build_router_from_config_with_overlay(config.clone(), &Arc::default(), secrets.clone())
            .await
            .unwrap();
    let swap = Arc::new(ArcSwap::from_pointee(router));

    // Act 1: reload against the flipping candidate. `Box::pin` keeps the
    // future off this test's stack frame (clippy's large-futures lint).
    let (flip_result, flip_events) =
        routectl_testkit::with_capture(Box::pin(handle_config_reload(
            Some(&cfg_path),
            &config,
            secrets.clone(),
            &swap,
            &usage,
            ReloadTrigger::ConfigFile,
        )))
        .await;
    let (flipped_config, _) = flip_result.expect("the flipping reload must apply");
    assert!(flipped_config.cache.k_gated_emission);

    // Act 2: reload again from the ALREADY-flipped config -- same file, so
    // the value does not change this time.
    let (steady_result, steady_events) =
        routectl_testkit::with_capture(Box::pin(handle_config_reload(
            Some(&cfg_path),
            &flipped_config,
            secrets,
            &swap,
            &usage,
            ReloadTrigger::ConfigFile,
        )))
        .await;
    steady_result.expect("the no-change reload must apply");

    // Assert: the flip stamped both fields; the no-change reload stamped
    // neither, on the SAME success message.
    let success_message = "config reloaded; router rebuilt and swapped";
    let flip_line = flip_events
        .iter()
        .find(|e| e.message == success_message)
        .expect("the flipping reload must log the success line");
    assert_eq!(flip_line.field("k_gated_emission_before"), Some("false"));
    assert_eq!(flip_line.field("k_gated_emission_after"), Some("true"));
    assert_eq!(
        flip_line.field("reduction_enabled_before"),
        None,
        "an unchanged reduction value must not ride along with the cache flip"
    );

    let steady_line = steady_events
        .iter()
        .find(|e| e.message == success_message)
        .expect("the no-change reload must log the success line");
    assert_eq!(
        steady_line.field("k_gated_emission_before"),
        None,
        "an unchanged k_gated_emission value must not stamp the transition fields"
    );
    assert_eq!(steady_line.field("k_gated_emission_after"), None);
}

/// A candidate that would turn `[cache] k_gated_emission` ON but cannot parse
/// logs NO success line at all, so the transition fields never appear for a
/// transition that did not happen, and the live gate stays off.
///
/// The failure path is the one an operator reads under pressure: a stamped
/// `k_gated_emission_after=true` on a reload that was declined would assert a
/// live suppression gate that is not in fact armed.
///
/// `#[serial]`: the loader re-reads the ambient `catalog_overlay.json` via
/// `routectl_router::overlay_default_path()`, so it joins the same XDG-pinning
/// group as its siblings.
#[tokio::test]
#[serial_test::serial]
async fn failed_reload_logs_no_k_gated_emission_transition() {
    // Arrange: a live config with the gate OFF and an unparseable candidate
    // that would turn it ON.
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 0\nprt = 8080\n\n\
         [cache]\nk_gated_emission = true\n",
    )
    .unwrap();

    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let mut config = Config::default();
    let _usage_dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, _writer) = build_usage_writer(&config);
    let router =
        build_router_from_config_with_overlay(config.clone(), &Arc::default(), secrets.clone())
            .await
            .unwrap();
    let swap = Arc::new(ArcSwap::from_pointee(router));
    let router_before = swap.load_full();

    // Act: the full reload against that candidate. `Box::pin` keeps the large
    // future off this test's stack frame (clippy's large-futures lint).
    let (result, events) = routectl_testkit::with_capture(Box::pin(handle_config_reload(
        Some(&cfg_path),
        &config,
        secrets,
        &swap,
        &usage,
        ReloadTrigger::ConfigFile,
    )))
    .await;

    // Assert: declined, no success line, prior router retained, gate still off.
    assert!(
        result.is_none(),
        "an unparseable candidate must reject the reload"
    );
    assert!(
        !events
            .iter()
            .any(|e| e.message == "config reloaded; router rebuilt and swapped"),
        "a failed reload must log no success line, hence no transition fields"
    );
    let router_after = swap.load_full();
    assert!(
        Arc::ptr_eq(&router_before, &router_after),
        "the prior router must stay installed on a rejected reload"
    );
    assert!(
        !router_after.config.cache.k_gated_emission,
        "a rejected reload must not arm the break-even emission gate"
    );
}

/// A reload that flips BOTH kill switches at once stamps all four transition
/// fields on the one success line -- the arm an operator hits when recovering
/// from a bad rollout by reverting a config that changed both.
///
/// `#[serial]`: the loader re-reads the ambient `catalog_overlay.json` via
/// `routectl_router::overlay_default_path()`, so it joins the same XDG-pinning
/// group as its siblings.
#[tokio::test]
#[serial_test::serial]
async fn a_reload_flipping_both_switches_stamps_both_pairs() {
    // Arrange: live reduction ON and the cache gate OFF; the candidate
    // inverts both.
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 0\n\n\
         [reduction]\nenabled = false\n\n[cache]\nk_gated_emission = true\n",
    )
    .unwrap();

    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let mut config = Config::default();
    let _usage_dir = isolate_usage_db(&mut config);
    config.reduction.enabled = true;
    let config = Arc::new(config);
    let (usage, _writer) = build_usage_writer(&config);
    let router =
        build_router_from_config_with_overlay(config.clone(), &Arc::default(), secrets.clone())
            .await
            .unwrap();
    let swap = Arc::new(ArcSwap::from_pointee(router));

    // Act. `Box::pin` keeps the large future off this test's stack frame
    // (clippy's large-futures lint).
    let (result, events) = routectl_testkit::with_capture(Box::pin(handle_config_reload(
        Some(&cfg_path),
        &config,
        secrets,
        &swap,
        &usage,
        ReloadTrigger::ConfigFile,
    )))
    .await;
    result.expect("the flipping reload must apply");

    // Assert: one line, four fields.
    let line = events
        .iter()
        .find(|e| e.message == "config reloaded; router rebuilt and swapped")
        .expect("the flipping reload must log the success line");
    assert_eq!(line.field("reduction_enabled_before"), Some("true"));
    assert_eq!(line.field("reduction_enabled_after"), Some("false"));
    assert_eq!(line.field("k_gated_emission_before"), Some("false"));
    assert_eq!(line.field("k_gated_emission_after"), Some("true"));
}

/// Same shape as `reduction_config_text` but with an unknown `[server]` field
/// (`prt`, a typo of `port`), so `deny_unknown_fields` refuses the candidate at
/// parse time.
#[cfg(test)]
fn reduction_config_text_unparseable(enabled: bool) -> String {
    format!(
        "version = 3\n[server]\nhost = \"127.0.0.1\"\nport = 0\nprt = 8080\n\n\
         [reduction]\nenabled = {enabled}\n"
    )
}

/// An unparseable candidate that would turn `[reduction] enabled` OFF is
/// DECLINED with a logged reason, and the live reduction state survives it.
///
/// This is the deterministic half of the rejection contract. The
/// integration-level
/// `tests/hot_reload.rs::failed_reload_keeps_old_reduction_state` proves the
/// live egress bytes stay compacted, but it cannot see the reload's log line:
/// there the reload runs on a spawned server task, and thread-local tracing
/// capture does not reach spawned tasks. The validation-failure WARN is what
/// distinguishes a reload that was ATTEMPTED and declined from one that never
/// fired at all, so it is asserted here.
///
/// The WARN is captured off `read_parse_validate_config` driven DIRECTLY rather
/// than off `handle_config_reload`: the reload runs that loader on
/// `spawn_blocking`, so its events land on a pool thread the thread-local
/// capture subscriber cannot see. Both are exercised -- the loader for the log,
/// the reload for the state it leaves behind.
///
/// `#[serial]`: the loader re-reads the ambient `catalog_overlay.json` via
/// `routectl_router::overlay_default_path()`, so it joins the same XDG-pinning
/// group as its siblings.
#[tokio::test]
#[serial_test::serial]
async fn unparseable_candidate_logs_its_rejection_and_keeps_reduction_on() {
    // Arrange: a live config with reduction ON, and an on-disk candidate that
    // would turn it OFF but cannot parse.
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, reduction_config_text_unparseable(false)).unwrap();

    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let mut config = Config::default();
    let _usage_dir = isolate_usage_db(&mut config);
    config.reduction.enabled = true;
    let config = Arc::new(config);
    let (usage, _writer) = build_usage_writer(&config);
    let router =
        build_router_from_config_with_overlay(config.clone(), &Arc::default(), secrets.clone())
            .await
            .unwrap();
    let swap = Arc::new(ArcSwap::from_pointee(router));
    let router_before = swap.load_full();

    // Act 1: the loader, driven synchronously so the thread-local capture
    // subscriber sees its WARN. In production `handle_config_reload` runs this
    // same function on `spawn_blocking`, which is exactly why the log is not
    // observable from the reload future itself.
    let mut loaded = None;
    let events = routectl_testkit::capture_events(|| {
        loaded = Some(read_parse_validate_config(&cfg_path));
    });
    assert!(
        loaded.expect("the loader ran").is_none(),
        "an unparseable candidate must not load"
    );

    // Act 2: the full reload against that same candidate. `Box::pin` keeps the
    // large future off this test's stack frame (clippy's large-futures lint).
    let result = Box::pin(handle_config_reload(
        Some(&cfg_path),
        &config,
        secrets,
        &swap,
        &usage,
        ReloadTrigger::ConfigFile,
    ))
    .await;

    // Assert: the decline is on the record, carrying the loader's own reason --
    // proof the candidate was READ rather than never seen. The offending field
    // NAME is scrubbed by `parse_error_redaction` (a mistyped key can carry a
    // secret), so the stable tells are the unknown-field class plus the
    // did-you-mean hint.
    let failure = events
        .iter()
        .find(|e| e.message == "config reload failed; keeping previous config")
        .expect("a rejected reload must log its failure");
    assert_eq!(failure.level, tracing::Level::WARN);
    let error = failure
        .field("error")
        .expect("the failure line must carry the loader error");
    assert!(
        error.contains("unknown field") && error.contains("did you mean `port`?"),
        "the logged reason must identify the rejection, got: {error}"
    );

    // The live state survives: reload declined, same router installed,
    // reduction still ON.
    assert!(
        result.is_none(),
        "an unparseable candidate must reject the reload"
    );
    let router_after = swap.load_full();
    assert!(
        Arc::ptr_eq(&router_before, &router_after),
        "the prior router must stay installed on a rejected reload"
    );
    assert!(
        router_after.config.reduction.enabled,
        "a rejected reload must not flip the live reduction kill switch"
    );
}

/// Both reload paths must carry the per-seat quota readings onto the
/// replacement router, and ONE missed site is silent: that path's config swap
/// empties the store, and an empty store reads exactly as a fleet of seats that
/// have not reported yet -- the cap-dormant fallback. No error, no warning, no
/// symptom to distinguish it from health.
///
/// A structural guard because the property is about the WIRING, not about any
/// one call: a unit test can only ever exercise the path it calls, so it would
/// stay green against the other path being unwired. It follows the calibration
/// carry-over's own guard, which pins the same property for the same reason.
#[test]
fn both_reload_paths_carry_the_quota_store_over() {
    let reload_src = include_str!("reload.rs");

    assert_eq!(
        reload_src.matches("carry_over_quota_from").count(),
        2,
        "both reload paths must carry the per-seat quota readings onto the new \
         router -- one-site-only silently empties the store on that path"
    );
    assert_eq!(
        reload_src.matches("carry_over_calibration_from").count(),
        reload_src.matches("carry_over_quota_from").count(),
        "the quota carry-over must be wired at exactly the sites the sibling \
         store carries at, so a future site added for one is added for both"
    );
}

/// Both reload paths must carry the per-session prefix-epoch baselines over,
/// and the same silence argument applies: an emptied store makes every live
/// session's next turn first-seen, and a first-seen turn is deliberately
/// unclassified -- so the detector simply stops finding anything, which reads
/// as healthy traffic. Structural for the same reason as the quota guard: the
/// property is about the wiring, not about either path in isolation.
#[test]
fn both_reload_paths_carry_the_prefix_epoch_store_over() {
    let reload_src = include_str!("reload.rs");

    assert_eq!(
        reload_src.matches("carry_over_prefix_epochs_from").count(),
        2,
        "both reload paths must carry the prefix-epoch baselines onto the new \
         router -- one-site-only silently blinds the detector on that path"
    );
    assert_eq!(
        reload_src.matches("carry_over_prefix_epochs_from").count(),
        reload_src.matches("carry_over_k_store_from").count(),
        "the prefix-epoch carry-over must be wired at exactly the sites the \
         sibling session-keyed store carries at"
    );
}

// ---- Router-metrics snapshot driver ----

/// Two Routers differing only in catalog-overlay revision, with the
/// revision bump carried across via the PUBLIC `carry_over_learned_from`
/// -- the only seam this crate has to produce a real, non-zero router
/// counter without reaching into `routectl-router`'s private metrics
/// internals. Bumps `rc_invalidations_total` to exactly 1.
fn router_with_one_invalidation() -> Router {
    let config = Arc::new(Config::default());
    let mut before = Router::new(config.clone());
    before.install_catalog_overlay(Arc::new(CatalogOverlay {
        revision: 1,
        ..Default::default()
    }));
    let mut router = Router::new(config);
    router.install_catalog_overlay(Arc::new(CatalogOverlay {
        revision: 2,
        ..Default::default()
    }));
    router.carry_over_learned_from(&before);
    router
}

/// Guards the router-metrics driver's shutdown-flush seam
/// (`run_router_metrics_snapshot_driver`): removing the production spawn,
/// or breaking its shutdown arm, leaves every other reload test green
/// because nothing else drives this function.
#[tokio::test]
async fn router_metrics_snapshot_driver_flushes_at_graceful_shutdown() {
    let router_swap = Arc::new(ArcSwap::from_pointee(router_with_one_invalidation()));
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    // Flip shutdown before driving the loop so the driver takes its
    // shutdown branch on the first poll and returns ON THIS THREAD --
    // `with_capture` only sees events emitted on the calling thread.
    shutdown_tx.send(()).unwrap();

    let ((), events) = routectl_testkit::with_capture(run_router_metrics_snapshot_driver(
        router_swap,
        shutdown_rx,
    ))
    .await;

    let snapshot = events
        .iter()
        .find(|event| {
            event.target == "routectl_router::router::metrics"
                && event.message == "router metrics snapshot"
        })
        .expect("graceful shutdown must flush a router metrics snapshot");
    assert_eq!(
        snapshot.field("rc_invalidations_total"),
        Some("1"),
        "the flushed snapshot must carry the shared instance's accumulated total"
    );
}

/// Guards the OTHER seam in the driver: the periodic
/// [`ROUTER_METRICS_SNAPSHOT_INTERVAL`] tick, which the shutdown-flush
/// test above cannot observe (it returns on the shutdown branch before
/// the timer ever elapses). `start_paused` auto-advances the clock to
/// each pending deadline, so a full interval passes without a single
/// millisecond of wall time.
#[tokio::test(start_paused = true)]
async fn router_metrics_snapshot_driver_flushes_on_the_periodic_tick() {
    let router_swap = Arc::new(ArcSwap::from_pointee(router_with_one_invalidation()));
    // The sender stays alive for the whole test: dropping it would make
    // `shutdown.changed()` resolve, firing the shutdown flush and
    // masking a missing periodic emission.
    let (_shutdown_tx, shutdown_rx) = watch::channel(());

    let (outcome, events) = routectl_testkit::with_capture(tokio::time::timeout(
        ROUTER_METRICS_SNAPSHOT_INTERVAL + std::time::Duration::from_secs(1),
        run_router_metrics_snapshot_driver(router_swap, shutdown_rx),
    ))
    .await;
    assert!(
        outcome.is_err(),
        "the loop must still be running: it may only exit on the shutdown signal"
    );

    let snapshots: Vec<_> = events
        .iter()
        .filter(|event| {
            event.target == "routectl_router::router::metrics"
                && event.message == "router metrics snapshot"
        })
        .collect();
    assert_eq!(
        snapshots.len(),
        1,
        "exactly one periodic snapshot must be emitted per elapsed interval, got {}",
        snapshots.len()
    );
    assert_eq!(
        snapshots[0].field("rc_invalidations_total"),
        Some("1"),
        "the periodic snapshot must carry the shared instance's accumulated total"
    );
}
