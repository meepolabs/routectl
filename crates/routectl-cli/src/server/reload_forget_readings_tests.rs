//! A credentials reload forgets the meter readings of every seat whose
//! stored login it changed: a reading taken under the replaced login must not
//! describe the login that replaced it. A seat whose record did not change,
//! and a refresh routectl made itself, keep their readings. The forget does
//! not wait on the router rebuild a reload may need: a failed rebuild still
//! forgets.

use super::*;
use routectl_auth::oauth::testing::{ScriptedRefresh, ScriptedRefreshFlow};
use routectl_router::{ObservationStamp, ReportState, UsageVendor};
use routectl_testkit::ScopedEnv;
use std::time::{SystemTime, UNIX_EPOCH};

const SEAT_A: &str = "anthropic#a";
const SEAT_B: &str = "anthropic#b";
const SEAT_C: &str = "anthropic#c";
const ENTRY_KEY: &str = "ROUTECTL_FORGET_READINGS_TEST_KEY";

/// Seats `a` and `b` behind their own models. `with_env_entry` adds model
/// `glm` on an `env://` key that alias `fast` routes, so a rebuild without
/// that key fails.
fn config_text(with_env_entry: bool) -> String {
    let mut text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [providers.seat_a]\nformat = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\ncredential = \"oauth://anthropic#a\"\nauth = \"bearer\"\n\n\
         [providers.seat_b]\nformat = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\ncredential = \"oauth://anthropic#b\"\nauth = \"bearer\"\n\n\
         [models.claude-a]\nprovider = \"seat_a\"\nupstream = \"claude-sonnet-4-6\"\n\n\
         [models.claude-b]\nprovider = \"seat_b\"\nupstream = \"claude-sonnet-4-6\"\n"
    );
    if with_env_entry {
        text.push_str(&format!(
            "\n[providers.compat]\nformat = \"openai-chat\"\nbase_url = \"http://127.0.0.1:1/v1\"\ncredential = \"env://{ENTRY_KEY}\"\nauth = \"bearer\"\n\n\
             [models.glm]\nprovider = \"compat\"\nupstream = \"glm-4.6\"\n\n\
             [aliases]\nfast = \"glm\"\n"
        ));
    }
    text
}

/// A running five-hour usage body at 42%, resetting in two hours.
fn running_body() -> Vec<u8> {
    let resets_at = time::OffsetDateTime::now_utc() + time::Duration::hours(2);
    let resets_at = resets_at
        .format(&time::format_description::well_known::Rfc3339)
        .expect("a formattable instant");
    format!(
        r#"{{"five_hour": {{"utilization": 42, "resets_at": "{resets_at}"}}, "seven_day": null}}"#
    )
    .into_bytes()
}

struct Rig {
    creds: std::path::PathBuf,
    stamp: u64,
    store: Arc<routectl_auth::OAuthStore>,
    secrets: Arc<dyn SecretStore>,
    config: Arc<Config>,
    swap: Arc<ArcSwap<Router>>,
    _usage_dir: tempfile::TempDir,
    _dir: tempfile::TempDir,
}

fn credentials_path(dir: &tempfile::TempDir) -> (std::path::PathBuf, u64) {
    let creds = dir.path().join("credentials.json");
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("a post-epoch clock")
        .as_secs();
    write_pool_credentials_at(&creds, &[(SEAT_A, "tok-a"), (SEAT_B, "tok-b")], stamp);
    (creds, stamp)
}

