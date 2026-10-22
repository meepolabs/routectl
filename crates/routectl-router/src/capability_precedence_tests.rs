//! Table tests over the staged precedence resolution.

use std::time::{Duration, Instant};

use super::*;

use crate::capability_display::{
    ActionInputs, LearnedActing, resolve_display_action, resolve_display_verdict,
};

const FEATURE: CellContext = CellContext {
    capability_enabled: true,
    beta_flag: false,
    droppable: false,
};
const DROPPABLE: CellContext = CellContext {
    droppable: true,
    ..FEATURE
};
const BETA: CellContext = CellContext {
    capability_enabled: true,
    beta_flag: true,
    droppable: true,
};
const FEATURE_OFF: CellContext = CellContext {
    capability_enabled: false,
    ..FEATURE
};
const BETA_OFF: CellContext = CellContext {
    capability_enabled: false,
    ..BETA
};

const ROUTE_AWAY: Option<OverrideVerdict> = Some(OverrideVerdict::RouteAway);
const FORCE: Option<OverrideVerdict> = Some(OverrideVerdict::ForceSupported);

const fn acting(phase: FailurePhase, tier: SignalTier, source: EvidenceSource) -> LearnedInput {
    LearnedInput::Acting {
        phase,
        tier,
        source: Some(source),
    }
}

const fn self_id(phase: FailurePhase) -> Option<LearnedInput> {
    Some(acting(
        phase,
        SignalTier::SelfIdentifying,
        EvidenceSource::Live,
    ))
}

/// What the operator stage returns for a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operator {
    /// Both axes settled; the claiming read is skipped.
    Decided,
    /// Open, and the learned tier must be read.
    OpenRead,
    /// Open, but the kill switch is off so the learned read is skipped.
    OpenSkip,
}

struct Row {
    name: &'static str,
    pinned: bool,
    override_verdict: Option<OverrideVerdict>,
    learned: Option<LearnedInput>,
    seed: Option<SeedCell>,
    prior: Option<bool>,
    ctx: CellContext,
    operator: Operator,
    wire: Wire,
    routing: Routing,
    layer: Option<Layer>,
    action: &'static str,
}

impl Row {
    const fn new(name: &'static str, ctx: CellContext) -> Self {
        Self {
            name,
            pinned: false,
            override_verdict: None,
            learned: None,
            seed: None,
            prior: None,
            ctx,
            operator: Operator::OpenRead,
            wire: Wire::Send,
            routing: Routing::Keep,
            layer: None,
            action: ACTION_NONE,
        }
    }
}

/// Run the staged resolution the way a dispatch caller does, recording what
/// the operator stage told it.
fn staged(row: &Row) -> (Operator, Resolution) {
    match resolve_operator(row.pinned, row.override_verdict, row.ctx) {
        Stage::Decided(resolution) => (Operator::Decided, resolution),
        Stage::Open(cell) => {
            assert_eq!(cell.pinned(), row.pinned, "{}: open cell pin", row.name);
            let operator = if cell.reads_learned() {
                Operator::OpenRead
            } else {
                Operator::OpenSkip
            };
            (
                operator,
                resolve_open(row.learned, row.seed, row.prior, cell),
            )
        }
    }
}

