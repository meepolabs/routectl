//! `Router::placement_status`: the status read of the pace rule.
//!
//! What needs a ROUTER is what the pure rule cannot show: that reading it
//! moves no routing state, that its rank-1 seats are the seats the next birth
//! draws from on the real eligibility snapshots, that an unavailable seat
//! loses its rank, that the configured upstream model reaches the scorer per
//! pool entry, and that its counters are the placement counters.
//!
//! Reuses the three-seat Anthropic login pool of the re-placement tests;
//! readings arrive as real usage bodies through `Router::observe_polled_usage`.

use std::collections::BTreeSet;
use std::time::Instant;

use routectl_core::{EligibilityReason, UpstreamReason};

use super::super::replacement_dispatch_tests::{
    LONG, MEMBERS, OPUS, POOL, SeatStub, open_lapsed, pin_key, pin_to, poll, pool_router,
    secret_ref, slot, state_key,
};
use super::super::*;
use super::{PlacementStatus, PoolPlacement};
use crate::config::{PoolEntry, ProviderEntry, ProviderRuntimePolicy, SeatSelection};
use crate::quota::freshness::ObservationStamp;
use crate::quota::placement::{Reason, Tier};
use crate::quota::usage_body::UsageVendor;
use crate::runtime_state::{CircuitPhase, GateDecision, UnusableCause};
use crate::seat_pool::SeatTarget;

const SONNET: &str = "claude-sonnet-4-5";

fn account(member: &str) -> String {
    crate::seat_pool::seat_identity(Some(&secret_ref(member))).expect("a login account")
}

fn status(router: &Router) -> PlacementStatus {
    router.placement_status(&ObservationStamp::now())
}

/// The one entry for `model`, asserting there is exactly one.
fn entry<'a>(status: &'a PlacementStatus, model: &str) -> &'a PoolPlacement {
    let mut found = status.pools.iter().filter(|pool| pool.model == model);
    let entry = found.next().unwrap_or_else(|| panic!("no {model} entry"));
    assert!(found.next().is_none(), "two {model} entries");
    entry
}

/// Each member's `(rank, reason)` in member order.
fn ranks(entry: &PoolPlacement) -> Vec<(Option<usize>, &'static str)> {
    entry
        .seats
        .iter()
        .map(|seat| (seat.rank, seat.reason))
        .collect()
}

/// The members ranked 1, by name.
fn rank_one(entry: &PoolPlacement) -> BTreeSet<String> {
    MEMBERS
        .iter()
        .zip(&entry.seats)
        .filter(|(_, seat)| seat.rank == Some(1))
        .map(|((member, _), _)| (*member).to_string())
        .collect()
}

/// The homes of `births` consecutive births on fresh sessions, by name.
fn birth_homes(router: &Router, births: usize) -> BTreeSet<String> {
    let seats = router.resolved_models["m"].seats.clone().expect("a pool");
    (0..births)
        .map(|n| {
            let (_, write, _) =
                router.sticky_seat_placement(&seats, &pin_key(&format!("birth-{n}")), "m", OPUS);
            match write {
                super::super::sticky::PinWrite::Born { home } => seats[home].provider_name.clone(),
                other => panic!("expected a birth, got {other:?}"),
            }
        })
        .collect()
}

/// Poll `member` with a five-hour window at `five_hour` percent (150 minutes
/// to run) and an opus-scoped weekly at `opus_weekly` percent (3 days to run).
fn poll_with_opus_weekly(router: &Router, member: &str, five_hour: u32, opus_weekly: u32) {
    let rfc = |offset: Duration| {
        (time::OffsetDateTime::now_utc() + offset)
            .format(&time::format_description::well_known::Rfc3339)
            .expect("an RFC 3339 rendering")
    };
    let body = format!(
        r#"{{"five_hour": {{"utilization": {five_hour}, "resets_at": "{}"}}, "seven_day": null,
            "seven_day_opus": {{"utilization": {opus_weekly}, "resets_at": "{}"}}}}"#,
        rfc(Duration::from_mins(150)),
        rfc(Duration::from_hours(72)),
    );
    let account = account(member);
    let generation = router.acquire_credential_generation(&account);
    router
        .observe_polled_usage(
            &account,
            UsageVendor::Anthropic,
            body.as_bytes(),
            generation,
        )
        .expect("the poll is accepted");
}

