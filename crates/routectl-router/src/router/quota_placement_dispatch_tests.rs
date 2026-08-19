//! Router-level tests for quota-aware birth placement and its kill switch.
//!
//! The pure partition is tested in `quota::placement`; what needs a ROUTER is
//! everything the partition cannot see: that the store read is keyed the way
//! the feed writes, that a healthy pin never reaches the store at all, and
//! above all that the switch OFF is byte-identical to the birth chooser with no
//! quota state in existence. That last one is why the switch exists, and it is
//! not provable against a pure function -- OFF must be proven at the boundary
//! the operator actually turns off.
//!
//! Every seat here has `rpm_available: None`, which is what a real pool reports
//! (no RPM limit configured). Under the pre-quota chooser that makes every seat
//! tie on headroom and the anti-herd rotation decides, so a quota-driven pick
//! and a quota-dormant one land on DIFFERENT seats -- the difference this suite
//! reads.

use super::*;

use async_trait::async_trait;

use crate::config::{PoolEntry, ProviderEntry, SeatSelection};
use crate::quota::freshness::{ObservationStamp, accept_reset};
use crate::quota::key::{SeatKey, seat_key_for_secret_ref};
use crate::quota::reduce::QuotaSnapshot;
use crate::quota::window::{Billing, QuotaWindow, Utilization};
use crate::seat_pool::SeatTarget;
use std::time::Duration;

const SEAT_POOL: &str = "anthropic-pool";
const FAST_WINDOW: Duration = Duration::from_hours(5);
const SEAT_MEMBERS: [&str; 3] = ["anthropic-a", "anthropic-b", "anthropic-c"];

/// A seat-pinned provider that never dispatches. Every assertion here reads
/// the resolved CHAIN, so no seat is ever called.
struct SeatStub {
    id: String,
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
        unreachable!("these tests read the resolved chain and never dispatch")
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("these tests read the resolved chain and never dispatch")
    }
}

fn secret_ref(member: &str) -> routectl_auth::SecretRef {
    routectl_auth::SecretRef::OAuth {
        provider: member.to_string(),
        label: None,
    }
}

/// The store key for one seat, derived through the EXPOSED read-side helper.
/// A hand-built key would pass whichever key the store happened to use, which
/// is exactly the silently-green failure this feature has to avoid.
fn seat_key(member: &str) -> SeatKey {
    seat_key_for_secret_ref(Some(&secret_ref(member))).expect("an oauth ref yields a key")
}

/// A three-seat `StickyLeastLoaded` pool on one Anthropic provider, with
/// `seat_quota.enabled` as given.
fn pooled_router(quota_enabled: bool) -> Router {
    pooled_router_with_selection(SeatSelection::StickyLeastLoaded, quota_enabled)
}

/// A three-seat pool on one Anthropic provider under `selection`, with
/// `seat_quota.enabled` as given.
fn pooled_router_with_selection(selection: SeatSelection, quota_enabled: bool) -> Router {
    let mut seats: Vec<SeatTarget> = Vec::new();
    let mut providers = BTreeMap::new();
    for member in SEAT_MEMBERS {
        let provider: Arc<dyn Provider> = Arc::new(SeatStub {
            id: member.to_string(),
        });
        seats.push(SeatTarget {
            provider_name: member.to_string(),
            provider,
            auth_secret_ref: Some(secret_ref(member)),
        });
        providers.insert(
            member.to_string(),
            ProviderEntry::anthropic_api(format!("oauth://{member}")),
        );
    }
    let default_provider = seats[0].provider.clone();

    let mut pools = BTreeMap::new();
    pools.insert(
        SEAT_POOL.to_string(),
        PoolEntry::new(SEAT_MEMBERS.iter().map(|m| (*m).to_string()).collect())
            .with_seat_selection(selection),
    );
    let mut cfg = Config {
        providers,
        pools,
        ..Config::default()
    };
    cfg.seat_quota.enabled = quota_enabled;

    let mut router = Router::new(Arc::new(cfg));
    let model = ResolvedModel::new("opus", SEAT_POOL, default_provider, "claude-opus-4-7")
        .with_seats(Arc::from(seats));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert("opus".to_string(), Arc::new(model));
    router.install_resolved_models(models);
    router
}