#[rustfmt::skip]
fn precedence_rows() -> Vec<Row> {
    use FailurePhase::{F1, F2, F3};
    vec![
        // --- pin > override ---
        Row { pinned: true, learned: self_id(F1), operator: Operator::Decided,
              layer: Some(Layer::Pin), action: ACTION_ALLOW,
              ..Row::new("pinned beta with an acting negative sends", BETA) },
        Row { pinned: true, override_verdict: ROUTE_AWAY, operator: Operator::Decided,
              layer: Some(Layer::Pin), action: ACTION_ALLOW,
              ..Row::new("pinned beta beats a route-away override", BETA) },
        Row { pinned: true, override_verdict: FORCE, operator: Operator::Decided,
              layer: Some(Layer::Pin), action: ACTION_ALLOW,
              ..Row::new("pinned beta beats a force-supported override", BETA) },
        Row { pinned: true, learned: Some(LearnedInput::Lapsed), operator: Operator::Decided,
              layer: Some(Layer::Pin), action: ACTION_ALLOW,
              ..Row::new("pinned beta skips the claiming read of a lapsed negative", BETA) },
        Row { pinned: true, learned: self_id(F1), seed: Some(SeedCell::Withhold),
              operator: Operator::Decided, layer: Some(Layer::Pin), action: ACTION_ALLOW,
              ..Row::new("pinned beta with the kill switch off still sends", BETA_OFF) },
        Row { pinned: true, override_verdict: ROUTE_AWAY, operator: Operator::Decided,
              routing: Routing::Drop, layer: Some(Layer::Override), action: ACTION_DROP,
              ..Row::new("pinned feature with a route-away override drops", DROPPABLE) },
        Row { pinned: true, learned: self_id(F1), routing: Routing::Tail,
              layer: Some(Layer::Learned), action: ACTION_ROUTE_AWAY,
              ..Row::new("pinned droppable F1 negative routes away, never strips", DROPPABLE) },
        Row { pinned: true, learned: Some(LearnedInput::Lapsed),
              layer: Some(Layer::Learned), action: ACTION_REPROBE,
              ..Row::new("pinned feature with a lapsed negative stays open to the read", DROPPABLE) },
        Row { pinned: true, prior: Some(false), routing: Routing::Tail,
              layer: Some(Layer::Prior), action: ACTION_ROUTE_AWAY,
              ..Row::new("pinned feature with a false prior tails", FEATURE) },
        Row { pinned: true,
              ..Row::new("pinned feature with no other opinion stays open", FEATURE) },
        // --- override > learned ---
        Row { override_verdict: ROUTE_AWAY, learned: Some(LearnedInput::Verified),
              operator: Operator::Decided, routing: Routing::Drop,
              layer: Some(Layer::Override), action: ACTION_DROP,
              ..Row::new("route-away override drops a feature over a positive", FEATURE) },
        Row { override_verdict: ROUTE_AWAY, operator: Operator::Decided,
              wire: Wire::Withhold, layer: Some(Layer::Override), action: ACTION_WITHHOLD,
              ..Row::new("route-away override withholds a beta flag", BETA) },
        Row { override_verdict: FORCE, learned: self_id(F2), prior: Some(false),
              operator: Operator::Decided, layer: Some(Layer::Override), action: ACTION_ALLOW,
              ..Row::new("force-supported masks a negative and a false prior", FEATURE) },
        Row { override_verdict: ROUTE_AWAY, operator: Operator::Decided,
              routing: Routing::Drop, layer: Some(Layer::Override), action: ACTION_DROP,
              ..Row::new("route-away override acts with the kill switch off", FEATURE_OFF) },
        // --- learned > seed ---
        Row { learned: self_id(F1), seed: Some(SeedCell::Cleared), wire: Wire::Withhold,
              layer: Some(Layer::Learned), action: ACTION_WITHHOLD,
              ..Row::new("beta negative withholds over a cleared seed", BETA) },
        Row { learned: Some(LearnedInput::Verified), seed: Some(SeedCell::Withhold),
              layer: Some(Layer::Learned), action: ACTION_ALLOW,
              ..Row::new("beta positive sends over a withholding seed", BETA) },
        Row { learned: Some(LearnedInput::Lapsed), seed: Some(SeedCell::Withhold),
              layer: Some(Layer::Learned), action: ACTION_REPROBE,
              ..Row::new("lapsed beta negative re-probes over the seed", BETA) },
        // --- seed > prior ---
        Row { seed: Some(SeedCell::Withhold), prior: Some(true), wire: Wire::Withhold,
              layer: Some(Layer::Seed), action: ACTION_WITHHOLD,
              ..Row::new("seed withholds", BETA) },
        Row { seed: Some(SeedCell::Cleared), layer: Some(Layer::Seed), action: ACTION_ALLOW,
              ..Row::new("cleared seed sends", BETA) },
        Row { prior: Some(false),
              ..Row::new("beta key has no prior tier", BETA) },
        // --- learned > prior ---
        Row { learned: self_id(F2), prior: Some(true), routing: Routing::Tail,
              layer: Some(Layer::Learned), action: ACTION_ROUTE_AWAY,
              ..Row::new("F2 negative routes away over a true prior", DROPPABLE) },
        Row { learned: self_id(F1), wire: Wire::Strip, layer: Some(Layer::Learned),
              action: ACTION_STRIP,
              ..Row::new("droppable F1 negative strips in place", DROPPABLE) },
        Row { learned: self_id(F1), routing: Routing::Tail, layer: Some(Layer::Learned),
              action: ACTION_ROUTE_AWAY,
              ..Row::new("non-droppable F1 negative routes away", FEATURE) },
        Row { learned: Some(LearnedInput::Verified), prior: Some(false),
              layer: Some(Layer::Learned), action: ACTION_ALLOW,
              ..Row::new("verified positive sends and keeps over a false prior", FEATURE) },
        Row { learned: Some(LearnedInput::Lapsed), prior: Some(false),
              layer: Some(Layer::Learned), action: ACTION_REPROBE,
              ..Row::new("lapsed negative keeps the target and re-probes", DROPPABLE) },
        Row { learned: Some(acting(F3, SignalTier::SelfIdentifying, EvidenceSource::Live)),
              prior: Some(false), routing: Routing::Tail, layer: Some(Layer::Prior),
              action: ACTION_ROUTE_AWAY,
              ..Row::new("live F3 negative has no opinion; the prior decides", FEATURE) },
        Row { learned: Some(acting(F3, SignalTier::SelfIdentifying, EvidenceSource::Live)),
              ..Row::new("live F3 negative alone has no opinion", FEATURE) },
        Row { learned: Some(acting(F3, SignalTier::SelfIdentifying, EvidenceSource::Probe)),
              routing: Routing::Tail, layer: Some(Layer::Learned), action: ACTION_ROUTE_AWAY,
              ..Row::new("probe F3 negative routes away", DROPPABLE) },
        Row { prior: Some(false), routing: Routing::Tail, layer: Some(Layer::Prior),
              action: ACTION_ROUTE_AWAY,
              ..Row::new("false prior tails", FEATURE) },
        Row { prior: Some(true), layer: Some(Layer::Prior),
              ..Row::new("true prior keeps with no action", FEATURE) },
        // --- kill switch off: learned and prior silent, seed still acts ---
        Row { learned: self_id(F2), prior: Some(false), operator: Operator::OpenSkip,
              ..Row::new("kill switch off silences learned and prior", FEATURE_OFF) },
        Row { learned: self_id(F1), seed: Some(SeedCell::Withhold), operator: Operator::OpenSkip,
              wire: Wire::Withhold, layer: Some(Layer::Seed), action: ACTION_WITHHOLD,
              ..Row::new("kill switch off leaves the seed acting", BETA_OFF) },
        Row::new("no tier has an opinion", FEATURE),
    ]
}

