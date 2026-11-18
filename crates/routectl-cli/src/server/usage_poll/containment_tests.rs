//! Containment: whatever a usage endpoint answers, a poll changes only the
//! poll schedule, the account meter (accepted bodies only), the history
//! queue (accepted readings only) and their counters. One row per failure
//! kind compares the credentials file bytes, every target's eligibility,
//! breaker phase, park, unusable mark and credential generation, the pool
//! counts, and the account's accepted reading and placement-safe view,
//! before and after, on a clean seat and on one already parked and marked
//! (so a poll that made a seat more eligible would show). Every row also
//! runs under a tracing capture that must hold no token, account id, body
//! or header marker; the Codex rows send a real account id to make that
//! check bite.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use routectl_core::UpstreamReason;
use routectl_router::runtime_state::UnusableCause;
use routectl_router::{ObservationStamp, RejectionTotals, Router};
use routectl_testkit::tls_front::TlsFront;
use routectl_testkit::with_capture;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;
use wiremock::ResponseTemplate;

use super::client::UsagePollClient;
use super::job_tests::{ReloginDuringRun, ReloginStep, anthropic_body, rfc3339_in};
use super::rig::{
    ACCOUNT_ID_MARKER, ANTHROPIC_SEAT, BODY_MARKER, CODEX_HOST, CODEX_SEAT, Containment,
    HEADER_MARKER, ReadingView, Rig, TOKEN_MARKER, WRITER_CAPACITY, leaked, state_key_of, unix_now,
};
use crate::server::seat_driver::sweep;

const MINUTE_MS: i64 = 60_000;
/// Far past any backoff the baseline poll could leave behind.
const NEXT_POLL_GAP_MS: i64 = 20 * MINUTE_MS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Timeout,
    Redirect,
    Unauthorized,
    Forbidden,
    RateLimited,
    ServerError,
    Oversize,
    Malformed,
    Contradiction,
    Superseded,
    HistoryChannelFull,
}

const KINDS: [Kind; 11] = [
    Kind::Timeout,
    Kind::Redirect,
    Kind::Unauthorized,
    Kind::Forbidden,
    Kind::RateLimited,
    Kind::ServerError,
    Kind::Oversize,
    Kind::Malformed,
    Kind::Contradiction,
    Kind::Superseded,
    Kind::HistoryChannelFull,
];

/// The counters a row may move, read together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Counters {
    rejections: RejectionTotals,
    history_enqueued: u64,
    history_dropped_full: u64,
}

impl Counters {
    fn read(rig: &Rig) -> Self {
        Self {
            rejections: rig
                .router()
                .account_readings(&ObservationStamp::now())
                .rejections,
            history_enqueued: rig.counters.account_readings_enqueued(),
            history_dropped_full: rig.counters.account_readings_dropped_full(),
        }
    }
}

struct Expected {
    result: &'static str,
    counters: Counters,
    /// Whether the meter takes the polled reading (the history channel
    /// refusing it does not undo the meter).
    reading_changes: bool,
}

/// The reset every body in a row shares, so a lower reading is a
/// contradiction rather than a new window.
fn shared_reset() -> String {
    rfc3339_in(Duration::from_hours(2))
}

/// A valid body at `percent` that also carries the body marker in a member
/// the reducer ignores.
fn marked_body(percent: u32, reset: &str) -> String {
    let body = anthropic_body(percent, reset);
    let trimmed = body.strip_suffix('}').expect("an object");
    format!(r#"{trimmed}, "note": "{BODY_MARKER}"}}"#)
}

fn marked(response: ResponseTemplate) -> ResponseTemplate {
    response.insert_header("x-poll-marker", HEADER_MARKER)
}

async fn answer_for(rig: &Rig, kind: Kind, reset: &str) {
    let response = match kind {
        Kind::Timeout => ResponseTemplate::new(200)
            .set_body_string(marked_body(30, reset))
            .set_delay(Duration::from_secs(12)),
        Kind::Redirect => ResponseTemplate::new(302)
            .insert_header("location", "https://chatgpt.com/backend-api/wham/usage"),
        Kind::Unauthorized => ResponseTemplate::new(401).set_body_string(BODY_MARKER),
        Kind::Forbidden => ResponseTemplate::new(403).set_body_string(BODY_MARKER),
        Kind::RateLimited => ResponseTemplate::new(429)
            .insert_header("retry-after", "120")
            .set_body_string(BODY_MARKER),
        Kind::ServerError => ResponseTemplate::new(500).set_body_string(BODY_MARKER),
        Kind::Oversize => {
            let mut body = BODY_MARKER.as_bytes().to_vec();
            body.resize(routectl_router::MAX_USAGE_BODY_BYTES + 1, b' ');
            ResponseTemplate::new(200).set_body_bytes(body)
        }
        Kind::Malformed => ResponseTemplate::new(200).set_body_string(format!(
            r#"{{"five_hour": "{BODY_MARKER}", "seven_day": null}}"#
        )),
        Kind::Contradiction => ResponseTemplate::new(200).set_body_string(marked_body(10, reset)),
        Kind::Superseded | Kind::HistoryChannelFull => {
            ResponseTemplate::new(200).set_body_string(marked_body(30, reset))
        }
    };
    rig.answer_anthropic(marked(response)).await;
}

fn expected(kind: Kind, before: Counters) -> Expected {
    let mut counters = before;
    let (result, reading_changes) = match kind {
        Kind::Timeout => ("timeout", false),
        Kind::Redirect => ("redirect", false),
        Kind::Unauthorized | Kind::Forbidden => ("auth", false),
        Kind::RateLimited => ("paused", false),
        Kind::ServerError => ("http", false),
        Kind::Oversize => ("too_large", false),
        Kind::Malformed => {
            counters.rejections.malformed_body += 1;
            ("malformed", false)
        }
        Kind::Contradiction => {
            counters.rejections.contradictory += 1;
            ("malformed", false)
        }
        Kind::Superseded => ("superseded", false),
        Kind::HistoryChannelFull => {
            counters.history_dropped_full += 1;
            ("accepted", true)
        }
    };
    Expected {
        result,
        counters,
        reading_changes,
    }
}

fn last_result(rig: &Rig, job: &super::UsagePollJob, account: &str) -> Option<&'static str> {
    job.state()
        .snapshot(&rig.router().config, &std::collections::BTreeSet::new())
        .seats
        .into_iter()
        .find(|seat| seat.account == account)
        .and_then(|seat| seat.last_result)
}

