//! Regression: `req.reasoning` passes through dispatch unchanged
//! when no operator overlay applies. The merge step is gone; the
//! caller's reasoning config must arrive at the egress unmodified.
use super::*;
use crate::resolved::ResolvedModel;
use crate::router::Router;
use async_trait::async_trait;
use futures::stream::BoxStream;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, Provider, ReasoningConfig, Result,
};
use std::sync::{Arc, Mutex};

struct CapturingProvider {
    captured: Arc<Mutex<Vec<ChatRequest>>>,
}

#[async_trait]
impl Provider for CapturingProvider {
    fn id(&self) -> &'static str {
        "capturing"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("capturing", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.captured.lock().unwrap().push(req);
        Ok(ChatResponse {
            id: "ok".into(),
            model: "m".into(),
            created: 0,
            choices: vec![Choice {
                logprobs: None,
                index: 0,
                message: Message {
                    refusal: None,
                    role: routectl_core::Role::Assistant,
                    content: routectl_core::MessageContent::Text("ok".into()),
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
        })
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!()
    }
}

fn router_with_capturing(provider: Arc<dyn Provider>) -> Router {
    let cfg = Arc::new(Config::default());
    let mut router = Router::new(cfg);
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m".to_string(),
        Arc::new(ResolvedModel::new("m", "p", provider, "upstream-model")),
    );
    router.install_resolved_models(models);
    router
}

#[tokio::test]
async fn caller_reasoning_passes_through_dispatch_unchanged() {
    // When the caller supplies a ReasoningConfig and no operator
    // merge step applies, the egress must see the caller's
    // reasoning field verbatim. The merge step is gone; nothing
    // in the dispatch path should modify req.reasoning.
    let captured: Arc<Mutex<Vec<ChatRequest>>> = Arc::new(Mutex::new(vec![]));
    let provider = Arc::new(CapturingProvider {
        captured: captured.clone(),
    });
    let router = router_with_capturing(provider);

    let caller_reasoning = ReasoningConfig {
        effort: Some("medium".into()),
        enabled: Some(true),
        max_tokens: Some(4096),
        exclude: Some(false),
    };
    let req = ChatRequest {
        model: "m".into(),
        messages: vec![].into(),
        reasoning: Some(caller_reasoning.clone()),
        ..Default::default()
    };
    router.complete(req).await.expect("dispatch succeeded");

    let calls = captured.lock().unwrap();
    let upstream = calls.first().expect("one upstream call");
    let got = upstream.reasoning.as_ref().expect("reasoning preserved");
    assert_eq!(got.effort, caller_reasoning.effort, "effort unchanged");
    assert_eq!(got.enabled, caller_reasoning.enabled, "enabled unchanged");
    assert_eq!(
        got.max_tokens, caller_reasoning.max_tokens,
        "max_tokens unchanged"
    );
    assert_eq!(got.exclude, caller_reasoning.exclude, "exclude unchanged");
}
