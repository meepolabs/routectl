//! Integration coverage for the router reasoning-replay strip-repair arm:
//! the fixed correctness branch that turns a proven replay rejection into a
//! single stripped in-request retry, drives the two-phase learn (commit on a
//! stripped success, release on a repeat or unrelated failure), strips
//! proactively once a negative persists, and leaves an ordinary bad request
//! on the ordinary path.
//!
//! Each test drives a REAL `complete` dispatch through a mock provider that
//! answers the m3 replay-rejection fixture on the carried variant and
//! succeeds on the stripped one, counting calls so the additive
//! (never multiplicative, never nested) call bound is asserted directly.

use super::super::Router;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::stream::BoxStream;
use routectl_core::{
    CODEX_OAUTH, ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, MessageContent,
    OPENAI_RESPONSES_V1, Provider, ReasoningDetail, ReasoningDetailKind, Result, Role,
};
use serde_json::json;

use crate::config::{AliasValue, Config};
use crate::resolved::ResolvedModel;

/// The pinned m3 replay-rejection body, byte-exact. The replay-fixture
/// corpus is gitignored and never ships, so this is an inline constant; it
/// carries no secret.
const M3_BODY: &str = r#"{"error":{"code":"validation_error","message":"encrypted content missing recognized prefix (expected `rsn_` or `smry_`)","param":null,"type":"invalid_request_error"}}"#;

/// A generic 400 that is NOT a replay rejection: its tokens match no proven
/// signature, so the classifier keeps it a plain bad request.
const GENERIC_400_BODY: &str =
    r#"{"error":{"type":"invalid_request_error","message":"malformed request"}}"#;

/// A mock openai-responses backend whose lane is mantle. It answers the m3
/// replay rejection on any request that still carries a reasoning artifact
/// and succeeds once the artifacts are stripped, so a repair round trip is
/// exactly two counted provider calls. `always_reject` keeps rejecting even
/// the stripped variant (the repeat-rejection release path); `generic_400`
/// answers a non-replay 400 regardless (the ordinary path).
struct ReplayMockProvider {
    calls: AtomicUsize,
    always_reject: bool,
    generic_400: bool,
}

impl ReplayMockProvider {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            always_reject: false,
            generic_400: false,
        }
    }

    fn always_reject() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            always_reject: true,
            generic_400: false,
        }
    }

    fn generic_400() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            always_reject: false,
            generic_400: true,
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
        if self.generic_400 {
            return Err(Error::upstream("replay-mock", 400, GENERIC_400_BODY));
        }
        let carries_artifact = req
            .messages
            .iter()
            .any(|message| !message.reasoning_details.is_empty());
        if self.always_reject || carries_artifact {
            return Err(Error::upstream("replay-mock", 400, M3_BODY));
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

/// An assistant turn echoing one reasoning artifact of the given format tag.
fn assistant_with_artifact(format: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: MessageContent::Text("prior answer".into()),
        reasoning: None,
        reasoning_details: vec![ReasoningDetail {
            kind: ReasoningDetailKind::Encrypted,
            id: Some("rs_1".into()),
            format: Some(format.to_string()),
            index: None,
            payload: json!({"encrypted_content": "opaque"}),
        }],
        name: None,
        tool_call_id: None,
        tool_calls: None,
        refusal: None,
    }
}

fn req_carrying(format: &str) -> ChatRequest {
    ChatRequest {
        model: "m1".into(),
        messages: vec![assistant_with_artifact(format)].into(),
        ..Default::default()
    }
}

/// Single openai-responses target `p1`/`m1`, the lane the mock declares
/// mantle. `openai-responses` is the fixture-backed kind the replay
/// classifier gates on.
const SINGLE_TARGET_TOML: &str = r#"
[providers.p1]
kind = "openai-responses"
api_key_ref = "literal:k"
auth_kind = "api-key"
"#;

/// Parse `toml_text` and install `provider` under nickname `m1` on provider
/// `p1`, mirroring the shared single-leg fixture.
fn router_from_toml(toml_text: &str, provider: Arc<dyn Provider>) -> Router {
    let config: Config = toml::from_str(toml_text).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m1".to_string(),
        Arc::new(ResolvedModel::new("m1", "p1", provider, "wire-model")),
    );
    router.install_resolved_models(models);
    router
}

/// A two-entry fallback chain `m1 -> m2`, each on its own openai-responses
/// provider instance so per-target call counts are independent.
fn router_two_target_chain(p1: Arc<dyn Provider>, p2: Arc<dyn Provider>) -> Router {
    let toml_text = r#"
[providers.p1]
kind = "openai-responses"
api_key_ref = "literal:k"
auth_kind = "api-key"

[providers.p2]
kind = "openai-responses"
api_key_ref = "literal:k"
auth_kind = "api-key"
"#;
    let mut config: Config = toml::from_str(toml_text).expect("valid test toml");
    config.aliases.insert(
        "chain".into(),
        AliasValue::Chain(vec!["m1".into(), "m2".into()]),
    );
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m1".into(),
        Arc::new(ResolvedModel::new("m1", "p1", p1, "wire-1")),
    );
    models.insert(
        "m2".into(),
        Arc::new(ResolvedModel::new("m2", "p2", p2, "wire-2")),
    );
    router.install_resolved_models(models);
    router
}

