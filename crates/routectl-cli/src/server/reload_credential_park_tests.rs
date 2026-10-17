//! A credentials reload or an accepted config reload lifts every credential
//! park: the operator just changed the credentials or the routes, so a seat
//! parked on the old ones deserves a fresh attempt. A reload that fails
//! leaves every park in place.

use super::*;
use routectl_core::UpstreamReason;
use std::time::{Duration, Instant};

fn park(router: &Router, state_key: &str) {
    assert!(
        router.park_credential_for_tests(
            state_key,
            UpstreamReason::CredentialExpired,
            Instant::now() + Duration::from_hours(1),
        ),
        "{state_key} must own a state slot",
    );
}

fn park_reason(router: &Router, state_key: &str) -> Option<UpstreamReason> {
    router
        .status_targets(Instant::now())
        .into_iter()
        .find(|target| target.state_key == state_key)
        .unwrap_or_else(|| panic!("no status target {state_key}"))
        .gate
        .credential_park
        .map(|park| park.reason)
}

#[tokio::test]
async fn a_credentials_reload_lifts_parks_even_when_the_seat_set_is_unchanged() {
    // Arrange: one seat, parked, then a token-only rewrite on disk.
    let dir = tempfile::tempdir().unwrap();
    let creds = dir.path().join("routectl").join("credentials.json");
    write_pool_credentials(&creds, &[("anthropic", "tok-old")]);
    let config = pooled_oauth_config();
    let (oauth, secrets, swap) = coordinator_rig(&creds, &config).await;
    park(&swap.load(), "claude");
    let before = swap.load_full();
    write_pool_credentials(&creds, &[("anthropic", "tok-new")]);

    // Act
    handle_credentials_reload(
        &Some(oauth),
        &config,
        &Arc::new(CatalogOverlay::default()),
        secrets,
        &swap,
        &Arc::new(ArcSwap::from_pointee(ActivationState::default())),
    )
    .await;

    // Assert
    assert!(
        Arc::ptr_eq(&before, &swap.load_full()),
        "a token-only change must not rebuild the router",
    );
    assert_eq!(park_reason(&swap.load(), "claude"), None);
}

#[tokio::test]
async fn a_failed_credentials_reload_keeps_every_park() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let creds = dir.path().join("routectl").join("credentials.json");
    write_pool_credentials(&creds, &[("anthropic", "tok-old")]);
    let config = pooled_oauth_config();
    let (oauth, secrets, swap) = coordinator_rig(&creds, &config).await;
    park(&swap.load(), "claude");
    std::fs::write(&creds, b"<<corrupt-json>>").unwrap();

    // Act
    handle_credentials_reload(
        &Some(oauth),
        &config,
        &Arc::new(CatalogOverlay::default()),
        secrets,
        &swap,
        &Arc::new(ArcSwap::from_pointee(ActivationState::default())),
    )
    .await;

    // Assert
    assert_eq!(
        park_reason(&swap.load(), "claude"),
        Some(UpstreamReason::CredentialExpired),
    );
}

/// A router serving model `glm` off the prompt-shaping fixture config, plus
/// the config it was built from. The caller holds the XDG and key scopes.
async fn glm_router(
    secrets: &Arc<dyn SecretStore>,
) -> (Arc<Config>, Arc<ArcSwap<Router>>, tempfile::TempDir) {
    let mut config = routectl_router::parse_config(&prompt_shaping_config_text("strip"))
        .expect("fixture config parses");
    let usage_dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let router =
        build_router_from_config_with_overlay(config.clone(), &Arc::default(), secrets.clone())
            .await
            .expect("initial router build");
    (config, Arc::new(ArcSwap::from_pointee(router)), usage_dir)
}

#[tokio::test]
#[serial_test::serial]
async fn an_accepted_config_reload_lifts_every_park() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let _key = ScopedEnv::set("ROUTECTL_PROMPT_SHAPING_TEST_KEY", "test-key");
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, prompt_shaping_config_text("strip")).unwrap();
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let (config, swap, _usage_dir) = glm_router(&secrets).await;
    let (usage, writer) = build_usage_writer(&config);
    park(&swap.load(), "glm");
    let before = swap.load_full();

    // Act
    handle_config_reload(
        Some(&cfg_path),
        &config,
        secrets,
        &swap,
        &usage,
        ReloadTrigger::ConfigFile,
        &mut never_shutdown(),
    )
    .await
    .expect("config reload applies")
    .expect("a successful reload returns its config");

    // Assert
    assert!(!Arc::ptr_eq(&before, &swap.load_full()), "router swapped");
    assert_eq!(park_reason(&swap.load(), "glm"), None);
    drop(usage);
    drain_usage_writer_strict(writer).await;
}

/// The failure lands AFTER the runtime state was carried onto the candidate
/// router, which shares the live router's gate slots: clearing at carry-over
/// time would already have lifted the live parks.
#[tokio::test]
#[serial_test::serial]
async fn a_config_reload_rejected_after_carry_over_keeps_every_park() {
    // Arrange: a closed usage channel refuses the replay boundary that the
    // overlay revision change below requires.
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let _key = ScopedEnv::set("ROUTECTL_PROMPT_SHAPING_TEST_KEY", "test-key");
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, prompt_shaping_config_text("strip")).unwrap();
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let (config, swap, _usage_dir) = glm_router(&secrets).await;
    let usage = routectl_usage::handle_with_closed_channel();
    park(&swap.load(), "glm");
    let overlay_dir = dir.path().join("routectl");
    std::fs::create_dir_all(&overlay_dir).unwrap();
    std::fs::write(
        overlay_dir.join("catalog_overlay.json"),
        r#"{"schema_version":1,"revision":1,"cells":{"anthropic-api:claude-opus-4-8*":
               {"source":"user","verified_at":"2026-07-01","wm":9.5}}}"#,
    )
    .unwrap();

    // Act
    let outcome = handle_config_reload(
        Some(&cfg_path),
        &config,
        secrets,
        &swap,
        &usage,
        ReloadTrigger::ConfigFile,
        &mut never_shutdown(),
    )
    .await;

    // Assert
    assert_eq!(outcome.err(), Some(ReloadFailure::BoundaryNotAdmitted));
    assert_eq!(
        park_reason(&swap.load(), "glm"),
        Some(UpstreamReason::CredentialExpired),
    );
}
