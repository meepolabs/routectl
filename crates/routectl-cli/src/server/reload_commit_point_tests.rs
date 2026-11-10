//! A reload writes to the live account map only at its commit point. A
//! committed config reload marks a seat logged out since boot as a
//! placeholder and lifts the park of each `[providers]` entry it changed, and
//! no OAuth seat's. A credentials reload whose rebuild fails keeps every park
//! and placeholder mark.

use super::*;
use routectl_core::UpstreamReason;
use routectl_router::runtime_state::UnusableCause;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const KEY_A: &str = "ROUTECTL_COMMIT_POINT_TEST_KEY";
const MEMBER_A: &str = "claude#seat_a";
const MEMBER_B: &str = "claude#seat_b";

/// The entry-account model of [`pool_config_text`].
const ENTRY_MODEL: &str = "glm";

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

/// Both seats are logged in when the live router is built; seat b then logs
/// out of the shared credentials store without touching the router, and the
/// operator edits the `compat` entry and reloads config.toml. The committed
/// reload marks seat b as a placeholder, lifts the edited entry's park, and
/// keeps seat a's park: a config reload never lifts an OAuth seat.
#[tokio::test]
#[serial_test::serial]
async fn a_committed_config_reload_marks_a_logged_out_seat_and_lifts_only_edited_entry_parks() {
    // Arrange
    let dir = tempfile::tempdir().unwrap();
    let _xdg = ScopedEnv::set("XDG_CONFIG_HOME", dir.path());
    let _key = ScopedEnv::set(KEY_A, "key-a");
    let creds = dir.path().join("routectl").join("credentials.json");
    write_pool_credentials(
        &creds,
        &[("anthropic#a", "tok-a"), ("anthropic#b", "tok-b")],
    );
    let cfg_path = dir.path().join("config.toml");
    std::fs::write(&cfg_path, pool_config_text(None)).unwrap();
    let composite = CompositeStore::open_at(&creds)
        .await
        .expect("open composite store at temp creds path");
    let oauth = composite.oauth_store().expect("oauth arm present");
    let secrets: Arc<dyn SecretStore> = Arc::new(composite);
    let mut config =
        routectl_router::parse_config(&pool_config_text(None)).expect("fixture parses");
    let _usage_dir = isolate_usage_db(&mut config);
    let config = Arc::new(config);
    let (usage, writer) = build_usage_writer(&config);
    let live = Arc::new(
        build_router_from_config_with_overlay(config.clone(), &Arc::default(), secrets.clone())
            .await
            .expect("initial router build"),
    );
    let swap = Arc::new(ArcSwap::from(Arc::clone(&live)));
    assert_eq!(
        account_state(&live, MEMBER_B),
        (None, None),
        "premise: seat b is built and unmarked"
    );
    assert!(live.park_credential_for_tests(
        ENTRY_MODEL,
        UpstreamReason::CredentialExpired,
        Instant::now() + Duration::from_hours(1),
    ));
    let generation = live
        .admit_for_credential_tests(MEMBER_A)
        .expect("a healthy seat admits");
    assert!(
        live.settle_credential_rejection_for_tests(MEMBER_A, &typed_rejection(), generation),
        "premise: the rejected attempt parks seat a"
    );
    write_pool_credentials(&creds, &[("anthropic#a", "tok-a")]);
    oauth
        .reload_from_disk()
        .await
        .expect("the store rereads the file");
    std::fs::write(&cfg_path, pool_config_text(Some(7))).unwrap();

    // Act
    let outcome = Box::pin(handle_config_reload(
        Some(&cfg_path),
        &config,
        secrets,
        &swap,
        &usage,
        ReloadTrigger::ConfigFile,
        &mut never_shutdown(),
    ))
    .await;

    // Assert
    assert!(
        matches!(outcome, Ok(Some(_))),
        "the reload commits: {:?}",
        outcome.as_ref().map(Option::is_some)
    );
    let published = swap.load_full();
    assert!(
        !Arc::ptr_eq(&live, &published),
        "the committed reload publishes"
    );
    assert_eq!(
        account_state(&published, MEMBER_A),
        (Some(UpstreamReason::CredentialInvalid), None),
        "a config reload keeps the OAuth seat's park"
    );
    assert_eq!(
        account_state(&published, MEMBER_B),
        (None, Some(UnusableCause::NotLoggedIn)),
        "the logged-out seat is marked at commit"
    );
    assert_eq!(
        account_state(&published, ENTRY_MODEL).0,
        None,
        "the committed reload lifts the edited entry's park"
    );
    drop(usage);
    drain_usage_writer_strict(writer).await;
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
