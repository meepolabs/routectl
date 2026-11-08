//! The registry's generic write entries refuse a key that is, or normalizes
//! into, the envelope-field namespace; a field verdict is written only through
//! the entries that take its minted identity.

use super::*;
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};

const LANE: &str = "p0#upstream";
const FIELD_PATH: &str = "thinking";

fn registry() -> LearnedCapabilityRegistry {
    LearnedCapabilityRegistry::new(
        Duration::from_hours(48),
        Duration::from_hours(1),
        DEFAULT_MAX_ENTRIES,
    )
}

fn field_key() -> String {
    crate::field_capability::field_capability_key(FIELD_PATH).expect("a qualified path")
}

/// `(raw key, provider kind)` pairs the registry would store in the field
/// namespace: the exact key, and a dotted key Bedrock reduces to it.
fn field_namespace_keys() -> Vec<(String, &'static str)> {
    vec![
        (field_key(), "anthropic-api"),
        (format!("toolConfig.{}", field_key()), "bedrock"),
    ]
}

/// Which generic write entry a row exercises.
#[derive(Debug, Clone, Copy)]
enum Entry {
    Observe,
    InGeneration,
    WithObservations,
}

/// `true` when the entry recorded the observation.
fn write(reg: &LearnedCapabilityRegistry, entry: Entry, key: &str, kind: &str) -> bool {
    let now = Instant::now();
    let (tier, phase, source) = (
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
    );
    match entry {
        Entry::Observe => reg
            .observe(
                crate::learned_capability::LearnKey {
                    state_key: LANE,
                    feature_key_raw: key,
                    provider_kind: kind,
                },
                crate::learned_capability::NegativeEvidence {
                    tier,
                    phase,
                    source,
                    evidence_class: None,
                },
                now,
            )
            .is_some(),
        Entry::InGeneration => reg
            .observe_in_generation(
                reg.generation(),
                crate::learned_capability::LearnKey {
                    state_key: LANE,
                    feature_key_raw: key,
                    provider_kind: kind,
                },
                crate::learned_capability::NegativeEvidence {
                    tier,
                    phase,
                    source,
                    evidence_class: None,
                },
                now,
            )
            .applied()
            .is_some(),
        Entry::WithObservations => reg
            .observe_in_generation_with_observations(
                reg.generation(),
                crate::learned_capability::LearnKey {
                    state_key: LANE,
                    feature_key_raw: key,
                    provider_kind: kind,
                },
                crate::learned_capability::NegativeEvidence {
                    tier,
                    phase,
                    source,
                    evidence_class: None,
                },
                now,
            )
            .applied()
            .is_some(),
    }
}

/// Mutation check: remove the namespace refusal from any one generic entry
/// (`observe`, `observe_in_generation`,
/// `observe_in_generation_with_observations`) -> red on that entry's rows.
#[test]
fn every_generic_entry_refuses_a_field_namespace_key() {
    for entry in [Entry::Observe, Entry::InGeneration, Entry::WithObservations] {
        for (key, kind) in field_namespace_keys() {
            let reg = registry();

            let recorded = write(&reg, entry, &key, kind);

            assert!(!recorded, "{entry:?} {key} ({kind}): refused");
            assert!(reg.is_empty(), "{entry:?} {key} ({kind}): nothing resident");
        }
        let control = registry();
        assert!(
            write(&control, entry, "web_search", "bedrock"),
            "{entry:?}: an ordinary key is recorded",
        );
    }
}

/// The field-verdict entries record the minted identity the generic ones refuse.
#[test]
fn the_field_verdict_entries_record_the_minted_identity() {
    let lane = crate::state_key::StateKey::parse(LANE).expect("lane");
    let key = crate::field_verdict::FieldVerdictKey::new(&lane, FIELD_PATH, "anthropic-api")
        .expect("identity");
    let now = Instant::now();
    let reg = registry();

    let observed = reg.observe_field_verdict(
        &key,
        crate::learned_capability::NegativeEvidence {
            tier: SignalTier::SelfIdentifying,
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
            evidence_class: None,
        },
        now,
    );
    let counted = reg.observe_field_verdict_in_generation(
        reg.generation(),
        &key,
        crate::learned_capability::NegativeEvidence {
            tier: SignalTier::SelfIdentifying,
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
            evidence_class: None,
        },
        now,
    );

    assert_eq!(observed, ObserveOutcome::Acting);
    assert_eq!(counted.value(), Some((ObserveOutcome::Acting, 2)));
    let resident: Vec<String> = reg.snapshot().into_iter().map(|e| e.feature_key).collect();
    assert_eq!(resident, vec![field_key()]);
}

