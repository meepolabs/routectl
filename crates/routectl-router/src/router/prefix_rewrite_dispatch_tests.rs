//! Dispatch-path wiring of the prefix-rewrite detector.
//!
//! The store itself is unit-tested in `prefix_rewrite.rs`; what is pinned
//! here is the wiring contract: exactly ONE observation per client request
//! (above the chain loop, so retries and fallback hops cannot mint extra
//! epochs), keyed on the inbound session key alone, stamped onto
//! `meta.prefix_epoch_event` on both the completion and streaming paths,
//! WARNing at most once per process and never for a compaction reseed, and
//! carried across a hot-reload rebuild by SHARING the store and the latch.

use super::*;
use crate::config::{ProviderEntry, RetryPolicy};
use async_trait::async_trait;
use routectl_core::schema::{Message, MessageContent, Role};
use routectl_testkit::{CapturedEvent, with_capture};

/// A caller-controlled value distinctive enough that any raw rendering of it
/// is unambiguous in a captured event.
const RAW_SESSION_KEY: &str = "sess-prefix-rewrite-canary-4c71";
/// Marker of the advisory WARN. Matches the emit in `prefix_rewrite.rs`.
const WARN_NEEDLE: &str = "cache_prefix_rewritten_in_epoch";

/// Every entry fails fallbackably, so the chain is always walked to the end
/// and every retry / hop runs. The detector must be unaffected by that.
struct AlwaysFailing {
    id: String,
}

impl AlwaysFailing {
    fn arc(id: &str) -> Arc<dyn Provider> {
        Arc::new(Self { id: id.into() })
    }
}

#[async_trait]
impl Provider for AlwaysFailing {
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
        Err(Error::upstream(&self.id, 503, "unavailable"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream(&self.id, 503, "unavailable"))
    }
}

fn compat_entry() -> ProviderEntry {
    ProviderEntry::openai_compat("https://example.test/v1", "literal:k")
}

