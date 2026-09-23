//! Test-only fixture, seeding, and instrumentation seams for the fidelity status
//! surface.
//!
//! Compiled only under `cfg(test)` or the non-default `test-utils` feature. THE
//! dangerous invariant, and the only one worth restating here: none of these may exist
//! in a release build. A production caller could otherwise mint a routing-affecting
//! verdict from nothing, force a re-verification off-schedule, or hand a status reader
//! a fabricated row -- each of which is a state the surrounding safety rules exist to
//! keep out of a deployment. A release-absence probe and the API baseline both check it.

use std::time::Instant;

use routectl_core::{EvidenceSource, FailurePhase};

use crate::field_canary::CanaryOutcome;
use crate::field_verdict::FieldVerdictKey;

// `super` is the `fidelity_status` module this is declared from, so its own public
// items come from there and its siblings from the grandparent `router`.
use super::{CanaryPosture, FieldVerdictStatus, PreflightBlockedReason};
use crate::router::Router;
use crate::router::field_verdict_observability::FieldRepairCounters;

/// A [`FieldVerdictStatus`]'s fields, as a constructable value.
///
/// EXISTS FOR CROSS-CRATE TESTS, and the shape is deliberate on both counts.
///
/// `FieldVerdictStatus` is `#[non_exhaustive]`, which refuses every foreign
/// struct expression -- literal and functional-update alike -- so that a new
/// observable stays additive for its readers. The cost is that the renderer tests
/// in `routectl-cli`, which need rows whose OPTIONAL fields are absent in
/// combinations no live fixture produces, cannot construct one at all.
///
/// This type is EXHAUSTIVE, which is the point rather than an oversight: a new
/// field on the row breaks every test that builds one, which is exactly the
/// moment somebody has to decide how the new field RENDERS. A `non_exhaustive`
/// spec would let a field be added, go unrendered, and read as absent on the
/// status surface forever.
///
/// GATED behind `cfg(test)` or the non-default `test-utils` feature, so neither
/// this type nor the constructor below exists in a release build. `pub` within
/// that gate because the consumer is another crate and cannot see this one's test
/// cfg -- a dev-dependency feature is how it is turned on for test builds only.
/// `doc(hidden)` and `_for_tests` on the conversion so neither is mistaken for a
/// production path: production rows come from [`Router::field_verdict_status`],
/// which reads live state, and a hand-built row in production would be a
/// fabricated status report.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
#[derive(Debug, Clone)]
pub struct FieldVerdictStatusSpec {
    pub state_key: String,
    pub capability_key: String,
    pub transform_class: Option<&'static str>,
    pub prefix_impacting: bool,
    pub source: EvidenceSource,
    pub phase: FailurePhase,
    pub confirmations: u32,
    pub required_quorum: Option<u32>,
    pub blocked_reason: Option<PreflightBlockedReason>,
    pub canary: CanaryPosture,
    pub canary_remaining_requests: u32,
    pub canary_last_outcome: Option<CanaryOutcome>,
    pub requests_in_flight: u64,
    pub unconfirmed_requests: u64,
}

#[cfg(any(test, feature = "test-utils"))]
impl FieldVerdictStatus {
    /// Build a row from `spec`. See `FieldVerdictStatusSpec`.
    #[doc(hidden)]
    #[must_use]
    pub fn from_spec_for_tests(spec: FieldVerdictStatusSpec) -> Self {
        Self {
            state_key: spec.state_key,
            capability_key: spec.capability_key,
            transform_class: spec.transform_class,
            prefix_impacting: spec.prefix_impacting,
            source: spec.source,
            phase: spec.phase,
            confirmations: spec.confirmations,
            required_quorum: spec.required_quorum,
            blocked_reason: spec.blocked_reason,
            canary: spec.canary,
            canary_remaining_requests: spec.canary_remaining_requests,
            canary_last_outcome: spec.canary_last_outcome,
            requests_in_flight: spec.requests_in_flight,
            unconfirmed_requests: spec.unconfirmed_requests,
        }
    }
}

/// Seed each fidelity counter to a DISTINCT value, for a cross-crate test that
/// asserts every emitted log field maps to the counter it is named for.
///
/// # Why distinct values, and why a seam at all
///
/// The mapping was previously pinned by PARSING the emitter's source text for
/// `field=source` pairs. That catches a swap, but it is a claim about how the code
/// is written rather than about what it emits: a `tracing` macro change, a
/// rustfmt-invisible rewrite, or a renamed local all break the parse while the
/// behavior is fine, and a genuinely wrong mapping written a different way slips
/// past. Asserting on the CAPTURED EVENT is the behavior -- but only if every
/// counter carries a different number, because equal values make any permutation
/// pass.
///
/// So this sets seven distinct counts. The counters are private to this crate and
/// the consumer is `routectl-cli`, hence the seam; it is gated to test builds, so no
/// release build can forge a counter.
///
/// Returns the values it set, in the order the fields are documented, so the test
/// asserts against what was actually seeded rather than restating literals.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn seed_distinct_fidelity_counters_for_tests(router: &Router) -> FieldRepairCounters {
    // Deliberately unequal, and deliberately not 1..=7 in field order either: a
    // permutation of consecutive values is easier to satisfy by accident than a
    // spread one.
    for _ in 0..3 {
        router.metrics.incr_field_repair_attempted();
    }
    for _ in 0..11 {
        router.metrics.incr_field_repair_succeeded();
    }
    for _ in 0..5 {
        router.metrics.incr_field_verdicts_learned();
    }
    for _ in 0..23 {
        router.metrics.incr_field_preflight_action();
    }
    for _ in 0..17 {
        router.metrics.incr_parser_unlocalized();
    }
    // The two alarm halves live on the shared canary registry rather than the
    // metrics atomics, so they are seeded through it -- and to two MORE distinct
    // values, since reporting either as the other is the inversion an operator
    // cannot detect.
    let key = FieldVerdictKey::from_capability_key(
        "seeded".to_string(),
        crate::field_capability::field_capability_key("thinking.enabled.display")
            .expect("the grounded path is well-formed"),
        String::new(),
    );
    let canaries = router.field_verdicts().canaries();
    for _ in 0..7 {
        drop(
            canaries
                .begin_modified_request(&key, 1)
                .expect("accounts at the resident incarnation"),
        );
    }
    // A disproof transfers the seven into the LIFETIME half, then two more are left
    // outstanding -- so the two halves read 2 and 7 rather than any shared number.
    canaries
        .claim_canary(&key, 1)
        .expect("claim admitted")
        .settle(CanaryOutcome::Regressed);
    for _ in 0..2 {
        drop(
            canaries
                .begin_modified_request(&key, 1)
                .expect("accounts at the resident incarnation"),
        );
    }
    router.field_repair_counters()
}

