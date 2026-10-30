//! A reload lifts only the credential parks whose credential it may have
//! changed. A credentials reload lifts the OAuth seats whose stored record it
//! added, removed or changed, and nothing else: a routine in-process token
//! refresh rewrites the file with what the cache already holds, so its reload
//! lifts nothing. An accepted config reload re-resolves every `[providers]`
//! entry's credential, so it lifts every entry account and no OAuth seat. A
//! reload that fails keeps every park, and an attempt admitted before a reload
//! that lifted its account cannot re-park it after it.

use super::*;
use routectl_core::UpstreamReason;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const KEY_ENV: &str = "ROUTECTL_PARK_LIFT_TEST_KEY";

/// Models `claude-a` and `claude-b` on OAuth seats `anthropic#a` and
/// `anthropic#b`, and `glm` on entry `compat` with an env credential: three
/// accounts. `extra_model` adds `claude-a2` on seat `anthropic#a`'s entry.
fn mixed_config_text(extra_model: bool) -> String {
    let mut text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [server]\nhost = \"127.0.0.1\"\nport = 0\n\n\
         [providers.seat_a]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic#a\"\n\
         auth_kind = \"oauth-bearer\"\n\n\
         [providers.seat_b]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"oauth://anthropic#b\"\n\
         auth_kind = \"oauth-bearer\"\n\n\
         [providers.compat]\n\
         kind = \"openai-compat\"\n\
         base_url = \"http://127.0.0.1:1/v1\"\n\
         api_key_ref = \"env://{KEY_ENV}\"\n\n\
         [models.claude-a]\nprovider = \"seat_a\"\nupstream = \"claude-sonnet-4-6\"\n\n\
         [models.claude-b]\nprovider = \"seat_b\"\nupstream = \"claude-sonnet-4-6\"\n\n\
         [models.glm]\nprovider = \"compat\"\nupstream = \"glm-4.6\"\n"
    );
    if extra_model {
        text.push_str(
            "\n[models.claude-a2]\nprovider = \"seat_a\"\nupstream = \"claude-opus-4-8\"\n",
        );
    }
    text
}

const MODELS: [&str; 3] = ["claude-a", "claude-b", "glm"];

/// The live rig the reload coordinator owns over [`mixed_config_text`]: the
/// OAuth store and the composite secrets over one temp credentials file, the
/// router swap, and the config file a config reload reads. The caller holds
/// the XDG and key scopes.
struct Rig {
    creds: PathBuf,
    cfg_path: PathBuf,
    stamp: u64,
    oauth: Arc<routectl_auth::OAuthStore>,
    secrets: Arc<dyn SecretStore>,
    config: Arc<Config>,
    swap: Arc<ArcSwap<Router>>,
    _usage_dir: tempfile::TempDir,
}

impl Rig {
    async fn new(dir: &Path) -> Self {
        let creds = dir.join("routectl").join("credentials.json");
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        write_pool_credentials_at(
            &creds,
            &[("anthropic#a", "tok-a"), ("anthropic#b", "tok-b")],
            stamp,
        );
        let cfg_path = dir.join("config.toml");
        std::fs::write(&cfg_path, mixed_config_text(false)).unwrap();
        let composite = CompositeStore::open_at(&creds)
            .await
            .expect("open composite store at temp creds path");
        let oauth = composite.oauth_store().expect("oauth arm present");
        let secrets: Arc<dyn SecretStore> = Arc::new(composite);
        let mut config = routectl_router::parse_config(&mixed_config_text(false))
            .expect("fixture config parses");
        let usage_dir = isolate_usage_db(&mut config);
        let config = Arc::new(config);
        let router =
            build_router_from_config_with_overlay(config.clone(), &Arc::default(), secrets.clone())
                .await
                .expect("initial router build");
        Self {
            creds,
            cfg_path,
            stamp,
            oauth,
            secrets,
            config,
            swap: Arc::new(ArcSwap::from_pointee(router)),
            _usage_dir: usage_dir,
        }
    }

    /// Rewrite the credentials file with these seats, every record stamped
    /// as the rig's first write was, so an unchanged token is an unchanged
    /// record.
    fn write_seats(&self, seats: &[(&str, &str)]) {
        write_pool_credentials_at(&self.creds, seats, self.stamp);
    }

    async fn reload_credentials(&self) {
        handle_credentials_reload(
            &Some(self.oauth.clone()),
            &self.config,
            &Arc::new(CatalogOverlay::default()),
            self.secrets.clone(),
            &self.swap,
            &Arc::new(ArcSwap::from_pointee(ActivationState::default())),
        )
        .await;
    }

