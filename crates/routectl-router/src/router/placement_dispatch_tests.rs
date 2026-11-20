//! Router-level tests for the placement record and counters: which won picks
//! make one, that it rides every target of the placing pool into the dispatch
//! meta, and that each counter key moves by exactly one per placement.
//!
//! Reuses the three-seat Anthropic login pool of the re-placement tests.

use std::sync::Barrier;
use std::time::Instant;

use routectl_core::{EligibilityReason, UpstreamReason};

use super::placement_record::{PlacementCounters, PlacementTotals};
use super::replacement_dispatch_tests::{
    LONG, MEMBERS, OPUS, POOL, SESSION, SeatStub, TURNS, pin_key, pin_to, poll, pool_router,
    state_key,
};
use super::sticky::PinWrite;
use super::*;
use crate::config::{PoolEntry, ProviderEntry, SeatSelection};
use crate::quota::placement::Tier;
use crate::runtime_state::UnusableCause;
use crate::seat_pool::SeatTarget;

fn account(member: &str) -> String {
    crate::seat_pool::seat_identity(Some(&super::replacement_dispatch_tests::secret_ref(member)))
        .expect("a login account")
}

fn totals(router: &Router) -> PlacementTotals {
    router.metrics.placement.totals()
}

/// The dispatch chain for `session` (`None` for keyless).
fn chain(router: &Router, session: Option<&str>) -> Vec<DispatchTarget> {
    router.dispatch_chain("m", session).expect("chain resolves")
}

/// The one record every target of the chain carries, asserting they agree.
fn chain_record(targets: &[DispatchTarget]) -> Option<PlacementRecord> {
    let first = targets[0].placement.as_deref().cloned();
    for target in targets {
        assert_eq!(target.placement.as_deref(), first.as_ref(), "{target:?}");
    }
    first
}

impl std::fmt::Debug for DispatchTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DispatchTarget")
            .field("state_key", &self.state_key)
            .field("placement", &self.placement)
            .finish_non_exhaustive()
    }
}

// ---- which picks record ----

#[test]
fn a_keyed_birth_records_on_every_target_and_the_meta_of_a_sibling_that_serves() {
    // Arrange: seat-c paces best.
    let router = pool_router(None);
    poll(&router, "seat-b", 60);
    poll(&router, "seat-c", 10);

    // Act
    let targets = chain(&router, Some(SESSION));
    let mut meta = DispatchMeta::for_alias("m");
    meta.mark_target(&targets[0], "m");
    meta.mark_target(&targets[1], "m");

    // Assert
    let record = chain_record(&targets).expect("a birth records");
    assert_eq!(
        record,
        PlacementRecord::new(PlacementEvent::Birth, Tier::Roomy, &account("seat-c"), None)
    );
    assert_eq!(targets.len(), MEMBERS.len());
    assert_eq!(
        meta.placement,
        Some(record),
        "the serving sibling carries it"
    );
}

#[test]
fn the_meta_keeps_the_first_placement_whatever_target_is_marked_later() {
    // Arrange: a birth on seat-c, a second pool's record, and a target
    // outside any pool.
    let router = pool_router(None);
    poll(&router, "seat-c", 10);
    let placed = chain(&router, Some(SESSION)).swap_remove(0);
    let first = placed
        .placement
        .as_deref()
        .cloned()
        .expect("a birth records");
    let mut other_pool = placed.clone();
    other_pool.placement = Some(Arc::new(PlacementRecord::new(
        PlacementEvent::Keyless,
        Tier::Unread,
        &account("seat-a"),
        None,
    )));
    let mut outside_any_pool = placed.clone();
    outside_any_pool.placement = None;
    // (row, targets marked after the placing one, in order)
    let rows: [(&str, Vec<&DispatchTarget>); 3] = [
        ("then a target outside any pool", vec![&outside_any_pool]),
        ("then a second placing pool", vec![&other_pool]),
        (
            "then both",
            vec![&outside_any_pool, &other_pool, &outside_any_pool],
        ),
    ];
    for (row, later) in rows {
        let mut meta = DispatchMeta::for_alias("m");

        // Act
        meta.mark_target(&placed, "m");
        for target in later {
            meta.mark_target(target, "m");
        }

        // Assert
        assert_eq!(meta.placement.as_ref(), Some(&first), "{row}");
    }
}

