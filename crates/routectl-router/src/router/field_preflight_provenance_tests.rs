// The authorization-provenance record: what one pre-flight decision reports
// about the evidence that permitted it, and how both diagnostic tiers render it.
//
// An `include!`d FRAGMENT of `field_preflight_tests.rs`, not a module of its own:
// the host's imports and fixture helpers stay in scope and every test here keeps
// its original fully qualified name. Carries no top-level `use` for that reason.
//
// # Why every field carries a DISTINCT value here
//
// The record has five fields and four of them are `&'static str`. Planted at the
// fixture's defaults, two of them would read `live` and a swap between them would
// be invisible; the confirmation count and the required quorum would both read
// one. So each test below plants values that cannot be confused for each other --
// a `probe`-sourced F2 verdict at a confirmation count above every quorum, with a
// settled canary outcome -- and asserts each field against the planted value
// rather than a literal restated at the assertion site.
//
// # The mutation checks each test names
//
// Deleting any one field from the emitter, or swapping two of them, must turn one
// of these red. That is stated per test rather than in the aggregate, because a
// test that only asserted "five fields are present" would survive every swap.

// ---------------------------------------------------------------------------
// Planting a verdict whose provenance is distinguishable
// ---------------------------------------------------------------------------

/// The confirmation count these tests plant: above BOTH class quorums, and not
/// equal to either, so an assertion on it cannot be satisfied by a value read
/// off a quorum constant instead.
const DISTINCT_CONFIRMATIONS: u32 = 7;

/// Plant a resident ACTING envelope verdict for `state_key` whose provenance is
/// deliberately UNLIKE the fixture default on every axis the record reports.
///
/// F2 rather than F1 and `Probe` rather than `Live`, because those are the two
/// halves a swapped-field emitter would render identically at the defaults: the
/// default verdict is F1/Live, and `f1` is also what a phase field would read if
/// it were wired to a constant. Both remain ACTING -- `is_acting` keys on the
/// self-identifying tier, and the routing decision routes away for every negative
/// except the F3+Live advisory combination -- so this plants a verdict the planner
/// genuinely acts on rather than one it refuses for an unrelated reason.
fn plant_distinguishable_provenance(router: &Router, state_key: &str) {
    let stamped = Instant::now();
    router
        .learned_capabilities
        .import_entries(vec![crate::learned_capability::ExportedEntry {
            state_key: state_key.to_string(),
            feature_key: grounded_key(),
            verdict: crate::learned_capability::EntryVerdict::Negative,
            signal: routectl_core::capability::SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: stamped,
            last_seen: stamped,
            expires_at: stamped + NOT_LAPSED,
            phase: routectl_core::capability::FailurePhase::F2,
            source: routectl_core::capability::EvidenceSource::Probe,
            in_flight: false,
            consecutive_failed_probes: 0,
            evidence_class: None,
        }]);
    let incarnation = resident_incarnation(router, state_key);
    acknowledge(router, state_key, incarnation, DISTINCT_CONFIRMATIONS);
    assert!(
        router.field_verdicts().preflight_eligible(
            &verdict_key(state_key),
            router.registry_generation(),
            Instant::now(),
        ),
        "fixture premise: {state_key} must be pre-flight eligible on this \
         provenance -- an F2/Probe negative acts exactly as an F1/Live one does",
    );
}

/// The authorization record on `decision`, or a failure naming the decision that
/// carried none.
fn authorization_of(decision: &FieldPreflight) -> super::FieldPreflightAuthorizationRecord {
    decision
        .authorization
        .unwrap_or_else(|| panic!("this decision must carry an authorization record: {decision:?}"))
}

// ---------------------------------------------------------------------------
// The record on an acting decision
// ---------------------------------------------------------------------------

