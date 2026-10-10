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
            .observe(LANE, key, kind, tier, phase, source, None, now)
            .is_some(),
        Entry::InGeneration => reg
            .observe_in_generation(
                reg.generation(),
                LANE,
                key,
                kind,
                tier,
                phase,
                source,
                None,
                now,
            )
            .applied()
            .is_some(),
        Entry::WithObservations => reg
            .observe_in_generation_with_observations(
                reg.generation(),
                LANE,
                key,
                kind,
                tier,
                phase,
                source,
                None,
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
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        now,
    );
    let counted = reg.observe_field_verdict_in_generation(
        reg.generation(),
        &key,
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        now,
    );

    assert_eq!(observed, ObserveOutcome::Acting);
    assert_eq!(counted.value(), Some((ObserveOutcome::Acting, 2)));
    let resident: Vec<String> = reg.snapshot().into_iter().map(|e| e.feature_key).collect();
    assert_eq!(resident, vec![field_key()]);
}
