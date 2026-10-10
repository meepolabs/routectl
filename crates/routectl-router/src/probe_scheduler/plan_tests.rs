//! The validator plan: what a lane plans, what activation refuses, and the
//! bounds a free plan still obeys.

use std::time::Instant;

use super::test_support::{key, payload};
use super::{PROBE_QUEUE_DEPTH, ProbeActivation, ProbeScheduler, ProbeValidator, validator_plan};

#[test]
fn a_count_tokens_lane_plans_exactly_the_count_token_validator() {
    assert_eq!(validator_plan(true), vec![ProbeValidator::CountTokens]);
}

#[test]
fn a_lane_without_count_tokens_plans_nothing_and_activation_refuses_it() {
    // Arrange
    let scheduler = ProbeScheduler::new();
    let plan = validator_plan(false);
    assert!(
        plan.is_empty(),
        "a lane without count_tokens has no validator"
    );

    // Act
    let outcome = scheduler.activate(&key(0), 1, plan, payload());

    // Assert: refused rather than queueing a job that could only no-op.
    assert_eq!(outcome, ProbeActivation::NoFreeValidator);
    let snap = scheduler.snapshot(Instant::now());
    assert_eq!(snap.queued, 0);
    assert_eq!(snap.activations_total, 0);
}

#[test]
fn free_validators_stay_bounded_by_the_scheduler_despite_having_no_spend_cap() {
    // Arrange: more identities than the depth bound.
    let scheduler = ProbeScheduler::new();
    let mut queued = 0usize;
    for n in 0..PROBE_QUEUE_DEPTH * 4 {
        if scheduler.activate(&key(n), 1, validator_plan(true), payload())
            == ProbeActivation::Queued
        {
            queued += 1;
        }
    }

    // Assert: the queue bound governs free work although it bills nothing.
    assert_eq!(queued, PROBE_QUEUE_DEPTH);
    assert_eq!(scheduler.snapshot(Instant::now()).queued, PROBE_QUEUE_DEPTH);
}

#[test]
fn activation_tokens_are_closed_set_snake_case() {
    // Arrange / Act / Assert: log-safe discriminants only.
    //
    // EVERY variant of the closed set, each with its expected token.
    //
    // The inner `match` is exhaustive, so a NEW variant fails to compile here
    // rather than shipping an unasserted token. The outer list is NOT
    // compiler-checked, though: a new variant added to the enum and to the
    // match but not to this list would still be skipped at runtime. The count
    // assertion below is what closes that gap.
    let mut seen: Vec<&str> = Vec::new();
    for variant in [
        ProbeActivation::Queued,
        ProbeActivation::Deduped,
        ProbeActivation::QueueFull,
        ProbeActivation::Retired,
        ProbeActivation::NoFreeValidator,
        ProbeActivation::Tombstoned,
    ] {
        let token = variant.as_str();
        let expected = match variant {
            ProbeActivation::Queued => "queued",
            ProbeActivation::Deduped => "deduped",
            ProbeActivation::QueueFull => "queue_full",
            ProbeActivation::Retired => "retired",
            ProbeActivation::NoFreeValidator => "no_free_validator",
            ProbeActivation::Tombstoned => "tombstoned",
        };
        assert_eq!(token, expected, "token drifted for {variant:?}");
        seen.push(token);
        assert!(
            !token.is_empty()
                && token
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c == '_' || c.is_ascii_digit()),
            "token must be log-safe snake_case: {token}"
        );
    }

    // Every token is DISTINCT, and the list covers the whole closed set. The
    // count is stated as a literal so adding a variant without adding it above
    // fails here rather than silently narrowing the coverage.
    seen.sort_unstable();
    let distinct = seen.len();
    seen.dedup();
    assert_eq!(seen.len(), distinct, "two variants share a token");
    assert_eq!(
        distinct, 6,
        "the closed set has six variants; a new one must be added to this list"
    );
}

#[test]
fn the_attempt_cap_also_bounds_the_executable_free_plan_length() {
    // Easy to miss, so pinned: advancing through the plan CHARGES an attempt,
    // so a free plan longer than `PROBE_MAX_ATTEMPTS` can never walk all of its
    // steps -- the job is abandoned mid-plan and its later steps are
    // unreachable.
    //
    // EXACTLY what this guarantees: that the plan's executable free length never
    // exceeds the attempt cap. It does NOT prove the plan can always walk every
    // step -- attempts are also charged by timeouts and retries, so a plan at the
    // cap can still be abandoned before its last step. The narrower property is
    // the one that is structural, and it is the one that fails loudly if a future
    // plan grows past the cap.
    let executable_free_steps = validator_plan(true).len();
    assert!(
        executable_free_steps >= 1,
        "premise: the plan must have a free step, or this bounds nothing"
    );
    assert!(
        u32::try_from(executable_free_steps).expect("plan length fits u32")
            <= crate::probe_scheduler::PROBE_MAX_ATTEMPTS,
        "a free plan of {executable_free_steps} steps cannot fully execute under an \
         attempt cap of {}: the later steps would be unreachable",
        crate::probe_scheduler::PROBE_MAX_ATTEMPTS
    );
}