#[test]
fn an_acting_decision_records_the_provenance_that_authorized_it() {
    // THE PROVENANCE PIN. Every value is asserted against the PLANTED fact,
    // and each planted fact is distinguishable from every other field's: a
    // `probe`-sourced F2 verdict at seven confirmations cannot have its phase
    // read as its source, or its count read off a quorum constant.
    //
    // Mutation checks, each turning THIS test red:
    //   - delete `phase` from `authorization_record` -> the phase assertion fails
    //     to compile, and stubbing it to a literal fails the assertion;
    //   - swap `phase` and `source` -> both assertions fail (f2 != probe);
    //   - read `confirmations` from the class quorum instead of the
    //     authorization -> 1 != 7.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_distinguishable_provenance(&router, "m0");

    let (_planned, decision) = plan(&router, &req_on(ALIAS), "m0");

    assert!(decision.acted, "premise: this fixture's row acts");
    let record = authorization_of(&decision);
    assert_eq!(
        record.phase,
        routectl_core::capability::FailurePhase::F2.as_str(),
        "the record must report the PLANTED phase, not a constant",
    );
    assert_eq!(
        record.source,
        routectl_core::capability::EvidenceSource::Probe.as_str(),
        "and the PLANTED evidence source, distinguishable from the phase",
    );
    assert_eq!(
        record.confirmations, DISTINCT_CONFIRMATIONS,
        "and the acknowledged count the authorization rested on, which is above \
         every class quorum so it cannot be one of those instead",
    );
    assert_eq!(
        record.canary,
        crate::router::CanaryPosture::Counting.as_str(),
        "a freshly-seeded identity is counting down, neither due nor in flight",
    );
    assert_eq!(
        record.canary_last_outcome, "none",
        "no canary has settled for this incarnation, and the absent case is \
         SPELLED rather than left as a missing field",
    );
}

#[test]
fn every_refusal_carries_no_authorization_record() {
    // THE PAIRED CONTROL, and the one that makes the record meaningful: a refusal
    // never held an authorization, so reporting one would attribute permission to
    // a decision that had none. Driven over three refusals whose gates sit at
    // different depths -- no row present at all, a row with no resident verdict,
    // and a row masked by an operator override -- so the absence is a property of
    // refusing rather than of one gate's code path.
    //
    // Mutation check: fill `authorization` in `unchanged_record` from a fresh
    // registry read -> red on whichever arm finds a resident verdict.
    let (router, _seen) = single_seat(Answer::ServeImmediately);

    // No closed-table row present: the planner has nothing to consider.
    let (_planned, decision) = plan(&router, &req_without_field(ALIAS), "m0");
    assert_eq!(decision.reason, FIELD_PREFLIGHT_NO_GROUNDED_FIELD);
    assert!(
        decision.authorization.is_none(),
        "a decision that considered no row held no authorization: {decision:?}",
    );

    // A row present, but no resident verdict to authorize anything.
    let (_planned, decision) = plan(&router, &req_on(ALIAS), "m0");
    assert_eq!(decision.reason, FIELD_PREFLIGHT_NOT_ELIGIBLE);
    assert!(
        decision.authorization.is_none(),
        "an ineligible row held no authorization: {decision:?}",
    );

    // A resident, eligible verdict the operator has MASKED. This is the arm that
    // discriminates: the authorization facts ARE readable from the registry here,
    // so a record filled from a later read would be populated -- and must not be.
    let (router, _seen) = install(
        config_with_force_supported("p0"),
        1,
        Answer::ServeImmediately,
    );
    plant_distinguishable_provenance(&router, "m0");
    let (_planned, decision) = plan(&router, &req_on(ALIAS), "m0");
    assert_eq!(
        decision.reason, FIELD_PREFLIGHT_MASKED_BY_OVERRIDE,
        "premise: the mask is the operative refusal here",
    );
    assert!(
        decision.authorization.is_none(),
        "a masked cell's facts are READABLE, which is exactly why the record must \
         stay absent: the mask means nothing authorized this: {decision:?}",
    );
}

