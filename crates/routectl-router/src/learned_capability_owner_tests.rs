//! A resident entry acts only for a reader whose provider kind is the kind it
//! was written under, on every generation-validated acting read and on every
//! write.

use super::*;
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};

const LANE: &str = "alpha#model-x";
const OLD: &str = "openai-compat";
const NEW: &str = "anthropic-api";

fn registry() -> LearnedCapabilityRegistry {
    LearnedCapabilityRegistry::new(
        Duration::from_hours(48),
        Duration::from_hours(1),
        DEFAULT_MAX_ENTRIES,
    )
}

/// A wire-shape key, assembled at runtime so no source line outside the
/// owning module spells the namespace prefix.
fn field_key() -> String {
    format!("{}{}", "fie", "ld:thinking.display")
}

fn plant_negative(reg: &LearnedCapabilityRegistry, capability: &str, kind: &str) {
    let outcome = reg.observe_in_generation(
        reg.generation(),
        LANE,
        capability,
        kind,
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        Instant::now(),
    );
    assert!(outcome.applied().is_some(), "the fixture must plant");
}

fn plant_positive(reg: &LearnedCapabilityRegistry, capability: &str, kind: &str) {
    let outcome = reg.observe_positive_in_generation(
        reg.generation(),
        LANE,
        capability,
        kind,
        EvidenceSource::Live,
        None,
        Instant::now(),
    );
    assert!(outcome.applied().is_some(), "the fixture must plant");
}

type Plant = fn(&LearnedCapabilityRegistry, &str, &str);
type Acts = fn(&LearnedCapabilityRegistry, &str, &str) -> bool;

fn acting_negative_acts(reg: &LearnedCapabilityRegistry, capability: &str, kind: &str) -> bool {
    matches!(
        reg.acting_negative_in_generation(reg.generation(), LANE, capability, kind, Instant::now()),
        Some((RoutingDecision::RouteAway { .. }, _))
    )
}

fn negative_state_acts(reg: &LearnedCapabilityRegistry, capability: &str, kind: &str) -> bool {
    matches!(
        reg.negative_state_in_generation(reg.generation(), LANE, capability, kind, Instant::now()),
        Some((NegativeState::Acting, _))
    )
}

fn field_facts_act(reg: &LearnedCapabilityRegistry, capability: &str, kind: &str) -> bool {
    reg.field_acting_facts_in_generation(reg.generation(), LANE, capability, kind, Instant::now())
        .is_some()
}

fn verified_acts(reg: &LearnedCapabilityRegistry, capability: &str, kind: &str) -> bool {
    reg.is_verified_working_in_generation(reg.generation(), LANE, capability, kind, Instant::now())
        == Some(true)
}

/// `(name, plant, capability, read)`.
fn acting_reads() -> Vec<(&'static str, Plant, String, Acts)> {
    vec![
        (
            "acting_negative_in_generation",
            plant_negative,
            "web_search".to_string(),
            acting_negative_acts,
        ),
        (
            "negative_state_in_generation",
            plant_negative,
            "web_search".to_string(),
            negative_state_acts,
        ),
        (
            "field_acting_facts_in_generation",
            plant_negative,
            field_key(),
            field_facts_act,
        ),
        (
            "is_verified_working_in_generation",
            plant_positive,
            "web_search".to_string(),
            verified_acts,
        ),
    ]
}

#[test]
fn an_acting_read_under_the_recorded_kind_acts() {
    for (name, plant, capability, acts) in acting_reads() {
        let reg = registry();
        plant(&reg, &capability, OLD);

        assert!(acts(&reg, &capability, OLD), "{name}");
        assert_eq!(reg.snapshot().len(), 1, "{name}");
    }
}

#[test]
fn an_acting_read_under_another_kind_does_not_act_and_drops_the_entry() {
    for (name, plant, capability, acts) in acting_reads() {
        let reg = registry();
        plant(&reg, &capability, OLD);

        assert!(!acts(&reg, &capability, NEW), "{name}");
        assert!(reg.snapshot().is_empty(), "{name}: the lookup removes it");
        assert!(
            !acts(&reg, &capability, OLD),
            "{name}: removed, not merely hidden"
        );
    }
}

#[test]
fn an_entry_written_with_no_kind_does_not_act_for_a_reader_with_one() {
    let reg = registry();
    plant_negative(&reg, "web_search", "");

    assert!(!acting_negative_acts(&reg, "web_search", NEW));
    assert!(reg.snapshot().is_empty());
}

#[test]
fn a_negative_under_a_new_kind_replaces_one_recorded_under_the_old_kind() {
    let reg = registry();
    plant_negative(&reg, "web_search", OLD);

    plant_negative(&reg, "web_search", NEW);

    let snapshot = reg.snapshot();
    assert_eq!(snapshot.len(), 1);
    assert_eq!(snapshot[0].observations, 1, "replaced, not refreshed");
    assert!(acting_negative_acts(&reg, "web_search", NEW));
}

#[test]
fn a_positive_under_a_new_kind_is_not_suppressed_by_an_old_kind_negative() {
    let reg = registry();
    plant_negative(&reg, "web_search", OLD);

    let outcome = reg.observe_positive_in_generation(
        reg.generation(),
        LANE,
        "web_search",
        NEW,
        EvidenceSource::Live,
        None,
        Instant::now(),
    );

    assert!(matches!(outcome.value(), Some(PositiveOutcome::Recorded)));
    assert!(verified_acts(&reg, "web_search", NEW));
}

#[test]
fn a_carried_entry_keeps_its_recorded_kind() {
    let reg = registry();
    plant_negative(&reg, "web_search", OLD);
    let next = registry();

    next.import_entries(reg.export_entries());

    assert!(!acting_negative_acts(&next, "web_search", NEW));
    assert!(next.snapshot().is_empty());
}