/// Store one FAST reading per `(member, utilization)` pair, observed now.
fn seed_readings(router: &Router, readings: &[(&str, f64)]) {
    let observed = ObservationStamp::now();
    for (member, fraction) in readings {
        let reset_at = accept_reset(
            std::time::SystemTime::now() + FAST_WINDOW / 2,
            &observed,
            FAST_WINDOW,
            crate::quota::curation::RESET_TOLERANCE,
        )
        .expect("a reset inside the window is accepted");
        let stored = router.quota_store.observe(
            &seat_key(member),
            QuotaSnapshot {
                observed,
                fast: QuotaWindow::Known {
                    utilization: Utilization::new(*fraction).expect("a valid fraction"),
                    reset_at,
                },
                slow: QuotaWindow::Unknown,
                billing: Billing::Unknown,
            },
        );
        assert!(
            stored,
            "an installed pool's own seat must be admitted by the store"
        );
    }
}

/// The birth chooser's output for `session`: the resolved seat walk order as
/// `state_key`s. THE EQUIVALENCE BOUNDARY -- the chooser invocation, read
/// before any later request observes the pin it wrote.
fn chain_order(router: &Router, session: &str) -> Vec<String> {
    router
        .dispatch_chain("opus", Some(session))
        .expect("chain resolves")
        .into_iter()
        .map(|t| t.state_key)
        .collect()
}

fn home_of(router: &Router, session: &str) -> String {
    chain_order(router, session)
        .into_iter()
        .next()
        .expect("a non-empty chain")
}

// ---- placement ON ----

#[test]
fn a_birth_pick_lands_on_the_emptiest_below_cap_seat() {
    // Arrange: every seat tied on RPM (none configured), so only quota can
    // move the pick off the rotation's choice.
    let router = pooled_router(true);
    seed_readings(&router, &[("anthropic-a", 0.40), ("anthropic-b", 0.05)]);

    // Act
    let home = home_of(&router, "S");

    // Assert: the emptiest KNOWN seat, not the rotation's seat 0.
    assert_eq!(home, "opus#anthropic-b");
}

#[test]
fn a_known_empty_seat_beats_the_seats_with_no_reading() {
    // The signal the feature exists to use: seat-c has never reported, and a
    // seat known to be nearly empty must win over it rather than the reverse.
    let router = pooled_router(true);
    seed_readings(&router, &[("anthropic-c", 0.02)]);

    assert_eq!(home_of(&router, "S"), "opus#anthropic-c");
}

#[test]
fn a_healthy_pin_is_never_moved_by_a_capped_reading() {
    // The birth pins seat-b (its only reading, below cap). Then seat-b's window
    // is reported FULL while a sibling reports empty. The session must stay:
    // a soft cap never costs a warm prompt cache.
    let router = pooled_router(true);
    seed_readings(&router, &[("anthropic-b", 0.05)]);
    assert_eq!(home_of(&router, "S"), "opus#anthropic-b");

    seed_readings(&router, &[("anthropic-b", 1.0), ("anthropic-c", 0.0)]);

    assert_eq!(
        home_of(&router, "S"),
        "opus#anthropic-b",
        "a pinned over-cap session runs to actual exhaustion rather than migrating"
    );
}

