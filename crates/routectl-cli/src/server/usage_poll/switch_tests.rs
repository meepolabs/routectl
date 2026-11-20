//! `[usage] poll`: with it off the job sends nothing, a reload switches it
//! off and back on without a restart, and a result that lands after a
//! reload switched it off is discarded before it changes anything, and a
//! seat still waiting in a sweep a reload switched it off during is never
//! read. The router is built from config text by the daemon's own factory,
//! reloads go through the daemon's reload handler, and each vendor usage
//! host is a wiremock stub behind a TLS front.

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
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use super::client::UsagePollClient;
use super::job_tests::{anthropic_body, codex_body, rfc3339_in};
use super::rig::{
    ACCOUNT_ID_MARKER, ANTHROPIC_HOST, CODEX_HOST, CODEX_SEAT, ManualClock, TOKEN_MARKER, now_ms,
    unix_now, write_credentials,
};
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

/// One Anthropic login, plus one Codex login when `with_codex`, with
/// `[usage] poll` as `poll` gives it (absent for `None`).
fn config_text(poll: Option<bool>, usage_db: &Path, with_codex: bool) -> String {
    let poll_line = poll.map_or_else(String::new, |poll| format!("poll = {poll}\n"));
    let mut text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n[usage]\ndb_path = \"{}\"\n{poll_line}\n\
         [providers.seat_a]\nformat = \"anthropic-messages\"\nbase_url = \"https://api.anthropic.com\"\n\
         credential = \"oauth://{SEAT}\"\nauth = \"bearer\"\n\n\
         [models.claude-a]\nprovider = \"seat_a\"\nupstream = \"claude-sonnet-4-6\"\n",
        usage_db.display()
    );
    if with_codex {
        text.push_str(&format!(
            "\n[providers.codex_c]\nformat = \"openai-responses\"\n\
             base_url = \"https://chatgpt.com/backend-api/codex\"\n\
             credential = \"oauth://{CODEX_SEAT}\"\nauth = \"bearer\"\n\n\
             [models.codex-c]\nprovider = \"codex_c\"\nupstream = \"gpt-5\"\n"
        ));
    }
    text
}

/// Answers an accepted 5-hour reading only once the test releases it,
/// telling the test when the request has arrived. Runs on the stub's own
/// thread, so blocking there holds no test task.
struct HeldAnswer {
    arrived: Arc<tokio::sync::Notify>,
    release: parking_lot::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl Respond for HeldAnswer {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        self.arrived.notify_one();
        let _ = self.release.lock().recv_timeout(Duration::from_secs(30));
        ResponseTemplate::new(200)
            .set_body_string(anthropic_body(25, &rfc3339_in(Duration::from_hours(2))))
    }
}

struct SwitchRig {
    config_path: PathBuf,
    usage_db: PathBuf,
    with_codex: bool,
    config: Arc<Config>,
    secrets: Arc<dyn SecretStore>,
    swap: Arc<ArcSwap<Router>>,
    clock: Arc<ManualClock>,
    stub: MockServer,
    codex_stub: MockServer,
    writer_rx: mpsc::Receiver<WriterMessage>,
    job: UsagePollJob,
    _fronts: (TlsFront, TlsFront),
    _dir: tempfile::TempDir,
}

impl SwitchRig {
    /// The seat logged in with a token far from expiry, the usage host
    /// answering an accepted 5-hour reading.
    async fn new(poll: Option<bool>) -> Self {
        Self::build(poll, false).await
    }

    /// As [`Self::new`], plus a logged-in Codex seat whose usage host
    /// answers every request with an accepted reading.
    async fn with_codex(poll: Option<bool>) -> Self {
        Self::build(poll, true).await
    }

