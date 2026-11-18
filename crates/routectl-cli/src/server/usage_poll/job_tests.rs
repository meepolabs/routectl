//! The usage poll job end to end through the seat driver's sweep: which
//! seats it polls, what each poll lands, the re-login race, shutdown during
//! a hung response, and the read-only poll-state snapshot. Every vendor
//! host is a wiremock stub behind a TLS front; credentials live in a
//! tempdir store.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use routectl_router::runtime_state::UnusableCause;
use routectl_router::{
    AcceptedReading, LengthClass, ReadingSource, ReadingWindow, ReportState, Started, UsageVendor,
};
use routectl_testkit::tls_front::TlsFront;
use routectl_usage::{AccountReadingSource, WindowLengthClass, WindowStarted};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use tokio::sync::watch;
use wiremock::ResponseTemplate;

use super::client::UsagePollClient;
use super::rig::{
    ANTHROPIC_HOST, ANTHROPIC_SEAT, CODEX_SEAT, Rig, TOKEN_MARKER, state_key_of, unix_now,
    write_credentials,
};
use super::{PollApplicability, PollAttempt, UsagePollJob, history};
use crate::server::seat_driver::{
    ManagedSeat, SEAT_DRIVER_INTERVAL, SeatDriverHealth, SeatJob, SeatVerdict, run_seat_driver,
    sweep,
};

const MINUTE_MS: i64 = 60_000;

pub(super) fn rfc3339_in(offset: Duration) -> String {
    OffsetDateTime::from(SystemTime::now() + offset)
        .format(&Rfc3339)
        .expect("a formattable instant")
}

/// An Anthropic usage body with the 5-hour window running at `percent`.
pub(super) fn anthropic_body(percent: u32, resets_at: &str) -> String {
    format!(
        r#"{{"five_hour": {{"utilization": {percent}, "resets_at": "{resets_at}"}}, "seven_day": null}}"#
    )
}

/// A Codex usage body with a running 5-hour primary window at `percent`.
fn codex_body(percent: u32) -> String {
    let reset_at = unix_now() + 3_600;
    format!(
        r#"{{"plan_type": "plus", "rate_limit": {{"allowed": true, "limit_reached": false, "primary_window": {{"used_percent": {percent}, "limit_window_seconds": 18000, "reset_after_seconds": 3600, "reset_at": {reset_at}}}, "secondary_window": null}}}}"#
    )
}

async fn answer_both_ok(rig: &Rig) {
    rig.answer_anthropic(
        ResponseTemplate::new(200)
            .set_body_string(anthropic_body(25, &rfc3339_in(Duration::from_hours(2)))),
    )
    .await;
    rig.answer_codex(ResponseTemplate::new(200).set_body_string(codex_body(30)))
        .await;
}

#[tokio::test]
async fn an_idle_seat_is_polled_once_due_and_a_seat_with_a_fresh_header_reading_is_not() {
    // Arrange: the Anthropic seat carries traffic (a fresh header reading),
    // the Codex seat is idle.
    let rig = Rig::new().await;
    answer_both_ok(&rig).await;
    rig.feed_header_reading().await;
    let job = rig.job();
    let router = rig.router();
    let start = rig.clock_ms();

    // Act: a sweep at first sight, one once the boot spread has passed, and
    // one straight after.
    sweep(&job, &router).await;
    let hits_at_first_sight = rig.codex_hits().await;
    rig.clock.set(start + 5 * MINUTE_MS);
    sweep(&job, &router).await;
    let hits_once_due = rig.codex_hits().await;
    sweep(&job, &router).await;

    // Assert
    assert_eq!(hits_at_first_sight, 0, "not due at first sight");
    assert_eq!(hits_once_due, 1, "due once the boot spread passed");
    assert_eq!(rig.codex_hits().await, 1, "polled once, not every sweep");
    assert_eq!(
        rig.anthropic_hits().await,
        0,
        "a seat with a fresh header reading is not polled"
    );
    let codex = rig.reading(CODEX_SEAT);
    assert_eq!(codex.report, ReportState::Windows);
    assert_eq!(
        codex.last_accepted.map(|last| last.source),
        Some(ReadingSource::Poll)
    );
    let anthropic = rig.reading(ANTHROPIC_SEAT);
    assert_eq!(
        anthropic.last_accepted.map(|last| last.source),
        Some(ReadingSource::Response)
    );
}