/// The three-seat login pool `selection` over models `(nickname, upstream)`.
fn pool_router_with(selection: SeatSelection, models: &[(&str, &str)]) -> Router {
    let mut seats: Vec<SeatTarget> = Vec::new();
    let mut providers = BTreeMap::new();
    for (member, _) in MEMBERS {
        let provider: Arc<dyn Provider> = Arc::new(SeatStub {
            id: member.to_string(),
        });
        seats.push(SeatTarget::new(
            member.to_string(),
            provider,
            Some(secret_ref(member)),
            false,
        ));
        providers.insert(
            member.to_string(),
            ProviderEntry::anthropic_api(secret_ref(member).to_string())
                .with_runtime(ProviderRuntimePolicy::default()),
        );
    }
    let seats: Arc<[SeatTarget]> = Arc::from(seats);
    let default_provider = seats[0].provider.clone().expect("seat 0 is built");
    let mut pools = BTreeMap::new();
    pools.insert(
        POOL.to_string(),
        PoolEntry::new(MEMBERS.iter().map(|(m, _)| (*m).to_string()).collect())
            .with_seat_selection(selection),
    );
    let mut router = Router::new(Arc::new(Config {
        providers,
        pools,
        ..Config::default()
    }));
    router.install_resolved_models(
        models
            .iter()
            .map(|(nickname, upstream)| {
                let model =
                    ResolvedModel::new(*nickname, POOL, default_provider.clone(), *upstream)
                        .with_seats(Arc::clone(&seats));
                ((*nickname).to_string(), Arc::new(model))
            })
            .collect(),
    );
    router
}

// ---- reading moves nothing ----

/// Everything a status read could move by mistake, read without moving it.
#[derive(Debug, PartialEq)]
struct RoutingState {
    pin_recency: Vec<String>,
    gates: Vec<(Option<f64>, CircuitPhase, bool)>,
    placement: super::super::placement_record::PlacementTotals,
    readings: crate::quota::view::AccountReadings,
}

fn routing_state(router: &Router, at: &ObservationStamp) -> RoutingState {
    RoutingState {
        pin_recency: router.sticky_pins.recency_order(),
        gates: MEMBERS
            .iter()
            .map(|(member, _)| {
                let gate = router.gate_status_for_tests(&state_key(member));
                (
                    gate.rpm_available,
                    gate.circuit,
                    gate.half_open_probe_in_flight,
                )
            })
            .collect(),
        placement: router.metrics.placement.totals(),
        readings: router.account_readings(at),
    }
}

#[test]
fn reading_the_status_many_times_moves_no_routing_state() {
    // Arrange: full RPM buckets, a half-open-ready seat with its probe slot
    // free, two pins in a known recency order, placement and meter counters
    // already nonzero.
    let router = pool_router(Some(10));
    poll(&router, "seat-b", 20);
    poll(&router, "seat-c", 10);
    router
        .dispatch_chain("m", None)
        .expect("a keyless placement");
    open_lapsed(&router, "seat-a");
    pin_to(&router, "older", "seat-b");
    pin_to(&router, "newer", "seat-c");
    let at = ObservationStamp::now();
    let before = routing_state(&router, &at);
    let tiebreak_before = router.sticky_pins.next_tiebreak();
    assert_eq!(before.gates[0].1, CircuitPhase::HalfOpenReady, "premise");
    assert_eq!(before.gates[1].0, Some(10.0), "premise: a full bucket");
    assert_eq!(before.placement.births_of(Tier::Roomy), 1, "premise");

    // Act
    let reads: Vec<PlacementStatus> = (0..25)
        .map(|_| router.placement_status(&ObservationStamp::now()))
        .collect();

    // Assert
    let tiebreak_after = router.sticky_pins.next_tiebreak();
    assert_eq!(tiebreak_after, tiebreak_before + 1, "the rotation advanced");
    assert_eq!(routing_state(&router, &at), before);
    assert!(reads.windows(2).all(|pair| pair[0] == pair[1]));
}

