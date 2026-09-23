//! Coverage for the per-verdict INFO status/doctor projection: the row's
//! content, the blocked-reason vocabulary, the canary posture, and the PURITY
//! contract the whole surface rests on.
//!
//! # Why purity gets both a behavioral and a source assertion
//!
//! Behaviorally, a status read is asserted to leave the cadence, the claim, the
//! in-flight count and the due flag byte-identical -- read the canary snapshot,
//! read status many times, compare. That catches a mutation the read performs.
//! It does NOT catch a mutation added to a code path this fixture's rows do not
//! reach, and a status surface grows rows. So a source guard additionally
//! refuses the mutating call names anywhere in the production half of the
//! module, which fails on the EDIT rather than on a fixture that happens to
//! exercise it.

use super::*;

use std::sync::Arc;
use std::time::{Duration, Instant};

use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};

use crate::config::{CANARY_INTERVAL, Config, ENVELOPE_QUORUM, PREFIX_QUORUM};
use crate::field_canary::CanaryOutcome;

/// The one grounded closed-table path.
const GROUNDED_PATH: &str = "thinking.enabled.display";

/// A state key no `[providers]` entry names, so its provider kind resolves to
/// the empty token -- inert in capability-key normalization, which is what keeps
/// these fixtures' keys byte-stable without a resolved-model rig.
const STATE_KEY: &str = "sonnet";

/// Long enough that nothing in a test lapses by wall-clock.
const NOT_LAPSED: Duration = Duration::from_hours(1);

/// A Router whose lane admits pre-flight: one anthropic-api provider with a
/// remote-looking base URL, one model, one alias. Remote-looking because the
/// planner refuses a local hop, and the default base URL is what the production
/// `anthropic_api()` constructor writes -- which is the real `api.anthropic.com`.
fn bare_router() -> Router {
    use crate::config::{AliasValue, ModelEntry, ProviderEntry};
    let mut providers = std::collections::BTreeMap::new();
    providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api("env://K"),
    );
    let mut models = std::collections::BTreeMap::new();
    models.insert(
        STATE_KEY.to_string(),
        ModelEntry::new("anthropic", "claude-sonnet-4-5"),
    );
    let mut aliases = std::collections::BTreeMap::new();
    aliases.insert(
        "default".to_string(),
        AliasValue::Single(STATE_KEY.to_string()),
    );
    Router::new(Arc::new(Config {
        providers,
        aliases,
        models,
        ..Config::default()
    }))
}

/// The capability key the grounded path mints, through the namespace's sole
/// constructor -- never a literal, which is the second prefix spelling the
/// workspace-wide scan forbids.
fn grounded_key() -> String {
    crate::field_capability::field_capability_key(GROUNDED_PATH)
        .expect("the grounded path is well-formed")
}

/// The identity for `STATE_KEY`'s grounded row on a router whose provider kind
/// resolves empty.
fn grounded_identity(router: &Router) -> crate::field_verdict::FieldVerdictKey {
    crate::field_verdict::FieldVerdictKey::from_capability_key(
        STATE_KEY.to_string(),
        grounded_key(),
        router.provider_kind_for_state_key(STATE_KEY).to_string(),
    )
}

/// Plant a resident ACTING field verdict for `capability_key`, through the
/// registry's own import seam.
fn plant_verdict(router: &Router, capability_key: String) {
    plant_verdict_on(router, STATE_KEY, capability_key);
}

/// Plant a resident ACTING verdict for `capability_key` on `state_key`, through the
/// registry's own import seam.
///
/// Takes the state key so a fixture can plant two rows on different targets, and so
/// the sanitization cases can plant a hostile one.
fn plant_verdict_on(router: &Router, state_key: &str, capability_key: String) {
    let stamped = Instant::now();
    router
        .learned_capabilities
        .import_entries(vec![crate::learned_capability::ExportedEntry {
            state_key: state_key.to_string(),
            feature_key: capability_key,
            verdict: crate::learned_capability::EntryVerdict::Negative,
            signal: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: stamped,
            last_seen: stamped,
            expires_at: stamped + NOT_LAPSED,
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
            evidence_class: None,
        }]);
}