#[test]
fn a_keyless_request_places_by_cap_and_creates_no_pin() {
    // A keyless request has no warm prompt cache to protect, and cache
    // preservation is the only thing that outranks quota fairness -- so it
    // places by remaining budget. It still mints no pin: there is no key to
    // pin under. The emptiest seat therefore LEADS the walk.
    let router = pooled_router(true);
    seed_readings(&router, &[("anthropic-c", 0.0)]);

    let order: Vec<String> = router
        .dispatch_chain("opus", None)
        .expect("chain resolves")
        .into_iter()
        .map(|t| t.state_key)
        .collect();

    assert_eq!(
        order.first().map(String::as_str),
        Some("opus#anthropic-c"),
        "the only seat with a fresh below-cap reading must lead a keyless walk"
    );
    assert_eq!(
        order.len(),
        3,
        "every eligible seat still follows, so the fall-through walk is preserved"
    );
    assert!(
        router.sticky_pins.is_empty(),
        "a keyless request must create no pin"
    );
}

#[test]
fn a_keyless_request_falls_back_to_the_unchanged_walk_without_quota_evidence() {
    // The other half of the same boundary: with nothing observed, a keyless
    // request keeps exactly the fill-first collapse it has always had.
    let router = pooled_router(true);

    let order: Vec<String> = router
        .dispatch_chain("opus", None)
        .expect("chain resolves")
        .into_iter()
        .map(|t| t.state_key)
        .collect();

    assert_eq!(
        order,
        vec!["opus#anthropic-a", "opus#anthropic-b", "opus#anthropic-c"]
    );
}

#[test]
fn a_keyless_request_with_the_switch_off_keeps_the_unchanged_walk() {
    // OFF must not consult quota for a keyless request either, so a reading
    // that would otherwise lead the walk changes nothing.
    let router = pooled_router(false);
    seed_readings(&router, &[("anthropic-c", 0.0)]);

    let order: Vec<String> = router
        .dispatch_chain("opus", None)
        .expect("chain resolves")
        .into_iter()
        .map(|t| t.state_key)
        .collect();

    assert_eq!(
        order,
        vec!["opus#anthropic-a", "opus#anthropic-b", "opus#anthropic-c"]
    );
}

#[test]
fn an_all_capped_pool_still_places_and_takes_the_most_remaining() {
    // Every seat over its cap. The request is NOT failed; it lands on the seat
    // with the most left.
    let router = pooled_router(true);
    seed_readings(
        &router,
        &[
            ("anthropic-a", 0.99),
            ("anthropic-b", 0.70),
            ("anthropic-c", 0.85),
        ],
    );

    assert_eq!(home_of(&router, "S"), "opus#anthropic-b");
}

// ---- placement OFF: byte-identity at the birth-chooser boundary ----

/// THE KILL SWITCH'S CONTRACT. For an unpinned session, OFF must be
/// byte-identical to the birth chooser with no quota state in existence -- so
/// the comparison is against a router whose store is EMPTY, which is precisely
/// what the chooser saw before this feature was built.
///
/// The equivalence boundary is the CHOOSER INVOCATION, not the whole request
/// stream: universal affinity and birth-only rotation are settled changes, so a
/// rotation cursor advances once per new unpinned session either way. Each
/// probe below therefore uses a FRESH session against a FRESH router.
#[test]
fn off_is_byte_identical_to_a_chooser_with_no_quota_state() {
    for readings in [
        // A pool whose quota evidence would have moved the pick: one seat far
        // emptier than the rotation's choice.
        vec![
            ("anthropic-a", 0.90),
            ("anthropic-b", 0.01),
            ("anthropic-c", 0.50),
        ],
        // A pool where every seat is over cap.
        vec![
            ("anthropic-a", 0.99),
            ("anthropic-b", 0.70),
            ("anthropic-c", 0.85),
        ],
        // A partially observed pool -- the mixed arm.
        vec![("anthropic-b", 0.95)],
        // A single below-cap seat, the strongest possible pull off seat 0.
        vec![("anthropic-c", 0.0)],
    ] {
        let off = pooled_router(false);
        seed_readings(&off, &readings);

        // The baseline: quota placement compiled or not, this router has NO
        // reading for any seat, so no arm of the partition can act.
        let baseline = pooled_router(true);

        assert_eq!(
            chain_order(&off, "S"),
            chain_order(&baseline, "S"),
            "with the switch off the birth chooser must agree with a chooser \
             that has no quota state at all; readings were {readings:?}"
        );
    }
}