#[tokio::test]
async fn an_accepted_poll_lands_on_the_meter_and_enqueues_one_history_observation() {
    // Arrange
    let mut rig = Rig::new().await;
    answer_both_ok(&rig).await;
    let job = rig.job();
    let router = rig.router();
    rig.sight_then_pass_boot_spread(&job);

    // Act
    let (_, events) = routectl_testkit::with_capture(sweep(&job, &router)).await;

    // Assert: both seats accepted, one history message each, and no log
    // line carries a credential, account id, body or header.
    assert_eq!(super::rig::leaked(&events), Vec::<String>::new());
    assert_eq!(
        events
            .iter()
            .filter(|event| event.field("event") == Some("usage_poll"))
            .count(),
        2,
        "premise: the capture saw both polls"
    );
    assert_eq!((rig.anthropic_hits().await, rig.codex_hits().await), (1, 1));
    for account in [ANTHROPIC_SEAT, CODEX_SEAT] {
        let reading = rig.reading(account);
        assert_eq!(reading.report, ReportState::Windows, "{account}");
        let [window] = reading.windows.as_slice() else {
            panic!("{account}: one window, got {:?}", reading.windows);
        };
        assert_eq!(window.source, ReadingSource::Poll, "{account}");
    }
    assert_eq!(rig.drain_writer(), 2);
    assert_eq!(rig.counters.account_readings_enqueued(), 2);
    let snapshot = job.state().snapshot(&router.config);
    let results: Vec<_> = snapshot
        .seats
        .iter()
        .map(|seat| (seat.account.as_str(), seat.last_result))
        .collect();
    assert_eq!(
        results,
        [
            (ANTHROPIC_SEAT, Some("accepted")),
            (CODEX_SEAT, Some("accepted")),
        ]
    );
}

#[tokio::test]
async fn a_codex_login_with_no_account_id_is_auth_and_sends_nothing() {
    // Arrange
    let rig = Rig::with_seats(&[(CODEX_SEAT, TOKEN_MARKER, unix_now() + 3600, None)]).await;
    answer_both_ok(&rig).await;
    let job = rig.job();
    let router = rig.router();
    rig.sight_then_pass_boot_spread(&job);

    // Act
    sweep(&job, &router).await;

    // Assert
    assert_eq!(rig.codex_hits().await, 0);
    let snapshot = job.state().snapshot(&router.config);
    let codex = snapshot
        .seats
        .iter()
        .find(|seat| seat.account == CODEX_SEAT)
        .expect("the codex seat");
    assert_eq!(codex.last_result, Some("auth"));
    assert_eq!(rig.reading(CODEX_SEAT).report, ReportState::NoReading);
}

#[tokio::test]
async fn a_token_inside_the_refresh_lead_is_not_sent() {
    // Arrange: the Anthropic token expires in a minute, inside the lead.
    let rig = Rig::with_seats(&[(ANTHROPIC_SEAT, TOKEN_MARKER, unix_now() + 60, None)]).await;
    answer_both_ok(&rig).await;
    let job = rig.job();
    let router = rig.router();
    rig.sight_then_pass_boot_spread(&job);

    // Act
    sweep(&job, &router).await;

    // Assert
    assert_eq!(rig.anthropic_hits().await, 0);
    let snapshot = job.state().snapshot(&router.config);
    assert_eq!(snapshot.seats[0].last_result, Some("token_not_ready"));
}

/// Which halves of a credentials reload land between the token read and
/// `apply`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ReloginStep {
    /// Only the router's reset of the seat; the store is not reloaded.
    RouterOnly,
    /// Only the store's reload, before the router hears of it; with
    /// `new_login`, a new record is written to disk first.
    StoreOnly { new_login: bool },
    /// A new record on disk reloaded into the store, then the router's
    /// reset of the changed seat.
    StoreThenRouter,
}

/// Re-logs `account` in after the job read its token and fetched, before
/// the outcome is applied, as a credentials reload does, per `step`. Every
/// other seat runs untouched.
pub(super) struct ReloginDuringRun<'a> {
    pub(super) inner: &'a UsagePollJob,
    pub(super) account: &'static str,
    pub(super) rig_store: Arc<routectl_auth::OAuthStore>,
    pub(super) creds: std::path::PathBuf,
    pub(super) router: Arc<routectl_router::Router>,
    pub(super) step: ReloginStep,
}