#[test]
fn staged_resolution_applies_pin_override_learned_seed_prior_in_order() {
    for row in precedence_rows() {
        let (operator, resolution) = staged(&row);

        assert_eq!(operator, row.operator, "{}: operator stage", row.name);
        assert_eq!(resolution.wire(), row.wire, "{}: wire", row.name);
        assert_eq!(resolution.routing(), row.routing, "{}: routing", row.name);
        assert_eq!(resolution.layer(), row.layer, "{}: layer", row.name);
        assert_eq!(resolution.as_str(), row.action, "{}: action", row.name);
        assert_eq!(resolution.pinned(), row.pinned, "{}: pinned", row.name);
        assert_ne!(
            (row.pinned, resolution.wire()),
            (true, Wire::Strip),
            "{}: a pin never strips",
            row.name
        );
        assert_eq!(
            resolution.overridden(),
            None,
            "{}: dispatch never fills overridden",
            row.name
        );
    }
}

#[test]
fn display_mode_picks_the_same_winner_as_the_staged_resolution() {
    for row in precedence_rows() {
        let (_, staged) = staged(&row);

        let display = resolve_display(DisplayInputs {
            pinned: row.pinned,
            override_verdict: row.override_verdict,
            learned: row.learned,
            seed: row.seed,
            prior: row.prior,
            ctx: row.ctx,
        });

        assert_eq!(
            (
                display.wire(),
                display.routing(),
                display.layer(),
                display.as_str()
            ),
            (
                staged.wire(),
                staged.routing(),
                staged.layer(),
                staged.as_str()
            ),
            "{}",
            row.name
        );
    }
}

