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

use crate::config::{ProviderEntry, ProviderRuntimePolicy, SeatSelection};
use crate::quota::freshness::{ObservationStamp, accept_reset};
use crate::quota::key::{SeatKey, seat_key_for_secret_ref};
use crate::quota::reduce::QuotaSnapshot;
use crate::quota::window::{Billing, QuotaWindow, Utilization};
use crate::seat_pool::SeatTarget;
use std::time::Duration;

const SEAT_PROVIDER: &str = "anthropic";
const FAST_WINDOW: Duration = Duration::from_hours(5);
const SEAT_LABELS: [Option<&str>; 3] = [None, Some("seat-b"), Some("seat-c")];

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

fn secret_ref(label: Option<&str>) -> routectl_auth::SecretRef {
    routectl_auth::SecretRef::OAuth {
        provider: SEAT_PROVIDER.to_string(),
        label: label.map(str::to_string),
    }
}

/// The store key for one seat, derived through the EXPOSED read-side helper.
/// A hand-built key would pass whichever key the store happened to use, which
/// is exactly the silently-green failure this feature has to avoid.
fn seat_key(label: Option<&str>) -> SeatKey {
    seat_key_for_secret_ref(Some(&secret_ref(label))).expect("an oauth ref yields a key")
}

/// A three-seat `StickyLeastLoaded` pool on one Anthropic provider, with
/// `seat_quota.enabled` as given.
fn pooled_router(quota_enabled: bool) -> Router {
    let mut seats: Vec<SeatTarget> = Vec::new();
    for label in SEAT_LABELS {
        let provider: Arc<dyn Provider> = Arc::new(SeatStub {
            id: format!("anthropic-{}", label.unwrap_or("default")),
        });
        seats.push(SeatTarget {
            label: label.map(str::to_string),
            state_key: crate::seat_pool::seat_state_key("opus", label),
            provider,
            auth_secret_ref: Some(secret_ref(label)),
        });
    }
    let default_provider = seats[0].provider.clone();

    let mut providers = BTreeMap::new();
    providers.insert(
        SEAT_PROVIDER.to_string(),
        ProviderEntry::anthropic_api(format!("oauth://{SEAT_PROVIDER}")).with_runtime(
            ProviderRuntimePolicy {
                seat_selection: SeatSelection::StickyLeastLoaded,
                ..Default::default()
            },
        ),
    );
    let mut cfg = Config {
        providers,
        ..Config::default()
    };
    cfg.seat_quota.enabled = quota_enabled;

    let mut router = Router::new(Arc::new(cfg));
    let model = ResolvedModel::new("opus", SEAT_PROVIDER, default_provider, "claude-opus-4-7")
        .with_seats(Arc::from(seats));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert("opus".to_string(), Arc::new(model));
    router.install_resolved_models(models);
    router
}

