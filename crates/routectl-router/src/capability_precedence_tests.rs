//! Table tests over the staged precedence resolution.

use std::time::{Duration, Instant};

use super::*;

use routectl_core::wire_check::EgressKind;

use crate::capability_display::resolve_display;

const FEATURE: CellContext = CellContext {
    capability_enabled: true,
    beta_flag: false,
    droppable: false,
    representable: true,
};
const DROPPABLE: CellContext = CellContext {
    droppable: true,
    ..FEATURE
};
const BETA: CellContext = CellContext {
    capability_enabled: true,
    beta_flag: true,
    droppable: true,
    representable: true,
};
const BETA_UNREPRESENTABLE: CellContext = CellContext {
    representable: false,
    ..BETA
};
const BETA_UNREPRESENTABLE_OFF: CellContext = CellContext {
    capability_enabled: false,
    ..BETA_UNREPRESENTABLE
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
        source,
    }
}

const LAPSED: Option<LearnedInput> = Some(LearnedInput::Lapsed {
    source: EvidenceSource::Live,
});
const VERIFIED: Option<LearnedInput> = Some(LearnedInput::Verified {
    source: EvidenceSource::Live,
});

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
        Row { pinned: true, learned: LAPSED, operator: Operator::Decided,
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
        Row { pinned: true, learned: LAPSED,
              layer: Some(Layer::Learned), action: ACTION_REPROBE,
              ..Row::new("pinned feature with a lapsed negative stays open to the read", DROPPABLE) },
        Row { pinned: true, prior: Some(false), routing: Routing::Tail,
              layer: Some(Layer::Prior), action: ACTION_ROUTE_AWAY,
              ..Row::new("pinned feature with a false prior tails", FEATURE) },
        Row { pinned: true,
              ..Row::new("pinned feature with no other opinion stays open", FEATURE) },
        // --- pin > representability > override ---
        Row { pinned: true, learned: self_id(F1), operator: Operator::Decided,
              layer: Some(Layer::Pin), action: ACTION_ALLOW,
              ..Row::new("pinned beta on an unrepresentable lane still sends", BETA_UNREPRESENTABLE) },
        Row { override_verdict: FORCE, learned: VERIFIED, seed: Some(SeedCell::Cleared),
              operator: Operator::Decided, wire: Wire::Withhold,
              layer: Some(Layer::Representability), action: ACTION_WITHHOLD,
              ..Row::new("unrepresentable beta withholds over force-supported", BETA_UNREPRESENTABLE) },
        Row { operator: Operator::Decided, wire: Wire::Withhold,
              layer: Some(Layer::Representability), action: ACTION_WITHHOLD,
              ..Row::new("unrepresentable beta withholds with the kill switch off", BETA_UNREPRESENTABLE_OFF) },
        // --- override > learned ---
        Row { override_verdict: ROUTE_AWAY, learned: VERIFIED,
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
        Row { learned: VERIFIED, seed: Some(SeedCell::Withhold),
              layer: Some(Layer::Learned), action: ACTION_ALLOW,
              ..Row::new("beta positive sends over a withholding seed", BETA) },
        Row { learned: LAPSED, seed: Some(SeedCell::Withhold),
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
        Row { learned: VERIFIED, prior: Some(false),
              layer: Some(Layer::Learned), action: ACTION_ALLOW,
              ..Row::new("verified positive sends and keeps over a false prior", FEATURE) },
        Row { learned: LAPSED, prior: Some(false),
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
        ("pin over representability",
         DisplayInputs { pinned: true, override_verdict: FORCE, ..inputs(BETA_UNREPRESENTABLE) },
         Some(Layer::Pin), Some(Layer::Override)),
        ("representability over an override",
         DisplayInputs { override_verdict: FORCE, learned: VERIFIED, ..inputs(BETA_UNREPRESENTABLE) },
         Some(Layer::Representability), Some(Layer::Override)),
        ("override over a learned negative",
         DisplayInputs { override_verdict: FORCE, learned: self_id(F1), ..inputs(FEATURE) },
         Some(Layer::Override), Some(Layer::Learned)),
        ("learned over the seed",
         DisplayInputs { learned: VERIFIED, seed: Some(SeedCell::Withhold), ..inputs(BETA) },
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
         DisplayInputs { learned: LAPSED, ..inputs(FEATURE) }, false),
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
         DisplayInputs { learned: VERIFIED, ..inputs(FEATURE) }, false),
    ];
    for (name, display_inputs, eligible) in rows {
        let resolution = resolve_display(*display_inputs);

        assert_eq!(resolution.hard_exclusion_eligible(), *eligible, "{name}");
    }
}

#[test]
fn learned_input_from_a_dispatch_reading_maps_a_claimed_probe_to_lapsed() {
    let probe = EvidenceSource::Probe;
    let rows = [
        (
            BetaLaneReading::ActingNegative {
                phase: FailurePhase::F2,
                tier: SignalTier::Inferred,
                source: probe,
            },
            Some(LearnedInput::Acting {
                phase: FailurePhase::F2,
                tier: SignalTier::Inferred,
                source: probe,
            }),
        ),
        (
            BetaLaneReading::ProbeAdmitted { source: probe },
            Some(LearnedInput::Lapsed { source: probe }),
        ),
        (
            BetaLaneReading::VerifiedPositive { source: probe },
            Some(LearnedInput::Verified { source: probe }),
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
            "uncorroborated inferred negative is shown but does not act",
            entry(broken, Inferred, 1, Probe, false, now),
            Some(LearnedInput::Uncorroborated { source: Probe }),
        ),
        (
            "corroborated inferred negative acts",
            entry(broken, Inferred, 2, Live, false, now),
            Some(acting(F1, Inferred, Live)),
        ),
        (
            "lapsed negative re-probes",
            entry(broken, SelfIdentifying, 1, Live, true, now),
            Some(LearnedInput::Lapsed { source: Live }),
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
            Some(LearnedInput::Lapsed { source: Probe }),
        ),
        (
            "verified positive, expiry ignored",
            entry(
                Verdict::VerifiedWorking,
                SelfIdentifying,
                1,
                Probe,
                true,
                now,
            ),
            Some(LearnedInput::Verified { source: Probe }),
        ),
    ];
    for (name, entry, expected) in rows {
        assert_eq!(LearnedInput::from_entry(&entry, now), expected, "{name}");
    }
}

#[test]
fn layer_tokens_match_the_doctor_vocabulary_plus_pin_and_representability() {
    let tokens: Vec<&str> = [
        Layer::Pin,
        Layer::Representability,
        Layer::Override,
        Layer::Learned,
        Layer::Seed,
        Layer::Prior,
    ]
    .iter()
    .map(|layer| layer.as_str())
    .collect();
    assert_eq!(
        tokens,
        [
            "pin",
            "representability",
            "override",
            "learned",
            "seed",
            "prior"
        ]
    );
}

/// A config whose `[capability] essential` list holds `essential`.
fn capability_config(essential: &[&str]) -> crate::config::CapabilityConfig {
    crate::config::CapabilityConfig {
        essential: essential.iter().map(|key| (*key).to_string()).collect(),
        ..Default::default()
    }
}

#[test]
fn cell_context_applies_the_essential_list_to_a_droppable_key() {
    use crate::capability_strip::CONTEXT_MANAGEMENT;
    let self_id_f1 = self_id(FailurePhase::F1);
    // (name, essential list, droppable, wire, routing, eligible)
    let rows = [
        (
            "not essential: an F1 negative strips in place",
            capability_config(&[]),
            true,
            Wire::Strip,
            Routing::Keep,
            false,
        ),
        (
            "essential: the same negative tails and counts",
            capability_config(&[CONTEXT_MANAGEMENT]),
            false,
            Wire::Send,
            Routing::Tail,
            true,
        ),
    ];
    for (name, capability, droppable, wire, routing, eligible) in rows {
        // Arrange
        let ctx = CellContext::new(CONTEXT_MANAGEMENT, &capability, None, "upstream");

        // Act
        let resolution = resolve_display(DisplayInputs {
            learned: self_id_f1,
            ..inputs(ctx)
        });

        // Assert
        assert_eq!(ctx.droppable, droppable, "{name}: droppable");
        assert!(!ctx.beta_flag, "{name}: a feature key");
        assert_eq!(resolution.wire(), wire, "{name}: wire");
        assert_eq!(resolution.routing(), routing, "{name}: routing");
        assert_eq!(
            resolution.hard_exclusion_eligible(),
            eligible,
            "{name}: eligible"
        );
    }
}

#[test]
fn cell_context_reads_the_kill_switch_and_the_beta_namespace() {
    let key = crate::beta_capability::beta_capability_key("fx-flag").expect("a flag");
    let off = crate::config::CapabilityConfig {
        enabled: false,
        ..capability_config(&[])
    };
    // (name, key, config, beta, enabled)
    let rows = [
        ("beta key", key.as_str(), capability_config(&[]), true, true),
        (
            "feature key",
            "web_search",
            capability_config(&[]),
            false,
            true,
        ),
        ("kill switch off", "web_search", off, false, false),
    ];
    for (name, capability_key, capability, beta, enabled) in rows {
        let ctx = CellContext::new(capability_key, &capability, None, "upstream");

        assert_eq!(ctx.beta_flag, beta, "{name}: beta_flag");
        assert_eq!(ctx.capability_enabled, enabled, "{name}: enabled");
        assert!(ctx.representable, "{name}: no Converse lane refuses it");
    }
}

#[test]
fn cell_context_marks_only_a_beta_key_on_a_refusing_lane_unrepresentable() {
    let beta = crate::beta_capability::beta_capability_key("fx-flag").expect("a flag");
    let llama = "meta.llama3-70b-instruct-v1:0";
    let claude = "anthropic.claude-haiku-4-5-20251001-v1:0";
    let converse = Some(EgressKind::BedrockConverse);
    // (name, key, egress, upstream, representable)
    let rows = [
        (
            "beta key, Converse, non-Claude",
            beta.as_str(),
            converse,
            llama,
            false,
        ),
        (
            "beta key, Converse, Claude",
            beta.as_str(),
            converse,
            claude,
            true,
        ),
        (
            "beta key, Invoke, non-Claude",
            beta.as_str(),
            Some(EgressKind::BedrockInvoke),
            llama,
            true,
        ),
        (
            "feature key, Converse, non-Claude",
            "web_search",
            converse,
            llama,
            true,
        ),
    ];
    for (name, key, egress, upstream, representable) in rows {
        let ctx = CellContext::new(key, &capability_config(&[]), egress, upstream);

        assert_eq!(ctx.representable, representable, "{name}");
    }
}
