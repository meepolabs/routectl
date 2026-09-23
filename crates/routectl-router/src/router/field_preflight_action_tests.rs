// Pre-flight action counter and the one-WARN-per-request contract.
// An `include!`d FRAGMENT of `field_preflight_tests.rs`.

// ---------------------------------------------------------------------------
// The pre-flight action count
// ---------------------------------------------------------------------------

#[test]
fn an_adopted_rewrite_counts_one_preflight_action() {
    // The counter status/doctor read to answer "is pre-flight acting at all",
    // which no log level can answer. Counted at the ADOPTION -- the one site
    // that swaps the rewritten body in -- so a decision that fell open on any
    // gate contributes nothing.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    assert_eq!(
        router.field_repair_counters().preflight_actions,
        0,
        "premise: a router that has planned nothing has acted nothing",
    );

    let (_planned, decision) = plan(&router, &req_on(ALIAS), "m0");

    assert!(decision.acted, "premise: this fixture's row acts");
    assert_eq!(
        router.field_repair_counters().preflight_actions,
        1,
        "one adopted rewrite counts exactly one action",
    );
}

#[test]
fn a_fail_open_decision_counts_no_preflight_action() {
    // The paired control, and the one that makes the count meaningful: a
    // counter bumped per DECISION rather than per adoption would report a busy
    // pre-flight surface on a daemon where every request forwards unchanged --
    // which is the routine case, so the inflation would be permanent.
    let (router, _seen) = single_seat(Answer::ServeImmediately);
    plant_eligible(&router, "m0");

    let (_planned, decision) = plan(&router, &req_without_field(ALIAS), "m0");

    assert!(!decision.acted, "premise: nothing present, nothing adopted");
    assert_eq!(
        router.field_repair_counters().preflight_actions,
        0,
        "a decision that adopted nothing must not count as an action",
    );
}

#[test]
fn two_acting_rows_on_one_request_count_two_adopted_row_rewrite_actions() {
    // The counter's unit is the ADOPTED ROW REWRITE, not the request, and the two
    // differ exactly here. A request carrying rows of two classes rewrites two
    // surfaces and exposes two identities, so counting it once would understate
    // what an operator is exposed to.
    //
    // The per-REQUEST unit is the WARN, which is emitted once however many rows
    // acted -- pinned separately below. The two units are deliberately different:
    // the counter measures exposure (per identity), the WARN reports an event
    // (per request). Reading either as the other is the misattribution the
    // wording in every doc now guards against. Asserted at two
    // rather than one because the two numbers differ exactly here.
    let (router, _seen) = install(config_with_opt_in(&["p0"]), 1, Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);

    let (_planned, records) = plan_all(&router, &req_both_rows(ALIAS), "m0");

    let acted = records.iter().filter(|r| r.acted).count();
    assert_eq!(acted, 2, "premise: both rows adopt under this fixture");
    assert_eq!(
        router.field_repair_counters().preflight_actions,
        2,
        "the action count is per adopted row, not per request",
    );
}

#[test]
fn a_request_whose_two_rows_both_act_still_emits_exactly_one_warn() {
    // ONE WARN PER MODIFIED REQUEST, never one per acted row. A request is the
    // event an operator is being told about -- "routectl rewrote a client's
    // request before dispatch" -- and a per-row line would report one such event
    // as two, inflating any count taken off the log while saying nothing new.
    //
    // The counter's unit is the opposite (per adopted row), and that asymmetry is
    // the point: the aggregate counts on this very line carry the row detail, so
    // the line NAMES one decision while its counts describe all of them.
    //
    // Mutation check: move the `warn!` inside the per-record loop -> red on the
    // count here.
    let (router, _seen) = install(config_with_opt_in(&["p0"]), 1, Answer::ServeImmediately);
    plant_eligible(&router, "m0");
    plant_prefix_verdict(&router, "m0", crate::config::PREFIX_QUORUM);

    let (_planned, records) = plan_all(&router, &req_both_rows(ALIAS), "m0");
    assert_eq!(
        records.iter().filter(|r| r.acted).count(),
        2,
        "premise: both rows adopt under this fixture, so the WARN count below is \
         about a request with two acting rows",
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
    assert_eq!(
        warns.len(),
        1,
        "two acting rows on one request are ONE reportable event, so one WARN",
    );
    assert_eq!(
        warns[0].field("decisions_acted"),
        Some("2"),
        "and its aggregate count still describes BOTH acting rows -- naming one \
         decision never narrows the counts",
    );
}
