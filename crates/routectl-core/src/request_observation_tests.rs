use super::{ObservationSnapshot, RequestObservation, ServedIdentity};

fn identity(model: &str) -> ServedIdentity {
    ServedIdentity {
        alias: Some("flow".into()),
        model: Some(model.into()),
        upstream: Some(format!("{model}-upstream")),
        provider: Some("prov".into()),
        provider_kind: Some("anthropic-api".into()),
        seat: Some("seat-a".into()),
    }
}

#[test]
fn fresh_slot_snapshot_is_empty() {
    assert_eq!(
        RequestObservation::new().snapshot(),
        ObservationSnapshot::default()
    );
}

#[test]
fn clones_share_one_state() {
    let kept = RequestObservation::new();
    let riding = kept.clone();

    riding.add_wire_losses(2);
    riding.add_local_refusal();
    riding.add_missing_terminal();
    riding.set_field_repairs(4);
    riding.note_translation_drop("lane-a", "class-a");
    riding.set_probe(true);
    riding.set_last_target(identity("m1"));

    let snap = kept.snapshot();
    assert_eq!(snap.wire_losses, 2);
    assert_eq!(snap.local_refusals, 1);
    assert_eq!(snap.missing_terminals, 1);
    assert_eq!(snap.field_repairs, 4);
    assert_eq!(snap.translation_drops, 1);
    assert!(snap.probe);
    assert_eq!(snap.last_target, Some(identity("m1")));
}

#[test]
fn independent_slots_bumped_from_separate_threads_never_see_each_other() {
    let first = RequestObservation::new();
    let second = RequestObservation::new();

    let bump = |slot: RequestObservation, times: u64, lane: &'static str| {
        std::thread::spawn(move || {
            for _ in 0..times {
                slot.add_wire_losses(1);
                slot.add_local_refusal();
                slot.add_missing_terminal();
            }
            slot.note_translation_drop(lane, "class");
        })
    };
    let a = bump(first.clone(), 3, "lane-a");
    let b = bump(second.clone(), 5, "lane-b");
    a.join().expect("first thread");
    b.join().expect("second thread");

    for (label, slot, expected) in [("first", &first, 3), ("second", &second, 5)] {
        let snap = slot.snapshot();
        assert_eq!(snap.wire_losses, expected, "{label} wire_losses");
        assert_eq!(snap.local_refusals, expected, "{label} local_refusals");
        assert_eq!(
            snap.missing_terminals, expected,
            "{label} missing_terminals"
        );
        assert_eq!(snap.translation_drops, 1, "{label} translation_drops");
    }
}

#[test]
fn note_translation_drop_is_true_once_per_distinct_pair() {
    let slot = RequestObservation::new();

    for (lane, class, first_sighting) in [
        ("lane-a", "class-a", true),
        ("lane-a", "class-a", false),
        ("lane-a", "class-b", true),
        ("lane-b", "class-a", true),
        ("lane-b", "class-a", false),
    ] {
        assert_eq!(
            slot.note_translation_drop(lane, class),
            first_sighting,
            "({lane}, {class})"
        );
    }

    assert_eq!(slot.snapshot().translation_drops, 3);
}

#[test]
fn a_new_slot_sees_a_pair_for_the_first_time_again() {
    let earlier = RequestObservation::new();
    assert!(earlier.note_translation_drop("lane-a", "class-a"));

    let later = RequestObservation::new();

    assert!(later.note_translation_drop("lane-a", "class-a"));
}

#[test]
fn note_lane_seen_is_true_once_per_lane() {
    let slot = RequestObservation::new();

    for (lane, first_sighting) in [("lane-a", true), ("lane-a", false), ("lane-b", true)] {
        assert_eq!(slot.note_lane_seen(lane), first_sighting, "{lane}");
    }
}

#[test]
fn add_wire_losses_accumulates() {
    let slot = RequestObservation::new();

    slot.add_wire_losses(2);
    slot.add_wire_losses(3);

    assert_eq!(slot.snapshot().wire_losses, 5);
}

#[test]
fn set_field_repairs_replaces_the_earlier_value() {
    let slot = RequestObservation::new();

    slot.set_field_repairs(4);
    slot.set_field_repairs(1);

    assert_eq!(slot.snapshot().field_repairs, 1);
}

#[test]
fn set_last_target_keeps_the_last_identity() {
    let slot = RequestObservation::new();

    slot.set_last_target(identity("m1"));
    slot.set_last_target(identity("m2"));

    assert_eq!(slot.snapshot().last_target, Some(identity("m2")));
}

#[test]
fn set_probe_can_clear_the_flag() {
    let slot = RequestObservation::new();

    slot.set_probe(true);
    slot.set_probe(false);

    assert!(!slot.snapshot().probe);
}

#[test]
fn snapshot_recovers_from_a_poisoned_lock() {
    let slot = RequestObservation::new();
    slot.add_local_refusal();
    let poisoner = slot.clone();
    let _ = std::thread::spawn(move || {
        let _guard = poisoner.state.lock().unwrap();
        panic!("poison the slot");
    })
    .join();

    slot.add_local_refusal();

    assert_eq!(slot.snapshot().local_refusals, 2);
}
