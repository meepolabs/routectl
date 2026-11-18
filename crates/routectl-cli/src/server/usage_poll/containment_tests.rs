//! Containment: whatever a usage endpoint answers, a poll changes only the
//! poll schedule, the account meter (accepted bodies only), the history
//! queue (accepted readings only) and their counters. One row per failure
//! kind compares the credentials file bytes, every target's eligibility,
//! breaker phase, park and unusable mark, the pool counts, and the
//! account's accepted reading and placement-safe view, before and after.
//! Every row also runs under a tracing capture that must hold no token,
//! account id, body or header marker.

use std::time::Duration;

use routectl_router::{ObservationStamp, RejectionTotals, Router};
use routectl_testkit::with_capture;
use wiremock::ResponseTemplate;

use super::job_tests::{ReloginDuringRun, anthropic_body, rfc3339_in};
use super::rig::{
    ANTHROPIC_SEAT, BODY_MARKER, Containment, HEADER_MARKER, ReadingView, Rig, TOKEN_MARKER,
    WRITER_CAPACITY, leaked, unix_now,
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

fn last_result(rig: &Rig, job: &super::UsagePollJob) -> Option<&'static str> {
    job.state()
        .snapshot(&rig.router().config)
        .seats
        .into_iter()
        .find(|seat| seat.account == ANTHROPIC_SEAT)
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

async fn poll_once(rig: &Rig, job: &super::UsagePollJob, router: &Router, kind: Kind) {
    if kind == Kind::Superseded {
        let relogin = ReloginDuringRun {
            inner: job,
            account: ANTHROPIC_SEAT,
            rig_store: std::sync::Arc::clone(&rig.store),
            creds: rig.creds.clone(),
            router: rig.router(),
            rewrite_file: false,
        };
        sweep(&relogin, router).await;
    } else {
        sweep(job, router).await;
    }
}

#[tokio::test]
async fn a_failing_poll_of_any_kind_changes_only_poll_state_and_its_counter() {
    for kind in KINDS {
        // Arrange: one accepted poll gives the seat a reading to protect.
        let mut rig =
            Rig::with_seats(&[(ANTHROPIC_SEAT, TOKEN_MARKER, unix_now() + 3600, None)]).await;
        let reset = shared_reset();
        rig.answer_anthropic(ResponseTemplate::new(200).set_body_string(marked_body(25, &reset)))
            .await;
        let job = rig.job();
        let router = rig.router();
        rig.sight_then_pass_boot_spread(&job);
        let (_, baseline_events) = with_capture(sweep(&job, &router)).await;
        assert_eq!(
            last_result(&rig, &job),
            Some("accepted"),
            "{kind:?}: premise"
        );
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
        assert_eq!(last_result(&rig, &job), Some(want.result), "{kind:?}");
        assert_eq!(Containment::read(&rig), contained, "{kind:?}");
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
}
