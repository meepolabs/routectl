//! Coverage for the live status/doctor visibility surface: the acting
//! field-verdict provenance filter and the process-lifetime repair
//! counters snapshot.

use super::*;

use std::sync::Arc;
use std::time::Instant;

use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier, Verdict};

use crate::config::Config;
use crate::learned_capability::LearnedRegistryEntry;
use crate::router::repair_budget::REPAIRS_PER_REQUEST;

const THINKING_DISPLAY_PATH: &str = "thinking.enabled.display";
const COMPUTER_USE_DISPLAY_PATH: &str = "computer_use.display_number";

fn bare_router() -> Router {
    Router::new(Arc::new(Config::default()))
}

/// Mints a real field capability key through the sole constructor, rather
/// than spelling the complete key as a literal -- the second spelling that
/// the workspace-wide prefix-uniqueness scan exists to forbid.
fn field_key(path: &str) -> String {
    crate::field_capability::field_capability_key(path).expect("path is a valid field path")
}

fn entry(feature_key: &str, verdict: Verdict, source: EvidenceSource) -> LearnedRegistryEntry {
    let now = Instant::now();
    LearnedRegistryEntry {
        state_key: "anthropic-api:claude-sonnet-4-5".to_string(),
        feature_key: feature_key.to_string(),
        verdict,
        signal_tier: SignalTier::SelfIdentifying,
        observations: 1,
        first_seen: now,
        last_seen: now,
        expires_at: now,
        evidence_class: None,
        phase: match verdict {
            Verdict::LearnedBroken(phase) => phase,
            _ => FailurePhase::F2,
        },
        source,
        recheck_owned: false,
    }
}

/// A field-namespaced `LearnedBroken` row is the only row status/doctor call
/// "acting" -- included with its state key, feature key, phase and source
/// carried through unchanged.
#[test]
fn acting_field_verdicts_includes_a_field_namespaced_learned_broken_row() {
    let expected_key = field_key(THINKING_DISPLAY_PATH);
    let rows = vec![entry(
        &expected_key,
        Verdict::LearnedBroken(FailurePhase::F1),
        EvidenceSource::Live,
    )];

    let acting = acting_field_verdicts(&rows);

    assert_eq!(acting.len(), 1);
    assert_eq!(acting[0].state_key, "anthropic-api:claude-sonnet-4-5");
    assert_eq!(acting[0].feature_key, expected_key);
    assert_eq!(acting[0].phase, FailurePhase::F1);
    assert_eq!(acting[0].source, EvidenceSource::Live);
}

/// A catalog-scoped `LearnedBroken` row (no `field:` prefix) is a different
/// namespace entirely and must not be reported as an acting field verdict.
#[test]
fn acting_field_verdicts_excludes_a_catalog_scoped_negative() {
    let rows = vec![entry(
        "web_search",
        Verdict::LearnedBroken(FailurePhase::F2),
        EvidenceSource::Live,
    )];

    assert!(acting_field_verdicts(&rows).is_empty());
}

/// A field-namespaced row that is NOT `LearnedBroken` (a cleared verdict, a
/// verified positive) is not acting -- surfacing it would claim a drop that
/// is not happening.
#[test]
fn acting_field_verdicts_excludes_non_broken_field_rows() {
    let rows = vec![
        entry(
            &field_key(THINKING_DISPLAY_PATH),
            Verdict::Cleared,
            EvidenceSource::Live,
        ),
        entry(
            &field_key(COMPUTER_USE_DISPLAY_PATH),
            Verdict::VerifiedWorking,
            EvidenceSource::Probe,
        ),
    ];

    assert!(acting_field_verdicts(&rows).is_empty());
}

/// An empty registry snapshot reports no acting verdicts -- the base case a
/// hand-picked-only fixture could otherwise pass vacuously.
#[test]
fn acting_field_verdicts_is_empty_for_an_empty_snapshot() {
    assert!(acting_field_verdicts(&[]).is_empty());
}

/// A mixed snapshot reports exactly the acting rows, in order, leaving the
/// non-acting rows out rather than merely not crashing on them.
#[test]
fn acting_field_verdicts_filters_a_mixed_snapshot_to_only_the_acting_rows() {
    let thinking_key = field_key(THINKING_DISPLAY_PATH);
    let computer_use_key = field_key(COMPUTER_USE_DISPLAY_PATH);
    let rows = vec![
        entry(
            &thinking_key,
            Verdict::LearnedBroken(FailurePhase::F1),
            EvidenceSource::Live,
        ),
        entry(
            "web_search",
            Verdict::LearnedBroken(FailurePhase::F2),
            EvidenceSource::Live,
        ),
        entry(
            &computer_use_key,
            Verdict::LearnedBroken(FailurePhase::F3),
            EvidenceSource::Probe,
        ),
        entry(&thinking_key, Verdict::Cleared, EvidenceSource::Live),
    ];

    let acting = acting_field_verdicts(&rows);

    assert_eq!(acting.len(), 2);
    assert_eq!(acting[0].feature_key, thinking_key);
    assert_eq!(acting[1].feature_key, computer_use_key);
}

