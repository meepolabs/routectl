//! Observability coverage for reasoning-replay degradation: the single
//! aggregated per-request WARN, its closed-set token payload, and the
//! blob-leak audit across the WARN, the TRACE lines, and the generic
//! retry/fallback error path. Modeled on `observability_seam_tests`: a
//! mock provider carrying sentinel reasoning bytes and upstream body,
//! captured events / lines scanned so no blob, id, or body ever surfaces.
//!
//! `super::` is the `dispatch` module (this file is `mod ... ;` there), so
//! the degradation event/token consts resolve as `super::*`.

use super::super::Router;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::stream::BoxStream;
use routectl_core::{
    CODEX_OAUTH, ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, MessageContent,
    Provider, ReasoningDetail, ReasoningDetailKind, Result, Role,
};
use routectl_testkit::{capture_lines, with_capture};
use serde_json::json;

use crate::config::Config;
use crate::resolved::ResolvedModel;

/// Sentinel reasoning-artifact bytes: the blob and the reasoning item id.
/// Neither may surface in the WARN or in any captured log line.
const REASONING_BLOB: &str = "SECRET-REASONING-BLOB-DO-NOT-LOG";
const REASONING_ID: &str = "rs_SECRET_ITEM_ID_DO_NOT_LOG";
/// A distinctive substring of the pinned replay-rejection body. The
/// upstream body must never reach a generic log line once the rejection is
/// classified and converted to a body-free structured error.
const BODY_MARKER: &str = "encrypted content missing recognized prefix";

/// The pinned replay-rejection body, byte-exact and secret-free.
const REPLAY_REJECT_BODY: &str = r#"{"error":{"code":"validation_error","message":"encrypted content missing recognized prefix (expected `rsn_` or `smry_`)","param":null,"type":"invalid_request_error"}}"#;

/// A mock openai-responses backend on the mantle lane. It answers the
/// replay rejection whenever the request still carries a reasoning artifact
/// and succeeds once stripped. `always_reject` keeps rejecting even the
/// stripped variant (the repeat-rejection generic-error path).
struct ReplayMockProvider {
    calls: AtomicUsize,
    always_reject: bool,
}

impl ReplayMockProvider {
    fn repairing() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            always_reject: false,
        }
    }

    fn always_reject() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            always_reject: true,
        }
    }
}

#[async_trait::async_trait]
impl Provider for ReplayMockProvider {
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        let carries_artifact = req
            .messages
            .iter()
            .any(|message| !message.reasoning_details.is_empty());
        if self.always_reject || carries_artifact {
            return Err(Error::upstream("replay-mock", 400, REPLAY_REJECT_BODY));
        }
        Ok(success_response())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream("replay-mock", 500, "unused"))
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

/// An assistant turn echoing one codex-tagged reasoning artifact whose id
/// and encrypted payload are sentinels.
fn assistant_with_sentinel_artifact() -> Message {
    Message {
        role: Role::Assistant,
        content: MessageContent::Text("prior answer".into()),
        reasoning: None,
        reasoning_details: vec![ReasoningDetail {
            kind: ReasoningDetailKind::Encrypted,
            id: Some(REASONING_ID.into()),
            format: Some(CODEX_OAUTH.to_string()),
            index: None,
            payload: json!({ "encrypted_content": REASONING_BLOB }),
        }],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    }
}

fn req_carrying_artifact() -> ChatRequest {
    ChatRequest {
        model: "m1".into(),
        messages: vec![assistant_with_sentinel_artifact()].into(),
        ..Default::default()
    }
}