// ---- rank against the next birth ----

#[test]
fn the_rank_one_seats_are_the_seats_the_next_births_draw_from() {
    type Arrange = fn(&Router);
    let rows: [(&str, Arrange, &[&str]); 4] = [
        (
            "two seats tie on pace above a third",
            |r| {
                poll(r, "seat-a", 60);
                poll(r, "seat-b", 10);
                poll(r, "seat-c", 10);
            },
            &["seat-b", "seat-c"],
        ),
        (
            "nothing read: every seat ties unread",
            |_| {},
            &["seat-a", "seat-b", "seat-c"],
        ),
        (
            "the best pace is half-open, so a closed seat leads",
            |r| {
                poll(r, "seat-a", 0);
                poll(r, "seat-b", 30);
                poll(r, "seat-c", 60);
                open_lapsed(r, "seat-a");
            },
            &["seat-b"],
        ),
        (
            "the best pace is parked, so the next best leads",
            |r| {
                poll(r, "seat-a", 0);
                poll(r, "seat-b", 30);
                poll(r, "seat-c", 30);
                assert!(r.park_credential_for_tests(
                    &state_key("seat-a"),
                    UpstreamReason::CredentialExpired,
                    Instant::now() + LONG,
                ));
            },
            &["seat-b", "seat-c"],
        ),
    ];
    for (name, arrange, expected) in rows {
        // Arrange
        let router = pool_router(None);
        arrange(&router);

        // Act
        let ranked = rank_one(entry(&status(&router), OPUS));
        let drawn = birth_homes(&router, 6);

        // Assert
        let expected: BTreeSet<String> = expected.iter().map(|m| (*m).to_string()).collect();
        assert_eq!(ranked, expected, "row: {name}");
        assert_eq!(drawn, ranked, "row: {name}");
    }
}

#[test]
fn ranks_are_dense_tied_seats_share_one_and_scores_are_rounded() {
    // Arrange: seat-b and seat-c tie; seat-a paces lower.
    let router = pool_router(None);
    poll(&router, "seat-a", 60);
    poll(&router, "seat-b", 10);
    poll(&router, "seat-c", 10);

    // Act
    let status = status(&router);

    // Assert
    let entry = entry(&status, OPUS);
    assert_eq!(entry.pool, POOL);
    assert_eq!(
        ranks(entry),
        [(Some(2), "pace"), (Some(1), "pace"), (Some(1), "pace")]
    );
    let accounts: Vec<Option<String>> = entry.seats.iter().map(|s| s.account.clone()).collect();
    let expected: Vec<Option<String>> = MEMBERS.iter().map(|(m, _)| Some(account(m))).collect();
    assert_eq!(accounts, expected);
    let score = entry.seats[1].score.expect("a read seat has a score");
    assert!(
        (score - 1.8).abs() < 0.011,
        "0.9 room over half the window: {score}"
    );
    assert_eq!(
        score,
        (score * 100.0).round() / 100.0,
        "two decimals: {score}"
    );
    assert!(entry.seats.iter().all(|s| s.tier == "roomy"));
}

#[test]
fn an_unread_seat_ranks_last_with_no_score() {
    // Arrange
    let router = pool_router(None);
    poll(&router, "seat-b", 10);

    // Act
    let status = status(&router);

    // Assert
    let entry = entry(&status, OPUS);
    assert_eq!(
        ranks(entry),
        [
            (Some(2), "no_reading"),
            (Some(1), "pace"),
            (Some(2), "no_reading")
        ]
    );
    assert_eq!(entry.seats[0].tier, "unread");
    assert_eq!(entry.seats[0].score, None);
}

// ---- an unavailable seat ----