#[test]
fn off_agrees_with_the_baseline_on_the_pin_it_writes() {
    // Byte-identity covers the pin too, not only the returned order: a
    // different home would be a different pin and every later turn of the
    // conversation would diverge.
    let off = pooled_router(false);
    seed_readings(&off, &[("anthropic-c", 0.0)]);
    let baseline = pooled_router(true);

    let _ = chain_order(&off, "S");
    let _ = chain_order(&baseline, "S");

    let pin_key = super::chain::sticky_pin_key("S", "opus");
    assert_eq!(
        off.sticky_pins.get(&pin_key),
        baseline.sticky_pins.get(&pin_key)
    );
}

#[test]
fn off_keeps_collecting_and_aging_observations() {
    // Following the learned correction's switch: OFF stops the reading from
    // being APPLIED and nothing else. The store keeps accepting and keeps
    // expiring, so re-enabling is instant rather than a re-observe.
    let router = pooled_router(false);
    seed_readings(&router, &[("anthropic-b", 0.05)]);
    let _ = chain_order(&router, "S");

    // Collected while off.
    let reading = router
        .quota_store
        .reading_for(&seat_key("anthropic-b"), &ObservationStamp::now())
        .expect("the store accepted the observation while the switch was off");
    assert!(matches!(reading.fast, QuotaWindow::Known { .. }));

    // Still aging while off: read past the window's own reset and it lapses to
    // no-evidence rather than standing at its last value.
    let past_reset = ObservationStamp::from_parts(
        std::time::SystemTime::now() + FAST_WINDOW,
        std::time::Instant::now() + FAST_WINDOW,
    );
    let aged = router
        .quota_store
        .reading_for(&seat_key("anthropic-b"), &past_reset)
        .expect("the entry survives; its window does not");
    assert_eq!(aged.fast, QuotaWindow::Unknown);
}

#[test]
fn off_preserves_the_one_time_migration_off_an_unhealthy_seat() {
    // The switch gates WHICH seat a birth picks. It does not gate affinity, and
    // it does not gate the migration that rescues a session whose home stopped
    // serving -- turning quota placement off must never leave a conversation
    // stranded on a dead seat.
    let router = pooled_router(false);
    let home = home_of(&router, "S");
    assert_eq!(
        home, "opus#anthropic-a",
        "the rotation's birth pick with no quota"
    );

    // Park the pinned home so it is no longer dispatchable.
    router
        .state
        .get("opus#anthropic-a")
        .expect("the home seat has a state slot")
        .lock()
        .force_open(std::time::Instant::now(), Duration::from_mins(5));

    let migrated = home_of(&router, "S");

    assert_ne!(
        migrated, "opus#anthropic-a",
        "an unhealthy home must still migrate once with the switch off"
    );
}

#[test]
fn off_emits_no_quota_placement_diagnostic() {
    // The diagnostics must be silent with the switch off, or an operator who
    // turned the feature off would still see it deciding. The dormant arm is
    // deliberately uncounted for the same reason.
    let router = pooled_router(false);
    seed_readings(&router, &[("anthropic-b", 0.05)]);

    let _ = chain_order(&router, "S");

    assert_eq!(
        router.metrics.quota_placement_totals(),
        QuotaPlacementTotals {
            below_cap: 0,
            all_capped: 0,
            mixed_unknown: 0,
            all_unknown: 0,
        },
    );
}

// ---- counters ----

