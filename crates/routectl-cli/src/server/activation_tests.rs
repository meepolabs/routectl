use super::*;
use routectl_testkit::with_capture;

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Seed a `credentials.json` at `path` holding one Present record for
/// `provider` (unexpired access token + a refresh token). The access and
/// refresh values are distinctive substrings so a leakage assertion can
/// prove they never reach any log field.
fn seed_present(path: &std::path::Path, provider: &str, expires_at_unix: u64) {
    let creds = serde_json::json!({
        "schema_version": 1,
        "providers": {
            provider: {
                "access_token": "seeded-access-secret-token",
                "refresh_token": "seeded-refresh-secret-token",
                "token_type": "Bearer",
                "expires_at_unix": expires_at_unix,
                "scopes": ["user:inference"],
                "obtained_at_unix": now_unix()
            }
        }
    });
    std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir creds parent");
    std::fs::write(path, serde_json::to_vec_pretty(&creds).unwrap()).expect("write creds");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod 0600");
    }
}

async fn oauth_at(path: &std::path::Path) -> Option<Arc<routectl_auth::OAuthStore>> {
    CompositeStore::open_at(path)
        .await
        .expect("open composite")
        .oauth_store()
}

#[tokio::test]
async fn startup_emits_activated_fields_without_leakage() {
    // Arrange: a Present anthropic credential; empty config.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    seed_present(&path, "anthropic", now_unix() + 3600);
    let oauth = oauth_at(&path).await;
    let cfg = Config::default();
    let swap = Arc::new(ArcSwap::from_pointee(ActivationState::default()));

    // Act
    let ((), events) = with_capture(apply_activation(
        &oauth,
        &cfg,
        &swap,
        ActivationTrigger::Startup,
    ))
    .await;

    // Assert: exactly one activation event, for anthropic, with the
    // documented startup fields.
    let inventory: Vec<_> = events
        .iter()
        .filter(|e| e.message == ACTIVATION_EVENT_MSG)
        .collect();
    let anthropic = inventory
        .iter()
        .find(|e| e.field("provider") == Some("anthropic"))
        .expect("anthropic activation event");
    assert_eq!(anthropic.level, tracing::Level::INFO);
    assert_eq!(anthropic.field("transition"), Some("activated"));
    assert_eq!(anthropic.field("kind"), Some("anthropic-api"));
    assert_eq!(anthropic.field("trigger"), Some("startup"));
    assert_eq!(anthropic.field("referenced_by_aliases"), Some("false"));

    // Redaction contract: neither the seeded token nor the credentials
    // path appears in any message or field of any captured event.
    let path_str = path.display().to_string();
    for e in &events {
        assert!(!e.message.contains("seeded-"), "secret in message: {e:?}");
        assert!(!e.message.contains(&path_str), "path in message: {e:?}");
        for (k, v) in &e.fields {
            assert!(!v.contains("seeded-"), "secret in field {k}={v}");
            assert!(!v.contains(&path_str), "path in field {k}={v}");
        }
    }
}

#[tokio::test]
async fn deactivation_emits_reason_code() {
    // Arrange: activate anthropic, then recompute against a store with no
    // record (Missing) so it transitions out of the activated set.
    let dir = tempfile::tempdir().unwrap();
    let present = dir.path().join("present.json");
    seed_present(&present, "anthropic", now_unix() + 3600);
    let empty = dir.path().join("empty.json");
    let cfg = Config::default();
    let swap = Arc::new(ArcSwap::from_pointee(ActivationState::default()));

    apply_activation(
        &oauth_at(&present).await,
        &cfg,
        &swap,
        ActivationTrigger::Startup,
    )
    .await;

    // Act: recompute on a credentials change with the empty store.
    let ((), events) = with_capture(apply_activation(
        &oauth_at(&empty).await,
        &cfg,
        &swap,
        ActivationTrigger::CredentialsChange,
    ))
    .await;

    // Assert: anthropic deactivated, carrying its machine-readable reason.
    let deactivation = events
        .iter()
        .find(|e| e.message == ACTIVATION_EVENT_MSG && e.field("provider") == Some("anthropic"))
        .expect("anthropic deactivation event");
    assert_eq!(deactivation.field("transition"), Some("deactivated"));
    assert_eq!(deactivation.field("reason"), Some("oauth_missing"));
    assert_eq!(deactivation.field("trigger"), Some("credentials_change"));
}