/// A router whose `chain` alias walks `entries`, retrying `max_attempts`
/// times per entry -- so one client request drives several dispatch passes.
fn router_over(entries: &[&str], max_attempts: u32) -> Router {
    let mut providers = BTreeMap::new();
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    let mut chain: Vec<String> = Vec::new();
    for name in entries {
        providers.insert((*name).to_string(), compat_entry());
        models.insert(
            (*name).to_string(),
            Arc::new(ResolvedModel::new(
                *name,
                *name,
                AlwaysFailing::arc(name),
                "wire-model",
            )),
        );
        chain.push((*name).to_string());
    }
    let config = Config {
        retry: RetryPolicy {
            max_attempts,
            initial_backoff_ms: 1,
            backoff_multiplier: 1.0,
            jitter_ms: 0,
            ..RetryPolicy::default()
        },
        providers,
        aliases: {
            let mut a = BTreeMap::new();
            a.insert("chain".to_string(), AliasValue::Chain(chain));
            a
        },
        ..Config::default()
    };
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

fn text_msg(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// A request carrying `texts` as its history and the caller-controlled
/// session key the detector keys on.
fn keyed_req(texts: &[&str]) -> ChatRequest {
    let mut req = ChatRequest {
        model: "chain".into(),
        messages: texts.iter().map(|t| text_msg(t)).collect(),
        ..Default::default()
    };
    req.routectl_internal.inbound_session_key = Some(RAW_SESSION_KEY.into());
    req
}

/// The same request shape with NO session key.
fn unkeyed_req(texts: &[&str]) -> ChatRequest {
    ChatRequest {
        model: "chain".into(),
        messages: texts.iter().map(|t| text_msg(t)).collect(),
        ..Default::default()
    }
}

fn rewrite_warns(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN && e.message.contains(WARN_NEEDLE))
        .collect()
}

/// Drive one completion request and return its meta plus the captured events.
async fn complete_once(router: &Router, req: ChatRequest) -> (DispatchMeta, Vec<CapturedEvent>) {
    let (dispatched, events) =
        with_capture(router.complete_with_options(req, RouterOptions::new())).await;
    (dispatched.meta, events)
}

#[tokio::test]
async fn first_request_for_a_session_records_a_baseline_without_classifying() {
    // Arrange
    let router = router_over(&["m1"], 1);

    // Act
    let (meta, events) = complete_once(&router, keyed_req(&["a", "b", "c"])).await;

    // Assert: a first-seen session has nothing to compare against, so no
    // event code and no WARN -- the bounded false negative a restart accepts.
    assert_eq!(meta.prefix_epoch_event, None);
    assert!(rewrite_warns(&events).is_empty());
    assert_eq!(router.prefix_epoch_store.len(), 1);
}

#[tokio::test]
async fn a_pure_append_turn_stamps_stable() {
    // Arrange
    let router = router_over(&["m1"], 1);
    complete_once(&router, keyed_req(&["a", "b"])).await;

    // Act
    let (meta, events) = complete_once(&router, keyed_req(&["a", "b", "c"])).await;

    // Assert
    assert_eq!(meta.prefix_epoch_event, Some(0));
    assert!(rewrite_warns(&events).is_empty());
}

#[tokio::test]
async fn the_detector_observes_once_per_client_request_not_once_per_attempt() {
    // Arrange: two entries, two attempts each -- four dispatch passes per
    // client request. An observation inside the chain loop would advance the
    // epoch on every pass.
    let router = router_over(&["m1", "m2"], 2);
    let (baseline, _) = complete_once(&router, keyed_req(&["a", "b", "c"])).await;
    assert_eq!(baseline.attempt_count, 4, "the whole chain must be walked");

    // Act: one rewriting request across the same four-pass walk.
    let (meta, events) = complete_once(&router, keyed_req(&["A", "b", "c"])).await;

    // Assert: exactly one rewrite was observed for the request, so the epoch
    // advanced by one and the next identical turn reads Stable against the
    // reseeded baseline.
    assert_eq!(meta.prefix_epoch_event, Some(1));
    assert_eq!(meta.attempt_count, 4);
    assert_eq!(
        rewrite_warns(&events).len(),
        1,
        "retries and fallback hops must not each emit an observation",
    );
    let (after, _) = complete_once(&router, keyed_req(&["A", "b", "c", "d"])).await;
    assert_eq!(
        after.prefix_epoch_event,
        Some(0),
        "a single reseed happened, not one per attempt",
    );
}

#[tokio::test]
async fn a_request_without_a_session_key_records_no_state_and_no_warn() {
    // Arrange
    let router = router_over(&["m1"], 1);

    // Act: two turns whose prefix visibly changes between them.
    let (first, first_events) = complete_once(&router, unkeyed_req(&["a", "b", "c"])).await;
    let (second, second_events) = complete_once(&router, unkeyed_req(&["A", "b", "c"])).await;

    // Assert
    assert_eq!(first.prefix_epoch_event, None);
    assert_eq!(second.prefix_epoch_event, None);
    assert!(router.prefix_epoch_store.is_empty());
    assert!(rewrite_warns(&first_events).is_empty());
    assert!(rewrite_warns(&second_events).is_empty());
}

#[tokio::test]
async fn repeated_rewrites_warn_exactly_once_per_process_but_keep_stamping_the_event() {
    // Arrange
    let router = router_over(&["m1"], 1);
    complete_once(&router, keyed_req(&["a", "b", "c"])).await;

    // Act
    let (first, first_events) = complete_once(&router, keyed_req(&["A", "b", "c"])).await;
    let (second, second_events) = complete_once(&router, keyed_req(&["B", "b", "c"])).await;

    // Assert: the WARN is edge-triggered per process; the event code carries
    // the unsuppressed volume.
    assert_eq!(rewrite_warns(&first_events).len(), 1);
    assert!(
        rewrite_warns(&second_events).is_empty(),
        "a second rewrite must not re-WARN in the same process",
    );
    assert_eq!(first.prefix_epoch_event, Some(1));
    assert_eq!(
        second.prefix_epoch_event,
        Some(1),
        "every rewritten turn stamps the code even when the WARN is suppressed",
    );
}

#[tokio::test]
async fn a_shortening_turn_reseeds_with_no_warn() {
    // Arrange: a summary-replaces-history compaction -- shorter AND different
    // bytes, which must never read as a client rewrite.
    let router = router_over(&["m1"], 1);
    complete_once(&router, keyed_req(&["a", "b", "c", "d", "e"])).await;

    // Act
    let (meta, events) = complete_once(&router, keyed_req(&["summary", "e"])).await;

    // Assert
    assert_eq!(
        meta.prefix_epoch_event,
        Some(2),
        "the reseed is still recorded for the ledger",
    );
    assert!(
        rewrite_warns(&events).is_empty(),
        "compaction must never produce a false-positive WARN",
    );
}

#[tokio::test]
async fn the_streaming_path_stamps_the_event_too() {
    // Arrange
    let router = router_over(&["m1"], 1);
    let (primed, _) =
        with_capture(router.stream_with_options(keyed_req(&["a", "b", "c"]), RouterOptions::new()))
            .await;
    assert_eq!(primed.meta.prefix_epoch_event, None);

    // Act
    let (dispatched, events) =
        with_capture(router.stream_with_options(keyed_req(&["A", "b", "c"]), RouterOptions::new()))
            .await;

    // Assert
    assert_eq!(dispatched.meta.prefix_epoch_event, Some(1));
    assert_eq!(rewrite_warns(&events).len(), 1);
}

#[tokio::test]
async fn the_warn_carries_only_a_hashed_session_key_lengths_and_the_epoch() {
    // Arrange
    let router = router_over(&["m1"], 1);
    complete_once(&router, keyed_req(&["a", "b", "c"])).await;

    // Act
    let (_, events) = complete_once(&router, keyed_req(&["A", "b", "c"])).await;

    // Assert: the payload identifies the session only through the salted
    // hash, and no captured event renders the raw key anywhere.
    let warns = rewrite_warns(&events);
    let warn = warns
        .first()
        .unwrap_or_else(|| panic!("expected a rewrite WARN, got events: {events:?}"));
    assert_eq!(
        warn.field("session_key_hash"),
        Some(
            crate::log_hash::salted_log_hash(RAW_SESSION_KEY)
                .to_string()
                .as_str()
        ),
    );
    assert_ne!(
        warn.field("session_key_hash"),
        Some(
            crate::context_trim::fnv1a_hash(RAW_SESSION_KEY.as_bytes())
                .to_string()
                .as_str()
        ),
        "the logged hash must be salted, not the invertible fingerprint hash",
    );
    assert_eq!(warn.field("previous_prefix_len"), Some("2"));
    assert_eq!(warn.field("prefix_len"), Some("2"));
    assert_eq!(warn.field("epoch"), Some("1"));
    let field_names: Vec<&str> = warn.fields.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        field_names,
        vec![
            "session_key_hash",
            "previous_prefix_len",
            "prefix_len",
            "epoch",
        ],
        "no prompt bytes and no content fingerprint may ride the WARN",
    );
    for event in &events {
        assert!(!event.message.contains(RAW_SESSION_KEY));
        for (_, value) in &event.fields {
            assert!(
                !value.contains(RAW_SESSION_KEY),
                "raw session key must never appear in a structured field: {event:?}",
            );
        }
    }
}

