use super::*;
use crate::learned_capability::{DEFAULT_MAX_ENTRIES, RoutingDecision};
use std::time::Duration;

const CV: u32 = 7;
const OV: u64 = 42;

/// A reader that hands back a fixed tombstone and row set, ignoring nothing
/// (the rebuild owns filtering and ordering).
struct FakeReader {
    tombstone: Option<ReplayTombstone>,
    rows: Vec<CapabilityEventRow>,
}

impl CapabilityLedgerReader for FakeReader {
    fn tombstone(&self) -> Option<ReplayTombstone> {
        self.tombstone
    }

    fn read_events(&self) -> Vec<CapabilityEventRow> {
        self.rows.clone()
    }
}

#[allow(clippy::too_many_arguments)]
fn row(
    rowid: i64,
    at: Instant,
    verdict: &str,
    phase: Option<&str>,
    source: &str,
    tier: Option<&str>,
    evidence_class: Option<&str>,
    capability: &str,
) -> CapabilityEventRow {
    CapabilityEventRow::new(
        rowid,
        at,
        verdict.to_string(),
        phase.map(str::to_string),
        source.to_string(),
        tier.map(str::to_string),
        evidence_class.map(str::to_string),
        capability.to_string(),
        "nn".to_string(),
        "openai-compat".to_string(),
        CV,
        OV,
    )
}

/// A `broken` (F1 self-identifying, live) row -- the common negative shape,
/// which carries no evidence class.
fn broken(rowid: i64, at: Instant, capability: &str) -> CapabilityEventRow {
    row(
        rowid,
        at,
        "broken",
        Some("f1"),
        "live",
        Some("self-identifying"),
        None,
        capability,
    )
}

/// A `cleared` (live) row.
fn cleared(rowid: i64, at: Instant, capability: &str) -> CapabilityEventRow {
    row(rowid, at, "cleared", None, "live", None, None, capability)
}

/// A `broken` (F1 self-identifying, probe) row -- the probe-sourced negative.
fn probe_broken(rowid: i64, at: Instant, capability: &str) -> CapabilityEventRow {
    row(
        rowid,
        at,
        "broken",
        Some("f1"),
        "probe",
        Some("self-identifying"),
        None,
        capability,
    )
}

/// A `cleared` (probe) row.
fn probe_cleared(rowid: i64, at: Instant, capability: &str) -> CapabilityEventRow {
    row(rowid, at, "cleared", None, "probe", None, None, capability)
}

fn registry() -> LearnedCapabilityRegistry {
    // A large decay keeps every replayed negative acting at query time so the
    // ordering assertions read the replayed state, not a lapse.
    LearnedCapabilityRegistry::new(
        Duration::from_hours(1),
        Duration::from_mins(1),
        DEFAULT_MAX_ENTRIES,
    )
}

#[test]
fn should_replay_skips_rows_at_or_before_the_tombstone() {
    let base = Instant::now();
    let tombstone = ReplayTombstone::new(5, CV, OV);

    let at_boundary = broken(5, base, "x");
    let before = broken(3, base, "x");
    let after = broken(6, base, "x");

    assert_eq!(
        should_replay(&at_boundary, &tombstone),
        ReplayDecision::SkipBoundary
    );
    assert_eq!(
        should_replay(&before, &tombstone),
        ReplayDecision::SkipBoundary
    );
    assert_eq!(should_replay(&after, &tombstone), ReplayDecision::Replay);
}

#[test]
fn should_replay_skips_post_tombstone_stragglers_of_a_different_revision() {
    let base = Instant::now();
    let tombstone = ReplayTombstone::new(5, CV, OV);

    let mut stale_catalog = broken(6, base, "x");
    stale_catalog.catalog_version = CV + 1;
    let mut stale_overlay = broken(7, base, "x");
    stale_overlay.overlay_revision = OV + 1;

    // Post-tombstone rows do NOT unconditionally replay: a straggler stamped
    // with a different revision is skipped.
    assert_eq!(
        should_replay(&stale_catalog, &tombstone),
        ReplayDecision::SkipRevision
    );
    assert_eq!(
        should_replay(&stale_overlay, &tombstone),
        ReplayDecision::SkipRevision
    );
}