fn req_no_artifact() -> ChatRequest {
    ChatRequest {
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

/// Count the aggregated degradation WARN events among captured events.
fn degrade_warns(
    events: &[routectl_testkit::CapturedEvent],
) -> Vec<&routectl_testkit::CapturedEvent> {
    events
        .iter()
        .filter(|e| e.message == super::REPLAY_DEGRADE_EVENT)
        .collect()
}

#[tokio::test]
async fn degradation_emits_exactly_one_warn_with_closed_tokens() {
    // Arrange: a codex artifact carried toward the mantle lane repairs.
    let provider = Arc::new(ReplayMockProvider::repairing());
    let router = router_with(provider.clone());

    // Act
    let (result, events) = with_capture(router.complete(req_carrying_artifact())).await;

    // Assert: the stripped repair succeeded, and EXACTLY one aggregated
    // degradation WARN fired for the whole request.
    assert!(result.is_ok(), "the stripped repair must succeed");
    let warns = degrade_warns(&events);
    assert_eq!(warns.len(), 1, "exactly one degradation WARN per request");
    let ev = warns[0];
    assert_eq!(ev.level, tracing::Level::WARN);
    assert_eq!(ev.field("action"), Some("strip_repair"));
    assert_eq!(ev.field("target_lane"), Some("mantle"));
    assert_eq!(ev.field("source_schemes"), Some("codex"));
    assert_eq!(ev.field("reason"), Some("upstream_replay_rejection"));
    assert_eq!(ev.field("artifact_count"), Some("1"));
    assert_eq!(ev.field("repair_attempted"), Some("true"));
    assert_eq!(ev.field("repair_succeeded"), Some("true"));
    assert_eq!(ev.field("learned"), Some("true"));
    assert!(
        ev.field("state_key").is_some_and(|s| !s.is_empty()),
        "the sanitized state key rides the WARN"
    );
}

#[tokio::test]
async fn no_degradation_emits_zero_warns() {
    // Arrange: a request that carries no reasoning artifact never enters the
    // repair branch, so nothing degrades.
    let provider = Arc::new(ReplayMockProvider::repairing());
    let router = router_with(provider.clone());

    // Act
    let (result, events) = with_capture(router.complete(req_no_artifact())).await;

    // Assert: served on the first call, and NOT one degradation WARN.
    assert!(result.is_ok());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        degrade_warns(&events).len(),
        0,
        "no WARN when nothing degrades"
    );
}

#[tokio::test]
async fn warn_carries_no_blob_id_or_body() {
    // Arrange
    let provider = Arc::new(ReplayMockProvider::repairing());
    let router = router_with(provider);

    // Act
    let (result, events) = with_capture(router.complete(req_carrying_artifact())).await;

    // Assert: the WARN's message and every field are free of the artifact
    // blob, the reasoning item id, and the upstream body.
    assert!(result.is_ok());
    let warns = degrade_warns(&events);
    assert_eq!(warns.len(), 1);
    let ev = warns[0];
    for probe in [REASONING_BLOB, REASONING_ID, BODY_MARKER] {
        assert!(!ev.message.contains(probe), "WARN message leaked: {probe}");
        for (k, v) in &ev.fields {
            assert!(!v.contains(probe), "WARN field {k} leaked: {probe}");
        }
    }
}

#[tokio::test]
async fn trace_across_repair_path_leaks_no_blob_id_or_body() {
    // Arrange: both variants reject (repeat rejection), so the classified
    // rejection flows through the generic retry/fallback logs that
    // debug-render the Error. Capture EVERY line at TRACE.
    let provider = Arc::new(ReplayMockProvider::always_reject());
    let router = router_with(provider);

    // Act
    let (result, lines) = capture_lines(router.complete(req_carrying_artifact())).await;

    // Assert: the request failed, and no captured line at ANY level carries
    // the artifact blob, the reasoning item id, or the upstream body.
    assert!(result.is_err());
    for line in &lines {
        for probe in [REASONING_BLOB, REASONING_ID, BODY_MARKER] {
            assert!(!line.contains(probe), "TRACE line leaked {probe}: {line}");
        }
    }
}

#[tokio::test]
async fn classified_replay_rejection_is_body_free_before_generic_logs() {
    // Arrange: the stripped repair also draws the replay rejection, so the
    // classified replay rejection reaches the generic error logs. Without
    // the body-free conversion the replay-rejection body would render into
    // `error = ?e`.
    let provider = Arc::new(ReplayMockProvider::always_reject());
    let router = router_with(provider.clone());

    // Act
    let (result, lines) = capture_lines(router.complete(req_carrying_artifact())).await;

    // Assert: exactly two calls (carried + one repair), and the upstream
    // body never appears in any rendered line.
    assert!(result.is_err());
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "carried attempt plus one stripped repair",
    );
    assert!(
        lines.iter().all(|line| !line.contains(BODY_MARKER)),
        "the classified replay rejection must be body-free before generic logs",
    );
}
