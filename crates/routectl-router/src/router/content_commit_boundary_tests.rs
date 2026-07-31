//! The hard non-retry boundary is the first CONTENT chunk emitted to the
//! client, not stream-open (contracts sec 5). A stream opens with a
//! content-free `delta.role` chunk plus optional id/model metadata; an
//! error, EOS, timeout, or buffer overflow in the [stream-open,
//! first-content] window must still fall over to a sibling provider, and
//! the buffered content-free chunks must never leak to the client on that
//! failure. Once content commits, mid-stream errors are terminal and the
//! buffered metadata is replayed in order ahead of it.

use super::*;
use crate::config::Config;
use crate::resolved::ResolvedModel;
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use routectl_core::{
    AnthropicUnifiedQuota, ChatChunk, ChatRequest, ChatResponse, ChunkChoice, ChunkDelta, Error,
    OpaqueSseEvent, Provider, ReasoningDetail, ReasoningDetailKind, Result, Role, UpstreamMeta,
    UsageDelta,
};
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

// -------- content / content-free chunk builders --------

/// A content-free leading `delta.role="assistant"` chunk (the stream
/// opener). Carries `id` for provenance tracing but no client-visible
/// content, so it must be buffered, never committing the provider.
fn role_chunk(id: &str) -> ChatChunk {
    ChatChunk {
        id: id.into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                role: Some(Role::Assistant),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
}

/// A content-free id/model metadata chunk (empty delta, empty choices).
fn meta_chunk(id: &str) -> ChatChunk {
    ChatChunk {
        id: id.into(),
        model: "wire".into(),
        ..Default::default()
    }
}

/// A content-bearing text chunk.
fn text_chunk(id: &str, text: &str) -> ChatChunk {
    ChatChunk {
        id: id.into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some(text.into()),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
}

/// A content-bearing tool-call chunk.
fn tool_chunk(id: &str) -> ChatChunk {
    ChatChunk {
        id: id.into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                tool_calls: Some(vec![serde_json::json!({
                    "index": 0,
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "f", "arguments": ""}
                })]),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
}

/// A content-bearing reasoning chunk carrying a typed reasoning block
/// (no plain text), exercising the `reasoning_details` content signal.
fn reasoning_chunk(id: &str) -> ChatChunk {
    ChatChunk {
        id: id.into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                reasoning_details: vec![ReasoningDetail {
                    kind: ReasoningDetailKind::Summary,
                    id: None,
                    format: None,
                    index: None,
                    payload: serde_json::json!({"text": "thinking"}),
                }],
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
}

/// A content-bearing opaque-event chunk (client-visible unknown block).
fn opaque_chunk(id: &str) -> ChatChunk {
    ChatChunk {
        id: id.into(),
        opaque_events: vec![OpaqueSseEvent::ContentBlockStop { upstream_index: 0 }],
        ..Default::default()
    }
}

// -------- scripted provider --------

/// One step in a stream script: a chunk to yield, an upstream error to
/// yield (status), or a hang (pend forever after the prior steps).
enum Step {
    Chunk(Box<ChatChunk>),
    Upstream(u16),
    Hang,
}

/// Wrap a chunk as a `Step::Chunk` (boxed to keep the enum small).
fn chunk(c: ChatChunk) -> Step {
    Step::Chunk(Box::new(c))
}

/// A provider whose `stream()` replays one scripted sequence per call
/// (front of the queue), so a provider dispatched twice (auth retry) can
/// answer differently on the retry. `stream_calls` counts dispatches.
struct ScriptedProvider {
    id: String,
    scripts: parking_lot::Mutex<VecDeque<Vec<Step>>>,
    stream_calls: Arc<AtomicUsize>,
}

impl ScriptedProvider {
    fn new(id: &str, scripts: Vec<Vec<Step>>, stream_calls: Arc<AtomicUsize>) -> Self {
        Self {
            id: id.into(),
            scripts: parking_lot::Mutex::new(scripts.into_iter().collect()),
            stream_calls,
        }
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
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
        unreachable!("streaming tests only")
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        let steps = self
            .scripts
            .lock()
            .pop_front()
            .expect("stream() called more times than scripted");
        let id = self.id.clone();
        let s = async_stream::stream! {
            for step in steps {
                match step {
                    Step::Chunk(c) => yield Ok(*c),
                    Step::Upstream(status) => {
                        yield Err(Error::upstream(&id, status, "scripted upstream error"));
                    }
                    Step::Hang => {
                        futures::future::pending::<()>().await;
                    }
                }
            }
        };
        Ok(s.boxed())
    }
    async fn on_auth_failure(&self) -> Result<()> {
        Ok(())
    }
}

// -------- routers --------

fn compat_toml() -> &'static str {
    r#"
[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"

[providers.p2]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"

[aliases]
alias = ["m1", "m2"]
"#
}

/// Two-leg alias chain: `alias -> [m1 (p1), m2 (p2)]`. `timeout_ms`, when
/// set, pins m1's per-model `stream_first_byte_timeout_ms`.
fn two_leg_router(
    leg1: Arc<dyn Provider>,
    leg2: Arc<dyn Provider>,
    timeout_ms: Option<u64>,
) -> Router {
    let config: Config = toml::from_str(compat_toml()).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    let mut m1 = ResolvedModel::new("m1", "p1", leg1, "wire-1");
    if let Some(ms) = timeout_ms {
        m1 = m1.with_stream_first_byte_timeout_ms(ms);
    }
    models.insert("m1".to_string(), Arc::new(m1));
    models.insert(
        "m2".to_string(),
        Arc::new(ResolvedModel::new("m2", "p2", leg2, "wire-2")),
    );
    router.install_resolved_models(models);
    router
}

/// Single-leg router (`m1` on `p1`) for the terminal-mid-stream cases.
fn single_leg_router(leg1: Arc<dyn Provider>, timeout_ms: Option<u64>) -> Router {
    let config: Config = toml::from_str(compat_toml()).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    let mut m1 = ResolvedModel::new("m1", "p1", leg1, "wire-1");
    if let Some(ms) = timeout_ms {
        m1 = m1.with_stream_first_byte_timeout_ms(ms);
    }
    models.insert("m1".to_string(), Arc::new(m1));
    router.install_resolved_models(models);
    router
}

fn alias_req() -> ChatRequest {
    ChatRequest {
        model: "alias".into(),
        messages: vec![].into(),
        ..Default::default()
    }
}

fn m1_req() -> ChatRequest {
    ChatRequest {
        model: "m1".into(),
        messages: vec![].into(),
        ..Default::default()
    }
}

/// Collect a returned stream into its Ok chunks, asserting no Err frames.
async fn collect_ok(stream: BoxStream<'static, Result<ChatChunk>>) -> Vec<ChatChunk> {
    stream
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .map(|r| r.expect("no mid-stream error expected"))
        .collect()
}

// ============================ tests ============================

#[tokio::test]
async fn role_then_overload_falls_over_to_sibling_without_leaking_buffer() {
    // leg1 opens with a role chunk then 529s before any content; leg2
    // opens with role + text. Exactly one fallback, both dispatched, and
    // the output carries ONLY leg2's chunks -- leg1's buffered role never
    // leaks.
    let c1 = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::new(AtomicUsize::new(0));
    let leg1 = Arc::new(ScriptedProvider::new(
        "p1",
        vec![vec![chunk(role_chunk("leg1-role")), Step::Upstream(529)]],
        c1.clone(),
    ));
    let leg2 = Arc::new(ScriptedProvider::new(
        "p2",
        vec![vec![
            chunk(role_chunk("leg2-role")),
            chunk(text_chunk("leg2-text", "hi")),
        ]],
        c2.clone(),
    ));
    let router = two_leg_router(leg1, leg2, None);

    let stream = router
        .stream(alias_req())
        .await
        .expect("fallover to the healthy sibling yields an Ok stream");
    let chunks = collect_ok(stream).await;

    assert_eq!(c1.load(Ordering::SeqCst), 1, "leg1 dispatched exactly once");
    assert_eq!(c2.load(Ordering::SeqCst), 1, "leg2 dispatched exactly once");
    assert!(
        chunks.iter().all(|c| !c.id.starts_with("leg1")),
        "no leg1 chunk (its buffered role) may leak into the output: {:?}",
        chunks.iter().map(|c| &c.id).collect::<Vec<_>>(),
    );
    assert!(
        chunks.iter().any(|c| c.id == "leg2-text"),
        "the sibling's content must be present",
    );
}

#[tokio::test]
async fn role_then_auth_401_recovers_with_one_refresh_retry() {
    // leg1 opens with role then 401. Pre-content auth recovery refreshes
    // and retries the SAME leg exactly once; the retry opens role + text.
    // The role of the first attempt did not commit, so recovery is allowed.
    let c1 = Arc::new(AtomicUsize::new(0));
    let leg1 = Arc::new(ScriptedProvider::new(
        "p1",
        vec![
            vec![chunk(role_chunk("leg1-role-a")), Step::Upstream(401)],
            vec![
                chunk(role_chunk("leg1-role-b")),
                chunk(text_chunk("leg1-text", "recovered")),
            ],
        ],
        c1.clone(),
    ));
    let router = single_leg_router(leg1, None);

    let stream = router
        .stream(m1_req())
        .await
        .expect("pre-content 401 recovers via refresh + one retry");
    let chunks = collect_ok(stream).await;

    assert_eq!(
        c1.load(Ordering::SeqCst),
        2,
        "the same leg is dispatched twice: the 401 attempt and the retry",
    );
    assert!(
        chunks.iter().any(|c| c.id == "leg1-text"),
        "the recovered attempt's content must reach the client",
    );
    assert!(
        chunks.iter().all(|c| c.id != "leg1-role-a"),
        "the failed first attempt's buffered role must not leak",
    );
}

#[tokio::test]
async fn role_then_eos_is_precontent_empty_and_falls_over() {
    // leg1 emits only a role chunk then closes (EOS before content):
    // pre-content empty stream, fall over to leg2.
    let c1 = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::new(AtomicUsize::new(0));
    let leg1 = Arc::new(ScriptedProvider::new(
        "p1",
        vec![vec![chunk(role_chunk("leg1-role"))]],
        c1.clone(),
    ));
    let leg2 = Arc::new(ScriptedProvider::new(
        "p2",
        vec![vec![chunk(text_chunk("leg2-text", "hi"))]],
        c2.clone(),
    ));
    let router = two_leg_router(leg1, leg2, None);

    let chunks = collect_ok(router.stream(alias_req()).await.expect("falls over")).await;

    assert_eq!(c1.load(Ordering::SeqCst), 1);
    assert_eq!(c2.load(Ordering::SeqCst), 1);
    assert!(chunks.iter().any(|c| c.id == "leg2-text"));
    assert!(chunks.iter().all(|c| !c.id.starts_with("leg1")));
}

#[tokio::test]
async fn immediate_eos_falls_over_unchanged() {
    // leg1 emits nothing at all (immediate EOS) -- the existing
    // empty-stream fallback, unchanged by the content boundary.
    let c1 = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::new(AtomicUsize::new(0));
    let leg1 = Arc::new(ScriptedProvider::new("p1", vec![vec![]], c1.clone()));
    let leg2 = Arc::new(ScriptedProvider::new(
        "p2",
        vec![vec![chunk(text_chunk("leg2-text", "hi"))]],
        c2.clone(),
    ));
    let router = two_leg_router(leg1, leg2, None);

    let chunks = collect_ok(router.stream(alias_req()).await.expect("falls over")).await;

    assert_eq!(c1.load(Ordering::SeqCst), 1);
    assert_eq!(c2.load(Ordering::SeqCst), 1);
    assert!(chunks.iter().any(|c| c.id == "leg2-text"));
}

#[tokio::test]
async fn error_first_falls_over_with_no_buffered_output() {
    // leg1 errors on the very first stream item (no role, no metadata):
    // unchanged fallback, nothing buffered to leak.
    let c1 = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::new(AtomicUsize::new(0));
    let leg1 = Arc::new(ScriptedProvider::new(
        "p1",
        vec![vec![Step::Upstream(503)]],
        c1.clone(),
    ));
    let leg2 = Arc::new(ScriptedProvider::new(
        "p2",
        vec![vec![chunk(text_chunk("leg2-text", "hi"))]],
        c2.clone(),
    ));
    let router = two_leg_router(leg1, leg2, None);

    let chunks = collect_ok(router.stream(alias_req()).await.expect("falls over")).await;

    assert_eq!(c1.load(Ordering::SeqCst), 1);
    assert_eq!(c2.load(Ordering::SeqCst), 1);
    assert_eq!(
        chunks.iter().filter(|c| c.id.starts_with("leg1")).count(),
        0,
        "no leg1 output at all",
    );
    assert!(chunks.iter().any(|c| c.id == "leg2-text"));
}

#[tokio::test]
async fn role_then_text_then_eos_commits_in_order_without_fallback() {
    // leg1 opens role + text then closes cleanly: no fallback, and the
    // buffered role is replayed in order ahead of the content.
    let c1 = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::new(AtomicUsize::new(0));
    let leg1 = Arc::new(ScriptedProvider::new(
        "p1",
        vec![vec![
            chunk(role_chunk("leg1-role")),
            chunk(text_chunk("leg1-text", "hi")),
        ]],
        c1.clone(),
    ));
    let leg2 = Arc::new(ScriptedProvider::new(
        "p2",
        vec![vec![chunk(text_chunk("leg2-text", "no"))]],
        c2.clone(),
    ));
    let router = two_leg_router(leg1, leg2, None);

    let chunks = collect_ok(router.stream(alias_req()).await.expect("commits on leg1")).await;

    assert_eq!(c1.load(Ordering::SeqCst), 1);
    assert_eq!(c2.load(Ordering::SeqCst), 0, "no fallback: leg1 committed");
    let ids: Vec<&str> = chunks.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["leg1-role", "leg1-text"],
        "the buffered role is replayed in order ahead of the first content",
    );
}

#[tokio::test]
async fn role_then_tool_calls_commits_as_content() {
    let c1 = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::new(AtomicUsize::new(0));
    let leg1 = Arc::new(ScriptedProvider::new(
        "p1",
        vec![vec![
            chunk(role_chunk("leg1-role")),
            chunk(tool_chunk("leg1-tool")),
        ]],
        c1.clone(),
    ));
    let leg2 = Arc::new(ScriptedProvider::new(
        "p2",
        vec![vec![chunk(text_chunk("leg2-text", "no"))]],
        c2.clone(),
    ));
    let router = two_leg_router(leg1, leg2, None);

    let chunks = collect_ok(router.stream(alias_req()).await.expect("tool-call commits")).await;

    assert_eq!(
        c2.load(Ordering::SeqCst),
        0,
        "tool_calls is content: no fallback"
    );
    assert!(chunks.iter().any(|c| c.id == "leg1-tool"));
}

#[tokio::test]
async fn role_then_reasoning_commits_as_content() {
    let c1 = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::new(AtomicUsize::new(0));
    let leg1 = Arc::new(ScriptedProvider::new(
        "p1",
        vec![vec![
            chunk(role_chunk("leg1-role")),
            chunk(reasoning_chunk("leg1-reasoning")),
        ]],
        c1.clone(),
    ));
    let leg2 = Arc::new(ScriptedProvider::new(
        "p2",
        vec![vec![chunk(text_chunk("leg2-text", "no"))]],
        c2.clone(),
    ));
    let router = two_leg_router(leg1, leg2, None);

    let chunks = collect_ok(router.stream(alias_req()).await.expect("reasoning commits")).await;

    assert_eq!(
        c2.load(Ordering::SeqCst),
        0,
        "reasoning is content: no fallback"
    );
    assert!(chunks.iter().any(|c| c.id == "leg1-reasoning"));
}

#[tokio::test]
async fn role_then_opaque_event_commits_as_content() {
    let c1 = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::new(AtomicUsize::new(0));
    let leg1 = Arc::new(ScriptedProvider::new(
        "p1",
        vec![vec![
            chunk(role_chunk("leg1-role")),
            chunk(opaque_chunk("leg1-opaque")),
        ]],
        c1.clone(),
    ));
    let leg2 = Arc::new(ScriptedProvider::new(
        "p2",
        vec![vec![chunk(text_chunk("leg2-text", "no"))]],
        c2.clone(),
    ));
    let router = two_leg_router(leg1, leg2, None);

    let chunks = collect_ok(router.stream(alias_req()).await.expect("opaque commits")).await;

    assert_eq!(
        c2.load(Ordering::SeqCst),
        0,
        "an opaque event is client-visible content: no fallback",
    );
    assert!(chunks.iter().any(|c| !c.opaque_events.is_empty()));
}

#[tokio::test]
async fn role_then_content_then_error_is_terminal_mid_stream() {
    // Once content commits, a subsequent error is terminal in-stream --
    // the sibling is NEVER dispatched.
    let c1 = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::new(AtomicUsize::new(0));
    let leg1 = Arc::new(ScriptedProvider::new(
        "p1",
        vec![vec![
            chunk(role_chunk("leg1-role")),
            chunk(text_chunk("leg1-text", "hi")),
            Step::Upstream(503),
        ]],
        c1.clone(),
    ));
    let leg2 = Arc::new(ScriptedProvider::new(
        "p2",
        vec![vec![chunk(text_chunk("leg2-text", "no"))]],
        c2.clone(),
    ));
    let router = two_leg_router(leg1, leg2, None);

    let stream = router
        .stream(alias_req())
        .await
        .expect("content committed -> Ok stream even though it errors mid-way");
    let items: Vec<_> = stream.collect().await;

    assert_eq!(c1.load(Ordering::SeqCst), 1);
    assert_eq!(
        c2.load(Ordering::SeqCst),
        0,
        "a post-content error is terminal: no fallback to the sibling",
    );
    assert!(
        items.last().expect("at least one item").is_err(),
        "the mid-stream error propagates as the terminal frame",
    );
    assert!(
        items.iter().filter(|r| r.is_ok()).count() >= 2,
        "role + text delivered before the error",
    );
}

#[tokio::test]
async fn role_then_hang_trips_first_content_timeout_and_falls_over() {
    // leg1 emits a role chunk then hangs. The role neither satisfies nor
    // resets the first-content timeout: the single timeout around
    // stream-open + the pre-content pull fires, and the chain falls over
    // to the healthy sibling.
    let c1 = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::new(AtomicUsize::new(0));
    let leg1 = Arc::new(ScriptedProvider::new(
        "p1",
        vec![vec![chunk(role_chunk("leg1-role")), Step::Hang]],
        c1.clone(),
    ));
    let leg2 = Arc::new(ScriptedProvider::new(
        "p2",
        vec![vec![chunk(text_chunk("leg2-text", "hi"))]],
        c2.clone(),
    ));
    // m1 gets a tight first-content timeout; m2 has none.
    let router = two_leg_router(leg1, leg2, Some(50));

    let chunks = collect_ok(
        router
            .stream(alias_req())
            .await
            .expect("role-then-hang times out and falls over"),
    )
    .await;

    assert_eq!(c1.load(Ordering::SeqCst), 1);
    assert_eq!(
        c2.load(Ordering::SeqCst),
        1,
        "timeout fell over to the sibling"
    );
    assert!(chunks.iter().any(|c| c.id == "leg2-text"));
    assert!(chunks.iter().all(|c| !c.id.starts_with("leg1")));
}

#[tokio::test]
async fn metadata_flood_exceeding_buffer_cap_falls_over() {
    // A stream of content-free metadata chunks exceeding the buffer cap
    // is a bounded pre-content failure: fall over, no leaked chunks.
    let c1 = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::new(AtomicUsize::new(0));
    let flood: Vec<Step> = (0..64)
        .map(|i| chunk(meta_chunk(&format!("leg1-meta-{i}"))))
        .collect();
    let leg1 = Arc::new(ScriptedProvider::new("p1", vec![flood], c1.clone()));
    let leg2 = Arc::new(ScriptedProvider::new(
        "p2",
        vec![vec![chunk(text_chunk("leg2-text", "hi"))]],
        c2.clone(),
    ));
    let router = two_leg_router(leg1, leg2, None);

    let chunks = collect_ok(router.stream(alias_req()).await.expect("falls over")).await;

    assert_eq!(c1.load(Ordering::SeqCst), 1);
    assert_eq!(c2.load(Ordering::SeqCst), 1);
    assert!(
        chunks.iter().all(|c| !c.id.starts_with("leg1")),
        "no buffered metadata chunk may leak on overflow",
    );
    assert!(chunks.iter().any(|c| c.id == "leg2-text"));
}

#[tokio::test]
async fn buffered_role_preserves_upstream_meta_on_commit() {
    // upstream_meta rides the first canonical chunk (the response head).
    // When that head is a buffered content-free role chunk, the metadata
    // must survive the buffer-then-replay and reach the client on commit.
    let c1 = Arc::new(AtomicUsize::new(0));
    let c2 = Arc::new(AtomicUsize::new(0));
    let mut head = role_chunk("leg1-role");
    head.upstream_meta = Some(UpstreamMeta::from_anthropic_unified(
        AnthropicUnifiedQuota::default(),
    ));
    let leg1 = Arc::new(ScriptedProvider::new(
        "p1",
        vec![vec![chunk(head), chunk(text_chunk("leg1-text", "hi"))]],
        c1.clone(),
    ));
    let leg2 = Arc::new(ScriptedProvider::new(
        "p2",
        vec![vec![chunk(text_chunk("leg2-text", "no"))]],
        c2.clone(),
    ));
    let router = two_leg_router(leg1, leg2, None);

    let chunks = collect_ok(router.stream(alias_req()).await.expect("commits on leg1")).await;

    assert_eq!(c2.load(Ordering::SeqCst), 0, "no fallback: leg1 committed");
    let role = chunks
        .iter()
        .find(|c| c.id == "leg1-role")
        .expect("the buffered role chunk is replayed ahead of content");
    assert!(
        role.upstream_meta.is_some(),
        "upstream_meta on the buffered head must survive buffer-then-replay",
    );
}

#[test]
fn is_content_bearing_classifies_every_variant() {
    // Content-bearing: each canonical content signal, in isolation.
    assert!(is_content_bearing(&text_chunk("x", "hi")), "text");
    assert!(is_content_bearing(&tool_chunk("x")), "tool_calls");
    assert!(
        is_content_bearing(&reasoning_chunk("x")),
        "reasoning_details"
    );
    assert!(is_content_bearing(&opaque_chunk("x")), "opaque_events");
    assert!(
        is_content_bearing(&delta_chunk(ChunkDelta {
            reasoning: Some("thinking".into()),
            ..Default::default()
        })),
        "plain reasoning text",
    );

    // Content-free: metadata, terminal-only, and empty-value chunks.
    assert!(!is_content_bearing(&role_chunk("x")), "role-only");
    assert!(!is_content_bearing(&meta_chunk("x")), "id/model metadata");
    assert!(
        !is_content_bearing(&ChatChunk {
            usage: Some(UsageDelta {
                total_tokens: Some(7),
                ..Default::default()
            }),
            ..Default::default()
        }),
        "usage-only",
    );
    assert!(
        !is_content_bearing(&ChatChunk {
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta::default(),
                finish_reason: Some("stop".into()),
                matched_stop_sequence: None,
            }],
            ..Default::default()
        }),
        "finish_reason-only",
    );
    assert!(
        !is_content_bearing(&delta_chunk(ChunkDelta {
            content: Some(String::new()),
            ..Default::default()
        })),
        "empty text string",
    );
    assert!(
        !is_content_bearing(&delta_chunk(ChunkDelta {
            reasoning: Some(String::new()),
            ..Default::default()
        })),
        "empty reasoning string",
    );
    assert!(
        !is_content_bearing(&delta_chunk(ChunkDelta {
            tool_calls: Some(vec![]),
            ..Default::default()
        })),
        "empty tool_calls vec",
    );
}

/// A chunk carrying a single choice with the given delta.
fn delta_chunk(delta: ChunkDelta) -> ChatChunk {
    ChatChunk {
        choices: vec![ChunkChoice {
            index: 0,
            delta,
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
}