#[test]
fn each_partition_arm_is_counted_on_a_real_birth_pick() {
    // Arrange: a below-cap pick.
    let below = pooled_router(true);
    seed_readings(&below, &[("anthropic-b", 0.05)]);
    let _ = chain_order(&below, "S");
    assert_eq!(below.metrics.quota_placement_totals().below_cap, 1);

    // An all-capped pick.
    let capped = pooled_router(true);
    seed_readings(
        &capped,
        &[
            ("anthropic-a", 0.99),
            ("anthropic-b", 0.70),
            ("anthropic-c", 0.85),
        ],
    );
    let _ = chain_order(&capped, "S");
    assert_eq!(capped.metrics.quota_placement_totals().all_capped, 1);

    // A mixed capped-known / unknown fall-through.
    let mixed = pooled_router(true);
    seed_readings(&mixed, &[("anthropic-a", 0.99)]);
    let _ = chain_order(&mixed, "S");
    assert_eq!(mixed.metrics.quota_placement_totals().mixed_unknown, 1);

    // An all-unknown fall-through -- a fresh process's normal state.
    let unknown = pooled_router(true);
    let _ = chain_order(&unknown, "S");
    assert_eq!(unknown.metrics.quota_placement_totals().all_unknown, 1);
}

#[test]
fn the_selection_decision_vocabulary_is_unchanged_by_quota() {
    // Quota changes WHICH seat wins inside the existing birth path and never
    // the persisted, operator-documented decision vocabulary.
    let router = pooled_router(true);
    seed_readings(&router, &[("anthropic-c", 0.0)]);

    let decisions: Vec<Option<&'static str>> = router
        .dispatch_chain("opus", Some("S"))
        .expect("chain resolves")
        .into_iter()
        .map(|t| t.selection_decision)
        .collect();
    assert_eq!(decisions, vec![Some("birth_pick"), None, None]);

    let stay: Vec<Option<&'static str>> = router
        .dispatch_chain("opus", Some("S"))
        .expect("chain resolves")
        .into_iter()
        .map(|t| t.selection_decision)
        .collect();
    assert_eq!(stay, vec![Some("sticky_stay"), None, None]);
}

#[test]
fn a_keyless_walk_leads_with_the_emptiest_of_several_below_cap_seats() {
    // The single-seeded-seat test cannot tell "picks the below-cap seat" from
    // "picks the MOST REMAINING below-cap seat". Three seats, all below cap,
    // and the emptiest is deliberately NOT the one the fixed order would
    // reach first.
    let router = pooled_router(true);
    seed_readings(
        &router,
        &[
            ("anthropic-a", 0.40),
            ("anthropic-b", 0.05),
            ("anthropic-c", 0.30),
        ],
    );

    let order: Vec<String> = router
        .dispatch_chain("opus", None)
        .expect("chain resolves")
        .into_iter()
        .map(|t| t.state_key)
        .collect();

    assert_eq!(
        order.first().map(String::as_str),
        Some("opus#anthropic-b"),
        "among several below-cap seats the walk must lead with the most remaining, \
         not with whichever the fixed order happens to reach first"
    );
    assert_eq!(order.len(), 3, "every seat still follows the lead");
}

// ---- the strategy matrix: which selections the quota layer reaches ----

/// Whether one `(selection, keyed?)` case mints an affinity pin and whether the
/// quota partition ran for it.
#[derive(Debug, PartialEq, Eq)]
struct Engagement {
    mints_pin: bool,
    consults_quota: bool,
}

/// Resolve one chain on a fresh pool that has ONE strongly below-cap reading,
/// and report what the selection engaged.
///
/// `consults_quota` is read from the placement counters rather than from the
/// store: the counters are incremented only by `record_quota_placement`, which
/// only the sticky paths call, and the seeded reading forces a decisive
/// `below_cap` arm wherever the partition does run. A selection that never
/// reaches the store therefore leaves every total at zero, and one that does
/// cannot.
fn engagement_of(selection: SeatSelection, keyed: bool) -> Engagement {
    let router = pooled_router_with_selection(selection, true);
    seed_readings(&router, &[("anthropic-c", 0.0)]);

    let _ = router
        .dispatch_chain("opus", if keyed { Some("S") } else { None })
        .expect("chain resolves");

    Engagement {
        mints_pin: !router.sticky_pins.is_empty(),
        consults_quota: router.metrics.quota_placement_totals()
            != QuotaPlacementTotals {
                below_cap: 0,
                all_capped: 0,
                mixed_unknown: 0,
                all_unknown: 0,
            },
    }
}