#[test]
#[rustfmt::skip]
fn display_mode_names_the_next_tier_with_an_opinion() {
    use FailurePhase::{F1, F3};
    let rows: &[(&str, DisplayInputs, Option<Layer>, Option<Layer>)] = &[
        ("pin over a learned negative",
         DisplayInputs { pinned: true, learned: self_id(F1), seed: Some(SeedCell::Withhold), ..inputs(BETA) },
         Some(Layer::Pin), Some(Layer::Learned)),
        ("pin over an override",
         DisplayInputs { pinned: true, override_verdict: ROUTE_AWAY, learned: self_id(F1), ..inputs(BETA) },
         Some(Layer::Pin), Some(Layer::Override)),
        ("override over a learned negative",
         DisplayInputs { override_verdict: FORCE, learned: self_id(F1), ..inputs(FEATURE) },
         Some(Layer::Override), Some(Layer::Learned)),
        ("learned over the seed",
         DisplayInputs { learned: Some(LearnedInput::Verified), seed: Some(SeedCell::Withhold), ..inputs(BETA) },
         Some(Layer::Learned), Some(Layer::Seed)),
        ("learned over a prior of either polarity",
         DisplayInputs { learned: self_id(F1), prior: Some(true), ..inputs(FEATURE) },
         Some(Layer::Learned), Some(Layer::Prior)),
        ("a live F3 negative is skipped as an opinion",
         DisplayInputs { override_verdict: FORCE,
                         learned: Some(acting(F3, SignalTier::SelfIdentifying, EvidenceSource::Live)),
                         prior: Some(false), ..inputs(FEATURE) },
         Some(Layer::Override), Some(Layer::Prior)),
        ("a pin on a feature key is not an opinion",
         DisplayInputs { pinned: true, learned: self_id(F1), ..inputs(FEATURE) },
         Some(Layer::Learned), None),
        ("kill switch off silences the learned tier as an opinion",
         DisplayInputs { override_verdict: ROUTE_AWAY, learned: self_id(F1), prior: Some(false), ..inputs(FEATURE_OFF) },
         Some(Layer::Override), None),
        ("the winner is the only opinion",
         DisplayInputs { seed: Some(SeedCell::Cleared), ..inputs(BETA) },
         Some(Layer::Seed), None),
        ("no opinion at all", inputs(FEATURE), None, None),
    ];
    for (name, display_inputs, layer, overridden) in rows {
        let resolution = resolve_display(*display_inputs);

        assert_eq!(resolution.layer(), *layer, "{name}: layer");
        assert_eq!(resolution.overridden(), *overridden, "{name}: overridden");
    }
}

const fn inputs(ctx: CellContext) -> DisplayInputs {
    DisplayInputs {
        pinned: false,
        override_verdict: None,
        learned: None,
        seed: None,
        prior: None,
        ctx,
    }
}

#[test]
#[rustfmt::skip]
fn hard_exclusion_counts_only_an_acting_self_identifying_route_away() {
    use FailurePhase::{F1, F2, F3};
    let inferred_f2 = Some(acting(F2, SignalTier::Inferred, EvidenceSource::Live));
    let rows: &[(&str, DisplayInputs, bool)] = &[
        ("self-identifying F2 route-away counts",
         DisplayInputs { learned: self_id(F2), ..inputs(FEATURE) }, true),
        ("self-identifying F1 non-droppable route-away counts",
         DisplayInputs { learned: self_id(F1), ..inputs(FEATURE) }, true),
        ("an inferred corroborated negative never counts",
         DisplayInputs { learned: inferred_f2, ..inputs(FEATURE) }, false),
        ("a lapsed negative does not count",
         DisplayInputs { learned: Some(LearnedInput::Lapsed), ..inputs(FEATURE) }, false),
        ("an F3 probe negative does not count",
         DisplayInputs { learned: Some(acting(F3, SignalTier::SelfIdentifying, EvidenceSource::Probe)),
                         ..inputs(FEATURE) }, false),
        ("a strip-eligible F1 negative does not count",
         DisplayInputs { learned: self_id(F1), ..inputs(DROPPABLE) }, false),
        ("a pinned route-away does not count",
         DisplayInputs { pinned: true, learned: self_id(F1), ..inputs(DROPPABLE) }, false),
        ("force-supported does not count",
         DisplayInputs { override_verdict: FORCE, learned: self_id(F2), ..inputs(FEATURE) }, false),
        ("a route-away override does not count",
         DisplayInputs { override_verdict: ROUTE_AWAY, learned: self_id(F2), ..inputs(FEATURE) }, false),
        ("kill switch off does not count",
         DisplayInputs { learned: self_id(F2), ..inputs(FEATURE_OFF) }, false),
        ("a beta-key withhold does not count",
         DisplayInputs { learned: self_id(F2), ..inputs(BETA) }, false),
        ("a false prior does not count",
         DisplayInputs { prior: Some(false), ..inputs(FEATURE) }, false),
        ("a verified positive does not count",
         DisplayInputs { learned: Some(LearnedInput::Verified), ..inputs(FEATURE) }, false),
    ];
    for (name, display_inputs, eligible) in rows {
        let resolution = resolve_display(*display_inputs);

        assert_eq!(resolution.hard_exclusion_eligible(), *eligible, "{name}");
    }
}

