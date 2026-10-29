//! Breaker accounting on a committed stream's tail: each of the three exits
//! (natural end, first error frame, a drop after content) settles the
//! breaker exactly once, on a closed breaker and on a half-open probe that
//! first content already closed.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{self, StreamExt};
use parking_lot::Mutex;
use routectl_core::failure_class::LastOutcome;
use routectl_core::{ChatChunk, ChunkChoice, ChunkDelta, Error, Result};

use super::stream_commit::wrap_with_breaker_accounting;
use crate::config::ProviderRuntimePolicy;
use crate::quota::feed::FirstChunkFeed;
use crate::quota::store::QuotaStore;
use crate::runtime_state::{GateDecision, ProviderState};

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

async fn run_exit(state: &Arc<Mutex<ProviderState>>, breaker: Breaker, exit: Exit) {
    let mut wrapped = wrap_with_breaker_accounting(
        stream::iter(tail(exit)).boxed(),
        Some(state.clone()),
        None,
        FirstChunkFeed::armed(Arc::new(QuotaStore::default()), None),
        matches!(breaker, Breaker::HalfOpenProbe),
        BTreeMap::new(),
        Duration::from_mins(1),
    );
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