/// Seed `confirmations` acknowledged cycles for the grounded identity, through
/// the cold-rebuild seed -- the confirmation count's only production writer.
fn seed_confirmations(router: &Router, confirmations: u32) {
    let key = grounded_identity(router);
    let incarnation = router.learned_capabilities.resident_incarnation_for_tests(
        STATE_KEY,
        &grounded_key(),
        router.provider_kind_for_state_key(STATE_KEY),
    );
    router
        .field_verdicts()
        .canaries()
        .seed_from_rebuild(&key, incarnation, confirmations, false);
}

/// The single status row, asserting the count on the way -- an index-zero read
/// would silently describe whichever row came first if a fixture grew a second.
fn only_row(rows: &[FieldVerdictStatus]) -> &FieldVerdictStatus {
    assert_eq!(
        rows.len(),
        1,
        "this fixture plants one field verdict, so it must produce one row: {rows:?}",
    );
    &rows[0]
}

// ---------------------------------------------------------------------------
// The row's content
// ---------------------------------------------------------------------------

/// An acting, acknowledged envelope verdict reports every field the
/// observability floor names, with nothing blocking it.
///
/// One test over the whole row rather than one per field, because the floor's
/// contract is that these facts arrive TOGETHER: a reader holding the
/// confirmation count without the required quorum cannot tell sufficiency from
/// shortfall, and one holding a blocked reason without the class cannot tell
/// which gates apply.
#[test]
fn an_eligible_envelope_verdict_reports_its_whole_row_unblocked() {
    let router = bare_router();
    plant_verdict(&router, grounded_key());
    seed_confirmations(&router, ENVELOPE_QUORUM);

    let rows = router.field_verdict_status();

    let row = only_row(&rows);
    assert_eq!(row.state_key, STATE_KEY);
    assert_eq!(row.capability_key, grounded_key());
    assert_eq!(row.transform_class, Some("envelope"));
    assert!(
        !row.prefix_impacting,
        "the grounded row is envelope-class: its removal touches no cache prefix",
    );
    assert_eq!(row.source, EvidenceSource::Live);
    assert_eq!(row.phase, FailurePhase::F1);
    assert_eq!(row.confirmations, ENVELOPE_QUORUM);
    assert_eq!(
        row.required_quorum,
        Some(ENVELOPE_QUORUM),
        "the required count travels with the actual one, so a reader can tell \
         sufficiency from shortfall without knowing a code constant",
    );
    assert_eq!(row.blocked_reason, None);
    assert_eq!(
        row.blocked_reason_token(),
        BLOCKED_REASON_NONE,
        "the unblocked state is spelled explicitly: an absent field would read as \
         an unavailable panel, and this is the state in which traffic is rewritten",
    );
    assert_eq!(row.canary, CanaryPosture::Counting);
    assert_eq!(
        row.canary_remaining_requests, CANARY_INTERVAL,
        "a freshly seeded identity has consumed none of its interval",
    );
    assert_eq!(row.canary_last_outcome, None);
    assert_eq!(row.canary_outcome_token(), CANARY_OUTCOME_NONE);
    assert_eq!(row.requests_in_flight, 0);
    assert_eq!(row.unconfirmed_requests, 0);
}

/// A resident verdict with NO acknowledged confirmation reports `not_eligible`.
///
/// The base case an operator hits most: a verdict minted by one reactive repair
/// whose confirmation has not been acknowledged is resident and inert, and a
/// surface that listed only acting rows would answer "why is pre-flight not
/// firing" with silence.
#[test]
fn an_unacknowledged_verdict_reports_not_eligible() {
    let router = bare_router();
    plant_verdict(&router, grounded_key());

    let rows = router.field_verdict_status();

    let row = only_row(&rows);
    assert_eq!(
        row.blocked_reason,
        Some(PreflightBlockedReason::NotEligible),
        "no acknowledged confirmation means nothing authorizes a rewrite",
    );
    assert_eq!(row.confirmations, 0);
    assert_eq!(
        row.canary_remaining_requests, CANARY_INTERVAL,
        "an identity with no resident canary state has its whole interval left -- \
         reporting zero would read as due now",
    );
}