/// The cold-replay layer: the revision comparison applies only to
/// catalog-scoped keys, so a wire-shape row stamped with a superseded
/// revision still replays.
///
/// Mixed batch on purpose: the two rows differ only in their capability
/// key, so an unconditional revision guard skips both and an unconditional
/// bypass replays both. Only the predicate separates them.
#[test]
fn should_replay_bypasses_the_revision_guard_for_field_keys_only() {
    let base = Instant::now();
    let tombstone = ReplayTombstone::new(5, CV, OV);
    let field_key = crate::field_capability::field_capability_key("thinking.enabled.display")
        .expect("a qualified dotted path mints a key");

    let mut stale_field = broken(6, base, &field_key);
    stale_field.catalog_version = CV + 1;
    let mut stale_catalog_scoped = broken(7, base, "web_search");
    stale_catalog_scoped.catalog_version = CV + 1;

    assert_eq!(
        should_replay(&stale_field, &tombstone),
        ReplayDecision::Replay,
        "a wire-shape fact does not depend on the catalog revision",
    );
    assert_eq!(
        should_replay(&stale_catalog_scoped, &tombstone),
        ReplayDecision::SkipRevision,
        "a catalog-scoped fact under a superseded revision is still evicted",
    );
}

/// The rowid boundary is unconditional: it applies to a wire-shape key
/// exactly as it does to a catalog-scoped one, because it guards against
/// double-replaying an event this boot has already accounted for -- a
/// guarantee the field namespace needs and the revision guard is not.
#[test]
fn should_replay_applies_the_rowid_boundary_to_field_keys() {
    let base = Instant::now();
    let tombstone = ReplayTombstone::new(5, CV, OV);
    let field_key = crate::field_capability::field_capability_key("thinking.enabled.display")
        .expect("a qualified dotted path mints a key");

    // Both at-or-before the boundary, one of them also revision-stale, so
    // the rowid rule is what decides in each case.
    let mut at_boundary = broken(5, base, &field_key);
    at_boundary.catalog_version = CV + 1;
    let before = broken(4, base, &field_key);
    let after = broken(6, base, &field_key);

    assert_eq!(
        should_replay(&at_boundary, &tombstone),
        ReplayDecision::SkipBoundary
    );
    assert_eq!(
        should_replay(&before, &tombstone),
        ReplayDecision::SkipBoundary
    );
    assert_eq!(should_replay(&after, &tombstone), ReplayDecision::Replay);
}

/// The revision skip is counted, and separately from the rowid and
/// unrecognized-token skips -- without that an operator cannot tell "no
/// wire-shape verdicts existed" from "every one of them was evicted".
#[test]
fn rebuild_counts_revision_skips_apart_from_boundary_and_unknown_skips() {
    let base = Instant::now();
    let field_key = crate::field_capability::field_capability_key("thinking.enabled.display")
        .expect("a qualified dotted path mints a key");

    let mut stale_catalog_scoped = broken(2, base, "web_search");
    stale_catalog_scoped.catalog_version = CV + 1;
    let mut stale_overlay_scoped = broken(3, base, "computer_use");
    stale_overlay_scoped.overlay_revision = OV + 1;
    let mut stale_field = broken(4, base, &field_key);
    stale_field.catalog_version = CV + 1;

    let reader = FakeReader {
        tombstone: Some(ReplayTombstone::new(1, CV, OV)),
        rows: vec![
            // At the boundary: a rowid skip, not a revision skip.
            broken(1, base, "prompt_caching"),
            stale_catalog_scoped,
            stale_overlay_scoped,
            stale_field,
            // Post-boundary, current revision, unrecognized verdict token.
            row(5, base, "nonsense", None, "live", None, None, "thinking"),
        ],
    };
    let reg = registry();

    let summary = rebuild_capabilities_into(&reader, &reg);

    assert_eq!(
        summary.skipped_revision, 2,
        "both catalog-scoped stragglers are counted, the field one is not",
    );
    assert_eq!(summary.skipped_unknown, 1);
    assert_eq!(
        summary.replayed_negative, 1,
        "only the revision-stale wire-shape row replayed",
    );
    assert!(matches!(
        reg.acting_negative_for(
            "nn",
            &field_key,
            "openai-compat",
            base + Duration::from_secs(1)
        ),
        RoutingDecision::RouteAway { .. },
    ));
}