#[test]
fn a_canary_restoration_records_the_provenance_it_spent_on_re_verification() {
    // A canary is NOT a refusal, and this is where the distinction is observable:
    // the authorization permitted an action, and the planner spent it on a
    // re-verification rather than a rewrite. An operator reading a restored row
    // needs to know which evidence the identity under test rests on, so the
    // provenance is reported even though `acted` is false.
    //
    // Mutation check: return a plain `unchanged_record` from `restored_decision`
    // (dropping the authorization) -> red here while every refusal test above
    // stays green.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_distinguishable_provenance(&router, "m0");
    // The cadence is forced due through the cold-rebuild seed's own
    // `due_immediately` flag, so the next eligible request claims the canary.
    let resident = router
        .field_verdicts()
        .canaries()
        .snapshot(&verdict_key("m0"))
        .expect("premise: the planted verdict has resident canary state");
    router.field_verdicts().canaries().seed_from_rebuild(
        &verdict_key("m0"),
        resident.incarnation,
        resident.confirmations,
        true,
    );

    let target = target_for(&router, "m0", "p0");
    let (_planned, records, plan) =
        router.plan_field_preflight(&req_on(ALIAS), &target, DispatchSurface::Complete);

    let decision = envelope_decision(&records);
    assert_eq!(
        decision.reason, FIELD_PREFLIGHT_CANARY_RESTORED,
        "premise: a due cadence must restore rather than rewrite",
    );
    assert!(
        !decision.acted,
        "premise: a restoration changes nothing, so it did not act",
    );
    let record = authorization_of(decision);
    assert_eq!(
        record.confirmations, DISTINCT_CONFIRMATIONS,
        "the restoration reports the acknowledged evidence the identity under \
         test rests on",
    );
    assert_eq!(
        record.phase,
        routectl_core::capability::FailurePhase::F2.as_str(),
    );
    assert_eq!(
        record.canary,
        crate::router::CanaryPosture::Counting.as_str(),
        "the posture is the one the AUTHORIZATION was decided against, which \
         precedes both the cadence tick and the claim this same call then takes \
         -- so a restoration reports the posture that permitted it, not the one \
         it produced. Reporting the post-claim posture would mean re-reading the \
         snapshot after the gates cleared, which is exactly the read this record \
         exists to avoid",
    );
    drop(plan);
}

#[test]
fn a_canary_already_in_flight_is_reported_as_such_on_the_next_requests_record() {
    // The posture field's NON-DEFAULT value, and the case that makes it carry
    // information: a canary claimed by an earlier request is still in flight when
    // the next request's authorization is read, so that request's record reports
    // `in_flight` rather than `counting`.
    //
    // Without this, every posture assertion in this file reads `counting` and a
    // field hardwired to that token would pass all of them.
    //
    // Mutation check: hardcode `canary` to the counting token -> red here while
    // every other posture assertion above stays green.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_distinguishable_provenance(&router, "m0");
    let resident = router
        .field_verdicts()
        .canaries()
        .snapshot(&verdict_key("m0"))
        .expect("premise: resident canary state");
    router.field_verdicts().canaries().seed_from_rebuild(
        &verdict_key("m0"),
        resident.incarnation,
        resident.confirmations,
        true,
    );
    let target = target_for(&router, "m0", "p0");

    // The first request CLAIMS the canary and its plan is HELD, so the claim is
    // still outstanding while the second request plans.
    let (_planned, first_records, held) =
        router.plan_field_preflight(&req_on(ALIAS), &target, DispatchSurface::Complete);
    assert_eq!(
        envelope_decision(&first_records).reason,
        FIELD_PREFLIGHT_CANARY_RESTORED,
        "premise: the first request must hold the claim",
    );

    let (_planned, records, plan) =
        router.plan_field_preflight(&req_on(ALIAS), &target, DispatchSurface::Complete);

    let decision = envelope_decision(&records);
    assert!(
        decision.acted,
        "premise: a request that cannot claim the canary rewrites normally",
    );
    assert_eq!(
        authorization_of(decision).canary,
        crate::router::CanaryPosture::InFlight.as_str(),
        "an identity whose canary is outstanding reports in_flight, so an \
         operator reading this rewrite knows a re-verification is already running \
         for it",
    );
    drop(plan);
    drop(held);
}