impl SeatJob for ReloginDuringRun<'_> {
    const KIND: &'static str = UsagePollJob::KIND;

    type Outcome = PollAttempt;

    fn due(&self, router: &routectl_router::Router) -> Vec<ManagedSeat> {
        self.inner.due(router)
    }

    fn run(&self, seat: &ManagedSeat) -> impl Future<Output = PollAttempt> + Send {
        let fetched = self.inner.run(seat);
        let store = Arc::clone(&self.rig_store);
        let router = Arc::clone(&self.router);
        let creds = self.creds.clone();
        let account = seat.account.clone();
        let step = self.step;
        let relogs = seat.account == self.account;
        async move {
            let attempt = fetched.await;
            if !relogs {
                return attempt;
            }
            let write_new_login = || {
                write_credentials(
                    &creds,
                    &[(&account, "synthetic-relogin-token", unix_now() + 7200, None)],
                );
            };
            match step {
                ReloginStep::RouterOnly => {
                    router.lift_oauth_credential_parks([account.as_str()]);
                }
                ReloginStep::StoreOnly { new_login } => {
                    if new_login {
                        write_new_login();
                    }
                    store.reload_from_disk().await.expect("reload");
                }
                ReloginStep::StoreThenRouter => {
                    write_new_login();
                    let changed = store.reload_from_disk().await.expect("reload");
                    router.lift_oauth_credential_parks(changed.iter());
                }
            }
            attempt
        }
    }

    fn apply(
        &self,
        router: &routectl_router::Router,
        seat: &ManagedSeat,
        generation: u64,
        outcome: PollAttempt,
    ) -> SeatVerdict {
        self.inner.apply(router, seat, generation, outcome)
    }
}

#[tokio::test]
async fn a_relogin_between_token_read_and_apply_is_superseded() {
    // (step, the token the store holds after it)
    let rows = [
        (ReloginStep::StoreThenRouter, "synthetic-relogin-token"),
        (ReloginStep::RouterOnly, TOKEN_MARKER),
    ];
    for (step, stored_token) in rows {
        // Arrange: only the Anthropic seat, its body valid.
        let mut rig =
            Rig::with_seats(&[(ANTHROPIC_SEAT, TOKEN_MARKER, unix_now() + 3600, None)]).await;
        answer_both_ok(&rig).await;
        let router = rig.router();
        let poll = rig.job();
        let job = ReloginDuringRun {
            inner: &poll,
            account: ANTHROPIC_SEAT,
            rig_store: Arc::clone(&rig.store),
            creds: rig.creds.clone(),
            router: Arc::clone(&router),
            step,
        };
        rig.sight_then_pass_boot_spread(job.inner);

        // Act
        sweep(&job, &router).await;

        // Assert: the body was fetched with the old token, then refused.
        assert_eq!(rig.anthropic_hits().await, 1, "{step:?}: premise, fetched");
        assert_eq!(
            rig.store.peek_access_token(ANTHROPIC_SEAT).await,
            routectl_auth::PeekedAccessToken::Ready(routectl_auth::oauth::SecretToken::new(
                stored_token
            )),
            "{step:?}: premise, the store's login"
        );
        let snapshot = job.inner.state().snapshot(&router.config);
        assert_eq!(
            snapshot.seats[0].last_result,
            Some("superseded"),
            "{step:?}"
        );
        assert_eq!(
            rig.reading(ANTHROPIC_SEAT).report,
            ReportState::NoReading,
            "{step:?}"
        );
        assert_eq!(rig.drain_writer(), 0, "{step:?}: no history row");
        assert_eq!(rig.counters.account_readings_enqueued(), 0, "{step:?}");
    }
}

#[tokio::test]
async fn a_store_reload_the_router_has_not_seen_yet_supersedes_the_poll() {
    // Arrange: the store reloads a new login while the poll is in flight,
    // and the router has not reset the seat yet.
    let mut rig = Rig::with_seats(&[(ANTHROPIC_SEAT, TOKEN_MARKER, unix_now() + 3600, None)]).await;
    answer_both_ok(&rig).await;
    let router = rig.router();
    let poll = rig.job();
    let job = ReloginDuringRun {
        inner: &poll,
        account: ANTHROPIC_SEAT,
        rig_store: Arc::clone(&rig.store),
        creds: rig.creds.clone(),
        router: Arc::clone(&router),
        step: ReloginStep::StoreOnly { new_login: true },
    };
    rig.sight_then_pass_boot_spread(job.inner);
    let state_key = state_key_of(&router, ANTHROPIC_SEAT);

    // Act
    sweep(&job, &router).await;

    // Assert: fetched with the old token, the router's generation never
    // moved, and still nothing landed.
    assert_eq!(rig.anthropic_hits().await, 1, "premise: fetched");
    assert_eq!(
        router.credential_generation_for_tests(&state_key),
        0,
        "premise: the router fence alone would admit the body"
    );
    let snapshot = job.inner.state().snapshot(&router.config);
    assert_eq!(snapshot.seats[0].last_result, Some("superseded"));
    assert_eq!(rig.reading(ANTHROPIC_SEAT).report, ReportState::NoReading);
    assert_eq!(rig.drain_writer(), 0, "no history row");
    assert_eq!(rig.counters.account_readings_enqueued(), 0);
}