#[test]
fn negative_then_cleared_ts_ordered_clears_the_negative() {
    let base = Instant::now();
    let reader = FakeReader {
        tombstone: Some(ReplayTombstone::new(0, CV, OV)),
        rows: vec![
            // Delivered out of ts order to prove the rebuild sorts.
            cleared(2, base + Duration::from_secs(2), "web_search"),
            broken(1, base + Duration::from_secs(1), "web_search"),
        ],
    };
    let reg = registry();

    let summary = rebuild_capabilities_into(&reader, &reg);

    assert_eq!(summary.replayed_negative, 1);
    assert_eq!(summary.replayed_cleared, 1);
    assert_eq!(
        reg.acting_negative_for(
            "nn",
            "web_search",
            "openai-compat",
            base + Duration::from_secs(3)
        ),
        RoutingDecision::Allow,
    );
}

#[test]
fn cleared_then_negative_ts_ordered_leaves_the_negative_acting() {
    let base = Instant::now();
    let reader = FakeReader {
        tombstone: Some(ReplayTombstone::new(0, CV, OV)),
        rows: vec![
            cleared(1, base + Duration::from_secs(1), "web_search"),
            broken(2, base + Duration::from_secs(2), "web_search"),
        ],
    };
    let reg = registry();

    let summary = rebuild_capabilities_into(&reader, &reg);

    // The cleared event finds nothing resident (a no-op); the later negative
    // then acts -- deterministic under ts ordering.
    assert_eq!(summary.cleared_noop, 1);
    assert_eq!(summary.replayed_negative, 1);
    assert!(matches!(
        reg.acting_negative_for(
            "nn",
            "web_search",
            "openai-compat",
            base + Duration::from_secs(3)
        ),
        RoutingDecision::RouteAway { .. },
    ));
}

#[test]
fn same_instant_rows_tie_break_by_rowid() {
    let base = Instant::now();
    let at = base + Duration::from_secs(1);
    let query = base + Duration::from_secs(2);

    // negative(rowid=1) + cleared(rowid=2) at the SAME instant, inserted
    // cleared-first: rowid order -- not vec order -- must place the negative
    // before the cleared, so the cleared removes it -> Allow.
    let reader = FakeReader {
        tombstone: Some(ReplayTombstone::new(0, CV, OV)),
        rows: vec![cleared(2, at, "web_search"), broken(1, at, "web_search")],
    };
    let reg = registry();
    let summary = rebuild_capabilities_into(&reader, &reg);
    assert_eq!(summary.replayed_negative, 1);
    assert_eq!(summary.replayed_cleared, 1);
    assert_eq!(
        reg.acting_negative_for("nn", "web_search", "openai-compat", query),
        RoutingDecision::Allow,
    );

    // Reverse the rowids at the same instant: cleared(rowid=1) sorts before
    // negative(rowid=2), so the cleared no-ops and the negative acts.
    let reader = FakeReader {
        tombstone: Some(ReplayTombstone::new(0, CV, OV)),
        rows: vec![broken(2, at, "web_search"), cleared(1, at, "web_search")],
    };
    let reg = registry();
    let summary = rebuild_capabilities_into(&reader, &reg);
    assert_eq!(summary.cleared_noop, 1);
    assert_eq!(summary.replayed_negative, 1);
    assert!(matches!(
        reg.acting_negative_for("nn", "web_search", "openai-compat", query),
        RoutingDecision::RouteAway { .. },
    ));
}

