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
        ReplayDecision::Skip
    );
    assert_eq!(should_replay(&before, &tombstone), ReplayDecision::Skip);
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
        ReplayDecision::Skip
    );
    assert_eq!(
        should_replay(&stale_overlay, &tombstone),
        ReplayDecision::Skip
    );
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
fn probe_source_rows_skip_with_a_counter() {
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

    assert_eq!(summary.skipped_probe, 1);
    assert_eq!(summary.replayed_negative, 0);
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
    assert_eq!(summary.skipped_probe, 0);
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