#[tokio::test]
async fn empty_delta_emits_no_events() {
    // Arrange: seed the swap ALREADY at the computed inventory so the
    // recompute produces an empty diff.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("credentials.json");
    seed_present(&path, "anthropic", now_unix() + 3600);
    let oauth = oauth_at(&path).await;
    let cfg = Config::default();
    let probes = gather_probes(&oauth).await;
    let swap = Arc::new(ArcSwap::from_pointee(compute_activation(&probes, &cfg)));

    // Act
    let ((), events) = with_capture(apply_activation(
        &oauth,
        &cfg,
        &swap,
        ActivationTrigger::ConfigChange,
    ))
    .await;

    // Assert: no activation events at all (store present -> no WARN; diff
    // empty -> no INFO).
    let inventory: Vec<_> = events
        .iter()
        .filter(|e| e.message.contains(ACTIVATION_EVENT_MSG))
        .collect();
    assert!(
        inventory.is_empty(),
        "an empty delta with a present store must emit nothing, got {inventory:?}"
    );
}

#[tokio::test]
async fn store_unavailable_warns_once_without_leaking_events() {
    // Arrange: no OAuth store at all.
    let none: Option<Arc<routectl_auth::OAuthStore>> = None;
    let cfg = Config::default();
    let swap = Arc::new(ArcSwap::from_pointee(ActivationState::default()));

    // Act
    let ((), events) = with_capture(apply_activation(
        &none,
        &cfg,
        &swap,
        ActivationTrigger::Startup,
    ))
    .await;

    // Assert: exactly one WARN, no transition INFO events (every id is
    // StoreUnavailable -> unresolved, so the diff against empty is empty).
    let warns: Vec<_> = events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN && e.message.contains(ACTIVATION_EVENT_MSG))
        .collect();
    assert_eq!(warns.len(), 1, "one store-unavailable WARN per apply");
    assert_eq!(warns[0].field("trigger"), Some("startup"));
    let infos: Vec<_> = events
        .iter()
        .filter(|e| e.message == ACTIVATION_EVENT_MSG)
        .collect();
    assert!(infos.is_empty(), "no transition events, got {infos:?}");
}

fn write_creds_json(path: &std::path::Path, value: &serde_json::Value) {
    std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir creds parent");
    std::fs::write(path, serde_json::to_vec_pretty(value).unwrap()).expect("write creds");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod 0600");
    }
}

/// Write a credentials.json holding no provider records (post-logout /
/// first-run shape). Every probe against it reports `Missing`.
fn seed_missing(path: &std::path::Path) {
    write_creds_json(
        path,
        &serde_json::json!({ "schema_version": 1, "providers": {} }),
    );
}

/// Seed one provider record whose access token is already expired and
/// that carries NO refresh token -> `Expired` (Unresolved), yet the seat
/// key is present so the credential-key set is unchanged by a later
/// refresh to a live token.
fn seed_expired(path: &std::path::Path, provider: &str) {
    write_creds_json(
        path,
        &serde_json::json!({
            "schema_version": 1,
            "providers": { provider: {
                "access_token": "seeded-access-secret-token",
                "refresh_token": "",
                "token_type": "Bearer",
                "expires_at_unix": now_unix().saturating_sub(3600),
                "scopes": ["user:inference"],
                "obtained_at_unix": now_unix().saturating_sub(7200)
            }}
        }),
    );
}