#[test]
fn verified_row_replays_as_a_positive() {
    let base = Instant::now();
    let reader = FakeReader {
        tombstone: Some(ReplayTombstone::new(0, CV, OV)),
        rows: vec![row(
            1,
            base,
            "verified",
            None,
            "live",
            None,
            Some("search_blocks"),
            "web_search",
        )],
    };
    let reg = registry();

    let summary = rebuild_capabilities_into(&reader, &reg);

    assert_eq!(summary.replayed_verified, 1);
    assert!(reg.is_verified_working(
        "nn",
        "web_search",
        "openai-compat",
        base + Duration::from_secs(1)
    ));
}

#[test]
fn suspect_row_with_f3_phase_replays_as_a_negative() {
    let base = Instant::now();
    let reader = FakeReader {
        tombstone: Some(ReplayTombstone::new(0, CV, OV)),
        rows: vec![row(
            1,
            base,
            "suspect",
            Some("f3"),
            "live",
            Some("inferred"),
            Some("schema_mismatch"),
            "structured_output",
        )],
    };
    let reg = registry();

    let summary = rebuild_capabilities_into(&reader, &reg);

    assert_eq!(summary.replayed_negative, 1);
    assert_eq!(summary.skipped_unknown, 0);
}

#[test]
fn suspect_row_with_non_f3_phase_skips() {
    let base = Instant::now();
    let reader = FakeReader {
        tombstone: Some(ReplayTombstone::new(0, CV, OV)),
        rows: vec![row(
            1,
            base,
            "suspect",
            Some("f1"),
            "live",
            Some("inferred"),
            Some("schema_mismatch"),
            "structured_output",
        )],
    };
    let reg = registry();

    let summary = rebuild_capabilities_into(&reader, &reg);

    // The live path always mints suspect at F3; a suspect row carrying any
    // other phase is malformed -- skip, fail closed.
    assert_eq!(summary.replayed_negative, 0);
    assert_eq!(summary.skipped_unknown, 1);
}

#[test]
fn probe_source_rows_replay_through_the_shared_arms() {
    let base = Instant::now();
    let reader = FakeReader {
        tombstone: Some(ReplayTombstone::new(0, CV, OV)),
        rows: vec![row(
            1,
            base,
            "broken",
            Some("f1"),
            "probe",
            Some("self-identifying"),
            None,
            "web_search",
        )],
    };
    let reg = registry();

    let summary = rebuild_capabilities_into(&reader, &reg);

    // A probe negative replays through the same admission the live path uses:
    // the by-verdict counter and the by-source probe tally both bump, and the
    // negative acts (RouteAway).
    assert_eq!(summary.replayed_negative, 1);
    assert_eq!(summary.replayed_probe, 1);
    assert!(matches!(
        reg.acting_negative_for(
            "nn",
            "web_search",
            "openai-compat",
            base + Duration::from_secs(1)
        ),
        RoutingDecision::RouteAway { .. },
    ));
}

#[test]
fn probe_cleared_row_removes_a_resident_negative() {
    let base = Instant::now();
    let reader = FakeReader {
        tombstone: Some(ReplayTombstone::new(0, CV, OV)),
        rows: vec![
            // A resident probe negative, then a later probe-sourced clear:
            // the clear now reaches the source-agnostic cleared arm and
            // removes the negative.
            probe_broken(1, base + Duration::from_secs(1), "web_search"),
            probe_cleared(2, base + Duration::from_secs(2), "web_search"),
        ],
    };
    let reg = registry();

    let summary = rebuild_capabilities_into(&reader, &reg);

    assert_eq!(summary.replayed_negative, 1);
    assert_eq!(summary.replayed_cleared, 1);
    // Both probe rows counted in the by-source tally alongside their
    // by-verdict counters.
    assert_eq!(summary.replayed_probe, 2);
    assert_eq!(
        reg.acting_negative_for(
            "nn",
            "web_search",
            "openai-compat",
            base + Duration::from_secs(3)
        ),
        RoutingDecision::Allow,
    );
}