#[test]
fn learned_input_from_a_dispatch_reading_maps_a_claimed_probe_to_lapsed() {
    let rows = [
        (
            BetaLaneReading::ActingNegative {
                phase: FailurePhase::F2,
                tier: SignalTier::Inferred,
            },
            Some(LearnedInput::Acting {
                phase: FailurePhase::F2,
                tier: SignalTier::Inferred,
                source: None,
            }),
        ),
        (BetaLaneReading::ProbeAdmitted, Some(LearnedInput::Lapsed)),
        (
            BetaLaneReading::VerifiedPositive,
            Some(LearnedInput::Verified),
        ),
        (BetaLaneReading::NoVerdict, None),
    ];
    for (reading, expected) in rows {
        assert_eq!(LearnedInput::from_reading(reading), expected, "{reading:?}");
    }
}

/// A snapshot entry with the given shape, lapsed or not against `now` (a
/// negative lapses at its expiry instant).
fn entry(
    verdict: Verdict,
    tier: SignalTier,
    observations: u32,
    source: EvidenceSource,
    lapsed: bool,
    now: Instant,
) -> LearnedRegistryEntry {
    let hour = Duration::from_hours(1);
    LearnedRegistryEntry {
        state_key: "lane#upstream".to_string(),
        feature_key: "web_search".to_string(),
        verdict,
        signal_tier: tier,
        observations,
        first_seen: now,
        last_seen: now,
        expires_at: if lapsed { now } else { now + hour },
        evidence_class: None,
        phase: match verdict {
            Verdict::LearnedBroken(phase) => phase,
            _ => FailurePhase::F3,
        },
        source,
        recheck_owned: false,
    }
}

#[test]
fn learned_input_from_a_snapshot_entry_applies_the_dispatch_rules() {
    use EvidenceSource::{Live, Probe};
    use FailurePhase::{F1, F3};
    use SignalTier::{Inferred, SelfIdentifying};
    let now = Instant::now();
    let broken = Verdict::LearnedBroken(F1);
    let rows = [
        (
            "fresh self-identifying negative acts",
            entry(broken, SelfIdentifying, 1, Live, false, now),
            Some(acting(F1, SelfIdentifying, Live)),
        ),
        (
            "uncorroborated inferred negative does not act",
            entry(broken, Inferred, 1, Live, false, now),
            None,
        ),
        (
            "corroborated inferred negative acts",
            entry(broken, Inferred, 2, Live, false, now),
            Some(acting(F1, Inferred, Live)),
        ),
        (
            "lapsed negative re-probes",
            entry(broken, SelfIdentifying, 1, Live, true, now),
            Some(LearnedInput::Lapsed),
        ),
        (
            "lapsed live F3 negative stays advisory",
            entry(
                Verdict::LearnedBroken(F3),
                SelfIdentifying,
                1,
                Live,
                true,
                now,
            ),
            Some(acting(F3, SelfIdentifying, Live)),
        ),
        (
            "lapsed probe F3 negative re-probes",
            entry(
                Verdict::LearnedBroken(F3),
                SelfIdentifying,
                1,
                Probe,
                true,
                now,
            ),
            Some(LearnedInput::Lapsed),
        ),
        (
            "verified positive, expiry ignored",
            entry(
                Verdict::VerifiedWorking,
                SelfIdentifying,
                1,
                Live,
                true,
                now,
            ),
            Some(LearnedInput::Verified),
        ),
    ];
    for (name, entry, expected) in rows {
        assert_eq!(LearnedInput::from_entry(&entry, now), expected, "{name}");
    }
}

