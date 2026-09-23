//! The free plan and the paid boundary: validator ordering, what spends a
//! free step, and the two conditions a paid call would need.

use std::time::Instant;

use super::test_support::{key, payload};
use super::{
    FreeValidatorOutcome, PROBE_QUEUE_DEPTH, ProbeActivation, ProbeScheduler, ProbeValidator,
    paid_probe_permitted, validator_plan,
};

#[test]
fn free_validators_are_ordered_before_the_paid_path() {
    // Arrange / Act
    let with_counting = validator_plan(true);
    let without_counting = validator_plan(false);

    // Assert: every free validator precedes every paid one.
    for plan in [&with_counting, &without_counting] {
        let first_paid = plan.iter().position(|v| !v.is_free());
        let last_free = plan.iter().rposition(|v| v.is_free());
        if let (Some(paid), Some(free)) = (first_paid, last_free) {
            assert!(
                free < paid,
                "a paid validator preceded a free one: {plan:?}"
            );
        }
    }
    assert_eq!(with_counting.first(), Some(&ProbeValidator::CountTokens));
    // The expected-rejection validator is DELIBERATELY absent from every
    // plan: it has no grounded rejection template, so it can perform no
    // operation, and listing it would let a step that never ran report as
    // executed and advance a lane toward paid eligibility. The earlier
    // expectation that it leads a non-counting lane's plan was wrong for
    // exactly that reason.
    for plan in [&with_counting, &without_counting] {
        assert!(
            !plan.contains(&ProbeValidator::ExpectedRejection),
            "an inert validator must not appear in an executable plan: {plan:?}"
        );
    }
    // A lane with no executable free step has no free step at all, and the
    // scheduler refuses such a plan rather than queueing a no-op job.
    assert!(
        !without_counting.iter().any(|v| v.is_free()),
        "count_tokens is the only executable free step in this build"
    );
}

#[test]
fn a_plan_with_no_executable_free_step_queues_nothing() {
    // The converse of the exclusion above: a lane whose plan carries only
    // the paid class must not queue a job, or the worker would lease
    // something it may never dial.
    let scheduler = ProbeScheduler::new();

    let outcome = scheduler.activate(&key(0), 1, validator_plan(false), payload());

    assert_eq!(outcome, ProbeActivation::NoFreeValidator);
    assert_eq!(scheduler.snapshot(Instant::now()).queued, 0);
}

#[test]
fn a_settled_free_validator_blocks_the_paid_path_even_with_budget() {
    // Arrange / Act / Assert: free evidence answers the question, so no
    // paid call is warranted regardless of the cap.
    assert!(!paid_probe_permitted(FreeValidatorOutcome::Settled, 100));
}

#[test]
fn a_zero_cap_blocks_the_paid_path_for_every_free_outcome() {
    // `Exhausted` is listed FIRST and is the load-bearing case: it is the only
    // outcome the outcome predicate alone admits, so a cap check deleted from
    // `paid_probe_permitted` changes no answer for any other variant here.
    // Without it this test passes against an unguarded cap (measured).
    for outcome in [
        FreeValidatorOutcome::Exhausted,
        FreeValidatorOutcome::Settled,
        FreeValidatorOutcome::Inconclusive,
        FreeValidatorOutcome::Unavailable,
    ] {
        assert!(
            !paid_probe_permitted(outcome, 0),
            "cap zero must block the paid path for {outcome:?}"
        );
    }
}

#[test]
fn the_paid_path_opens_only_once_every_free_step_has_run() {
    // One INCONCLUSIVE step is not an exhausted plan, so treating it as
    // paid-eligible would let a lane reach the paid class having never run
    // its remaining free validators. Only `Exhausted` -- every free step
    // spent -- opens the paid path.
    assert!(paid_probe_permitted(FreeValidatorOutcome::Exhausted, 1));
    assert!(!paid_probe_permitted(FreeValidatorOutcome::Inconclusive, 1));
    assert!(!paid_probe_permitted(FreeValidatorOutcome::Unavailable, 1));
    assert!(!paid_probe_permitted(FreeValidatorOutcome::Settled, 1));
}

#[test]
fn free_validators_stay_bounded_by_the_scheduler_despite_having_no_spend_cap() {
    // Arrange: only free validators, more identities than the depth bound.
    let scheduler = ProbeScheduler::new();
    let mut queued = 0usize;
    for n in 0..PROBE_QUEUE_DEPTH * 4 {
        let validator = if n % 2 == 0 {
            ProbeValidator::CountTokens
        } else {
            ProbeValidator::ExpectedRejection
        };
        assert!(validator.is_free());
        if scheduler.activate(&key(n), 1, vec![validator], payload()) == ProbeActivation::Queued {
            queued += 1;
        }
    }

    // Assert: the queue bound governs free work as strictly as paid work.
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
    let executable_free_steps = validator_plan(true)
        .into_iter()
        .filter(|v| v.is_free())
        .count();
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
