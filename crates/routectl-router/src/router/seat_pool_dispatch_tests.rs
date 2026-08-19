//! Router-level tests for OAuth credential-pool dispatch: a pooled
//! model expands into one DispatchTarget per seat, each with its own
//! breaker entry, ordered by the provider's `seat_selection`.

use super::chain::empty_pool_error;
use super::*;
use crate::config::{AliasValue, PoolEntry, ProviderEntry, SeatSelection};
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

/// Build a pooled model `opus` on pool `anthropic-pool` with three member
/// accounts, each backed by its own `SeatProvider` + call counter. Returns
/// the installed Router plus the three per-member counters in member order.
fn pooled_router(selection: SeatSelection) -> (Router, Vec<Arc<AtomicUsize>>) {
    pooled_router_with_members(selection, &["anthropic-a", "anthropic-b", "anthropic-c"])
}

/// Build a pooled `opus` model with one seat per entry in `members`. Lets a
/// test stand up pools of arbitrary membership -- e.g. a "before reload"
/// two-member pool and an "after reload" three-member pool -- to exercise the
/// coordinator's rebuild + per-state_key carry-over.
fn pooled_router_with_members(
    selection: SeatSelection,
    members: &[&str],
) -> (Router, Vec<Arc<AtomicUsize>>) {
    let mut counters = Vec::new();
    let mut seats: Vec<SeatTarget> = Vec::new();
    let mut providers = BTreeMap::new();
    for member in members {
        let counter = Arc::new(AtomicUsize::new(0));
        counters.push(counter.clone());
        let provider: Arc<dyn Provider> = Arc::new(SeatProvider {
            id: (*member).to_string(),
            calls: counter,
        });
        seats.push(SeatTarget {
            provider_name: (*member).to_string(),
            provider,
            auth_secret_ref: Some(routectl_auth::SecretRef::OAuth {
                provider: (*member).to_string(),
                label: None,
            }),
        });
        providers.insert(
            (*member).to_string(),
            ProviderEntry::anthropic_api(format!("oauth://{member}")),
        );
    }
    let default_provider = seats[0].provider.clone();

    let mut pools = BTreeMap::new();
    pools.insert(
        "anthropic-pool".to_string(),
        PoolEntry::new(members.iter().map(|m| (*m).to_string()).collect())
            .with_seat_selection(selection),
    );
    let cfg = Arc::new(Config {
        providers,
        pools,
        ..Config::default()
    });

    let mut router = Router::new(cfg);
    let model = ResolvedModel::new(
        "opus",
        "anthropic-pool",
        default_provider,
        "claude-opus-4-7",
    )
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
        vec!["opus#anthropic-a", "opus#anthropic-b", "opus#anthropic-c"]
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

#[test]
fn mark_target_records_the_served_seats_identity_not_the_first() {
    // Arrange: a three-seat pool whose targets carry per-seat OAuth refs.
    let (router, _counters) = pooled_router(SeatSelection::FillFirst);
    let chain = router.dispatch_chain("opus", None).expect("chain resolves");
    let mut meta = DispatchMeta::for_alias("opus");

    // Act: the walk falls back past the first seat, so the SECOND target
    // is the one that served.
    meta.mark_target(&chain[0], "opus");
    meta.mark_target(&chain[1], "opus");

    // Assert: the served seat's credential identity, not the first's.
    assert_eq!(meta.served_seat, Some("anthropic-b".to_string()));
}

#[test]
fn mark_target_leaves_served_seat_none_for_a_non_oauth_credential() {
    // Arrange: a non-pooled model authenticating with a file:// ref -- a
    // filesystem path that must never reach the usage ledger.
    let mut providers = BTreeMap::new();
    providers.insert(
        "openai".to_string(),
        ProviderEntry::openai_compat("http://127.0.0.1:1", "file:///etc/routectl/key"),
    );
    let cfg = Arc::new(Config {
        providers,
        ..Config::default()
    });
    let mut router = Router::new(cfg);
    let provider: Arc<dyn Provider> = Arc::new(SeatProvider {
        id: "openai".to_string(),
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let model = ResolvedModel::new("gpt", "openai", provider, "gpt-4o").with_auth_secret_ref(
        routectl_auth::SecretRef::parse("file:///etc/routectl/key").expect("parse file ref"),
    );
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert("gpt".to_string(), Arc::new(model));
    router.install_resolved_models(models);
    let chain = router.dispatch_chain("gpt", None).expect("chain resolves");
    let mut meta = DispatchMeta::for_alias("gpt");

    // Act
    meta.mark_target(&chain[0], "gpt");

    // Assert
    assert_eq!(meta.served_seat, None);
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
    for key in ["opus#anthropic-a", "opus#anthropic-b", "opus#anthropic-c"] {
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
        vec!["opus#anthropic-a", "opus#anthropic-b", "opus#anthropic-c"]
    );
    assert!(
        pinned_member(&router, "S").is_none(),
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
    assert!(router.state.contains_key("opus#anthropic-a"));
    assert!(router.state.contains_key("opus#anthropic-b"));
    assert!(router.state.contains_key("opus#anthropic-c"));

    // Park the default seat for a long cooldown.
    router.park_provider("opus#anthropic-a", Duration::from_hours(1));

    // The default seat's breaker is open; siblings are untouched.
    assert!(
        router
            .gate_check("opus#anthropic-a", "anthropic-a")
            .is_some(),
        "parked default seat must gate-block"
    );
    assert!(
        router
            .gate_check("opus#anthropic-b", "anthropic-b")
            .is_none(),
        "sibling seat-b must remain dispatchable"
    );
    assert!(
        router
            .gate_check("opus#anthropic-c", "anthropic-c")
            .is_none(),
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
    assert_eq!(
        first,
        vec!["opus#anthropic-a", "opus#anthropic-b", "opus#anthropic-c"]
    );
    assert_eq!(
        second,
        vec!["opus#anthropic-a", "opus#anthropic-b", "opus#anthropic-c"]
    );
}

#[tokio::test]
async fn round_robin_rotates_start_seat_per_request() {
    // RoundRobin: the starting seat advances by one per request and
    // wraps modulo the seat count.
    let (router, _counters) = pooled_router(SeatSelection::RoundRobin);
    assert_eq!(
        chain_state_keys(&router),
        vec!["opus#anthropic-a", "opus#anthropic-b", "opus#anthropic-c"]
    );
    assert_eq!(
        chain_state_keys(&router),
        vec!["opus#anthropic-b", "opus#anthropic-c", "opus#anthropic-a"]
    );
    assert_eq!(
        chain_state_keys(&router),
        vec!["opus#anthropic-c", "opus#anthropic-a", "opus#anthropic-b"]
    );
    assert_eq!(
        chain_state_keys(&router),
        vec!["opus#anthropic-a", "opus#anthropic-b", "opus#anthropic-c"]
    );
}

#[tokio::test]
async fn parked_seat_is_skipped_and_sibling_serves() {
    // Full dispatch: park the first member's seat, then a request must fall
    // to the next member and that member's provider serves.
    let (router, counters) = pooled_router(SeatSelection::FillFirst);
    router.park_provider("opus#anthropic-a", Duration::from_hours(1));

    let resp = router.complete(req()).await.expect("sibling serves");
    assert_eq!(resp.routectl_provider.as_deref(), Some("anthropic-b"));
    assert_eq!(
        counters[0].load(Ordering::SeqCst),
        0,
        "the parked member must not be hit"
    );
    assert_eq!(
        counters[1].load(Ordering::SeqCst),
        1,
        "the second member must serve the request"
    );
    assert_eq!(
        counters[2].load(Ordering::SeqCst),
        0,
        "the third member must not be reached once the second succeeds"
    );
}

#[tokio::test]
async fn fill_first_serves_default_seat_first() {
    // Sanity: with no seat parked, FillFirst serves the first member.
    let (router, counters) = pooled_router(SeatSelection::FillFirst);
    let resp = router.complete(req()).await.expect("default serves");
    assert_eq!(resp.routectl_provider.as_deref(), Some("anthropic-a"));
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
        pooled_router_with_members(SeatSelection::FillFirst, &["anthropic-a", "anthropic-b"]);
    // Trip the first member's breaker for a long cooldown.
    assert!(
        before.force_open_breaker("opus#anthropic-a", Duration::from_hours(1)),
        "first member must own a state slot to trip"
    );
    assert_eq!(
        before.breaker_open_for("opus#anthropic-a"),
        Some(true),
        "first member's breaker must read open after force_open"
    );

    // Rebuild with the added third member, then carry over from `before`.
    let (mut after, _c2) = pooled_router_with_members(
        SeatSelection::FillFirst,
        &["anthropic-a", "anthropic-b", "anthropic-c"],
    );
    after.carry_over_runtime_state_from(&before);

    // The surviving member's tripped breaker carried over.
    assert_eq!(
        after.breaker_open_for("opus#anthropic-a"),
        Some(true),
        "surviving seat's breaker state must survive the rebuild"
    );
    // The freshly-added member starts closed (fresh state).
    assert_eq!(
        after.breaker_open_for("opus#anthropic-c"),
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
    assert_eq!(
        sticky_order,
        vec!["opus#anthropic-a", "opus#anthropic-b", "opus#anthropic-c"]
    );
}

/// The pool every `pooled_router` fixture builds its model on. The sticky pin
/// namespace is the POOL, not the model, so a test reading a pin keys on this.
const POOL: &str = "anthropic-pool";

/// The member (account) `pooled_router`'s session is pinned to, or `None` when
/// the session holds no pin. Reads the pin through the pool-keyed namespace
/// dispatch writes, so a test cannot accidentally assert against a lane
/// nothing writes.
fn pinned_member(router: &Router, session: &str) -> Option<String> {
    router
        .sticky_pins
        .get(&super::chain::sticky_pin_key(session, POOL))
        .map(|pin| pin.member)
}

/// The seat `state_key` a member maps to for model `opus` -- the shape
/// `chain_state_keys_for` returns. Lets a member-shaped pin be compared
/// against a chain-shaped seat key without either side re-deriving the other's
/// format inline.
fn opus_state_key(member: &str) -> String {
    format!("opus#{member}")
}

#[tokio::test]
async fn sticky_stale_pin_not_in_pool_is_re_picked() {
    // A pin whose member no longer exists in the pool resolves to a
    // miss: the request re-picks a valid in-pool seat (and re-pins it).
    let (router, _counters) = pooled_router(SeatSelection::StickyLeastLoaded);
    router.sticky_pins.put(
        &super::chain::sticky_pin_key("S", POOL),
        crate::seat_pool::SeatPin {
            member: "anthropic-gone".to_string(),
            repinned: false,
        },
    );
    let order = chain_state_keys_for(&router, Some("S"));
    let valid = ["opus#anthropic-a", "opus#anthropic-b", "opus#anthropic-c"];
    assert!(
        valid.contains(&order[0].as_str()),
        "stale pin must re-pick a valid in-pool seat, got {}",
        order[0]
    );
    // The re-pick repaired the pin to an in-pool member.
    let repaired = pinned_member(&router, "S").expect("re-pinned");
    assert!(
        ["anthropic-a", "anthropic-b", "anthropic-c"].contains(&repaired.as_str()),
        "stale pin must re-pick a member the pool still serves, got {repaired}"
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
        !router
            .sticky_pins
            .get(&super::chain::sticky_pin_key("S", POOL))
            .expect("pinned")
            .repinned,
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
    let pin_after = router
        .sticky_pins
        .get(&super::chain::sticky_pin_key("S", POOL))
        .expect("re-pinned");
    assert_eq!(
        opus_state_key(&pin_after.member),
        sibling,
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
    let still_pinned = pinned_member(&router, "S").expect("still pinned");
    assert_eq!(
        opus_state_key(&still_pinned),
        sibling,
        "the pin must remain on the sibling -- no second migration"
    );
}

/// Build a chain `hot = [opusPool, sonnetPool]` over TWO distinct pools --
/// `opus-pool` and `sonnet-pool` -- one model each, both StickyLeastLoaded and
/// both two-member.
///
/// Two POOLS is what makes the independence property meaningful: affinity is
/// namespaced per pool, so two models on DIFFERENT pools must hold distinct
/// pins for one inbound session. (Two models on ONE pool deliberately SHARE a
/// pin -- that is the sibling test below, not this one.) Each pool gets its own
/// member accounts, since a provider entry belongs to at most one pool.
fn two_pool_sticky_chain_router() -> Router {
    const OPUS_MEMBERS: [&str; 2] = ["anthropic-a", "anthropic-b"];
    const SONNET_MEMBERS: [&str; 2] = ["anthropic-c", "anthropic-d"];

    fn seats_for(members: &[&str]) -> Vec<SeatTarget> {
        members
            .iter()
            .map(|member| {
                let provider: Arc<dyn Provider> = Arc::new(SeatProvider {
                    id: (*member).to_string(),
                    calls: Arc::new(AtomicUsize::new(0)),
                });
                SeatTarget {
                    provider_name: (*member).to_string(),
                    provider,
                    auth_secret_ref: None,
                }
            })
            .collect()
    }

    let mut providers = BTreeMap::new();
    for member in OPUS_MEMBERS.iter().chain(SONNET_MEMBERS.iter()) {
        providers.insert(
            (*member).to_string(),
            ProviderEntry::anthropic_api(format!("oauth://{member}")),
        );
    }
    let mut pools = BTreeMap::new();
    for (pool, members) in [("opus-pool", OPUS_MEMBERS), ("sonnet-pool", SONNET_MEMBERS)] {
        pools.insert(
            pool.to_string(),
            PoolEntry::new(members.iter().map(|m| (*m).to_string()).collect())
                .with_seat_selection(SeatSelection::StickyLeastLoaded),
        );
    }
    let mut config = Config {
        providers,
        pools,
        ..Config::default()
    };
    config.aliases.insert(
        "hot".into(),
        AliasValue::Chain(vec!["opusPool".into(), "sonnetPool".into()]),
    );

    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    for (nickname, pool, wire, members) in [
        ("opusPool", "opus-pool", "claude-opus", OPUS_MEMBERS),
        ("sonnetPool", "sonnet-pool", "claude-sonnet", SONNET_MEMBERS),
    ] {
        let seats = seats_for(&members);
        let default_provider = seats[0].provider.clone();
        let model =
            ResolvedModel::new(nickname, pool, default_provider, wire).with_seats(Arc::from(seats));
        models.insert(nickname.to_string(), Arc::new(model));
    }
    router.install_resolved_models(models);
    router
}

#[tokio::test]
async fn two_sticky_pools_in_one_chain_keep_independent_stable_pins() {
    // Chain `hot = [opusPool, sonnetPool]` over TWO pools, both
    // StickyLeastLoaded, same session "S". Each pool must birth-then-stay on
    // its OWN home seat. With a single session-keyed pin the two pools clobber
    // each other every turn, so the 2nd request would report birth_pick again
    // for both.
    let router = two_pool_sticky_chain_router();

    // Request 1: both pools birth. Chain layout is
    // [opus home, opus seat, sonnet home, sonnet seat]; the sticky token
    // lands on each pool's home (first) target only.
    let first = router
        .dispatch_chain("hot", Some("S"))
        .expect("chain resolves");
    assert_eq!(first.len(), 4, "two two-seat pools expand to four targets");
    assert_eq!(
        first[0].selection_decision,
        Some("birth_pick"),
        "opus pool births on the first request"
    );
    assert_eq!(
        first[2].selection_decision,
        Some("birth_pick"),
        "sonnet pool births on the first request"
    );
    let opus_home = first[0].state_key.clone();
    let sonnet_home = first[2].state_key.clone();
    assert!(
        opus_home.starts_with("opusPool"),
        "opus home is an opus seat, got {opus_home}"
    );
    assert!(
        sonnet_home.starts_with("sonnetPool"),
        "sonnet home is a sonnet seat, got {sonnet_home}"
    );

    // Request 2 for the SAME session: each pool STAYS on its own pinned home
    // (sticky_stay, not a fresh birth) and the homes are unchanged.
    let second = router
        .dispatch_chain("hot", Some("S"))
        .expect("chain resolves");
    assert_eq!(
        second[0].selection_decision,
        Some("sticky_stay"),
        "opus pool must retain its pin, not re-birth"
    );
    assert_eq!(
        second[2].selection_decision,
        Some("sticky_stay"),
        "sonnet pool must retain its pin, not re-birth"
    );
    assert_eq!(
        second[0].state_key, opus_home,
        "opus pool keeps the same home seat across turns"
    );
    assert_eq!(
        second[2].state_key, sonnet_home,
        "sonnet pool keeps the same home seat across turns"
    );

    // The two pools hold DISTINCT, independent pins, keyed by POOL name --
    // and each names a member of its OWN pool, which is what proves the two
    // namespaces never resolved to one slot.
    let opus_pin = router
        .sticky_pins
        .get(&super::chain::sticky_pin_key("S", "opus-pool"))
        .expect("opus pool owns its own namespaced pin");
    let sonnet_pin = router
        .sticky_pins
        .get(&super::chain::sticky_pin_key("S", "sonnet-pool"))
        .expect("sonnet pool owns its own namespaced pin");
    assert!(
        ["anthropic-a", "anthropic-b"].contains(&opus_pin.member.as_str()),
        "opus pool must pin one of its OWN members, got {}",
        opus_pin.member
    );
    assert!(
        ["anthropic-c", "anthropic-d"].contains(&sonnet_pin.member.as_str()),
        "sonnet pool must pin one of its OWN members, got {}",
        sonnet_pin.member
    );
}

/// The D4 counterpart of the independence test above: two models on ONE pool
/// deliberately SHARE a pin, so a session stays on one account across every
/// model of that pool.
///
/// The account -- not the model -- is what holds the warm prompt cache, so a
/// session that moves from `opusShared` to `sonnetShared` must land on the SAME
/// member rather than birthing a second pin and a second cold cache. Keyed
/// per-model this test fails: the second model reads no pin and births.
fn one_pool_two_models_router() -> Router {
    const MEMBERS: [&str; 2] = ["anthropic-a", "anthropic-b"];

    let mut providers = BTreeMap::new();
    for member in MEMBERS {
        providers.insert(
            member.to_string(),
            ProviderEntry::anthropic_api(format!("oauth://{member}")),
        );
    }
    let mut pools = BTreeMap::new();
    pools.insert(
        "shared-pool".to_string(),
        PoolEntry::new(MEMBERS.iter().map(|m| (*m).to_string()).collect())
            .with_seat_selection(SeatSelection::StickyLeastLoaded),
    );
    let config = Config {
        providers,
        pools,
        ..Config::default()
    };

    // ONE shared seat set, exactly as the factory compiles a pool once and
    // hands the same `Arc` to every model naming it.
    let seats: Arc<[SeatTarget]> = Arc::from(
        MEMBERS
            .iter()
            .map(|member| {
                let provider: Arc<dyn Provider> = Arc::new(SeatProvider {
                    id: (*member).to_string(),
                    calls: Arc::new(AtomicUsize::new(0)),
                });
                SeatTarget {
                    provider_name: (*member).to_string(),
                    provider,
                    auth_secret_ref: None,
                }
            })
            .collect::<Vec<_>>(),
    );

    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    for (nickname, wire) in [
        ("opusShared", "claude-opus"),
        ("sonnetShared", "claude-sonnet"),
    ] {
        let model = ResolvedModel::new(nickname, "shared-pool", seats[0].provider.clone(), wire)
            .with_seats(Arc::clone(&seats));
        models.insert(nickname.to_string(), Arc::new(model));
    }
    router.install_resolved_models(models);
    router
}

#[tokio::test]
async fn two_models_on_one_pool_share_one_session_pin() {
    // Arrange: two models, one pool, one shared seat set.
    let router = one_pool_two_models_router();

    // Act: the session births on the first model, then arrives at the SECOND
    // model of the same pool.
    let first = router
        .dispatch_chain("opusShared", Some("S"))
        .expect("chain resolves");
    let second = router
        .dispatch_chain("sonnetShared", Some("S"))
        .expect("chain resolves");

    // Assert: the first model births; the second READS that pin rather than
    // minting its own.
    assert_eq!(
        first[0].selection_decision,
        Some("birth_pick"),
        "the first model of the pool births the session's pin"
    );
    assert_eq!(
        second[0].selection_decision,
        Some("sticky_stay"),
        "the second model of the SAME pool must read the shared pin, not birth"
    );

    // And it is the same ACCOUNT, which is the point: the state_keys differ by
    // model while the member behind them is one.
    let home_member = first[0]
        .state_key
        .strip_prefix("opusShared#")
        .expect("pooled seat key is model-scoped");
    let second_member = second[0]
        .state_key
        .strip_prefix("sonnetShared#")
        .expect("pooled seat key is model-scoped");
    assert_eq!(
        home_member, second_member,
        "the session must stay on ONE account across models of the pool"
    );

    // Exactly one pin exists, under the POOL namespace.
    assert_eq!(
        router.sticky_pins.len(),
        1,
        "one pool holds one pin per session, not one per model"
    );
    assert_eq!(
        router
            .sticky_pins
            .get(&super::chain::sticky_pin_key("S", "shared-pool"))
            .expect("the pin is keyed by pool")
            .member,
        home_member,
        "the shared pin names the account both models served"
    );
}

// ---- D8 pool counters + the empty-pool dispatch refusal ----

#[test]
fn a_healthy_pool_dispatch_counts_once_and_is_not_degraded() {
    // Arrange: the compiled seat count equals the configured member count.
    let (router, _counters) = pooled_router(SeatSelection::FillFirst);

    // Act
    let _ = router.dispatch_chain("opus", None).expect("chain resolves");

    // Assert
    let pool = router.metrics.pool_totals();
    assert_eq!(pool.dispatch, 1, "one pooled model expansion, counted once");
    assert_eq!(
        pool.degraded_dispatch, 0,
        "a full-strength pool must never read as degraded"
    );
    assert_eq!(pool.unavailable, 0);
}

#[test]
fn a_pool_serving_fewer_seats_than_members_counts_a_degraded_dispatch() {
    // The counter's whole job: traffic is concentrating on fewer accounts than
    // the operator configured. Built with the third member declared in the pool
    // but absent from the compiled seat set, which is exactly the shape a
    // build-time omission leaves behind.
    let (mut router, _counters) =
        pooled_router_with_members(SeatSelection::FillFirst, &["anthropic-a", "anthropic-b"]);
    let mut config = (*router.config).clone();
    config.pools.insert(
        "anthropic-pool".to_string(),
        PoolEntry::new(vec![
            "anthropic-a".to_string(),
            "anthropic-b".to_string(),
            "anthropic-c".to_string(),
        ]),
    );
    router.config = Arc::new(config);

    let _ = router.dispatch_chain("opus", None).expect("chain resolves");

    let pool = router.metrics.pool_totals();
    assert_eq!(pool.dispatch, 1);
    assert_eq!(
        pool.degraded_dispatch, 1,
        "two seats against three configured members is a degraded serve"
    );
}

#[test]
fn dispatch_on_an_empty_pool_returns_a_retryable_error_never_unknown_alias() {
    // The defect this pins: an empty seat set produced zero dispatch targets,
    // which surfaced at the end of the dispatch loop as `UnknownAlias` -- a 404
    // naming a route that IS configured, which no client retries and which
    // sends the operator hunting a routing typo for a credential outage.
    let (mut router, _counters) =
        pooled_router_with_members(SeatSelection::FillFirst, &["anthropic-a"]);
    let empty: Vec<SeatTarget> = Vec::new();
    let model = ResolvedModel::new(
        "opus",
        "anthropic-pool",
        router.resolved_models["opus"].provider.clone(),
        "claude-opus-4-7",
    )
    .with_seats(Arc::from(empty));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert("opus".to_string(), Arc::new(model));
    router.install_resolved_models(models);

    let err = match router.dispatch_chain("opus", None) {
        Err(e) => e,
        Ok(targets) => panic!(
            "an empty pool must refuse rather than resolve to {} targets",
            targets.len()
        ),
    };

    assert!(
        !matches!(err, Error::UnknownAlias(_)),
        "an empty pool is never an unknown route: {err:?}"
    );
    let Error::Upstream { status, .. } = err else {
        panic!("expected an upstream-shaped error, got {err:?}");
    };
    assert_eq!(status, 503, "must map to a retryable 5xx");
    // And the class the router derives from it must be BOTH retryable and
    // fallbackable, or the error shape would be cosmetic.
    let classified =
        routectl_core::failure_class::classify(&empty_pool_error("opus"), Some("anthropic-api"));
    let policy = crate::config::RetryPolicy::default();
    let (retry_cap, fallback) = policy.resolved_class(&classified.class);
    assert!(retry_cap > 0, "the class must retry: {classified:?}");
    assert!(fallback, "the class must fall back: {classified:?}");
    assert_eq!(
        router.metrics.pool_totals().unavailable,
        1,
        "the defensive empty-pool path must be observable"
    );
}

// ---- per-pool shared rotation + reload carry-over ----

/// Two RoundRobin models on ONE pool, sharing one compiled seat set -- the
/// rotation counterpart of `one_pool_two_models_router`.
fn one_pool_two_round_robin_models_router() -> Router {
    const MEMBERS: [&str; 2] = ["anthropic-a", "anthropic-b"];

    let mut providers = BTreeMap::new();
    for member in MEMBERS {
        providers.insert(
            member.to_string(),
            ProviderEntry::anthropic_api(format!("oauth://{member}")),
        );
    }
    let mut pools = BTreeMap::new();
    pools.insert(
        "shared-pool".to_string(),
        PoolEntry::new(MEMBERS.iter().map(|m| (*m).to_string()).collect())
            .with_seat_selection(SeatSelection::RoundRobin),
    );
    let config = Config {
        providers,
        pools,
        ..Config::default()
    };

    let seats: Arc<[SeatTarget]> = Arc::from(
        MEMBERS
            .iter()
            .map(|member| {
                let provider: Arc<dyn Provider> = Arc::new(SeatProvider {
                    id: (*member).to_string(),
                    calls: Arc::new(AtomicUsize::new(0)),
                });
                SeatTarget {
                    provider_name: (*member).to_string(),
                    provider,
                    auth_secret_ref: None,
                }
            })
            .collect::<Vec<_>>(),
    );

    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    for (nickname, wire) in [("opusRr", "claude-opus"), ("sonnetRr", "claude-sonnet")] {
        let model = ResolvedModel::new(nickname, "shared-pool", seats[0].provider.clone(), wire)
            .with_seats(Arc::clone(&seats));
        models.insert(nickname.to_string(), Arc::new(model));
    }
    router.install_resolved_models(models);
    router
}

/// The member each model's first dispatch target names, for a chain resolved
/// without a session key.
fn lead_member(router: &Router, alias: &str) -> String {
    router
        .dispatch_chain(alias, None)
        .expect("chain resolves")
        .swap_remove(0)
        .provider_name
}

#[tokio::test]
async fn two_models_on_one_pool_share_one_rotation_cursor() {
    // THE rotation-sharing contract: one cursor per POOL, so two models naming
    // it interleave across the pool's accounts instead of each rotating
    // independently. Keyed per-model both models would read their own cursor
    // and each start at seat 0 -- so both first requests would name
    // anthropic-a, and the shared advance below would not be observable.
    let router = one_pool_two_round_robin_models_router();

    // Act: alternate models, one request each, over a two-member pool.
    let first = lead_member(&router, "opusRr");
    let second = lead_member(&router, "sonnetRr");
    let third = lead_member(&router, "opusRr");
    let fourth = lead_member(&router, "sonnetRr");

    // Assert: the SECOND model's request sees the cursor the FIRST advanced,
    // so consecutive requests never repeat an account while one is free.
    assert_eq!(
        first, "anthropic-a",
        "the first request of a fresh pool leads with the first member"
    );
    assert_eq!(
        second, "anthropic-b",
        "a sibling model on the same pool must observe the shared advance, \
         not restart the rotation at seat 0"
    );
    assert_eq!(third, "anthropic-a", "the shared cursor wraps");
    assert_eq!(fourth, "anthropic-b");
}

#[tokio::test]
async fn a_same_name_pool_keeps_its_rotation_cursor_across_a_reload() {
    // The cursor is carried, not reset: a reload mid-rotation must not send the
    // next request back to seat 0, which would double-serve one account.
    let before = one_pool_two_round_robin_models_router();
    assert_eq!(lead_member(&before, "opusRr"), "anthropic-a");

    // Rebuild the same config and carry over, exactly as the coordinator does.
    let mut after = one_pool_two_round_robin_models_router();
    after.carry_over_pool_state_from(&before);

    // The next request continues the rotation rather than repeating seat 0.
    assert_eq!(
        lead_member(&after, "opusRr"),
        "anthropic-b",
        "a surviving pool's cursor must continue across a reload, not reset"
    );
}

#[tokio::test]
async fn a_renamed_pool_starts_a_fresh_rotation_cursor() {
    // A rename is a NEW pool: no heuristic state transfer. The fresh map simply
    // has no key for the old name, so the cursor starts at seat 0.
    let before = one_pool_two_round_robin_models_router();
    assert_eq!(lead_member(&before, "opusRr"), "anthropic-a");

    // Rebuild with the pool renamed; the models now name `renamed-pool`.
    let mut after = one_pool_two_round_robin_models_router();
    {
        let mut config = (*after.config).clone();
        let pool = config.pools.remove("shared-pool").expect("pool exists");
        config.pools.insert("renamed-pool".to_string(), pool);
        let mut models = BTreeMap::new();
        for (nickname, model) in &after.resolved_models {
            let seats = model.seats.clone().expect("pooled");
            let rebuilt = ResolvedModel::new(
                nickname,
                "renamed-pool",
                model.provider.clone(),
                model.upstream.clone(),
            )
            .with_seats(seats);
            models.insert(nickname.clone(), Arc::new(rebuilt));
        }
        after = Router::new(Arc::new(config));
        after.install_resolved_models(models);
    }
    after.carry_over_pool_state_from(&before);

    assert_eq!(
        lead_member(&after, "opusRr"),
        "anthropic-a",
        "a renamed pool is a new pool: its rotation starts fresh"
    );
}

#[test]
fn cursor_carry_over_builds_a_fresh_map_without_historical_pool_names() {
    // The keyspace bound: carry-over writes into THIS router's fresh map, so a
    // run of reloads over renamed pools cannot accumulate every name the
    // process ever saw.
    let before = one_pool_two_round_robin_models_router();
    assert_eq!(before.round_robin.keys(), vec!["shared-pool"]);

    // A router that declares NO pool carries nothing forward.
    let mut after = Router::new(Arc::new(Config::default()));
    after.carry_over_pool_state_from(&before);

    assert!(
        after.round_robin.keys().is_empty(),
        "a pool absent from the new config must not survive the carry-over"
    );
}

/// A three-member StickyLeastLoaded pool with a session pinned to `pinned`,
/// built over `members`. Returns the router.
fn sticky_pool_router(members: &[&str]) -> Router {
    let mut providers = BTreeMap::new();
    let mut seats: Vec<SeatTarget> = Vec::new();
    for member in members {
        providers.insert(
            (*member).to_string(),
            ProviderEntry::anthropic_api(format!("oauth://{member}")),
        );
        let provider: Arc<dyn Provider> = Arc::new(SeatProvider {
            id: (*member).to_string(),
            calls: Arc::new(AtomicUsize::new(0)),
        });
        seats.push(SeatTarget {
            provider_name: (*member).to_string(),
            provider,
            auth_secret_ref: Some(routectl_auth::SecretRef::OAuth {
                provider: (*member).to_string(),
                label: None,
            }),
        });
    }
    let mut pools = BTreeMap::new();
    pools.insert(
        "shared-pool".to_string(),
        PoolEntry::new(members.iter().map(|m| (*m).to_string()).collect())
            .with_seat_selection(SeatSelection::StickyLeastLoaded),
    );
    let config = Config {
        providers,
        pools,
        ..Config::default()
    };
    let default_provider = seats[0].provider.clone();
    let mut router = Router::new(Arc::new(config));
    let model = ResolvedModel::new("opus", "shared-pool", default_provider, "claude-opus")
        .with_seats(Arc::from(seats));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert("opus".to_string(), Arc::new(model));
    router.install_resolved_models(models);
    router
}

/// Pin `session` to `member` in `router`'s pool-keyed namespace.
fn pin_session_to(router: &Router, session: &str, member: &str) {
    router.sticky_pins.put(
        &super::chain::sticky_pin_key(session, "shared-pool"),
        crate::seat_pool::SeatPin {
            member: member.to_string(),
            repinned: false,
        },
    );
}

/// The member `session` is pinned to in `router`'s pool-keyed namespace.
fn pinned_in_shared_pool(router: &Router, session: &str) -> Option<String> {
    router
        .sticky_pins
        .get(&super::chain::sticky_pin_key(session, "shared-pool"))
        .map(|pin| pin.member)
}

#[test]
fn a_removed_members_pin_repicks_once_onto_a_survivor_and_counts_it() {
    // A member leaving the pool must move ITS pins exactly once, onto a
    // survivor, and the move must be observable -- a burst of these is the
    // operator's only signal that a credential change scattered conversations.
    let before = sticky_pool_router(&["anthropic-a", "anthropic-b", "anthropic-c"]);
    pin_session_to(&before, "doomed", "anthropic-c");

    // Reload drops anthropic-c.
    let mut after = sticky_pool_router(&["anthropic-a", "anthropic-b"]);
    after.carry_over_metrics_from(&before);
    after.carry_over_sticky_from(&before);
    after.carry_over_pool_state_from(&before);

    // Re-picked onto a surviving member, exactly once, and counted once.
    let repicked = pinned_in_shared_pool(&after, "doomed").expect("pin survives, re-pointed");
    assert!(
        ["anthropic-a", "anthropic-b"].contains(&repicked.as_str()),
        "a retired member's pin must move to a survivor, got {repicked}"
    );
    assert_eq!(
        after.metrics.pool_totals().removed_pin_repick,
        1,
        "each moved pin must be counted exactly once"
    );

    // Idempotent: carrying over again moves nothing, because the pin now names
    // a member the pool still serves.
    let mut third = sticky_pool_router(&["anthropic-a", "anthropic-b"]);
    third.carry_over_metrics_from(&after);
    third.carry_over_sticky_from(&after);
    third.carry_over_pool_state_from(&after);
    assert_eq!(
        third.metrics.pool_totals().removed_pin_repick,
        1,
        "a pin already on a survivor must not be re-picked a second time"
    );
}

#[test]
fn a_surviving_members_pin_is_never_touched_by_a_membership_change() {
    // The blast radius: a member leaving costs a cold miss ONLY to the sessions
    // pinned to it. Every other pin -- and its one-time overflow marker -- is
    // left byte-for-byte alone.
    let before = sticky_pool_router(&["anthropic-a", "anthropic-b", "anthropic-c"]);
    pin_session_to(&before, "doomed", "anthropic-c");
    before.sticky_pins.put(
        &super::chain::sticky_pin_key("survivor", "shared-pool"),
        crate::seat_pool::SeatPin {
            member: "anthropic-a".to_string(),
            // Already migrated once: the marker must survive, or the reload
            // would silently re-open this session's flap window.
            repinned: true,
        },
    );

    let mut after = sticky_pool_router(&["anthropic-a", "anthropic-b"]);
    after.carry_over_metrics_from(&before);
    after.carry_over_sticky_from(&before);
    after.carry_over_pool_state_from(&before);

    let kept = after
        .sticky_pins
        .get(&super::chain::sticky_pin_key("survivor", "shared-pool"))
        .expect("a surviving member's pin is never dropped");
    assert_eq!(
        kept.member, "anthropic-a",
        "a surviving member's pin must not move"
    );
    assert!(
        kept.repinned,
        "the one-time overflow marker must survive the reload"
    );
    assert_eq!(
        after.metrics.pool_totals().removed_pin_repick,
        1,
        "only the retired member's pin counts as a re-pick"
    );
}

#[test]
fn a_renamed_pool_leaves_its_pins_to_resolve_as_a_miss_rather_than_repicking() {
    // A rename is a new pool on both sides, so there is no survivor to move a
    // pin to. The old pin is left alone under its old namespace: nothing reads
    // it, so it resolves as a miss and re-picks naturally. Picking a survivor
    // from an unrelated pool would be worse than a cold miss.
    let before = sticky_pool_router(&["anthropic-a", "anthropic-b", "anthropic-c"]);
    pin_session_to(&before, "S", "anthropic-c");

    // The new config's pool has a different NAME, so no join matches.
    let mut after = sticky_pool_router(&["anthropic-a", "anthropic-b"]);
    {
        let mut config = (*after.config).clone();
        let pool = config.pools.remove("shared-pool").expect("pool exists");
        config.pools.insert("renamed-pool".to_string(), pool);
        let model = after.resolved_models.get("opus").expect("model").clone();
        let rebuilt = ResolvedModel::new(
            "opus",
            "renamed-pool",
            model.provider.clone(),
            model.upstream.clone(),
        )
        .with_seats(model.seats.clone().expect("pooled"));
        let mut models = BTreeMap::new();
        models.insert("opus".to_string(), Arc::new(rebuilt));
        after = Router::new(Arc::new(config));
        after.install_resolved_models(models);
    }
    after.carry_over_metrics_from(&before);
    after.carry_over_sticky_from(&before);
    after.carry_over_pool_state_from(&before);

    assert_eq!(
        after.metrics.pool_totals().removed_pin_repick,
        0,
        "a renamed pool transfers no pin state, so nothing is re-picked"
    );
    assert!(
        after
            .sticky_pins
            .get(&super::chain::sticky_pin_key("S", "renamed-pool"))
            .is_none(),
        "the renamed pool starts with no pin for the session"
    );
}

#[test]
fn pool_state_carry_over_re_declares_quota_admission_for_the_new_seat_set() {
    // Quota admission is account-keyed and re-declared by the quota carry-over,
    // not by this one -- but the two run at the same reload site, so the
    // combination must leave the store admitting exactly the NEW seat set. A
    // retired member's write is refused; a surviving member's lands.
    use crate::quota::key::seat_key_for_secret_ref;

    let retired_ref = routectl_auth::SecretRef::OAuth {
        provider: "anthropic-c".to_string(),
        label: None,
    };
    let kept_ref = routectl_auth::SecretRef::OAuth {
        provider: "anthropic-a".to_string(),
        label: None,
    };
    let retired = seat_key_for_secret_ref(Some(&retired_ref)).expect("oauth ref yields a key");
    let kept = seat_key_for_secret_ref(Some(&kept_ref)).expect("oauth ref yields a key");

    let before = sticky_pool_router(&["anthropic-a", "anthropic-b", "anthropic-c"]);
    let mut after = sticky_pool_router(&["anthropic-a", "anthropic-b"]);
    after.carry_over_metrics_from(&before);
    after.carry_over_sticky_from(&before);
    after.carry_over_quota_from(&before);
    after.carry_over_pool_state_from(&before);

    assert!(
        after.quota_store.admits(&kept),
        "a surviving account must still be admitted after the reload"
    );
    assert!(
        !after.quota_store.admits(&retired),
        "an account the new config dropped must no longer be admitted"
    );
}

#[tokio::test]
async fn an_in_flight_request_finishes_on_the_old_router_after_a_pool_reload() {
    // In-flight requests finish on the Arc they started with: the swap replaces
    // the router a NEW request resolves against and never reaches back into one
    // already resolved. Proven by resolving a chain, reloading with the pinned
    // member removed, and asserting the already-resolved chain is unchanged.
    let before = sticky_pool_router(&["anthropic-a", "anthropic-b", "anthropic-c"]);
    let in_flight: Vec<String> = before
        .dispatch_chain("opus", Some("S"))
        .expect("chain resolves")
        .into_iter()
        .map(|t| t.state_key)
        .collect();

    let mut after = sticky_pool_router(&["anthropic-a", "anthropic-b"]);
    after.carry_over_metrics_from(&before);
    after.carry_over_sticky_from(&before);
    after.carry_over_pool_state_from(&before);

    // The chain resolved before the swap still names all three seats.
    assert_eq!(
        in_flight.len(),
        3,
        "the in-flight chain keeps the seat set it resolved against"
    );
    assert!(
        in_flight.iter().any(|k| k.ends_with("anthropic-c")),
        "the retired seat is still dispatchable for the request already holding \
         the old router: {in_flight:?}"
    );
    // While a request resolving against the NEW router sees only survivors.
    let fresh: Vec<String> = after
        .dispatch_chain("opus", Some("S"))
        .expect("chain resolves")
        .into_iter()
        .map(|t| t.state_key)
        .collect();
    assert_eq!(fresh.len(), 2, "the new router serves only survivors");
    assert!(
        !fresh.iter().any(|k| k.ends_with("anthropic-c")),
        "the retired seat must not appear on the new router: {fresh:?}"
    );
}
