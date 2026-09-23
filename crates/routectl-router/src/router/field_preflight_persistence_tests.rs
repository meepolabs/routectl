// The capability-persistence gate on the planner: learned pre-flight suspends
// while durable capability writes cannot be guaranteed, and reactive
// forward-and-repair keeps serving.
//
// An `include!`d FRAGMENT of `field_preflight_tests.rs`, not a module of its own:
// the host's imports and fixture helpers stay in scope and every test here keeps
// its original fully qualified name. Carries no top-level `use` for that reason.
//
// # Why these fixtures OMIT the durability opt-in
//
// The host's `install` states the durability assumption for every other test in
// this file, so a test here that needs the SUSPENDED path builds its router
// through `install` and then strips the assumption -- which it cannot do, since
// the installation is a consuming builder with no remover. So these fixtures
// construct the Router the way production does (`Router::new` plus a resolved
// table, and nothing else) and the gate they exercise is the shipped default
// rather than an installed `false`.

/// The host's raw fixture WITHOUT the durability assumption: production's own default.
///
/// A named wrapper over the shared `install_raw` body rather than a copy of it, and
/// named rather than a flag on `install`: threading a boolean through would put the
/// durability assumption of every test in the host file behind a parameter nobody
/// reads at the call site, which is exactly the shape that lets a test exercise the
/// suspended planner while claiming to exercise a quorum.
fn install_without_durable_writes(
    config: Config,
    seats: usize,
    answer: Answer,
) -> (Router, Vec<Arc<Observed>>) {
    install_raw(config, seats, answer)
}

/// A one-seat fixture with NO capability-persistence health read.
fn single_seat_without_durable_writes() -> (Router, Arc<Observed>) {
    let (router, mut observed) = install_without_durable_writes(
        chain_config(ALIAS, 1, ANTHROPIC),
        1,
        Answer::ServeImmediately,
    );
    (router, observed.remove(0))
}

// ---------------------------------------------------------------------------
// The gate itself
// ---------------------------------------------------------------------------

#[test]
fn an_eligible_verdict_does_not_preflight_while_capability_writes_are_unguaranteed() {
    // THE PERSISTENCE PIN. The verdict is fully eligible -- resident, acting, acknowledged,
    // past every quorum -- and the ONLY difference from the acting fixture is that
    // durable capability persistence cannot be guaranteed. Pre-flight must refuse,
    // because every escape hatch it depends on is a capability-event write: a
    // verdict that turns out to be wrong could not be durably retracted, and the
    // next boot's replay would restore it.
    //
    // A POSITIVE CONTROL runs second on the same planting: the identical fixture
    // WITH the durability assumption acts. Without it, "did not act" would be free
    // on any fixture that could never act at all.
    //
    // Mutation check: delete the `capability_writes_durable` gate from
    // `preflight_authorization_for` -> red here.
    let (router, _seen) = single_seat_without_durable_writes();
    plant_eligible(&router, "m0");
    let original = req_on(ALIAS);

    let (planned, decision) = plan(&router, &original, "m0");

    assert!(
        !decision.acted,
        "an eligible verdict must not rewrite while its durable retraction cannot \
         be guaranteed: {decision:?}",
    );
    assert_eq!(
        decision.reason,
        super::FIELD_PREFLIGHT_WRITER_UNHEALTHY,
        "and it must report the WRITER, not a confirmation shortfall -- an \
         operator sent after missing evidence would never find any",
    );
    assert!(
        carries_field(&planned),
        "the planned body must forward the field exactly as the client sent it",
    );

    // THE CONTROL: the same planting on a fixture that assumes durable writes.
    let (healthy, _seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&healthy, "m0");
    let (planned, decision) = plan(&healthy, &original, "m0");
    assert!(
        decision.acted,
        "control: this planting DOES act once durable writes are assumed, so the \
         refusal above is attributable to the gate rather than to the fixture",
    );
    assert!(!carries_field(&planned));
}

#[test]
fn the_persistence_gate_is_reported_behind_the_operator_mask() {
    // GATE ORDER, and it matters for the operator's next action: a masked cell's
    // fate does not change when the writer recovers, because the operator has said
    // to send the field. Reporting the writer there would send them to fix
    // something that would not help.
    //
    // Mutation check: move the persistence gate above the mask -> red here.
    let (router, _seen) = install_without_durable_writes(
        config_with_force_supported("p0"),
        1,
        Answer::ServeImmediately,
    );
    plant_eligible(&router, "m0");

    let (_planned, decision) = plan(&router, &req_on(ALIAS), "m0");

    assert_eq!(
        decision.reason, FIELD_PREFLIGHT_MASKED_BY_OVERRIDE,
        "the mask is the operative reason at any writer health",
    );
}