#[test]
fn a_settled_canary_outcome_reaches_the_next_requests_record() {
    // The last-outcome half, which the fresh-seed case above can only show as
    // absent. A settled INCONCLUSIVE outcome is the one a dropped plan produces,
    // so this drives the production settlement rather than writing the field.
    //
    // Mutation check: hardcode `canary_last_outcome` to the absent token -> red
    // here while the fresh-seed assertion above stays green.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_distinguishable_provenance(&router, "m0");
    let resident = router
        .field_verdicts()
        .canaries()
        .snapshot(&verdict_key("m0"))
        .expect("premise: resident canary state");
    router.field_verdicts().canaries().seed_from_rebuild(
        &verdict_key("m0"),
        resident.incarnation,
        resident.confirmations,
        true,
    );
    let target = target_for(&router, "m0", "p0");

    // First request claims the canary; dropping its plan settles INCONCLUSIVE,
    // which is what an abandoned re-verification IS.
    let (_planned, _records, plan) =
        router.plan_field_preflight(&req_on(ALIAS), &target, DispatchSurface::Complete);
    drop(plan);

    // The NEXT request's record carries that settled outcome.
    let (_planned, records, plan) =
        router.plan_field_preflight(&req_on(ALIAS), &target, DispatchSurface::Complete);
    let decision = envelope_decision(&records);
    assert!(
        decision.acted,
        "premise: the claim was released, so this request rewrites",
    );
    assert_eq!(
        authorization_of(decision).canary_last_outcome,
        "inconclusive",
        "the record reports the outcome the PREVIOUS interval settled, through \
         the status surface's own token table",
    );
    drop(plan);
}

// ---------------------------------------------------------------------------
// Both diagnostic tiers render the record
// ---------------------------------------------------------------------------

#[test]
fn the_per_decision_debug_line_renders_every_provenance_field() {
    // The DEBUG tier, which is where an operator correlating a WARN lands. Each
    // field is asserted against the PLANTED value, so deleting one from the
    // emitter reds this test and swapping two reds two assertions.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_distinguishable_provenance(&router, "m0");
    let (_planned, records) = plan_all(&router, &req_on(ALIAS), "m0");
    let mut meta = super::super::DispatchMeta::for_alias(ALIAS);
    meta.field_preflight = records;

    let events = routectl_testkit::capture_events(|| {
        super::emit_field_preflight(&meta);
    });

    let debugs: Vec<&routectl_testkit::CapturedEvent> = events
        .iter()
        .filter(|e| e.level == tracing::Level::DEBUG)
        .collect();
    assert_eq!(
        debugs.len(),
        1,
        "premise: one considered row is one DEBUG line: {events:?}",
    );
    let line = debugs[0];
    assert_eq!(
        line.field("provenance_phase"),
        Some(routectl_core::capability::FailurePhase::F2.as_str()),
    );
    assert_eq!(
        line.field("provenance_source"),
        Some(routectl_core::capability::EvidenceSource::Probe.as_str()),
    );
    assert_eq!(
        line.field("confirmations"),
        Some(DISTINCT_CONFIRMATIONS.to_string().as_str()),
    );
    assert_eq!(
        line.field("canary"),
        Some(crate::router::CanaryPosture::Counting.as_str()),
    );
    assert_eq!(line.field("canary_last_outcome"), Some("none"));
}

