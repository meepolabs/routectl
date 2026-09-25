//! `OpeningMeter` against a real Router: admission keys and bounds, the
//! head and served lanes both carry the Router's publication generation,
//! the fast-path opener decision, and which settlements publish.

use std::collections::BTreeMap;
use std::sync::Arc;

use routectl_core::schema::{ChunkChoice, ChunkDelta, UsageDelta};
use routectl_core::test_utils::{assistant_text_msg, user_msg};
use routectl_core::{
    ChatChunk, ChatRequest, Message, OpeningUsage, OpeningUsageOrigin, UpstreamMeta,
    UsageInputSource,
};
use routectl_router::{
    AliasValue, Config, DispatchMeta, ModelEntry, ProviderEntry, Router, estimate_meter_tokens,
};

use super::{OpeningMeter, OpeningOrigin, WireOpening};
use crate::ingress::anthropic::AnthropicIngress;
use crate::ingress::anthropic::context_anchor::{
    AnchorKey, ContextAnchorStore, MAX_ANCHOR_MODEL_BYTES, MissReason, OpeningReason,
    OpeningSelection, OpeningSource, SettleOutcome,
};
use crate::ingress::{IngressAdapter, StreamRequestContext};

const SESSION: &str = "meter-session";
const ALIAS: &str = "claude-opus";
const NICKNAME: &str = "glm";
const UPSTREAM: &str = "glm-4.6";
const PRIOR_ACTUAL: u64 = 40_000;

/// A Router built through the real bootstrap, so its route table resolves.
/// These tests never dispatch; the provider points at a test-owned
/// loopback listener so no request could reach another service.
async fn router() -> Router {
    let upstream = crate::handlers::ingress_handle::opening_rig::Upstream::start().await;
    let mut providers = BTreeMap::new();
    providers.insert(
        "compat".to_string(),
        ProviderEntry::openai_compat(
            format!("{}/v1", upstream.base()),
            crate::test_secret::file_ref("k"),
        ),
    );
    let mut models = BTreeMap::new();
    models.insert(NICKNAME.to_string(), ModelEntry::new("compat", UPSTREAM));
    let mut aliases = BTreeMap::new();
    aliases.insert(ALIAS.to_string(), AliasValue::Single(NICKNAME.to_string()));
    let config = Arc::new(Config {
        providers,
        models,
        aliases,
        ..Default::default()
    });
    let secrets: Arc<dyn routectl_auth::SecretStore> = Arc::new(routectl_auth::MemoryStore::new());
    crate::server::build_router_from_config(config, secrets)
        .await
        .expect("router builds")
}

fn served_meta() -> DispatchMeta {
    let mut meta = DispatchMeta::for_alias(ALIAS);
    meta.served_provider_kind = Some("openai-compat".into());
    meta.served_model = Some(NICKNAME.into());
    meta.served_upstream = Some(UPSTREAM.into());
    meta.served_seat = Some("seat-a".into());
    meta
}

fn history() -> Vec<Message> {
    vec![
        user_msg("Summarize the layout."),
        assistant_text_msg("One crate."),
    ]
}

fn grown() -> Vec<Message> {
    let mut messages = history();
    messages.push(user_msg("And its dependencies?"));
    messages
}

fn request(session: Option<&str>, messages: Vec<Message>) -> ChatRequest {
    let mut req = ChatRequest {
        model: ALIAS.into(),
        messages: messages.into(),
        stream: Some(true),
        ..Default::default()
    };
    req.routectl_internal.inbound_session_key = session.map(str::to_string);
    req
}

fn opener_chunk() -> ChatChunk {
    let opening = OpeningUsage::new(
        OpeningUsageOrigin::AnthropicMessages,
        std::time::Instant::now(),
        7,
    );
    ChatChunk {
        upstream_meta: Some(routectl_core::UpstreamMeta::from_opening_usage(opening)),
        ..Default::default()
    }
}

fn explicit_final() -> Option<UpstreamMeta> {
    Some(UpstreamMeta::from_usage_input_source(
        UsageInputSource::ExplicitFinal,
    ))
}

/// A usage-only chunk reporting `prompt`, marked as the upstream's own
/// closing report.
fn usage_chunk(prompt: u32) -> ChatChunk {
    ChatChunk {
        usage: Some(UsageDelta {
            prompt_tokens: Some(prompt),
            ..Default::default()
        }),
        upstream_meta: explicit_final(),
        ..Default::default()
    }
}

