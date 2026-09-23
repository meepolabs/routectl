// Coherence and log-safety of the fidelity snapshot: one shared learned read behind
// the acting and detailed lists, and sanitization of both identity keys on both row
// types.
//
// An `include!`d FRAGMENT of `fidelity_status_tests.rs`, which is why it carries no
// top-level `use` and no `mod` of its own -- see that host's own note on why the
// convention is one `mod tests` plus includes rather than sibling `#[path]` modules.

// ---------------------------------------------------------------------------
// The coherent snapshot
// ---------------------------------------------------------------------------

/// The acting list and the detailed rows come from ONE learned read.
///
/// The property the snapshot type exists for. Four separate reads meant four
/// moments, so an acting count could describe one registry state while the detailed
/// rows described another -- and a reader reconciling "3 acting, 2 rows" could not
/// tell a documented filter from a race.
///
/// Asserted as a SUPERSET relation over one planted state rather than by
/// instrumenting the read count: the rows list reports every resident verdict while
/// the acting list applies the routing filter, so for any single snapshot the
/// acting set must be contained in the rows set by capability key. Two reads that
/// raced would break exactly that containment.
#[test]
fn the_snapshot_derives_acting_and_detailed_rows_from_one_learned_read() {
    let router = bare_router();
    plant_verdict(&router, grounded_key());
    seed_confirmations(&router, ENVELOPE_QUORUM);
    // A catalog-scoped negative too, so the two lists differ by a real filter and a
    // containment assertion is not trivially satisfied by identical sets.
    plant_verdict_on(&router, "other", "web_search".to_string());

    let snapshot = router.fidelity_snapshot();

    assert!(
        !snapshot.acting.is_empty(),
        "premise: the fixture plants an acting field verdict",
    );
    for acting in &snapshot.acting {
        assert!(
            snapshot
                .verdicts
                .iter()
                .any(|row| row.capability_key == acting.feature_key),
            "every acting verdict must appear among the detailed rows: the rows are the \
             superset, and a containment break is what two racing reads would produce",
        );
    }
    assert!(
        snapshot
            .verdicts
            .iter()
            .all(|row| !row.capability_key.contains("web_search")),
        "and the catalog-scoped negative is in NEITHER list -- so the two sets differ \
         by their documented filters, which is what makes the containment meaningful",
    );
}

/// The snapshot carries the counters and the probe state, not just the rows.
///
/// The observability floor names probe activation, queue state, last outcome, and
/// next retry; a snapshot that carried only verdicts would leave those to a second
/// read the caller had to remember.
#[test]
fn the_snapshot_carries_the_counters_and_the_probe_state() {
    let router = bare_router();
    router.metrics.incr_field_preflight_action();

    let snapshot = router.fidelity_snapshot();

    assert_eq!(
        snapshot.counters.preflight_actions, 1,
        "the counters ride the same snapshot, so a reader needs one call",
    );
    assert_eq!(
        (
            snapshot.probes.queued,
            snapshot.probes.in_flight,
            snapshot.probes.backing_off,
        ),
        (0, 0, 0),
        "and so does the scheduler's own queue state, read from the scheduler rather \
         than re-derived",
    );
    assert_eq!(
        snapshot.probes.last_settlement, None,
        "a scheduler nothing has settled on reports no last outcome -- distinct from \
         any settled one",
    );
    assert_eq!(
        snapshot.probes.next_retry_in, None,
        "and nothing backing off reports no wait, which is distinct from a zero wait",
    );
}