#[tokio::test]
async fn a_seat_awaiting_login_is_not_polled_until_its_mark_is_lifted() {
    for cause in [UnusableCause::Revoked, UnusableCause::NotLoggedIn] {
        // Arrange
        let rig = Rig::with_seats(&[(ANTHROPIC_SEAT, TOKEN_MARKER, unix_now() + 3600, None)]).await;
        answer_both_ok(&rig).await;
        let job = rig.job();
        let router = rig.router();
        assert!(
            router
                .mark_credential_unusable_for_tests(&state_key_of(&router, ANTHROPIC_SEAT), cause),
            "{cause:?}: premise, marked"
        );
        rig.sight_then_pass_boot_spread(&job);

        // Act
        sweep(&job, &router).await;
        let hits_while_marked = rig.anthropic_hits().await;
        router.lift_oauth_credential_parks([ANTHROPIC_SEAT]);
        rig.sight_then_pass_boot_spread(&job);
        sweep(&job, &router).await;

        // Assert
        assert_eq!(hits_while_marked, 0, "{cause:?}: not polled while marked");
        assert_eq!(
            rig.anthropic_hits().await,
            1,
            "{cause:?}: polled once lifted"
        );
    }
}

/// A plain-http backend that reads one request head, reports it, and then
/// never answers.
async fn hung_backend(seen: Arc<AtomicBool>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let seen = Arc::clone(&seen);
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(n) if n > 0 => head.extend_from_slice(&buf[..n]),
                        _ => return,
                    }
                }
                seen.store(true, Ordering::SeqCst);
                std::future::pending::<()>().await;
                drop(socket);
            });
        }
    });
    addr
}

#[tokio::test(start_paused = true)]
async fn shutdown_during_a_hung_response_returns_promptly_and_applies_nothing() {
    // Arrange: the Anthropic usage host accepts the request and never
    // answers. A blocking task holds the paused clock still, so only the
    // test moves it and the client's own timeout cannot end the hang.
    let mut rig = Rig::with_seats(&[(ANTHROPIC_SEAT, TOKEN_MARKER, unix_now() + 3600, None)]).await;
    let seen = Arc::new(AtomicBool::new(false));
    let front = TlsFront::start(ANTHROPIC_HOST, hung_backend(Arc::clone(&seen)).await).await;
    let job = rig.job_with_client(UsagePollClient::with_fronts_for_tests(&[(
        ANTHROPIC_HOST,
        &front,
    )]));
    let state = job.state();
    rig.sight_then_pass_boot_spread(&job);
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let hold = tokio::task::spawn_blocking(move || {
        let _ = release_rx.recv();
    });
    let health = Arc::new(SeatDriverHealth::default());
    let (shutdown_tx, shutdown_rx) = watch::channel(());
    let driver = tokio::spawn(run_seat_driver(
        job,
        rig.swap.clone(),
        shutdown_rx,
        Arc::clone(&health),
    ));
    tokio::task::yield_now().await;
    tokio::time::advance(SEAT_DRIVER_INTERVAL).await;
    let waited = std::time::Instant::now();
    while !seen.load(Ordering::SeqCst) {
        assert!(
            waited.elapsed() < Duration::from_secs(10),
            "the poll never reached the usage host"
        );
        tokio::task::yield_now().await;
    }

    // Act
    let stopping = std::time::Instant::now();
    shutdown_tx.send(()).expect("the driver listens");
    driver.await.expect("the driver task ends cleanly");
    let stopped_after = stopping.elapsed();
    release_tx.send(()).expect("release the clock hold");
    hold.await.expect("the hold ends");

    // Assert
    assert!(
        stopped_after < Duration::from_secs(1),
        "stopped after {stopped_after:?}"
    );
    let snapshot = state.snapshot(&rig.router().config);
    assert_eq!(snapshot.seats[0].last_attempt_ms, None, "no apply");
    assert_eq!(rig.reading(ANTHROPIC_SEAT).report, ReportState::NoReading);
    assert_eq!(rig.drain_writer(), 0, "writer queue unchanged");
    assert_eq!(rig.counters.account_readings_enqueued(), 0);
    assert_eq!(
        health.snapshot().sweeps,
        0,
        "the dropped sweep is not counted"
    );
    assert!(!health.snapshot().running);
}

