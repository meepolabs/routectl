//! Router-level tests for OAuth credential-pool dispatch: a pooled
//! model expands into one DispatchTarget per seat, each with its own
//! breaker entry, ordered by the provider's `seat_selection`.

use super::*;
use crate::config::{ProviderEntry, ProviderRuntimePolicy, SeatSelection};
use crate::seat_pool::SeatTarget;
use async_trait::async_trait;
use routectl_core::{Choice, Message};
use std::sync::atomic::{AtomicUsize, Ordering};

/// Provider that records each `complete` call against a shared
/// counter so a test can assert which seat served a request.
struct SeatProvider {
    id: String,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for SeatProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(&self.id, "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse {
            id: format!("ok-{}", self.id),
            model: req.model,
            created: 0,
            choices: vec![Choice {
                logprobs: None,
                index: 0,
                message: Message {
                    refusal: None,
                    role: routectl_core::Role::Assistant,
                    content: routectl_core::MessageContent::Text("ok".into()),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
                finish_reason: Some("stop".into()),
                matched_stop_sequence: None,
            }],
            usage: Some(routectl_core::Usage::default()),
            routectl_provider: None,
            extras: Default::default(),
            upstream_meta: None,
        })
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!()
    }
}

/// Build a pooled model `opus` on provider `anthropic` with three
/// seats (default + seat-b + seat-c), each backed by its own
/// `SeatProvider` + call counter. Returns the installed Router plus
/// the three per-seat counters in seat order.
fn pooled_router(selection: SeatSelection) -> (Router, Vec<Arc<AtomicUsize>>) {
    pooled_router_with_labels(
        selection,
        &[None, Some("seat-b".into()), Some("seat-c".into())],
    )
}

/// Build a pooled `opus` model with one seat per entry in `labels`
/// (`None` is the default seat). Lets a test stand up pools of
/// arbitrary seat sets -- e.g. a "before reload" two-seat pool and an
/// "after reload" three-seat pool -- to exercise the coordinator's
/// rebuild + per-state_key carry-over.
fn pooled_router_with_labels(
    selection: SeatSelection,
    labels: &[Option<String>],
) -> (Router, Vec<Arc<AtomicUsize>>) {
    let mut counters = Vec::new();
    let mut seats: Vec<SeatTarget> = Vec::new();
    for label in labels {
        let counter = Arc::new(AtomicUsize::new(0));
        counters.push(counter.clone());
        let provider: Arc<dyn Provider> = Arc::new(SeatProvider {
            id: format!("anthropic-{}", label.as_deref().unwrap_or("default")),
            calls: counter,
        });
        seats.push(SeatTarget {
            label: label.clone(),
            state_key: crate::seat_pool::seat_state_key("opus", label.as_deref()),
            provider,
            auth_secret_ref: None,
        });
    }
    let default_provider = seats[0].provider.clone();

    let mut providers = BTreeMap::new();
    let runtime = ProviderRuntimePolicy {
        seat_selection: selection,
        ..Default::default()
    };
    providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api("oauth://anthropic").with_runtime(runtime),
    );
    let cfg = Arc::new(Config {
        providers,
        ..Config::default()
    });

    let mut router = Router::new(cfg);
    let model = ResolvedModel::new("opus", "anthropic", default_provider, "claude-opus-4-7")
        .with_seats(Arc::from(seats));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert("opus".to_string(), Arc::new(model));
    router.install_resolved_models(models);
    (router, counters)
}

fn req() -> ChatRequest {
    ChatRequest {
        model: "opus".into(),
        messages: vec![].into(),
        ..Default::default()
    }
}

/// The seat order produced by `dispatch_chain` for `opus`, as a list
/// of `state_key`s. Same-module access to the private method.
fn chain_state_keys(router: &Router) -> Vec<String> {
    chain_state_keys_for(router, None)
}

/// Like [`chain_state_keys`] but threads an explicit inbound session key
/// (the sticky-pin lookup key) into resolution.
fn chain_state_keys_for(router: &Router, session_key: Option<&str>) -> Vec<String> {
    router
        .dispatch_chain("opus", session_key)
        .expect("chain resolves")
        .into_iter()
        .map(|t| t.state_key)
        .collect()
}

