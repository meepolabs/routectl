//! The client's native `thinking` object survives the per-attempt
//! `routectl_internal` rebuild, so a fallback target sees the same object
//! (the same allocation, not a copy) the first target saw, and a repair
//! one target makes to its copy never reaches the next.
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use parking_lot::Mutex;
use routectl_core::{
    AnthropicThinking, ChatChunk, ChatResponse, Choice, Error, Message, MessageContent, Provider,
    Result, Role, ThinkingObject,
};
use serde_json::json;

use super::*;
use crate::config::{AliasValue, RetryPolicy};
use crate::resolved::ResolvedModel;
use crate::router::Router;
use crate::router::chain::into_one_dispatch_target;

type Captured = Arc<Mutex<Vec<ChatRequest>>>;

/// Records every request it is handed, then fails with a fallbackable 503
/// when `fails` is set, or serves a minimal response. With `repairs_type`
/// set, it first rewrites the `type` of the carrier it received, the way a
/// per-target repair does.
struct RecordingProvider {
    captured: Captured,
    fails: bool,
    repairs_type: Option<&'static str>,
}

#[async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str {
        "recording"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("recording", "unused"))
    }
    async fn complete(&self, mut req: ChatRequest) -> Result<ChatResponse> {
        if let (Some(repaired), AnthropicThinking::Sent(object)) = (
            self.repairs_type,
            &mut req.routectl_internal.anthropic_thinking,
        ) {
            object.set_thinking_type(repaired);
        }
        let model = req.model.clone();
        self.captured.lock().push(req);
        if self.fails {
            return Err(Error::upstream("recording", 503, "down; fall back"));
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

fn sent_thinking() -> ThinkingObject {
    ThinkingObject::from_client(json!({
        "type": "adaptive",
        "between_tools": true,
    }))
    .expect("an object builds a carrier")
}

/// Asserts `carried` is `Sent` and shares `original`'s allocation.
fn assert_same_carrier(carried: &AnthropicThinking, original: &ThinkingObject, label: &str) {
    let AnthropicThinking::Sent(object) = carried else {
        panic!("{label}: carrier must stay Sent, got {carried:?}");
    };
    assert!(
        std::ptr::eq(object.as_object(), original.as_object()),
        "{label}: carrier must be the same Arc, not a copy or a rebuild",
    );
}

#[test]
fn apply_layered_overlays_preserves_anthropic_thinking() {
    let model = Arc::new(ResolvedModel::new(
        "nick",
        "test-prov",
        Arc::new(RecordingProvider {
            captured: Captured::default(),
            fails: false,
            repairs_type: None,
        }),
        "claude-x",
    ));
    let target = into_one_dispatch_target(model);
    let original = sent_thinking();
    let mut req = ChatRequest::default();
    req.routectl_internal.anthropic_thinking = AnthropicThinking::Sent(original.clone());

    for attempt in 1..=2 {
        apply_layered_overlays(&Config::default(), &target, &mut req);
        assert_same_carrier(
            &req.routectl_internal.anthropic_thinking,
            &original,
            &format!("overlay rebuild attempt {attempt}"),
        );
    }
}

#[test]
fn apply_layered_overlays_preserves_sent_none() {
    let model = Arc::new(ResolvedModel::new(
        "nick",
        "test-prov",
        Arc::new(RecordingProvider {
            captured: Captured::default(),
            fails: false,
            repairs_type: None,
        }),
        "claude-x",
    ));
    let target = into_one_dispatch_target(model);
    let mut req = ChatRequest::default();
    req.routectl_internal.anthropic_thinking = AnthropicThinking::SentNone;

    for attempt in 1..=2 {
        apply_layered_overlays(&Config::default(), &target, &mut req);
        assert_eq!(
            req.routectl_internal.anthropic_thinking,
            AnthropicThinking::SentNone,
            "\"client sent no thinking\" must not reset to NotCaptured (attempt {attempt})",
        );
    }
}

/// Two-entry chain: the first entry applies `first_repair` (when set) and
/// 503s, the second serves.
fn two_entry_chain(first_repair: Option<&'static str>) -> (Router, Captured, Captured) {
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
    for (nick, captured, fails, repairs_type) in [
        ("entry1", &first, true, first_repair),
        ("entry2", &second, false, None),
    ] {
        let provider: Arc<dyn Provider> = Arc::new(RecordingProvider {
            captured: Arc::clone(captured),
            fails,
            repairs_type,
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
async fn fallback_attempt_receives_the_same_thinking_carrier() {
    let (router, first, second) = two_entry_chain(None);
    let original = sent_thinking();
    let mut req = ChatRequest {
        model: "flow".into(),
        ..Default::default()
    };
    req.routectl_internal.anthropic_thinking = AnthropicThinking::Sent(original.clone());

    router.complete(req).await.expect("the second entry serves");

    let first = first.lock();
    let second = second.lock();
    assert_eq!(first.len(), 1, "the first entry is tried once");
    assert_eq!(second.len(), 1, "the chain falls back to the second entry");
    assert_same_carrier(
        &first[0].routectl_internal.anthropic_thinking,
        &original,
        "first chain attempt",
    );
    assert_same_carrier(
        &second[0].routectl_internal.anthropic_thinking,
        &original,
        "second chain attempt",
    );
}

#[tokio::test]
async fn first_attempt_repair_does_not_reach_the_fallback_attempt() {
    let (router, first, second) = two_entry_chain(Some("enabled"));
    let original = sent_thinking();
    let mut req = ChatRequest {
        model: "flow".into(),
        ..Default::default()
    };
    req.routectl_internal.anthropic_thinking = AnthropicThinking::Sent(original.clone());

    router.complete(req).await.expect("the second entry serves");

    let first = first.lock();
    let second = second.lock();
    assert_eq!(first.len(), 1, "the first entry is tried once");
    assert_eq!(second.len(), 1, "the chain falls back to the second entry");
    let AnthropicThinking::Sent(repaired) = &first[0].routectl_internal.anthropic_thinking else {
        panic!("first chain attempt: carrier must stay Sent");
    };
    assert_eq!(
        repaired.thinking_type(),
        Some("enabled"),
        "the first entry's repair must take effect on its own copy",
    );
    let AnthropicThinking::Sent(fallback) = &second[0].routectl_internal.anthropic_thinking else {
        panic!("second chain attempt: carrier must stay Sent");
    };
    assert_eq!(
        fallback.thinking_type(),
        Some("adaptive"),
        "the fallback attempt must see the client's original type",
    );
    assert_same_carrier(
        &second[0].routectl_internal.anthropic_thinking,
        &original,
        "second chain attempt",
    );
    assert_eq!(
        original.thinking_type(),
        Some("adaptive"),
        "the caller's carrier is untouched",
    );
}
