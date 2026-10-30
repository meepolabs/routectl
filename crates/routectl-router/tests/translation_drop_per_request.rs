//! Translation-drop counters advance once per CLIENT request, not once per
//! normalize call, when the request carries an observation slot: a
//! same-target retry, a learned beta repair and a fallback hop all normalize
//! the request again, and none of them may count it again.
//!
//! Each test drives a real `Router` over real providers at mock upstreams and
//! reads the process-global counters as deltas. Every test in this binary is
//! serialized, because every one of them moves the same global keys.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError};

use routectl_core::{
    AnthropicThinking, ChatRequest, ContentPart, KnownContentPart, Message, MessageContent,
    RequestObservation, Role, ThinkingObject,
};
use routectl_providers::anthropic_api::{
    AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig,
};
use routectl_providers::openai_compat::{
    HistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect,
};
use routectl_providers::translation_drop_metrics::{
    translation_drop_snapshot, translation_lane_seen,
};
use routectl_router::{
    AliasValue, Config, Dispatched, ProviderEntry, ResolvedModel, RetryPolicy, Router,
    RouterOptions,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

mod common;

const COMPAT: &str = "openai-compat";
const ANTHROPIC: &str = "anthropic";

/// What [`lossy_request`] loses on the openai-compat wire, written out by
/// hand so no expectation is derived from the counters under test.
const COMPAT_DROPS: &[&str] = &[
    "anthropic_beta_unsupported",
    "thinking_type_unrepresentable",
];
/// What [`lossy_request`] loses on the anthropic wire.
const ANTHROPIC_DROPS: &[&str] = &["system_part_unrepresentable"];

/// The committed Bedrock Mantle beta-rejection capture.
const MANTLE_FIXTURE: &str =
    include_str!("fixtures/rejections/anthropic-api/beta-prompt-caching-scope-mantle.json");
/// The flag the capture names.
const REJECTED_BETA: &str = "prompt-caching-scope-2026-01-05";
/// A client flag every mock accepts.
const KEPT_BETA: &str = "kept-2099-01-01";
const CLAUDE_MODEL: &str = "claude-opus-5-5";

fn text_turn(role: Role, text: &str) -> Message {
    Message {
        refusal: None,
        role,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// A request both lanes lose something from: a system-role turn carrying an
/// image, Anthropic beta flags, and a thinking type canonical reasoning
/// cannot express. Carries `slot`.
fn lossy_request(model: &str, slot: &RequestObservation) -> ChatRequest {
    let system_turn = Message {
        content: MessageContent::Parts(vec![
            ContentPart::Known(KnownContentPart::Text {
                text: "be brief".into(),
                citations: None,
                cache_control: None,
            }),
            ContentPart::Known(KnownContentPart::ImageUrl {
                image_url: json!({"url": "https://example.com/a.png"}),
                cache_control: None,
            }),
        ]),
        ..text_turn(Role::System, "unused")
    };
    let mut req = ChatRequest {
        model: model.into(),
        messages: vec![system_turn, text_turn(Role::User, "hello")].into(),
        max_tokens: Some(16),
        anthropic_beta: vec![KEPT_BETA.to_string(), REJECTED_BETA.to_string()],
        ..Default::default()
    };
    req.routectl_internal.anthropic_thinking = AnthropicThinking::Sent(
        ThinkingObject::from_client(json!({"type": "future_mode"})).expect("an object"),
    );
    req.routectl_internal.observation = Some(slot.clone());
    req
}

/// Global counters for the lanes these tests drive, read once.
#[derive(Debug, Clone)]
struct Counters {
    lane_seen: BTreeMap<&'static str, u64>,
    drops: BTreeMap<(&'static str, &'static str), u64>,
}

impl Counters {
    fn read() -> Self {
        Self {
            lane_seen: [COMPAT, ANTHROPIC]
                .into_iter()
                .map(|lane| (lane, translation_lane_seen(lane)))
                .collect(),
            drops: translation_drop_snapshot()
                .into_iter()
                .map(|e| ((e.lane, e.drop_class), e.drop_count))
                .collect(),
        }
    }

    fn lane_seen_delta(&self, before: &Self, lane: &str) -> u64 {
        self.lane_seen[lane] - before.lane_seen[lane]
    }

    /// Every `(lane, class)` pair that moved, with its delta.
    fn drop_deltas(&self, before: &Self) -> BTreeMap<(&'static str, &'static str), u64> {
        self.drops
            .iter()
            .filter_map(|(key, after)| {
                let prior = before.drops.get(key).copied().unwrap_or(0);
                (*after > prior).then_some((*key, after - prior))
            })
            .collect()
    }
}

/// `lane`'s expected pairs, each moved by exactly one.
fn once_each(
    lane: &'static str,
    classes: &[&'static str],
) -> Vec<((&'static str, &'static str), u64)> {
    classes.iter().map(|class| ((lane, *class), 1)).collect()
}

/// Answers each request with the next status in `statuses` (repeating the
/// last), as an openai-compat chat completion on success.
struct CompatSequence {
    calls: Mutex<usize>,
    statuses: Vec<u16>,
}

impl Respond for CompatSequence {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let mut calls = self.calls.lock().unwrap_or_else(PoisonError::into_inner);
        let status = self.statuses[(*calls).min(self.statuses.len() - 1)];
        *calls += 1;
        if status != 200 {
            return ResponseTemplate::new(status).set_body_string("unavailable");
        }
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-observation",
            "object": "chat.completion",
            "created": 0,
            "model": "gpt-4o",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
        }))
    }
}