#[test]
fn the_one_request_warn_renders_the_headline_decisions_provenance() {
    // ONE WARN per modified request, carrying the HEADLINE decision's own
    // provenance -- not an aggregate across the acting set. The two rows here have
    // DIFFERENT confirmation counts on purpose: an emitter that summed, averaged,
    // or took the wrong row's count would report a number neither identity has.
    //
    // The prefix row is the headline (higher impact), so the WARN must report the
    // PREFIX row's count, and the envelope row's distinct count is what makes that
    // attributable.
    //
    // Mutation checks: read the provenance off `acted[0]` instead of `headline`
    // -> the envelope count appears and this reds; drop the WARN's provenance
    // fields -> the assertions find them absent.
    let (router, _seen) = install(config_with_opt_in(&["p0"]), 1, Answer::ServeImmediately);
    plant_distinguishable_provenance(&router, "m0");
    // The prefix row's own count, distinct from the envelope row's seven and from
    // both quorums, so the headline attribution is unambiguous.
    let prefix_confirmations = DISTINCT_CONFIRMATIONS + 4;
    plant_prefix_verdict(&router, "m0", prefix_confirmations);

    let (_planned, records) = plan_all(&router, &req_both_rows(ALIAS), "m0");
    assert_eq!(
        records.iter().filter(|r| r.acted).count(),
        2,
        "premise: both rows act under this fixture, so the WARN has a headline to \
         choose: {records:?}",
    );
    let mut meta = super::super::DispatchMeta::for_alias(ALIAS);
    meta.field_preflight = records;

    let events = routectl_testkit::capture_events(|| {
        super::emit_field_preflight(&meta);
    });

    let warns: Vec<&routectl_testkit::CapturedEvent> = events
        .iter()
        .filter(|e| {
            e.level == tracing::Level::WARN && e.message == super::FIELD_PREFLIGHT_WARN_MESSAGE
        })
        .collect();
    assert_eq!(warns.len(), 1, "one modified request is one WARN");
    let line = warns[0];
    assert_eq!(
        line.field("transform_class"),
        Some(super::super::field_repair::TransformClass::PrefixImpacting.as_str()),
        "premise: the prefix row is the headline, being the higher-impact one",
    );
    assert_eq!(
        line.field("confirmations"),
        Some(prefix_confirmations.to_string().as_str()),
        "so the WARN reports the HEADLINE row's own count, not the envelope \
         row's and not an aggregate of the two",
    );
    assert_eq!(
        line.field("provenance_phase"),
        Some(routectl_core::capability::FailurePhase::F1.as_str()),
        "and the headline row's own phase -- the prefix verdict is planted F1 \
         while the envelope one is F2, so a wrong-row read is visible here too",
    );
}

#[test]
fn no_provenance_field_carries_a_request_value_or_a_body() {
    // THE LOG-HYGIENE PIN for the five new fields. Every one is a closed-set
    // token, a count, or the spelled absent token, so a request value could only
    // reach a line through a field this asserts on. The fixture carries a
    // distinctive value in BOTH envelope carriers and in the system prompt, and no
    // emitted field may contain any of them.
    //
    // A positive control runs first: the fixture's marker must be findable in the
    // request itself, or "not found in the log" would be free.
    let (router, _seen) = install(config_with_opt_in(&["p0"]), 1, Answer::ServeImmediately);
    plant_distinguishable_provenance(&router, "m0");
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);
    let mut original = req_both_rows(ALIAS);
    let marker = "provenance-leak-canary-value";
    original.routectl_internal.anthropic_thinking_display = Some(marker.to_string());
    assert!(
        original
            .routectl_internal
            .anthropic_thinking_display
            .as_deref()
            == Some(marker),
        "positive control: the marker must actually be in the request, or a \
         not-found assertion below would be vacuous",
    );

    let (_planned, records) = plan_all(&router, &original, "m0");
    let mut meta = super::super::DispatchMeta::for_alias(ALIAS);
    meta.field_preflight = records;

    let events = routectl_testkit::capture_events(|| {
        super::emit_field_preflight(&meta);
    });

    assert!(
        !events.is_empty(),
        "positive control: the emitter must have produced lines, or the sweep \
         below would pass over nothing",
    );
    for event in &events {
        for field in [
            "provenance_phase",
            "provenance_source",
            "confirmations",
            "canary",
            "canary_last_outcome",
        ] {
            if let Some(value) = event.field(field) {
                assert!(
                    !value.contains(marker),
                    "{field} carried a request value: {value}",
                );
                assert!(
                    !value.contains(SYSTEM_PROMPT),
                    "{field} carried the system prompt: {value}",
                );
            }
        }
    }
}
