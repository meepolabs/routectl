//! `[usage] poll`: with it off the job sends nothing, a reload switches it
//! off and back on without a restart, and a result that lands after a
//! reload switched it off is discarded before it changes anything. The
//! router is built from config text by the daemon's own factory, reloads go
//! through the daemon's reload handler, and the Anthropic usage host is a
//! wiremock stub behind a TLS front.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use routectl_auth::{OAuthStore, SecretStore};
use routectl_router::{CURRENT_CONFIG_VERSION, Config, ObservationStamp, ReportState, Router};
use routectl_testkit::tls_front::TlsFront;
use routectl_usage::WriterMessage;
use tokio::sync::mpsc;
use wiremock::matchers::any;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::client::UsagePollClient;
use super::job_tests::{anthropic_body, rfc3339_in};
use super::rig::{ANTHROPIC_HOST, ManualClock, TOKEN_MARKER, now_ms, unix_now, write_credentials};
use super::schedule::RandomJitter;
use super::{PollApplicability, PollStateSnapshot, UsagePollJob, UsagePollState, WallClock};
use crate::server::reload::handle_config_reload;
use crate::server::router_build::build_router_from_config;
use crate::server::seat_driver::{SeatJob, SeatVerdict, sweep};
use crate::server::test_support::never_shutdown;

const SEAT: &str = "anthropic#a";
/// One scheduler tick's worth of wall clock, generous enough that every
/// deadline a poll can set (cadence, jitter, the boot spread) has passed.
const PAST_EVERY_DEADLINE_MS: i64 = 20 * 60_000;
const SWEEPS: usize = 4;

/// One Anthropic login with `[usage] poll` as `poll` gives it (absent for
/// `None`).
fn config_text(poll: Option<bool>, usage_db: &Path) -> String {
    let poll_line = poll.map_or_else(String::new, |poll| format!("poll = {poll}\n"));
    format!(
        "version = {CURRENT_CONFIG_VERSION}\n[usage]\ndb_path = \"{}\"\n{poll_line}\n\
         [providers.seat_a]\nformat = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\n\
         credential = \"oauth://{SEAT}\"\nauth = \"bearer\"\n\n\
         [models.claude-a]\nprovider = \"seat_a\"\nupstream = \"claude-sonnet-4-6\"\n",
        usage_db.display()
    )
}

struct SwitchRig {
    config_path: PathBuf,
    usage_db: PathBuf,
    config: Arc<Config>,
    secrets: Arc<dyn SecretStore>,
    swap: Arc<ArcSwap<Router>>,
    clock: Arc<ManualClock>,
    stub: MockServer,
    writer_rx: mpsc::Receiver<WriterMessage>,
    job: UsagePollJob,
    _front: TlsFront,
    _dir: tempfile::TempDir,
}

impl SwitchRig {
    /// The seat logged in with a token far from expiry, the usage host
    /// answering an accepted 5-hour reading.
    async fn new(poll: Option<bool>) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let creds = dir.path().join("credentials.json");
        write_credentials(&creds, &[(SEAT, TOKEN_MARKER, unix_now() + 3600, None)]);
        let store = Arc::new(OAuthStore::open(&creds).await.expect("open the store"));
        let usage_db = dir.path().join("usage.db");
        let config_path = dir.path().join("config.toml");
        let text = config_text(poll, &usage_db);
        std::fs::write(&config_path, &text).expect("write config");
        let config = Arc::new(routectl_router::parse_config(&text).expect("fixture parses"));
        let secrets: Arc<dyn SecretStore> = store.clone();
        let router = build_router_from_config(config.clone(), secrets.clone())
            .await
            .expect("router builds");
        let swap = Arc::new(ArcSwap::from_pointee(router));
        let stub = MockServer::start().await;
        Mock::given(any())
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(anthropic_body(25, &rfc3339_in(Duration::from_hours(2)))),
            )
            .mount(&stub)
            .await;
        let front = TlsFront::start(ANTHROPIC_HOST, *stub.address()).await;
        let client = UsagePollClient::with_fronts_for_tests(&[(ANTHROPIC_HOST, &front)]);
        let clock = Arc::new(ManualClock::starting_at(now_ms()));
        let wall: Arc<dyn WallClock> = clock.clone();
        let (tx, writer_rx) = mpsc::channel(8);
        let job = UsagePollJob::with_parts(
            store,
            client,
            routectl_usage::handle_over_channel(tx),
            UsagePollState::new(RandomJitter::seeded(11), wall),
            Arc::clone(&swap),
        );
        // First sight: the seat's first due time is drawn here, within the
        // boot spread every tick moves past.
        let _ = job.due(&swap.load_full());
        Self {
            config_path,
            usage_db,
            config,
            secrets,
            swap,
            clock,
            stub,
            writer_rx,
            job,
            _front: front,
            _dir: dir,
        }
    }

    fn pass_every_deadline(&self) {
        self.clock.set(self.clock.now_ms() + PAST_EVERY_DEADLINE_MS);
    }

    /// `count` scheduler ticks through the live router, the clock moved
    /// past every deadline before each.
    async fn tick(&self, count: usize) {
        for _ in 0..count {
            self.pass_every_deadline();
            sweep(&self.job, &self.swap.load_full()).await;
        }
    }

    async fn reload_to(&mut self, poll: Option<bool>) {
        std::fs::write(&self.config_path, config_text(poll, &self.usage_db))
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

    async fn hits(&self) -> usize {
        self.stub
            .received_requests()
            .await
            .expect("recording")
            .len()
    }

    fn report(&self) -> ReportState {
        self.swap
            .load()
            .account_readings(&ObservationStamp::now())
            .accounts
            .into_iter()
            .find(|reading| reading.account == SEAT)
            .expect("the configured account is listed")
            .report
    }

    fn last_attempt_ms(&self) -> Option<i64> {
        self.job
            .state
            .schedule
            .lock()
            .seat(SEAT)
            .and_then(super::schedule::SeatState::last_attempt_ms)
    }

    fn drain_writer(&mut self) -> usize {
        let mut count = 0;
        while self.writer_rx.try_recv().is_ok() {
            count += 1;
        }
        count
    }

    fn snapshot(&self) -> PollStateSnapshot {
        self.job
            .state()
            .snapshot(&self.swap.load().config, &std::collections::BTreeSet::new())
    }

    fn applicability(&self) -> PollApplicability {
        self.snapshot().applicability(SEAT)
    }
}