#[test]
fn a_warm_stay_records_nothing_and_counts_nothing() {
    // Arrange
    let router = pool_router(None);
    poll(&router, "seat-c", 10);
    pin_to(&router, SESSION, "seat-a");

    // Act
    let targets = chain(&router, Some(SESSION));

    // Assert
    assert_eq!(chain_record(&targets), None);
    assert_eq!(totals(&router), PlacementCounters::default().totals());
}

#[test]
fn a_move_records_its_trigger_and_the_moved_to_seats_arm() {
    // Arrange: seat-c is the only sibling read, at low room.
    let router = pool_router(None);
    poll(&router, "seat-b", 95);
    poll(&router, "seat-c", 95);
    pin_to(&router, SESSION, "seat-a");
    assert!(router.force_open_breaker(&state_key("seat-a"), LONG));

    // Act
    let record = chain_record(&chain(&router, Some(SESSION))).expect("a move records");

    // Assert
    assert_eq!(record.event, PlacementEvent::Move);
    assert_eq!(record.arm, "low_room");
    assert_eq!(record.trigger, Some("breaker_open"));
    assert_eq!(
        record.to_value()["seat"],
        router
            .sticky_pins
            .get(&pin_key(SESSION))
            .map(|pin| account(&pin.member))
            .expect("pinned")
            .as_str()
    );
}

#[test]
fn a_keyless_placement_on_a_read_pool_records_keyless() {
    // Arrange
    let router = pool_router(None);
    poll(&router, "seat-c", 10);

    // Act
    let record = chain_record(&chain(&router, None)).expect("a keyless placement records");

    // Assert
    assert_eq!(
        record,
        PlacementRecord::new(
            PlacementEvent::Keyless,
            Tier::Roomy,
            &account("seat-c"),
            None
        )
    );
    assert_eq!(totals(&router).births_of(Tier::Roomy), 1);
}

#[test]
fn a_keyless_fill_first_fallback_with_nothing_read_records_nothing() {
    // Arrange: login seats, but no reading anywhere.
    let router = pool_router(None);

    // Act
    let targets = chain(&router, None);

    // Assert
    assert_eq!(chain_record(&targets), None);
    assert_eq!(totals(&router), PlacementCounters::default().totals());
}

/// A sticky pool whose seats carry no login, so the meter can read none.
pub(super) fn dormant_router() -> Router {
    let mut seats: Vec<SeatTarget> = Vec::new();
    let mut providers = BTreeMap::new();
    for (member, _) in MEMBERS {
        let provider: Arc<dyn Provider> = Arc::new(SeatStub {
            id: member.to_string(),
        });
        seats.push(SeatTarget::new(
            member.to_string(),
            provider,
            Some(routectl_auth::SecretRef::Env("SEAT_KEY".into())),
            false,
        ));
        providers.insert(
            member.to_string(),
            ProviderEntry::openai_compat("https://api.example.com/v1", "env://SEAT_KEY"),
        );
    }
    let default_provider = seats[0].provider.clone().expect("seat 0 is built");
    let mut pools = BTreeMap::new();
    pools.insert(
        POOL.to_string(),
        PoolEntry::new(MEMBERS.iter().map(|(m, _)| (*m).to_string()).collect())
            .with_seat_selection(SeatSelection::StickyLeastLoaded),
    );
    let mut router = Router::new(Arc::new(Config {
        providers,
        pools,
        ..Config::default()
    }));
    let model = ResolvedModel::new("m", POOL, default_provider, OPUS).with_seats(Arc::from(seats));
    router.install_resolved_models(std::iter::once(("m".to_string(), Arc::new(model))).collect());
    router
}

