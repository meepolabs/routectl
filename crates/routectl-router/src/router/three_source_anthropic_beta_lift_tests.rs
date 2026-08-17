//! Integration: pin that ingress + provider + model anthropic-beta
//! all union onto the upstream request on both dispatch paths.
use super::*;
use crate::config::{ProviderEntry, ProviderRuntimePolicy};
use crate::resolved::ResolvedModel;
use crate::router::Router;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use parking_lot::Mutex as ParkingMutex;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, Provider, Result,
};
use std::collections::BTreeMap;
use std::sync::Arc;

struct CapturingProvider {
    id: String,
    captured: Arc<ParkingMutex<Vec<ChatRequest>>>,
}

#[async_trait]
impl Provider for CapturingProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(&self.id, "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        let model = req.model.clone();
        self.captured.lock().push(req);
        Ok(ChatResponse {
            id: "ok".into(),
            model,
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
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.captured.lock().push(req);
        let s = futures::stream::once(async move {
            Ok(ChatChunk {
                id: "c0".into(),
                model: "x".into(),
                choices: vec![routectl_core::ChunkChoice {
                    index: 0,
                    delta: routectl_core::ChunkDelta {
                        content: Some("ok".into()),
                        ..Default::default()
                    },
                    finish_reason: None,
                    matched_stop_sequence: None,
                }],
                usage: None,
                opaque_events: Vec::new(),
                upstream_meta: None,
            })
        });
        Ok(s.boxed())
    }
}

fn router_with_capture(
    provider_betas: Option<&str>,
    model_betas: Option<&str>,
) -> (Router, Arc<ParkingMutex<Vec<ChatRequest>>>) {
    let mut config = Config::default();
    // Provider-side `header_extras`.
    let mut provider_headers: BTreeMap<String, String> = BTreeMap::new();
    if let Some(v) = provider_betas {
        provider_headers.insert("anthropic-beta".into(), v.to_string());
    }
    config.providers.insert(
        "anthropic".into(),
        ProviderEntry::OpenaiCompat {
            base_url: "https://placeholder.invalid/v1".into(),
            api_key_ref: "literal:k".into(),
            header_extras: provider_headers,
            payload_extras: None,
            user_agent: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            auto_emit_per_block_breakpoints: None,
            reduction_enabled: None,
            #[cfg(feature = "bedrock")]
            bedrock_mantle: None,
            runtime: ProviderRuntimePolicy::default(),
        },
    );

    let mut router = Router::new(Arc::new(config));
    let captured: Arc<ParkingMutex<Vec<ChatRequest>>> = Arc::new(ParkingMutex::new(Vec::new()));
    let provider: Arc<dyn Provider> = Arc::new(CapturingProvider {
        id: "cap".into(),
        captured: captured.clone(),
    });
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    let mut resolved = ResolvedModel::new("haiku", "anthropic", provider, "claude-haiku-4-5");
    if let Some(v) = model_betas {
        let mut h = BTreeMap::new();
        h.insert("anthropic-beta".into(), v.to_string());
        resolved = resolved.with_header_extras(h);
    }
    models.insert("haiku".into(), Arc::new(resolved));
    router.install_resolved_models(models);
    (router, captured)
}

#[tokio::test]
async fn complete_path_unions_three_sources() {
    // ingress: "foo", provider: "claude-code-20250219,oauth-2025-04-20",
    // model: "context-1m-2025-08-07" -- all unioned.
    let (router, captured) = router_with_capture(
        Some("claude-code-20250219,oauth-2025-04-20"),
        Some("context-1m-2025-08-07"),
    );
    let req = ChatRequest {
        model: "haiku".into(),
        messages: vec![].into(),
        anthropic_beta: vec!["foo".into()],
        ..Default::default()
    };
    router.complete(req).await.expect("ok");
    let captured = captured.lock();
    let upstream = captured.first().expect("one upstream call");
    assert_eq!(
        upstream.anthropic_beta,
        vec![
            "foo".to_string(),
            "claude-code-20250219".to_string(),
            "oauth-2025-04-20".to_string(),
            "context-1m-2025-08-07".to_string(),
        ]
    );
}

#[tokio::test]
async fn stream_path_unions_three_sources() {
    let (router, captured) =
        router_with_capture(Some("oauth-2025-04-20"), Some("context-1m-2025-08-07"));
    let req = ChatRequest {
        model: "haiku".into(),
        messages: vec![].into(),
        anthropic_beta: vec![],
        ..Default::default()
    };
    let _ = router
        .stream(req)
        .await
        .expect("ok")
        .collect::<Vec<_>>()
        .await;
    let captured = captured.lock();
    let upstream = captured.first().expect("one upstream call");
    assert_eq!(
        upstream.anthropic_beta,
        vec![
            "oauth-2025-04-20".to_string(),
            "context-1m-2025-08-07".to_string(),
        ]
    );
}