/// An F3 negative sourced from live traffic is advisory-only per the
/// learned-capability registry's own routing decision: a probe, not this
/// row alone, settles whether the target is actually routed away.
/// Status/doctor must not report it as acting.
#[test]
fn acting_field_verdicts_excludes_an_f3_live_advisory_field_row() {
    let rows = vec![entry(
        &field_key(THINKING_DISPLAY_PATH),
        Verdict::LearnedBroken(FailurePhase::F3),
        EvidenceSource::Live,
    )];

    assert!(acting_field_verdicts(&rows).is_empty());
}

/// The same F3 negative, sourced from a probe instead, carries routing
/// authority and does act -- the source is what flips the verdict, not the
/// phase alone.
#[test]
fn acting_field_verdicts_includes_an_f3_probe_field_row() {
    let expected_key = field_key(THINKING_DISPLAY_PATH);
    let rows = vec![entry(
        &expected_key,
        Verdict::LearnedBroken(FailurePhase::F3),
        EvidenceSource::Probe,
    )];

    let acting = acting_field_verdicts(&rows);

    assert_eq!(acting.len(), 1);
    assert_eq!(acting[0].feature_key, expected_key);
    assert_eq!(acting[0].phase, FailurePhase::F3);
    assert_eq!(acting[0].source, EvidenceSource::Probe);
}

/// The counters snapshot starts at zero on a freshly constructed router --
/// the base case a fixture that only ever bumps counters could not fail on.
#[test]
fn field_repair_counters_starts_at_zero() {
    let router = bare_router();

    let counters = router.field_repair_counters();

    assert_eq!(counters.repair_attempted, 0);
    assert_eq!(counters.repair_succeeded, 0);
    assert_eq!(counters.verdicts_learned, 0);
    assert_eq!(counters.outstanding_unconfirmed, 0);
    assert_eq!(counters.disproved_requests, 0);
}

/// The counters snapshot reflects the live metrics the moment they move --
/// this is the read path status/doctor rely on, so it must not lag or cache.
#[test]
fn field_repair_counters_reflects_the_live_metrics_after_they_move() {
    let router = bare_router();

    router.metrics.incr_field_repair_attempted();
    router.metrics.incr_field_repair_succeeded();
    router.metrics.incr_field_verdicts_learned();

    let counters = router.field_repair_counters();

    assert_eq!(counters.repair_attempted, 1);
    assert_eq!(counters.repair_succeeded, 1);
    assert_eq!(counters.verdicts_learned, 1);
}

/// The two wrong-repair ALARM halves come from the shared canary registry rather
/// than the metrics counters, so they need their own drive: this reads them back
/// through the same `field_repair_counters()` snapshot status/doctor call, after
/// moving the registry the router actually owns.
///
/// Both halves are asserted NONZERO and are deliberately DIFFERENT numbers, so
/// neither assertion can pass on the other's value and a snapshot that wired one
/// field to the wrong source is caught.
///
/// Mutation checks, each red on its own named assertion: hardcode
/// `outstanding_unconfirmed: 0` in `Router::field_repair_counters` -> red on the
/// outstanding assertion; hardcode `disproved_requests: 0` -> red on the
/// lifetime assertion; SWAP the two right-hand sources between the fields -> red
/// on both, which is what the differing expected values buy (equal fixtures
/// would let a swap pass).
///
/// The same swap one layer out -- between the two `rc_field_*` log field names in
/// the CLI reporter -- is invisible here and in every trace assertion, so it is
/// pinned by a source guard in that module
/// (`each_alarm_log_field_reads_the_counter_it_is_named_for`).
#[test]
fn field_repair_counters_reports_both_nonzero_alarm_halves() {
    let router = bare_router();
    let canaries = router.field_verdicts().canaries();
    let disproved_key = crate::field_verdict::FieldVerdictKey::new(
        &crate::state_key::StateKey::new("anthropic-api", "claude-sonnet-4-5").expect("lane"),
        THINKING_DISPLAY_PATH,
        "anthropic-api",
    )
    .expect("a qualified path mints a key");
    let live_key = crate::field_verdict::FieldVerdictKey::new(
        &crate::state_key::StateKey::new("anthropic-api", "claude-opus-4-1").expect("lane"),
        COMPUTER_USE_DISPLAY_PATH,
        "anthropic-api",
    )
    .expect("a qualified path mints a key");

    // Identity one: three repaired requests, then a canary DISPROVES the verdict,
    // transferring all three into the lifetime alarm and leaving it holding none.
    for _ in 0..3 {
        drop(
            canaries
                .begin_modified_request(&disproved_key, 1)
                .expect("accounts at the resident incarnation"),
        );
    }
    canaries
        .claim_due_canary(&disproved_key, 1)
        .expect("claim admitted")
        .settle(crate::field_canary::CanaryOutcome::Regressed);
    // Identity two: two repaired requests nothing has disproved, so these stay
    // OUTSTANDING. A different count from the three above on purpose.
    for _ in 0..2 {
        drop(
            canaries
                .begin_modified_request(&live_key, 1)
                .expect("accounts at the resident incarnation"),
        );
    }

    let counters = router.field_repair_counters();

    assert_eq!(
        counters.outstanding_unconfirmed, 2,
        "the CURRENT half reports the exposure no canary has settled, and does \
         not include the requests the disproof already transferred out",
    );
    assert_eq!(
        counters.disproved_requests, 3,
        "the LIFETIME half reports every request the disproved verdict had \
         modified -- requests affected, not canary attempts",
    );
}

