//! `Router::run_probe_pass`: one driver tick runs the due free batch to
//! completion and reports how many validators it ran.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::field_verdict::FieldVerdictKey;
use crate::probe_scheduler::ProbeValidator;
use crate::router::probe_test_support::{FailingProvider, GROUNDED_PATH, lane, remote_router};

fn failing_provider() -> Arc<FailingProvider> {
    Arc::new(FailingProvider {
        status: 500,
        calls: AtomicUsize::new(0),
        count_calls: AtomicUsize::new(0),
    })
}

#[tokio::test]
async fn a_pass_on_a_freshly_built_router_runs_no_work() {
    // Arrange: no queued job.
    let provider = failing_provider();
    let router = remote_router(provider.clone());

    // Act
    let summary = router.run_probe_pass().await;

    // Assert
    assert_eq!(summary.free_validators_run, 0);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_pass_runs_the_due_free_validator_and_nothing_else() {
    // Arrange: one due free job on a resolvable lane.
    let provider = failing_provider();
    let router = remote_router(provider.clone());
    let key = FieldVerdictKey::new(&lane("m1"), GROUNDED_PATH, "anthropic-api").expect("identity");
    router.activate_probe_lane(&key, ProbeValidator::CountTokens);

    // Act
    let summary = router.run_probe_pass().await;

    // Assert: the one free validator ran, and it dialed only `count_tokens`.
    assert_eq!(summary.free_validators_run, 1);
    assert_eq!(provider.count_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "a pass dispatches no call beyond the free validator's count_tokens",
    );
}
