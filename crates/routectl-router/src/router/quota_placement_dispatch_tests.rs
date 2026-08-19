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
/// The pool's members, paired with the SEAT LABEL each one's credential
/// reference carries within the `anthropic` account family.
///
/// The pairing is load-bearing, not decoration. A quota key is ACCOUNT-scoped
/// (`provider` for a default seat, `provider#label` for a labelled one) while a
/// state key is MODEL-scoped (`nickname#member`), and the two are not
/// interchangeable -- a suite whose members all carried distinct unlabelled
/// refs would key every account by a different `provider` and so could never
/// catch the two derivations disagreeing about the default-vs-labelled
/// distinction. So the first member is the family's DEFAULT seat
/// (`oauth://anthropic`, label `None`) and the other two are labelled siblings
/// of the SAME account family (`oauth://anthropic#seat-b` / `#seat-c`).
const SEAT_MEMBERS: [(&str, Option<&str>); 3] = [
    ("anthropic-default", None),
    ("anthropic-seat-b", Some("seat-b")),
    ("anthropic-seat-c", Some("seat-c")),
];

/// The OAuth account family every member of this pool authenticates against.
const SEAT_FAMILY: &str = "anthropic";

/// The seat label of `member`, per [`SEAT_MEMBERS`].
fn label_of(member: &str) -> Option<&'static str> {
    SEAT_MEMBERS
        .iter()
        .find(|(name, _)| *name == member)
        .map(|(_, label)| *label)
        .expect("every member named by a test is declared in SEAT_MEMBERS")
}

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