/// The pre-flight ACTION count and the parser-unlocalized count are reported
/// beside the repair halves, and start at zero on a fresh router.
///
/// Their presence at zero is the contract: an absent field on the status
/// surface reads as an unavailable panel rather than as "nothing happened", and
/// a pre-flight gate an operator cannot see acting is one they cannot audit.
#[test]
fn field_repair_counters_starts_the_preflight_and_parser_halves_at_zero() {
    let router = bare_router();

    let counters = router.field_repair_counters();

    assert_eq!(
        counters.preflight_actions, 0,
        "a router that dispatched nothing has applied no pre-flight rewrite",
    );
    assert_eq!(
        counters.parser_unlocalized, 0,
        "and has seen no rejection its parser could not localize",
    );
}

/// Each of the two new halves reads its OWN counter.
///
/// Driven to DIFFERENT non-zero values deliberately: equal values would let a
/// snapshot that wired both fields to one source pass, and the two mean
/// unrelated things -- one counts requests routectl rewrote before dispatch,
/// the other counts rejections it could not attribute to a field at all.
///
/// Mutation checks: hardcode `preflight_actions: 0` -> red on the first
/// assertion; hardcode `parser_unlocalized: 0` -> red on the second; SWAP the
/// two right-hand sources -> red on both.
#[test]
fn field_repair_counters_reports_the_preflight_and_parser_halves_separately() {
    let router = bare_router();

    for _ in 0..4 {
        router.metrics.incr_field_preflight_action();
    }
    for _ in 0..7 {
        router.metrics.incr_parser_unlocalized();
    }

    let counters = router.field_repair_counters();

    assert_eq!(
        counters.preflight_actions, 4,
        "the pre-flight half counts requests a pre-flight rewrite modified",
    );
    assert_eq!(
        counters.parser_unlocalized, 7,
        "and the parser half counts rejections no field path was localized from",
    );
}

// ---------------------------------------------------------------------------
// The per-lane lossy-repair tally
// ---------------------------------------------------------------------------

/// Record one lossy repair on `lane` through the shared recorder, the way a
/// dispatch walk does.
fn record_lossy(router: &Router, lane: Option<&crate::state_key::StateKey>) -> DispatchMeta {
    let mut meta = DispatchMeta::for_alias("a");
    router.note_field_repair_at(
        &mut meta,
        super::field_repair::RepairTarget {
            state_key: "nickname",
            lane,
        },
        THINKING_DISPLAY_PATH,
        super::field_repair::FIELD_ACTION_LOSSY_DROP_REPAIR,
    );
    meta
}

fn lane(provider_entry: &str) -> crate::state_key::StateKey {
    crate::state_key::StateKey::fixture(provider_entry)
}

/// `(lane, lossy)` pairs of the router's per-lane rows.
fn lane_rows(router: &Router) -> Vec<(String, u64)> {
    router
        .lossy_repairs_by_lane()
        .into_iter()
        .map(|row| (row.lane, row.lossy))
        .collect()
}

