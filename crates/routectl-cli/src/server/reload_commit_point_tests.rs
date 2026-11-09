//! A reload writes to the live account map only at its commit point. The
//! candidate router shares that map from carry-over on, so a reload rejected
//! after carry-over must leave it as the live router had it: no placeholder
//! mark from the candidate's build, no entry park lifted, and a park an
//! in-flight attempt writes while the reload waits on its replay boundary
//! kept. A
//! credentials reload whose rebuild fails likewise keeps every park and
//! placeholder mark.

use super::*;
use routectl_core::UpstreamReason;
use routectl_router::runtime_state::UnusableCause;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const KEY_A: &str = "ROUTECTL_COMMIT_POINT_TEST_KEY";
const MEMBER_A: &str = "claude#seat_a";
const MEMBER_B: &str = "claude#seat_b";

/// Pool `the-pool` of OAuth seats `anthropic#a` and `anthropic#b` behind
/// model `claude`, and model `glm` on entry `compat` with an env credential.
/// `compat_rpm` adds an `rpm_limit` to that entry, so a reload can change it.
fn pool_config_text(compat_rpm: Option<u32>) -> String {
    let mut text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [server]\nhost = \"127.0.0.1\"\nport = 0\n\n"
    );
    for seat in ["a", "b"] {
        text.push_str(&format!(
            "[providers.seat_{seat}]\nkind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic#{seat}\"\nauth_kind = \"oauth-bearer\"\n\n"
        ));
    }
    text.push_str(&format!(
        "[providers.compat]\nkind = \"openai-compat\"\nbase_url = \"http://127.0.0.1:1/v1\"\n\
         api_key_ref = \"env://{KEY_A}\"\n"
    ));
    if let Some(rpm) = compat_rpm {
        text.push_str(&format!("rpm_limit = {rpm}\n"));
    }
    text.push_str(
        "\n[pools.the-pool]\nmembers = [\"seat_a\", \"seat_b\"]\n\n\
         [models.claude]\nprovider = \"the-pool\"\nupstream = \"claude-sonnet-4-6\"\n\n\
         [models.glm]\nprovider = \"compat\"\nupstream = \"glm-4.6\"\n",
    );
    text
}

/// The entry-account model of [`pool_config_text`].
const ENTRY_MODEL: &str = "glm";

/// An overlay revision bump, so the reload must cross the replay boundary.
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

fn typed_rejection() -> routectl_core::Error {
    routectl_core::Error::upstream("p", 401, "credential rejected")
        .with_upstream_reason(Some(UpstreamReason::CredentialInvalid))
}

/// The park reason and unusable cause `router` reports on `state_key`.
fn account_state(
    router: &Router,
    state_key: &str,
) -> (Option<UpstreamReason>, Option<UnusableCause>) {
    let target = router
        .status_targets(Instant::now())
        .into_iter()
        .find(|target| target.state_key == state_key)
        .unwrap_or_else(|| panic!("no status target {state_key}"));
    (
        target.gate.credential_park.map(|park| park.reason),
        target.gate.credential_unusable.map(|mark| mark.cause),
    )
}

/// The config-reload rig: both seats are logged in when the live router is
/// built. [`Self::log_out_seat_b`] then drops seat b's record from the
/// shared credentials store without touching the router, so the next
/// config reload builds a candidate holding seat b as a placeholder.
struct ConfigRig {
    creds: PathBuf,
    cfg_path: PathBuf,
    oauth: Arc<routectl_auth::OAuthStore>,
    secrets: Arc<dyn SecretStore>,
    config: Arc<Config>,
    swap: Arc<ArcSwap<Router>>,
    _usage_dir: tempfile::TempDir,
}

