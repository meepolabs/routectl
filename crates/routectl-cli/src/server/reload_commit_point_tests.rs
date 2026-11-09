//! A credentials reload writes to the live account map only at its commit
//! point: one whose rebuild fails keeps every park and placeholder mark.

use super::*;
use routectl_core::UpstreamReason;
use routectl_router::runtime_state::UnusableCause;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const KEY_A: &str = "ROUTECTL_COMMIT_POINT_TEST_KEY";

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
