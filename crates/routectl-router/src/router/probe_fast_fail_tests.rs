//! Availability-probe fast-fail. Claude Code sends `max_tokens=1`
//! quota/health probes to `/v1/messages`. On a rate-limit (429) or
//! overload (529) these skip retry+fallback -- walking the
//! all-Anthropic chain is futile (every hop shares the limit) and
//! the 1-token output is unread. Every OTHER error class is
//! unaffected, so real requests and 4xx-capability fallback keep
//! today's behavior. Each test names the (is_probe, status) shape
//! it pins.
use super::super::dispatch::{should_fallback, should_retry_same_provider};
use super::*;
use routectl_core::failure_class::{FailureClass, classify};

fn upstream(status: u16) -> Error {
    Error::upstream("probe-test-provider", status, "x")
}

fn class_of(err: &Error) -> FailureClass {
    classify(err, None).class
}

fn req_with_max_tokens(max_tokens: Option<u32>) -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: vec![].into(),
        max_tokens,
        ..Default::default()
    }
}

#[test]
fn probe_429_does_not_fall_back() {
    // Arrange
    let err = upstream(429);
    let class = class_of(&err);
    let policy = RetryPolicy::default();
    // Act
    let fall_back = should_fallback(&err, &class, &policy, true);
    // Assert
    assert!(
        !fall_back,
        "a max_tokens<=probe_max_tokens probe must not walk the chain on 429",
    );
}

#[test]
fn probe_429_does_not_retry_same_provider() {
    // Arrange: attempts_made=0 so the ONLY reason not to retry is
    // the probe short-circuit (the cap would otherwise allow it).
    let err = upstream(429);
    let class = class_of(&err);
    let policy = RetryPolicy::default();
    // Act
    let retry = should_retry_same_provider(&err, &class, &policy, 0, true);
    // Assert
    assert!(
        !retry,
        "a probe must not burn retry attempts against a rate-limited provider",
    );
}

#[test]
fn probe_529_does_not_fall_back() {
    // 529 is Anthropic's overload status; on an all-Anthropic chain
    // every hop shares it, so a probe fast-fails it like a 429.
    let err = upstream(529);
    let class = class_of(&err);
    let policy = RetryPolicy::default();
    assert!(!should_fallback(&err, &class, &policy, true));
}

#[test]
fn probe_529_does_not_retry_same_provider() {
    // Symmetry with the 429 retry short-circuit, for the 529 branch.
    let err = upstream(529);
    let class = class_of(&err);
    let policy = RetryPolicy::default();
    assert!(!should_retry_same_provider(&err, &class, &policy, 0, true));
}

#[test]
fn probe_400_still_falls_back() {
    // Bedrock rejects max_tokens=1 with a 400; a sibling provider
    // may accept it, so a probe must still walk the chain on 4xx.
    let err = upstream(400);
    let class = class_of(&err);
    let policy = RetryPolicy::default();
    assert!(should_fallback(&err, &class, &policy, true));
}

#[test]
fn probe_503_still_falls_back() {
    // 503 is generic unavailability (not the chain-wide 429/529); a
    // sibling provider may be healthy, so the probe still falls back.
    let err = upstream(503);
    let class = class_of(&err);
    let policy = RetryPolicy::default();
    assert!(should_fallback(&err, &class, &policy, true));
}

#[test]
fn real_request_429_still_retries_and_falls_back() {
    // is_probe=false (a real request): a 429 keeps today's behavior
    // -- fallbackable AND retryable up to the policy cap.
    let err = upstream(429);
    let class = class_of(&err);
    let policy = RetryPolicy::default();
    assert!(
        should_fallback(&err, &class, &policy, false),
        "real-request 429 still falls back",
    );
    assert!(
        should_retry_same_provider(&err, &class, &policy, 0, false),
        "real-request 429 still retries (attempts_made=0 < cap)",
    );
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
    // real request (falls back + retries) -- today's behavior.
    let policy = RetryPolicy {
        probe_max_tokens: 0,
        ..RetryPolicy::default()
    };
    let req = req_with_max_tokens(Some(1));
    assert!(
        !is_probe_request(&req, &policy),
        "threshold 0 disables probe detection",
    );

    let is_probe = is_probe_request(&req, &policy); // false
    let err = upstream(429);
    let class = class_of(&err);
    assert!(should_fallback(&err, &class, &policy, is_probe));
    assert!(should_retry_same_provider(
        &err, &class, &policy, 0, is_probe
    ));
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
    // Downstream: an at-threshold probe still fast-fails a 429.
    let is_probe = is_probe_request(&req_with_max_tokens(Some(2)), &policy);
    let err = upstream(429);
    let class = class_of(&err);
    assert!(
        !should_fallback(&err, &class, &policy, is_probe),
        "an at-threshold probe must not fall back on 429 at a custom threshold",
    );
}
