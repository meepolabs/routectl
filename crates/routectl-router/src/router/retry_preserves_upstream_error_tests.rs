//! A retry that re-gates (auth-401 refresh, reasoning-replay strip repair)
//! must preserve the GENUINE upstream error across its `continue`. If the
//! re-gate then refuses (breaker open / RPM exhausted), the client must see
//! the real upstream error, not the synthetic status-0 gate error. Complete
//! and stream paths are pinned symmetrically for both retry branches.

use super::*;
use crate::config::{Config, ProviderEntry, ProviderRuntimePolicy};
use crate::resolved::ResolvedModel;
use async_trait::async_trait;
use futures::stream::BoxStream;
use routectl_core::Result;
use routectl_core::{
    CODEX_OAUTH, ChatChunk, ChatRequest, ChatResponse, Error, Provider, ReasoningDetail,
    ReasoningDetailKind, Role,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The pinned replay-rejection body (byte-exact, carries no secret),
/// mirroring `replay_repair_tests`. An openai-responses 400 with this body
/// classifies as a replay rejection, entering the strip-repair arm.
const REPLAY_REJECT_BODY: &str = r#"{"error":{"code":"validation_error","message":"encrypted content missing recognized prefix (expected `rsn_` or `smry_`)","param":null,"type":"invalid_request_error"}}"#;

/// A distinctive fragment that stands in for the reasoning-artifact blob a
/// replay-rejection envelope can echo in its variable message tail. It must
/// NEVER survive into the error handed back through the generic
/// upstream-error path -- `replay_rejection_body_free` strips it.
const BLOB_MARKER: &str = "REASONING_BLOB_MUST_NOT_LEAK";

/// The replay-rejection body with a blob marker appended to the message
/// TAIL. The message still opens with the classifier's anchor prefix
/// (`encrypted content missing recognized prefix`), so it classifies as a
/// replay rejection and enters the strip-repair arm, while the marker rides
/// in the body that `replay_rejection_body_free` must drop.
const REPLAY_REJECT_BODY_WITH_BLOB: &str = r#"{"error":{"code":"validation_error","message":"encrypted content missing recognized prefix (expected `rsn_` or `smry_`) REASONING_BLOB_MUST_NOT_LEAK","param":null,"type":"invalid_request_error"}}"#;

/// Provider that returns a 401 on complete + stream-open (auth class -- no
/// breaker debit) and refreshes successfully, so the router takes the
/// auth-retry `continue`. Counts calls so the test can assert the re-gate
/// refuses BEFORE a second dispatch.
struct Auth401Provider {
    id: String,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for Auth401Provider {
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
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream(&self.id, 401, "stale token"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream(&self.id, 401, "stale token"))
    }
    async fn on_auth_failure(&self) -> Result<()> {
        Ok(())
    }
}

/// Provider that returns the replay-rejection 400 on complete + stream so
/// the strip-repair arm fires and `continue`s. `replay_lane = Mantle` +
/// openai-responses kind are the classifier's gate for the replay class.
struct ReplayReject400Provider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for ReplayReject400Provider {
    fn id(&self) -> &'static str {
        "replay-reject"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("replay-reject", "unused"))
    }
    fn replay_lane(&self) -> routectl_core::ReplayScheme {
        routectl_core::ReplayScheme::Mantle
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream("replay-reject", 400, REPLAY_REJECT_BODY))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream("replay-reject", 400, REPLAY_REJECT_BODY))
    }
}

/// Same replay-rejection shape as [`ReplayReject400Provider`] but the body
/// carries the blob marker in its message tail, so a re-gate refusal that
/// preserves the raw error would leak the marker unless it is stored
/// body-free.
struct ReplayRejectBlobProvider {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for ReplayRejectBlobProvider {
    fn id(&self) -> &'static str {
        "replay-reject"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("replay-reject", "unused"))
    }
    fn replay_lane(&self) -> routectl_core::ReplayScheme {
        routectl_core::ReplayScheme::Mantle
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream(
            "replay-reject",
            400,
            REPLAY_REJECT_BODY_WITH_BLOB,
        ))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream(
            "replay-reject",
            400,
            REPLAY_REJECT_BODY_WITH_BLOB,
        ))
    }
}

/// Single-target OpenaiCompat router whose provider allows exactly one
/// request per minute (`rpm_limit = 1`). The first attempt consumes the only
/// token; a retry's re-gate is RPM-refused, exercising the mask.
fn rpm1_router(provider: Arc<dyn Provider>) -> Router {
    let mut config = Config::default();
    config.providers.insert(
        "p".into(),
        ProviderEntry::OpenaiCompat {
            base_url: "https://placeholder.invalid/v1".into(),
            api_key_ref: "literal:k".into(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            auto_emit_per_block_breakpoints: None,
            reduction_enabled: None,
            #[cfg(feature = "bedrock")]
            bedrock_mantle: None,
            runtime: ProviderRuntimePolicy {
                rpm_limit: Some(1),
                ..Default::default()
            },
        },
    );
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m".into(),
        Arc::new(ResolvedModel::new("m", "p", provider, "u")),
    );
    router.install_resolved_models(models);
    router
}