async fn compat_upstream(statuses: Vec<u16>) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(CompatSequence {
            calls: Mutex::new(0),
            statuses,
        })
        .mount(&server)
        .await;
    server
}

fn compat_provider(server: &MockServer) -> Arc<OpenAiCompatProvider> {
    Arc::new(OpenAiCompatProvider::new(OpenAiCompatConfig {
        id: "p-compat".into(),
        base_url: server.uri(),
        api_key: "test-key".into(),
        header_extras: Vec::new(),
        payload_extras: None,
        reasoning_dialect: ReasoningDialect::OpenAi,
        history_reasoning: HistoryReasoning::Auto,
        user_agent: None,
        strict_translation: false,
        #[cfg(feature = "bedrock")]
        mantle: None,
    }))
}

/// With a `rejection`, answers it while the request's `anthropic-beta`
/// header carries [`REJECTED_BETA`]; answers a Messages success otherwise.
struct MantleLikeUpstream {
    rejection: Option<(u16, String)>,
}

impl Respond for MantleLikeUpstream {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let rejected = self.rejection.is_some()
            && request
                .headers
                .get("anthropic-beta")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|beta| beta.split(',').any(|flag| flag.trim() == REJECTED_BETA));
        if let (true, Some((status, body))) = (rejected, &self.rejection) {
            return ResponseTemplate::new(*status).set_body_raw(body.clone(), "application/json");
        }
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_observation",
            "type": "message",
            "role": "assistant",
            "model": CLAUDE_MODEL,
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 1}
        }))
    }
}

fn mantle_capture() -> (u16, String) {
    let fixture: Value = serde_json::from_str(MANTLE_FIXTURE).expect("fixture is JSON");
    let status = u16::try_from(fixture["status"].as_u64().expect("status")).expect("u16 status");
    let body = fixture["body"].as_str().expect("body").to_string();
    assert!(
        body.contains(&format!("`{REJECTED_BETA}`")),
        "premise: the capture names the rejected flag"
    );
    (status, body)
}

/// An anthropic-api upstream: the Mantle capture for the rejected flag when
/// `reject_beta`, otherwise success for every request.
async fn anthropic_upstream(reject_beta: bool) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(MantleLikeUpstream {
            rejection: reject_beta.then(mantle_capture),
        })
        .mount(&server)
        .await;
    server
}

fn anthropic_provider(server: &MockServer) -> Arc<AnthropicApiProvider> {
    let addr = *server.address();
    let cfg = AnthropicApiConfig {
        id: "p-anthropic".into(),
        auth: Arc::new(routectl_core::StaticToken::new("observation-test-key")),
        base_url: format!("http://api.anthropic.com:{}", addr.port()),
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
        #[cfg(feature = "bedrock")]
        mantle: None,
    };
    Arc::new(AnthropicApiProvider::new(cfg).with_host_resolved_to_for_tests(addr))
}

/// One chain target: config key, provider entry, provider, upstream id.
struct Target {
    name: &'static str,
    entry: ProviderEntry,
    provider: Arc<dyn routectl_core::Provider>,
    upstream: &'static str,
}