#[test]
fn an_unavailable_seat_has_no_rank_and_its_reason_is_the_eligibility_token() {
    type Outage = fn(&Router);
    let rows: [(&str, Outage, &str); 4] = [
        (
            "credential unusable",
            |r| {
                assert!(r.mark_credential_unusable_for_tests(
                    &state_key("seat-a"),
                    UnusableCause::Revoked
                ));
            },
            "credential_unusable",
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
            "account_parked",
        ),
        (
            "breaker open",
            |r| assert!(r.force_open_breaker(&state_key("seat-a"), LONG)),
            "breaker_open",
        ),
        (
            "RPM bucket empty",
            |r| {
                let decision = slot(r, "seat-a").lock().try_dispatch(Instant::now());
                assert_eq!(decision, GateDecision::Allow);
            },
            "rate_limited",
        ),
    ];
    for (name, outage, token) in rows {
        // Arrange: seat-a paces best, so only its outage can cost it rank 1.
        let router = pool_router(Some(1));
        poll(&router, "seat-a", 0);
        poll(&router, "seat-b", 30);
        poll(&router, "seat-c", 60);
        outage(&router);

        // Act
        let status = status(&router);

        // Assert
        let entry = entry(&status, OPUS);
        assert_eq!(
            ranks(entry),
            [(None, token), (Some(1), "pace"), (Some(2), "pace")],
            "row: {name}"
        );
        assert_eq!(entry.seats[0].tier, "roomy", "row: {name}");
        assert!(entry.seats[0].score.is_some(), "row: {name}");
    }
}

#[test]
fn an_unavailable_seat_tied_with_a_set_aside_half_open_seat_still_has_no_rank() {
    // Arrange: seat-a (parked) and seat-c (half-open, set aside for the
    // closed seat-b) stand equal.
    let router = pool_router(None);
    poll(&router, "seat-a", 30);
    poll(&router, "seat-b", 60);
    poll(&router, "seat-c", 30);
    assert!(router.park_credential_for_tests(
        &state_key("seat-a"),
        UpstreamReason::CredentialExpired,
        Instant::now() + LONG,
    ));
    open_lapsed(&router, "seat-c");

    // Act
    let status = status(&router);

    // Assert
    assert_eq!(
        ranks(entry(&status, OPUS)),
        [
            (None, "account_parked"),
            (Some(1), "pace"),
            (Some(2), "pace")
        ]
    );
}

// ---- which pools and models are listed ----

#[test]
fn two_models_on_one_pool_are_two_entries_and_an_opus_weekly_demotes_only_opus() {
    // Arrange: seat-a has the most five-hour room but its opus weekly is
    // nearly spent.
    let router = pool_router_with(
        SeatSelection::StickyLeastLoaded,
        &[("m-opus", OPUS), ("m-sonnet", SONNET)],
    );
    poll_with_opus_weekly(&router, "seat-a", 0, 95);
    poll_with_opus_weekly(&router, "seat-b", 30, 10);

    // Act
    let status = status(&router);

    // Assert
    assert_eq!(status.pools.len(), 2);
    assert_eq!(
        ranks(entry(&status, OPUS)),
        [
            (Some(2), "low_room"),
            (Some(1), "pace"),
            (Some(3), "no_reading")
        ]
    );
    assert_eq!(entry(&status, OPUS).seats[0].tier, "low_room");
    assert_eq!(
        ranks(entry(&status, SONNET)),
        [
            (Some(1), "pace"),
            (Some(2), "pace"),
            (Some(3), "no_reading")
        ]
    );
}

#[test]
fn two_models_on_one_upstream_share_one_entry_available_through_either() {
    for open_for in ["m-one", "m-two"] {
        // Arrange: seat-a is open for one of the two models only.
        let router = pool_router_with(
            SeatSelection::StickyLeastLoaded,
            &[("m-one", OPUS), ("m-two", OPUS)],
        );
        poll(&router, "seat-a", 0);
        assert!(router.force_open_breaker(&format!("{open_for}#seat-a"), LONG));

        // Act
        let status = status(&router);

        // Assert
        assert_eq!(status.pools.len(), 1, "open for {open_for}");
        assert_eq!(
            entry(&status, OPUS).seats[0].rank,
            Some(1),
            "open for {open_for}"
        );
    }
}

#[test]
fn a_pool_with_no_readable_account_is_listed_with_every_seat_unread_and_tied() {
    // Arrange
    let router = super::super::placement_dispatch_tests::dormant_router();

    // Act
    let status = status(&router);

    // Assert
    let entry = entry(&status, OPUS);
    assert_eq!(ranks(entry), [(Some(1), "no_reading"); 3]);
    assert!(
        entry
            .seats
            .iter()
            .all(|s| s.tier == "unread" && s.score.is_none())
    );
}