/// A canary-SUSPENDED verdict reports the suspension, not the eligibility arm it
/// also fails.
///
/// The distinction is the whole reason this token exists: a suspension is a
/// verdict a canary proved WRONG, whose durable clear may be failing, while the
/// rest of the not-eligible arm is a verdict not yet known to be right. The
/// planner folds the two together because its decision is the same either way;
/// the operator action is not.
///
/// Mutation check: drop the suspension arm from `blocked_reason_for` and this
/// goes red reporting `not_eligible`, which is exactly the report that would
/// hide a failing clear.
#[test]
fn a_canary_suspended_verdict_reports_the_suspension_rather_than_ineligibility() {
    let router = bare_router();
    plant_verdict(&router, grounded_key());
    seed_confirmations(&router, ENVELOPE_QUORUM);
    let key = grounded_identity(&router);
    let incarnation = router
        .field_verdicts()
        .canaries()
        .snapshot(&key)
        .expect("the seed is resident")
        .incarnation;
    // A disproof is what suspends pre-flight, and it goes through the registry's
    // own settlement rather than a hand-set flag.
    router
        .field_verdicts()
        .canaries()
        .claim_canary(&key, incarnation)
        .expect("claim admitted")
        .settle(CanaryOutcome::Regressed);

    let rows = router.field_verdict_status();

    let row = only_row(&rows);
    assert_eq!(
        row.blocked_reason,
        Some(PreflightBlockedReason::CanarySuspended),
        "a disproved verdict is reported as suspended, never as merely ineligible",
    );
    assert_eq!(
        row.blocked_reason_token(),
        "canary_suspended",
        "and its token is distinct from every planner gate token",
    );
    assert_eq!(
        row.canary_last_outcome,
        Some(CanaryOutcome::Regressed),
        "the settled outcome is reported beside the suspension it caused",
    );
    assert_eq!(row.canary_outcome_token(), "regressed");
}

/// A resident verdict whose path this build's closed table does not carry
/// reports an ABSENT class and no required quorum.
///
/// This is what a verdict persisted by a build with a WIDER table looks like.
/// Reported absent rather than guessed: the class decides which gates apply and
/// carries the prefix-impact cost, so inventing one would misreport both what the
/// verdict is waiting on and what it costs.
#[test]
fn a_verdict_outside_the_closed_table_reports_no_class_and_no_quorum() {
    let router = bare_router();
    let foreign = crate::field_capability::field_capability_key("some.future.envelope.field")
        .expect("a well-formed path outside the table");
    plant_verdict(&router, foreign.clone());

    let rows = router.field_verdict_status();

    let row = only_row(&rows);
    assert_eq!(row.capability_key, foreign);
    assert_eq!(row.transform_class, None);
    assert_eq!(row.required_quorum, None);
    assert!(
        !row.prefix_impacting,
        "an unknown class claims no prefix cost it cannot substantiate",
    );
}

/// An ACKNOWLEDGED, acting, incarnation-matched verdict whose class this build does
/// not carry is reported BLOCKED, never unblocked.
///
/// The regression, and the value it replaces was the most misleading one this field
/// can carry: a verdict nothing in this build can act on was reported as actively
/// rewriting traffic. The state is a real one -- a cold rebuild replays a ledger this
/// build did not necessarily write, so a row whose path a wider build's closed table
/// carried arrives resident, acting, and fully confirmed here with no row to act on.
///
/// Every eligibility fact is satisfied on purpose: the entry is acting, its canary
/// state matches its incarnation, and the confirmation count clears the envelope
/// quorum. So the ONLY thing blocking it is the absent class, which is exactly the
/// arm under test.
///
/// Mutation check: restore `let class = class?` -> red here, reporting `None`.
#[test]
fn an_acknowledged_verdict_with_a_foreign_transform_class_is_blocked_not_unblocked() {
    let router = bare_router();
    let foreign = crate::field_capability::field_capability_key("some.future.envelope.field")
        .expect("a well-formed path outside this build's table");
    plant_verdict(&router, foreign.clone());
    // Acknowledged at the envelope quorum through the cold-rebuild seed -- the same
    // path a real boot uses -- so this row is eligible in every respect but its class.
    let provider_kind = router.provider_kind_for_state_key(STATE_KEY).to_string();
    let key = crate::field_verdict::FieldVerdictKey::from_capability_key(
        STATE_KEY.to_string(),
        foreign.clone(),
        provider_kind.clone(),
    );
    let incarnation = router.learned_capabilities.resident_incarnation_for_tests(
        STATE_KEY,
        &foreign,
        &provider_kind,
    );
    router
        .field_verdicts()
        .canaries()
        .seed_from_rebuild(&key, incarnation, ENVELOPE_QUORUM, false);

    let rows = router.field_verdict_status();

    let row = only_row(&rows);
    assert_eq!(
        row.confirmations, ENVELOPE_QUORUM,
        "premise: the verdict IS acknowledged, so the block below is attributable to \
         the absent class alone rather than to a confirmation shortfall",
    );
    assert_eq!(row.transform_class, None, "premise: the class is foreign");
    assert_eq!(
        row.blocked_reason,
        Some(PreflightBlockedReason::NotEligible),
        "a verdict with no row in THIS build's closed table is unactionable, so it is \
         reported blocked. Reporting it unblocked would say a foreign-key row from a \
         wider build's ledger is actively rewriting traffic",
    );
    assert_ne!(
        row.blocked_reason_token(),
        "none",
        "and its token is not the unblocked one",
    );
}