    async fn reload_config(
        &self,
        usage: &routectl_usage::UsageHandle,
    ) -> Result<Option<(Arc<Config>, Arc<CatalogOverlay>)>, ReloadFailure> {
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

    fn park_every_account(&self) {
        for model in MODELS {
            park(&self.swap.load(), model);
        }
    }

    fn parks(&self, models: &[&str]) -> Vec<Option<UpstreamReason>> {
        models
            .iter()
            .map(|model| park_reason(&self.swap.load(), model))
            .collect()
    }
}

const PARKED: Option<UpstreamReason> = Some(UpstreamReason::CredentialExpired);

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

/// The expected park on `claude-a`, `claude-b` and `glm`, in that order.
type ParkRow = [Option<UpstreamReason>; 3];

/// One credentials-reload row: its name, the seats the file is rewritten
/// with, and the parks expected after the reload.
type SeatEditRow = (
    &'static str,
    &'static [(&'static str, &'static str)],
    ParkRow,
);

#[tokio::test]
#[serial_test::serial]
async fn a_credentials_reload_lifts_only_the_seats_whose_record_changed() {
    let rows: [SeatEditRow; 3] = [
        (
            "unchanged file",
            &[("anthropic#a", "tok-a"), ("anthropic#b", "tok-b")],
            [PARKED, PARKED, PARKED],
        ),
        (
            "only seat b changed",
            &[("anthropic#a", "tok-a"), ("anthropic#b", "tok-b2")],
            [PARKED, None, PARKED],
        ),
        (
            "both seats changed",
            &[("anthropic#a", "tok-a2"), ("anthropic#b", "tok-b2")],
            [None, None, PARKED],
        ),
    ];
    for (name, seats, expected) in rows {
        // Arrange: every account parked, then the file rewritten.
        let dir = tempfile::tempdir().unwrap();
        let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
        let _key = ScopedEnv::set(KEY_ENV, "test-key");
        let rig = Rig::new(dir.path()).await;
        rig.park_every_account();
        let before = rig.swap.load_full();
        rig.write_seats(seats);

        // Act
        rig.reload_credentials().await;

        // Assert
        assert!(
            Arc::ptr_eq(&before, &rig.swap.load_full()),
            "{name}: a same-seat-set reload must not rebuild the router",
        );
        assert_eq!(rig.parks(&MODELS), expected, "{name}");
    }
}

/// A credentials-file key shaped like an entry account (`#compat`) names no
/// OAuth seat, so its appearance lifts nothing, even beside a seat whose
/// record did change. The key changes the seat set, so the router rebuilds.
#[tokio::test]
#[serial_test::serial]
async fn a_credentials_reload_never_lifts_an_entry_account_named_by_a_file_key() {
    let rows: [SeatEditRow; 2] = [
        (
            "entry-shaped key added",
            &[
                ("anthropic#a", "tok-a"),
                ("anthropic#b", "tok-b"),
                ("#compat", "tok-c"),
            ],
            [PARKED, PARKED, PARKED],
        ),
        (
            "entry-shaped key beside a changed seat",
            &[
                ("anthropic#a", "tok-a"),
                ("anthropic#b", "tok-b2"),
                ("#compat", "tok-c"),
            ],
            [PARKED, None, PARKED],
        ),
    ];
    for (name, seats, expected) in rows {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
        let _key = ScopedEnv::set(KEY_ENV, "test-key");
        let rig = Rig::new(dir.path()).await;
        rig.park_every_account();
        rig.write_seats(seats);

        // Act
        rig.reload_credentials().await;

        // Assert
        assert!(
            rig.oauth.credential_keys().await.contains("#compat"),
            "{name}: premise, the reload accepted the entry-shaped key",
        );
        assert_eq!(rig.parks(&MODELS), expected, "{name}");
    }
}

#[tokio::test]
#[serial_test::serial]
async fn a_failed_credentials_reload_keeps_every_park() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let _key = ScopedEnv::set(KEY_ENV, "test-key");
    let rig = Rig::new(dir.path()).await;
    rig.park_every_account();
    std::fs::write(&rig.creds, b"<<corrupt-json>>").unwrap();

    // Act
    rig.reload_credentials().await;

    // Assert
    assert_eq!(rig.parks(&MODELS), [PARKED, PARKED, PARKED]);
}

#[tokio::test]
#[serial_test::serial]
async fn an_accepted_config_reload_lifts_every_entry_account_and_no_oauth_seat() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let _key = ScopedEnv::set(KEY_ENV, "test-key");
    let rig = Rig::new(dir.path()).await;
    let (usage, writer) = build_usage_writer(&rig.config);
    rig.park_every_account();
    let before = rig.swap.load_full();

    // Act
    rig.reload_config(&usage)
        .await
        .expect("config reload applies")
        .expect("a successful reload returns its config");

    // Assert
    assert!(
        !Arc::ptr_eq(&before, &rig.swap.load_full()),
        "router swapped"
    );
    assert_eq!(rig.parks(&MODELS), [PARKED, PARKED, None]);
    drop(usage);
    drain_usage_writer_strict(writer).await;
}