    async fn build(poll: Option<bool>, with_codex: bool) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let creds = dir.path().join("credentials.json");
        let expires_at = unix_now() + 3600;
        let mut logins = vec![(SEAT, TOKEN_MARKER, expires_at, None)];
        if with_codex {
            logins.push((
                CODEX_SEAT,
                TOKEN_MARKER,
                expires_at,
                Some(ACCOUNT_ID_MARKER),
            ));
        }
        write_credentials(&creds, &logins);
        let store = Arc::new(OAuthStore::open(&creds).await.expect("open the store"));
        let usage_db = dir.path().join("usage.db");
        let config_path = dir.path().join("config.toml");
        let text = config_text(poll, &usage_db, with_codex);
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
        let codex_stub = MockServer::start().await;
        Mock::given(any())
            .respond_with(ResponseTemplate::new(200).set_body_string(codex_body(30)))
            .mount(&codex_stub)
            .await;
        let front = TlsFront::start(ANTHROPIC_HOST, *stub.address()).await;
        let codex_front = TlsFront::start(CODEX_HOST, *codex_stub.address()).await;
        let client = UsagePollClient::with_fronts_for_tests(&[
            (ANTHROPIC_HOST, &front),
            (CODEX_HOST, &codex_front),
        ]);
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
            with_codex,
            config,
            secrets,
            swap,
            clock,
            stub,
            codex_stub,
            writer_rx,
            job,
            _fronts: (front, codex_front),
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
        self.config = self.reload(poll).await;
    }

    /// Reload to `poll` through the daemon's handler, returning the config
    /// it applied without storing it, so a sweep may borrow the rig meanwhile.
    async fn reload(&self, poll: Option<bool>) -> Arc<Config> {
        std::fs::write(
            &self.config_path,
            config_text(poll, &self.usage_db, self.with_codex),
        )
        .expect("rewrite config");
        Box::pin(handle_config_reload(
            Some(&self.config_path),
            &self.config,
            self.secrets.clone(),
            &self.swap,
            &routectl_usage::handle_with_closed_channel(),
            &mut never_shutdown(),
        ))
        .await
        .expect("the reload applies")
        .expect("a successful reload returns its config")
    }

    async fn hits(&self) -> usize {
        self.stub
            .received_requests()
            .await
            .expect("recording")
            .len()
    }

    async fn codex_hits(&self) -> usize {
        self.codex_stub
            .received_requests()
            .await
            .expect("recording")
            .len()
    }

    fn report(&self) -> ReportState {
        self.report_of(SEAT)
    }

    fn report_of(&self, account: &str) -> ReportState {
        self.swap
            .load()
            .account_readings(&ObservationStamp::now())
            .accounts
            .into_iter()
            .find(|reading| reading.account == account)
            .expect("the configured account is listed")
            .report
    }

    fn last_attempt_ms(&self) -> Option<i64> {
        self.last_attempt_ms_of(SEAT)
    }

    fn last_attempt_ms_of(&self, account: &str) -> Option<i64> {
        self.job
            .state
            .schedule
            .lock()
            .seat(account)
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

#[tokio::test]
async fn a_reload_switching_the_poll_off_mid_sweep_sends_nothing_for_the_seats_still_waiting() {
    // (row, `[usage] poll` the reload sets while the first request is out)
    for (row, reload_to) in [("switched off", Some(false)), ("left on", Some(true))] {
        // Arrange: both vendors' seats due in one sweep; the Anthropic seat
        // goes first and its usage request is held at the stub.
        let mut rig = SwitchRig::with_codex(None).await;
        let arrived = Arc::new(tokio::sync::Notify::new());
        let (release, held) = std::sync::mpsc::channel();
        rig.stub.reset().await;
        Mock::given(any())
            .respond_with(HeldAnswer {
                arrived: Arc::clone(&arrived),
                release: parking_lot::Mutex::new(held),
            })
            .mount(&rig.stub)
            .await;
        rig.pass_every_deadline();
        let router = rig.swap.load_full();

        // Act: reload while the first request is out, then let it answer.
        let reload_then_release = async {
            arrived.notified().await;
            let _ = rig.reload(reload_to).await;
            release.send(()).expect("the held answer waits");
        };
        let ((_, ()), events) = routectl_testkit::with_capture(async {
            tokio::join!(sweep(&rig.job, &router), reload_then_release)
        })
        .await;

        // Assert
        let landed = reload_to == Some(true);
        assert_eq!(
            rig.hits().await,
            1,
            "{row}: premise, the first request went out"
        );
        assert_eq!(
            rig.codex_hits().await,
            usize::from(landed),
            "{row}: second seat"
        );
        let expected_report = if landed {
            ReportState::Windows
        } else {
            ReportState::NoReading
        };
        for account in [SEAT, CODEX_SEAT] {
            assert_eq!(
                rig.report_of(account),
                expected_report,
                "{row} {account}: meter"
            );
            assert_eq!(
                rig.last_attempt_ms_of(account).is_some(),
                landed,
                "{row} {account}: schedule"
            );
        }
        assert_eq!(
            rig.drain_writer(),
            2 * usize::from(landed),
            "{row}: history"
        );
        let poll_lines = events
            .iter()
            .filter(|event| event.target.contains("usage_poll"))
            .count();
        assert_eq!(poll_lines, 2 * usize::from(landed), "{row}: log lines");
    }
}
