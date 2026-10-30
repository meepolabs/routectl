//! The request observation slot survives the per-attempt `routectl_internal`
//! rebuild, so every chain attempt bumps the one slot the caller kept.
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use parking_lot::Mutex;
use routectl_core::{
    ChatChunk, ChatResponse, Choice, Error, Message, MessageContent, Provider, RequestObservation,
    Result, Role,
};
use serde_json::json;

use super::*;
use crate::config::{AliasValue, RetryPolicy};
use crate::resolved::ResolvedModel;
use crate::router::Router;
use crate::router::chain::into_one_dispatch_target;

type Captured = Arc<Mutex<Vec<ChatRequest>>>;

/// Records every request it is handed and bumps the slot it carries: a
/// failing entry adds one wire loss and answers a fallbackable 503, a serving
/// entry adds one local refusal and answers.
struct BumpingProvider {
    captured: Captured,
    fails: bool,
}

#[async_trait]
impl Provider for BumpingProvider {
    fn id(&self) -> &'static str {
        "bumping"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("bumping", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        if let Some(slot) = &req.routectl_internal.observation {
            if self.fails {
                slot.add_wire_losses(1);
            } else {
                slot.add_local_refusal();
            }
        }
        let model = req.model.clone();
        self.captured.lock().push(req);
        if self.fails {
            return Err(Error::upstream("bumping", 503, "down; fall back"));
        }
        Ok(ok_response(model))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!()
    }
}

fn ok_response(model: String) -> ChatResponse {
    ChatResponse {
        id: "ok".into(),
        model,
        created: 0,
        choices: vec![Choice {
            logprobs: None,
            index: 0,
            message: Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("ok".into()),
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
    }
}

#[test]
fn apply_layered_overlays_carries_the_observation_slot() {
    let model = Arc::new(ResolvedModel::new(
        "nick",
        "test-prov",
        Arc::new(BumpingProvider {
            captured: Captured::default(),
            fails: false,
        }),
        "claude-x",
    ));
    let target = into_one_dispatch_target(model);
    let kept = RequestObservation::new();
    let mut req = ChatRequest::default();
    req.routectl_internal.observation = Some(kept.clone());

    for attempt in 1..=2 {
        apply_layered_overlays(&Config::default(), &target, &mut req);
        let carried = req
            .routectl_internal
            .observation
            .as_ref()
            .unwrap_or_else(|| panic!("attempt {attempt}: slot must be carried"));
        carried.add_local_refusal();
        assert_eq!(
            kept.snapshot().local_refusals,
            attempt,
            "attempt {attempt}: a bump through the carried slot reaches the kept one",
        );
    }
}

#[test]
fn apply_layered_overlays_leaves_an_absent_slot_absent() {
    let model = Arc::new(ResolvedModel::new(
        "nick",
        "test-prov",
        Arc::new(BumpingProvider {
            captured: Captured::default(),
            fails: false,
        }),
        "claude-x",
    ));
    let target = into_one_dispatch_target(model);
    let mut req = ChatRequest::default();

    apply_layered_overlays(&Config::default(), &target, &mut req);

    assert!(req.routectl_internal.observation.is_none());
}

/// Two-entry chain: the first entry 503s, the second serves.
fn two_entry_chain() -> (Router, Captured, Captured) {
    let mut config = Config {
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            ..Default::default()
        },
        ..Config::default()
    };
    config.aliases.insert(
        "flow".into(),
        AliasValue::Chain(vec!["entry1".into(), "entry2".into()]),
    );
    let first = Captured::default();
    let second = Captured::default();
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    for (nick, captured, fails) in [("entry1", &first, true), ("entry2", &second, false)] {
        let provider: Arc<dyn Provider> = Arc::new(BumpingProvider {
            captured: Arc::clone(captured),
            fails,
        });
        models.insert(
            nick.into(),
            Arc::new(ResolvedModel::new(nick, "p", provider, "upstream")),
        );
    }
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    (router, first, second)
}

#[tokio::test]
async fn fallback_attempt_bumps_the_slot_the_caller_kept() {
    let (router, first, second) = two_entry_chain();
    let kept = RequestObservation::new();
    let mut req = ChatRequest {
        model: "flow".into(),
        ..Default::default()
    };
    req.routectl_internal.observation = Some(kept.clone());

    router.complete(req).await.expect("the second entry serves");

    assert_eq!(first.lock().len(), 1, "the first entry is tried once");
    let second = second.lock();
    assert_eq!(second.len(), 1, "the chain falls back to the second entry");
    let snap = kept.snapshot();
    assert_eq!(snap.wire_losses, 1, "the first attempt's bump is kept");
    assert_eq!(
        snap.local_refusals, 1,
        "the fallback attempt's bump is kept"
    );
    let fallback_slot = second[0]
        .routectl_internal
        .observation
        .as_ref()
        .expect("the fallback attempt carries the slot");
    fallback_slot.add_missing_terminal();
    assert_eq!(
        kept.snapshot().missing_terminals,
        1,
        "the fallback attempt's slot is a clone of the kept one",
    );
}