/// Open a composite store at `creds_path`, build a Router from `config`,
/// and return the shared OAuth arm, the composite secrets store, and the
/// live Router swap -- the dependency set `handle_credentials_reload`
/// needs. Mirrors `tests::coordinator_rig`.
async fn creds_rig(
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
    (oauth, secrets, Arc::new(ArcSwap::from_pointee(router)))
}

fn activation_events(
    events: &[routectl_testkit::CapturedEvent],
) -> Vec<&routectl_testkit::CapturedEvent> {
    events
        .iter()
        .filter(|e| e.message == ACTIVATION_EVENT_MSG)
        .collect()
}

#[tokio::test]
async fn credentials_reload_adding_live_token_emits_activated() {
    // Arrange: no anthropic record on disk; baseline activation computed
    // against that (Unresolved).
    let dir = tempfile::tempdir().unwrap();
    let creds = dir.path().join("credentials.json");
    seed_missing(&creds);
    let cfg = Arc::new(Config::default());
    let (oauth, secrets, router_swap) = creds_rig(&creds, &cfg).await;
    let activation_swap = Arc::new(ArcSwap::from_pointee(ActivationState::default()));
    apply_activation(
        &Some(oauth.clone()),
        &cfg,
        &activation_swap,
        ActivationTrigger::Startup,
    )
    .await;

    // Act: a login writes a live anthropic token; reload credentials.
    seed_present(&creds, "anthropic", now_unix() + 3600);
    let ((), events) = with_capture(Box::pin(handle_credentials_reload(
        &Some(oauth),
        &cfg,
        &Arc::new(CatalogOverlay::default()),
        secrets,
        &router_swap,
        &activation_swap,
    )))
    .await;

    // Assert: a newly-activated transition attributed to the credentials
    // reload.
    let activated = activation_events(&events)
        .into_iter()
        .find(|e| e.field("provider") == Some("anthropic"))
        .expect("anthropic activation event");
    assert_eq!(activated.field("transition"), Some("activated"));
    assert_eq!(activated.field("trigger"), Some("credentials_change"));
}

#[tokio::test]
async fn credentials_reload_removing_token_emits_deactivated_with_reason() {
    // Arrange: a live anthropic record; baseline activation = Activated.
    let dir = tempfile::tempdir().unwrap();
    let creds = dir.path().join("credentials.json");
    seed_present(&creds, "anthropic", now_unix() + 3600);
    let cfg = Arc::new(Config::default());
    let (oauth, secrets, router_swap) = creds_rig(&creds, &cfg).await;
    let activation_swap = Arc::new(ArcSwap::from_pointee(ActivationState::default()));
    apply_activation(
        &Some(oauth.clone()),
        &cfg,
        &activation_swap,
        ActivationTrigger::Startup,
    )
    .await;

    // Act: a logout removes the record; reload credentials.
    seed_missing(&creds);
    let ((), events) = with_capture(Box::pin(handle_credentials_reload(
        &Some(oauth),
        &cfg,
        &Arc::new(CatalogOverlay::default()),
        secrets,
        &router_swap,
        &activation_swap,
    )))
    .await;

    // Assert: a newly-deactivated transition carrying its machine-readable
    // reason, attributed to the credentials reload.
    let deactivated = activation_events(&events)
        .into_iter()
        .find(|e| e.field("provider") == Some("anthropic"))
        .expect("anthropic deactivation event");
    assert_eq!(deactivated.field("transition"), Some("deactivated"));
    assert_eq!(deactivated.field("reason"), Some("oauth_missing"));
    assert_eq!(deactivated.field("trigger"), Some("credentials_change"));
}