fn assert_rows_sum_to_total(name: &str, router: &Router) {
    let sum: u64 = router
        .lossy_repairs_by_lane()
        .iter()
        .map(|row| row.lossy)
        .sum();
    assert_eq!(
        sum,
        router.field_repair_counters().repair_lossy,
        "{name}: the per-lane rows sum to the lossy total",
    );
}

/// Each lossy repair lands on the learned lane of the target it repaired, and
/// a lane-less target lands in the overflow bucket.
///
/// Mutation checks: key the tally by a constant lane -> red on `two lanes`;
/// count a lane-less repair on no row -> red on `no learned lane`.
#[test]
fn lossy_repairs_count_on_the_lane_they_repaired() {
    type Row<'a> = (&'a str, Vec<Option<&'a str>>, Vec<(&'a str, u64)>);
    let rows: [Row<'_>; 3] = [
        (
            "two lanes",
            vec![Some("alpha"), Some("beta"), Some("alpha")],
            vec![("alpha#fixture-upstream", 2), ("beta#fixture-upstream", 1)],
        ),
        ("no learned lane", vec![None], vec![("other", 1)]),
        (
            "a lane beside a lane-less target",
            vec![Some("alpha"), None],
            vec![("alpha#fixture-upstream", 1), ("other", 1)],
        ),
    ];
    for (name, repairs, expected) in rows {
        let router = bare_router();
        for provider_entry in repairs {
            let lane = provider_entry.map(lane);
            record_lossy(&router, lane.as_ref());
        }

        let expected: Vec<(String, u64)> = expected
            .into_iter()
            .map(|(lane, lossy)| (lane.to_string(), lossy))
            .collect();
        assert_eq!(lane_rows(&router), expected, "{name}");
        assert_rows_sum_to_total(name, &router);
    }
}

/// The tally names at most 256 lanes: the 257th distinct lane counts under
/// `other`, while a lane already named keeps counting on its own row.
///
/// Mutation checks: drop the cap check in `LaneTally::record` -> red
/// (257 named rows, no `other`); route a named lane to `other` once the cap is
/// reached -> red on the first lane's count.
#[test]
fn the_lane_past_the_cap_counts_under_other() {
    let router = bare_router();
    let lanes: Vec<_> = (0..=super::field_verdict_observability::LANE_TALLY_CAP)
        .map(|n| lane(&format!("lane{n:03}")))
        .collect();
    for lane in &lanes {
        record_lossy(&router, Some(lane));
    }
    record_lossy(&router, Some(&lanes[0]));

    let rows = lane_rows(&router);
    assert_eq!(
        rows.len(),
        super::field_verdict_observability::LANE_TALLY_CAP + 1,
        "256 named lanes plus the overflow bucket",
    );
    assert_eq!(
        rows.last(),
        Some(&("other".to_string(), 1)),
        "the 257th distinct lane lands in other",
    );
    assert!(
        !rows
            .iter()
            .any(|(name, _)| name == "lane256#fixture-upstream"),
        "and is not named",
    );
    assert_eq!(
        rows[0],
        ("lane000#fixture-upstream".to_string(), 2),
        "a lane named before the cap keeps counting on its own row",
    );
    assert_eq!(router.field_repair_counters().repair_lossy, 258);
    assert_rows_sum_to_total("past the cap", &router);
}

/// A lossless repair moves no lane row: the map counts exactly what the total
/// counts.
#[test]
fn a_lossless_repair_adds_no_lane_row() {
    let router = bare_router();
    let lossless = lane("alpha");
    let mut meta = DispatchMeta::for_alias("a");
    router.note_field_repair_at(
        &mut meta,
        super::field_repair::RepairTarget {
            state_key: "nickname",
            lane: Some(&lossless),
        },
        THINKING_DISPLAY_PATH,
        super::field_repair::FIELD_ACTION_DROP_REPAIR,
    );

    assert!(router.lossy_repairs_by_lane().is_empty());
    assert_eq!(
        router.field_repair_counters().repair_attempted,
        1,
        "premise"
    );
}

/// A lane key is sanitized on the way into a row and the WARN, never logged raw.
#[test]
fn a_lane_key_is_sanitized_into_its_row_and_its_warn() {
    let router = bare_router();
    let raw = crate::state_key::StateKey::new("alpha", "up\nstream").expect("lane mints");
    let meta = record_lossy(&router, Some(&raw));

    let events = routectl_testkit::capture_events(|| super::field_repair::emit_field_repair(&meta));

    let rows = lane_rows(&router);
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].0.contains('\n'), "row lane is sanitized: {rows:?}");
    assert_eq!(rows[0].0, raw.for_log());
    assert_eq!(events[0].field("lane"), Some(raw.for_log().as_str()));
}