/// The `selection_decision` token on each target of the resolved chain
/// for `opus`. Same-module access to the private field; lets the
/// observability tests assert which token (if any) landed on the home
/// seat without changing any routing.
fn chain_decisions_for(router: &Router, session_key: Option<&str>) -> Vec<Option<&'static str>> {
    router
        .dispatch_chain("opus", session_key)
        .expect("chain resolves")
        .into_iter()
        .map(|t| t.selection_decision)
        .collect()
}

#[tokio::test]
async fn fill_first_records_no_selection_decision() {
    // A genuinely non-sticky pool has no sticky decision: every target's
    // selection_decision is None.
    let (router, _counters) = pooled_router(SeatSelection::FillFirst);
    let decisions = chain_decisions_for(&router, None);
    assert_eq!(decisions, vec![None, None, None]);
}

#[tokio::test]
async fn sticky_keyed_records_decision_on_home_only() {
    // A keyed StickyLeastLoaded pool stamps the sticky token on the home
    // (first) target ONLY -- birth_pick on the first request, sticky_stay
    // on a follow-up for the same session. The fallback seats stay None.
    let (router, _counters) = pooled_router(SeatSelection::StickyLeastLoaded);

    let birth = chain_decisions_for(&router, Some("S"));
    assert_eq!(
        birth,
        vec![Some("birth_pick"), None, None],
        "first request for a session is a birth pick on the home seat"
    );

    let stay = chain_decisions_for(&router, Some("S"));
    assert_eq!(
        stay,
        vec![Some("sticky_stay"), None, None],
        "a follow-up for the same session stays on the pinned home seat"
    );
}

#[tokio::test]
async fn keyless_sticky_records_keyless_fill_first() {
    // StickyLeastLoaded on a multi-seat pool WITHOUT a session key
    // collapses to fill-first; the token must surface that collapse so an
    // operator can spot it. Order is unchanged (byte-for-byte fill-first).
    let (router, _counters) = pooled_router(SeatSelection::StickyLeastLoaded);
    let decisions = chain_decisions_for(&router, None);
    assert_eq!(decisions, vec![Some("keyless_fill_first"), None, None]);
    // The collapse must not alter the seat order.
    assert_eq!(
        chain_state_keys_for(&router, None),
        vec!["opus", "opus#seat-b", "opus#seat-c"]
    );
}

#[test]
fn mark_target_copies_selection_decision_into_meta() {
    // mark_target propagates the home target's selection_decision into
    // the per-request DispatchMeta exactly like the served_* fields.
    let (router, _counters) = pooled_router(SeatSelection::StickyLeastLoaded);
    let chain = router
        .dispatch_chain("opus", Some("S"))
        .expect("chain resolves");
    let home = &chain[0];
    assert_eq!(home.selection_decision, Some("birth_pick"));

    let mut meta = DispatchMeta::for_alias("opus");
    meta.mark_target(home, "opus");
    assert_eq!(meta.selection_decision, Some("birth_pick"));
}

#[tokio::test]
async fn sticky_overflow_repin_stamps_overflow_repin_token() {
    // The thrash signal: a session pinned (birth_pick) whose home seat
    // then trips must, on re-request, migrate to a healthy sibling AND
    // stamp `overflow_repin` on the NEW home target. Reuses the
    // park-and-re-request seam from
    // `sticky_overflow_repin_migrates_once_and_does_not_flap`.
    let (router, _counters) = pooled_router(SeatSelection::StickyLeastLoaded);

    // Birth: first request pins session S and stamps birth_pick.
    let birth = chain_decisions_for(&router, Some("S"));
    assert_eq!(birth[0], Some("birth_pick"));
    let home = chain_state_keys_for(&router, Some("S"))[0].clone();

    // Force the pinned home seat's breaker open.
    assert!(
        router.force_open_breaker(&home, Duration::from_hours(1)),
        "home seat must own a state slot to trip"
    );

    // Re-request ONCE: the migration stamps overflow_repin on the new
    // home (first) target only; fallback seats stay None. A single
    // resolution is read so the one-time-cap (repinned=true) does not
    // turn a follow-up into a sticky_stay before we observe the token.
    let migrated = router
        .dispatch_chain("opus", Some("S"))
        .expect("chain resolves");
    let migrated_decisions: Vec<Option<&'static str>> =
        migrated.iter().map(|t| t.selection_decision).collect();
    assert_ne!(
        migrated[0].state_key, home,
        "overflow-repin must migrate off the parked home seat"
    );
    assert_eq!(
        migrated_decisions,
        vec![Some("overflow_repin"), None, None],
        "the thrash signal must land on the migrated home target only"
    );
}

