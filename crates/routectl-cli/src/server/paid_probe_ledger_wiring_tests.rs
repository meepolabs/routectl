// Wiring: which Routers carry the accounting adapter, and whether both rebuild
// paths keep it.
//
// Observed through the WRITER CHANNEL rather than by asking a Router to reserve:
// the reservation entry point is the router crate's own internal seam, and a
// debug string would assert a rendering rather than a behavior. The adapter
// holds a producer clone of the writer's channel, so "this Router carries
// accounting" is directly observable as "the writer's drain is still waiting on
// it" -- which is the same fact the daemon's shutdown ordering turns on, and it
// fails in both directions (an uninstalled Router lets the drain finish; a
// reload that dropped the installation lets it finish early).

/// Whether a PROVABLY-STARTED drain finishes promptly, given whatever still
/// holds producer clones.
///
/// `true` means nothing is holding the channel open. The drain signals from
/// inside its own blocking closure before entering the writer's shutdown, and
/// this waits for that signal first, so a `false` is a drain that is genuinely
/// blocked rather than one the blocking pool has not scheduled -- the
/// distinction a bare bound cannot make, and the one every ownership claim here
/// rests on.
async fn writer_drains_promptly(writer: UsageWriter) -> bool {
    begin_writer_drain(writer)
        .await
        .completes_within(PROMPT_SHUTDOWN)
        .await
}

/// A Router built the way the library and every non-daemon path builds one.
async fn built_router(config: &Arc<routectl_router::Config>) -> Router {
    let secrets: Arc<dyn routectl_auth::SecretStore> = Arc::new(routectl_auth::MemoryStore::new());
    crate::server::build_router_from_config(config.clone(), secrets)
        .await
        .expect("router build")
}

/// A config whose usage ledger is the given path.
fn config_at(db_path: &Path) -> Arc<routectl_router::Config> {
    let mut config = routectl_router::Config::default();
    config.usage.db_path = db_path.to_path_buf();
    Arc::new(config)
}

/// A Router the ordinary builder produced carries NO accounting: it holds
/// nothing of the writer's, which is the same absence that makes it unable to
/// authorize a paid call at all.
///
/// The negative half of the pair below. Without it, the positive test would pass
/// on a build where every Router held a handle for unrelated reasons.
#[tokio::test]
async fn an_uninstalled_router_holds_no_accounting() {
    // Arrange
    let (_dir, path, handle, writer) = live_writer();
    let router = built_router(&config_at(&path)).await;
    drop(handle);

    // Act / Assert: the router is alive for the whole drain and holds nothing.
    assert!(
        writer_drains_promptly(writer).await,
        "a Router nobody installed accounting on must hold no producer handle",
    );
    drop(router);
}

/// Installing the adapter is what gives a Router accounting, and the unit it
/// reserves is durable in the ledger the writer owns.
#[tokio::test]
async fn an_installed_router_commits_through_the_real_writer() {
    // Arrange
    let (_dir, path, handle, writer) = live_writer();
    let router = install_paid_probe_ledger(built_router(&config_at(&path)).await, &handle);
    let before = control_rows(&path);

    // Act: the adapter the boot installs is built from the same handle, so a
    // reservation through one is a reservation through the other -- the router's
    // own seam is exercised by the router crate's tests.
    let committed = reserve(&paid_probe_ledger(&handle), "anthropic", 3).await;
    drop(handle);

    // Assert: durable in the writer's ledger, and the Router holds the channel.
    assert_eq!(
        committed,
        PaidProbeReservation::Committed { used: 1, cap: 3 }
    );
    let added = added_rows(&before, &control_rows(&path));
    assert_eq!(
        added.values().collect::<Vec<_>>(),
        vec!["1"],
        "the unit must be durable in the writer's own ledger: {added:?}",
    );
    assert!(
        !writer_drains_promptly(writer).await,
        "an installed Router holds a producer handle, which is what the shutdown \
         ordering has to release",
    );
    drop(router);
}

