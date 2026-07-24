//! When the LAST chain entry is gate-refused (breaker open /
//! RPM) but an EARLIER entry produced a real upstream error, the
//! client must see the real error, not the synthetic "circuit
//! breaker open" gate error. The fix keeps the first real error in
//! `last_err` instead of overwriting it with the gate error.
use super::*;
use crate::config::{AliasValue, Config, ProviderEntry, ProviderRuntimePolicy, RetryPolicy};
use crate::resolved::ResolvedModel;
use async_trait::async_trait;
use futures::stream::BoxStream;
use routectl_core::Result;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider};
use std::collections::BTreeMap;

/// Provider that fails both complete + stream-open with a real,
/// fallbackable 503 carrying a distinctive message.
struct Real503Provider {
    id: String,
}

#[async_trait]
impl Provider for Real503Provider {
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
        Err(Error::upstream(&self.id, 503, "real upstream down"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream(&self.id, 503, "real upstream down"))
    }
}

/// Provider for the second chain entry. Its breaker is force-opened
/// before dispatch, so its body is never reached -- the gate refuses
/// first.
struct UnreachedProvider {
    id: String,
}

#[async_trait]
impl Provider for UnreachedProvider {
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
        unreachable!("gate must refuse entry2 before its body runs")
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("gate must refuse entry2 before its body runs")
    }
}

/// Build a router with a two-entry chain `flow = [entry1, entry2]`.
/// entry1 fails 503; entry2 has a breaker and is force-opened so its
/// gate refuses. Global retry is capped at one attempt so entry1
/// fails fast without burning backoff sleeps.
fn router_with_two_entry_chain() -> Router {
    let mut config = Config {
        retry: RetryPolicy {
            max_attempts: 1,
            ..RetryPolicy::default()
        },
        ..Config::default()
    };
    config.aliases.insert(
        "flow".into(),
        AliasValue::Chain(vec!["entry1".into(), "entry2".into()]),
    );
    config.providers.insert(
        "p2".into(),
        ProviderEntry::OpenaiCompat {
            base_url: "https://placeholder.invalid/v1".into(),
            api_key_ref: "literal:k".into(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
            #[cfg(feature = "bedrock")]
            bedrock_mantle: None,
            runtime: ProviderRuntimePolicy {
                circuit_failures: Some(1),
                circuit_cooldown_ms: Some(60_000),
                ..Default::default()
            },
        },
    );

    let mut router = Router::new(Arc::new(config));
    let p1: Arc<dyn Provider> = Arc::new(Real503Provider { id: "p1".into() });
    let p2: Arc<dyn Provider> = Arc::new(UnreachedProvider { id: "p2".into() });
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "entry1".into(),
        Arc::new(ResolvedModel::new("entry1", "p1", p1, "u1")),
    );
    models.insert(
        "entry2".into(),
        Arc::new(ResolvedModel::new("entry2", "p2", p2, "u2")),
    );
    router.install_resolved_models(models);
    // Force entry2's breaker open so its gate refuses on dispatch.
    assert!(
        router.force_open_breaker("entry2", std::time::Duration::from_hours(1)),
        "entry2 breaker must be force-open-able",
    );
    router
}

#[tokio::test]
async fn complete_surfaces_real_error_not_gate_error() {
    let router = router_with_two_entry_chain();
    let req = ChatRequest {
        model: "flow".into(),
        messages: vec![],
        ..Default::default()
    };
    let err = router
        .complete(req)
        .await
        .expect_err("both entries unavailable -> Err");
    let msg = err.to_string();
    assert!(
        msg.contains("real upstream down"),
        "must surface entry1's real 503, got: {msg}"
    );
    assert!(
        !msg.contains("circuit breaker open"),
        "must NOT surface entry2's synthetic gate error, got: {msg}"
    );
}

#[tokio::test]
async fn stream_surfaces_real_error_not_gate_error() {
    let router = router_with_two_entry_chain();
    let req = ChatRequest {
        model: "flow".into(),
        messages: vec![],
        ..Default::default()
    };
    let err = router
        .stream(req)
        .await
        .err()
        .expect("both entries unavailable -> Err");
    let msg = err.to_string();
    assert!(
        msg.contains("real upstream down"),
        "stream must surface entry1's real 503, got: {msg}"
    );
    assert!(
        !msg.contains("circuit breaker open"),
        "stream must NOT surface entry2's synthetic gate error, got: {msg}"
    );
}