#[tokio::test]
async fn sticky_defer_no_healthy_stamps_defer_token() {
    // A fresh keyed session (pin miss) over a pool whose every seat's
    // breaker is forced open has no dispatchable home: the outcome is
    // DeferNoHealthy -> `defer_no_healthy` token, fill-first order, and
    // NO pin written.
    let (router, _counters) = pooled_router(SeatSelection::StickyLeastLoaded);
    for key in ["opus", "opus#seat-b", "opus#seat-c"] {
        assert!(
            router.force_open_breaker(key, Duration::from_hours(1)),
            "seat {key} must own a state slot to trip"
        );
    }

    let decisions = chain_decisions_for(&router, Some("S"));
    assert_eq!(
        decisions,
        vec![Some("defer_no_healthy"), None, None],
        "a no-healthy-seat miss must stamp defer_no_healthy on the home target"
    );
    // Order is the fill-first walk (a hint, not a filter), and no pin
    // was written for the deferred session.
    assert_eq!(
        chain_state_keys_for(&router, Some("S")),
        vec!["opus", "opus#seat-b", "opus#seat-c"]
    );
    assert!(
        router.sticky_pins.get("S").is_none(),
        "DeferNoHealthy must not write a pin"
    );
}

#[tokio::test]
async fn each_seat_has_independent_breaker() {
    // Parking seat-a (force_open) must leave seat-b/seat-c
    // dispatchable -- the three seats own distinct state_key slots,
    // so there is no shared breaker.
    let (router, _counters) = pooled_router(SeatSelection::FillFirst);
    // All three seats own a state slot.
    assert!(router.state.contains_key("opus"));
    assert!(router.state.contains_key("opus#seat-b"));
    assert!(router.state.contains_key("opus#seat-c"));

    // Park the default seat for a long cooldown.
    router.park_provider("opus", Duration::from_hours(1));

    // The default seat's breaker is open; siblings are untouched.
    assert!(
        router.gate_check("opus", "anthropic").is_some(),
        "parked default seat must gate-block"
    );
    assert!(
        router.gate_check("opus#seat-b", "anthropic").is_none(),
        "sibling seat-b must remain dispatchable"
    );
    assert!(
        router.gate_check("opus#seat-c", "anthropic").is_none(),
        "sibling seat-c must remain dispatchable"
    );
}

#[tokio::test]
async fn fill_first_walks_seats_in_fixed_order() {
    // FillFirst: the chain's seat order is stable across requests
    // (default seat first, then sorted labels).
    let (router, _counters) = pooled_router(SeatSelection::FillFirst);
    let first = chain_state_keys(&router);
    let second = chain_state_keys(&router);
    assert_eq!(first, vec!["opus", "opus#seat-b", "opus#seat-c"]);
    assert_eq!(second, vec!["opus", "opus#seat-b", "opus#seat-c"]);
}

#[tokio::test]
async fn round_robin_rotates_start_seat_per_request() {
    // RoundRobin: the starting seat advances by one per request and
    // wraps modulo the seat count.
    let (router, _counters) = pooled_router(SeatSelection::RoundRobin);
    assert_eq!(
        chain_state_keys(&router),
        vec!["opus", "opus#seat-b", "opus#seat-c"]
    );
    assert_eq!(
        chain_state_keys(&router),
        vec!["opus#seat-b", "opus#seat-c", "opus"]
    );
    assert_eq!(
        chain_state_keys(&router),
        vec!["opus#seat-c", "opus", "opus#seat-b"]
    );
    assert_eq!(
        chain_state_keys(&router),
        vec!["opus", "opus#seat-b", "opus#seat-c"]
    );
}

