//! End-to-end coverage for a lane that rejects a forced `tool_choice`: the
//! captured rejection learns `forced_tool_choice` through the real dispatch
//! error arm, the next forced request is routed away from that lane rather
//! than having its directive removed, and an operator `unsupported` override
//! routes away before any dispatch.

use super::*;
use std::collections::BTreeMap;
use std::sync::Arc;

use futures::stream::BoxStream;
use parking_lot::Mutex;
#[cfg(feature = "bedrock")]
use routectl_core::capability::FailurePhase;
use routectl_core::capability::SignalTier;
use routectl_core::{ChatChunk, ChatResponse, Provider, Result, ToolDef};
use serde_json::{Value, json};

use crate::config::Config;
use crate::resolved::ResolvedModel;
use crate::router::RouterOptions;

const FORCED_TOOL_CHOICE: &str = "forced_tool_choice";

const FIXTURE: &str = include_str!("../../tests/fixtures/forced_tool_choice_capture.json");

fn fixture() -> Value {
    serde_json::from_str(FIXTURE).expect("valid forced tool_choice fixture")
}

fn fixture_str(fx: &Value, section: &str, key: &str) -> String {
    fx[section][key]
        .as_str()
        .unwrap_or_else(|| panic!("fixture string {section}.{key}"))
        .to_string()
}

/// The upstream rejection a lane answers with, verbatim from the capture.
#[derive(Clone)]
struct Rejection {
    body: String,
    upstream_type: Option<String>,
}

/// A lane that records the `tool_choice` of every request it receives and
/// either rejects it with a fixed captured 400 or succeeds.
struct RecordingLane {
    id: &'static str,
    rejection: Option<Rejection>,
    seen: Mutex<Vec<Option<Value>>>,
}