#[test]
fn a_dormant_pool_records_and_counts_nothing_keyed_or_keyless() {
    // Arrange
    let router = dormant_router();

    // Act
    let keyed = chain(&router, Some(SESSION));
    let keyless = chain(&router, None);

    // Assert
    assert!(
        router.sticky_pins.get(&pin_key(SESSION)).is_some(),
        "premise: the keyed birth pinned"
    );
    assert_eq!(chain_record(&keyed), None);
    assert_eq!(chain_record(&keyless), None);
    assert_eq!(totals(&router), PlacementCounters::default().totals());
}

#[test]
fn a_fresh_router_and_store_record_an_unread_birth() {
    // Arrange: nothing carried from any earlier router.
    let router = pool_router(None);

    // Act
    let record = chain_record(&chain(&router, Some(SESSION))).expect("a birth records");

    // Assert
    assert_eq!(record.event, PlacementEvent::Birth);
    assert_eq!(record.arm, "unread");
    assert_eq!(totals(&router).births_of(Tier::Unread), 1);
}

// ---- concurrent turns ----

#[test]
fn concurrent_births_make_one_record_and_one_count() {
    let router = pool_router(None);
    let seats = router.resolved_models["m"].seats.clone().expect("a pool");
    for round in 0..20 {
        // Arrange
        let session = format!("birth-{round}");
        let before = totals(&router).births_of(Tier::Unread);
        let barrier = Barrier::new(TURNS);

        // Act
        let turns: Vec<(PinWrite, Option<PlacementRecord>)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..TURNS)
                .map(|_| {
                    scope.spawn(|| {
                        barrier.wait();
                        let (_, write, record) =
                            router.sticky_seat_placement(&seats, &pin_key(&session), "m", OPUS);
                        (write, record)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().expect("a turn completes"))
                .collect()
        });

        // Assert
        let recorded: Vec<_> = turns.iter().filter(|(_, r)| r.is_some()).collect();
        assert_eq!(recorded.len(), 1, "round {round}: {turns:?}");
        assert!(matches!(recorded[0].0, PinWrite::Born { .. }), "{turns:?}");
        assert!(
            turns
                .iter()
                .filter(|(w, _)| !matches!(w, PinWrite::Born { .. }))
                .all(|(_, r)| r.is_none()),
            "a loser records nothing: {turns:?}"
        );
        assert_eq!(totals(&router).births_of(Tier::Unread), before + 1);
    }
}

// ---- one count per single-source case ----

#[test]
fn a_birth_counts_once_under_its_arm_and_nowhere_else() {
    type Read = fn(&Router);
    let rows: [(&str, Read, Tier); 3] = [
        ("roomy", |r| poll(r, "seat-c", 10), Tier::Roomy),
        (
            "low_room",
            |r| {
                for (member, _) in MEMBERS {
                    poll(r, member, 95);
                }
            },
            Tier::LowRoom,
        ),
        ("unread", |_| {}, Tier::Unread),
    ];
    for (name, read, arm) in rows {
        // Arrange
        let router = pool_router(None);
        read(&router);

        // Act
        chain(&router, Some(SESSION));

        // Assert
        let after = totals(&router);
        for other in super::placement_record::ARMS {
            let want = u64::from(other == arm);
            assert_eq!(after.births_of(other), want, "row {name}: {other:?}");
        }
        assert_eq!(after.moves, [0; 6], "row {name}");
        assert_eq!(after.cap_forced_moves, 0, "row {name}");
    }
}