#[tokio::test]
async fn a_fresh_router_starts_from_a_first_seen_baseline() {
    // A process restart (or a rebuild with no carry-over) must degrade to a
    // false NEGATIVE -- an unclassified turn -- never to a false positive.
    // Arrange
    let before = router_over(&["m1"], 1);
    complete_once(&before, keyed_req(&["a", "b", "c"])).await;

    // Act
    let after = router_over(&["m1"], 1);
    let (meta, events) = complete_once(&after, keyed_req(&["A", "b", "c"])).await;

    // Assert
    assert_eq!(meta.prefix_epoch_event, None);
    assert!(rewrite_warns(&events).is_empty());
}

#[tokio::test]
async fn the_store_survives_a_hot_reload_via_the_carry_over_chain() {
    // Arrange: build a baseline (and an epoch) on the outgoing router.
    let before = router_over(&["m1"], 1);
    complete_once(&before, keyed_req(&["a", "b", "c"])).await;
    let (rewritten, _) = complete_once(&before, keyed_req(&["A", "b", "c"])).await;
    assert_eq!(rewritten.prefix_epoch_event, Some(1));
    let mut after = router_over(&["m1"], 1);

    // Act
    after.carry_over_prefix_epochs_from(&before);
    let (stable, _) = complete_once(&after, keyed_req(&["A", "b", "c", "d"])).await;
    let (again, _) = complete_once(&after, keyed_req(&["Z", "b", "c", "d"])).await;

    // Assert: the carried baseline classifies the very next turn (no
    // first-seen gap), and the carried epoch counter keeps counting.
    assert_eq!(stable.prefix_epoch_event, Some(0));
    assert_eq!(again.prefix_epoch_event, Some(1));
    assert_eq!(after.prefix_epoch_store.len(), 1);
}

