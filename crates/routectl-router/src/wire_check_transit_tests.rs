use routectl_core::wire_check::WirePath;

use super::{
    CheckOutcome, LaneTransitRow, TRANSIT_LANE_CAP, TRANSIT_PATH_CAP, TRANSIT_PATH_SLOTS,
    TransitLane, WireCheckState,
};
use crate::state_key::StateKey;

fn lane(provider_entry: &str) -> StateKey {
    StateKey::fixture(provider_entry)
}

fn path(key: &str) -> WirePath {
    WirePath::from_segments([key])
}

fn row_for<'r>(rows: &'r [LaneTransitRow], wanted: &TransitLane) -> Option<&'r LaneTransitRow> {
    rows.iter().find(|row| &row.lane == wanted)
}

#[test]
fn each_outcome_bumps_only_the_transit_counts_it_earns() {
    type Row = (
        &'static str,
        CheckOutcome,
        &'static [&'static str],
        Option<(u64, u64)>,
        &'static [(&'static str, u64)],
    );
    let rows: [Row; 4] = [
        (
            "clean bumps checked only",
            CheckOutcome::Clean,
            &[],
            Some((1, 0)),
            &[],
        ),
        (
            "loss bumps checked, changed and each path",
            CheckOutcome::Loss,
            &["stop_reason", "usage", "stop_reason"],
            Some((1, 1)),
            &[("/stop_reason", 2), ("/usage", 1)],
        ),
        (
            "budget bumps nothing",
            CheckOutcome::Budget,
            &["usage"],
            None,
            &[],
        ),
        (
            "incomplete bumps nothing",
            CheckOutcome::Incomplete,
            &[],
            None,
            &[],
        ),
    ];
    for (name, outcome, findings, expected, expected_paths) in rows {
        let state = WireCheckState::default();
        let key = lane("p0");
        let findings: Vec<WirePath> = findings.iter().map(|k| path(k)).collect();

        state.record_transit(Some(&key), outcome, findings.iter());

        let rows = state.transit_snapshot().lanes;
        let Some((checked, changed)) = expected else {
            assert!(rows.is_empty(), "{name}: {rows:?}");
            continue;
        };
        assert_eq!(rows.len(), 1, "{name}: {rows:?}");
        let row = &rows[0];
        assert_eq!(row.lane, TransitLane::Lane(key), "{name}");
        assert_eq!((row.checked, row.changed), (checked, changed), "{name}");
        let paths: Vec<(&str, u64)> = row.paths.iter().map(|(p, c)| (p.as_str(), *c)).collect();
        assert_eq!(paths, expected_paths, "{name}");
        assert_eq!(row.other_paths, 0, "{name}");
    }
}

#[test]
fn a_target_with_no_learned_lane_is_counted_under_no_lane() {
    let state = WireCheckState::default();
    let lost = path("usage");

    state.record_transit(None, CheckOutcome::Loss, [&lost]);
    state.record_transit(None, CheckOutcome::Clean, std::iter::empty());

    assert!(state.transit_snapshot().lanes.is_empty());
    state.record_transit(Some(&lane("p0")), CheckOutcome::Loss, [&lost]);
    assert_eq!(
        state.transit_snapshot().lanes.len(),
        1,
        "control: the same call with a lane is counted"
    );
}

#[test]
fn lanes_past_the_cap_count_under_other_and_sums_match_the_totals() {
    let state = WireCheckState::default();
    let lost = path("usage");
    let lanes: Vec<StateKey> = (0..=TRANSIT_LANE_CAP + 1)
        .map(|i| lane(&format!("p{i:03}")))
        .collect();
    for (i, key) in lanes.iter().enumerate() {
        let outcome = if i % 2 == 0 {
            CheckOutcome::Loss
        } else {
            CheckOutcome::Clean
        };
        state.record_transit(Some(key), outcome, [&lost]);
    }
    state.record_transit(Some(&lanes[0]), CheckOutcome::Clean, std::iter::empty());

    let rows = state.transit_snapshot().lanes;

    assert_eq!(rows.len(), TRANSIT_LANE_CAP + 1);
    let other = row_for(&rows, &TransitLane::Other).expect("an overflow row");
    assert_eq!(rows.last(), Some(other), "the overflow row comes last");
    assert_eq!((other.checked, other.changed), (2, 1));
    assert!(
        row_for(
            &rows,
            &TransitLane::Lane(lanes[TRANSIT_LANE_CAP - 1].clone())
        )
        .is_some(),
        "the 256th lane is named"
    );
    assert!(
        row_for(&rows, &TransitLane::Lane(lanes[TRANSIT_LANE_CAP].clone())).is_none(),
        "the 257th lane is not named"
    );
    let first = row_for(&rows, &TransitLane::Lane(lanes[0].clone())).expect("first lane named");
    assert_eq!(
        (first.checked, first.changed),
        (2, 1),
        "a named lane keeps counting"
    );
    let checked: u64 = rows.iter().map(|r| r.checked).sum();
    let changed: u64 = rows.iter().map(|r| r.changed).sum();
    let findings: u64 = rows
        .iter()
        .map(|r| r.paths.iter().map(|(_, c)| c).sum::<u64>() + r.other_paths)
        .sum();
    let recorded = u64::try_from(lanes.len()).expect("small");
    assert_eq!(checked, recorded + 1);
    assert_eq!(changed, recorded.div_ceil(2));
    assert_eq!(findings, changed);
}

