//! Router-level tests for quota-aware birth placement.
//!
//! The pure partition is tested in `quota::placement`; what needs a ROUTER is
//! everything the partition cannot see: that the store read is keyed the way
//! the feed writes, that a healthy pin never reaches the store at all, that
//! the provider kind is read off each seat's own entry, and that the
//! `[seat_quota] enabled` key has no say in the pick.
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
fn pooled_router(seat_quota_enabled: bool) -> Router {
    pooled_router_with_selection(SeatSelection::StickyLeastLoaded, seat_quota_enabled)
}

/// A three-seat pool on one Anthropic provider under `selection`, with
/// `seat_quota.enabled` as given.
fn pooled_router_with_selection(selection: SeatSelection, seat_quota_enabled: bool) -> Router {
    pooled_router_on(selection, seat_quota_enabled, ProviderEntry::anthropic_api)
}

/// A three-seat pool under `selection` whose every member's `[providers]`
/// entry is built by `provider_entry` from that member's credential ref, with
/// `seat_quota.enabled` as given.
fn pooled_router_on(
    selection: SeatSelection,
    seat_quota_enabled: bool,
    provider_entry: fn(String) -> ProviderEntry,
) -> Router {
    let mut seats: Vec<SeatTarget> = Vec::new();
    let mut providers = BTreeMap::new();
    for (member, _) in SEAT_MEMBERS {
        let provider: Arc<dyn Provider> = Arc::new(SeatStub {
            id: member.to_string(),
        });
        let member_ref = secret_ref(member);
        seats.push(SeatTarget::new(
            member.to_string(),
            provider,
            Some(member_ref.clone()),
            false,
        ));
        providers.insert(member.to_string(), provider_entry(member_ref.to_string()));
    }
    let default_provider = seats[0].provider.clone().expect("seat 0 is built");

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
    cfg.seat_quota.enabled = seat_quota_enabled;

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
fn a_keyless_request_places_by_quota_whatever_the_seat_quota_key_says() {
    // Keyless placement reads the same tiers as a birth pick, so the key has
    // no say there either: the below-cap seat leads the walk in both rows.
    for seat_quota_enabled in [true, false] {
        let router = pooled_router(seat_quota_enabled);
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
                "opus#anthropic-seat-c",
                "opus#anthropic-default",
                "opus#anthropic-seat-b"
            ],
            "seat_quota.enabled = {seat_quota_enabled}"
        );
    }
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

// ---- the `[seat_quota] enabled` key has no effect ----

/// The paired rows: `enabled = false` places a birth exactly as `enabled =
/// true` does, by quota, and the decision is counted in both. Each reading set
/// pulls the pick off the rotation's seat 0, so a row that skipped quota would
/// land on `opus#anthropic-default` and count nothing.
#[test]
fn birth_placement_follows_quota_tiers_whatever_the_seat_quota_key_says() {
    let cases = [
        (
            "below-cap tier",
            [
                ("anthropic-default", 0.90),
                ("anthropic-seat-b", 0.01),
                ("anthropic-seat-c", 0.50),
            ],
            "opus#anthropic-seat-b",
        ),
        (
            "all capped",
            [
                ("anthropic-default", 0.99),
                ("anthropic-seat-b", 0.85),
                ("anthropic-seat-c", 0.70),
            ],
            "opus#anthropic-seat-c",
        ),
    ];
    for (name, readings, expected_home) in cases {
        for seat_quota_enabled in [true, false] {
            // Arrange
            let router = pooled_router(seat_quota_enabled);
            seed_readings(&router, &readings);

            // Act
            let home = home_of(&router, "S");

            // Assert
            assert_eq!(
                home, expected_home,
                "{name}: seat_quota.enabled = {seat_quota_enabled}"
            );
            let totals = router.metrics.quota_placement_totals();
            assert_eq!(
                totals.below_cap + totals.all_capped,
                1,
                "{name}: seat_quota.enabled = {seat_quota_enabled} must count \
                 the placing arm: {totals:?}"
            );
        }
    }
}

/// An uncurated provider kind still contributes no tiers: the same reading
/// that moves an Anthropic pool's birth pick leaves an `openai-compat` pool on
/// the rotation's seat 0, and quota counts nothing, whatever the key says.
#[test]
fn an_uncurated_provider_kind_gets_no_tiers() {
    fn openai_compat(member_ref: String) -> ProviderEntry {
        ProviderEntry::openai_compat("https://api.example.com/v1", member_ref)
    }

    for seat_quota_enabled in [true, false] {
        // Arrange
        let router = pooled_router_on(
            SeatSelection::StickyLeastLoaded,
            seat_quota_enabled,
            openai_compat,
        );
        seed_readings(&router, &[("anthropic-seat-c", 0.0)]);

        // Act
        let home = home_of(&router, "S");

        // Assert
        assert_eq!(
            home, "opus#anthropic-default",
            "seat_quota.enabled = {seat_quota_enabled}"
        );
        assert_eq!(
            router.metrics.quota_placement_totals(),
            QuotaPlacementTotals {
                below_cap: 0,
                all_capped: 0,
                mixed_unknown: 0,
                all_unknown: 0,
            },
            "seat_quota.enabled = {seat_quota_enabled}"
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
fn a_quota_birth_writes_an_unrepinned_pin_that_a_follow_up_keeps() {
    // Quota changes WHICH seat wins inside the existing birth path and never
    // whether or how the pin is written: the quota-chosen home is pinned as an
    // ordinary birth, and the next turn stays on it without a rewrite.
    let router = pooled_router(true);
    seed_readings(&router, &[("anthropic-seat-c", 0.0)]);
    let pin_key = super::chain::sticky_pin_key("S", SEAT_POOL);

    let birth = chain_order(&router, "S");
    let pin = router
        .sticky_pins
        .get(&pin_key)
        .expect("a keyed quota birth writes a pin");
    assert_eq!(birth[0], "opus#anthropic-seat-c");
    assert_eq!(pin.member, "anthropic-seat-c");
    assert!(
        !pin.repinned,
        "a quota birth is an ordinary, un-repinned birth"
    );

    let mut stay = Vec::new();
    let rebirths = routectl_testkit::capture_events(|| stay = chain_order(&router, "S"))
        .iter()
        .filter(|e| e.message == "sticky least-loaded birth pick: pinned session to seat")
        .count();
    assert_eq!(
        rebirths, 0,
        "the follow-up reads the pin rather than re-picking"
    );
    assert_eq!(stay, birth, "the follow-up walks the same order");
    assert_eq!(
        router.sticky_pins.get(&pin_key),
        Some(pin),
        "the follow-up stays without rewriting the pin"
    );
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