#[tokio::test]
async fn the_carry_over_shares_the_store_rather_than_snapshotting_it() {
    // A snapshotting carry-over loses any observation that lands between the
    // snapshot and the swap -- the request already dispatching against the
    // outgoing Router. Sharing the Arc makes the window unobservable.
    // Arrange
    let before = router_over(&["m1"], 1);
    complete_once(&before, keyed_req(&["a", "b"])).await;
    let mut after = router_over(&["m1"], 1);

    // Act: carry over, THEN a late observation through the outgoing Router --
    // the order a request in flight across a reload takes.
    after.carry_over_prefix_epochs_from(&before);
    complete_once(&before, keyed_req(&["a", "b", "c"])).await;

    // Assert: the incoming Router sees the late baseline, so the next turn
    // classifies against `["a", "b", "c"]` rather than the pre-swap snapshot.
    let (stable, _) = complete_once(&after, keyed_req(&["a", "b", "c", "d"])).await;
    assert_eq!(
        stable.prefix_epoch_event,
        Some(0),
        "an observation racing the swap must not be lost",
    );
    assert_eq!(after.prefix_epoch_store.len(), 1);
}

#[tokio::test]
async fn the_warn_stays_once_per_process_across_a_hot_reload() {
    // The latch rides the same carry-over as the store: a per-Router latch
    // would re-warn on every config reload, so the line's frequency would
    // track reloads rather than rewrites.
    // Arrange: one rewrite on the outgoing Router burns the latch.
    let before = router_over(&["m1"], 1);
    complete_once(&before, keyed_req(&["a", "b", "c"])).await;
    let (rewritten, before_events) = complete_once(&before, keyed_req(&["A", "b", "c"])).await;
    assert_eq!(rewritten.prefix_epoch_event, Some(1));
    assert_eq!(rewrite_warns(&before_events).len(), 1);
    let mut after = router_over(&["m1"], 1);

    // Act
    after.carry_over_prefix_epochs_from(&before);
    let (again, after_events) = complete_once(&after, keyed_req(&["B", "b", "c"])).await;

    // Assert: the event code still records the rewrite, the WARN does not
    // repeat.
    assert_eq!(again.prefix_epoch_event, Some(1));
    assert!(
        rewrite_warns(&after_events).is_empty(),
        "a reload must not re-arm the once-per-process WARN",
    );
}

#[tokio::test]
async fn the_carry_over_preserves_lru_recency_order() {
    // A carry-over that rebuilt the map would keep every entry but race the
    // eviction frontier across the rebuild. Sharing the store makes the
    // ordering identical by construction, which this pins.
    // Arrange
    let before = router_over(&["m1"], 1);
    let store = &before.prefix_epoch_store;
    let req = keyed_req(&["a", "b"]);
    store.observe("A", &req);
    store.observe("B", &req);
    store.observe("C", &req);
    store.observe("A", &req);
    let mut after = router_over(&["m1"], 1);

    // Act
    after.carry_over_prefix_epochs_from(&before);

    // Assert
    assert_eq!(
        after.prefix_epoch_store.keys_lru_first(),
        vec!["B".to_string(), "C".to_string(), "A".to_string()]
    );
}
