//! A status's failure class comes from the built-in table only:
//! same-provider retry, fallback and the committed-stream breaker settle by
//! the built-in class, and a `[retry.classes]` count moves only retry.
//!
//! Each path carries a positive control showing the fixture can see the
//! decision move: a `[retry.classes]` count for retry, a status whose
//! built-in class is unknown for fallback, and a status whose built-in class
//! does not debit for the breaker (no `[retry.classes]` count reaches
//! fallback or breaker debit).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use futures::StreamExt as _;
use futures::stream::BoxStream;
use routectl_core::failure_class::LastOutcome;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result};
use routectl_router::runtime_state::CircuitPhase;
use routectl_router::{Config, ResolvedModel, Router};

/// A seat that fails every attempt with `status` (after one content chunk
/// on a stream), or serves when `status` is `None`. Counts calls.
struct Seat {
    id: &'static str,
    status: Option<u16>,
    calls: AtomicUsize,
}

impl Seat {
    fn new(id: &'static str, status: Option<u16>) -> Arc<Self> {
        Arc::new(Self {
            id,
            status,
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

fn content_chunk() -> ChatChunk {
    ChatChunk {
        choices: vec![routectl_core::ChunkChoice {
            index: 0,
            delta: routectl_core::ChunkDelta {
                content: Some("ok".into()),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
}

#[async_trait::async_trait]
impl Provider for Seat {
    fn id(&self) -> &str {
        self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(self.id, "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.status {
            Some(status) => Err(Error::upstream(self.id, status, "failed")),
            None => Ok(ChatResponse {
                model: req.model,
                ..Default::default()
            }),
        }
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut frames = vec![Ok(content_chunk())];
        if let Some(status) = self.status {
            frames.push(Err(Error::upstream(self.id, status, "failed mid-stream")));
        }
        Ok(Box::pin(futures::stream::iter(frames)))
    }
}

/// `[retry]` and the two provider tables every row shares; `extra` is
/// appended verbatim, so a row adds `[retry.classes]`.
/// The breaker threshold is high enough that no row opens it.
fn config(extra: &str) -> Config {
    let text = format!(
        "[retry]\nmax_attempts = 3\ninitial_backoff_ms = 1\nbackoff_multiplier = 1.0\n\
         jitter_ms = 0\n\n\
         [providers.p1]\nformat = \"openai-chat\"\nbase_url = \"https://example.test/v1\"\ncredential = \"literal:k\"\nauth = \"bearer\"\ncircuit_failures = 10\n\n\
         [providers.p2]\nformat = \"openai-chat\"\nbase_url = \"https://example.test/v1\"\ncredential = \"literal:k\"\nauth = \"bearer\"\n\n\
         [aliases]\nchain = [\"m1\", \"m2\"]\n\n{extra}"
    );
    toml::from_str(&text).expect("valid test toml")
}

fn router(extra: &str, first: Arc<Seat>, second: Arc<Seat>) -> Router {
    let mut router = Router::new(Arc::new(config(extra)));
    router.install_resolved_models(BTreeMap::from([
        (
            "m1".to_string(),
            Arc::new(ResolvedModel::new("m1", "p1", first, "u1")),
        ),
        (
            "m2".to_string(),
            Arc::new(ResolvedModel::new("m2", "p2", second, "u2")),
        ),
    ]));
    router
}

fn request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.into(),
        messages: vec![].into(),
        ..Default::default()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Path {
    /// A single-seat completion: how many times the failing seat is dialed.
    Retry,
    /// A two-seat completion with the first seat failing: whether the walk
    /// reaches the second seat.
    Fallback,
    /// A committed stream whose frame error follows content: what the first
    /// seat's breaker records.
    StreamBreaker,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Observed {
    Calls(usize),
    ReachedSecondSeat(bool),
    Breaker(Settled),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settled {
    Debited,
    Untouched,
}

async fn observe(path: Path, status: u16, extra: &str) -> Observed {
    let first = Seat::new("p1", Some(status));
    let second = Seat::new("p2", None);
    let router = router(extra, Arc::clone(&first), Arc::clone(&second));
    match path {
        Path::Retry => {
            let failed = router.complete(request("m1")).await.is_err();
            assert!(failed, "premise: the lone seat fails");
            Observed::Calls(first.calls())
        }
        Path::Fallback => {
            let _ = router.complete(request("chain")).await;
            Observed::ReachedSecondSeat(second.calls() > 0)
        }
        Path::StreamBreaker => {
            let frames: Vec<Result<ChatChunk>> = router
                .stream(request("m1"))
                .await
                .expect("first content commits the stream")
                .collect()
                .await;
            assert!(
                frames.last().is_some_and(Result::is_err),
                "premise: error last"
            );
            let gate = router
                .status_targets(Instant::now())
                .into_iter()
                .find(|t| t.nickname == "m1")
                .expect("the failing seat has a status row")
                .gate;
            assert_eq!(gate.circuit, CircuitPhase::Closed, "premise: no row trips");
            Observed::Breaker(match gate.last_outcome {
                Some(LastOutcome::Ok) | None => Settled::Untouched,
                Some(_) => Settled::Debited,
            })
        }
    }
}

#[tokio::test]
async fn retry_fallback_and_breaker_follow_the_built_in_class_and_retry_classes() {
    let rows: [(&str, Path, u16, &str, Observed); 7] = [
        (
            "retry / 503 built-in",
            Path::Retry,
            503,
            "",
            Observed::Calls(3),
        ),
        (
            "retry / control: [retry.classes] caps server-error",
            Path::Retry,
            503,
            "[retry.classes]\nserver-error = 1\n",
            Observed::Calls(1),
        ),
        (
            "fallback / 503 built-in",
            Path::Fallback,
            503,
            "",
            Observed::ReachedSecondSeat(true),
        ),
        (
            "fallback / a server-error count of 0 still walks on",
            Path::Fallback,
            503,
            "[retry.classes]\nserver-error = 0\n",
            Observed::ReachedSecondSeat(true),
        ),
        (
            "fallback / control: an unknown status ends the walk",
            Path::Fallback,
            302,
            "",
            Observed::ReachedSecondSeat(false),
        ),
        (
            "stream breaker / 503 built-in",
            Path::StreamBreaker,
            503,
            "",
            Observed::Breaker(Settled::Debited),
        ),
        (
            "stream breaker / control: a built-in 400 does not debit",
            Path::StreamBreaker,
            400,
            "",
            Observed::Breaker(Settled::Untouched),
        ),
    ];

    let mut mismatches = Vec::new();
    for (name, path, status, extra, expected) in rows {
        let got = observe(path, status, extra).await;
        if got != expected {
            mismatches.push(format!("{name}: expected {expected:?}, got {got:?}"));
        }
    }

    assert!(mismatches.is_empty(), "{mismatches:#?}");
}
