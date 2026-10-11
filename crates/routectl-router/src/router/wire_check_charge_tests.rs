//! The sampler charge an attempt takes at tap install: given back exactly
//! once when the attempt fails or its future is dropped before the provider
//! answers, and kept by an attempt that succeeded, including a stream that
//! fails after its first content.

use std::time::Duration;

use super::*;

/// What one provider call does.
#[derive(Clone, Copy, Debug)]
enum Step {
    /// Never answer.
    Hang,
    /// Fail with a retryable 503 that is not a rejection.
    Fail,
    /// Succeed, depositing the client's body without [`LOST_KEY`].
    ServeLosing,
    /// Open a stream that yields one content chunk and then fails.
    ContentThenFail,
}

/// A provider that plays `steps` in order, one per call, repeating the last.
struct ScriptedProvider {
    steps: Vec<Step>,
    calls: Mutex<usize>,
}

impl ScriptedProvider {
    fn new(steps: impl Into<Vec<Step>>) -> Self {
        Self {
            steps: steps.into(),
            calls: Mutex::new(0),
        }
    }

    fn next_step(&self) -> Step {
        let mut calls = self.calls.lock();
        let step = self
            .steps
            .get(*calls)
            .or_else(|| self.steps.last())
            .copied()
            .expect("a script has at least one step");
        *calls += 1;
        step
    }
}

#[async_trait::async_trait]
impl Provider for ScriptedProvider {
    fn id(&self) -> &'static str {
        "scripted"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("scripted", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        match self.next_step() {
            Step::Hang => std::future::pending().await,
            Step::Fail => Err(unavailable()),
            Step::ServeLosing | Step::ContentThenFail => {
                DepositingProvider::new(Deposit::IngressWithout(LOST_KEY)).report(&req);
                Ok(ok_response())
            }
        }
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        match self.next_step() {
            Step::Hang => std::future::pending().await,
            Step::Fail => Err(unavailable()),
            Step::ServeLosing => {
                DepositingProvider::new(Deposit::IngressWithout(LOST_KEY)).report(&req);
                Ok(stream::iter([Ok(content_chunk())]).boxed())
            }
            Step::ContentThenFail => {
                DepositingProvider::new(Deposit::IngressWithout(LOST_KEY)).report(&req);
                Ok(stream::iter([Ok(content_chunk()), Err(unavailable())]).boxed())
            }
        }
    }
}

/// Dispatch `req` on `surface` and drain whatever comes back, success or
/// not.
async fn dispatch_any(router: &Router, surface: Surface, req: ChatRequest) {
    match surface {
        Surface::Complete => {
            let _ = Box::pin(router.complete(req)).await;
        }
        Surface::Stream => {
            if let Ok(stream) = Box::pin(router.stream(req)).await {
                let _: Vec<_> = stream.collect().await;
            }
        }
    }
}

fn loss_checks(router: &Router) -> u64 {
    router
        .wire_check_snapshot()
        .checks
        .iter()
        .filter(|c| c.outcome == CheckOutcome::Loss)
        .map(|c| c.reading.count)
        .sum()
}

#[tokio::test]
async fn an_attempt_dropped_before_the_provider_answers_gives_its_charge_back() {
    for surface in SURFACES {
        let provider = Arc::new(ScriptedProvider::new([Step::Hang, Step::ServeLosing]));
        let router = router("first_n = 1\nsample_one_in = 1000", vec![provider]);

        let dropped = tokio::time::timeout(
            Duration::from_millis(50),
            dispatch_any(&router, surface, anthropic_request()),
        )
        .await;
        assert!(dropped.is_err(), "{surface:?}: the hung attempt is dropped");
        dispatch(&router, surface, anthropic_request()).await;

        assert_eq!(
            outcomes(&router.wire_check_snapshot()),
            [("anthropic->anthropic-api", CheckOutcome::Loss, 1)],
            "{surface:?}: the dropped attempt's slot goes to the next success"
        );
    }
}

#[tokio::test]
async fn a_failed_attempt_gives_its_charge_back_exactly_once() {
    for surface in SURFACES {
        let steps = [
            Step::ServeLosing,
            Step::Fail,
            Step::Fail,
            Step::Fail,
            Step::ServeLosing,
        ];
        let provider = Arc::new(ScriptedProvider::new(steps));
        let router = router("first_n = 2\nsample_one_in = 1000", vec![provider]);

        for _ in steps {
            dispatch_any(&router, surface, anthropic_request()).await;
        }
        dispatch(&router, surface, anthropic_request()).await;

        assert_eq!(
            loss_checks(&router),
            2,
            "{surface:?}: the first two successes are checked, the third is not"
        );
    }
}

#[tokio::test]
async fn a_stream_failing_after_first_content_keeps_its_charge() {
    let provider = Arc::new(ScriptedProvider::new([
        Step::ContentThenFail,
        Step::ServeLosing,
    ]));
    let router = router("first_n = 1\nsample_one_in = 1000", vec![provider]);

    dispatch_any(&router, Surface::Stream, anthropic_request()).await;
    dispatch(&router, Surface::Stream, anthropic_request()).await;

    assert_eq!(
        loss_checks(&router),
        1,
        "the failed stream spent the only first-N slot"
    );
}

#[tokio::test]
async fn a_retry_on_the_same_target_is_charged_once_after_the_failed_attempt_gives_back() {
    // Only the complete surface retries a target in place; a stream walks on.
    let provider = Arc::new(ScriptedProvider::new([Step::Fail, Step::ServeLosing]));
    let router = router_retrying(
        2,
        "first_n = 2\nsample_one_in = 1000",
        vec![provider.clone()],
    );

    let mut checked = Vec::new();
    for _ in 0..3 {
        dispatch(&router, Surface::Complete, anthropic_request()).await;
        checked.push(loss_checks(&router));
    }

    assert_eq!(
        *provider.calls.lock(),
        4,
        "premise: only the first 503 was retried"
    );
    assert_eq!(
        checked,
        [1, 2, 2],
        "the failed attempt gives its slot back and the retry takes one, so the next \
         request gets the second first-N slot and the one after it gets none"
    );
}
