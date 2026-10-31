//! Breaker accounting on a committed stream's tail: each of the three exits
//! (natural end, first error frame, a drop after content) settles the
//! breaker exactly once, on a closed breaker and on a half-open probe that
//! first content already closed; and an error frame settles by its class
//! after the target's remap, as a failure before content does.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{self, StreamExt};
use parking_lot::Mutex;
use routectl_core::failure_class::{FailureClass, LastOutcome};
use routectl_core::{ChatChunk, ChunkChoice, ChunkDelta, Error, Result};

use super::stream_commit::wrap_with_breaker_accounting;
use crate::config::ProviderRuntimePolicy;
use crate::quota::feed::FirstChunkFeed;
use crate::quota::store::QuotaStore;
use crate::runtime_state::{CircuitPhase, GateDecision, ProviderState};

/// High enough that no row's debits trip the breaker, so every row reads a
/// closed breaker and the failure count alone tells the exits apart.
const THRESHOLD: u32 = 3;

/// The outcome seeded before the stream runs. Distinct from the 503 frame's
/// `Http5xx`, so a row that settles nothing is told apart from a debit.
const SEEDED: LastOutcome = LastOutcome::Timeout;

#[derive(Debug, Clone, Copy)]
enum Breaker {
    Closed,
    HalfOpenProbe,
}

#[derive(Debug, Clone, Copy)]
enum Exit {
    EndOfStream,
    FrameError,
    Cancel,
}

fn content(text: &str) -> ChatChunk {
    ChatChunk {
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some(text.into()),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
}

/// A breaker holding one failure and the `SEEDED` outcome, in the state the
/// wrap finds it in. For a half-open probe that is after first content
/// closed it and freed the slot, plus one failure a concurrent request on the
/// same provider recorded while the probe's stream was still open.
fn seeded_state(breaker: Breaker) -> Arc<Mutex<ProviderState>> {
    let policy = ProviderRuntimePolicy {
        circuit_failures: Some(THRESHOLD),
        circuit_cooldown_ms: Some(60_000),
        ..Default::default()
    };
    let mut state = ProviderState::new(&policy);
    let now = Instant::now();
    if matches!(breaker, Breaker::HalfOpenProbe) {
        state.force_open(now, Duration::ZERO);
        assert_eq!(state.try_dispatch(now), GateDecision::Allow);
        assert!(state.half_open_probe_in_flight(), "premise: probe claimed");
        state.record_success(now);
    }
    state.record_failure(now, SEEDED);
    assert!(
        !state.half_open_probe_in_flight(),
        "premise: the wrap never holds the probe slot"
    );
    assert_eq!(state.consecutive_failures_for_tests(), 1, "premise: seeded");
    Arc::new(Mutex::new(state))
}

fn tail(exit: Exit) -> Vec<Result<ChatChunk>> {
    match exit {
        Exit::EndOfStream | Exit::Cancel => vec![Ok(content("a")), Ok(content("b"))],
        Exit::FrameError => vec![
            Ok(content("a")),
            Err(Error::upstream("p", 503, "mid-stream boom")),
            Ok(content("b")),
        ],
    }
}

/// The Retry-After ceiling every row runs under: well above the in-loop cap,
/// and distinct from the seeded breaker's own cooldown so a park's length
/// tells the clamped hint apart from that cooldown.
const MAX_HONORED_RETRY_AFTER: Duration = Duration::from_secs(90);

fn wrap(
    state: &Arc<Mutex<ProviderState>>,
    breaker: Breaker,
    items: Vec<Result<ChatChunk>>,
    class_overrides: BTreeMap<u16, FailureClass>,
) -> futures::stream::BoxStream<'static, Result<ChatChunk>> {
    wrap_with_breaker_accounting(
        stream::iter(items).boxed(),
        Some(state.clone()),
        None,
        FirstChunkFeed::armed(Arc::new(QuotaStore::default()), None),
        matches!(breaker, Breaker::HalfOpenProbe),
        class_overrides,
        MAX_HONORED_RETRY_AFTER,
    )
}

async fn run_exit(state: &Arc<Mutex<ProviderState>>, breaker: Breaker, exit: Exit) {
    let mut wrapped = wrap(state, breaker, tail(exit), BTreeMap::new());
    match exit {
        Exit::EndOfStream | Exit::FrameError => while wrapped.next().await.is_some() {},
        Exit::Cancel => {
            let first = wrapped.next().await;
            assert!(matches!(first, Some(Ok(_))), "premise: content delivered");
        }
    }
    drop(wrapped);
}