impl ConfigRig {
    async fn new(dir: &Path) -> Self {
        let creds = dir.join("routectl").join("credentials.json");
        write_pool_credentials(
            &creds,
            &[("anthropic#a", "tok-a"), ("anthropic#b", "tok-b")],
        );
        let cfg_path = dir.join("config.toml");
        std::fs::write(&cfg_path, pool_config_text(None)).unwrap();
        let composite = CompositeStore::open_at(&creds)
            .await
            .expect("open composite store at temp creds path");
        let oauth = composite.oauth_store().expect("oauth arm present");
        let secrets: Arc<dyn SecretStore> = Arc::new(composite);
        let mut config =
            routectl_router::parse_config(&pool_config_text(None)).expect("fixture parses");
        let usage_dir = isolate_usage_db(&mut config);
        let config = Arc::new(config);
        let router =
            build_router_from_config_with_overlay(config.clone(), &Arc::default(), secrets.clone())
                .await
                .expect("initial router build");
        assert_eq!(
            account_state(&router, MEMBER_B),
            (None, None),
            "premise: seat b is built and unmarked"
        );
        Self {
            creds,
            cfg_path,
            oauth,
            secrets,
            config,
            swap: Arc::new(ArcSwap::from_pointee(router)),
            _usage_dir: usage_dir,
        }
    }

    async fn log_out_seat_b(&self) {
        write_pool_credentials(&self.creds, &[("anthropic#a", "tok-a")]);
        self.oauth
            .reload_from_disk()
            .await
            .expect("the store rereads the file");
    }