/// A config reload's replacement Router keeps the accounting installation.
///
/// Driven through the production handler, not through the carry-over call: the
/// two reload sites are separate sequences, and a site that dropped its
/// carry-over would still pass a test that called the carry-over itself.
///
/// The assertion holds in both directions. A replacement that lost the
/// installation holds nothing, so the drain finishes inside the prompt bound;
/// the control at the end proves the drain CAN finish once the replacement is
/// released, so a pass is the installation surviving rather than a wedged writer.
///
/// `#[serial]`: `handle_config_reload` re-reads the ambient catalog overlay, like
/// every other test that drives it.
#[tokio::test]
#[serial_test::serial]
async fn a_config_reload_keeps_the_accounting_installation() {
    // Arrange: a config on disk, a live writer, and an installed router published
    // behind the swap the coordinator owns.
    let dir = TempDir::new().expect("tempdir");
    let _xdg = routectl_testkit::ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let db_path = dir.path().join("usage.db");
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(
        &cfg_path,
        format!(
            "version = {}\n[server]\nhost = \"127.0.0.1\"\nport = 0\n[usage]\ndb_path = \"{}\"\n",
            routectl_router::CURRENT_CONFIG_VERSION,
            db_path.display(),
        ),
    )
    .expect("write config");
    let (handle, writer) = UsageWriter::start(db_path.clone(), CHANNEL_CAPACITY, 0, true);
    let config = config_at(&db_path);
    let secrets: Arc<dyn routectl_auth::SecretStore> = Arc::new(routectl_auth::MemoryStore::new());
    let swap = Arc::new(arc_swap::ArcSwap::from_pointee(install_paid_probe_ledger(
        built_router(&config).await,
        &handle,
    )));
    let outgoing = swap.load_full();

    // Act
    let (_new_config, _new_overlay) = crate::server::reload::handle_config_reload(
        Some(&cfg_path),
        &config,
        secrets,
        &swap,
        &handle,
        crate::server::reload::ReloadTrigger::ConfigFile,
        &mut never_firing_shutdown(),
    )
    .await
    .expect("config reload must apply");
    let replacement = swap.load_full();
    assert!(
        !Arc::ptr_eq(&outgoing, &replacement),
        "the premise: the reload must have swapped in a replacement Router",
    );

    // Assert: with the boot handle and the outgoing Router gone, the replacement
    // is the only thing that can still be holding the channel. ONE drain,
    // observed twice -- the bounded wait borrows its handle, so the drain that
    // had to time out here is the same one released below.
    drop(outgoing);
    drop(handle);
    let mut drain = begin_writer_drain(writer).await;
    assert!(
        !drain.completes_within(PROMPT_SHUTDOWN).await,
        "the replacement Router must still carry the accounting installation",
    );

    // Control: releasing it lets the drain finish, so the wait above was the
    // installation and not a wedged writer.
    swap.store(Arc::new(built_router(&config).await));
    drop(replacement);
    drain.finish().await;
}

/// The credentials-driven rebuild keeps the accounting installation too, driven
/// end to end through the production seat-change handler.
#[tokio::test]
async fn a_credentials_rebuild_keeps_the_accounting_installation() {
    // Arrange: one seat on disk, a pooled config, a live writer, and an installed
    // router published behind the coordinator's swap.
    let dir = TempDir::new().expect("tempdir");
    let creds = dir.path().join("routectl").join("credentials.json");
    write_seat_credentials(&creds, &[("anthropic", "tok-default")]);
    let db_path = dir.path().join("usage.db");
    let (handle, writer) = UsageWriter::start(db_path.clone(), CHANNEL_CAPACITY, 0, true);
    let config = pooled_oauth_config_at(&db_path);
    let composite = crate::server::CompositeStore::open_at(&creds)
        .await
        .expect("open composite store");
    let oauth = composite.oauth_store().expect("oauth arm present");
    let secrets: Arc<dyn routectl_auth::SecretStore> = Arc::new(composite);
    let overlay = Arc::new(routectl_router::CatalogOverlay::default());
    let initial = crate::server::build_router_from_config_with_overlay(
        config.clone(),
        &overlay,
        secrets.clone(),
    )
    .await
    .expect("initial router build");
    let swap = Arc::new(arc_swap::ArcSwap::from_pointee(install_paid_probe_ledger(
        initial, &handle,
    )));
    let outgoing = swap.load_full();

    // Act: a real seat-set change.
    write_seat_credentials(
        &creds,
        &[("anthropic", "tok-default"), ("anthropic#seat-b", "tok-b")],
    );
    crate::server::reload::handle_credentials_reload(
        &Some(oauth),
        &config,
        &overlay,
        secrets,
        &swap,
        &Arc::new(arc_swap::ArcSwap::from_pointee(
            routectl_router::ActivationState::default(),
        )),
    )
    .await;
    let rebuilt = swap.load_full();
    assert!(
        !Arc::ptr_eq(&outgoing, &rebuilt),
        "the premise: a seat-set change must rebuild and swap the Router",
    );

    // Assert: one drain, observed twice, as in the config-reload case above.
    drop(outgoing);
    drop(handle);
    let mut drain = begin_writer_drain(writer).await;
    assert!(
        !drain.completes_within(PROMPT_SHUTDOWN).await,
        "the rebuilt Router must still carry the accounting installation",
    );

    // Control
    swap.store(Arc::new(built_router(&config).await));
    drop(rebuilt);
    drain.finish().await;
}

