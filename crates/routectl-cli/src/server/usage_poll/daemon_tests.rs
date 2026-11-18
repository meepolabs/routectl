//! The usage poll in the assembled daemon: with a credential store both
//! background drivers run and report their own health, a restart starts
//! every seat at `no_reading` whatever history the ledger holds, and the
//! poll's usage handle is released before the writer drains, so graceful
//! shutdown still drains rather than waiting out its deadline. Without a
//! store neither driver runs.
//!
//! Hermetic: the managed store is opened at a tempdir credentials file that
//! holds no login, named to the daemon directly rather than through the
//! process environment, so the one managed seat has no token and the poll
//! cannot send anything; no test here lets a sweep interval elapse either.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use routectl_auth::{MemoryStore, SecretStore};
use routectl_router::{CURRENT_CONFIG_VERSION, Config, ObservationStamp, ReportState, Router};
use routectl_testkit::with_capture;
use routectl_usage::{
    AccountReadingRow, AccountReadingSource, AccountReadingWindow, UsageWriter, WindowLengthClass,
    WindowStarted,
};
use serde_json::Value;
use tokio::net::TcpListener;

use crate::server::serve::{DaemonTestSeams, serve_on_listener_with_injected_router};

const SEAT: &str = "anthropic#a";
const WAIT: Duration = Duration::from_secs(10);
const SEEDED_ROWS: i64 = 2;

fn config(usage_db: &Path) -> Config {
    let text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n[usage]\ndb_path = \"{}\"\n\n\
         [providers.seat_a]\nformat = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\ncredential = \"oauth://anthropic#a\"\nauth = \"bearer\"\n\n\
         [models.claude-a]\nprovider = \"seat_a\"\nupstream = \"claude-sonnet-4-6\"\n",
        usage_db.display()
    );
    routectl_router::parse_config(&text).expect("fixture parses")
}

/// Write `SEEDED_ROWS` accepted poll readings for [`SEAT`] through the
/// production writer, as a previous daemon run would have.
async fn seed_history(usage_db: &Path) {
    let path = usage_db.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let (handle, writer) = UsageWriter::start(path, 4, 0);
        let now_ms = super::rig::now_ms();
        let window = |window_secs, length_class| AccountReadingWindow {
            window_secs,
            scope: None,
            length_class,
            used: 0.25,
            reset_ms: Some(now_ms + 3_600_000),
            started: WindowStarted::Yes,
        };
        handle.try_send_account_readings(vec![
            AccountReadingRow {
                ts_ms: now_ms,
                account: SEAT.into(),
                source: AccountReadingSource::Poll,
                window: Some(window(18_000, WindowLengthClass::Short)),
            },
            AccountReadingRow {
                ts_ms: now_ms,
                account: SEAT.into(),
                source: AccountReadingSource::Poll,
                window: Some(window(604_800, WindowLengthClass::Long)),
            },
        ]);
        drop(handle);
        writer.shutdown();
    })
    .await
    .expect("the seeding writer drains");
}

fn history_rows(usage_db: &Path) -> i64 {
    rusqlite::Connection::open(usage_db)
        .expect("raw open")
        .query_row(
            "SELECT COUNT(*) FROM account_readings WHERE account = ?1",
            [SEAT],
            |row| row.get(0),
        )
        .expect("count history rows")
}

struct Daemon {
    base_url: String,
    router: Arc<ArcSwap<Router>>,
    trigger: tokio::sync::oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<routectl_core::Result<()>>,
}

/// Where the daemon's credentials come from.
enum Credentials {
    /// The routectl-managed store, opened by the daemon itself at this file.
    ManagedAt(PathBuf),
    /// A caller-supplied resolver: the daemon has no managed store.
    Injected(Arc<dyn SecretStore>),
}