/// A finish chunk, reporting `usage` as the upstream's own closing report
/// when given.
fn finish_chunk(usage: Option<u32>) -> ChatChunk {
    ChatChunk {
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta::default(),
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        upstream_meta: usage.and(explicit_final()),
        usage: usage.map(|prompt| UsageDelta {
            prompt_tokens: Some(prompt),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Drive `chunks` through the real Anthropic renderer and the meter, the
/// way the stream handler does.
fn render_through(meter: &mut OpeningMeter, chunks: Vec<ChatChunk>) {
    let adapter = AnthropicIngress;
    let mut state = adapter.new_stream_state(&StreamRequestContext::default());
    for chunk in chunks {
        let candidate = OpeningMeter::terminal_candidate(&chunk);
        let events = adapter
            .render_chunk(chunk, state.as_mut())
            .expect("renders");
        meter.accept_rendered(candidate, &events);
    }
}

/// Settle `meter` after a terminal chunk reporting `terminal` (or none).
fn settle_with(mut meter: OpeningMeter, terminal: Option<u64>) -> SettleOutcome {
    let terminal = terminal.map(|t| u32::try_from(t).expect("fits"));
    render_through(&mut meter, vec![finish_chunk(terminal)]);
    meter.settle_completed()
}

/// Complete one turn on the served lane, publishing `actual` as its input.
fn complete_turn(store: &ContextAnchorStore, router: &Router, req: &ChatRequest, actual: u64) {
    let mut meter = OpeningMeter::admit(store, router, req);
    meter.served_opening(&served_meta(), router);
    assert_eq!(settle_with(meter, Some(actual)), SettleOutcome::Published);
}

fn selection_of(meter: &OpeningMeter) -> OpeningSelection {
    match meter.opening().expect("an opening was chosen") {
        OpeningOrigin::Selected(selection) => selection,
        OpeningOrigin::UpstreamWire(_) => panic!("expected a selected opening"),
    }
}

#[tokio::test]
async fn an_unkeyed_request_reserves_no_turn_and_opens_on_the_raw_estimate() {
    // Arrange
    let store = ContextAnchorStore::new();
    let router = router().await;
    let req = request(None, history());

    // Act
    let mut meter = OpeningMeter::admit(&store, &router, &req);
    let tokens = meter.served_opening(&served_meta(), &router);

    // Assert
    assert_eq!(tokens, estimate_meter_tokens(&req));
    assert_eq!(selection_of(&meter).reason, OpeningReason::Unanchored);
    assert_eq!(settle_with(meter, Some(10)), SettleOutcome::NotPublished);
    assert!(store.is_empty(), "an unkeyed turn never publishes");
}

#[tokio::test]
async fn an_overlong_requested_model_is_not_anchored() {
    // Arrange: premise -- the key constructor refuses this model.
    let store = ContextAnchorStore::new();
    let router = router().await;
    let mut req = request(Some(SESSION), history());
    req.model = "m".repeat(MAX_ANCHOR_MODEL_BYTES + 1);
    assert!(AnchorKey::new(SESSION, &req.model).is_none());

    // Act
    let mut meter = OpeningMeter::admit(&store, &router, &req);
    meter.served_opening(&served_meta(), &router);

    // Assert
    assert_eq!(selection_of(&meter).reason, OpeningReason::Unanchored);
    assert_eq!(settle_with(meter, Some(10)), SettleOutcome::NotPublished);
    assert!(store.is_empty());
}

#[tokio::test]
async fn a_second_turn_opens_on_the_anchor_against_the_served_lane() {
    // Arrange
    let store = ContextAnchorStore::new();
    let router = router().await;
    complete_turn(
        &store,
        &router,
        &request(Some(SESSION), history()),
        PRIOR_ACTUAL,
    );

    // Act
    let mut meter = OpeningMeter::admit(&store, &router, &request(Some(SESSION), grown()));
    let tokens = meter.served_opening(&served_meta(), &router);

    // Assert
    let selection = selection_of(&meter);
    assert_eq!(selection.source, OpeningSource::Anchor);
    assert!(!selection.provisional);
    assert!(
        tokens > PRIOR_ACTUAL,
        "the anchor grows with the appended turn: {tokens}"
    );
}

#[tokio::test]
async fn the_head_lane_carries_the_same_generation_as_the_served_lane() {
    // Arrange: an anchor published under the served lane of this Router.
    let store = ContextAnchorStore::new();
    let router = router().await;
    complete_turn(
        &store,
        &router,
        &request(Some(SESSION), history()),
        PRIOR_ACTUAL,
    );

    // Act: the warm path selects against the route head of the same Router.
    let mut meter = OpeningMeter::admit(&store, &router, &request(Some(SESSION), grown()));
    meter.provisional_opening(&router);

    // Assert: the head lane matches the served one, generation included.
    let selection = selection_of(&meter);
    assert_eq!(selection.source, OpeningSource::Anchor);
    assert!(selection.provisional);
}

#[tokio::test]
async fn a_republished_router_does_not_reuse_an_anchor_from_the_old_generation() {
    // Arrange: publish once so the next Router draws a strictly newer value.
    let store = ContextAnchorStore::new();
    let old = router().await;
    old.publish_probe_incarnation();
    complete_turn(
        &store,
        &old,
        &request(Some(SESSION), history()),
        PRIOR_ACTUAL,
    );
    let mut new = router().await;
    new.carry_over_learned_from(&old);
    new.publish_probe_incarnation();
    assert!(new.publication_generation() > old.publication_generation());

    // Act
    let mut meter = OpeningMeter::admit(&store, &new, &request(Some(SESSION), grown()));
    meter.served_opening(&served_meta(), &new);

    // Assert
    assert_eq!(
        selection_of(&meter).reason,
        OpeningReason::AnchorMiss(MissReason::GenerationChanged)
    );
}

#[tokio::test]
async fn a_fast_opener_chunk_marks_the_opening_as_upstream_wire() {
    // Arrange
    let store = ContextAnchorStore::new();
    let router = router().await;
    let mut meter = OpeningMeter::admit(&store, &router, &request(Some(SESSION), history()));
    meter.served_opening(&served_meta(), &router);

    // Act
    meter.observe_opening_chunk(&opener_chunk());
    meter.observe_opening_chunk(&ChatChunk::default());

    // Assert: decided by the first chunk only.
    let origin = meter.opening().expect("decided");
    assert_eq!(
        origin,
        OpeningOrigin::UpstreamWire(WireOpening {
            origin: OpeningUsageOrigin::AnthropicMessages,
            from_vendor_endpoint: false,
        })
    );
    assert_eq!(origin.source_label(), "upstream_wire_unverified");
}

#[tokio::test]
async fn a_vendor_endpoint_opener_is_labelled_distinctly_from_a_proxy_one() {
    let store = ContextAnchorStore::new();
    let router = router().await;
    let mut meter = OpeningMeter::admit(&store, &router, &request(Some(SESSION), history()));
    meter.served_opening(&served_meta(), &router);
    let mut vendor = OpeningUsage::new(
        OpeningUsageOrigin::BedrockInvoke,
        std::time::Instant::now(),
        7,
    );
    vendor.from_vendor_endpoint = true;

    meter.observe_opening_chunk(&ChatChunk {
        upstream_meta: Some(UpstreamMeta::from_opening_usage(vendor)),
        ..Default::default()
    });

    let origin = meter.opening().expect("decided");
    assert_eq!(
        origin,
        OpeningOrigin::UpstreamWire(WireOpening {
            origin: OpeningUsageOrigin::BedrockInvoke,
            from_vendor_endpoint: true,
        })
    );
    assert_eq!(origin.source_label(), "upstream_wire");
}

#[tokio::test]
async fn a_provisional_opening_is_not_replaced_by_a_later_opener() {
    // Arrange
    let store = ContextAnchorStore::new();
    let router = router().await;
    let mut meter = OpeningMeter::admit(&store, &router, &request(Some(SESSION), history()));
    meter.provisional_opening(&router);

    // Act
    meter.observe_opening_chunk(&opener_chunk());

    // Assert
    assert!(matches!(
        meter.opening(),
        Some(OpeningOrigin::Selected(selection)) if selection.provisional
    ));
}

#[tokio::test]
async fn a_completion_without_reported_input_publishes_nothing() {
    let store = ContextAnchorStore::new();
    let router = router().await;
    let mut meter = OpeningMeter::admit(&store, &router, &request(Some(SESSION), history()));
    meter.served_opening(&served_meta(), &router);

    assert_eq!(settle_with(meter, None), SettleOutcome::NotPublished);
    assert!(store.is_empty());
}

#[tokio::test]
async fn a_turn_with_no_served_lane_publishes_nothing() {
    let store = ContextAnchorStore::new();
    let router = router().await;
    let meter = OpeningMeter::admit(&store, &router, &request(Some(SESSION), history()));

    assert_eq!(settle_with(meter, Some(10)), SettleOutcome::NotPublished);
    assert!(store.is_empty());
}

#[tokio::test]
async fn a_dropped_turn_publishes_nothing() {
    let store = ContextAnchorStore::new();
    let router = router().await;
    let mut meter = OpeningMeter::admit(&store, &router, &request(Some(SESSION), history()));
    meter.served_opening(&served_meta(), &router);

    drop(meter);

    assert!(store.is_empty());
}

#[tokio::test]
async fn an_older_overlapping_turn_settling_last_does_not_overwrite_the_newer() {
    // Arrange: both turns admitted before either settles; the older one
    // reserved first.
    let store = ContextAnchorStore::new();
    let router = router().await;
    let mut older = OpeningMeter::admit(&store, &router, &request(Some(SESSION), history()));
    let mut newer = OpeningMeter::admit(&store, &router, &request(Some(SESSION), grown()));
    older.served_opening(&served_meta(), &router);
    newer.served_opening(&served_meta(), &router);

    // Act
    let newer_settled = settle_with(newer, Some(PRIOR_ACTUAL));
    let older_settled = settle_with(older, Some(1));

    // Assert
    assert_eq!(newer_settled, SettleOutcome::Published);
    assert_eq!(older_settled, SettleOutcome::Superseded);
    let key = AnchorKey::new(SESSION, ALIAS).expect("key within bounds");
    assert_eq!(
        store.get(&key).expect("record held").actual_input(),
        PRIOR_ACTUAL
    );
}

#[tokio::test]
async fn interim_usage_before_the_finish_is_not_terminal_evidence() {
    // Arrange: usage-only chunk, then a finish chunk with no usage.
    let store = ContextAnchorStore::new();
    let router = router().await;
    let mut meter = OpeningMeter::admit(&store, &router, &request(Some(SESSION), history()));
    meter.served_opening(&served_meta(), &router);

    // Act
    render_through(&mut meter, vec![usage_chunk(5_000), finish_chunk(None)]);
    let settled = meter.settle_completed();

    // Assert
    assert_eq!(settled, SettleOutcome::NotPublished);
    assert!(store.is_empty());
}

#[tokio::test]
async fn a_usage_only_chunk_after_the_finish_is_terminal() {
    // Arrange: interim usage, finish, then the trailing usage-only chunk.
    let store = ContextAnchorStore::new();
    let router = router().await;
    let mut meter = OpeningMeter::admit(&store, &router, &request(Some(SESSION), history()));
    meter.served_opening(&served_meta(), &router);

    // Act
    render_through(
        &mut meter,
        vec![usage_chunk(5_000), finish_chunk(None), usage_chunk(7_000)],
    );
    let settled = meter.settle_completed();

    // Assert
    assert_eq!(settled, SettleOutcome::Published);
    let key = AnchorKey::new(SESSION, ALIAS).expect("key within bounds");
    assert_eq!(store.get(&key).expect("record").actual_input(), 7_000);
}

#[tokio::test]
async fn usage_on_the_finish_chunk_is_terminal() {
    let store = ContextAnchorStore::new();
    let router = router().await;
    let mut meter = OpeningMeter::admit(&store, &router, &request(Some(SESSION), history()));
    meter.served_opening(&served_meta(), &router);

    render_through(
        &mut meter,
        vec![usage_chunk(5_000), finish_chunk(Some(6_000))],
    );

    assert_eq!(meter.settle_completed(), SettleOutcome::Published);
    let key = AnchorKey::new(SESSION, ALIAS).expect("key within bounds");
    assert_eq!(store.get(&key).expect("record").actual_input(), 6_000);
}

#[tokio::test]
async fn the_raw_opening_is_the_display_estimate_from_one_pass() {
    let store = ContextAnchorStore::new();
    let router = router().await;
    let req = request(None, history());

    let meter = OpeningMeter::admit(&store, &router, &req);

    assert_eq!(meter.raw_tokens(), estimate_meter_tokens(&req));
}

// ------------------------------------------------ provenance and the stop

/// A finish chunk whose usage reports `prompt` from `source`.
fn finish_from(prompt: u32, source: Option<UsageInputSource>) -> ChatChunk {
    ChatChunk {
        upstream_meta: source.map(UpstreamMeta::from_usage_input_source),
        ..finish_chunk(Some(prompt))
    }
}

async fn settled_after(chunks: Vec<ChatChunk>) -> (SettleOutcome, Option<u64>) {
    let store = ContextAnchorStore::new();
    let router = router().await;
    let mut meter = OpeningMeter::admit(&store, &router, &request(Some(SESSION), history()));
    meter.served_opening(&served_meta(), &router);
    render_through(&mut meter, chunks);
    let settled = meter.settle_completed();
    let key = AnchorKey::new(SESSION, ALIAS).expect("key within bounds");
    (settled, store.get(&key).map(|r| r.actual_input()))
}

#[tokio::test]
async fn only_terminal_evidence_sources_publish() {
    for (source, publishes) in [
        (Some(UsageInputSource::ExplicitFinal), true),
        (Some(UsageInputSource::VendorOpening), true),
        (Some(UsageInputSource::ProxyOpening), false),
        (Some(UsageInputSource::InterimCarry), false),
        (None, false),
    ] {
        let (settled, held) = settled_after(vec![finish_from(9_000, source)]).await;

        assert_eq!(
            settled == SettleOutcome::Published,
            publishes,
            "source {source:?}"
        );
        assert_eq!(held.is_some(), publishes, "source {source:?}");
    }
}

#[tokio::test]
async fn only_the_first_usage_chunk_after_the_finish_is_accepted() {
    // Arrange: positive control -- the renderer stops the message on the
    // first usage chunk after a bare finish, so anything later is dropped.
    let adapter = AnthropicIngress;
    let mut state = adapter.new_stream_state(&StreamRequestContext::default());
    adapter
        .render_chunk(finish_chunk(None), state.as_mut())
        .expect("renders");
    let events = adapter
        .render_chunk(usage_chunk(9_000), state.as_mut())
        .expect("renders");
    assert!(
        events
            .iter()
            .any(|e| e.event.as_deref() == Some("message_stop"))
    );

    // Act
    let (settled, held) = settled_after(vec![
        finish_chunk(None),
        usage_chunk(9_000),
        usage_chunk(4_000),
    ])
    .await;

    // Assert
    assert_eq!(settled, SettleOutcome::Published);
    assert_eq!(held, Some(9_000));
}

#[tokio::test]
async fn a_straggler_after_an_inline_terminal_cannot_overwrite_it() {
    // Arrange: positive control -- the inline terminal renders message_stop.
    let adapter = AnthropicIngress;
    let mut state = adapter.new_stream_state(&StreamRequestContext::default());
    let events = adapter
        .render_chunk(finish_chunk(Some(9_000)), state.as_mut())
        .expect("renders");
    assert!(
        events
            .iter()
            .any(|e| e.event.as_deref() == Some("message_stop"))
    );

    // Act
    let (settled, held) = settled_after(vec![finish_chunk(Some(9_000)), usage_chunk(1_234)]).await;

    // Assert
    assert_eq!(settled, SettleOutcome::Published);
    assert_eq!(held, Some(9_000));
}

#[tokio::test]
async fn a_straggler_after_a_terminal_without_input_cannot_create_an_anchor() {
    // A finish with output-only usage stops the message; usage after it
    // was never rendered.
    let output_only = ChatChunk {
        usage: Some(UsageDelta {
            completion_tokens: Some(3),
            ..Default::default()
        }),
        ..finish_chunk(None)
    };

    let (settled, held) = settled_after(vec![output_only, usage_chunk(1_234)]).await;

    assert_eq!(settled, SettleOutcome::NotPublished);
    assert_eq!(held, None);
}

/// A finish chunk with two choices where only `finishing` carries the
/// finish reason.
fn two_choice_finish(finishing: u32) -> ChatChunk {
    let choice = |index: u32| ChunkChoice {
        index,
        delta: ChunkDelta::default(),
        finish_reason: (index == finishing).then(|| "stop".to_string()),
        matched_stop_sequence: None,
    };
    ChatChunk {
        choices: vec![choice(0), choice(1)],
        ..Default::default()
    }
}

#[tokio::test]
async fn a_non_first_choice_finish_does_not_open_the_terminal_window() {
    // Arrange: positive control -- the renderer ignores the second
    // choice's finish, so it emits no message_delta for it.
    let adapter = AnthropicIngress;
    let mut state = adapter.new_stream_state(&StreamRequestContext::default());
    let events = adapter
        .render_chunk(two_choice_finish(1), state.as_mut())
        .expect("renders");
    assert!(
        !events
            .iter()
            .any(|e| e.event.as_deref() == Some("message_delta"))
    );

    // Act
    let (settled, held) = settled_after(vec![two_choice_finish(1), usage_chunk(9_000)]).await;

    // Assert
    assert_eq!(settled, SettleOutcome::NotPublished);
    assert_eq!(held, None);
}

#[tokio::test]
async fn a_first_choice_finish_opens_the_terminal_window() {
    let (settled, held) = settled_after(vec![two_choice_finish(0), usage_chunk(9_000)]).await;

    assert_eq!(settled, SettleOutcome::Published);
    assert_eq!(held, Some(9_000));
}