#[test]
fn a_move_counts_once_under_its_trigger_and_no_birth() {
    type Outage = fn(&Router);
    let rows: [(&str, Outage, EligibilityReason); 3] = [
        (
            "credential unusable",
            |r| {
                assert!(r.mark_credential_unusable_for_tests(
                    &state_key("seat-a"),
                    UnusableCause::Revoked
                ));
            },
            EligibilityReason::CredentialUnusable,
        ),
        (
            "parked",
            |r| {
                assert!(r.park_credential_for_tests(
                    &state_key("seat-a"),
                    UpstreamReason::CredentialExpired,
                    Instant::now() + LONG,
                ));
            },
            EligibilityReason::AccountParked,
        ),
        (
            "breaker open",
            |r| assert!(r.force_open_breaker(&state_key("seat-a"), LONG)),
            EligibilityReason::BreakerOpen,
        ),
    ];
    for (name, outage, trigger) in rows {
        // Arrange
        let router = pool_router(None);
        poll(&router, "seat-c", 10);
        pin_to(&router, SESSION, "seat-a");
        outage(&router);

        // Act
        let record = chain_record(&chain(&router, Some(SESSION)));

        // Assert
        assert_eq!(
            record.and_then(|r| r.trigger),
            Some(trigger.as_str()),
            "row {name}"
        );
        let after = totals(&router);
        for other in EligibilityReason::ALL {
            let want = u64::from(other == trigger);
            assert_eq!(after.moves_of(other), want, "row {name}: {other:?}");
        }
        assert_eq!(after.births, [0; 3], "row {name}");
        assert_eq!(after.cap_forced_moves, 0, "row {name}: an unread home");
    }
}

#[test]
fn a_breaker_open_move_is_cap_forced_only_off_a_low_room_home() {
    let rows = [("low_room home", 95, 1), ("roomy home", 10, 0)];
    for (name, home_percent, want) in rows {
        // Arrange
        let router = pool_router(None);
        poll(&router, "seat-a", home_percent);
        poll(&router, "seat-c", 10);
        pin_to(&router, SESSION, "seat-a");
        assert!(router.force_open_breaker(&state_key("seat-a"), LONG));

        // Act
        chain(&router, Some(SESSION));

        // Assert
        let after = totals(&router);
        assert_eq!(
            after.moves_of(EligibilityReason::BreakerOpen),
            1,
            "row {name}"
        );
        assert_eq!(after.cap_forced_moves, want, "row {name}");
    }
}

#[test]
fn a_low_room_home_parked_rather_than_open_is_not_cap_forced() {
    // Arrange
    let router = pool_router(None);
    poll(&router, "seat-a", 95);
    poll(&router, "seat-c", 10);
    pin_to(&router, SESSION, "seat-a");
    assert!(router.park_credential_for_tests(
        &state_key("seat-a"),
        UpstreamReason::CredentialExpired,
        Instant::now() + LONG,
    ));

    // Act
    chain(&router, Some(SESSION));

    // Assert
    let after = totals(&router);
    assert_eq!(after.moves_of(EligibilityReason::AccountParked), 1);
    assert_eq!(after.cap_forced_moves, 0);
}

// ---- the metrics snapshot line ----

#[test]
fn the_metrics_snapshot_carries_the_placement_counters() {
    // Arrange: one roomy birth and one cap-forced move.
    let router = pool_router(None);
    poll(&router, "seat-a", 95);
    poll(&router, "seat-c", 10);
    chain(&router, Some("born"));
    pin_to(&router, SESSION, "seat-a");
    assert!(router.force_open_breaker(&state_key("seat-a"), LONG));
    chain(&router, Some(SESSION));

    // Act
    let events = routectl_testkit::capture_events(|| router.log_metrics_snapshot());

    // Assert
    let snapshot = events
        .iter()
        .find(|e| e.message == "router metrics snapshot")
        .expect("one snapshot");
    let rows = [
        ("rc_placement_births_roomy", "1"),
        ("rc_placement_births_low_room", "0"),
        ("rc_placement_births_unread", "0"),
        ("rc_placement_moves_credential_unusable", "0"),
        ("rc_placement_moves_account_parked", "0"),
        ("rc_placement_moves_breaker_open", "1"),
        ("rc_placement_cap_forced_moves", "1"),
    ];
    for (field, want) in rows {
        assert_eq!(snapshot.field(field), Some(want), "field {field}");
    }
}
