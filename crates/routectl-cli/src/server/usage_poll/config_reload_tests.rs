//! The poll schedule follows the live config across a reload: a seat a
//! reload adds is scheduled on the next sweep, and a seat a reload removes
//! leaves the schedule with all of its state. Only `due` runs, so nothing
//! is ever sent; the credential store is an empty tempdir file.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use arc_swap::ArcSwap;
use routectl_auth::{OAuthStore, SecretStore};
use routectl_router::{CURRENT_CONFIG_VERSION, Config, Router, UsageVendor};

use super::client::UsagePollClient;
use super::schedule::{PollOutcome, RandomJitter};
use super::{UsagePollJob, UsagePollState, WallClock};
use crate::server::reload::handle_config_reload;
use crate::server::router_build::build_router_from_config;
use crate::server::seat_driver::SeatJob;
use crate::server::test_support::never_shutdown;

const SEAT_A: &str = "anthropic#a";
const SEAT_B: &str = "anthropic#b";

/// A wall clock fixed at one instant.
struct FixedClock(i64);

impl WallClock for FixedClock {
    fn now_ms(&self) -> i64 {
        self.0
    }
}

/// One model per labeled Anthropic login in `labels`.
fn config_text(labels: &[&str], usage_db: &Path) -> String {
    let mut text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n[usage]\ndb_path = \"{}\"\n\n",
        usage_db.display()
    );
    for label in labels {
        text.push_str(&format!(
            "[providers.seat_{label}]\nformat = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\ncredential = \"oauth://anthropic#{label}\"\nauth = \"bearer\"\n\n\
             [models.claude-{label}]\nprovider = \"seat_{label}\"\nupstream = \"claude-sonnet-4-6\"\n\n"
        ));
    }
    text
}

struct ReloadRig {
    config_path: PathBuf,
    usage_db: PathBuf,
    config: Arc<Config>,
    secrets: Arc<dyn SecretStore>,
    swap: Arc<ArcSwap<Router>>,
    job: UsagePollJob,
    _dir: tempfile::TempDir,
}

impl ReloadRig {
    async fn new(labels: &[&str]) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let creds = dir.path().join("credentials.json");
        super::rig::write_credentials(&creds, &[]);
        let store = Arc::new(OAuthStore::open(&creds).await.expect("open the store"));
        let usage_db = dir.path().join("usage.db");
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, config_text(labels, &usage_db)).expect("write config");
        let config = Arc::new(
            routectl_router::parse_config(&config_text(labels, &usage_db)).expect("parses"),
        );
        let secrets: Arc<dyn SecretStore> = store.clone();
        let router = build_router_from_config(config.clone(), secrets.clone())
            .await
            .expect("router builds");
        let clock: Arc<dyn WallClock> = Arc::new(FixedClock(super::rig::now_ms()));
        let job = UsagePollJob::with_parts(
            store,
            UsagePollClient::with_fronts_for_tests(&[]),
            routectl_usage::handle_with_closed_channel(),
            UsagePollState::new(RandomJitter::seeded(3), clock),
        );
        Self {
            config_path,
            usage_db,
            config,
            secrets,
            swap: Arc::new(ArcSwap::from_pointee(router)),
            job,
            _dir: dir,
        }
    }

    async fn reload_to(&mut self, labels: &[&str]) {
        std::fs::write(&self.config_path, config_text(labels, &self.usage_db))
            .expect("rewrite config");
        self.config = Box::pin(handle_config_reload(
            Some(&self.config_path),
            &self.config,
            self.secrets.clone(),
            &self.swap,
            &routectl_usage::handle_with_closed_channel(),
            &mut never_shutdown(),
        ))
        .await
        .expect("the reload applies")
        .expect("a successful reload returns its config");
    }

    fn sweep_schedule(&self) {
        let _ = self.job.due(&self.swap.load_full());
    }

    fn scheduled(&self, account: &str) -> bool {
        self.job.state.schedule.lock().seat(account).is_some()
    }

    fn last_attempt_ms(&self, account: &str) -> Option<i64> {
        self.job
            .state
            .schedule
            .lock()
            .seat(account)
            .and_then(super::schedule::SeatState::last_attempt_ms)
    }

    fn snapshot_accounts(&self) -> Vec<String> {
        self.job
            .state()
            .snapshot(&self.swap.load().config)
            .seats
            .into_iter()
            .map(|seat| seat.account)
            .collect()
    }
}

#[tokio::test]
async fn a_reload_that_adds_a_seat_schedules_it_on_the_next_sweep() {
    // Arrange
    let mut rig = ReloadRig::new(&["a"]).await;
    rig.sweep_schedule();
    assert!(rig.scheduled(SEAT_A), "premise: seat a is scheduled");
    assert!(!rig.scheduled(SEAT_B), "premise: seat b is not configured");

    // Act
    rig.reload_to(&["a", "b"]).await;
    rig.sweep_schedule();

    // Assert
    assert!(rig.scheduled(SEAT_B));
    assert!(rig.scheduled(SEAT_A));
    assert_eq!(rig.snapshot_accounts(), vec![SEAT_A, SEAT_B]);
}

#[tokio::test]
async fn a_reload_that_removes_a_seat_drops_it_and_its_state() {
    // Arrange: seat a carries an attempt the schedule remembers.
    let mut rig = ReloadRig::new(&["a", "b"]).await;
    rig.sweep_schedule();
    let attempted_ms = super::rig::now_ms();
    rig.job.state.schedule.lock().record(
        SEAT_A,
        UsageVendor::Anthropic,
        attempted_ms,
        PollOutcome::Http(500),
    );
    assert_eq!(rig.last_attempt_ms(SEAT_A), Some(attempted_ms), "premise");

    // Act
    rig.reload_to(&["b"]).await;
    rig.sweep_schedule();

    // Assert
    assert!(
        !rig.scheduled(SEAT_A),
        "the removed seat leaves the schedule"
    );
    assert!(rig.scheduled(SEAT_B), "the kept seat stays");
    assert_eq!(rig.snapshot_accounts(), vec![SEAT_B]);

    // A seat added back starts over: its earlier attempt is gone.
    rig.reload_to(&["a", "b"]).await;
    rig.sweep_schedule();
    assert!(rig.scheduled(SEAT_A));
    assert_eq!(rig.last_attempt_ms(SEAT_A), None);
}