#[test]
fn the_persistence_gate_is_reported_ahead_of_every_evidence_gate() {
    // The other half of the order. An UNACKNOWLEDGED verdict on an unhealthy
    // writer must report the writer, not the shortfall: the shortfall is real but
    // it is not what is standing between this lane and pre-flight, and telling an
    // operator to gather confirmations on an unhealthy writer sends them after
    // evidence the acknowledgment path cannot record anyway.
    //
    // Mutation check: move the persistence gate below the eligibility read ->
    // `not_eligible` comes back and this reds.
    let (router, _seen) = single_seat_without_durable_writes();
    // Resident and ACTING but never acknowledged: `not_eligible` is what a healthy
    // writer would report for this state, asserted as the control below.
    plant_verdict(&router, "m0", false);

    let (_planned, decision) = plan(&router, &req_on(ALIAS), "m0");

    assert_eq!(
        decision.reason,
        super::FIELD_PREFLIGHT_WRITER_UNHEALTHY,
        "the writer outranks the evidence gates, because it refuses the MECHANISM \
         rather than this verdict's evidence",
    );

    // THE CONTROL: the same planting with durable writes assumed reports the
    // evidence gate, which is what makes the ordering above observable rather
    // than a fixture that could only ever report one token.
    let (healthy, _seen) = single_seat(Answer::ServeImmediately);
    plant_verdict(&healthy, "m0", false);
    let (_planned, decision) = plan(&healthy, &req_on(ALIAS), "m0");
    assert_eq!(
        decision.reason, FIELD_PREFLIGHT_NOT_ELIGIBLE,
        "control: a healthy writer reports the real confirmation shortfall",
    );
}

#[test]
fn a_suspended_preflight_leaves_reactive_forward_and_repair_available() {
    // The other half, and the reason the suspension is safe rather than an
    // outage: the reactive arm still admits a repair on the same identity, on the
    // same unhealthy-writer router. It acts only AFTER an upstream rejection, so
    // its evidence is in hand on the request it serves and it needs no durable
    // record to be correct -- a dropped event costs it the chance to act
    // proactively next time, nothing more.
    //
    // Mutation check: apply the `capability_writes_durable` gate to
    // `plan_field_carry` as well -> red here, which is what stops the gate from
    // being widened into a full capability-learning kill switch.
    //
    // The verdict is planted LAPSED rather than acting, and that is about the
    // reactive arm's own rules rather than about this gate: its admission refuses
    // an ACTING verdict by design (reacting to a verdict already routing traffic
    // is a routing decision, not a repair), so an acting planting would refuse for
    // a documented reason of its own and prove nothing about persistence. A lapsed
    // verdict is admissible reactively -- exactly one caller gets the guard and
    // re-verifies the field -- while pre-flight refuses it either way, which is
    // what makes the two halves of this assertion independent.
    let (router, _seen) = single_seat_without_durable_writes();
    plant_verdict(&router, "m0", true);
    let original = req_on(ALIAS);

    // Premise: pre-flight IS suspended on this router.
    let (_planned, decision) = plan(&router, &original, "m0");
    assert_eq!(
        decision.reason,
        super::FIELD_PREFLIGHT_WRITER_UNHEALTHY,
        "premise: pre-flight must be suspended, or the reactive assertion below \
         would be about an ordinary router",
    );

    // The reactive admission, on the same router and the same identity. It is
    // admitted through the PRODUCTION entry point, and what it returns is a plan
    // -- so this asserts the arm is available, not merely that a predicate is true.
    let carry = router.plan_field_carry(
        &target_for(&router, "m0", "p0"),
        &original,
        super::super::field_repair::FieldSettlementMode::Settling,
        Instant::now(),
    );
    assert!(
        carry.is_some(),
        "reactive forward-and-repair must remain available while pre-flight is \
         suspended: it acts on a rejection it has in hand, so no durable record \
         is a precondition for it",
    );
}

#[test]
fn a_walk_on_an_unhealthy_writer_dispatches_the_clients_own_bytes() {
    // The END-TO-END consequence, through the real dispatch walk rather than the
    // planner alone: a suspended pre-flight must forward what the client sent. A
    // planner-only assertion cannot see a walk that recovered the rewrite from
    // some other stage.
    let (router, seen) = single_seat_without_durable_writes();
    plant_eligible(&router, "m0");
    let original = req_on(ALIAS);

    let dispatched =
        block_on_dispatch(router.complete_with_options(original.clone(), RouterOptions::default()));

    assert!(
        dispatched.meta.field_preflight.iter().all(|r| !r.acted),
        "premise: no decision on this walk may have acted: {:?}",
        dispatched.meta.field_preflight,
    );
    let dispatched_bodies = seen.dispatched.lock();
    assert_eq!(
        dispatched_bodies.len(),
        1,
        "premise: exactly one attempt reached the seat",
    );
    assert!(
        carries_field(&dispatched_bodies[0]),
        "the bytes that went upstream must still carry the field the client sent",
    );
}