/// A verdict an operator `[capability.overrides]` cell FORCES SUPPORTED reports the
/// mask, ahead of every other reason.
///
/// Matching the planner's own precedence, which checks the mask first and for the same
/// reason: the operator has said to send this field, so a learned verdict does not act
/// whatever its evidence. The distinct token matters because it is the one blocked
/// reason whose remedy is a CONFIG edit -- an operator reading `not_eligible` for a
/// masked cell would hunt for missing confirmations that could never unblock it.
///
/// The verdict is acknowledged at quorum here, so the mask is demonstrably OUTRANKING
/// a verdict that would otherwise be unblocked rather than merely coinciding with one
/// that was already refused.
///
/// Mutation checks: drop the mask arm -> red, reporting `none`; move it after the
/// suspension or eligibility checks -> still red here, because this fixture is
/// otherwise unblocked.
#[test]
fn a_force_supported_override_is_reported_as_the_mask_ahead_of_every_other_reason() {
    use crate::config::{ModelEntry, OverrideEntry, ProviderEntry};
    let mut config = Config::default();
    config.providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api("env://K"),
    );
    config.models.insert(
        STATE_KEY.to_string(),
        ModelEntry::new("anthropic", "claude-sonnet-4-5"),
    );
    // Keyed on the PROVIDER tier: the model resolves through `anthropic`, so the
    // provider-tier spec matches the override.
    config.capability.overrides.insert(
        "anthropic".to_string(),
        OverrideEntry {
            unsupported: Vec::new(),
            force_supported: vec![grounded_key()],
        },
    );
    let router = Router::new(Arc::new(config));
    plant_verdict(&router, grounded_key());
    seed_confirmations(&router, ENVELOPE_QUORUM);

    let rows = router.field_verdict_status();

    let row = only_row(&rows);
    assert_eq!(
        row.confirmations, ENVELOPE_QUORUM,
        "premise: acknowledged at quorum, so without the mask this row would be \
         UNBLOCKED -- which is what makes the mask's precedence observable",
    );
    assert_eq!(
        row.blocked_reason,
        Some(PreflightBlockedReason::MaskedByOverride),
        "the operator mask is reported, ahead of eligibility and quorum",
    );
    assert_eq!(row.blocked_reason_token(), "masked_by_override");
}

/// The paired control: with NO override configured, the same fixture is unblocked.
///
/// Without it, a mask check that fired unconditionally would satisfy the test above
/// while reporting every verdict as masked -- and pre-flight would look permanently
/// disabled on the status surface.
#[test]
fn without_an_override_the_same_fixture_is_unblocked() {
    let router = bare_router();
    plant_verdict(&router, grounded_key());
    seed_confirmations(&router, ENVELOPE_QUORUM);

    let rows = router.field_verdict_status();

    assert_eq!(
        only_row(&rows).blocked_reason,
        None,
        "no override means nothing masks the verdict, so it is unblocked",
    );
}

/// A catalog-scoped learned negative is a different namespace and produces no
/// row at all.
///
/// The filter's control: a surface that reported every learned negative as a
/// field verdict would attribute pre-flight gates to catalog capabilities that
/// have none.
#[test]
fn a_catalog_scoped_negative_produces_no_fidelity_row() {
    let router = bare_router();
    plant_verdict(&router, "web_search".to_string());

    assert!(
        router.field_verdict_status().is_empty(),
        "only the field namespace has pre-flight gates to report",
    );
}