#[test]
fn refuses_beta_bag_key_only_for_converse_to_a_provably_non_anthropic_model() {
    let rows = [
        (
            Some(EgressKind::BedrockConverse),
            "meta.llama3-70b-instruct-v1:0",
            true,
        ),
        (
            Some(EgressKind::BedrockConverse),
            "anthropic.claude-haiku-4-5-20251001-v1:0",
            false,
        ),
        (
            Some(EgressKind::BedrockConverse),
            "arn:aws:bedrock:us-east-1:123456789012:inference-profile/p",
            false,
        ),
        (
            Some(EgressKind::BedrockInvoke),
            "meta.llama3-70b-instruct-v1:0",
            false,
        ),
        (None, "meta.llama3-70b-instruct-v1:0", false),
    ];
    for (egress, upstream, refuses) in rows {
        assert_eq!(
            refuses_beta_bag_key(egress, upstream),
            refuses,
            "{egress:?} {upstream}"
        );
    }
}

#[test]
fn layer_tokens_match_the_doctor_vocabulary_plus_pin() {
    let tokens: Vec<&str> = [
        Layer::Pin,
        Layer::Override,
        Layer::Learned,
        Layer::Seed,
        Layer::Prior,
    ]
    .iter()
    .map(|layer| layer.as_str())
    .collect();
    assert_eq!(tokens, ["pin", "override", "learned", "seed", "prior"]);
}

/// Every learned-entry shape the old display action distinguishes.
fn entry_grid(now: Instant) -> Vec<Option<LearnedRegistryEntry>> {
    let mut grid = vec![
        None,
        Some(entry(
            Verdict::VerifiedWorking,
            SignalTier::SelfIdentifying,
            1,
            EvidenceSource::Live,
            false,
            now,
        )),
    ];
    for phase in [FailurePhase::F1, FailurePhase::F2, FailurePhase::F3] {
        for source in [EvidenceSource::Live, EvidenceSource::Probe] {
            for lapsed in [false, true] {
                for (tier, observations) in [
                    (SignalTier::SelfIdentifying, 1),
                    (SignalTier::Inferred, 1),
                    (SignalTier::Inferred, 2),
                ] {
                    grid.push(Some(entry(
                        Verdict::LearnedBroken(phase),
                        tier,
                        observations,
                        source,
                        lapsed,
                        now,
                    )));
                }
            }
        }
    }
    grid
}

/// The display action token is unchanged for every unpinned cell the old
/// display resolver could see, except a catalog prior on a beta key: the
/// dispatch beta path has no prior tier, so the seam gives it no opinion.
#[test]
fn display_action_tokens_match_the_previous_display_resolver() {
    let now = Instant::now();
    let mut compared = 0;
    let mut beta_prior_cells = 0;
    for learned_entry in entry_grid(now) {
        for override_verdict in [None, ROUTE_AWAY, FORCE] {
            for seed in [None, Some(SeedCell::Withhold), Some(SeedCell::Cleared)] {
                for prior in [None, Some(true), Some(false)] {
                    for (beta_flag, droppable, capability_enabled) in bool_triples() {
                        let old = resolve_display_action(ActionInputs {
                            display: resolve_display_verdict(
                                override_verdict,
                                learned_entry.as_ref().map(|e| (e.verdict, e.source)),
                                seed,
                                prior,
                            ),
                            learned: learned_entry
                                .as_ref()
                                .map(|e| LearnedActing::from_entry(e, now)),
                            prior,
                            strip_applies: droppable,
                            capability_enabled,
                            beta_flag,
                            seed,
                        });
                        let new = resolve_display(DisplayInputs {
                            pinned: false,
                            override_verdict,
                            learned: learned_entry
                                .as_ref()
                                .and_then(|e| LearnedInput::from_entry(e, now)),
                            seed,
                            prior,
                            ctx: CellContext {
                                capability_enabled,
                                beta_flag,
                                droppable,
                            },
                        })
                        .as_str();
                        compared += 1;
                        if beta_flag && prior == Some(false) && old != new {
                            assert_eq!((old, new), (ACTION_ROUTE_AWAY, ACTION_NONE));
                            beta_prior_cells += 1;
                            continue;
                        }
                        assert_eq!(
                            new, old,
                            "override {override_verdict:?} learned {learned_entry:?} seed \
                             {seed:?} prior {prior:?} beta {beta_flag} droppable {droppable} \
                             enabled {capability_enabled}"
                        );
                    }
                }
            }
        }
    }
    assert_eq!(compared, 38 * 3 * 3 * 3 * 8);
    assert!(
        beta_prior_cells > 0,
        "the beta-prior exception is exercised"
    );
}

fn bool_triples() -> impl Iterator<Item = (bool, bool, bool)> {
    (0..8u8).map(|bits| (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0))
}