#[tokio::test]
async fn each_tail_exit_settles_the_breaker_once_by_its_own_rule() {
    let rows = [
        (
            "closed / end of stream",
            Breaker::Closed,
            Exit::EndOfStream,
            0,
            LastOutcome::Ok,
        ),
        (
            "closed / frame error",
            Breaker::Closed,
            Exit::FrameError,
            2,
            LastOutcome::Http5xx,
        ),
        ("closed / cancel", Breaker::Closed, Exit::Cancel, 1, SEEDED),
        (
            "probe / end of stream",
            Breaker::HalfOpenProbe,
            Exit::EndOfStream,
            0,
            LastOutcome::Ok,
        ),
        (
            "probe / frame error",
            Breaker::HalfOpenProbe,
            Exit::FrameError,
            2,
            LastOutcome::Http5xx,
        ),
        (
            "probe / cancel",
            Breaker::HalfOpenProbe,
            Exit::Cancel,
            1,
            SEEDED,
        ),
    ];
    let mut mismatches = Vec::new();
    for (name, breaker, exit, failures, last) in rows {
        let state = seeded_state(breaker);

        run_exit(&state, breaker, exit).await;

        let st = state.lock();
        let got = (
            st.consecutive_failures_for_tests(),
            st.gate_status(Instant::now()).last_outcome,
        );
        if got != (failures, Some(last)) {
            mismatches.push(format!("{name}: want ({failures}, {last:?}), got {got:?}"));
        }
    }
    assert!(
        mismatches.is_empty(),
        "rows settled wrongly: {mismatches:#?}"
    );
}

struct FrameRow {
    name: &'static str,
    breaker: Breaker,
    frame: Error,
    class_overrides: BTreeMap<u16, FailureClass>,
    failures: u32,
    last: LastOutcome,
    circuit: CircuitPhase,
    /// How long the frame parks the slot, when it parks.
    parked_for: Option<Duration>,
}

fn frame_row(name: &'static str, breaker: Breaker, frame: Error) -> FrameRow {
    FrameRow {
        name,
        breaker,
        frame,
        class_overrides: BTreeMap::new(),
        failures: 1,
        last: SEEDED,
        circuit: CircuitPhase::Closed,
        parked_for: None,
    }
}

fn rate_limited_for_an_hour() -> Error {
    Error::upstream_with_retry_after("p", 429, "slow down", Some(Duration::from_hours(1)))
}

/// The slot is refused just before `park` elapses from the earliest moment
/// the park could have started, and admits a half-open probe just after it
/// elapses from the latest. Claims the probe slot when it admits.
fn park_window_mismatch(
    state: &mut ProviderState,
    name: &str,
    before: Instant,
    after: Instant,
    park: Duration,
) -> Option<String> {
    let margin = Duration::from_secs(1);
    let got = (
        state.try_dispatch(before + park.saturating_sub(margin)),
        state.try_dispatch(after + park + margin),
    );
    let want = (GateDecision::CircuitOpen, GateDecision::Allow);
    (got != want).then(|| format!("{name}: park window want {want:?}, got {got:?}"))
}

#[tokio::test]
async fn committed_frame_settles_by_class() {
    let rows = [
        FrameRow {
            failures: 2,
            last: LastOutcome::Http5xx,
            ..frame_row(
                "closed / 503 debits",
                Breaker::Closed,
                Error::upstream("p", 503, "boom"),
            )
        },
        frame_row(
            "closed / 401 records nothing",
            Breaker::Closed,
            Error::upstream("p", 401, "denied"),
        ),
        frame_row(
            "closed / 400 records nothing",
            Breaker::Closed,
            Error::upstream("p", 400, "bad"),
        ),
        FrameRow {
            failures: 2,
            last: LastOutcome::TransportError,
            ..frame_row(
                "closed / status-less streaming error debits",
                Breaker::Closed,
                Error::Streaming("connection reset".into()),
            )
        },
        FrameRow {
            failures: 2,
            last: LastOutcome::Http5xx,
            ..frame_row(
                "closed / 529 debits",
                Breaker::Closed,
                Error::upstream("p", 529, "overloaded"),
            )
        },
        FrameRow {
            class_overrides: BTreeMap::from([(503, FailureClass::BadRequest)]),
            ..frame_row(
                "closed / 503 remapped to bad-request records nothing",
                Breaker::Closed,
                Error::upstream("p", 503, "boom"),
            )
        },
        FrameRow {
            circuit: CircuitPhase::Open,
            parked_for: Some(MAX_HONORED_RETRY_AFTER),
            ..frame_row(
                "closed / 429 with a long Retry-After parks for the ceiling",
                Breaker::Closed,
                rate_limited_for_an_hour(),
            )
        },
        frame_row(
            "probe / 429 with a long Retry-After records nothing",
            Breaker::HalfOpenProbe,
            rate_limited_for_an_hour(),
        ),
    ];
    let mut mismatches = Vec::new();
    for row in rows {
        let state = seeded_state(row.breaker);
        let items = vec![Ok(content("a")), Err(row.frame), Ok(content("b"))];

        let before = Instant::now();
        let mut wrapped = wrap(&state, row.breaker, items, row.class_overrides);
        while wrapped.next().await.is_some() {}
        drop(wrapped);
        let after = Instant::now();

        let mut st = state.lock();
        let status = st.gate_status(Instant::now());
        let got = (
            st.consecutive_failures_for_tests(),
            status.last_outcome,
            status.circuit,
        );
        let want = (row.failures, Some(row.last), row.circuit);
        if got != want {
            mismatches.push(format!("{}: want {want:?}, got {got:?}", row.name));
        }
        if let Some(park) = row.parked_for {
            mismatches.extend(park_window_mismatch(&mut st, row.name, before, after, park));
        }
    }
    assert!(
        mismatches.is_empty(),
        "rows settled wrongly: {mismatches:#?}"
    );
}
