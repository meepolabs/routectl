//! Shared fixtures for [`super::provider_remap_tests`] and
//! [`super::bedrock_class_remap_tests`]: both exercise the
//! per-provider status remap (`[providers.X.class_overrides]`)
//! through the REAL `Config` TOML path against a single-leg
//! `p1`/`m1` router, so the fixture that builds that router and the
//! provider that fails it on demand live here once instead of
//! twice.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A provider that always fails with a fixed status, counting calls
/// so a test can pin exactly how many times the SAME provider was
/// dispatched -- the direct behavioral proof that a same-provider
/// retry did or did not fire.
pub(super) struct CountingFailingProvider {
    pub(super) id: String,
    pub(super) status: u16,
    pub(super) calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Provider for CountingFailingProvider {
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
        Err(Error::upstream(&self.id, self.status, "body"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream(&self.id, self.status, "body"))
    }
}

/// Parse `toml_text` through the real `Config` deserialize path (so
/// `[providers.p1.class_overrides]` / `[retry.classes]` genuinely
/// exercise their adapters), install `provider` under nickname `m1`
/// on provider `p1`, and return the resulting `Router`.
pub(super) fn router_from_toml(toml_text: &str, provider: Arc<dyn Provider>) -> Router {
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

pub(super) fn req_m1() -> ChatRequest {
    ChatRequest {
        model: "m1".into(),
        messages: vec![].into(),
        ..Default::default()
    }
}

pub(super) fn find_decision(
    events: &[routectl_testkit::CapturedEvent],
) -> &routectl_testkit::CapturedEvent {
    events
        .iter()
        .find(|e| e.message == "router failure class decision")
        .expect("a class-decision event must fire")
}