#[test]
fn a_fill_first_pool_is_not_listed() {
    // Arrange
    let router = pool_router_with(SeatSelection::FillFirst, &[("m", OPUS)]);
    poll(&router, "seat-a", 10);

    // Act
    let status = status(&router);

    // Assert
    assert!(status.pools.is_empty(), "{status:?}");
}

// ---- counters ----

#[test]
fn the_counters_mirror_the_placement_counters_after_births_and_moves() {
    // Arrange
    let router = pool_router(None);
    poll(&router, "seat-a", 95);
    poll(&router, "seat-b", 40);
    poll(&router, "seat-c", 10);
    for session in [Some("s1"), Some("s2"), None] {
        router.dispatch_chain("m", session).expect("a birth");
    }
    pin_to(&router, "from-a", "seat-a");
    assert!(router.force_open_breaker(&state_key("seat-a"), LONG));
    router.dispatch_chain("m", Some("from-a")).expect("a move");
    pin_to(&router, "from-b", "seat-b");
    assert!(
        router.mark_credential_unusable_for_tests(&state_key("seat-b"), UnusableCause::Revoked)
    );
    router.dispatch_chain("m", Some("from-b")).expect("a move");

    // Act
    let status = status(&router);

    // Assert
    let totals = router.metrics.placement.totals();
    assert_eq!(
        (
            status.births.roomy,
            status.births.low_room,
            status.births.unread
        ),
        (3, 0, 0)
    );
    assert_eq!(
        (
            status.moves.credential_unusable,
            status.moves.account_parked,
            status.moves.breaker_open
        ),
        (1, 0, 1)
    );
    assert_eq!(status.cap_forced_moves, 1);
    assert_eq!(status.births.roomy, totals.births_of(Tier::Roomy));
    assert_eq!(
        status.moves.credential_unusable,
        totals.moves_of(EligibilityReason::CredentialUnusable)
    );
    assert_eq!(
        status.moves.breaker_open,
        totals.moves_of(EligibilityReason::BreakerOpen)
    );
    assert_eq!(status.cap_forced_moves, totals.cap_forced_moves);
}

// ---- the closed reason set ----

/// Every reason token a seat can carry: the standing reasons, then the
/// eligibility reasons a seat's state can refuse with.
const REASON_TOKENS: [&str; 11] = [
    "overage",
    "no_cap",
    "no_reading",
    "stale_reading",
    "low_room",
    "pace",
    "unstarted",
    "credential_unusable",
    "account_parked",
    "breaker_open",
    "rate_limited",
];

/// The token each standing reason renders as. No wildcard arm, so a new
/// reason fails to compile here until it is given a row.
const fn standing_token(reason: Reason) -> &'static str {
    match reason {
        Reason::Overage => "overage",
        Reason::NoCap => "no_cap",
        Reason::NoReading => "no_reading",
        Reason::StaleReading => "stale_reading",
        Reason::LowRoom => "low_room",
        Reason::Pace => "pace",
        Reason::Unstarted => "unstarted",
    }
}

#[test]
fn every_reason_token_is_in_the_closed_set() {
    let standing = [
        Reason::Overage,
        Reason::NoCap,
        Reason::NoReading,
        Reason::StaleReading,
        Reason::LowRoom,
        Reason::Pace,
        Reason::Unstarted,
    ];
    for reason in standing {
        assert_eq!(reason.as_str(), standing_token(reason), "{reason:?}");
        assert!(REASON_TOKENS.contains(&reason.as_str()), "{reason:?}");
    }
    let target_state: Vec<EligibilityReason> = EligibilityReason::ALL
        .into_iter()
        .filter(|reason| !reason.is_request_only())
        .collect();
    assert_eq!(target_state.len(), 4);
    for reason in target_state {
        assert!(REASON_TOKENS.contains(&reason.as_str()), "{reason:?}");
    }
    let distinct: BTreeSet<&str> = REASON_TOKENS.into_iter().collect();
    assert_eq!(distinct.len(), REASON_TOKENS.len());
}