fn named_plus_other(row: &LaneTransitRow) -> u64 {
    row.paths.iter().map(|(_, c)| c).sum::<u64>() + row.other_paths
}

#[test]
fn a_row_names_the_cap_most_changed_paths_and_sums_the_rest_in_other() {
    let state = WireCheckState::default();
    let key = lane("p0");
    let paths: Vec<WirePath> = (0..=TRANSIT_PATH_CAP + 1)
        .map(|i| path(&format!("k{i:02}")))
        .collect();

    state.record_transit(Some(&key), CheckOutcome::Loss, paths.iter());
    state.record_transit(
        Some(&key),
        CheckOutcome::Loss,
        [&paths[0], &paths[TRANSIT_PATH_CAP]],
    );

    let rows = state.transit_snapshot().lanes;
    let row = row_for(&rows, &TransitLane::Lane(key)).expect("lane named");
    assert_eq!(row.paths.len(), TRANSIT_PATH_CAP);
    assert_eq!(
        row.paths[..2],
        [(paths[0].clone(), 2), (paths[TRANSIT_PATH_CAP].clone(), 2)],
        "most findings first, then by path"
    );
    assert!(
        row.paths
            .iter()
            .all(|(p, _)| p != &paths[TRANSIT_PATH_CAP - 1] && p != &paths[TRANSIT_PATH_CAP + 1]),
        "the two last one-off paths by order are not named: {:?}",
        row.paths
    );
    assert_eq!(row.other_paths, 2, "the two unnamed one-off paths");
    assert_eq!((row.checked, row.changed), (2, 2));
    let recorded = u64::try_from(paths.len() + 2).expect("small");
    assert_eq!(named_plus_other(row), recorded);
}

#[test]
fn a_path_first_seen_after_the_cap_that_changes_most_is_named_first() {
    let state = WireCheckState::default();
    let key = lane("p0");
    let one_offs: Vec<WirePath> = (0..TRANSIT_PATH_CAP)
        .map(|i| path(&format!("k{i:02}")))
        .collect();
    let frequent = path("z_frequent");
    for one_off in &one_offs {
        state.record_transit(Some(&key), CheckOutcome::Loss, [one_off]);
    }

    for _ in 0..5 {
        state.record_transit(Some(&key), CheckOutcome::Loss, [&frequent]);
    }

    let rows = state.transit_snapshot().lanes;
    let row = row_for(&rows, &TransitLane::Lane(key)).expect("lane named");
    assert_eq!(row.paths.len(), TRANSIT_PATH_CAP);
    assert_eq!(row.paths[0], (frequent, 5), "{:?}", row.paths);
    assert_eq!(row.other_paths, 1, "one one-off path is left unnamed");
    let recorded = u64::try_from(one_offs.len() + 5).expect("small");
    assert_eq!(row.changed, recorded);
    assert_eq!(named_plus_other(row), recorded);
}

#[test]
fn a_frequent_path_stays_tracked_past_the_slot_bound_with_its_exact_count() {
    // Arrange: every slot holds a one-off path, then a frequent path arrives
    // interleaved with twice as many fresh one-off paths, each evicting the
    // slot with the fewest findings.
    let state = WireCheckState::default();
    let key = lane("p0");
    let mut fresh = (0..).map(|i| path(&format!("n{i:04}")));
    for _ in 0..TRANSIT_PATH_SLOTS {
        let one_off = fresh.next().expect("endless");
        state.record_transit(Some(&key), CheckOutcome::Loss, [&one_off]);
    }
    let frequent = path("a_frequent");
    let rounds: u64 = 40;

    // Act
    for _ in 0..rounds {
        let noise = [
            fresh.next().expect("endless"),
            fresh.next().expect("endless"),
        ];
        state.record_transit(
            Some(&key),
            CheckOutcome::Loss,
            [&frequent, &noise[0], &noise[1]],
        );
    }

    // Assert
    let rows = state.transit_snapshot().lanes;
    let row = row_for(&rows, &TransitLane::Lane(key)).expect("lane named");
    let recorded = u64::try_from(TRANSIT_PATH_SLOTS).expect("small") + 3 * rounds;
    assert_eq!(
        row.paths[0],
        (frequent, rounds),
        "tracked since its first finding, so its count is exact: {:?}",
        row.paths
    );
    assert_eq!(named_plus_other(row), recorded);
}

#[test]
fn started_ms_is_stamped_when_the_state_is_built() {
    let before = super::unix_now_ms();
    let state = WireCheckState::default();
    let after = super::unix_now_ms();

    let started_ms = state.transit_snapshot().started_ms;

    assert!(
        (before..=after).contains(&started_ms),
        "{before} <= {started_ms} <= {after}"
    );
    std::thread::sleep(std::time::Duration::from_millis(2));
    assert_eq!(
        state.transit_snapshot().started_ms,
        started_ms,
        "a read does not restamp"
    );
}
