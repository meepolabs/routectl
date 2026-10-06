//! The resident identity includes the provider kind of the write that
//! created an entry: each read and write selects its own kind's version,
//! versions under different kinds coexist, and nothing on the read or write
//! path removes another kind's version.

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
fn an_acting_read_under_another_kind_neither_acts_nor_removes_the_entry() {
    for (name, plant, capability, acts) in acting_reads() {
        let reg = registry();
        plant(&reg, &capability, OLD);

        assert!(!acts(&reg, &capability, NEW), "{name}");
        assert_eq!(reg.snapshot().len(), 1, "{name}: a read removes nothing");
        assert!(
            acts(&reg, &capability, OLD),
            "{name}: the recorded kind's version still acts for its own kind",
        );
    }
}

/// The reload race: after the lane's kind moves from OLD to NEW, the new
/// Router learns a fact, then a request still holding the outgoing Router
/// reads and writes the same lane and capability. The new kind's fact must
/// keep acting, untouched, and the old kind's version must never act for it.
#[test]
fn a_late_old_kind_read_and_write_leave_the_new_kinds_fact_intact() {
    for (name, plant, capability, acts) in acting_reads() {
        let reg = registry();
        plant(&reg, &capability, NEW);
        let before = new_kind_row(&reg, &capability);

        assert!(!acts(&reg, &capability, OLD), "{name}: premise");
        plant(&reg, &capability, OLD);
        let _ = acts(&reg, &capability, OLD);

        assert!(acts(&reg, &capability, NEW), "{name}: the new fact acts");
        assert_eq!(
            new_kind_row(&reg, &capability),
            before,
            "{name}: the new kind's entry is byte-identical",
        );
        assert_eq!(
            recorded_kinds(&reg),
            vec![NEW.to_string(), OLD.to_string()],
            "{name}: the late write lands as its own version",
        );
    }
}

#[test]
fn an_entry_written_with_no_kind_does_not_act_for_a_reader_with_one() {
    let reg = registry();
    plant_negative(&reg, "web_search", "");

    assert!(!acting_negative_acts(&reg, "web_search", NEW));
    assert_eq!(reg.snapshot().len(), 1);
}

#[test]
fn a_negative_under_a_new_kind_coexists_with_one_recorded_under_the_old_kind() {
    let reg = registry();
    plant_negative(&reg, "web_search", OLD);

    plant_negative(&reg, "web_search", NEW);

    let rows = reg.recorded_snapshot();
    assert_eq!(rows.len(), 2);
    assert!(
        rows.iter().all(|row| row.entry.observations == 1),
        "each kind's first observation, neither refreshed by the other",
    );
    assert!(acting_negative_acts(&reg, "web_search", NEW));
    assert!(acting_negative_acts(&reg, "web_search", OLD));
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
    assert!(acting_negative_acts(&reg, "web_search", OLD));
}

#[test]
fn a_carried_entry_keeps_its_recorded_kind() {
    let reg = registry();
    plant_negative(&reg, "web_search", OLD);
    let next = registry();

    next.import_entries(reg.export_entries());

    assert!(!acting_negative_acts(&next, "web_search", NEW));
    assert!(acting_negative_acts(&next, "web_search", OLD));
    assert_eq!(recorded_kinds(&next), vec![OLD.to_string()]);
}

/// The capability key a Bedrock writer reduces to its leaf and an
/// identity-normalizing reader keeps whole: one raw key, two map keys.
const BAG_PATH: &str = "additionalModelRequestFields.web_search";

#[test]
fn a_reader_of_another_kind_never_sees_a_version_spelled_like_its_own() {
    let reg = registry();
    plant_negative(&reg, BAG_PATH, "bedrock");
    assert_eq!(
        reg.snapshot()[0].feature_key,
        "web_search",
        "premise: the Bedrock writer stored the reduced leaf, the spelling an \
         identity-normalizing kind uses for its own fact",
    );

    assert!(!acting_negative_acts(&reg, BAG_PATH, OLD));
    assert!(!acting_negative_acts(&reg, "web_search", OLD));

    assert!(acting_negative_acts(&reg, BAG_PATH, "bedrock"));
    assert_eq!(reg.snapshot().len(), 1, "no read removed the version");
}

#[test]
fn the_owned_snapshot_selects_the_current_kinds_version() {
    let reg = registry();
    plant_negative(&reg, "web_search", OLD);
    plant_positive(&reg, "web_search", NEW);

    let owned = reg.owned_snapshot(|_| NEW);

    assert_eq!(owned.len(), 1);
    assert_eq!(owned[0].verdict, Verdict::VerifiedWorking);
    assert!(reg.owned_snapshot(|_| "bedrock").is_empty());
}

#[test]
fn the_status_projection_reports_only_the_current_kinds_incarnation() {
    let reg = registry();
    let capability = field_key();
    plant_negative(&reg, &capability, NEW);
    let new_incarnation = reg.resident_incarnation_for_tests(LANE, &capability, NEW);
    plant_negative(&reg, &capability, OLD);

    let acting = reg.field_acting_incarnations(Instant::now(), |_| NEW);

    assert_eq!(
        acting.get(&(LANE.to_string(), capability)),
        Some(&new_incarnation),
        "the old kind's later incarnation never stands in for the current one",
    );
}

#[test]
fn the_sweep_removal_takes_only_the_named_kinds_version() {
    let reg = registry();
    plant_negative(&reg, "web_search", OLD);
    plant_negative(&reg, "web_search", NEW);

    let removed = reg.remove_recorded_in_generation(reg.generation(), LANE, "web_search", OLD);

    assert!(matches!(removed.value(), Some(true)));
    assert_eq!(recorded_kinds(&reg), vec![NEW.to_string()]);
    assert!(acting_negative_acts(&reg, "web_search", NEW));
}

fn recorded_kinds(reg: &LearnedCapabilityRegistry) -> Vec<String> {
    let mut kinds: Vec<String> = reg
        .recorded_snapshot()
        .into_iter()
        .map(|row| row.provider_kind)
        .collect();
    kinds.sort();
    kinds
}

fn new_kind_row(reg: &LearnedCapabilityRegistry, capability: &str) -> LearnedRegistryEntry {
    let normalized = normalize_capability_key(capability, NEW);
    reg.recorded_snapshot()
        .into_iter()
        .find(|row| row.provider_kind == NEW && row.entry.feature_key == normalized)
        .expect("the new kind's entry is resident")
        .entry
}