/// Force a resident verdict's canary DUE on the next eligible request.
///
/// The production writer for exactly this state is the cold-rebuild seed's
/// `due_immediately` flag: a boot that finds an already-acting verdict makes it due
/// on its very next eligible request rather than waiting a full interval. This uses
/// that same seed, so what a test establishes here is a state a real boot produces.
///
/// Its purpose is to let an assembled-daemon test assert the canary SETTLEMENT over a
/// real request boundary without re-proving the cadence NUMBER, which is walked in
/// full against a real Router elsewhere. Dispatching a hundred HTTP requests to reach
/// the same state would test the cadence a second time and the boundary no better.
///
/// GATED to test builds: forcing a re-verification due in production would send the
/// unrepaired variant upstream off-schedule.
///
/// # Panics
///
/// If `field_path` is not a well-formed qualified field path, or if no verdict is
/// resident for it -- a seed over an absent identity would leave the caller asserting
/// against a cadence that never starts.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn make_field_canary_due_for_tests(router: &Router, state_key: &str, field_path: &str) {
    let capability_key = crate::field_capability::field_capability_key(field_path)
        .expect("the fixture's field path must be a well-formed qualified path");
    let provider_kind = router.provider_kind_for_state_key(state_key).to_string();
    let key =
        FieldVerdictKey::from_capability_key(state_key.to_string(), capability_key, provider_kind);
    let resident = router
        .field_verdicts()
        .canaries()
        .snapshot(&key)
        .expect("a verdict must be resident before its canary can be made due");
    router.field_verdicts().canaries().seed_from_rebuild(
        &key,
        resident.incarnation,
        resident.confirmations,
        // The `due_immediately` flag: the cadence countdown is set to one, so the
        // next eligible request trips it.
        true,
    );
}

/// Plant one resident ACTING envelope-field verdict and acknowledge
/// `confirmations` cycles for it, so a cross-crate test can drive the REAL status
/// surface against real registry state.
///
/// # Why this seam exists at all
///
/// The `routectl-cli` status tests read the router through a facade whose whole
/// contract is that it exposes only non-mutating reads -- correctly, since it is
/// what makes a status poll structurally unable to mutate. That leaves those tests
/// no way to establish the state they need to assert ON, so the alternative was a
/// test that emits an EMPTY row set and asserts the field is present: vacuous
/// exactly where it matters, because a projection that rendered nothing would pass
/// it.
///
/// So the planting goes through the same two production seams the cold-boot path
/// uses -- the registry's own `import_entries` and the canary registry's
/// rebuild seed -- rather than through a back door into either store. What a test
/// plants here is a state a real ledger replay can produce.
///
/// GATED behind `cfg(test)` or the non-default `test-utils` feature, so it is
/// absent from every release build: a production caller could otherwise mint a
/// routing-affecting verdict from nothing.
///
/// # Panics
///
/// If `field_path` is not a well-formed qualified field path. A fixture naming an
/// unmintable path would plant nothing and leave the test asserting over an empty
/// surface, which is the vacuity this seam exists to remove.
#[cfg(any(test, feature = "test-utils"))]
#[doc(hidden)]
pub fn plant_acting_field_verdict_for_tests(
    router: &Router,
    state_key: &str,
    field_path: &str,
    confirmations: u32,
) {
    let capability_key = crate::field_capability::field_capability_key(field_path)
        .expect("the fixture's field path must be a well-formed qualified path");
    let stamped = Instant::now();
    router
        .learned_registry()
        .import_entries(vec![crate::learned_capability::ExportedEntry {
            state_key: state_key.to_string(),
            feature_key: capability_key.clone(),
            verdict: crate::learned_capability::EntryVerdict::Negative,
            signal: routectl_core::capability::SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: stamped,
            last_seen: stamped,
            // An hour out, so nothing in a test lapses by wall-clock.
            expires_at: stamped + std::time::Duration::from_hours(1),
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
            evidence_class: None,
        }]);
    let provider_kind = router.provider_kind_for_state_key(state_key).to_string();
    let key =
        FieldVerdictKey::from_capability_key(state_key.to_string(), capability_key, provider_kind);
    // The confirmation count's only production writer is the cold-rebuild seed, so
    // that is what this uses -- a test-only mutator on the canary registry would be
    // a second way to raise a count that gates traffic.
    router.field_verdicts().canaries().seed_from_rebuild(
        &key,
        router.learned_registry().resident_incarnation_for_tests(
            state_key,
            key.capability_key(),
            router.provider_kind_for_state_key(state_key),
        ),
        confirmations,
        false,
    );
}