impl RecordingLane {
    fn rejecting(id: &'static str, rejection: Rejection) -> Arc<Self> {
        Arc::new(Self {
            id,
            rejection: Some(rejection),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn accepting(id: &'static str) -> Arc<Self> {
        Arc::new(Self {
            id,
            rejection: None,
            seen: Mutex::new(Vec::new()),
        })
    }

    fn calls(&self) -> usize {
        self.seen.lock().len()
    }

    fn seen(&self) -> Vec<Option<Value>> {
        self.seen.lock().clone()
    }
}

#[async_trait::async_trait]
impl Provider for RecordingLane {
    fn id(&self) -> &str {
        self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(self.id, "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.seen.lock().push(req.tool_choice.clone());
        match &self.rejection {
            Some(rejection) => Err(Error::upstream_full(
                self.id,
                400,
                rejection.body.clone(),
                None,
                rejection.upstream_type.clone(),
                None,
            )),
            None => Ok(ChatResponse {
                model: req.model,
                ..Default::default()
            }),
        }
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream(self.id, 500, "unused"))
    }
}

const ALIAS: &str = "forced-alias";

/// `m1` on provider `p1` (of `p1_kind_block`), `m2` on an openai-compat `p2`,
/// chained `[m1, m2]` under [`ALIAS`].
fn two_lane_router(
    p1_kind_block: &str,
    extra_toml: &str,
    m1: Arc<RecordingLane>,
    m2: Arc<RecordingLane>,
) -> Router {
    let toml_text = format!(
        "[providers.p1]\n{p1_kind_block}\n\
         [providers.p2]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://example.test/v1\"\n\
         api_key_ref = \"literal:k\"\n\
         [aliases]\n\
         \"{ALIAS}\" = [\"m1\", \"m2\"]\n\
         {extra_toml}"
    );
    let config: Config = toml::from_str(&toml_text).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m1".to_string(),
        Arc::new(ResolvedModel::new("m1", "p1", m1, "wire-model-1")),
    );
    models.insert(
        "m2".to_string(),
        Arc::new(ResolvedModel::new("m2", "p2", m2, "wire-model-2")),
    );
    router.install_resolved_models(models);
    router
}

const ANTHROPIC_KIND_BLOCK: &str = "kind = \"anthropic-api\"\n";

#[cfg(feature = "bedrock")]
const BEDROCK_KIND_BLOCK: &str = "kind = \"bedrock\"\n\
     region = \"us-east-1\"\n\
     creds = { kind = \"default-chain\" }\n";

fn forced_request(tool_choice: Value) -> ChatRequest {
    ChatRequest {
        model: ALIAS.into(),
        messages: vec![].into(),
        tools: Some(vec![ToolDef::Other(json!({
            "type": "function",
            "function": {"name": "lookup", "parameters": {"type": "object"}}
        }))]),
        tool_choice: Some(tool_choice),
        ..Default::default()
    }
}

fn anthropic_rejection(fx: &Value) -> Rejection {
    Rejection {
        body: fixture_str(fx, "anthropic_api", "body"),
        upstream_type: Some("invalid_request_error".into()),
    }
}

#[cfg(feature = "bedrock")]
fn bedrock_rejection(fx: &Value, body_key: &str) -> Rejection {
    Rejection {
        body: fixture_str(fx, "bedrock", body_key),
        upstream_type: Some(fixture_str(fx, "bedrock", "lifted_type")),
    }
}

#[cfg(feature = "bedrock")]
#[tokio::test]
async fn bedrock_rejection_learns_and_next_forced_request_skips_the_lane() {
    for (body_key, tool_choice) in [
        ("converse_body", json!({"type": "any"})),
        ("invoke_body", json!({"type": "tool", "name": "lookup"})),
    ] {
        // Arrange
        let fx = fixture();
        let m1 = RecordingLane::rejecting("p1", bedrock_rejection(&fx, body_key));
        let m2 = RecordingLane::accepting("p2");
        let router = two_lane_router(BEDROCK_KIND_BLOCK, "", m1.clone(), m2.clone());

        // Act: the first forced request hits the rejecting lane and falls back.
        let first = router
            .complete_with_options(
                forced_request(tool_choice.clone()),
                RouterOptions::default(),
            )
            .await;

        // Assert: one self-identifying negative on the rejecting lane.
        assert!(
            first.result.is_ok(),
            "{body_key}: the fallback lane serves it"
        );
        let learned = &first.meta.learned_capabilities;
        assert_eq!(learned.len(), 1, "{body_key}");
        assert_eq!(learned[0].state_key, "m1");
        assert_eq!(learned[0].capability_key, FORCED_TOOL_CHOICE);
        assert_eq!(learned[0].signal_tier, SignalTier::SelfIdentifying);
        assert_eq!(learned[0].phase, FailurePhase::F1);

        // Act: the next forced request.
        let second = router
            .complete_with_options(
                forced_request(tool_choice.clone()),
                RouterOptions::default(),
            )
            .await;

        // Assert: routed to the accepting lane first; the rejecting lane is
        // not reached again, nothing was stripped, and every lane saw the
        // caller's directive unchanged.
        assert!(second.result.is_ok(), "{body_key}");
        assert_eq!(m1.calls(), 1, "{body_key}: the learned lane is demoted");
        assert_eq!(m2.calls(), 2, "{body_key}");
        assert_eq!(router.metrics.strip_total(), 0, "{body_key}");
        for seen in m1.seen().into_iter().chain(m2.seen()) {
            assert_eq!(seen.as_ref(), Some(&tool_choice), "{body_key}");
        }
    }
}

#[tokio::test]
async fn anthropic_rejection_learns_once_corroborated_and_then_skips_the_lane() {
    // Arrange
    let fx = fixture();
    let m1 = RecordingLane::rejecting("p1", anthropic_rejection(&fx));
    let m2 = RecordingLane::accepting("p2");
    let router = two_lane_router(ANTHROPIC_KIND_BLOCK, "", m1.clone(), m2.clone());
    let tool_choice = json!({"type": "any"});

    // Act: two forced requests; the inferred phrase acts on corroboration.
    let first = router
        .complete_with_options(
            forced_request(tool_choice.clone()),
            RouterOptions::default(),
        )
        .await;
    let second = router
        .complete_with_options(
            forced_request(tool_choice.clone()),
            RouterOptions::default(),
        )
        .await;

    // Assert: the first observation is pending, the second acts.
    assert!(first.result.is_ok());
    assert!(first.meta.learned_capabilities.is_empty());
    assert!(second.result.is_ok());
    let learned = &second.meta.learned_capabilities;
    assert_eq!(learned.len(), 1);
    assert_eq!(learned[0].state_key, "m1");
    assert_eq!(learned[0].capability_key, FORCED_TOOL_CHOICE);
    assert_eq!(learned[0].signal_tier, SignalTier::Inferred);

    // Act: a third forced request.
    let third = router
        .complete_with_options(
            forced_request(tool_choice.clone()),
            RouterOptions::default(),
        )
        .await;

    // Assert: the rejecting lane is no longer reached and the directive was
    // never removed on the way to either lane.
    assert!(third.result.is_ok());
    assert_eq!(m1.calls(), 2);
    assert_eq!(m2.calls(), 3);
    assert_eq!(router.metrics.strip_total(), 0);
    for seen in m1.seen().into_iter().chain(m2.seen()) {
        assert_eq!(seen.as_ref(), Some(&tool_choice));
    }
}

#[tokio::test]
async fn an_unforced_request_on_a_learned_lane_still_reaches_it() {
    // Arrange: learn the negative on m1 from two forced requests.
    let fx = fixture();
    let m1 = RecordingLane::rejecting("p1", anthropic_rejection(&fx));
    let m2 = RecordingLane::accepting("p2");
    let router = two_lane_router(ANTHROPIC_KIND_BLOCK, "", m1.clone(), m2.clone());
    for _ in 0..2 {
        let _ = router
            .complete_with_options(
                forced_request(json!({"type": "any"})),
                RouterOptions::default(),
            )
            .await;
    }
    assert_eq!(m1.calls(), 2);

    // Act: an auto request derives no forced key.
    let _ = router
        .complete_with_options(forced_request(json!("auto")), RouterOptions::default())
        .await;

    // Assert: the learned negative does not demote a request that forces
    // nothing, so m1 is tried first again.
    assert_eq!(m1.calls(), 3);
}

#[tokio::test]
async fn unsupported_override_routes_forced_requests_away_before_dispatch() {
    // Arrange: an operator override on m1, which would otherwise accept.
    let m1 = RecordingLane::accepting("p1");
    let m2 = RecordingLane::accepting("p2");
    let router = two_lane_router(
        ANTHROPIC_KIND_BLOCK,
        "[capability.overrides.\"p1:m1\"]\nunsupported = [\"forced_tool_choice\"]\n",
        m1.clone(),
        m2.clone(),
    );
    let tool_choice = json!("required");

    // Act
    let forced = router
        .complete_with_options(
            forced_request(tool_choice.clone()),
            RouterOptions::default(),
        )
        .await;

    // Assert: m1 is never dispatched; m2 serves the directive unchanged.
    assert!(forced.result.is_ok());
    assert_eq!(m1.calls(), 0);
    assert_eq!(m2.seen(), vec![Some(tool_choice)]);

    // Act: an unforced request is untouched by the override.
    let unforced = router
        .complete_with_options(forced_request(json!("auto")), RouterOptions::default())
        .await;

    // Assert
    assert!(unforced.result.is_ok());
    assert_eq!(m1.calls(), 1);
}