/// Single openai-responses target with `rpm_limit = 1`, so a replay-repair
/// re-dispatch is RPM-refused on its re-gate.
fn rpm1_responses_router(provider: Arc<dyn Provider>) -> Router {
    let toml_text = r#"
[providers.p1]
kind = "openai-responses"
api_key_ref = "literal:k"
auth_kind = "api-key"
rpm_limit = 1
"#;
    let config: Config = toml::from_str(toml_text).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m1".into(),
        Arc::new(ResolvedModel::new("m1", "p1", provider, "wire-model")),
    );
    router.install_resolved_models(models);
    router
}

fn plain_req() -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: vec![].into(),
        ..Default::default()
    }
}

/// A request carrying one reasoning artifact toward the mantle lane, so the
/// router builds a replay plan and the replay-rejection 400 enters the
/// strip-repair arm.
fn artifact_req() -> ChatRequest {
    let message = routectl_core::Message {
        role: Role::Assistant,
        content: routectl_core::MessageContent::Text("prior answer".into()),
        reasoning: None,
        reasoning_details: vec![ReasoningDetail {
            kind: ReasoningDetailKind::Encrypted,
            id: Some("rs_1".into()),
            format: Some(CODEX_OAUTH.to_string()),
            index: None,
            payload: json!({"encrypted_content": "opaque"}),
        }],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    };
    ChatRequest {
        model: "m1".into(),
        messages: vec![message].into(),
        ..Default::default()
    }
}

fn assert_is_the_real_error(err: &Error, want_status: u16, forbidden_substr: &str) {
    match err {
        Error::Upstream { status, .. } => assert_eq!(
            *status, want_status,
            "must surface the genuine upstream status, got {status}"
        ),
        other => panic!("expected an upstream error, got {other:?}"),
    }
    let msg = err.to_string();
    assert!(
        !msg.contains(forbidden_substr),
        "must NOT surface the synthetic gate error, got: {msg}"
    );
}

#[tokio::test]
async fn complete_auth_retry_regate_refusal_surfaces_401_not_rpm_error() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(Auth401Provider {
        id: "p".into(),
        calls: calls.clone(),
    });
    let router = rpm1_router(provider);

    let err = router
        .complete(plain_req())
        .await
        .expect_err("401 then rpm-exhausted re-gate must Err");
    assert_is_the_real_error(&err, 401, "rpm_limit");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the re-gate refuses before a second dispatch",
    );
}

#[tokio::test]
async fn stream_auth_retry_regate_refusal_surfaces_401_not_rpm_error() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(Auth401Provider {
        id: "p".into(),
        calls: calls.clone(),
    });
    let router = rpm1_router(provider);

    let err = router
        .stream(plain_req())
        .await
        .err()
        .expect("401 then rpm-exhausted re-gate must Err on the stream path");
    assert_is_the_real_error(&err, 401, "rpm_limit");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the re-gate refuses first");
}

#[tokio::test]
async fn complete_replay_repair_regate_refusal_surfaces_upstream_not_rpm_error() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(ReplayReject400Provider {
        calls: calls.clone(),
    });
    let router = rpm1_responses_router(provider);

    let err = router
        .complete(artifact_req())
        .await
        .expect_err("replay rejection then rpm-exhausted re-gate must Err");
    assert_is_the_real_error(&err, 400, "rpm_limit");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the stripped re-dispatch is rpm-refused before reaching the provider",
    );
}

#[tokio::test]
async fn stream_replay_repair_regate_refusal_surfaces_upstream_not_rpm_error() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(ReplayReject400Provider {
        calls: calls.clone(),
    });
    let router = rpm1_responses_router(provider);

    let err = router
        .stream(artifact_req())
        .await
        .err()
        .expect("replay rejection then rpm-exhausted re-gate must Err on the stream path");
    assert_is_the_real_error(&err, 400, "rpm_limit");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the re-gate refuses first");
}

#[tokio::test]
async fn complete_replay_repair_regate_refusal_is_body_free_not_blob() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(ReplayRejectBlobProvider {
        calls: calls.clone(),
    });
    let router = rpm1_responses_router(provider);

    let err = router
        .complete(artifact_req())
        .await
        .expect_err("replay rejection then rpm-exhausted re-gate must Err");
    // Still the genuine replay-rejection class/status (400, not the
    // synthetic status-0 gate error) yet body-free: the blob marker the
    // rejection echoed must not survive the re-gate refusal.
    assert_is_the_real_error(&err, 400, BLOB_MARKER);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the stripped re-dispatch is rpm-refused before reaching the provider",
    );
}

#[tokio::test]
async fn stream_replay_repair_regate_refusal_is_body_free_not_blob() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(ReplayRejectBlobProvider {
        calls: calls.clone(),
    });
    let router = rpm1_responses_router(provider);

    let err = router
        .stream(artifact_req())
        .await
        .err()
        .expect("replay rejection then rpm-exhausted re-gate must Err on the stream path");
    assert_is_the_real_error(&err, 400, BLOB_MARKER);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the re-gate refuses first");
}