/// The credential reference of one member: the shared account family plus that
/// member's own seat label (`None` for the family's default seat).
fn secret_ref(member: &str) -> routectl_auth::SecretRef {
    routectl_auth::SecretRef::OAuth {
        provider: SEAT_FAMILY.to_string(),
        label: label_of(member).map(str::to_string),
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
    for (member, _) in SEAT_MEMBERS {
        let provider: Arc<dyn Provider> = Arc::new(SeatStub {
            id: member.to_string(),
        });
        let member_ref = secret_ref(member);
        seats.push(SeatTarget {
            provider_name: member.to_string(),
            provider,
            auth_secret_ref: Some(member_ref.clone()),
        });
        providers.insert(
            member.to_string(),
            ProviderEntry::anthropic_api(member_ref.to_string()),
        );
    }
    let default_provider = seats[0].provider.clone();

    let mut pools = BTreeMap::new();
    pools.insert(
        SEAT_POOL.to_string(),
        PoolEntry::new(
            SEAT_MEMBERS
                .iter()
                .map(|(member, _)| (*member).to_string())
                .collect(),
        )
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
    seed_readings(
        &router,
        &[("anthropic-default", 0.40), ("anthropic-seat-b", 0.05)],
    );

    // Act
    let home = home_of(&router, "S");

    // Assert: the emptiest KNOWN seat, not the rotation's seat 0.
    assert_eq!(home, "opus#anthropic-seat-b");
}

#[test]
fn a_known_empty_seat_beats_the_seats_with_no_reading() {
    // The signal the feature exists to use: seat-c has never reported, and a
    // seat known to be nearly empty must win over it rather than the reverse.
    let router = pooled_router(true);
    seed_readings(&router, &[("anthropic-seat-c", 0.02)]);

    assert_eq!(home_of(&router, "S"), "opus#anthropic-seat-c");
}

#[test]
fn a_healthy_pin_is_never_moved_by_a_capped_reading() {
    // The birth pins seat-b (its only reading, below cap). Then seat-b's window
    // is reported FULL while a sibling reports empty. The session must stay:
    // a soft cap never costs a warm prompt cache.
    let router = pooled_router(true);
    seed_readings(&router, &[("anthropic-seat-b", 0.05)]);
    assert_eq!(home_of(&router, "S"), "opus#anthropic-seat-b");

    seed_readings(
        &router,
        &[("anthropic-seat-b", 1.0), ("anthropic-seat-c", 0.0)],
    );

    assert_eq!(
        home_of(&router, "S"),
        "opus#anthropic-seat-b",
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
    seed_readings(&router, &[("anthropic-seat-c", 0.0)]);

    let order: Vec<String> = router
        .dispatch_chain("opus", None)
        .expect("chain resolves")
        .into_iter()
        .map(|t| t.state_key)
        .collect();

    assert_eq!(
        order.first().map(String::as_str),
        Some("opus#anthropic-seat-c"),
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
        vec![
            "opus#anthropic-default",
            "opus#anthropic-seat-b",
            "opus#anthropic-seat-c"
        ]
    );
}

#[test]
fn a_keyless_request_with_the_switch_off_keeps_the_unchanged_walk() {
    // OFF must not consult quota for a keyless request either, so a reading
    // that would otherwise lead the walk changes nothing.
    let router = pooled_router(false);
    seed_readings(&router, &[("anthropic-seat-c", 0.0)]);

    let order: Vec<String> = router
        .dispatch_chain("opus", None)
        .expect("chain resolves")
        .into_iter()
        .map(|t| t.state_key)
        .collect();

    assert_eq!(
        order,
        vec![
            "opus#anthropic-default",
            "opus#anthropic-seat-b",
            "opus#anthropic-seat-c"
        ]
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
            ("anthropic-default", 0.99),
            ("anthropic-seat-b", 0.70),
            ("anthropic-seat-c", 0.85),
        ],
    );

    assert_eq!(home_of(&router, "S"), "opus#anthropic-seat-b");
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
            ("anthropic-default", 0.90),
            ("anthropic-seat-b", 0.01),
            ("anthropic-seat-c", 0.50),
        ],
        // A pool where every seat is over cap.
        vec![
            ("anthropic-default", 0.99),
            ("anthropic-seat-b", 0.70),
            ("anthropic-seat-c", 0.85),
        ],
        // A partially observed pool -- the mixed arm.
        vec![("anthropic-seat-b", 0.95)],
        // A single below-cap seat, the strongest possible pull off seat 0.
        vec![("anthropic-seat-c", 0.0)],
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
fn the_default_seat_and_its_labelled_siblings_mint_distinct_quota_keys() {
    // THE dimension this suite exists to hold end-to-end, and the one a fixture
    // of three unrelated unlabelled refs cannot exercise at all: within ONE
    // account family the default seat keys as the bare provider while a
    // labelled sibling keys as `provider#label`. Two derivations produce these
    // keys -- the read side from a `SecretRef`, the write side from the identity
    // a dispatch derived -- and a disagreement is SILENTLY GREEN: every write
    // lands, every read misses, every lane reads as no-evidence.
    let keys: Vec<String> = SEAT_MEMBERS
        .iter()
        .map(|(member, _)| seat_key(member).as_str().to_string())
        .collect();

    assert_eq!(
        keys,
        vec![
            SEAT_FAMILY.to_string(),
            format!("{SEAT_FAMILY}#seat-b"),
            format!("{SEAT_FAMILY}#seat-c"),
        ],
        "the default seat must key as the bare family and each labelled sibling \
         as `family#label` -- all three inside ONE account family"
    );

    // And the model-scoped state keys stay distinct from the account-scoped
    // quota keys: the two are not interchangeable, which is exactly why a
    // `state_key` cannot be passed where a seat key is expected.
    let router = pooled_router(true);
    let state_keys = chain_order(&router, "S");
    for key in &keys {
        assert!(
            !state_keys.iter().any(|sk| sk == key),
            "quota key `{key}` must never appear as a state key: {state_keys:?}"
        );
    }
}

#[test]
fn a_reading_on_one_labelled_seat_does_not_bleed_onto_its_family_siblings() {
    // The consequence of the key distinction, at the store boundary: seeding
    // ONE labelled sibling must leave the family's default seat and the other
    // sibling with no evidence. A collapsed keyspace (all three keying by the
    // bare family) would show the same reading on all three.
    let router = pooled_router(true);
    seed_readings(&router, &[("anthropic-seat-b", 0.05)]);

    let now = ObservationStamp::now();
    assert!(
        matches!(
            router
                .quota_store
                .reading_for(&seat_key("anthropic-seat-b"), &now)
                .expect("the seeded sibling has a reading")
                .fast,
            QuotaWindow::Known { .. }
        ),
        "the seeded labelled seat must hold the reading"
    );
    for unseeded in ["anthropic-default", "anthropic-seat-c"] {
        let reading = router.quota_store.reading_for(&seat_key(unseeded), &now);
        assert!(
            reading.is_none_or(|r| r.fast == QuotaWindow::Unknown),
            "{unseeded} must carry no evidence -- a reading here means the \
             account keyspace collapsed across the family"
        );
    }
}

#[test]
fn off_agrees_with_the_baseline_on_the_pin_it_writes() {
    // Byte-identity covers the pin too, not only the returned order: a
    // different home would be a different pin and every later turn of the
    // conversation would diverge.
    let off = pooled_router(false);
    seed_readings(&off, &[("anthropic-seat-c", 0.0)]);
    let baseline = pooled_router(true);

    let _ = chain_order(&off, "S");
    let _ = chain_order(&baseline, "S");

    // Keyed by the POOL, not the model: pool-backed pins share one lane across
    // every model of the pool. Reading the old model-keyed namespace here made
    // both lookups miss, so the comparison held two `None`s equal and asserted
    // nothing -- hence the explicit Some checks before the value comparison.
    let pin_key = super::chain::sticky_pin_key("S", SEAT_POOL);
    let off_pin = off.sticky_pins.get(&pin_key);
    let baseline_pin = baseline.sticky_pins.get(&pin_key);
    assert!(
        off_pin.is_some(),
        "the switched-off chooser must have written a pin under {pin_key}"
    );
    assert!(
        baseline_pin.is_some(),
        "the baseline chooser must have written a pin under {pin_key}"
    );
    assert_eq!(off_pin, baseline_pin);
}

#[test]
fn off_keeps_collecting_and_aging_observations() {
    // Following the learned correction's switch: OFF stops the reading from
    // being APPLIED and nothing else. The store keeps accepting and keeps
    // expiring, so re-enabling is instant rather than a re-observe.
    let router = pooled_router(false);
    seed_readings(&router, &[("anthropic-seat-b", 0.05)]);
    let _ = chain_order(&router, "S");

    // Collected while off.
    let reading = router
        .quota_store
        .reading_for(&seat_key("anthropic-seat-b"), &ObservationStamp::now())
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
        .reading_for(&seat_key("anthropic-seat-b"), &past_reset)
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
        home, "opus#anthropic-default",
        "the rotation's birth pick with no quota"
    );

    // Park the pinned home so it is no longer dispatchable.
    router
        .state
        .get("opus#anthropic-default")
        .expect("the home seat has a state slot")
        .lock()
        .force_open(std::time::Instant::now(), Duration::from_mins(5));

    let migrated = home_of(&router, "S");

    assert_ne!(
        migrated, "opus#anthropic-default",
        "an unhealthy home must still migrate once with the switch off"
    );
}

#[test]
fn off_emits_no_quota_placement_diagnostic() {
    // The diagnostics must be silent with the switch off, or an operator who
    // turned the feature off would still see it deciding. The dormant arm is
    // deliberately uncounted for the same reason.
    let router = pooled_router(false);
    seed_readings(&router, &[("anthropic-seat-b", 0.05)]);

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
    seed_readings(&below, &[("anthropic-seat-b", 0.05)]);
    let _ = chain_order(&below, "S");
    assert_eq!(below.metrics.quota_placement_totals().below_cap, 1);

    // An all-capped pick.
    let capped = pooled_router(true);
    seed_readings(
        &capped,
        &[
            ("anthropic-default", 0.99),
            ("anthropic-seat-b", 0.70),
            ("anthropic-seat-c", 0.85),
        ],
    );
    let _ = chain_order(&capped, "S");
    assert_eq!(capped.metrics.quota_placement_totals().all_capped, 1);

    // A mixed capped-known / unknown fall-through.
    let mixed = pooled_router(true);
    seed_readings(&mixed, &[("anthropic-default", 0.99)]);
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
    seed_readings(&router, &[("anthropic-seat-c", 0.0)]);

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
            ("anthropic-default", 0.40),
            ("anthropic-seat-b", 0.05),
            ("anthropic-seat-c", 0.30),
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
        Some("opus#anthropic-seat-b"),
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
    seed_readings(&router, &[("anthropic-seat-c", 0.0)]);

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
    seed_readings(&router, &[("anthropic-seat-c", 0.0)]);

    for _ in 0..3 {
        assert_eq!(
            chain_order(&router, "S"),
            [
                "opus#anthropic-default",
                "opus#anthropic-seat-b",
                "opus#anthropic-seat-c"
            ]
        );
    }
}

#[test]
fn round_robin_still_advances_per_request_with_quota_readings_present() {
    // RoundRobin's contract is a per-REQUEST advance, not a per-session one, so
    // repeated requests under one session key must keep rotating -- and a
    // below-cap reading must not reorder the walk.
    let router = pooled_router_with_selection(SeatSelection::RoundRobin, true);
    seed_readings(&router, &[("anthropic-seat-c", 0.0)]);

    assert_eq!(
        chain_order(&router, "S"),
        [
            "opus#anthropic-default",
            "opus#anthropic-seat-b",
            "opus#anthropic-seat-c"
        ]
    );
    assert_eq!(
        chain_order(&router, "S"),
        [
            "opus#anthropic-seat-b",
            "opus#anthropic-seat-c",
            "opus#anthropic-default"
        ]
    );
    assert_eq!(
        chain_order(&router, "S"),
        [
            "opus#anthropic-seat-c",
            "opus#anthropic-default",
            "opus#anthropic-seat-b"
        ]
    );
    assert_eq!(
        chain_order(&router, "S"),
        [
            "opus#anthropic-default",
            "opus#anthropic-seat-b",
            "opus#anthropic-seat-c"
        ]
    );
}
