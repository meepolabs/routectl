//! Router-level tests for moving a pinned conversation off a seat with a
//! lasting outage.
//!
//! The predicate is tested in `quota::placement`; what needs a ROUTER is what
//! it cannot see: that the real park, unusable mark and breaker state reach
//! it, that the move ranks siblings with the conversation's own readings, that
//! the pin write is atomic on the real `StickyPins` under concurrent turns,
//! and that pins survive a hot reload but not a restart.

use super::*;

use std::sync::Barrier;
use std::time::Instant;

use async_trait::async_trait;
use routectl_core::failure_class::LastOutcome;
use routectl_core::{EligibilityReason, UpstreamReason};

use super::sticky::PinWrite;
use crate::config::{PoolEntry, ProviderEntry, ProviderRuntimePolicy, SeatSelection};
use crate::quota::usage_body::UsageVendor;
use crate::runtime_state::UnusableCause;
use crate::seat_pool::{SeatPin, SeatTarget};

pub(super) const POOL: &str = "move-pool";
pub(super) const OPUS: &str = "claude-opus-4-7";
pub(super) const SESSION: &str = "S";
/// The pool's members with the seat label each one's login carries; the
/// first is the family's default seat.
pub(super) const MEMBERS: [(&str, Option<&str>); 3] = [
    ("seat-a", None),
    ("seat-b", Some("seat-b")),
    ("seat-c", Some("seat-c")),
];
pub(super) const LONG: Duration = Duration::from_mins(10);

pub(super) struct SeatStub {
    pub(super) id: String,
}

#[async_trait]
impl Provider for SeatStub {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(&self.id, "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!("these tests read the resolved order and never dispatch")
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("these tests read the resolved order and never dispatch")
    }
}

pub(super) fn secret_ref(member: &str) -> routectl_auth::SecretRef {
    let label = MEMBERS
        .iter()
        .find(|(name, _)| *name == member)
        .expect("a declared member")
        .1;
    routectl_auth::SecretRef::OAuth {
        provider: "anthropic".to_string(),
        label: label.map(str::to_string),
    }
}