/// Every repair WARN names the learned lane of the target it repaired, or
/// `other` for a lane-less target.
///
/// Mutation check: drop `lane` from `emit_field_repair` -> red.
#[test]
fn every_repair_warn_carries_the_repaired_lane() {
    let rows: [(&str, Option<&str>, &str); 2] = [
        ("a learned lane", Some("alpha"), "alpha#fixture-upstream"),
        ("no learned lane", None, "other"),
    ];
    for (name, provider_entry, expected) in rows {
        let router = bare_router();
        let lane = provider_entry.map(lane);
        let meta = record_lossy(&router, lane.as_ref());

        let events =
            routectl_testkit::capture_events(|| super::field_repair::emit_field_repair(&meta));

        let warns: Vec<_> = events
            .iter()
            .filter(|e| e.level == tracing::Level::WARN && e.message == "envelope_field_repaired")
            .collect();
        assert_eq!(warns.len(), 1, "{name}: one WARN per repair");
        assert_eq!(warns[0].field("lane"), Some(expected), "{name}");
    }
}

// ---------------------------------------------------------------------------
// The per-lane budget-exhaustion tally
// ---------------------------------------------------------------------------

/// `(lane, exhausted)` pairs of the router's per-lane exhaustion rows.
fn exhaustion_rows(router: &Router) -> Vec<(String, u64)> {
    router
        .budget_exhaustions_by_lane()
        .into_iter()
        .map(|row| (row.lane, row.exhausted))
        .collect()
}

/// The exhaustion tally names at most 256 lanes like the lossy one: the 257th
/// distinct lane and a lane-less target count under `other`, a lane named
/// before the cap keeps its own row, and the rows sum to the total.
///
/// Mutation checks: drop the cap check in `LaneTally::record` -> red (257
/// named rows); report the lossy tally's total as `budget_exhausted` -> red on
/// the total.
#[test]
fn budget_exhaustion_past_the_lane_cap_counts_under_other_and_sums_to_the_total() {
    let router = bare_router();
    let lanes: Vec<_> = (0..=super::field_verdict_observability::LANE_TALLY_CAP)
        .map(|n| lane(&format!("lane{n:03}")))
        .collect();
    for lane in &lanes {
        router.metrics.incr_repair_budget_exhausted(Some(lane));
    }
    router.metrics.incr_repair_budget_exhausted(Some(&lanes[0]));
    router.metrics.incr_repair_budget_exhausted(None);

    let rows = exhaustion_rows(&router);
    assert_eq!(
        rows.len(),
        super::field_verdict_observability::LANE_TALLY_CAP + 1,
        "256 named lanes plus the overflow bucket",
    );
    assert_eq!(
        rows.last(),
        Some(&("other".to_string(), 2)),
        "the 257th distinct lane and the lane-less target land in other",
    );
    assert_eq!(rows[0], ("lane000#fixture-upstream".to_string(), 2));
    let total = router.field_repair_counters().budget_exhausted;
    assert_eq!(total, 259);
    assert_eq!(rows.iter().map(|(_, n)| n).sum::<u64>(), total);
    assert!(
        router.lossy_repairs_by_lane().is_empty(),
        "the two tallies are separate maps",
    );
}

/// Each finished request lands in the bucket of the draws it spent, and the
/// snapshot carries every bucket from zero to the whole allowance.
///
/// Mutation check: index the bucket by `draws + 1` -> red.
#[test]
fn repairs_per_request_counts_each_request_in_its_draw_bucket() {
    let router = bare_router();

    for draws in [0, 2, 2, REPAIRS_PER_REQUEST] {
        router.metrics.record_repairs_per_request(draws);
    }

    let mut expected = vec![0; usize::from(REPAIRS_PER_REQUEST) + 1];
    expected[0] = 1;
    expected[2] = 2;
    expected[usize::from(REPAIRS_PER_REQUEST)] = 1;
    assert_eq!(router.repairs_per_request(), expected);
}

/// The chain-step exits and the histogram start at zero, so a later nonzero
/// reading is the walk's and not the fixture's.
#[test]
fn the_chain_exit_counters_start_at_zero() {
    let counters = bare_router().field_repair_counters();

    assert_eq!(
        (
            counters.chain_steps_committed,
            counters.chain_steps_dropped,
            counters.budget_exhausted,
        ),
        (0, 0, 0),
    );
    assert_eq!(
        bare_router().repairs_per_request(),
        vec![0; usize::from(REPAIRS_PER_REQUEST) + 1],
    );
    assert!(bare_router().budget_exhaustions_by_lane().is_empty());
}
