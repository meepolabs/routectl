//! Opening-tier selection: anchor before calibration before raw, the
//! provisional warm path, the stable labels, and the lane conversions the
//! handler feeds the selector.

use std::cell::Cell;

use routectl_router::{DispatchMeta, OpeningLane};

use super::super::tests::{grown, history, lane, request};
use super::super::{
    AnchorKey, AnchorLane, AnchorRecord, ContextAnchorStore, MissReason, RequestIdentity,
    TurnOutcome,
};
use super::{
    AnchorProbe, OpeningLaneBasis, OpeningReason, OpeningSelection, OpeningSource, select_opening,
};

const RAW: u64 = 10_000;
const PRIOR_ACTUAL: u64 = 50_000;

/// A calibrator that corrects every lane by 2x and counts its calls.
fn doubling(calls: &Cell<u32>) -> impl FnOnce(&AnchorLane, u64) -> Option<u64> + '_ {
    move |_, raw| {
        calls.set(calls.get() + 1);
        Some(raw * 2)
    }
}

fn no_factor(_: &AnchorLane, _: u64) -> Option<u64> {
    None
}

/// A published anchor record for the base history on `lane()`, plus the
/// identity of the grown next request measured against it.
fn anchored_turn() -> (AnchorRecord, RequestIdentity) {
    let store = ContextAnchorStore::new();
    let key = AnchorKey::new("session", "claude-opus").expect("key within bounds");
    store
        .reserve_turn(key.clone())
        .measure(&request(history()), None)
        .settle(TurnOutcome::Completed {
            served_lane: lane(),
            cache_inclusive_input: Some(PRIOR_ACTUAL),
        });
    let ticket = store.reserve_turn(key);
    let record = ticket.current_record().expect("record published");
    let pending = ticket.measure(&request(grown(&history())), Some(record.message_count()));
    ((*record).clone(), pending.identity().clone())
}

fn cold_identity() -> RequestIdentity {
    RequestIdentity::measure(&request(history()), None)
}

fn anchored_opening() -> u64 {
    let (record, identity) = anchored_turn();
    super::super::anchored_input(
        record.actual_input(),
        record.normalized_estimate(),
        identity.normalized_estimate(),
    )
}

// ------------------------------------------------------------ tier order

#[test]
fn a_verified_anchor_wins_over_calibration_and_is_not_corrected() {
    // Arrange
    let (record, identity) = anchored_turn();
    let calls = Cell::new(0);

    // Act
    let selection = select_opening(
        Some(AnchorProbe {
            record: Some(&record),
            identity: &identity,
        }),
        OpeningLaneBasis::Served(&lane()),
        RAW,
        doubling(&calls),
    );

    // Assert
    assert_eq!(
        selection,
        OpeningSelection {
            tokens: anchored_opening(),
            source: OpeningSource::Anchor,
            reason: OpeningReason::AnchorHit,
            provisional: false,
        }
    );
    assert_eq!(calls.get(), 0, "an anchored opening is never re-corrected");
}

#[test]
fn a_cold_anchor_falls_to_the_calibrated_estimate() {
    let identity = cold_identity();
    let calls = Cell::new(0);

    let selection = select_opening(
        Some(AnchorProbe {
            record: None,
            identity: &identity,
        }),
        OpeningLaneBasis::Served(&lane()),
        RAW,
        doubling(&calls),
    );

    assert_eq!(selection.tokens, RAW * 2);
    assert_eq!(selection.source, OpeningSource::Calibrated);
    assert_eq!(
        selection.reason,
        OpeningReason::AnchorMiss(MissReason::Cold)
    );
    assert_eq!(calls.get(), 1);
}

#[test]
fn a_cold_anchor_without_a_factor_falls_to_raw() {
    let identity = cold_identity();

    let selection = select_opening(
        Some(AnchorProbe {
            record: None,
            identity: &identity,
        }),
        OpeningLaneBasis::Served(&lane()),
        RAW,
        no_factor,
    );

    assert_eq!(selection.tokens, RAW);
    assert_eq!(selection.source, OpeningSource::Raw);
    assert_eq!(
        selection.reason,
        OpeningReason::AnchorMiss(MissReason::Cold)
    );
}

#[test]
fn an_anchor_from_another_upstream_model_is_rejected() {
    let (record, identity) = anchored_turn();
    let repointed = AnchorLane {
        upstream_model: "glm-5".into(),
        ..lane()
    };

    let selection = select_opening(
        Some(AnchorProbe {
            record: Some(&record),
            identity: &identity,
        }),
        OpeningLaneBasis::Served(&repointed),
        RAW,
        no_factor,
    );

    assert_eq!(selection.source, OpeningSource::Raw);
    assert_eq!(
        selection.reason,
        OpeningReason::AnchorMiss(MissReason::LaneChanged)
    );
}