/// A sticky three-seat Anthropic login pool serving model `m` on upstream
/// [`OPUS`], each seat limited to `rpm_limit` requests a minute.
pub(super) fn pool_router(rpm_limit: Option<u32>) -> Router {
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
            ProviderEntry::anthropic_api(secret_ref(member).to_string()).with_runtime(
                ProviderRuntimePolicy {
                    rpm_limit,
                    ..Default::default()
                },
            ),
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

pub(super) fn state_key(member: &str) -> String {
    format!("m#{member}")
}

fn slot(router: &Router, member: &str) -> Arc<Mutex<ProviderState>> {
    router.state[&state_key(member)].clone()
}

/// Poll `member` at `percent` of a five-hour window with 2.5 hours to run.
pub(super) fn poll(router: &Router, member: &str, percent: u32) {
    let reset = time::OffsetDateTime::now_utc() + Duration::from_mins(150);
    let body = format!(
        r#"{{"five_hour": {{"utilization": {percent}, "resets_at": "{}"}}, "seven_day": null}}"#,
        reset
            .format(&time::format_description::well_known::Rfc3339)
            .expect("an RFC 3339 rendering")
    );
    let account =
        crate::seat_pool::seat_identity(Some(&secret_ref(member))).expect("a login account");
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

pub(super) fn pin_key(session: &str) -> String {
    super::chain::sticky_pin_key(session, POOL)
}

pub(super) fn pin_to(router: &Router, session: &str, member: &str) {
    router.sticky_pins.put(
        &pin_key(session),
        SeatPin {
            member: member.to_string(),
        },
    );
}

fn pinned(router: &Router, session: &str) -> Option<String> {
    router
        .sticky_pins
        .get(&pin_key(session))
        .map(|pin| pin.member)
}

/// One sticky selection for `session`: the walk as members, and the write.
fn place(router: &Router, session: &str) -> (Vec<String>, PinWrite) {
    let seats = router.resolved_models["m"]
        .seats
        .clone()
        .expect("a pooled model");
    let (order, write, _) = router.sticky_seat_placement(&seats, &pin_key(session), "m", OPUS);
    let members = order
        .into_iter()
        .map(|i| seats[i].provider_name.clone())
        .collect();
    (members, write)
}

/// The resolved dispatch chain for `session`, as members.
fn walk(router: &Router, session: &str) -> Vec<String> {
    router
        .dispatch_chain("m", Some(session))
        .expect("chain resolves")
        .into_iter()
        .map(|target| {
            target
                .state_key
                .strip_prefix("m#")
                .expect("a pooled state key")
                .to_string()
        })
        .collect()
}

pub(super) fn index_of(member: &str) -> usize {
    MEMBERS
        .iter()
        .position(|(name, _)| *name == member)
        .expect("a declared member")
}

pub(super) fn open_baseline(router: &Router, member: &str) {
    let slot = slot(router, member);
    let mut slot = slot.lock();
    let now = Instant::now();
    for _ in 0..5 {
        slot.record_failure(now, LastOutcome::Http5xx);
    }
}

fn open_lapsed(router: &Router, member: &str) {
    let opened = Instant::now()
        .checked_sub(Duration::from_mins(2))
        .expect("a clock two minutes past its origin");
    slot(router, member)
        .lock()
        .force_open(opened, Duration::from_mins(1));
}

// ---- which outages move the pin ----

#[test]
fn a_lasting_outage_moves_the_pin_to_the_best_standing_sibling() {
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
            "parked for ten minutes",
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
            "breaker open for ten minutes",
            |r| assert!(r.force_open_breaker(&state_key("seat-a"), LONG)),
            EligibilityReason::BreakerOpen,
        ),
    ];
    for (name, outage, trigger) in rows {
        // Arrange: seat-c paces best of the siblings; rotation alone would
        // pick seat-b first.
        let router = pool_router(None);
        poll(&router, "seat-b", 60);
        poll(&router, "seat-c", 10);
        pin_to(&router, SESSION, "seat-a");
        outage(&router);

        // Act
        let (order, write) = place(&router, SESSION);

        // Assert
        assert_eq!(
            write,
            PinWrite::Moved {
                home: index_of("seat-c"),
                trigger
            },
            "row: {name}"
        );
        assert_eq!(order, ["seat-c", "seat-a", "seat-b"], "row: {name}");
        assert_eq!(
            pinned(&router, SESSION).as_deref(),
            Some("seat-c"),
            "row: {name}"
        );
    }
}

#[test]
fn a_short_or_untimed_refusal_leaves_the_pin_and_the_home_first_walk() {
    type Outage = fn(&Router);
    let rows: [(&str, Outage); 4] = [
        ("the 60 s baseline open", |r| open_baseline(r, "seat-a")),
        ("a half-open-ready breaker", |r| open_lapsed(r, "seat-a")),
        ("a probe in flight", |r| {
            open_lapsed(r, "seat-a");
            let claimed = slot(r, "seat-a")
                .lock()
                .try_dispatch_admitting(Instant::now())
                .claimed_half_open;
            assert!(claimed, "premise: the probe slot is claimed");
        }),
        ("an empty RPM bucket", |r| {
            let decision = slot(r, "seat-a").lock().try_dispatch(Instant::now());
            assert_eq!(decision, crate::runtime_state::GateDecision::Allow);
        }),
    ];
    for (name, outage) in rows {
        // Arrange: the siblings are healthy and seat-c reads best, so any
        // move would show.
        let router = pool_router(Some(1));
        poll(&router, "seat-c", 0);
        pin_to(&router, SESSION, "seat-a");
        outage(&router);

        // Act
        let (order, write) = place(&router, SESSION);

        // Assert
        assert_eq!(write, PinWrite::None, "row: {name}");
        assert_eq!(order, ["seat-a", "seat-b", "seat-c"], "row: {name}");
        assert_eq!(
            pinned(&router, SESSION).as_deref(),
            Some("seat-a"),
            "row: {name}"
        );
    }
}

