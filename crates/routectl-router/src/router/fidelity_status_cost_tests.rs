// The COST of a status read, and the one-learned-read contract stated executably.
//
// These are the two properties no correctness assertion can see: the per-row shape
// this replaces returned identical rows while taking one exclusive dispatch lock per
// row, and a second learned read returns the same entries on a static registry. Both
// are therefore pinned by instrumentation rather than by output.
//
// An `include!`d FRAGMENT of `fidelity_status_tests.rs` -- no top-level `use`, no
// `mod`; see that host's note.

// ---------------------------------------------------------------------------
// The lock cost of a status read
// ---------------------------------------------------------------------------

/// A status read takes ONE `entries` acquisition however many verdicts are
/// resident.
///
/// The property is about COST, not correctness, and no correctness assertion can
/// see it: the per-row shape this replaces returned identical rows while taking one
/// EXCLUSIVE acquisition per row -- on the same `entries` lock every dispatch needs,
/// every few seconds, on a surface an operator polls. At a thousand resident
/// verdicts that is a thousand exclusive locks per poll.
///
/// Counted through the registry's own acquire hook rather than inferred from timing,
/// which would measure the scheduler. The count is asserted CONSTANT across two
/// very different resident-set sizes rather than against a literal: what must hold
/// is that it does not grow with the row count, and pinning an exact number would
/// break on any unrelated lock the read legitimately gains.
///
/// Mutation check: restore the per-row `preflight_authorization` call in
/// `blocked_reason_for` -> red, because the count then scales with the rows.
#[test]
fn a_status_read_takes_a_constant_number_of_entries_locks() {
    /// Resident verdicts in the LARGE fixture. Well above the small one, so a
    /// per-row acquisition is unmistakable in the difference rather than a rounding
    /// artifact.
    const MANY: usize = 64;

    let acquisitions = |verdicts: usize| -> usize {
        let router = bare_router();
        for idx in 0..verdicts {
            // Distinct paths, so each mints its own resident identity rather than
            // overwriting one.
            let path = format!("thinking.enabled.display{idx}");
            plant_verdict(
                &router,
                crate::field_capability::field_capability_key(&path).expect("valid path"),
            );
        }
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sink = Arc::clone(&count);
        router
            .learned_capabilities
            .set_lock_acquire_hook(Box::new(move |lock| {
                if lock == "entries" {
                    sink.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }));

        let rows = router.field_verdict_status();

        assert_eq!(
            rows.len(),
            verdicts,
            "premise: the fixture really planted {verdicts} resident rows, so the \
             count below is over a walk that visited them",
        );
        count.load(std::sync::atomic::Ordering::SeqCst)
    };

    let few = acquisitions(1);
    let many = acquisitions(MANY);

    assert_eq!(
        few, many,
        "the `entries` acquisition count must not grow with the resident set: {few} \
         for one verdict versus {many} for {MANY}. A per-row guarded read takes an \
         EXCLUSIVE lock each time, on the lock every dispatch needs",
    );
    assert!(
        many <= 2,
        "and it stays small in absolute terms -- {many} acquisitions for {MANY} \
         verdicts. The bound is loose on purpose: what is pinned is that the cost is \
         O(1) in the row count, not an exact number a legitimate new read would break",
    );
}

// ---------------------------------------------------------------------------
// The one-learned-read contract, executably
// ---------------------------------------------------------------------------

/// `fidelity_snapshot` calls the learned snapshot EXACTLY ONCE.
///
/// The contract stated as a count rather than inferred from the source. A previous
/// version of this claim rested on reading the function body, which is not evidence:
/// a refactor that reintroduced a second read inside `verdict_rows_from` would leave
/// every other assertion green, because two reads of an unchanging registry return
/// the same entries.
///
/// Mutation check: make `verdict_rows_from` fetch its own snapshot instead of taking
/// one -> red here at two reads.
#[test]
fn the_fidelity_snapshot_reads_the_learned_registry_exactly_once() {
    let router = bare_router();
    plant_verdict(&router, grounded_key());
    let before = router.learned_capabilities.snapshot_reads();

    let _snapshot = router.fidelity_snapshot();

    assert_eq!(
        router.learned_capabilities.snapshot_reads() - before,
        1,
        "the acting list and the detailed rows come from ONE learned read: two reads \
         could disagree, and a reader reconciling their counts could not tell a \
         documented filter from a race",
    );
}

/// With each successive learned read made to answer DIFFERENTLY, the coherent
/// snapshot still reconciles -- because it only reads once.
///
/// This is what a count alone cannot establish. Two reads of a static registry
/// return identical entries, so a counting test proves the number without proving
/// that the number MATTERS. Here the instrument perturbs the registry on every read,
/// so a consumer that read twice would assemble an acting list and a row list drawn
/// from two different registries, and the containment below would break.
///
/// Mutation check: make `verdict_rows_from` fetch its own snapshot -> red on the
/// containment, because the rows then describe a registry the acting list never saw.
#[test]
fn the_coherent_snapshot_reconciles_even_when_every_read_would_answer_differently() {
    let router = bare_router();
    plant_verdict(&router, grounded_key());
    seed_confirmations(&router, ENVELOPE_QUORUM);
    // Arm the instrument: each snapshot read plants one more resident entry, so no
    // two reads see the same registry.
    let stamped = Instant::now();
    let seed = crate::learned_capability::ExportedEntry {
        state_key: STATE_KEY.to_string(),
        feature_key: grounded_key(),
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
    };
    router
        .learned_capabilities
        .perturb_each_snapshot_read_for_tests(seed.clone());
    let planted_before = router.learned_capability_snapshot().len();
    router
        .learned_capabilities
        .perturb_each_snapshot_read_for_tests(seed);
    assert!(
        router.learned_capability_snapshot().len() > planted_before,
        "premise: the instrument really does make successive reads differ, so the \
         containment below is a live check rather than a tautology",
    );

    let snapshot = router.fidelity_snapshot();

    for acting in &snapshot.acting {
        assert!(
            snapshot
                .verdicts
                .iter()
                .any(|row| row.capability_key == acting.feature_key),
            "every acting verdict appears among the detailed rows even under a \
             registry that changes on every read -- which can only hold if the two \
             lists came from ONE read. Acting: {:?}, rows: {:?}",
            snapshot.acting.len(),
            snapshot.verdicts.len(),
        );
    }
}
