//! Tests for the registry's field-repair table: which install takes, when a
//! read fixes it, and what the rebuild scratch registry inherits.

use super::*;
use crate::router::FieldRepairTable;

const DECAY: Duration = Duration::from_hours(48);
const WINDOW: Duration = Duration::from_hours(1);

fn registry() -> LearnedCapabilityRegistry {
    LearnedCapabilityRegistry::new(DECAY, WINDOW, DEFAULT_MAX_ENTRIES)
}

/// A table distinguishable from a unit-test build's default by row count.
const fn installed_table() -> FieldRepairTable {
    FieldRepairTable::grounded_only()
}

/// Mutation check: make `field_repair_table` read without initializing
/// (`get().copied().unwrap_or_default()`) -> red, the install succeeds
/// after the read.
#[test]
fn the_first_read_fixes_the_table_against_a_later_install() {
    // Arrange
    let reg = registry();
    let read_first = reg.field_repair_table();

    // Act
    let late = reg.set_field_repair_table(installed_table());

    // Assert
    assert!(late.is_err(), "a read fixed the table");
    assert_eq!(reg.field_repair_table().row_count(), read_first.row_count());
}

#[test]
fn an_install_before_any_read_is_the_table_read_back() {
    // Arrange
    let reg = registry();

    // Act
    let installed = reg.set_field_repair_table(installed_table());

    // Assert
    assert!(installed.is_ok());
    assert_eq!(
        reg.field_repair_table().row_count(),
        installed_table().row_count()
    );
    assert_ne!(
        installed_table().row_count(),
        FieldRepairTable::default().row_count()
    );
}

#[test]
fn a_second_install_fails() {
    // Arrange
    let reg = registry();
    reg.set_field_repair_table(installed_table())
        .expect("first install");

    // Act
    let second = reg.set_field_repair_table(FieldRepairTable::default());

    // Assert
    assert!(second.is_err());
    assert_eq!(
        reg.field_repair_table().row_count(),
        installed_table().row_count()
    );
}

#[test]
fn the_rebuild_scratch_is_empty_and_carries_the_source_configuration() {
    // Arrange
    let source =
        LearnedCapabilityRegistry::new(Duration::from_hours(7), Duration::from_mins(3), 11);
    source
        .set_field_repair_table(installed_table())
        .expect("install");
    let scope = BetaSeedScope::new("bedrock", &["fx-scratch"]);
    source.set_seed_scope(scope);
    source.observe(
        "lane",
        "computer_use",
        "openai-compat",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        Instant::now(),
    );
    assert!(
        !source.is_empty(),
        "the source holds an entry to leave behind"
    );

    // Act
    let scratch = source.empty_rebuild_scratch();

    // Assert
    assert!(scratch.is_empty());
    assert_eq!(scratch.decay(), Duration::from_hours(7));
    assert_eq!(scratch.inferred_window(), Duration::from_mins(3));
    assert_eq!(scratch.max_entries(), 11);
    assert_eq!(scratch.seed_scope(), scope);
    assert_eq!(
        scratch.field_repair_table().row_count(),
        installed_table().row_count()
    );
    assert!(
        scratch
            .set_field_repair_table(FieldRepairTable::default())
            .is_err(),
        "the scratch's table is fixed like the source's",
    );
}
