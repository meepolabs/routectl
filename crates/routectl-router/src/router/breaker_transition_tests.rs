//! Each breaker state transition logs exactly one INFO line carrying the
//! event token and the sanitized state key, and nothing else; a failure or
//! success that leaves the circuit where it was logs no transition.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use parking_lot::Mutex;
use routectl_core::failure_class::LastOutcome;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result};
use routectl_testkit::{CapturedEvent, with_capture};

use super::super::Router;
use super::{BREAKER_CLOSED, BREAKER_OPENED, BREAKER_PARKED};
use crate::config::{Config, ProviderEntry, ProviderRuntimePolicy, RetryPolicy};
use crate::resolved::ResolvedModel;

const TARGET: &str = "routectl_router::router::breaker_transition";
const MODEL: &str = "m-test";
const MAX_HONORED_RETRY_AFTER_MS: u64 = 90_000;

#[derive(Clone, Copy, Debug)]
enum Reply {
    Ok,
    Status(u16, Option<Duration>),
}

struct ScriptedProvider {
    script: Mutex<VecDeque<Reply>>,
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn id(&self) -> &'static str {
        "p"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        let reply = self.script.lock().pop_front().unwrap_or(Reply::Ok);
        match reply {
            Reply::Ok => Ok(ChatResponse {
                model: req.model,
                ..Default::default()
            }),
            Reply::Status(status, hint) => Err(Error::upstream_with_retry_after(
                "p",
                status,
                "upstream failure",
                hint,
            )),
        }
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("not exercised by these tests")
    }
}

fn router(nickname: &str, script: &[Reply], threshold: u32, cooldown_ms: u64) -> Router {
    let retry = RetryPolicy {
        max_attempts: 1,
        initial_backoff_ms: 0,
        backoff_multiplier: 1.0,
        jitter_ms: 0,
        max_honored_retry_after_ms: Some(MAX_HONORED_RETRY_AFTER_MS),
        ..RetryPolicy::default()
    };
    let mut config = Config {
        retry,
        ..Config::default()
    };
    config.providers.insert(
        "p".into(),
        ProviderEntry::anthropic_api("literal:k").with_runtime(ProviderRuntimePolicy {
            circuit_failures: Some(threshold),
            circuit_cooldown_ms: Some(cooldown_ms),
            ..Default::default()
        }),
    );
    let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider {
        script: Mutex::new(script.iter().copied().collect()),
    });
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        nickname.into(),
        Arc::new(ResolvedModel::new(nickname, "p", provider, "u")),
    );
    router.install_resolved_models(models);
    router
}

fn request(nickname: &str) -> ChatRequest {
    ChatRequest {
        model: nickname.into(),
        max_tokens: Some(1024),
        ..Default::default()
    }
}

fn transitions(events: Vec<CapturedEvent>) -> Vec<CapturedEvent> {
    events.into_iter().filter(|e| e.target == TARGET).collect()
}

fn fields(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect()
}

/// One INFO line per transition, with the token and the key as its only
/// fields.
fn assert_transition(event: &CapturedEvent, token: &str, label: &str) {
    assert_eq!(event.level, tracing::Level::INFO, "{label}: level");
    assert_eq!(
        event.fields,
        fields(&[("event", token), ("state_key", MODEL)]),
        "{label}: fields"
    );
}

#[tokio::test]
async fn a_trip_and_its_half_open_close_each_log_one_info_line() {
    // Arrange: threshold 2 and a zero cooldown, so the request after the
    // trip is the half-open probe.
    let script = [
        Reply::Ok,
        Reply::Status(500, None),
        Reply::Status(500, None),
        Reply::Ok,
    ];
    let router = router(MODEL, &script, 2, 0);
    let steps = [
        ("success on a closed circuit", 0),
        ("first debit below threshold", 0),
        ("debit reaching threshold", 1),
        ("half-open probe success", 1),
    ];

    // Act
    let mut all = Vec::new();
    for (label, want) in steps {
        let (_, events) = with_capture(Box::pin(router.complete(request(MODEL)))).await;
        let got = transitions(events);
        assert_eq!(got.len(), want, "{label}: {got:?}");
        all.extend(got);
    }

    // Assert
    assert_eq!(all.len(), 2, "{all:?}");
    assert_transition(&all[0], BREAKER_OPENED, "trip");
    assert_transition(&all[1], BREAKER_CLOSED, "close");
}

#[tokio::test]
async fn a_long_retry_after_parks_with_the_clamped_cooldown() {
    // Arrange: a threshold no single failure reaches, so only the park can
    // open the circuit.
    let script = [Reply::Status(429, Some(Duration::from_hours(1)))];
    let router = router(MODEL, &script, 5, 60_000);

    // Act
    let (_, events) = with_capture(Box::pin(router.complete(request(MODEL)))).await;

    // Assert
    let got = transitions(events);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].level, tracing::Level::INFO);
    assert_eq!(
        got[0].fields,
        fields(&[
            ("event", BREAKER_PARKED),
            ("state_key", MODEL),
            ("cooldown_ms", &MAX_HONORED_RETRY_AFTER_MS.to_string()),
        ])
    );
}

#[derive(Clone, Copy, Debug)]
enum Step {
    Debit,
    ClaimProbe,
}

#[tokio::test]
async fn only_a_debit_that_moves_the_circuit_to_open_logs_opened() {
    // A zero cooldown makes an open circuit half-open-ready at once; a
    // minute keeps it open for the whole row.
    let rows: [(&str, u64, &[Step], usize); 3] = [
        (
            "debits past the threshold on an open circuit",
            60_000,
            &[Step::Debit, Step::Debit, Step::Debit, Step::Debit],
            1,
        ),
        (
            "failed half-open probe re-trips",
            0,
            &[Step::Debit, Step::Debit, Step::ClaimProbe, Step::Debit],
            2,
        ),
        (
            "debit on a lapsed open circuit without a probe claim",
            0,
            &[Step::Debit, Step::Debit, Step::Debit],
            1,
        ),
    ];
    for (label, cooldown_ms, steps, want) in rows {
        // Arrange
        let router = router(MODEL, &[], 2, cooldown_ms);

        // Act
        let (tripped, events) = with_capture(async {
            let mut tripped = Vec::new();
            for step in steps {
                match step {
                    Step::Debit => {
                        tripped.push(router.record_failure_opened(MODEL, LastOutcome::Http5xx));
                    }
                    Step::ClaimProbe => {
                        let (refusal, mut guard) = router.admit_dispatch(MODEL, "p");
                        assert!(refusal.is_none(), "{label}: premise: probe admitted");
                        guard.disarm();
                    }
                }
            }
            tripped
        })
        .await;

        // Assert
        let got = transitions(events);
        assert_eq!(got.len(), want, "{label}: {got:?} (tripped {tripped:?})");
        for event in &got {
            assert_transition(event, BREAKER_OPENED, label);
        }
    }
}

#[tokio::test]
async fn the_state_key_is_sanitized() {
    // Arrange
    let nickname = "m\u{1b}[31m";
    let router = router(nickname, &[], 1, 60_000);

    // Act
    let ((), events) = with_capture(async {
        router.record_failure_opened(nickname, LastOutcome::Http5xx);
    })
    .await;

    // Assert
    let got = transitions(events);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!(got[0].field("state_key"), Some("m?[31m"));
}