// ---------------------------------------------------------------------------
// Canary posture and the exposure counts
// ---------------------------------------------------------------------------

/// A DUE identity reports `due` and a claimed one reports `in_flight`.
///
/// Asserted as a progression on one identity rather than two fixtures, because
/// the states are sequential: the countdown trips, then a claim consumes the
/// trip. The claim CLEARS the due flag in the same critical section that takes
/// it, so the two are never both set -- which is why this test pins the two
/// postures and not a precedence between them. That the flag is genuinely gone
/// after the claim is asserted here directly, since it is the fact the
/// posture's arm order would otherwise have to arbitrate.
#[test]
fn the_canary_posture_reports_due_then_in_flight_as_the_claim_is_taken() {
    let router = bare_router();
    plant_verdict(&router, grounded_key());
    seed_confirmations(&router, ENVELOPE_QUORUM);
    let key = grounded_identity(&router);
    let incarnation = router
        .field_verdicts()
        .canaries()
        .snapshot(&key)
        .expect("resident")
        .incarnation;
    // Walk the countdown to its trip through the registry's own tick, so the due
    // flag is set by the mechanism production uses.
    for _ in 0..CANARY_INTERVAL {
        router
            .field_verdicts()
            .canaries()
            .tick_cadence(&key, incarnation);
    }

    let due = router.field_verdict_status();
    assert_eq!(
        only_row(&due).canary,
        CanaryPosture::Due,
        "a tripped countdown no claim has consumed reports due",
    );

    let claim = router
        .field_verdicts()
        .canaries()
        .claim_canary(&key, incarnation)
        .expect("a due identity admits a claim");

    let claimed = router
        .field_verdicts()
        .canaries()
        .snapshot(&key)
        .expect("resident");
    assert!(
        claimed.canary_claimed && !claimed.due,
        "premise: the claim consumed the due flag, so the two states are exclusive \
         and the posture has no ambiguous pair to arbitrate",
    );
    let in_flight = router.field_verdict_status();
    assert_eq!(
        only_row(&in_flight).canary,
        CanaryPosture::InFlight,
        "a claimed canary reports in_flight",
    );
    claim.release();
}

/// The two exposure counts report separately: requests currently applying the
/// repair, and requests modified since the last confirmation.
///
/// Driven to DIFFERENT values deliberately -- one guard dropped, two held -- so
/// neither assertion can pass on the other's number. The two mean opposite
/// things: the in-flight count falls as requests finish, while the unconfirmed
/// tally only rises within an incarnation, because a finished request does not
/// undo the fact that the verdict modified it.
#[test]
fn the_in_flight_and_unconfirmed_counts_are_reported_separately() {
    let router = bare_router();
    plant_verdict(&router, grounded_key());
    seed_confirmations(&router, ENVELOPE_QUORUM);
    let key = grounded_identity(&router);
    let incarnation = router
        .field_verdicts()
        .canaries()
        .snapshot(&key)
        .expect("resident")
        .incarnation;

    // One request that finished: it leaves the tally but not the in-flight count.
    drop(
        router
            .field_verdicts()
            .canaries()
            .begin_modified_request(&key, incarnation)
            .expect("accounts at the resident incarnation"),
    );
    let _held_a = router
        .field_verdicts()
        .canaries()
        .begin_modified_request(&key, incarnation)
        .expect("accounts at the resident incarnation");
    let _held_b = router
        .field_verdicts()
        .canaries()
        .begin_modified_request(&key, incarnation)
        .expect("accounts at the resident incarnation");

    let rows = router.field_verdict_status();

    let row = only_row(&rows);
    assert_eq!(
        row.requests_in_flight, 2,
        "the in-flight count reports only requests still outstanding",
    );
    assert_eq!(
        row.unconfirmed_requests, 3,
        "the unconfirmed tally reports every request modified since the last \
         confirmation, including the one that finished",
    );
}

// ---------------------------------------------------------------------------
// Purity
// ---------------------------------------------------------------------------