#[test]
fn an_anchor_from_another_publication_is_rejected() {
    let (record, identity) = anchored_turn();
    let republished = AnchorLane {
        generation: lane().generation + 1,
        ..lane()
    };
    let calls = Cell::new(0);

    let selection = select_opening(
        Some(AnchorProbe {
            record: Some(&record),
            identity: &identity,
        }),
        OpeningLaneBasis::Served(&republished),
        RAW,
        doubling(&calls),
    );

    assert_eq!(selection.source, OpeningSource::Calibrated);
    assert_eq!(
        selection.reason,
        OpeningReason::AnchorMiss(MissReason::GenerationChanged)
    );
}

#[test]
fn an_unkeyed_turn_skips_the_anchor_tier() {
    let calls = Cell::new(0);

    let selection = select_opening(
        None,
        OpeningLaneBasis::Served(&lane()),
        RAW,
        doubling(&calls),
    );

    assert_eq!(selection.source, OpeningSource::Calibrated);
    assert_eq!(selection.reason, OpeningReason::Unanchored);
}

/// Selection for an unresolved lane, with an anchor that WOULD hit and a
/// calibrator that WOULD correct, so a raw result proves neither ran.
fn select_unresolved(basis: OpeningLaneBasis<'_>, calls: &Cell<u32>) -> OpeningSelection {
    let (record, identity) = anchored_turn();
    select_opening(
        Some(AnchorProbe {
            record: Some(&record),
            identity: &identity,
        }),
        basis,
        RAW,
        doubling(calls),
    )
}

#[test]
fn an_unresolved_head_uses_raw_provisionally_and_never_consults_calibration() {
    let calls = Cell::new(0);

    let selection = select_unresolved(OpeningLaneBasis::UnresolvedHead, &calls);

    assert_eq!(
        selection,
        OpeningSelection {
            tokens: RAW,
            source: OpeningSource::Raw,
            reason: OpeningReason::LaneUnresolved,
            provisional: true,
        }
    );
    assert_eq!(calls.get(), 0);
}

#[test]
fn an_unresolved_served_lane_uses_raw_finally_and_never_consults_calibration() {
    let calls = Cell::new(0);

    let selection = select_unresolved(OpeningLaneBasis::UnresolvedServed, &calls);

    assert_eq!(
        selection,
        OpeningSelection {
            tokens: RAW,
            source: OpeningSource::Raw,
            reason: OpeningReason::LaneUnresolved,
            provisional: false,
        }
    );
    assert_eq!(calls.get(), 0);
}

