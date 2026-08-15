//! The calibration estimate must describe the payload that actually went
//! upstream, on every path where a reasoning-replay strip shrinks it.
//!
//! `record_would_trim` stamps the estimate once, above the retry loop. Both
//! strip moments happen after that stamp -- the proactive strip a resident
//! negative forces, and the strip repair an upstream rejection triggers -- and
//! each makes the dispatched request smaller than the stamp describes while the
//! provider's prompt total reflects the smaller payload. An uncorrected stamp
//! therefore biases the evidence ratio LOW, and a low correction factor shrinks
//! a corrected estimate until the context-window gate admits requests the static
//! estimate had correctly judged too large.
//!
//! Each test drives a REAL dispatch through a mock provider that keeps the
//! request it was handed, then asserts the persisted estimate equals the
//! estimator's value for THAT request -- the bytes the upstream priced -- and
//! is materially below the carried variant's estimate.

use super::super::{Router, RouterOptions};

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::stream::{self, BoxStream, StreamExt};
use parking_lot::Mutex;
use routectl_core::{
    CODEX_OAUTH, ChatChunk, ChatRequest, ChatResponse, Choice, ChunkChoice, ChunkDelta, Error,
    Message, MessageContent, Provider, ReasoningDetail, ReasoningDetailKind, Result, Role,
};
use serde_json::json;

use crate::config::Config;
use crate::context_trim::estimate_total_tokens;
use crate::resolved::ResolvedModel;

/// The pinned replay-rejection body, byte-exact (mirrors
/// `replay_repair_tests`). The fixture corpus is gitignored and never ships,
/// so this is an inline constant; it carries no secret.
const REPLAY_REJECT_BODY: &str = r#"{"error":{"code":"validation_error","message":"encrypted content missing recognized prefix (expected `rsn_` or `smry_`)","param":null,"type":"invalid_request_error"}}"#;

/// A mantle-lane mock that REMEMBERS every request it was handed, so a test
/// can estimate the exact payload the upstream priced. It rejects any request
/// still carrying a reasoning artifact and serves a stripped one, on both the
/// unary and the streaming surface.
struct RecordingReplayMock {
    calls: AtomicUsize,
    seen: Mutex<Vec<ChatRequest>>,
}

impl RecordingReplayMock {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// The request handed to the LAST upstream call -- the payload whose
    /// reported prompt total would be the paired actual.
    fn last_dispatched(&self) -> ChatRequest {
        self.seen.lock().last().cloned().expect("an upstream call")
    }

    fn admit(&self, req: &ChatRequest) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().push(req.clone());
        let carries_artifact = req
            .messages
            .iter()
            .any(|message| !message.reasoning_details.is_empty());
        if carries_artifact {
            return Err(Error::upstream("replay-mock", 400, REPLAY_REJECT_BODY));
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Provider for RecordingReplayMock {
    fn id(&self) -> &'static str {
        "replay-mock"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("replay-mock", "unused"))
    }
    fn replay_lane(&self) -> routectl_core::ReplayScheme {
        routectl_core::ReplayScheme::Mantle
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.admit(&req)?;
        Ok(success_response())
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.admit(&req)?;
        Ok(stream::once(async { Ok(content_chunk()) }).boxed())
    }
}

fn success_response() -> ChatResponse {
    ChatResponse {
        choices: vec![Choice {
            index: 0,
            message: Message {
                role: Role::Assistant,
                content: MessageContent::Text("ok".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
                refusal: None,
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
            logprobs: None,
        }],
        ..Default::default()
    }
}

/// A content-bearing chunk, so `try_stream_with_first_content` commits the
/// stream instead of walking on.
fn content_chunk() -> ChatChunk {
    ChatChunk {
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some("ok".into()),
                ..Default::default()
            },
            finish_reason: Some("stop".into()),
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
}

/// A request whose single assistant turn carries one BULKY non-portable
/// reasoning artifact. The bulk is the point: the strip has to move the
/// estimate by far more than the estimator's four-bytes-per-token
/// granularity, so an unchanged stamp cannot pass by rounding.
fn req_carrying_bulky_artifact() -> ChatRequest {
    ChatRequest {
        model: "m1".into(),
        messages: vec![Message {
            role: Role::Assistant,
            content: MessageContent::Text("prior answer".into()),
            reasoning: None,
            reasoning_details: vec![ReasoningDetail {
                kind: ReasoningDetailKind::Encrypted,
                id: Some("rs_1".into()),
                format: Some(CODEX_OAUTH.to_string()),
                index: None,
                payload: json!({"encrypted_content": "x".repeat(8_000)}),
            }],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        }]
        .into(),
        ..Default::default()
    }
}

const SINGLE_TARGET_TOML: &str = r#"
[providers.p1]
kind = "openai-responses"
api_key_ref = "literal:k"
auth_kind = "api-key"
"#;

fn router_with(provider: Arc<dyn Provider>) -> Router {
    let config: Config = toml::from_str(SINGLE_TARGET_TOML).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m1".to_string(),
        Arc::new(ResolvedModel::new("m1", "p1", provider, "wire-model")),
    );
    router.install_resolved_models(models);
    router
}

/// The shared assertion: the stamped estimate is EXACTLY the estimator's value
/// for the request the upstream was handed, and the strip moved it down by a
/// margin no rounding could explain.
fn assert_estimate_describes_dispatched_payload(
    stamped: Option<u64>,
    dispatched: &ChatRequest,
    carried: &ChatRequest,
) {
    let dispatched_estimate = estimate_total_tokens(dispatched);
    let carried_estimate = estimate_total_tokens(carried);
    assert_eq!(
        stamped,
        Some(dispatched_estimate),
        "the evidence numerator must be the estimate of the dispatched payload",
    );
    assert!(
        dispatched_estimate + 1_000 < carried_estimate,
        "sanity: the strip must move the estimate measurably \
         (dispatched {dispatched_estimate}, carried {carried_estimate})",
    );
}

#[tokio::test]
async fn a_strip_repair_restamps_the_estimate_on_the_unary_path() {
    // Arrange: the carried variant draws the proven replay rejection, so the
    // strip repair re-dispatches the smaller payload.
    let provider = Arc::new(RecordingReplayMock::new());
    let router = router_with(provider.clone());
    let carried = req_carrying_bulky_artifact();

    // Act
    let dispatched = router
        .complete_with_options(carried.clone(), RouterOptions::default())
        .await;

    // Assert
    assert!(dispatched.result.is_ok(), "the stripped repair succeeds");
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "sanity: one carried attempt plus one stripped repair",
    );
    assert_estimate_describes_dispatched_payload(
        dispatched.meta.calib_estimated_tokens,
        &provider.last_dispatched(),
        &carried,
    );
}