impl Daemon {
    /// Serve `config` over `credentials`.
    async fn spawn(config: Config, credentials: Credentials) -> Self {
        let (secrets, credentials_path) = match credentials {
            Credentials::ManagedAt(path) => (None, Some(path)),
            Credentials::Injected(store) => (Some(store), None),
        };
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let base_url = format!("http://{}", listener.local_addr().expect("read addr"));
        let (observer_tx, observer_rx) = tokio::sync::oneshot::channel();
        let (trigger, shutdown) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(serve_on_listener_with_injected_router(
            Arc::new(config),
            listener,
            None,
            secrets,
            DaemonTestSeams {
                router_observer: Some(observer_tx),
                shutdown: Some(shutdown),
                credentials_path,
                ..DaemonTestSeams::default()
            },
        ));
        let router = tokio::time::timeout(WAIT, observer_rx)
            .await
            .expect("the daemon publishes its router at boot")
            .expect("the observer is not dropped before publication");
        Self {
            base_url,
            router,
            trigger,
            handle,
        }
    }

    /// `/status/health`, once the listener answers.
    async fn health(&self) -> Value {
        let deadline = tokio::time::Instant::now() + WAIT;
        loop {
            if let Ok(response) = reqwest::get(format!("{}/status/health", self.base_url)).await
                && response.status().is_success()
            {
                return response.json().await.expect("health is JSON");
            }
            assert!(!self.handle.is_finished(), "the daemon exited at boot");
            assert!(
                tokio::time::Instant::now() < deadline,
                "the daemon never answered /status/health"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn shut_down(self) {
        drop(self.router);
        self.trigger.send(()).expect("the daemon holds the trigger");
        tokio::time::timeout(WAIT, self.handle)
            .await
            .expect("serve returns after the shutdown trigger")
            .expect("the serve task does not panic")
            .expect("graceful shutdown returns Ok");
    }
}

/// A daemon over a credential store and a ledger holding the seat's earlier
/// poll history: both drivers run with their own health, the restarted
/// meter holds no reading for the seat, and shutdown drains the writer with
/// the poll's handle already released.
#[tokio::test]
async fn with_a_store_both_drivers_run_a_restart_reads_no_reading_and_the_writer_drains() {
    // Arrange
    let dir = tempfile::tempdir().expect("fixture dir");
    let creds = dir.path().join("credentials.json");
    super::rig::write_credentials(&creds, &[]);
    let usage_db = dir.path().join("usage.db");
    seed_history(&usage_db).await;
    assert_eq!(
        history_rows(&usage_db),
        SEEDED_ROWS,
        "premise: the ledger holds the seat's history"
    );

    // Act
    let (outcome, events) = with_capture(async {
        let daemon = Daemon::spawn(config(&usage_db), Credentials::ManagedAt(creds)).await;
        let health = daemon.health().await;
        let reading = daemon
            .router
            .load()
            .account_readings(&ObservationStamp::now())
            .accounts
            .into_iter()
            .find(|reading| reading.account == SEAT)
            .expect("the configured seat is listed");
        daemon.shut_down().await;
        (health, reading)
    })
    .await;
    let (health, reading) = outcome;

    // Assert
    assert_eq!(health["schema_version"], 9);
    assert_eq!(health["data"]["seat_driver"]["running"], true);
    assert_eq!(health["data"]["usage_poll_driver"]["running"], true);
    assert_eq!(reading.report, ReportState::NoReading);
    assert_eq!(reading.last_accepted, None);
    assert!(reading.windows.is_empty(), "{:?}", reading.windows);
    let messages: Vec<&str> = events.iter().map(|e| e.message.as_str()).collect();
    assert!(
        messages.contains(&"usage writer drained"),
        "the writer drains once every producer handle is gone: {messages:?}"
    );
    assert!(
        !events.iter().any(|event| event
            .message
            .contains("seat driver stopped before shutdown")),
        "both drivers stop on the shutdown signal"
    );
}

/// A daemon with no credential store runs neither driver, and both health
/// rows say so.
#[tokio::test]
async fn without_a_store_neither_driver_runs() {
    // Arrange
    let dir = tempfile::tempdir().expect("usage dir");
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());

    // Act
    let daemon = Daemon::spawn(
        config(&dir.path().join("usage.db")),
        Credentials::Injected(secrets),
    )
    .await;
    let health = daemon.health().await;
    daemon.shut_down().await;

    // Assert
    assert_eq!(health["data"]["seat_driver"]["running"], false);
    assert_eq!(health["data"]["usage_poll_driver"]["running"], false);
}