/// Alias `fast` over `targets` in order, with `retry_on_5xx` same-target
/// attempts per target.
fn router(targets: Vec<Target>, retry_on_5xx: u32) -> Router {
    let mut config = Config::default();
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    let mut chain = Vec::new();
    for target in targets {
        let model = format!("m-{}", target.name);
        config.providers.insert(target.name.into(), target.entry);
        models.insert(
            model.clone(),
            Arc::new(ResolvedModel::new(
                &model,
                target.name,
                target.provider,
                target.upstream,
            )),
        );
        chain.push(model);
    }
    config
        .aliases
        .insert("fast".into(), AliasValue::Chain(chain));
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    retry.retry_on_5xx = Some(retry_on_5xx);
    retry.initial_backoff_ms = 1;
    retry.backoff_multiplier = 1.0;
    retry.jitter_ms = 0;
    config.retry = retry;
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

fn compat_target(server: &MockServer) -> Target {
    Target {
        name: "p-compat",
        entry: ProviderEntry::openai_compat(server.uri(), common::file_ref("k")),
        provider: compat_provider(server),
        upstream: "gpt-4o",
    }
}

fn anthropic_target(server: &MockServer) -> Target {
    Target {
        name: "p-anthropic",
        entry: ProviderEntry::anthropic_api(common::file_ref("k")),
        provider: anthropic_provider(server),
        upstream: CLAUDE_MODEL,
    }
}

async fn wire_calls(server: &MockServer) -> usize {
    server.received_requests().await.expect("recorded").len()
}

#[tokio::test]
#[serial_test::serial(translation_drop_globals)]
async fn a_same_target_retry_counts_the_lane_and_each_drop_once() {
    // Arrange
    let upstream = compat_upstream(vec![503, 200]).await;
    let router = router(vec![compat_target(&upstream)], 2);
    let slot = RequestObservation::new();
    let before = Counters::read();

    // Act
    let Dispatched { result, meta } = router
        .complete_with_options(lossy_request("fast", &slot), RouterOptions::new())
        .await;

    // Assert: two normalizes reached the wire, one request was counted.
    result.expect("the retry is served");
    assert_eq!(
        wire_calls(&upstream).await,
        2,
        "premise: the target was retried"
    );
    assert_eq!(meta.fallback_count, 0, "premise: no fallback hop");
    let after = Counters::read();
    assert_eq!(after.lane_seen_delta(&before, COMPAT), 1);
    assert_eq!(
        after.drop_deltas(&before).into_iter().collect::<Vec<_>>(),
        once_each(COMPAT, COMPAT_DROPS)
    );
    assert_eq!(slot.snapshot().translation_drops, COMPAT_DROPS.len() as u64);
}

#[tokio::test]
#[serial_test::serial(translation_drop_globals)]
async fn a_learned_beta_repair_redispatch_counts_the_lane_and_each_drop_once() {
    // Arrange
    let upstream = anthropic_upstream(true).await;
    let router = router(vec![anthropic_target(&upstream)], 0);
    let slot = RequestObservation::new();
    let before = Counters::read();

    // Act
    let Dispatched { result, meta } = router
        .complete_with_options(lossy_request("fast", &slot), RouterOptions::new())
        .await;

    // Assert: the repair re-dispatched (two wire calls, one learned beta) and
    // the request was counted once.
    result.expect("the repaired request is served");
    assert_eq!(
        wire_calls(&upstream).await,
        2,
        "premise: the repair re-sent"
    );
    assert_eq!(
        meta.learned_capabilities.len(),
        1,
        "premise: the beta was learned"
    );
    let after = Counters::read();
    assert_eq!(after.lane_seen_delta(&before, ANTHROPIC), 1);
    assert_eq!(
        after.drop_deltas(&before).into_iter().collect::<Vec<_>>(),
        once_each(ANTHROPIC, ANTHROPIC_DROPS)
    );
    assert_eq!(
        slot.snapshot().translation_drops,
        ANTHROPIC_DROPS.len() as u64
    );
}

#[tokio::test]
#[serial_test::serial(translation_drop_globals)]
async fn a_fallback_to_a_second_lane_counts_each_lane_once_and_the_slot_counts_both() {
    // Arrange
    let first = compat_upstream(vec![503]).await;
    let second = anthropic_upstream(false).await;
    let router = router(vec![compat_target(&first), anthropic_target(&second)], 2);
    let slot = RequestObservation::new();
    let before = Counters::read();

    // Act
    let Dispatched { result, meta } = router
        .complete_with_options(lossy_request("fast", &slot), RouterOptions::new())
        .await;

    // Assert
    result.expect("the second lane serves");
    assert_eq!(meta.fallback_count, 1, "premise: one fallback hop");
    assert_eq!(
        wire_calls(&first).await,
        2,
        "premise: the first lane was retried"
    );
    assert_eq!(
        wire_calls(&second).await,
        1,
        "premise: the second lane served once"
    );
    let after = Counters::read();
    assert_eq!(after.lane_seen_delta(&before, COMPAT), 1);
    assert_eq!(after.lane_seen_delta(&before, ANTHROPIC), 1);
    let mut expected = once_each(ANTHROPIC, ANTHROPIC_DROPS);
    expected.extend(once_each(COMPAT, COMPAT_DROPS));
    expected.sort_unstable();
    assert_eq!(
        after.drop_deltas(&before).into_iter().collect::<Vec<_>>(),
        expected
    );
    assert_eq!(
        slot.snapshot().translation_drops,
        (COMPAT_DROPS.len() + ANTHROPIC_DROPS.len()) as u64
    );
}

#[tokio::test]
#[serial_test::serial(translation_drop_globals)]
async fn one_requests_global_drop_delta_equals_its_slot_value() {
    // Arrange: the retried request is the one with the most normalize calls
    // per pair, so a per-call count would show here first.
    let upstream = compat_upstream(vec![503, 503, 200]).await;
    let router = router(vec![compat_target(&upstream)], 3);
    let slot = RequestObservation::new();
    let before = Counters::read();

    // Act
    let Dispatched { result, .. } = router
        .complete_with_options(lossy_request("fast", &slot), RouterOptions::new())
        .await;

    // Assert
    result.expect("served on the third attempt");
    assert_eq!(wire_calls(&upstream).await, 3, "premise: three attempts");
    let global: u64 = Counters::read().drop_deltas(&before).values().sum();
    assert_eq!(global, slot.snapshot().translation_drops);
    assert!(global > 0, "premise: the request dropped something");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[serial_test::serial(translation_drop_globals)]
async fn two_concurrent_requests_keep_separate_first_sighting_sets() {
    // Arrange: each request has its own target answering 503 then 200, so
    // each is normalized exactly twice and the four calls interleave across
    // worker threads.
    let upstream_a = compat_upstream(vec![503, 200]).await;
    let upstream_b = compat_upstream(vec![503, 200]).await;
    let router_a = Arc::new(router(vec![compat_target(&upstream_a)], 2));
    let router_b = Arc::new(router(vec![compat_target(&upstream_b)], 2));
    let first = RequestObservation::new();
    let second = RequestObservation::new();
    let before = Counters::read();

    // Act
    let send = |router: Arc<Router>, slot: RequestObservation| {
        tokio::spawn(async move {
            router
                .complete_with_options(lossy_request("fast", &slot), RouterOptions::new())
                .await
                .result
                .is_ok()
        })
    };
    let (a, b) = tokio::join!(
        send(Arc::clone(&router_a), first.clone()),
        send(Arc::clone(&router_b), second.clone())
    );

    // Assert: each request counted itself once; neither absorbed the other.
    assert!(a.expect("joined") && b.expect("joined"), "both are served");
    assert_eq!(wire_calls(&upstream_a).await, 2, "premise: first retried");
    assert_eq!(wire_calls(&upstream_b).await, 2, "premise: second retried");
    let after = Counters::read();
    assert_eq!(after.lane_seen_delta(&before, COMPAT), 2);
    let expected: BTreeSet<_> = COMPAT_DROPS
        .iter()
        .map(|class| ((COMPAT, *class), 2))
        .collect();
    assert_eq!(
        after
            .drop_deltas(&before)
            .into_iter()
            .collect::<BTreeSet<_>>(),
        expected
    );
    assert_eq!(
        first.snapshot().translation_drops,
        COMPAT_DROPS.len() as u64
    );
    assert_eq!(
        second.snapshot().translation_drops,
        COMPAT_DROPS.len() as u64
    );
}