/// Reading status mutates NOTHING an operator or a dispatch can observe.
///
/// THE purity test, and the only one: the source-text guard that used to sit beside
/// it -- a fixed list of mutator names scanned out of the module's text -- is gone.
/// It was brittle in both directions. It fired on prose that merely explained why a
/// write would be wrong, and it could not see a mutation reached through any name
/// its list did not happen to carry, which is every future one.
///
/// What replaces it is a WIDE observation. Every piece of state a status-triggered
/// write could touch is captured before and after, and compared as a whole:
///
/// - the per-identity canary state (cadence countdown, sticky due flag, claim,
///   in-flight count, wrong-repair tally, suspension, last outcome, incarnation),
///   which is what a cadence tick, a claim, or modified-request accounting moves;
/// - the LEARNED registry's own snapshot, which is what an observe, a clear, a
///   lapse-into-probe, or an incarnation bump moves;
/// - the probe scheduler's snapshot, which is what an activation, a lease, or a
///   settlement moves;
/// - the registry's SNAPSHOT-READ count, so a read that grew its own work is visible
///   even when the state it read is unchanged.
///
/// Every mutating entry point on any of those three reaches at least one of these
/// observations by construction, which is what makes this cover names a list could
/// not anticipate.
///
/// Read FIFTY times rather than once: a mutation that consumed one unit per call is
/// caught by any comparison, but repeating also catches one that only fires on a
/// later pass (a lazily initialized slot, a memoized row).
#[test]
fn repeated_status_reads_mutate_no_observable_state() {
    let router = bare_router();
    plant_verdict(&router, grounded_key());
    seed_confirmations(&router, ENVELOPE_QUORUM);
    let key = grounded_identity(&router);

    let observe = || {
        (
            router.field_verdicts().canaries().snapshot(&key),
            // Sorted, because a registry snapshot's order is a hash-map order and an
            // unsorted comparison would flag a reordering as a mutation.
            {
                let mut rows: Vec<String> = router
                    .learned_capability_snapshot()
                    .into_iter()
                    .map(|entry| {
                        format!(
                            "{}|{}|{:?}",
                            entry.state_key, entry.feature_key, entry.verdict
                        )
                    })
                    .collect();
                rows.sort_unstable();
                rows
            },
            {
                let probes = router.probe_scheduler_snapshot();
                (
                    probes.queued,
                    probes.in_flight,
                    probes.backing_off,
                    probes.activations_total,
                    probes.resolved_total,
                    probes.stale_settlements_total,
                    probes.tombstoned,
                )
            },
        )
    };

    let before = observe();
    // Measured AFTER the first observation, so the baseline already includes that
    // observation's own snapshot read -- and the expected delta below is derived from
    // the loop count plus the second observation rather than hardcoded, which is what
    // keeps it correct if `observe` ever reads more than once.
    let reads_before = router.learned_capabilities.snapshot_reads();
    const STATUS_READS: usize = 50;
    for _ in 0..STATUS_READS {
        let rows = router.field_verdict_status();
        assert_eq!(rows.len(), 1, "each read reports the planted verdict");
    }
    let reads_after_status = router.learned_capabilities.snapshot_reads();
    let after = observe();

    assert_eq!(
        before, after,
        "fifty status reads leave the canary state, the learned registry, and the \
         probe scheduler exactly as they were: a status poll runs every few seconds, so \
         any write here re-verifies verdicts and claims slots on the operator's refresh \
         interval rather than on traffic",
    );
    // The read count is asserted SEPARATELY, because it necessarily changes -- the
    // status read is itself a snapshot read. What must hold is that it grew by exactly
    // the reads the status calls make and no more, so a read that internally
    // re-snapshotted is visible even against unchanged state.
    let reads = reads_after_status - reads_before;
    assert_eq!(
        reads, STATUS_READS,
        "each status read takes exactly ONE learned snapshot: {reads} reads over \
         {STATUS_READS} calls means one of them read twice, which is how two lists on \
         one line come to describe two different registries",
    );
}

// The remaining sections live in `include!`d fragments so no one compiled file grows
// past the repo's size ceiling. ONE `#[path] mod tests` plus `include!`s is the shape
// the convention requires: a second `#[path] mod` would RENAME every test it moved
// (the path segment comes from the `mod` name, not from `#[path]`), so a named-test
// inventory would report the whole set removed and re-added.
//
// Fragments carry no top-level `use`: the imports live in this host, above.
include!("fidelity_status_vocab_tests.rs");
include!("fidelity_status_coherence_tests.rs");
include!("fidelity_status_cost_tests.rs");
include!("fidelity_status_pooled_tests.rs");
include!("fidelity_status_convergence_tests.rs");