#[tokio::test]
async fn parked_seat_is_skipped_and_sibling_serves() {
    // Full dispatch: park the default seat, then a request must fall
    // to the next seat (seat-b) and that seat's provider serves.
    let (router, counters) = pooled_router(SeatSelection::FillFirst);
    router.park_provider("opus", Duration::from_hours(1));

    let resp = router.complete(req()).await.expect("sibling serves");
    assert_eq!(resp.routectl_provider.as_deref(), Some("anthropic"));
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        0,
        "parked default seat must not be hit"
    );
    assert_eq!(
        counters[1].load(Ordering::SeqCst),
        1,
        "seat-b must serve the request"
    );
    assert_eq!(
        counters[2].load(Ordering::SeqCst),
        0,
        "seat-c must not be reached once seat-b succeeds"
    );
}

#[tokio::test]
async fn fill_first_serves_default_seat_first() {
    // Sanity: with no seat parked, FillFirst serves the default seat.
    let (router, counters) = pooled_router(SeatSelection::FillFirst);
    let resp = router.complete(req()).await.expect("default serves");
    assert_eq!(resp.routectl_provider.as_deref(), Some("anthropic"));
    assert_eq!(counters[0].load(Ordering::SeqCst), 1);
    assert_eq!(counters[1].load(Ordering::SeqCst), 0);
    assert_eq!(counters[2].load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn carry_over_preserves_surviving_seat_breaker_and_starts_new_seat_fresh() {
    // Simulate the credentials-reload rebuild: a two-seat pool
    // (default + seat-b) trips the default seat's breaker, then a seat
    // is added on disk so the coordinator rebuilds a THREE-seat pool
    // (default + seat-b + seat-c) and carries over per-state_key
    // runtime state. The surviving seat's tripped breaker must persist
    // (carry-over by state_key); the freshly-added seat must start
    // closed.
    let (before, _c1) =
        pooled_router_with_labels(SeatSelection::FillFirst, &[None, Some("seat-b".into())]);
    // Trip the default seat's breaker for a long cooldown.
    assert!(
        before.force_open_breaker("opus", Duration::from_hours(1)),
        "default seat must own a state slot to trip"
    );
    assert_eq!(
        before.breaker_open_for("opus"),
        Some(true),
        "default seat breaker must read open after force_open"
    );

    // Rebuild with the added seat-c, then carry over from `before`.
    let (mut after, _c2) = pooled_router_with_labels(
        SeatSelection::FillFirst,
        &[None, Some("seat-b".into()), Some("seat-c".into())],
    );
    after.carry_over_runtime_state_from(&before);

    // The surviving default seat's tripped breaker carried over.
    assert_eq!(
        after.breaker_open_for("opus"),
        Some(true),
        "surviving seat's breaker state must survive the rebuild"
    );
    // The freshly-added seat-c starts closed (fresh state).
    assert_eq!(
        after.breaker_open_for("opus#seat-c"),
        Some(false),
        "newly-added seat must start with a fresh, closed breaker"
    );
    // And the pool re-expanded to three seats.
    assert_eq!(after.seat_count_for("opus"), Some(3));
}

#[tokio::test]
async fn sticky_pins_on_miss_then_stays_on_session() {
    // A multi-seat StickyLeastLoaded pool: the first request for session
    // "S" picks (and pins) a home seat; a second request for "S" returns
    // the SAME home seat first (it reads the pin rather than re-picking).
    let (router, _counters) = pooled_router(SeatSelection::StickyLeastLoaded);
    let first = chain_state_keys_for(&router, Some("S"));
    let home = first[0].clone();
    let second = chain_state_keys_for(&router, Some("S"));
    assert_eq!(
        second[0], home,
        "second request for the same session must lead with the pinned home seat"
    );
    // Every seat still appears (the order is a hint, not a filter).
    assert_eq!(first.len(), 3);
    assert_eq!(second.len(), 3);
}

#[tokio::test]
async fn sticky_keyless_matches_fill_first() {
    // Keyless StickyLeastLoaded routes through seat_order_for_request, so
    // its order is identical to a FillFirst pool's.
    let (sticky, _c1) = pooled_router(SeatSelection::StickyLeastLoaded);
    let (fill, _c2) = pooled_router(SeatSelection::FillFirst);
    let sticky_order = chain_state_keys_for(&sticky, None);
    let fill_order = chain_state_keys(&fill);
    assert_eq!(sticky_order, fill_order);
    assert_eq!(sticky_order, vec!["opus", "opus#seat-b", "opus#seat-c"]);
}

#[tokio::test]
async fn sticky_stale_pin_not_in_pool_is_re_picked() {
    // A pin whose state_key no longer exists in the pool resolves to a
    // miss: the request re-picks a valid in-pool seat (and re-pins it).
    let (router, _counters) = pooled_router(SeatSelection::StickyLeastLoaded);
    router.sticky_pins.put(
        "S",
        crate::seat_pool::SeatPin {
            state_key: "opus#seat-gone".to_string(),
            repinned: false,
        },
    );
    let order = chain_state_keys_for(&router, Some("S"));
    let valid = ["opus", "opus#seat-b", "opus#seat-c"];
    assert!(
        valid.contains(&order[0].as_str()),
        "stale pin must re-pick a valid in-pool seat, got {}",
        order[0]
    );
    // The re-pick repaired the pin to an in-pool seat.
    assert!(
        valid.contains(
            &router
                .sticky_pins
                .get("S")
                .expect("re-pinned")
                .state_key
                .as_str()
        )
    );
}

#[tokio::test]
async fn sticky_overflow_repin_migrates_once_and_does_not_flap() {
    // End-to-end overflow-repin: pin a session, force its home seat's breaker open,
    // and assert a subsequent call leads with a healthy sibling AND the
    // pin records repinned=true. Then heal the original and assert the
    // session STAYS on the sibling (hysteresis -- no A->B->A flap). Then
    // park the sibling (new home) and assert the already-repinned session
    // STAYS (does not chase a third seat -- one-time cap).
    let (router, _counters) = pooled_router(SeatSelection::StickyLeastLoaded);

    // Birth: pin session S to its home seat.
    let first = chain_state_keys_for(&router, Some("S"));
    let home = first[0].clone();
    assert!(
        !router.sticky_pins.get("S").expect("pinned").repinned,
        "birth pin must start un-repinned"
    );

    // Force the home seat's breaker open for a long cooldown.
    assert!(
        router.force_open_breaker(&home, Duration::from_hours(1)),
        "home seat must own a state slot to trip"
    );

    // The session migrates ONCE to a healthy sibling.
    let migrated = chain_state_keys_for(&router, Some("S"));
    assert_ne!(
        migrated[0], home,
        "overflow-repin must migrate off the parked home seat"
    );
    let sibling = migrated[0].clone();
    let pin_after = router.sticky_pins.get("S").expect("re-pinned");
    assert_eq!(
        pin_after.state_key, sibling,
        "pin must point at the sibling"
    );
    assert!(
        pin_after.repinned,
        "overflow-repin must set repinned=true (the one-time cap marker)"
    );

    // Heal the original home seat. The session must NOT flap back: the pin
    // now points at the healthy sibling, so it STAYS there.
    router.record_success(&home);
    let healed = chain_state_keys_for(&router, Some("S"));
    assert_eq!(
        healed[0], sibling,
        "a recovered original must NOT pull the session back (no A->B->A flap)"
    );

    // Park the NEW home (the sibling). An already-repinned session must
    // STAY rather than chase a third seat (one-time cap).
    assert!(
        router.force_open_breaker(&sibling, Duration::from_hours(1)),
        "sibling seat must own a state slot to trip"
    );
    let capped = chain_state_keys_for(&router, Some("S"));
    assert_eq!(
        capped[0], sibling,
        "an already-repinned session must not chase a third seat"
    );
    assert_eq!(
        router.sticky_pins.get("S").expect("still pinned").state_key,
        sibling,
        "the pin must remain on the sibling -- no second migration"
    );
}