fn fill_writer_channel(rig: &Rig) {
    let filler = routectl_usage::AccountReadingRow {
        ts_ms: 1,
        account: "filler".into(),
        source: routectl_usage::AccountReadingSource::Poll,
        window: None,
    };
    let usage = rig.usage_handle();
    for _ in 0..WRITER_CAPACITY {
        usage.try_send_account_readings(vec![filler.clone()]);
    }
}

/// Polls once; the Superseded row reloads the store while the poll is in
/// flight, before the router hears of it. The reload leaves the router
/// alone, so everything the table compares stays put.
async fn poll_once(rig: &Rig, job: &super::UsagePollJob, router: &Router, kind: Kind) {
    if kind == Kind::Superseded {
        let relogin = ReloginDuringRun {
            inner: job,
            account: ANTHROPIC_SEAT,
            rig_store: Arc::clone(&rig.store),
            creds: rig.creds.clone(),
            router: rig.router(),
            step: ReloginStep::StoreOnly { new_login: false },
        };
        sweep(&relogin, router).await;
    } else {
        sweep(job, router).await;
    }
}

/// The seat's credential state when the failing poll runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Baseline {
    Clean,
    /// Parked, and marked unusable for a cause the poll still runs under.
    ParkedAndMarked,
}

fn park_and_mark(router: &Router) {
    let state_key = state_key_of(router, ANTHROPIC_SEAT);
    assert!(router.park_credential_for_tests(
        &state_key,
        UpstreamReason::CredentialExpired,
        Instant::now() + Duration::from_hours(1),
    ));
    assert!(router.mark_credential_unusable_for_tests(&state_key, UnusableCause::Expired));
}

#[tokio::test]
async fn a_failing_poll_of_any_kind_changes_only_poll_state_and_its_counter() {
    for kind in KINDS {
        assert_row_contained(kind, Baseline::Clean).await;
    }
}

#[tokio::test]
async fn a_failing_poll_of_any_kind_neither_lifts_a_park_nor_clears_a_mark() {
    for kind in KINDS {
        assert_row_contained(kind, Baseline::ParkedAndMarked).await;
    }
}

async fn assert_row_contained(kind: Kind, baseline: Baseline) {
    // Arrange: one accepted poll gives the seat a reading to protect.
    let mut rig = Rig::with_seats(&[(ANTHROPIC_SEAT, TOKEN_MARKER, unix_now() + 3600, None)]).await;
    let reset = shared_reset();
    rig.answer_anthropic(ResponseTemplate::new(200).set_body_string(marked_body(25, &reset)))
        .await;
    let job = rig.job();
    let router = rig.router();
    rig.sight_then_pass_boot_spread(&job);
    let (_, baseline_events) = with_capture(sweep(&job, &router)).await;
    assert_eq!(
        last_result(&rig, &job, ANTHROPIC_SEAT),
        Some("accepted"),
        "{kind:?}: premise"
    );
    if baseline == Baseline::ParkedAndMarked {
        park_and_mark(&router);
    }
    rig.drain_writer();
    answer_for(&rig, kind, &reset).await;
    if kind == Kind::HistoryChannelFull {
        fill_writer_channel(&rig);
    }
    rig.clock.set(rig.clock_ms() + NEXT_POLL_GAP_MS);
    let contained = Containment::read(&rig);
    let reading = ReadingView::of(rig.reading(ANTHROPIC_SEAT));
    let counters = Counters::read(&rig);
    let want = expected(kind, counters);

    // Act
    let ((), events) = with_capture(poll_once(&rig, &job, &router, kind)).await;

    // Assert
    assert_eq!(rig.anthropic_hits().await, 1, "{kind:?}: premise, polled");
    assert_eq!(
        rig.codex_hits().await,
        0,
        "{kind:?}: no request reached another host"
    );
    assert_eq!(
        last_result(&rig, &job, ANTHROPIC_SEAT),
        Some(want.result),
        "{kind:?} {baseline:?}"
    );
    assert_eq!(Containment::read(&rig), contained, "{kind:?} {baseline:?}");
    let after = ReadingView::of(rig.reading(ANTHROPIC_SEAT));
    if want.reading_changes {
        assert_ne!(after.last_accepted, reading.last_accepted, "{kind:?}");
    } else {
        assert_eq!(after, reading, "{kind:?}");
    }
    assert_eq!(Counters::read(&rig), want.counters, "{kind:?}");
    let leaks: Vec<String> = leaked(&baseline_events)
        .into_iter()
        .chain(leaked(&events))
        .collect();
    assert!(
        leaks.is_empty(),
        "{kind:?}: log carried a marker: {leaks:?}"
    );
    assert!(
        events
            .iter()
            .any(|event| event.field("event") == Some("usage_poll")),
        "{kind:?}: premise, the capture saw the poll"
    );
}