impl Rig {
    /// Seats `a` and `b` on a store whose refreshes `script` answers.
    async fn new(script: ScriptedRefresh) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let (creds, stamp) = credentials_path(&dir);
        let store = Arc::new(
            routectl_auth::OAuthStore::open_with_scripted_refresh_for_tests(
                &creds,
                Arc::new(ScriptedRefreshFlow::new(script)),
            )
            .await
            .expect("open the scripted store"),
        );
        let secrets: Arc<dyn SecretStore> = store.clone();
        Self::build(dir, creds, stamp, store, secrets, &config_text(false)).await
    }

    /// Seats `a` and `b` plus the `env://` entry, on a store that resolves
    /// both kinds of reference and never refreshes.
    async fn with_env_entry() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let (creds, stamp) = credentials_path(&dir);
        let composite = CompositeStore::open_at(&creds)
            .await
            .expect("open the composite store");
        let store = composite.oauth_store().expect("the OAuth arm is present");
        let secrets: Arc<dyn SecretStore> = Arc::new(composite);
        Self::build(dir, creds, stamp, store, secrets, &config_text(true)).await
    }

    async fn build(
        dir: tempfile::TempDir,
        creds: std::path::PathBuf,
        stamp: u64,
        store: Arc<routectl_auth::OAuthStore>,
        secrets: Arc<dyn SecretStore>,
        text: &str,
    ) -> Self {
        let mut config = routectl_router::parse_config(text).expect("fixture parses");
        let usage_dir = isolate_usage_db(&mut config);
        let config = Arc::new(config);
        let router = build_router_from_config(config.clone(), secrets.clone())
            .await
            .expect("router builds");
        Self {
            creds,
            stamp,
            store,
            secrets,
            config,
            swap: Arc::new(ArcSwap::from_pointee(router)),
            _usage_dir: usage_dir,
            _dir: dir,
        }
    }

    /// Land an accepted poll reading for both seats.
    fn read_both(&self) {
        let router = self.swap.load_full();
        for account in [SEAT_A, SEAT_B] {
            let generation = router.acquire_credential_generation(account);
            router
                .observe_polled_usage(account, UsageVendor::Anthropic, &running_body(), generation)
                .expect("the reading is accepted");
        }
    }

    async fn reload_credentials(&self) {
        handle_credentials_reload(
            &Some(self.store.clone()),
            &self.config,
            self.secrets.clone(),
            &self.swap,
            &Arc::new(ArcSwap::from_pointee(ActivationState::default())),
        )
        .await;
    }

    fn reports(&self) -> [ReportState; 2] {
        let readings = self.swap.load().account_readings(&ObservationStamp::now());
        [SEAT_A, SEAT_B].map(|account| {
            readings
                .accounts
                .iter()
                .find(|reading| reading.account == account)
                .expect("the configured seat is listed")
                .report
        })
    }
}

/// Each row: how seat a's login changes before the credentials reload, and
/// the two seats' report states after it.
#[tokio::test]
async fn a_credentials_reload_forgets_the_readings_of_a_seat_whose_login_changed() {
    #[derive(Debug, Clone, Copy)]
    enum Change {
        Unchanged,
        ReLoggedOnDisk,
        RefreshedByRoutectl,
    }
    let rows = [
        (
            Change::Unchanged,
            [ReportState::Windows, ReportState::Windows],
        ),
        (
            Change::ReLoggedOnDisk,
            [ReportState::NoReading, ReportState::Windows],
        ),
        (
            Change::RefreshedByRoutectl,
            [ReportState::Windows, ReportState::Windows],
        ),
    ];
    for (change, expected) in rows {
        // Arrange
        let rig = Rig::new(ScriptedRefresh::Mint("tok-a-refreshed".into())).await;
        rig.read_both();
        assert_eq!(
            rig.reports(),
            [ReportState::Windows, ReportState::Windows],
            "{change:?}: premise, both seats hold a reading"
        );
        match change {
            Change::Unchanged => {}
            Change::ReLoggedOnDisk => write_pool_credentials_at(
                &rig.creds,
                &[(SEAT_A, "tok-a-other-login"), (SEAT_B, "tok-b")],
                rig.stamp,
            ),
            Change::RefreshedByRoutectl => {
                rig.store
                    .force_refresh("anthropic", Some("a"))
                    .await
                    .expect("the scripted refresh mints");
            }
        }

        // Act
        rig.reload_credentials().await;

        // Assert
        assert_eq!(rig.reports(), expected, "{change:?}");
    }
}

/// Seat a is re-logged on disk in the same reload that adds seat c, so the
/// router must rebuild, and the rebuild fails. The shared token cache already
/// dials a with the new login, so a's readings go anyway; b's record did not
/// change and keeps its reading.
#[tokio::test]
#[serial_test::serial]
async fn a_credentials_reload_whose_rebuild_fails_still_forgets_a_changed_seat() {
    // Arrange
    let key = ScopedEnv::set(ENTRY_KEY, "key-glm");
    let rig = Rig::with_env_entry().await;
    rig.read_both();
    let live = rig.swap.load_full();
    write_pool_credentials_at(
        &rig.creds,
        &[
            (SEAT_A, "tok-a-other-login"),
            (SEAT_B, "tok-b"),
            (SEAT_C, "tok-c"),
        ],
        rig.stamp,
    );
    drop(key);

    // Act
    rig.reload_credentials().await;

    // Assert
    assert!(
        Arc::ptr_eq(&live, &rig.swap.load_full()),
        "premise: the rebuild failed and published nothing"
    );
    assert_eq!(
        rig.reports(),
        [ReportState::NoReading, ReportState::Windows]
    );
}
