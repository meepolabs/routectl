//! The placement record's JSON shape and the counters' per-key bumps.

use routectl_core::EligibilityReason;
use serde_json::json;

use super::*;

#[test]
fn a_birth_record_holds_exactly_rule_event_arm_and_seat() {
    // Arrange
    let record = PlacementRecord::new(PlacementEvent::Birth, Tier::Roomy, "anthropic#b", None);

    // Act
    let value = record.to_value();

    // Assert
    assert_eq!(
        value,
        json!({"rule": "pace_v1", "event": "birth", "arm": "roomy", "seat": "anthropic#b"})
    );
}

#[test]
fn a_move_record_adds_only_its_trigger_token() {
    // Arrange
    let record = PlacementRecord::new(
        PlacementEvent::Move,
        Tier::LowRoom,
        "anthropic",
        Some(EligibilityReason::BreakerOpen),
    );

    // Act
    let value = record.to_value();

    // Assert
    assert_eq!(
        value,
        json!({
            "rule": "pace_v1",
            "event": "move",
            "arm": "low_room",
            "seat": "anthropic",
            "trigger": "breaker_open",
        })
    );
}

#[test]
fn every_event_and_arm_renders_its_closed_token() {
    let rows = [
        (PlacementEvent::Birth, Tier::Roomy, "birth", "roomy"),
        (PlacementEvent::Move, Tier::LowRoom, "move", "low_room"),
        (PlacementEvent::Keyless, Tier::Unread, "keyless", "unread"),
    ];
    for (event, arm, event_token, arm_token) in rows {
        let value = PlacementRecord::new(event, arm, "s", None).to_value();
        assert_eq!(value["event"], event_token, "row: {event_token}");
        assert_eq!(value["arm"], arm_token, "row: {arm_token}");
    }
}

#[test]
fn each_counter_key_counts_only_its_own_bumps() {
    // Arrange
    let counters = PlacementCounters::default();

    // Act
    counters.incr_birth(Tier::LowRoom);
    counters.incr_move(EligibilityReason::AccountParked, false);
    counters.incr_move(EligibilityReason::BreakerOpen, true);
    let totals = counters.totals();

    // Assert
    assert_eq!(totals.births_of(Tier::Roomy), 0);
    assert_eq!(totals.births_of(Tier::LowRoom), 1);
    assert_eq!(totals.births_of(Tier::Unread), 0);
    assert_eq!(totals.moves_of(EligibilityReason::CredentialUnusable), 0);
    assert_eq!(totals.moves_of(EligibilityReason::AccountParked), 1);
    assert_eq!(totals.moves_of(EligibilityReason::BreakerOpen), 1);
    assert_eq!(totals.cap_forced_moves, 1);
}