fn req_chain_carrying(format: &str) -> ChatRequest {
    ChatRequest {
        model: "chain".into(),
        messages: vec![assistant_with_artifact(format)].into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn codex_blob_toward_mantle_repairs_in_exactly_two_calls() {
    // Arrange -- a codex artifact carried toward the mantle lane.
    let provider = Arc::new(ReplayMockProvider::new());
    let router = router_from_toml(SINGLE_TARGET_TOML, provider.clone());

    // Act
    let result = router.complete(req_carrying(CODEX_OAUTH)).await;

    // Assert -- carried variant rejected, stripped variant succeeded:
    // exactly two provider calls, and the negative was persisted.
    assert!(result.is_ok(), "the stripped repair must succeed");
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "one carried attempt plus one stripped repair",
    );
    assert!(
        !router.learned_capability_snapshot().is_empty(),
        "a stripped success commits the learned negative",
    );
}

#[tokio::test]
async fn repair_fires_once_per_target_and_never_nests_across_fallback() {
    // Arrange -- both targets reject even the stripped variant, so the
    // repair cannot succeed and the chain is walked. A nested or stacked
    // repair would multiply the per-target call count.
    let p1 = Arc::new(ReplayMockProvider::always_reject());
    let p2 = Arc::new(ReplayMockProvider::always_reject());
    let router = router_two_target_chain(
        p1.clone() as Arc<dyn Provider>,
        p2.clone() as Arc<dyn Provider>,
    );

    // Act
    let result = router.complete(req_chain_carrying(CODEX_OAUTH)).await;

    // Assert -- each target: one carried attempt + exactly one repair =
    // two calls, additive and not multiplied by any retry, then fallback.
    assert!(
        result.is_err(),
        "no target could serve the stripped variant"
    );
    assert_eq!(
        p1.calls.load(Ordering::SeqCst),
        2,
        "first target: carried + one repair, never nested",
    );
    assert_eq!(
        p2.calls.load(Ordering::SeqCst),
        2,
        "second target repairs independently, additive across the hop",
    );
    // Repeated rejection releases the provisional state: nothing persisted.
    assert!(
        router.learned_capability_snapshot().is_empty(),
        "a repair that hit the same rejection persists no negative",
    );
}

#[tokio::test]
async fn unrelated_failure_during_carry_releases_without_negative() {
    // Arrange -- the carried variant draws a generic 400 (not a replay
    // rejection). The repair branch is never entered; the provisional
    // state is released.
    let provider = Arc::new(ReplayMockProvider::generic_400());
    let router = router_from_toml(SINGLE_TARGET_TOML, provider.clone());

    // Act
    let result = router.complete(req_carrying(CODEX_OAUTH)).await;

    // Assert -- one call (ordinary bad-request path), no repair, no
    // persisted negative.
    assert!(result.is_err());
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        1,
        "a generic 400 takes the ordinary path -- no stripped repair",
    );
    assert!(
        router.learned_capability_snapshot().is_empty(),
        "an unrelated failure learns nothing",
    );
}

#[tokio::test]
async fn generic_400_is_not_classified_as_a_replay_rejection() {
    // A generic 400 must not enter the repair arm even though the request
    // carried an artifact: proven by the single call and the untouched
    // learned registry (covered together with the release path above, but
    // pinned here as its own behavior).
    let provider = Arc::new(ReplayMockProvider::generic_400());
    let router = router_from_toml(SINGLE_TARGET_TOML, provider.clone());

    let result = router.complete(req_carrying(CODEX_OAUTH)).await;

    assert!(result.is_err());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn persisted_negative_strips_proactively_with_no_carried_attempt() {
    // Arrange -- first request repairs and commits the negative.
    let provider = Arc::new(ReplayMockProvider::new());
    let router = router_from_toml(SINGLE_TARGET_TOML, provider.clone());
    let first = router.complete(req_carrying(CODEX_OAUTH)).await;
    assert!(first.is_ok());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);

    // Act -- a later request on the same (scheme, target_lane).
    let second = router.complete(req_carrying(CODEX_OAUTH)).await;

    // Assert -- the acting negative forces a proactive strip: the stripped
    // variant is dispatched directly, so only ONE additional call fires and
    // there is no carried attempt.
    assert!(second.is_ok(), "the proactively stripped request succeeds");
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        3,
        "the second request strips proactively -- one call, no carry",
    );
}

#[tokio::test]
async fn ambiguous_tag_toward_mantle_is_stripped_by_the_ladder() {
    // SECURITY: an artifact whose ambiguous compatibility tag claims nothing
    // provable about the mantle lane is treated as non-portable by the
    // deterministic ladder (never by any tag-borne claim) -- so the repair
    // strips it and the request still succeeds.
    let provider = Arc::new(ReplayMockProvider::new());
    let router = router_from_toml(SINGLE_TARGET_TOML, provider.clone());

    let result = router.complete(req_carrying(OPENAI_RESPONSES_V1)).await;

    assert!(result.is_ok(), "the ladder strips the ambiguous artifact");
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "carried attempt rejected, stripped repair succeeded",
    );
}