/// Which positive write entry a row exercises.
#[derive(Debug, Clone, Copy)]
enum PositiveDoor {
    ObservePositive,
    PositiveInGeneration,
    AcceptedBetaInGeneration,
    ReplayAcceptedBeta,
}

const POSITIVE_DOORS: [PositiveDoor; 4] = [
    PositiveDoor::ObservePositive,
    PositiveDoor::PositiveInGeneration,
    PositiveDoor::AcceptedBetaInGeneration,
    PositiveDoor::ReplayAcceptedBeta,
];

/// What a positive door answered, flattened across its two return shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PositiveAnswer {
    Outcome(PositiveOutcome),
    FieldNamespace,
    OtherRefusal,
}

impl PositiveDoor {
    /// The answer a field-namespace key must get from this door.
    const fn field_refusal(self) -> PositiveAnswer {
        match self {
            Self::ObservePositive | Self::ReplayAcceptedBeta => {
                PositiveAnswer::Outcome(PositiveOutcome::Refused)
            }
            Self::PositiveInGeneration | Self::AcceptedBetaInGeneration => {
                PositiveAnswer::FieldNamespace
            }
        }
    }
}

fn gated(outcome: GenerationOutcome<PositiveOutcome>) -> PositiveAnswer {
    match outcome {
        GenerationOutcome::Applied { value, .. } => PositiveAnswer::Outcome(value),
        GenerationOutcome::FieldNamespace => PositiveAnswer::FieldNamespace,
        GenerationOutcome::Stale | GenerationOutcome::Reserved | GenerationOutcome::Exhausted => {
            PositiveAnswer::OtherRefusal
        }
    }
}

fn write_positive(
    reg: &LearnedCapabilityRegistry,
    door: PositiveDoor,
    key: &str,
    kind: &str,
) -> PositiveAnswer {
    let now = Instant::now();
    let learn_key = crate::learned_capability::LearnKey {
        state_key: LANE,
        feature_key_raw: key,
        provider_kind: kind,
    };
    match door {
        PositiveDoor::ObservePositive => PositiveAnswer::Outcome(reg.observe_positive(
            LANE,
            key,
            kind,
            EvidenceSource::Live,
            None,
            now,
        )),
        PositiveDoor::PositiveInGeneration => gated(reg.observe_positive_in_generation(
            reg.generation(),
            learn_key,
            EvidenceSource::Live,
            None,
            now,
        )),
        PositiveDoor::AcceptedBetaInGeneration => {
            gated(reg.observe_accepted_beta_in_generation(reg.generation(), LANE, key, kind, now))
        }
        PositiveDoor::ReplayAcceptedBeta => PositiveAnswer::Outcome(reg.replay_accepted_beta(
            LANE,
            key,
            kind,
            EvidenceSource::Live,
            now,
        )),
    }
}

/// Mutation checks: switch any one positive door back to an unscreened
/// target -> red on that door's field rows (the write is recorded and the key
/// is resident); drop the field-namespace arm of `screened_positive` -> the
/// ungated doors' field rows panic.
#[test]
fn every_positive_door_refuses_a_field_namespace_key() {
    for door in POSITIVE_DOORS {
        for (key, kind) in field_namespace_keys() {
            let reg = registry();

            let answer = write_positive(&reg, door, &key, kind);

            assert_eq!(answer, door.field_refusal(), "{door:?} {key} ({kind})");
            assert!(reg.is_empty(), "{door:?} {key} ({kind}): nothing resident");
        }
        let control = registry();
        assert_eq!(
            write_positive(&control, door, "web_search", "bedrock"),
            PositiveAnswer::Outcome(PositiveOutcome::Recorded),
            "{door:?}: an ordinary key is recorded",
        );
        assert_eq!(
            control.snapshot().len(),
            1,
            "{door:?}: the control is resident"
        );
    }
}