#[test]
fn equal_ts_live_and_probe_rows_tie_break_by_rowid() {
    let base = Instant::now();
    let at = base + Duration::from_secs(1);
    let query = base + Duration::from_secs(2);

    // A probe negative (rowid=1) and a live clear (rowid=2) at the SAME
    // instant, delivered clear-first: rowid order -- not vec order -- places
    // the negative before the clear across sources, so the clear removes it.
    let reader = FakeReader {
        tombstone: Some(ReplayTombstone::new(0, CV, OV)),
        rows: vec![
            cleared(2, at, "web_search"),
            probe_broken(1, at, "web_search"),
        ],
    };
    let reg = registry();
    let summary = rebuild_capabilities_into(&reader, &reg);
    assert_eq!(summary.replayed_negative, 1);
    assert_eq!(summary.replayed_cleared, 1);
    assert_eq!(summary.replayed_probe, 1);
    assert_eq!(
        reg.acting_negative_for("nn", "web_search", "openai-compat", query),
        RoutingDecision::Allow,
    );

    // Reverse the rowids at the same instant across sources: live clear
    // (rowid=1) sorts before the probe negative (rowid=2), so the clear
    // no-ops and the probe negative acts.
    let reader = FakeReader {
        tombstone: Some(ReplayTombstone::new(0, CV, OV)),
        rows: vec![
            probe_broken(2, at, "web_search"),
            cleared(1, at, "web_search"),
        ],
    };
    let reg = registry();
    let summary = rebuild_capabilities_into(&reader, &reg);
    assert_eq!(summary.cleared_noop, 1);
    assert_eq!(summary.replayed_negative, 1);
    assert_eq!(summary.replayed_probe, 1);
    assert!(matches!(
        reg.acting_negative_for("nn", "web_search", "openai-compat", query),
        RoutingDecision::RouteAway { .. },
    ));
}

#[test]
fn unknown_tokens_skip_without_panic() {
    let base = Instant::now();
    let reader = FakeReader {
        tombstone: Some(ReplayTombstone::new(0, CV, OV)),
        rows: vec![
            // Unknown verdict.
            row(1, base, "teleported", None, "live", None, None, "a"),
            // Unknown source.
            row(
                2,
                base,
                "broken",
                Some("f1"),
                "martian",
                Some("self-identifying"),
                None,
                "b",
            ),
            // Unknown tier.
            row(
                3,
                base,
                "broken",
                Some("f1"),
                "live",
                Some("psychic"),
                None,
                "c",
            ),
            // Missing phase.
            row(
                4,
                base,
                "broken",
                None,
                "live",
                Some("self-identifying"),
                None,
                "d",
            ),
            // Verified with a NULL evidence class (the live path always sets
            // one).
            row(5, base, "verified", None, "live", None, None, "e"),
            // Verified with an unrecognized evidence class.
            row(
                6,
                base,
                "verified",
                None,
                "live",
                None,
                Some("bogus_class"),
                "f",
            ),
            // Suspect with an unrecognized evidence class.
            row(
                7,
                base,
                "suspect",
                Some("f3"),
                "live",
                Some("inferred"),
                Some("bogus_class"),
                "g",
            ),
        ],
    };
    let reg = registry();

    let summary = rebuild_capabilities_into(&reader, &reg);

    // Every malformed token -- verdict, source, tier, phase, evidence class --
    // skips its row, none replay, and nothing panics.
    assert_eq!(summary.skipped_unknown, 7);
    assert_eq!(summary.replayed_probe, 0);
    assert_eq!(summary.replayed_negative, 0);
    assert_eq!(summary.replayed_verified, 0);
}

#[test]
fn no_tombstone_replays_nothing() {
    let base = Instant::now();
    let reader = FakeReader {
        tombstone: None,
        rows: vec![broken(1, base, "web_search")],
    };
    let reg = registry();

    let summary = rebuild_capabilities_into(&reader, &reg);

    assert_eq!(summary, CapabilityRebuildSummary::default());
    assert_eq!(
        reg.acting_negative_for(
            "nn",
            "web_search",
            "openai-compat",
            base + Duration::from_secs(1)
        ),
        RoutingDecision::Allow,
    );
}