#[tokio::test]
async fn the_snapshot_reports_each_seat_and_an_active_vendor_pause() {
    // Arrange: Anthropic answers 429 with a two-minute wait.
    let rig = Rig::new().await;
    rig.answer_anthropic(ResponseTemplate::new(429).insert_header("retry-after", "120"))
        .await;
    rig.answer_codex(ResponseTemplate::new(200).set_body_string(codex_body(30)))
        .await;
    let job = rig.job();
    let router = rig.router();
    rig.sight_then_pass_boot_spread(&job);
    let polled_at = rig.clock_ms();

    // Act
    sweep(&job, &router).await;
    let snapshot = job.state().snapshot(&router.config);
    rig.clock.set(polled_at + 3 * MINUTE_MS);
    let after_pause = job.state().snapshot(&router.config);

    // Assert
    let seats: Vec<_> = snapshot
        .seats
        .iter()
        .map(|seat| {
            (
                seat.account.as_str(),
                seat.vendor,
                seat.last_attempt_ms,
                seat.last_result,
            )
        })
        .collect();
    assert_eq!(
        seats,
        [
            (
                ANTHROPIC_SEAT,
                UsageVendor::Anthropic,
                Some(polled_at),
                Some("paused")
            ),
            (
                CODEX_SEAT,
                UsageVendor::Codex,
                Some(polled_at),
                Some("accepted")
            ),
        ]
    );
    assert!(snapshot.seats.iter().all(|seat| seat.next_due_ms.is_some()));
    assert_eq!(
        snapshot.paused_until_ms,
        [(UsageVendor::Anthropic, polled_at + 2 * MINUTE_MS)]
    );
    assert!(
        after_pause.paused_until_ms.is_empty(),
        "a past pause is not active"
    );
    assert_eq!(
        snapshot.applicability(ANTHROPIC_SEAT),
        PollApplicability::Polled(UsageVendor::Anthropic)
    );
    assert_eq!(
        snapshot.applicability("#some-api-key-entry"),
        PollApplicability::NotPolled
    );
}

#[test]
fn history_rows_carry_every_window_column_and_a_no_windows_row() {
    // Arrange
    let observed_at = SystemTime::UNIX_EPOCH + Duration::from_millis(1_800_000_000_123);
    let reset = SystemTime::UNIX_EPOCH + Duration::from_hours(500_001);
    let accepted = AcceptedReading {
        observed_at,
        windows: vec![
            ReadingWindow {
                length_secs: 18_000,
                class: LengthClass::Short,
                scope: None,
                used: 0.25,
                reset: Some(reset),
                started: Started::Yes,
            },
            ReadingWindow {
                length_secs: 604_800,
                class: LengthClass::Long,
                scope: Some("opus".into()),
                used: 0.0,
                reset: None,
                started: Started::No,
            },
        ],
    };
    let none = AcceptedReading {
        observed_at,
        windows: Vec::new(),
    };

    // Act
    let rows = history::rows(ANTHROPIC_SEAT, &accepted);
    let empty = history::rows(ANTHROPIC_SEAT, &none);

    // Assert
    let summary: Vec<_> = rows
        .iter()
        .map(|row| {
            let window = row.window.as_ref().expect("a window row");
            (
                row.ts_ms,
                row.account.as_str(),
                row.source,
                window.window_secs,
                window.scope.as_deref(),
                window.length_class,
                window.used,
                window.reset_ms,
                window.started,
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            (
                1_800_000_000_123,
                ANTHROPIC_SEAT,
                AccountReadingSource::Poll,
                18_000,
                None,
                WindowLengthClass::Short,
                0.25,
                Some(1_800_003_600_000),
                WindowStarted::Yes,
            ),
            (
                1_800_000_000_123,
                ANTHROPIC_SEAT,
                AccountReadingSource::Poll,
                604_800,
                Some("opus"),
                WindowLengthClass::Long,
                0.0,
                None,
                WindowStarted::No,
            ),
        ]
    );
    let [row] = empty.as_slice() else {
        panic!("one no_windows row, got {empty:?}");
    };
    assert_eq!(row.window, None);
    assert_eq!(row.source, AccountReadingSource::Poll);
    assert_eq!(row.ts_ms, 1_800_000_000_123);
}

#[tokio::test]
async fn the_production_job_runs_on_the_system_wall_clock() {
    // Arrange
    let rig = Rig::new().await;

    // Act
    let job = UsagePollJob::new(Arc::clone(&rig.store), rig.usage_handle())
        .expect("the production job builds");
    let read = job.state().clock.now_ms();

    // Assert
    assert!(
        (read - super::rig::now_ms()).abs() < 5_000,
        "the production clock read {read}"
    );
}