#[test]
fn calibration_is_asked_for_the_selected_lane_only() {
    let identity = cold_identity();
    let seen = Cell::new(None::<&'static str>);
    let head = AnchorLane {
        model: "opus".into(),
        ..lane()
    };

    let _ = select_opening(
        Some(AnchorProbe {
            record: None,
            identity: &identity,
        }),
        OpeningLaneBasis::Head(&head),
        RAW,
        |asked, _| {
            seen.set(Some(if asked == &head { "head" } else { "other" }));
            None
        },
    );

    assert_eq!(seen.get(), Some("head"));
}

// --------------------------------------------------------- provisional

#[test]
fn a_head_lane_selection_is_provisional_on_every_tier() {
    let (record, identity) = anchored_turn();
    let cold = cold_identity();
    let calls = Cell::new(0);

    let anchored = select_opening(
        Some(AnchorProbe {
            record: Some(&record),
            identity: &identity,
        }),
        OpeningLaneBasis::Head(&lane()),
        RAW,
        no_factor,
    );
    let calibrated = select_opening(
        Some(AnchorProbe {
            record: None,
            identity: &cold,
        }),
        OpeningLaneBasis::Head(&lane()),
        RAW,
        doubling(&calls),
    );
    let raw = select_opening(None, OpeningLaneBasis::Head(&lane()), RAW, no_factor);

    assert_eq!(anchored.source, OpeningSource::Anchor);
    assert!(anchored.provisional);
    assert_eq!(calibrated.source, OpeningSource::Calibrated);
    assert!(calibrated.provisional);
    assert_eq!(raw.source, OpeningSource::Raw);
    assert!(raw.provisional);
}

#[test]
fn a_served_lane_selection_is_final() {
    let selection = select_opening(None, OpeningLaneBasis::Served(&lane()), RAW, no_factor);

    assert!(!selection.provisional);
}

#[test]
fn provisional_is_set_by_timing_alone_and_source_and_reason_are_not() {
    // Every pre-dispatch basis is provisional and every post-dispatch one is
    // final, while the same inputs give the same source and reason on
    // either side of dispatch.
    let calls = Cell::new(0);
    let before = [
        select_opening(None, OpeningLaneBasis::Head(&lane()), RAW, no_factor),
        select_unresolved(OpeningLaneBasis::UnresolvedHead, &calls),
    ];
    let after = [
        select_opening(None, OpeningLaneBasis::Served(&lane()), RAW, no_factor),
        select_unresolved(OpeningLaneBasis::UnresolvedServed, &calls),
    ];

    assert!(before.iter().all(|s| s.provisional));
    assert!(after.iter().all(|s| !s.provisional));
    for (pre, post) in before.iter().zip(&after) {
        assert_eq!(
            (pre.tokens, pre.source, pre.reason),
            (post.tokens, post.source, post.reason)
        );
    }
}

#[test]
fn head_and_served_constructors_pin_the_timing_of_an_unresolved_lane() {
    let resolved = lane();

    let bases = [
        OpeningLaneBasis::head(Some(&resolved)),
        OpeningLaneBasis::head(None),
        OpeningLaneBasis::served(Some(&resolved)),
        OpeningLaneBasis::served(None),
    ]
    .map(|basis| {
        let named = match basis {
            OpeningLaneBasis::Head(_) => "head",
            OpeningLaneBasis::UnresolvedHead => "unresolved_head",
            OpeningLaneBasis::Served(_) => "served",
            OpeningLaneBasis::UnresolvedServed => "unresolved_served",
        };
        (named, basis.is_pre_dispatch())
    });

    assert_eq!(
        bases,
        [
            ("head", true),
            ("unresolved_head", true),
            ("served", false),
            ("unresolved_served", false),
        ]
    );
}

// --------------------------------------------------------------- labels

#[test]
fn source_labels_are_stable() {
    let labels = [
        OpeningSource::Anchor,
        OpeningSource::Calibrated,
        OpeningSource::Raw,
    ]
    .map(OpeningSource::as_str);

    assert_eq!(labels, ["anchor", "calibrated", "raw"]);
}

#[test]
fn reason_labels_are_stable_and_distinct() {
    let labels = [
        OpeningReason::AnchorHit,
        OpeningReason::AnchorMiss(MissReason::Cold),
        OpeningReason::AnchorMiss(MissReason::LaneChanged),
        OpeningReason::AnchorMiss(MissReason::GenerationChanged),
        OpeningReason::AnchorMiss(MissReason::HistoryShrank),
        OpeningReason::AnchorMiss(MissReason::PrefixChanged),
        OpeningReason::Unanchored,
        OpeningReason::LaneUnresolved,
    ]
    .map(OpeningReason::as_str);

    assert_eq!(
        labels,
        [
            "anchor_hit",
            "cold",
            "lane_changed",
            "generation_changed",
            "history_shrank",
            "prefix_changed",
            "unanchored",
            "lane_unresolved",
        ]
    );
}

// ----------------------------------------------------- lane conversions

#[test]
fn a_router_opening_lane_converts_field_for_field() {
    let opening = OpeningLane {
        provider_kind: "openai-compat",
        nickname: "glm".into(),
        upstream_model: "glm-4.6".into(),
        generation: 7,
    };

    assert_eq!(AnchorLane::from(opening), lane());
}

#[test]
fn a_served_lane_carries_the_wire_model_and_the_given_generation() {
    let mut meta = DispatchMeta::for_alias("claude-opus");
    meta.served_provider_kind = Some("openai-compat".into());
    meta.served_model = Some("glm".into());
    meta.served_upstream = Some("glm-4.6".into());
    meta.served_seat = Some("seat-a".into());

    let served = AnchorLane::served(&meta, 7);

    assert_eq!(served, Some(lane()));
}

#[test]
fn a_served_lane_ignores_the_seat() {
    let mut meta = DispatchMeta::for_alias("claude-opus");
    meta.served_provider_kind = Some("openai-compat".into());
    meta.served_model = Some("glm".into());
    meta.served_upstream = Some("glm-4.6".into());
    meta.served_seat = Some("seat-a".into());
    let first = AnchorLane::served(&meta, 7);
    meta.served_seat = Some("seat-b".into());

    let second = AnchorLane::served(&meta, 7);

    assert_eq!(first, second);
}

#[test]
fn a_dispatch_missing_any_lane_field_forms_no_served_lane() {
    let complete = || {
        let mut meta = DispatchMeta::for_alias("claude-opus");
        meta.served_provider_kind = Some("openai-compat".into());
        meta.served_model = Some("glm".into());
        meta.served_upstream = Some("glm-4.6".into());
        meta
    };
    let mut no_kind = complete();
    no_kind.served_provider_kind = None;
    let mut no_model = complete();
    no_model.served_model = None;
    let mut no_upstream = complete();
    no_upstream.served_upstream = None;

    assert!(
        AnchorLane::served(&complete(), 7).is_some(),
        "positive control"
    );
    assert_eq!(AnchorLane::served(&no_kind, 7), None);
    assert_eq!(AnchorLane::served(&no_model, 7), None);
    assert_eq!(AnchorLane::served(&no_upstream, 7), None);
}