    /// Edit the `compat` entry and bump the overlay revision, then reload:
    /// the reload must cross the replay boundary, and on commit it lifts the
    /// `compat` account's park.
    async fn reload(
        &self,
        dir: &Path,
        usage: &routectl_usage::UsageHandle,
    ) -> Result<Option<(Arc<Config>, Arc<CatalogOverlay>)>, ReloadFailure> {
        std::fs::write(&self.cfg_path, pool_config_text(Some(7))).unwrap();
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
async fn a_config_reload_refused_at_the_boundary_marks_no_new_placeholder() {
    // Arrange: a closed usage channel refuses the boundary after carry-over.
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let _key = ScopedEnv::set(KEY_A, "key-a");
    let rig = ConfigRig::new(dir.path()).await;
    let before = rig.swap.load_full();
    rig.log_out_seat_b().await;

    // Act
    let outcome = rig
        .reload(dir.path(), &routectl_usage::handle_with_closed_channel())
        .await;

    // Assert
    assert_eq!(outcome.err(), Some(ReloadFailure::BoundaryNotAdmitted));
    assert!(Arc::ptr_eq(&before, &rig.swap.load_full()));
    assert_eq!(account_state(&rig.swap.load(), MEMBER_B), (None, None));
}

/// What the writer does with the boundary batch the reload is waiting on.
#[derive(Clone, Copy, Debug)]
enum Boundary {
    /// Forwarded to a real writer: the boundary commits and the reload
    /// publishes.
    Commits,
    /// Dropped unanswered: the boundary is not durable and the reload is
    /// rejected.
    Lost,
}

/// One barrier row: the boundary's fate, the reload's verdict, and what the
/// published router reports for seats a and b and the `compat` entry
/// account afterwards.
type BarrierRow = (
    Boundary,
    Result<(), ReloadFailure>,
    (Option<UpstreamReason>, Option<UnusableCause>),
    (Option<UpstreamReason>, Option<UnusableCause>),
    Option<UpstreamReason>,
);

/// Holds the reload at its replay boundary: the batch arriving on the
/// writer channel proves carry-over and admission are done and publication
/// is not. At that point an attempt admitted before the reload is rejected,
/// parking seat a on the live map, and the batch is then committed or lost.
/// A config reload never lifts an OAuth seat, so the park stands either way.
/// The `compat` entry account is parked before the reload, which edits that
/// entry: only a committed reload lifts it. The committed row is the positive
/// control for that lift and for seat b's placeholder mark.
#[tokio::test]
#[serial_test::serial]
async fn a_park_written_while_a_reload_waits_on_its_boundary_survives_a_rejection() {
    let rows: [BarrierRow; 2] = [
        (
            Boundary::Lost,
            Err(ReloadFailure::BoundaryNotDurable),
            (Some(UpstreamReason::CredentialInvalid), None),
            (None, None),
            Some(UpstreamReason::CredentialExpired),
        ),
        (
            Boundary::Commits,
            Ok(()),
            (Some(UpstreamReason::CredentialInvalid), None),
            (None, Some(UnusableCause::NotLoggedIn)),
            None,
        ),
    ];
    for (boundary, expected, expected_a, expected_b, expected_compat) in rows {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
        let _key = ScopedEnv::set(KEY_A, "key-a");
        let rig = ConfigRig::new(dir.path()).await;
        let live = rig.swap.load_full();
        assert!(live.park_credential_for_tests(
            ENTRY_MODEL,
            UpstreamReason::CredentialExpired,
            Instant::now() + Duration::from_hours(1),
        ));
        let generation = live
            .admit_for_credential_tests(MEMBER_A)
            .expect("a healthy seat admits");
        rig.log_out_seat_b().await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let usage = routectl_usage::handle_over_channel(tx);
        let (writer_tx, writer_rx) = tokio::sync::mpsc::channel(8);
        let db_path = dir.path().join("usage.db");
        drop(routectl_usage::open(&db_path).expect("migrating open"));
        let writer = UsageWriter::start_over_channel(
            db_path,
            0,
            writer_rx,
            Arc::new(routectl_usage::UsageCounters::default()),
        );
        let mut at_barrier = None;

        // Act
        let barrier = async {
            let batch = rx.recv().await.expect("the reload admits its boundary");
            let parked = live.settle_credential_rejection_for_tests(
                MEMBER_A,
                &typed_rejection(),
                generation,
            );
            at_barrier = Some((
                parked,
                Arc::ptr_eq(&live, &rig.swap.load_full()),
                account_state(&live, MEMBER_B),
            ));
            match boundary {
                Boundary::Commits => writer_tx.send(batch).await.expect("writer running"),
                Boundary::Lost => drop(batch),
            }
        };
        let (outcome, ()) = tokio::join!(rig.reload(dir.path(), &usage), barrier);

        // Assert
        assert_eq!(
            at_barrier,
            Some((true, true, (None, None))),
            "{boundary:?}: at the barrier the attempt parks seat a, the live \
             router is still published, and seat b is unmarked"
        );
        assert_eq!(outcome.map(|_| ()), expected, "{boundary:?}");
        assert_eq!(
            Arc::ptr_eq(&live, &rig.swap.load_full()),
            expected.is_err(),
            "{boundary:?}: only a committed reload publishes"
        );
        let published = rig.swap.load_full();
        assert_eq!(
            account_state(&published, MEMBER_A),
            expected_a,
            "{boundary:?}"
        );
        assert_eq!(
            account_state(&published, MEMBER_B),
            expected_b,
            "{boundary:?}"
        );
        assert_eq!(
            account_state(&published, ENTRY_MODEL).0,
            expected_compat,
            "{boundary:?}: only a committed reload lifts the edited entry's park"
        );
        drop((usage, writer_tx));
        drain_usage_writer_strict(writer).await;
    }
}

/// The credentials-reload rig: pool `the-pool` of OAuth seats `a`, `b` and
/// `c` behind model `claude`, where seat `a` has no login (a placeholder),
/// plus direct model `glm` on an env key that alias `fast` routes, so a
/// rebuild without that key fails.
struct CredentialsRig {
    creds: PathBuf,
    stamp: u64,
    oauth: Arc<routectl_auth::OAuthStore>,
    secrets: Arc<dyn SecretStore>,
    config: Arc<Config>,
    swap: Arc<ArcSwap<Router>>,
    _usage_dir: tempfile::TempDir,
}

fn credentials_config_text() -> String {
    let mut text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [server]\nhost = \"127.0.0.1\"\nport = 0\n\n"
    );
    for seat in ["a", "b", "c"] {
        text.push_str(&format!(
            "[providers.seat_{seat}]\nkind = \"anthropic-api\"\n\
             api_key_ref = \"oauth://anthropic#{seat}\"\nauth_kind = \"oauth-bearer\"\n\n"
        ));
    }
    text.push_str(&format!(
        "[providers.compat]\nkind = \"openai-compat\"\nbase_url = \"http://127.0.0.1:1/v1\"\n\
         api_key_ref = \"env://{KEY_A}\"\n\n\
         [pools.the-pool]\nmembers = [\"seat_a\", \"seat_b\", \"seat_c\"]\n\n\
         [models.claude]\nprovider = \"the-pool\"\nupstream = \"claude-sonnet-4-6\"\n\n\
         [models.glm]\nprovider = \"compat\"\nupstream = \"glm-4.6\"\n\n\
         [aliases]\nfast = \"glm\"\n"
    ));
    text
}

impl CredentialsRig {
    async fn new(dir: &Path) -> Self {
        let creds = dir.join("routectl").join("credentials.json");
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        write_pool_credentials_at(
            &creds,
            &[("anthropic#b", "tok-b"), ("anthropic#c", "tok-c")],
            stamp,
        );
        let composite = CompositeStore::open_at(&creds)
            .await
            .expect("open composite store at temp creds path");
        let oauth = composite.oauth_store().expect("oauth arm present");
        let secrets: Arc<dyn SecretStore> = Arc::new(composite);
        let mut config =
            routectl_router::parse_config(&credentials_config_text()).expect("fixture parses");
        let usage_dir = isolate_usage_db(&mut config);
        let config = Arc::new(config);
        let router =
            build_router_from_config_with_overlay(config.clone(), &Arc::default(), secrets.clone())
                .await
                .expect("initial router build");
        Self {
            creds,
            stamp,
            oauth,
            secrets,
            config,
            swap: Arc::new(ArcSwap::from_pointee(router)),
            _usage_dir: usage_dir,
        }
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
}

/// `(rebuild succeeds, seat a, seat b, whether c's in-flight rejection parks)`.
type CredentialsRow = (
    bool,
    (Option<UpstreamReason>, Option<UnusableCause>),
    (Option<UpstreamReason>, Option<UnusableCause>),
    bool,
);

/// Seat `a` logs in, so the seat set changes and the router rebuilds; seats
/// `b` (parked) and `c` (an attempt in flight) get new tokens. A failed
/// rebuild publishes nothing, so it lifts nothing: `b` stays parked, `a`
/// keeps its placeholder cause, and `c`'s attempt, admitted before the
/// reload, still parks it. A published rebuild lifts both and starts `c` on a
/// new generation.
#[tokio::test]
#[serial_test::serial]
async fn a_credentials_reload_lifts_parks_only_when_its_rebuild_publishes() {
    let rows: [CredentialsRow; 2] = [
        (
            false,
            (None, Some(UnusableCause::NotLoggedIn)),
            (Some(UpstreamReason::CredentialExpired), None),
            true,
        ),
        (true, (None, None), (None, None), false),
    ];
    for (rebuild_succeeds, expected_a, expected_b, expected_c_parks) in rows {
        // Arrange
        let dir = tempfile::tempdir().unwrap();
        let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
        let key = ScopedEnv::set(KEY_A, "key-a");
        let rig = CredentialsRig::new(dir.path()).await;
        let live = rig.swap.load_full();
        assert_eq!(
            account_state(&live, "claude#seat_a"),
            (None, Some(UnusableCause::NotLoggedIn)),
            "premise: seat a is a placeholder"
        );
        assert!(live.park_credential_for_tests(
            "claude#seat_b",
            UpstreamReason::CredentialExpired,
            Instant::now() + Duration::from_hours(1),
        ));
        let generation = live
            .admit_for_credential_tests("claude#seat_c")
            .expect("a healthy seat admits");
        write_pool_credentials_at(
            &rig.creds,
            &[
                ("anthropic#a", "tok-a"),
                ("anthropic#b", "tok-b2"),
                ("anthropic#c", "tok-c2"),
            ],
            rig.stamp,
        );
        if !rebuild_succeeds {
            drop(key);
        }

        // Act
        rig.reload_credentials().await;
        let c_parks = live.settle_credential_rejection_for_tests(
            "claude#seat_c",
            &typed_rejection(),
            generation,
        );

        // Assert
        let name = if rebuild_succeeds {
            "published"
        } else {
            "failed"
        };
        assert_eq!(
            !Arc::ptr_eq(&live, &rig.swap.load_full()),
            rebuild_succeeds,
            "{name}: premise, the rebuild's outcome"
        );
        let published = rig.swap.load_full();
        assert_eq!(
            account_state(&published, "claude#seat_a"),
            expected_a,
            "{name}"
        );
        assert_eq!(
            account_state(&published, "claude#seat_b"),
            expected_b,
            "{name}"
        );
        assert_eq!(c_parks, expected_c_parks, "{name}");
    }
}