#[tokio::test]
#[serial_test::serial]
async fn a_config_reload_adding_a_model_on_a_parked_oauth_seat_leaves_it_refused() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let _key = ScopedEnv::set(KEY_ENV, "test-key");
    let rig = Rig::new(dir.path()).await;
    let (usage, writer) = build_usage_writer(&rig.config);
    park(&rig.swap.load(), "claude-a");
    std::fs::write(&rig.cfg_path, mixed_config_text(true)).unwrap();

    // Act
    rig.reload_config(&usage)
        .await
        .expect("config reload applies")
        .expect("a successful reload returns its config");
    let admitted = rig.swap.load().admit_for_credential_tests("claude-a2");

    // Assert
    assert_eq!(
        admitted, None,
        "the new model on the parked seat is refused"
    );
    assert_eq!(rig.parks(&["claude-a", "claude-a2"]), [PARKED, PARKED]);
    drop(usage);
    drain_usage_writer_strict(writer).await;
}

/// The failure lands AFTER the runtime state was carried onto the candidate
/// router, which shares the live router's park map: lifting at carry-over
/// time would already have lifted the live parks.
#[tokio::test]
#[serial_test::serial]
async fn a_config_reload_rejected_after_carry_over_keeps_every_park() {
    // Arrange: a closed usage channel refuses the replay boundary that the
    // overlay revision change below requires.
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let _key = ScopedEnv::set(KEY_ENV, "test-key");
    let rig = Rig::new(dir.path()).await;
    let usage = routectl_usage::handle_with_closed_channel();
    rig.park_every_account();
    std::fs::write(
        dir.path().join("routectl").join("catalog_overlay.json"),
        r#"{"schema_version":1,"revision":1,"cells":{"anthropic-api:claude-opus-4-8*":
               {"source":"user","verified_at":"2026-07-01","wm":9.5}}}"#,
    )
    .unwrap();

    // Act
    let outcome = rig.reload_config(&usage).await;

    // Assert
    assert_eq!(outcome.err(), Some(ReloadFailure::BoundaryNotAdmitted));
    assert_eq!(rig.parks(&MODELS), [PARKED, PARKED, PARKED]);
}

fn typed_rejection() -> routectl_core::Error {
    routectl_core::Error::upstream("p", 401, "credential rejected")
        .with_upstream_reason(Some(UpstreamReason::CredentialInvalid))
}

/// A credentials reload starts a new generation only on the seats it lifted:
/// a rejection admitted before it re-parks a seat whose record did not change
/// and does not park one whose record did.
#[tokio::test]
#[serial_test::serial]
async fn a_rejection_admitted_before_a_credentials_reload_parks_only_an_unchanged_seat() {
    // Arrange: an attempt in flight on each seat; then only seat b changes.
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let _key = ScopedEnv::set(KEY_ENV, "test-key");
    let rig = Rig::new(dir.path()).await;
    let seats = ["claude-a", "claude-b"];
    let admitted: Vec<u64> = seats
        .iter()
        .map(|model| {
            rig.swap
                .load()
                .admit_for_credential_tests(model)
                .expect("a healthy seat admits")
        })
        .collect();
    rig.write_seats(&[("anthropic#a", "tok-a"), ("anthropic#b", "tok-b2")]);

    // Act
    rig.reload_credentials().await;
    let parked: Vec<bool> = seats
        .iter()
        .zip(admitted)
        .map(|(model, generation)| {
            rig.swap.load().settle_credential_rejection_for_tests(
                model,
                &typed_rejection(),
                generation,
            )
        })
        .collect();

    // Assert
    assert_eq!(parked, [true, false]);
    assert_eq!(
        rig.parks(&seats),
        [Some(UpstreamReason::CredentialInvalid), None]
    );
}

/// A config reload starts a new generation on every entry account and on no
/// OAuth seat.
#[tokio::test]
#[serial_test::serial]
async fn a_rejection_admitted_before_a_config_reload_parks_only_an_oauth_seat() {
    // Arrange: an attempt in flight on every account.
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let _key = ScopedEnv::set(KEY_ENV, "test-key");
    let rig = Rig::new(dir.path()).await;
    let (usage, writer) = build_usage_writer(&rig.config);
    let old_router = rig.swap.load_full();
    let admitted: Vec<u64> = MODELS
        .iter()
        .map(|model| {
            old_router
                .admit_for_credential_tests(model)
                .expect("a healthy account admits")
        })
        .collect();

    // Act: each attempt settles through the router that admitted it, after
    // the reload published a successor sharing its park map.
    rig.reload_config(&usage)
        .await
        .expect("config reload applies")
        .expect("a successful reload returns its config");
    let parked: Vec<bool> = MODELS
        .iter()
        .zip(admitted)
        .map(|(model, generation)| {
            old_router.settle_credential_rejection_for_tests(model, &typed_rejection(), generation)
        })
        .collect();

    // Assert
    assert!(
        !Arc::ptr_eq(&old_router, &rig.swap.load_full()),
        "router swapped"
    );
    assert_eq!(parked, [true, true, false]);
    drop(usage);
    drain_usage_writer_strict(writer).await;
}