/// Building the snapshot mutates nothing, including the scheduler.
///
/// The purity contract extended to the coherent read: it now touches the scheduler
/// too, so a snapshot that activated a lane would make an operator's dashboard the
/// first organic evidence -- and on a paid lane, the thing that spends.
#[test]
fn repeated_snapshots_leave_the_scheduler_and_canary_state_untouched() {
    let router = bare_router();
    plant_verdict(&router, grounded_key());
    seed_confirmations(&router, ENVELOPE_QUORUM);
    let key = grounded_identity(&router);
    let before_canary = router
        .field_verdicts()
        .canaries()
        .snapshot(&key)
        .expect("resident");
    let before_probes = router.probe_scheduler_snapshot();

    for _ in 0..50 {
        let snapshot = router.fidelity_snapshot();
        assert_eq!(snapshot.verdicts.len(), 1, "each read reports the verdict");
    }

    assert_eq!(
        before_canary,
        router
            .field_verdicts()
            .canaries()
            .snapshot(&key)
            .expect("still resident"),
        "fifty coherent reads leave the cadence, claim, and exposure counts intact",
    );
    let after_probes = router.probe_scheduler_snapshot();
    assert_eq!(
        (
            before_probes.activations_total,
            before_probes.queued,
            before_probes.in_flight,
        ),
        (
            after_probes.activations_total,
            after_probes.queued,
            after_probes.in_flight,
        ),
        "and activate no lane and queue no job",
    );
}

// ---------------------------------------------------------------------------
// Sanitization of BOTH keys
// ---------------------------------------------------------------------------

/// A hostile CAPABILITY key reaches the row sanitized and length-capped.
///
/// The state key's sanitization is pinned above; this is its sibling, and it needs
/// its own case because the two are separate field assignments and sanitizing one
/// says nothing about the other. The capability key is normally a closed-table
/// token, but the registry is fed from a ledger this build did not necessarily
/// write, so the read side must not assume its shape.
///
/// Mutation check: drop `sanitize_for_log` from the `capability_key` assignment in
/// `status_row_for` -> red on the cap assertion.
#[test]
fn a_hostile_capability_key_reaches_the_row_sanitized_and_capped() {
    // Built through the namespace constructor where possible, then extended past
    // the cap: a key this shape is what a foreign or corrupted ledger row looks
    // like, which is exactly the input the read side cannot assume away.
    let hostile = format!("{}\u{1b}[31m\u{7}{}", grounded_key(), "A".repeat(4096));
    let router = bare_router();
    plant_verdict_on(&router, STATE_KEY, hostile.clone());

    let rows = router.field_verdict_status();

    let row = only_row(&rows);
    assert!(
        row.capability_key.len() < hostile.len(),
        "premise plus contract: the hostile key exceeds the sanitizer's cap, and the \
         row's key is shorter -- so the cap genuinely applied",
    );
    assert!(
        !row.capability_key.contains(&"A".repeat(1024)),
        "the row's capability key is length-capped, so one row cannot print an \
         unbounded string into an operator's log",
    );
    assert!(
        !row.capability_key.contains('\u{1b}') && !row.capability_key.contains('\u{7}'),
        "and no control byte survives: an escape sequence in a log field is how a \
         forged second line is injected",
    );
}

/// The ACTING list sanitizes both of its keys too.
///
/// A separate producer from the detailed row, so it needs its own assertion: the
/// two derive from the same learned entries but assign their fields independently.
#[test]
fn the_acting_list_sanitizes_both_of_its_keys() {
    let hostile_state = format!("m0\u{1b}[31m{}", "S".repeat(4096));
    let hostile_key = format!("{}\u{7}{}", grounded_key(), "K".repeat(4096));
    let router = bare_router();
    plant_verdict_on(&router, &hostile_state, hostile_key.clone());

    let acting = crate::router::acting_field_verdicts(&router.learned_capability_snapshot());

    assert_eq!(acting.len(), 1, "premise: the planted row is acting");
    let row = &acting[0];
    assert!(
        !row.state_key.contains(&"S".repeat(1024)) && !row.state_key.contains('\u{1b}'),
        "the acting entry's state key is sanitized and capped",
    );
    assert!(
        !row.feature_key.contains(&"K".repeat(1024)) && !row.feature_key.contains('\u{7}'),
        "and so is its capability key -- the two are separate assignments, so one \
         being safe says nothing about the other",
    );
}
