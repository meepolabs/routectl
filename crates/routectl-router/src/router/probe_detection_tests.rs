//! Availability-probe detection. Claude Code sends `max_tokens=1`
//! quota/health probes to `/v1/messages`; a request at or under
//! `probe_max_tokens` is a probe, and the failure decision never retries
//! it and keeps it inside the run of seats it started in. What a probe's
//! failure does, status by status, is pinned with the decision itself;
//! these tests pin which requests are probes and that the detection
//! reaches the decision.
use super::super::class_observe::DispatchSurface;
use super::super::outcome_rule::{Disposition, Failure, SameProvider, decide};
use super::super::walk_state::{Exclusion, RequestWalk, WalkTarget};
use super::*;
use routectl_core::failure_class::classify;

fn upstream(status: u16) -> Error {
    Error::upstream("probe-test-provider", status, "x")
}

fn req_with_max_tokens(max_tokens: Option<u32>) -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: vec![].into(),
        max_tokens,
        ..Default::default()
    }
}

/// The decision for a 429 on a fresh complete walk, for a request whose
/// probe-ness is `is_probe`.
fn decided_429(is_probe: bool, policy: &RetryPolicy) -> Disposition {
    let err = upstream(429);
    let class = classify(&err, None).class;
    let target = WalkTarget::fixture("p", None, "m", 0);
    let failure = Failure {
        err: &err,
        native_class: &class,
        class: &class,
        is_probe,
        surface: DispatchSurface::Complete,
        deadline_fired: false,
        disable_fallbacks: false,
        target,
    };
    decide(&failure, policy, None, &RequestWalk::new())
}

fn fenced(disposition: &Disposition) -> bool {
    disposition
        .exclusions
        .iter()
        .any(|exclusion| matches!(exclusion, Exclusion::OutsideRun(_)))
}

#[test]
fn is_probe_request_predicate_boundary() {
    // Default threshold is 1.
    let policy = RetryPolicy::default();
    assert_eq!(policy.probe_max_tokens, 1, "default probe_max_tokens is 1");

    assert!(
        is_probe_request(&req_with_max_tokens(Some(1)), &policy),
        "max_tokens=1 at threshold 1 IS a probe",
    );
    assert!(
        !is_probe_request(&req_with_max_tokens(Some(2)), &policy),
        "max_tokens=2 above threshold 1 is NOT a probe",
    );
    assert!(
        !is_probe_request(&req_with_max_tokens(None), &policy),
        "max_tokens=None is NEVER a probe",
    );
}

#[test]
fn probe_disabled_when_threshold_zero() {
    // probe_max_tokens=0 disables probe detection entirely: a
    // max_tokens=1 request is NOT a probe, so a 429 behaves like a
    // real request (retries, no probe fence).
    let policy = RetryPolicy {
        probe_max_tokens: 0,
        ..RetryPolicy::default()
    };
    let req = req_with_max_tokens(Some(1));
    assert!(
        !is_probe_request(&req, &policy),
        "threshold 0 disables probe detection",
    );

    let disposition = decided_429(is_probe_request(&req, &policy), &policy);
    assert!(matches!(
        disposition.same_target,
        SameProvider::Retry { .. }
    ));
    assert!(!fenced(&disposition), "{:?}", disposition.exclusions);
}

#[test]
fn custom_probe_max_tokens_threshold_is_inclusive() {
    // A non-default threshold (probe_max_tokens=2) pins the `<=`
    // boundary the default-1 tests cannot distinguish from `<`:
    // max_tokens=2 IS a probe (at the threshold), max_tokens=3 is
    // NOT (above it). A `<` regression would misclassify the
    // at-threshold value as a real request.
    let policy = RetryPolicy {
        probe_max_tokens: 2,
        ..RetryPolicy::default()
    };
    assert!(
        is_probe_request(&req_with_max_tokens(Some(2)), &policy),
        "max_tokens=2 is AT the custom probe_max_tokens=2 threshold (inclusive)",
    );
    assert!(
        !is_probe_request(&req_with_max_tokens(Some(3)), &policy),
        "max_tokens=3 is ABOVE the custom threshold; a real request",
    );
    // Downstream: an at-threshold probe stays in its run and never retries.
    let is_probe = is_probe_request(&req_with_max_tokens(Some(2)), &policy);
    let disposition = decided_429(is_probe, &policy);
    assert_eq!(disposition.same_target, SameProvider::No);
    assert!(fenced(&disposition), "{:?}", disposition.exclusions);
}