#[test]
fn no_eligible_sibling_keeps_the_pin_on_an_unusable_home() {
    // Arrange
    let router = pool_router(None);
    pin_to(&router, SESSION, "seat-a");
    assert!(
        router.mark_credential_unusable_for_tests(&state_key("seat-a"), UnusableCause::Revoked)
    );
    assert!(router.force_open_breaker(&state_key("seat-b"), LONG));
    assert!(router.force_open_breaker(&state_key("seat-c"), LONG));

    // Act
    let (order, write) = place(&router, SESSION);

    // Assert
    assert_eq!(write, PinWrite::None);
    assert_eq!(order, ["seat-a", "seat-b", "seat-c"]);
    assert_eq!(pinned(&router, SESSION).as_deref(), Some("seat-a"));
}

#[test]
fn the_rotation_advances_only_when_a_move_is_attempted() {
    type Outage = fn(&Router);
    let rows: [(&str, Outage, usize); 3] = [
        ("a serving home", |_| {}, 0),
        (
            "a home out for too short a time",
            |r| open_baseline(r, "seat-a"),
            0,
        ),
        (
            "a home out for long",
            |r| assert!(r.force_open_breaker(&state_key("seat-a"), LONG)),
            1,
        ),
    ];
    for (name, outage, consumed) in rows {
        let router = pool_router(None);
        pin_to(&router, SESSION, "seat-a");
        outage(&router);
        let before = router.sticky_pins.next_tiebreak();

        place(&router, SESSION);

        let after = router.sticky_pins.next_tiebreak();
        assert_eq!(after - before - 1, consumed, "row: {name}");
    }
}

// ---- repeated re-placement ----

#[test]
fn a_moved_conversation_stays_after_its_old_seat_recovers_and_moves_again_by_pace() {
    // Arrange: seat-b paces best, then seat-c, then seat-a.
    let router = pool_router(Some(10));
    poll(&router, "seat-a", 60);
    poll(&router, "seat-b", 0);
    poll(&router, "seat-c", 20);
    pin_to(&router, SESSION, "seat-a");

    // Act 1: seat-a opens for ten minutes.
    assert!(router.force_open_breaker(&state_key("seat-a"), LONG));
    let moved = walk(&router, SESSION);

    // Act 2: seat-a recovers.
    let generation = router.credential_generation_for_tests(&state_key("seat-a"));
    router.settle_seat_success_for_tests(&state_key("seat-a"), true, generation);
    let recovered = walk(&router, SESSION);

    // Act 3: seat-c has less RPM headroom than seat-a, then seat-b is parked
    // for ten minutes.
    for _ in 0..5 {
        slot(&router, "seat-c").lock().try_dispatch(Instant::now());
    }
    let seats = router.resolved_models["m"].seats.clone().expect("a pool");
    let snapshots = router.gather_capacity_snapshots(&seats, "m", Instant::now());
    assert!(
        snapshots[index_of("seat-a")].rpm_available > snapshots[index_of("seat-c")].rpm_available,
        "premise: headroom alone would pick seat-a: {snapshots:?}"
    );
    assert!(router.park_credential_for_tests(
        &state_key("seat-b"),
        UpstreamReason::CredentialExpired,
        Instant::now() + LONG,
    ));
    let replaced = walk(&router, SESSION);

    // Assert
    assert_eq!(moved[0], "seat-b");
    assert_eq!(
        recovered[0], "seat-b",
        "a recovered seat never pulls it back"
    );
    assert_eq!(
        replaced[0], "seat-c",
        "the best standing, not the most headroom"
    );
    assert_eq!(pinned(&router, SESSION).as_deref(), Some("seat-c"));
}

// ---- reload and restart ----

