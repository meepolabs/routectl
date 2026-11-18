//! A credentials reload forgets the meter readings of every seat whose
//! stored login it changed: a reading taken under the replaced login must not
//! describe the login that replaced it. A seat whose record did not change,
//! and a refresh routectl made itself, keep their readings.

use super::*;
use routectl_auth::oauth::testing::{ScriptedRefresh, ScriptedRefreshFlow};
use routectl_router::{ObservationStamp, ReportState, UsageVendor};
use std::time::{SystemTime, UNIX_EPOCH};

const SEAT_A: &str = "anthropic#a";
const SEAT_B: &str = "anthropic#b";

fn config_text() -> String {
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n\
         [providers.seat_a]\nformat = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\ncredential = \"oauth://anthropic#a\"\nauth = \"bearer\"\n\n\
         [providers.seat_b]\nformat = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\ncredential = \"oauth://anthropic#b\"\nauth = \"bearer\"\n\n\
         [models.claude-a]\nprovider = \"seat_a\"\nupstream = \"claude-sonnet-4-6\"\n\n\
         [models.claude-b]\nprovider = \"seat_b\"\nupstream = \"claude-sonnet-4-6\"\n"
    )
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
    config: Arc<Config>,
    swap: Arc<ArcSwap<Router>>,
    _usage_dir: tempfile::TempDir,
    _dir: tempfile::TempDir,
}

impl Rig {
    async fn new(script: ScriptedRefresh) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let creds = dir.path().join("credentials.json");
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("a post-epoch clock")
            .as_secs();
        write_pool_credentials_at(&creds, &[(SEAT_A, "tok-a"), (SEAT_B, "tok-b")], stamp);
        let store = Arc::new(
            routectl_auth::OAuthStore::open_with_scripted_refresh_for_tests(
                &creds,
                Arc::new(ScriptedRefreshFlow::new(script)),
            )
            .await
            .expect("open the scripted store"),
        );
        let mut config = routectl_router::parse_config(&config_text()).expect("fixture parses");
        let usage_dir = isolate_usage_db(&mut config);
        let config = Arc::new(config);
        let secrets: Arc<dyn SecretStore> = store.clone();
        let router = build_router_from_config(config.clone(), secrets)
            .await
            .expect("router builds");
        Self {
            creds,
            stamp,
            store,
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
        let secrets: Arc<dyn SecretStore> = self.store.clone();
        handle_credentials_reload(
            &Some(self.store.clone()),
            &self.config,
            secrets,
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