/// Store one FAST reading per `(label, utilization)` pair, observed now.
fn seed_readings(router: &Router, readings: &[(Option<&str>, f64)]) {
    let observed = ObservationStamp::now();
    for (label, fraction) in readings {
        let reset_at = accept_reset(
            std::time::SystemTime::now() + FAST_WINDOW / 2,
            &observed,
            FAST_WINDOW,
            crate::quota::curation::RESET_TOLERANCE,
        )
        .expect("a reset inside the window is accepted");
        let stored = router.quota_store.observe(
            &seat_key(*label),
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
    seed_readings(&router, &[(None, 0.40), (Some("seat-b"), 0.05)]);

    // Act
    let home = home_of(&router, "S");

    // Assert: the emptiest KNOWN seat, not the rotation's seat 0.
    assert_eq!(home, "opus#seat-b");
}

#[test]
fn a_known_empty_seat_beats_the_seats_with_no_reading() {
    // The signal the feature exists to use: seat-c has never reported, and a
    // seat known to be nearly empty must win over it rather than the reverse.
    let router = pooled_router(true);
    seed_readings(&router, &[(Some("seat-c"), 0.02)]);

    assert_eq!(home_of(&router, "S"), "opus#seat-c");
}

#[test]
fn a_healthy_pin_is_never_moved_by_a_capped_reading() {
    // The birth pins seat-b (its only reading, below cap). Then seat-b's window
    // is reported FULL while a sibling reports empty. The session must stay:
    // a soft cap never costs a warm prompt cache.
    let router = pooled_router(true);
    seed_readings(&router, &[(Some("seat-b"), 0.05)]);
    assert_eq!(home_of(&router, "S"), "opus#seat-b");

    seed_readings(&router, &[(Some("seat-b"), 1.0), (Some("seat-c"), 0.0)]);

    assert_eq!(
        home_of(&router, "S"),
        "opus#seat-b",
        "a pinned over-cap session runs to actual exhaustion rather than migrating"
    );
}

#[test]
fn a_keyless_request_creates_no_pin_and_places_by_the_unchanged_order() {
    // A request with no inbound session key makes no sticky pick at all, so it
    // mints no pin -- and its order is the unchanged fill-first walk, which is
    // what the keyless collapse has always been.
    let router = pooled_router(true);
    seed_readings(&router, &[(Some("seat-c"), 0.0)]);

    let order: Vec<String> = router
        .dispatch_chain("opus", None)
        .expect("chain resolves")
        .into_iter()
        .map(|t| t.state_key)
        .collect();

    assert_eq!(order, vec!["opus", "opus#seat-b", "opus#seat-c"]);
    assert!(
        router.sticky_pins.export_entries().is_empty(),
        "a keyless request must create no pin"
    );
}

#[test]
fn an_all_capped_pool_still_places_and_takes_the_most_remaining() {
    // Every seat over its cap. The request is NOT failed; it lands on the seat
    // with the most left.
    let router = pooled_router(true);
    seed_readings(
        &router,
        &[(None, 0.99), (Some("seat-b"), 0.70), (Some("seat-c"), 0.85)],
    );

    assert_eq!(home_of(&router, "S"), "opus#seat-b");
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
        vec![(None, 0.90), (Some("seat-b"), 0.01), (Some("seat-c"), 0.50)],
        // A pool where every seat is over cap.
        vec![(None, 0.99), (Some("seat-b"), 0.70), (Some("seat-c"), 0.85)],
        // A partially observed pool -- the mixed arm.
        vec![(Some("seat-b"), 0.95)],
        // A single below-cap seat, the strongest possible pull off seat 0.
        vec![(Some("seat-c"), 0.0)],
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
    seed_readings(&off, &[(Some("seat-c"), 0.0)]);
    let baseline = pooled_router(true);

    let _ = chain_order(&off, "S");
    let _ = chain_order(&baseline, "S");

    assert_eq!(
        off.sticky_pins.export_entries(),
        baseline.sticky_pins.export_entries(),
    );
}

#[test]
fn off_keeps_collecting_and_aging_observations() {
    // Following the learned correction's switch: OFF stops the reading from
    // being APPLIED and nothing else. The store keeps accepting and keeps
    // expiring, so re-enabling is instant rather than a re-observe.
    let router = pooled_router(false);
    seed_readings(&router, &[(Some("seat-b"), 0.05)]);
    let _ = chain_order(&router, "S");

    // Collected while off.
    let reading = router
        .quota_store
        .reading_for(&seat_key(Some("seat-b")), &ObservationStamp::now())
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
        .reading_for(&seat_key(Some("seat-b")), &past_reset)
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
    assert_eq!(home, "opus", "the rotation's birth pick with no quota");

    // Park the pinned home so it is no longer dispatchable.
    router
        .state
        .get("opus")
        .expect("the home seat has a state slot")
        .lock()
        .force_open(std::time::Instant::now(), Duration::from_mins(5));

    let migrated = home_of(&router, "S");

    assert_ne!(
        migrated, "opus",
        "an unhealthy home must still migrate once with the switch off"
    );
}

#[test]
fn off_emits_no_quota_placement_diagnostic() {
    // The diagnostics must be silent with the switch off, or an operator who
    // turned the feature off would still see it deciding. The dormant arm is
    // deliberately uncounted for the same reason.
    let router = pooled_router(false);
    seed_readings(&router, &[(Some("seat-b"), 0.05)]);

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
    seed_readings(&below, &[(Some("seat-b"), 0.05)]);
    let _ = chain_order(&below, "S");
    assert_eq!(below.metrics.quota_placement_totals().below_cap, 1);

    // An all-capped pick.
    let capped = pooled_router(true);
    seed_readings(
        &capped,
        &[(None, 0.99), (Some("seat-b"), 0.70), (Some("seat-c"), 0.85)],
    );
    let _ = chain_order(&capped, "S");
    assert_eq!(capped.metrics.quota_placement_totals().all_capped, 1);

    // A mixed capped-known / unknown fall-through.
    let mixed = pooled_router(true);
    seed_readings(&mixed, &[(None, 0.99)]);
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
    seed_readings(&router, &[(Some("seat-c"), 0.0)]);

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