/// A router whose conversation moved off seat-a (open for ten minutes) onto
/// seat-b.
fn moved_router() -> Router {
    let router = pool_router(None);
    poll(&router, "seat-b", 0);
    pin_to(&router, SESSION, "seat-a");
    assert!(router.force_open_breaker(&state_key("seat-a"), LONG));
    assert_eq!(walk(&router, SESSION)[0], "seat-b", "premise: moved");
    router
}

#[test]
fn a_hot_reload_keeps_the_moved_pin() {
    // Arrange
    let before = moved_router();
    let mut after = pool_router(None);
    after.carry_over_runtime_state_from(&before);
    after.carry_over_sticky_from(&before);
    after.carry_over_quota_from(&before);

    // Act
    let (order, write) = place(&after, SESSION);

    // Assert
    assert_eq!(write, PinWrite::None);
    assert_eq!(order[0], "seat-b");
}

#[test]
fn a_restart_forgets_the_moved_pin_and_births_unread() {
    // Arrange: a fresh router and quota store, nothing carried.
    let before = moved_router();
    let restarted = pool_router(None);
    let seats = restarted.resolved_models["m"]
        .seats
        .clone()
        .expect("a pool");

    // Act
    let held = pinned(&restarted, SESSION);
    let standings = restarted.birth_standings(&seats, OPUS);
    let (order, write) = place(&restarted, SESSION);

    // Assert
    assert_eq!(pinned(&before, SESSION).as_deref(), Some("seat-b"));
    assert_eq!(held, None);
    assert!(
        standings
            .iter()
            .all(|s| s.tier == crate::quota::placement::Tier::Unread),
        "{standings:?}"
    );
    assert_eq!(write, PinWrite::Born { home: 0 });
    assert_eq!(order[0], "seat-a");
}

// ---- concurrent turns on one conversation ----

pub(super) const TURNS: usize = 16;
const ROUNDS: usize = 40;

/// Run `TURNS` concurrent selections for `session`, released together.
pub(super) fn concurrent_turns(router: &Router, session: &str) -> Vec<(Vec<String>, PinWrite)> {
    let barrier = Barrier::new(TURNS);
    std::thread::scope(|scope| {
        let turns: Vec<_> = (0..TURNS)
            .map(|_| {
                scope.spawn(|| {
                    barrier.wait();
                    place(router, session)
                })
            })
            .collect();
        turns
            .into_iter()
            .map(|turn| turn.join().expect("a turn completes"))
            .collect()
    })
}

/// Every turn walks from the pin the conversation ends with.
fn assert_all_walk_the_pin(router: &Router, session: &str, turns: &[(Vec<String>, PinWrite)]) {
    let pin = pinned(router, session).expect("a pin is held");
    for (order, write) in turns {
        assert_eq!(order[0], pin, "{write:?}");
    }
}

#[test]
fn concurrent_turns_off_a_lasting_outage_make_exactly_one_repin() {
    let router = pool_router(None);
    assert!(router.force_open_breaker(&state_key("seat-a"), LONG));
    for round in 0..ROUNDS {
        // Arrange: siblings tie, so the turns' own picks rotate across both.
        let session = format!("move-{round}");
        pin_to(&router, &session, "seat-a");

        // Act
        let turns = concurrent_turns(&router, &session);

        // Assert
        let moves = turns
            .iter()
            .filter(|(_, write)| matches!(write, PinWrite::Moved { .. }))
            .count();
        assert_eq!(moves, 1, "round {round}: {turns:?}");
        assert_all_walk_the_pin(&router, &session, &turns);
    }
}

#[test]
fn concurrent_births_make_exactly_one_claim_and_all_walk_the_winner() {
    let router = pool_router(None);
    for round in 0..ROUNDS {
        // Arrange: every seat ties, so the turns' own picks rotate.
        let session = format!("birth-{round}");

        // Act
        let turns = concurrent_turns(&router, &session);

        // Assert
        let births = turns
            .iter()
            .filter(|(_, write)| matches!(write, PinWrite::Born { .. }))
            .count();
        assert_eq!(births, 1, "round {round}: {turns:?}");
        assert_all_walk_the_pin(&router, &session, &turns);
    }
}