#[tokio::test]
async fn credentials_reload_unchanged_seat_set_recomputes_without_router_rebuild() {
    // Arrange: an expired-no-refresh anthropic record (Unresolved) whose
    // seat key is nonetheless present.
    let dir = tempfile::tempdir().unwrap();
    let creds = dir.path().join("credentials.json");
    seed_expired(&creds, "anthropic");
    let cfg = Arc::new(Config::default());
    let (oauth, secrets, router_swap) = creds_rig(&creds, &cfg).await;
    let activation_swap = Arc::new(ArcSwap::from_pointee(ActivationState::default()));
    apply_activation(
        &Some(oauth.clone()),
        &cfg,
        &activation_swap,
        ActivationTrigger::Startup,
    )
    .await;
    let router_before = router_swap.load_full();

    // Act: refresh the SAME seat to a live token -- a token-value-only
    // change, so the credential-key set is identical across the reload.
    seed_present(&creds, "anthropic", now_unix() + 3600);
    let ((), events) = with_capture(Box::pin(handle_credentials_reload(
        &Some(oauth),
        &cfg,
        &Arc::new(CatalogOverlay::default()),
        secrets,
        &router_swap,
        &activation_swap,
    )))
    .await;

    // Assert: activation recomputed (Unresolved -> Activated) even though
    // the seat set did not change ...
    let activated = activation_events(&events)
        .into_iter()
        .find(|e| e.field("provider") == Some("anthropic"))
        .expect("recompute must emit an activation transition");
    assert_eq!(activated.field("transition"), Some("activated"));
    assert_eq!(activated.field("trigger"), Some("credentials_change"));

    // ... and the Router was NOT rebuilt (pointer-identical swap payload).
    assert!(
        Arc::ptr_eq(&router_before, &router_swap.load_full()),
        "an unchanged seat set must not rebuild the router",
    );
}

#[tokio::test]
async fn failed_config_reload_keeps_previous_activation() {
    // Arrange: a live anthropic record and an activation swap seeded to
    // the computed (Activated) state.
    let dir = tempfile::tempdir().unwrap();
    let creds = dir.path().join("credentials.json");
    seed_present(&creds, "anthropic", now_unix() + 3600);
    let oauth = oauth_at(&creds).await;
    let cfg = Arc::new(Config::default());
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let router = build_router_from_config(cfg.clone(), secrets.clone())
        .await
        .expect("initial router build");
    let router_swap = Arc::new(ArcSwap::from_pointee(router));
    let probes = gather_probes(&oauth).await;
    let activation_swap = Arc::new(ArcSwap::from_pointee(compute_activation(&probes, &cfg)));
    let activation_before = activation_swap.load_full();

    // A syntactically invalid config the reload must reject (parse fails
    // before any overlay / XDG read, so no environment isolation needed).
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, b"<<<not valid toml>>>").unwrap();
    let (usage, _writer) =
        UsageWriter::start(dir.path().join("usage.db"), CHANNEL_CAPACITY, 0, false);

    // Drive the coordinator (not the handler) so the config-reload
    // recompute guard is the code under test. One Config request, then the
    // channel closes so the loop returns.
    let (reload_tx, reload_rx) = mpsc::channel::<ReloadRequest>(4);
    reload_tx.send(ReloadRequest::Config).await.unwrap();
    drop(reload_tx);
    let (_shutdown_tx, shutdown_rx) = watch::channel(());
    let ctx = ReloadContext {
        config_path: Some(cfg_path),
        oauth_store: oauth,
        secrets,
        router_swap,
        activation_swap: activation_swap.clone(),
        usage,
    };

    // Act
    let ((), events) = with_capture(Box::pin(run_reload_coordinator(
        cfg.clone(),
        Arc::new(CatalogOverlay::default()),
        ctx,
        reload_rx,
        shutdown_rx,
    )))
    .await;

    // Assert: a rejected reload recomputes nothing -- no activation events
    // and the swap payload is pointer-unchanged.
    assert!(
        activation_events(&events).is_empty(),
        "a failed config reload must not recompute activation, got {events:?}",
    );
    assert!(
        Arc::ptr_eq(&activation_before, &activation_swap.load_full()),
        "a failed config reload must leave the activation state untouched",
    );
}