/// A shutdown receiver that never fires, for the reload cases whose subject is
/// not the shutdown race. The sender is leaked deliberately: dropping it resolves
/// `changed()` immediately and abandons the reload under test.
fn never_firing_shutdown() -> tokio::sync::watch::Receiver<()> {
    let (tx, rx) = tokio::sync::watch::channel(());
    std::mem::forget(tx);
    rx
}

/// A pooled-OAuth config whose single model resolves its bearer through the
/// credentials store, so adding a second stored seat is a real seat-set change.
fn pooled_oauth_config_at(db_path: &Path) -> Arc<routectl_router::Config> {
    let text = format!(
        r#"
[server]
host = "127.0.0.1"
port = 0

[usage]
db_path = "{}"

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
"#,
        db_path.display(),
    );
    Arc::new(toml::from_str(&text).expect("pooled oauth config must parse"))
}

/// Write a `credentials.json` carrying one record per seat, in the shape and with
/// the permissions the production credentials writer emits, so the store accepts
/// it.
fn write_seat_credentials(path: &Path, seats: &[(&str, &str)]) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after the epoch")
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
    let parent = path.parent().expect("creds path has parent");
    std::fs::create_dir_all(parent).expect("mkdir creds parent");
    std::fs::write(
        path,
        serde_json::to_vec_pretty(&doc).expect("serialize creds"),
    )
    .expect("write creds");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod 0600");
    }
}

/// The daemon's boot installs the adapter, and installs it only AFTER the writer
/// exists and BEFORE the Router is published.
///
/// A source-text guard over the boot sequence, for the reason its sibling
/// writer-ordering guard in `serve_tests.rs` is one: the ordering is a property
/// of the sequence rather than of any value, so a reorder that installed the
/// adapter against a not-yet-started writer would compile and leave every
/// behavioral test green. An ordered pair, not mere presence -- presence alone
/// would pass on exactly the swap that breaks it.
#[test]
fn the_boot_installs_the_ledger_only_after_the_usage_writer_exists() {
    let src = include_str!("serve.rs");
    let writer_at = src
        .find("build_usage_writer(&config)")
        .expect("the boot must start the usage writer");
    let install_at = src
        .find("install_paid_probe_ledger(")
        .expect("the boot must install the paid-probe accounting adapter");
    let publish_at = src
        .find("ArcSwap::from_pointee(router)")
        .expect("the boot must publish the Router behind its ArcSwap");
    assert!(
        writer_at < install_at,
        "the adapter must be installed after the writer exists, or it holds a \
         producer handle for a writer that is not there",
    );
    assert!(
        install_at < publish_at,
        "the adapter must be installed while the Router is still owned, before publication",
    );
}

/// The shutdown sequence must release the Router BEFORE it drains the writer.
///
/// A source-text guard alongside the behavioral deadline test in the lifetime
/// sidecar: that test proves the daemon does not hang, and this one names the
/// ordering that keeps it from hanging, so a reorder is reported as itself rather
/// than as a slow shutdown on someone's machine.
#[test]
fn the_shutdown_sequence_releases_the_router_before_draining_the_writer() {
    let src = include_str!("serve.rs");
    let release_at = src
        .find("drop(router_swap)")
        .expect("the shutdown sequence must release the published Router");
    let drain_at = src
        .find("drain_usage_writer(usage_writer)")
        .expect("the shutdown sequence must drain the writer");
    assert!(
        release_at < drain_at,
        "the Router (and the accounting adapter it holds) must be released before \
         the writer is drained, or the drain waits out its abandon deadline",
    );
}