/// Replay precedence is APPEND order (rowid), never the mapped instant.
///
/// The reader derives `observed_at` from the persisted wall-clock `ts`, so a
/// clock rollback between two appends can map a later-appended row to an
/// earlier instant. Sorting by instant would then replay a settled
/// negative-then-cleared pair backwards and leave the negative resident --
/// resurrecting a verdict the clear had settled. The fixture inverts instant
/// order against rowid order on purpose, so an instant-sorted implementation
/// cannot pass.
#[test]
fn replay_precedence_follows_rowid_when_mapped_instants_disagree() {
    let base = Instant::now();
    let late = base + Duration::from_mins(1);

    // The negative is appended FIRST (rowid 1) but carries the LATER instant;
    // the clear is appended second (rowid 2) with the earlier instant.
    let reader = FakeReader {
        tombstone: Some(ReplayTombstone::new(0, CV, OV)),
        rows: vec![
            broken(1, late, "web_search"),
            cleared(2, base, "web_search"),
        ],
    };
    let reg = registry();

    let summary = rebuild_capabilities_into(&reader, &reg);

    // Append order wins: the clear ran last and removed the negative.
    assert_eq!(summary.replayed_negative, 1);
    assert_eq!(
        summary.replayed_cleared, 1,
        "the clear must find the negative resident, i.e. replay after it",
    );
    assert_eq!(summary.cleared_noop, 0);
    assert_eq!(
        reg.acting_negative_for("nn", "web_search", "openai-compat", late),
        RoutingDecision::Allow,
        "a settled clear must not be inverted by a clock rollback",
    );
}

/// An INFERRED negative's acting state is decided by how many observations
/// replay, so the row count a restatement emits is a routing decision.
///
/// One row replays as a single pending observation -- resident, and NOT acting.
/// Two replay as corroborated, which acts. This is the router-owned half of the
/// restatement contract: it pins the mechanism on the real replay path
/// (`rebuild_capabilities_into`), while the CLI's real-ledger test pins that the
/// boundary actually emits the right number of rows end to end.
///
/// Owned here rather than in the CLI because the assertion is a `RoutingDecision`
/// -- a routing-internal type that should not become public API just to be named
/// by an acceptance test in another crate.
#[test]
fn one_replayed_inferred_row_stays_pending_while_two_act() {
    let base = Instant::now();
    let query = base + Duration::from_secs(1);

    /// An inferred (not self-identifying) F1 negative -- the shape that needs
    /// corroboration before it acts.
    fn inferred(rowid: i64, at: Instant, capability: &str) -> CapabilityEventRow {
        row(
            rowid,
            at,
            "broken",
            Some("f1"),
            "live",
            Some("inferred"),
            None,
            capability,
        )
    }

    // A single restated row: pending, so routing must still ALLOW.
    let one = FakeReader {
        tombstone: Some(ReplayTombstone::new(0, CV, OV)),
        rows: vec![inferred(1, base, "web_search")],
    };
    let reg = registry();
    let summary = rebuild_capabilities_into(&one, &reg);
    assert_eq!(summary.replayed_negative, 1);
    assert_eq!(
        reg.acting_negative_for("nn", "web_search", "openai-compat", query),
        RoutingDecision::Allow,
        "one inferred observation must replay as PENDING and not route away",
    );

    // Two restated rows: corroborated, so routing must ROUTE AWAY.
    let two = FakeReader {
        tombstone: Some(ReplayTombstone::new(0, CV, OV)),
        rows: vec![
            inferred(1, base, "web_search"),
            inferred(2, base + Duration::from_millis(1), "web_search"),
        ],
    };
    let reg = registry();
    let summary = rebuild_capabilities_into(&two, &reg);
    assert_eq!(summary.replayed_negative, 2);
    assert!(
        matches!(
            reg.acting_negative_for("nn", "web_search", "openai-compat", query),
            RoutingDecision::RouteAway { .. }
        ),
        "two inferred observations must replay as CORROBORATED and route away",
    );
}