/// The failures a Codex poll can meet on the way to the endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CodexKind {
    Unauthorized,
    RateLimited,
    Redirect,
    Timeout,
    Transport,
}

const CODEX_KINDS: [(CodexKind, &str); 5] = [
    (CodexKind::Unauthorized, "auth"),
    (CodexKind::RateLimited, "paused"),
    (CodexKind::Redirect, "redirect"),
    (CodexKind::Timeout, "timeout"),
    (CodexKind::Transport, "transport"),
];

/// A plain-http backend that reads each request head, counts it, and
/// closes the connection without answering.
async fn closing_backend(hits: Arc<AtomicUsize>) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let hits = Arc::clone(&hits);
            tokio::spawn(async move {
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match socket.read(&mut buf).await {
                        Ok(n) if n > 0 => head.extend_from_slice(&buf[..n]),
                        _ => return,
                    }
                }
                hits.fetch_add(1, Ordering::SeqCst);
                drop(socket);
            });
        }
    });
    addr
}

/// The Codex stub's answer; `None` for Transport, whose request goes to a
/// backend that closes the connection instead.
fn codex_answer(kind: CodexKind) -> Option<ResponseTemplate> {
    let response = match kind {
        CodexKind::Unauthorized => ResponseTemplate::new(401).set_body_string(BODY_MARKER),
        CodexKind::RateLimited => ResponseTemplate::new(429)
            .insert_header("retry-after", "120")
            .set_body_string(BODY_MARKER),
        CodexKind::Redirect => ResponseTemplate::new(302)
            .insert_header("location", "https://api.anthropic.com/api/oauth/usage"),
        CodexKind::Timeout => ResponseTemplate::new(200)
            .set_body_string(BODY_MARKER)
            .set_delay(Duration::from_secs(12)),
        CodexKind::Transport => return None,
    };
    Some(marked(response))
}

#[tokio::test]
async fn a_failing_codex_poll_logs_no_account_id_and_changes_nothing_contained() {
    for (kind, result) in CODEX_KINDS {
        // Arrange: a Codex seat with a recorded account id.
        let rig = Rig::with_seats(&[(
            CODEX_SEAT,
            TOKEN_MARKER,
            unix_now() + 3600,
            Some(ACCOUNT_ID_MARKER),
        )])
        .await;
        if let Some(answer) = codex_answer(kind) {
            rig.answer_codex(answer).await;
        }
        let closed_hits = Arc::new(AtomicUsize::new(0));
        let closing_front =
            TlsFront::start(CODEX_HOST, closing_backend(Arc::clone(&closed_hits)).await).await;
        let job = if kind == CodexKind::Transport {
            rig.job_with_client(UsagePollClient::with_fronts_for_tests(&[(
                CODEX_HOST,
                &closing_front,
            )]))
        } else {
            rig.job()
        };
        let router = rig.router();
        rig.sight_then_pass_boot_spread(&job);
        let contained = Containment::read(&rig);

        // Act
        let ((), events) = with_capture(async {
            sweep(&job, &router).await;
        })
        .await;

        // Assert
        let hits = rig.codex_hits().await + closed_hits.load(Ordering::SeqCst);
        assert_eq!(hits, 1, "{kind:?}: premise, the Codex endpoint was reached");
        assert_eq!(rig.anthropic_hits().await, 0, "{kind:?}");
        assert_eq!(
            last_result(&rig, &job, CODEX_SEAT),
            Some(result),
            "{kind:?}"
        );
        assert_eq!(Containment::read(&rig), contained, "{kind:?}");
        let leaks = leaked(&events);
        assert!(
            leaks.is_empty(),
            "{kind:?}: log carried a marker: {leaks:?}"
        );
        assert!(
            events
                .iter()
                .any(|event| event.field("event") == Some("usage_poll")),
            "{kind:?}: premise, the capture saw the poll"
        );
    }
}