/// THE SETTLED SCOPE, stated per variant so widening it cannot pass silently.
///
/// Quota-aware placement and the affinity layer reach `StickyLeastLoaded` and
/// nothing else. `FillFirst` and `RoundRobin` keep their own contracts:
/// `FillFirst` drains one seat before advancing, which is the price of the cache
/// locality an operator asked for by writing it, and `RoundRobin` spreads per
/// request. Extending either to mint a pin or to read a budget is a
/// product-visible contract change, and must fail here rather than land quietly.
///
/// Keyless `StickyLeastLoaded` mints no pin because there is no key to pin
/// under; it still places by budget, because the only thing that outranks quota
/// fairness is a warm prompt cache and a keyless request has none.
#[test]
fn only_sticky_least_loaded_mints_pins_and_reads_quota() {
    let cases = [
        (
            SeatSelection::FillFirst,
            true,
            Engagement {
                mints_pin: false,
                consults_quota: false,
            },
        ),
        (
            SeatSelection::FillFirst,
            false,
            Engagement {
                mints_pin: false,
                consults_quota: false,
            },
        ),
        (
            SeatSelection::RoundRobin,
            true,
            Engagement {
                mints_pin: false,
                consults_quota: false,
            },
        ),
        (
            SeatSelection::RoundRobin,
            false,
            Engagement {
                mints_pin: false,
                consults_quota: false,
            },
        ),
        (
            SeatSelection::StickyLeastLoaded,
            true,
            Engagement {
                mints_pin: true,
                consults_quota: true,
            },
        ),
        (
            SeatSelection::StickyLeastLoaded,
            false,
            Engagement {
                mints_pin: false,
                consults_quota: true,
            },
        ),
    ];

    for (selection, keyed, expected) in cases {
        assert_eq!(
            engagement_of(selection, keyed),
            expected,
            "{selection:?} with{} a session key",
            if keyed { "" } else { "out" }
        );
    }
}

#[test]
fn fill_first_stays_at_seat_zero_with_a_reading_that_would_move_a_sticky_pool() {
    // The same reading that leads a sticky walk with seat-c must leave a
    // fill-first pool draining seat 0, request after request. Its contract is
    // the drain order itself, not a placement.
    let router = pooled_router_with_selection(SeatSelection::FillFirst, true);
    seed_readings(&router, &[("anthropic-c", 0.0)]);

    for _ in 0..3 {
        assert_eq!(
            chain_order(&router, "S"),
            ["opus#anthropic-a", "opus#anthropic-b", "opus#anthropic-c"]
        );
    }
}

#[test]
fn round_robin_still_advances_per_request_with_quota_readings_present() {
    // RoundRobin's contract is a per-REQUEST advance, not a per-session one, so
    // repeated requests under one session key must keep rotating -- and a
    // below-cap reading must not reorder the walk.
    let router = pooled_router_with_selection(SeatSelection::RoundRobin, true);
    seed_readings(&router, &[("anthropic-c", 0.0)]);

    assert_eq!(
        chain_order(&router, "S"),
        ["opus#anthropic-a", "opus#anthropic-b", "opus#anthropic-c"]
    );
    assert_eq!(
        chain_order(&router, "S"),
        ["opus#anthropic-b", "opus#anthropic-c", "opus#anthropic-a"]
    );
    assert_eq!(
        chain_order(&router, "S"),
        ["opus#anthropic-c", "opus#anthropic-a", "opus#anthropic-b"]
    );
    assert_eq!(
        chain_order(&router, "S"),
        ["opus#anthropic-a", "opus#anthropic-b", "opus#anthropic-c"]
    );
}
