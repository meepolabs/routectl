//! A config reload moves the carried breaker and RPM state to the candidate's
//! tuning only once the reload commits. The carried slots are shared with the
//! live router, so a reload rejected after carry-over must leave that router's
//! gates on the settings it still reports.

use super::*;
use std::time::Instant;

const KEY_ENV: &str = "ROUTECTL_RETUNE_TEST_KEY";
const MODEL: &str = "glm";

/// One openai-compat model whose provider sets the two gate keys a test
/// varies.
fn gate_config_text(circuit_failures: u32, rpm_limit: u32) -> String {
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [server]\nhost = \"127.0.0.1\"\nport = 0\n\n\
         [providers.compat]\n\
         kind = \"openai-compat\"\n\
         base_url = \"http://127.0.0.1:1/v1\"\n\
         api_key_ref = \"env://{KEY_ENV}\"\n\
         circuit_failures = {circuit_failures}\n\
         rpm_limit = {rpm_limit}\n\n\
         [models.{MODEL}]\n\
         provider = \"compat\"\n\
         upstream = \"glm-4.6\"\n"
    )
}

/// An overlay revision bump, so the reload must cross the capability replay
/// boundary before it can publish.
fn write_revised_overlay(dir: &Path) {
    let overlay_dir = dir.join("routectl");
    std::fs::create_dir_all(&overlay_dir).unwrap();
    std::fs::write(
        overlay_dir.join("catalog_overlay.json"),
        r#"{"schema_version":1,"revision":1,"cells":{"anthropic-api:claude-opus-4-8*":
               {"source":"user","verified_at":"2026-07-01","wm":9.5}}}"#,
    )
    .unwrap();
}

/// The published router's projected RPM bucket for the model: a fresh bucket
/// sits at its capacity, and a retune caps it at the new one.
fn published_rpm_available(swap: &ArcSwap<Router>) -> Option<f64> {
    swap.load()
        .status_targets(Instant::now())
        .into_iter()
        .find(|target| target.state_key == MODEL)
        .expect("the model has a status target")
        .gate
        .rpm_available
}

/// The live rig: the config file, its parsed form, and the router swap built
/// from it with `circuit_failures = 5` and `rpm_limit = 100`.
struct Rig {
    cfg_path: PathBuf,
    config: Arc<Config>,
    secrets: Arc<dyn SecretStore>,
    swap: Arc<ArcSwap<Router>>,
    _usage_dir: tempfile::TempDir,
}

impl Rig {
    async fn new(dir: &Path) -> Self {
        let cfg_path = dir.join("config.toml");
        std::fs::write(&cfg_path, gate_config_text(5, 100)).unwrap();
        let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
        let mut config =
            routectl_router::parse_config(&gate_config_text(5, 100)).expect("fixture parses");
        let usage_dir = isolate_usage_db(&mut config);
        let config = Arc::new(config);
        let router =
            build_router_from_config_with_overlay(config.clone(), &Arc::default(), secrets.clone())
                .await
                .expect("initial router build");
        Self {
            cfg_path,
            config,
            secrets,
            swap: Arc::new(ArcSwap::from_pointee(router)),
            _usage_dir: usage_dir,
        }
    }

    /// Write the candidate config and an overlay revision bump, then reload.
    async fn reload_to_tighter_gates(
        &self,
        dir: &Path,
        usage: &routectl_usage::UsageHandle,
    ) -> Result<Option<(Arc<Config>, Arc<CatalogOverlay>)>, ReloadFailure> {
        std::fs::write(&self.cfg_path, gate_config_text(2, 1)).unwrap();
        write_revised_overlay(dir);
        Box::pin(handle_config_reload(
            Some(&self.cfg_path),
            &self.config,
            self.secrets.clone(),
            &self.swap,
            usage,
            ReloadTrigger::ConfigFile,
            &mut never_shutdown(),
        ))
        .await
    }
}

#[tokio::test]
#[serial_test::serial]
async fn a_config_reload_rejected_at_the_boundary_leaves_the_live_gates_untuned() {
    // Arrange: a closed usage channel refuses the replay boundary.
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let _key = ScopedEnv::set(KEY_ENV, "test-key");
    let rig = Rig::new(dir.path()).await;
    let usage = routectl_usage::handle_with_closed_channel();
    let before = rig.swap.load_full();
    assert_eq!(published_rpm_available(&rig.swap), Some(100.0));

    // Act
    let outcome = rig.reload_to_tighter_gates(dir.path(), &usage).await;

    // Assert: the old router is still published, on its own bucket capacity.
    assert_eq!(outcome.err(), Some(ReloadFailure::BoundaryNotAdmitted));
    assert!(Arc::ptr_eq(&before, &rig.swap.load_full()));
    assert_eq!(published_rpm_available(&rig.swap), Some(100.0));
}

/// Positive control for the rejection test: the same fixture, with a boundary
/// that commits, does surface the retune on the published router.
#[tokio::test]
#[serial_test::serial]
async fn a_config_reload_committed_at_the_boundary_retunes_the_carried_gates() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let _key = ScopedEnv::set(KEY_ENV, "test-key");
    let rig = Rig::new(dir.path()).await;
    let (usage, writer) = build_usage_writer(&rig.config);
    let before = rig.swap.load_full();

    // Act
    rig.reload_to_tighter_gates(dir.path(), &usage)
        .await
        .expect("config reload applies")
        .expect("a successful reload returns its config");

    // Assert: a new router, carrying the old bucket capped at the new limit.
    assert!(!Arc::ptr_eq(&before, &rig.swap.load_full()));
    assert_eq!(published_rpm_available(&rig.swap), Some(1.0));
    drop(usage);
    drain_usage_writer_strict(writer).await;
}