#[tokio::test]
async fn with_poll_off_no_request_reaches_the_usage_endpoint_over_several_ticks() {
    // (row, `[usage] poll`, requests the stub sees over the ticks)
    let rows = [
        ("off", Some(false), 0),
        ("on", Some(true), SWEEPS),
        ("absent", None, SWEEPS),
    ];
    for (row, poll, expected_hits) in rows {
        // Arrange
        let mut rig = SwitchRig::new(poll).await;

        // Act
        rig.tick(SWEEPS).await;

        // Assert
        assert_eq!(rig.hits().await, expected_hits, "{row}");
        let expected_report = if expected_hits == 0 {
            ReportState::NoReading
        } else {
            ReportState::Windows
        };
        assert_eq!(rig.report(), expected_report, "{row}");
        assert_eq!(rig.drain_writer(), expected_hits, "{row}: history rows");
    }
}

#[tokio::test]
async fn a_reload_switches_polling_off_and_back_on_without_a_restart() {
    // Arrange
    let mut rig = SwitchRig::new(None).await;
    rig.tick(1).await;
    assert_eq!(rig.hits().await, 1, "premise: polling runs");

    // Act: off for several ticks, then on again.
    rig.reload_to(Some(false)).await;
    let seat_before_a_sweep = rig.snapshot().seats.into_iter().next().expect("the seat");
    rig.tick(SWEEPS).await;
    let hits_while_off = rig.hits().await;
    let applicability_while_off = rig.applicability();
    rig.reload_to(Some(true)).await;
    rig.tick(1).await;

    // Assert
    assert_eq!(hits_while_off, 1, "nothing is sent while off");
    assert_eq!(
        (
            seat_before_a_sweep.last_attempt_ms,
            seat_before_a_sweep.next_due_ms
        ),
        (None, None),
        "switched off, the seat shows no sweep state even before the next sweep"
    );
    assert_eq!(
        applicability_while_off,
        PollApplicability::SwitchedOff(routectl_router::UsageVendor::Anthropic)
    );
    assert_eq!(
        rig.hits().await,
        2,
        "switched back on, a seat whose last reading is old is polled at once"
    );
    assert_eq!(
        rig.applicability(),
        PollApplicability::Polled(routectl_router::UsageVendor::Anthropic)
    );
}

#[tokio::test]
async fn a_result_landing_after_a_reload_switched_the_poll_off_is_discarded() {
    // (row, whether a reload switches the poll off while the request is out)
    for (row, switch_off) in [("switched off", true), ("left on", false)] {
        // Arrange: the seat is due and its request has gone out under the
        // router the sweep loaded.
        let mut rig = SwitchRig::new(None).await;
        let router = rig.swap.load_full();
        rig.pass_every_deadline();
        let [seat] = rig.job.due(&router).try_into().expect("the seat is due");
        let generation = router.acquire_credential_generation(&seat.account);
        let attempt = rig.job.run(&seat).await;
        assert_eq!(rig.hits().await, 1, "{row}: premise, the request went out");

        // Act
        if switch_off {
            rig.reload_to(Some(false)).await;
        }
        let (verdict, events) = routectl_testkit::with_capture(async {
            rig.job.apply(&router, &seat, generation, attempt)
        })
        .await;

        // Assert
        let landed = !switch_off;
        assert_eq!(verdict, SeatVerdict::Quiet, "{row}");
        let expected_report = if landed {
            ReportState::Windows
        } else {
            ReportState::NoReading
        };
        assert_eq!(rig.report(), expected_report, "{row}: meter");
        assert_eq!(rig.drain_writer(), usize::from(landed), "{row}: history");
        assert_eq!(rig.last_attempt_ms().is_some(), landed, "{row}: schedule");
        let logged = events
            .iter()
            .filter(|event| event.field("event") == Some("usage_poll"))
            .count();
        assert_eq!(logged, usize::from(landed), "{row}: log line");
    }
}
