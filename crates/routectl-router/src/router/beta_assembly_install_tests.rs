//! Every provider call on every walk carries a fresh beta assembly slot, and
//! the router's handle reads what the provider deposited on it, whether or
//! not the request carries a captured ingress body.

use super::*;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::stream::{BoxStream, StreamExt};
use parking_lot::Mutex;
use routectl_core::{
    BetaAssembly, ChatChunk, ChatResponse, ChunkChoice, ChunkDelta, Error, Provider, Result,
    TokenCount,
};
use serde_json::json;

use crate::config::Config;
use crate::resolved::ResolvedModel;
use crate::router::RouterOptions;

const TOML: &str = r#"
[retry]
max_attempts = 2
initial_backoff_ms = 0
jitter_ms = 0

[providers.p1]
kind = "anthropic-api"
api_key_ref = "literal:k"
"#;

/// What a provider call saw on its beta assembly slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SlotSeen {
    Missing,
    Fresh,
    Stale,
}

/// A provider that fails its first call with `first_status`, succeeds after,
/// and on every call records what its slot held and then deposits the call's
/// ordinal as a floor flag.
struct DepositingProvider {
    first_status: u16,
    calls: AtomicUsize,
    seen: Mutex<Vec<SlotSeen>>,
    handles: Mutex<Vec<routectl_core::BetaAssemblyReport>>,
}

impl DepositingProvider {
    fn new(first_status: u16) -> Arc<Self> {
        Arc::new(Self {
            first_status,
            calls: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            handles: Mutex::new(Vec::new()),
        })
    }

    fn play(&self, req: &ChatRequest) -> Result<()> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let seen = match req.routectl_internal.beta_assembly.as_ref() {
            None => SlotSeen::Missing,
            Some(slot) => {
                let held = slot.take();
                let seen = if held.is_none() {
                    SlotSeen::Fresh
                } else {
                    SlotSeen::Stale
                };
                slot.record(deposit(n));
                self.handles.lock().push(slot.clone());
                seen
            }
        };
        self.seen.lock().push(seen);
        if n == 0 {
            return Err(Error::upstream("p1", self.first_status, "upstream refused"));
        }
        Ok(())
    }
}

fn deposit(call: usize) -> BetaAssembly {
    BetaAssembly::new(vec![format!("call-{call}")], Vec::new())
}

fn content_chunk() -> ChatChunk {
    ChatChunk {
        id: "c1".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some("ok".into()),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
}

#[async_trait::async_trait]
impl Provider for DepositingProvider {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p1", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.play(&req)?;
        Ok(ChatResponse {
            model: req.model,
            ..Default::default()
        })
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.play(&req)?;
        Ok(futures::stream::iter(vec![Ok(content_chunk())]).boxed())
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.play(&req)?;
        Ok(TokenCount::default())
    }
}

fn router_with(provider: Arc<dyn Provider>) -> Router {
    let config: Config = toml::from_str(TOML).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m1".to_string(),
        Arc::new(ResolvedModel::new(
            "m1",
            "p1",
            provider,
            "claude-wire-model",
        )),
    );
    router.install_resolved_models(models);
    router
}

fn request() -> ChatRequest {
    let req = ChatRequest {
        model: "m1".into(),
        messages: vec![].into(),
        anthropic_beta: vec!["client-flag-2099-01-01".into()],
        ..Default::default()
    };
    assert!(
        req.routectl_internal.ingress_wire.is_none(),
        "premise: no captured ingress body, so no wire tap is installed"
    );
    req
}

#[derive(Debug, Clone, Copy)]
enum Walk {
    Complete,
    Stream,
    CountTokens,
}

async fn dispatch(router: &Router, walk: Walk) -> bool {
    match walk {
        Walk::Complete => router
            .complete_with_options(request(), RouterOptions::default())
            .await
            .result
            .is_ok(),
        Walk::Stream => router
            .stream_with_options(request(), RouterOptions::default())
            .await
            .result
            .is_ok(),
        Walk::CountTokens => router
            .count_tokens_with_meta(request())
            .await
            .result
            .is_ok(),
    }
}

#[tokio::test]
async fn every_provider_call_gets_its_own_fresh_beta_assembly_slot() {
    // (walk, status of the first call that makes the walk call again)
    let rows = [
        (Walk::Complete, 500),
        (Walk::Stream, 401),
        (Walk::CountTokens, 401),
    ];
    for (walk, first_status) in rows {
        let provider = DepositingProvider::new(first_status);
        let router = router_with(provider.clone());

        assert!(dispatch(&router, walk).await, "{walk:?}: dispatch succeeds");

        assert_eq!(
            *provider.seen.lock(),
            vec![SlotSeen::Fresh, SlotSeen::Fresh],
            "{walk:?}: each call carries a slot no earlier call wrote"
        );
        let handles = provider.handles.lock();
        assert_eq!(
            handles[1].take(),
            Some(deposit(1)),
            "{walk:?}: second call's deposit"
        );
        assert_eq!(
            handles[0].take(),
            Some(deposit(0)),
            "{walk:?}: first call's deposit"
        );
    }
}