#[tokio::test]
async fn a_proactive_strip_restamps_the_estimate_on_the_unary_path() {
    // Arrange: a first request commits the negative, so the next request is
    // stripped BEFORE its first attempt -- no rejection involved.
    let provider = Arc::new(RecordingReplayMock::new());
    let router = router_with(provider.clone());
    let carried = req_carrying_bulky_artifact();
    assert!(
        router.complete(carried.clone()).await.is_ok(),
        "the first request repairs and commits the negative",
    );

    // Act
    let dispatched = router
        .complete_with_options(carried.clone(), RouterOptions::default())
        .await;

    // Assert
    assert!(dispatched.result.is_ok());
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        3,
        "sanity: the second request strips proactively -- one call, no carry",
    );
    assert_estimate_describes_dispatched_payload(
        dispatched.meta.calib_estimated_tokens,
        &provider.last_dispatched(),
        &carried,
    );
}

#[tokio::test]
async fn a_strip_repair_restamps_the_estimate_on_the_streaming_path() {
    // Arrange: the streaming arm is separate code from the unary arm and has
    // its own strip site.
    let provider = Arc::new(RecordingReplayMock::new());
    let router = router_with(provider.clone());
    let carried = req_carrying_bulky_artifact();

    // Act
    let dispatched = router
        .stream_with_options(carried.clone(), RouterOptions::default())
        .await;

    // Assert
    assert!(dispatched.result.is_ok(), "the stripped repair streams");
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "sanity: one carried attempt plus one stripped repair",
    );
    assert_estimate_describes_dispatched_payload(
        dispatched.meta.calib_estimated_tokens,
        &provider.last_dispatched(),
        &carried,
    );
}

#[tokio::test]
async fn a_proactive_strip_restamps_the_estimate_on_the_streaming_path() {
    // Arrange: a first stream commits the negative on first content, so the
    // next stream is stripped before its first attempt.
    let provider = Arc::new(RecordingReplayMock::new());
    let router = router_with(provider.clone());
    let carried = req_carrying_bulky_artifact();
    assert!(
        router
            .stream_with_options(carried.clone(), RouterOptions::default())
            .await
            .result
            .is_ok(),
        "the first stream repairs and commits the negative",
    );

    // Act
    let dispatched = router
        .stream_with_options(carried.clone(), RouterOptions::default())
        .await;

    // Assert
    assert!(dispatched.result.is_ok());
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        3,
        "sanity: the second stream strips proactively -- one call, no carry",
    );
    assert_estimate_describes_dispatched_payload(
        dispatched.meta.calib_estimated_tokens,
        &provider.last_dispatched(),
        &carried,
    );
}

#[tokio::test]
async fn a_dispatch_that_strips_nothing_keeps_the_original_stamp() {
    // Arrange: no reasoning artifact at all -- the overwhelmingly common
    // shape. Nothing is stripped, so nothing is re-estimated: the stamp
    // `record_would_trim` produced is the one that persists.
    let provider = Arc::new(RecordingReplayMock::new());
    let router = router_with(provider.clone());
    let plain = ChatRequest {
        model: "m1".into(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        }]
        .into(),
        ..Default::default()
    };

    // Act
    let dispatched = router
        .complete_with_options(plain, RouterOptions::default())
        .await;

    // Assert: one call, and the stamp still equals the dispatched payload's
    // estimate.
    assert!(dispatched.result.is_ok());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        dispatched.meta.calib_estimated_tokens,
        Some(estimate_total_tokens(&provider.last_dispatched())),
    );
}
