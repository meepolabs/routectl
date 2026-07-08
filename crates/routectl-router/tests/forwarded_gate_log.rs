//! Structured-log safety for the ROUTER-side forwarded-passthrough gate.
//!
//! When a forwarded (pure-proxy) request resolves to a non-Anthropic
//! target, the router refuses BEFORE dispatch and emits ONE WARN. This
//! test pins the operator-grep contract: the WARN carries SAFE
//! dimensions only (`reason`, `credential_source`, `provider_kind`) and
//! NEVER the forwarded token -- in a field, in the message, or anywhere.
//!
//! Lives in its own integration-test binary (not the router lib's unit
//! tests) on purpose: a thread-local capture subscriber over a shared
//! `warn!` callsite is unreliable inside the 600+-test lib binary,
//! because sibling tests hit the same callsite under the default
//! `NoSubscriber` first and poison tracing's global per-callsite
//! `Interest` cache. In a dedicated binary the callsite is only ever
//! evaluated under this capture subscriber. Mirrors the capture pattern
//! in `factory_context_management_warning.rs`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use routectl_core::error::Result;
use routectl_core::schema::ForwardedBearer;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Provider, TokenCount};
use routectl_router::{AliasValue, Config, ProviderEntry, ResolvedModel, Router};
use tracing::field::{Field, Visit};

/// The forwarded token. Distinctive so any leak into a log field, the
/// log message, or the client error is unmistakable.
const FORWARDED_TOKEN: &str = "sk-ant-oat01-FORWARDED-SECRET-must-never-surface";

/// A provider that must never be reached: the gate refuses a forwarded
/// non-Anthropic target BEFORE dispatch, so any call here is a bug.
struct NeverDispatched {
    id: String,
}

#[async_trait]
impl Provider for NeverDispatched {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        unreachable!("gate must refuse before any provider interaction")
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        unreachable!("gate must refuse before any provider interaction")
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!("gate must refuse before dispatch")
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("gate must refuse before dispatch")
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        unreachable!("gate must refuse before dispatch")
    }
}

/// Router whose alias `"alias"` resolves to a single non-Anthropic
/// (`openai-compat`) target -- a forwarded request must be refused.
fn router_with_non_anthropic_target() -> Router {
    let mut config = Config::default();
    config.providers.insert(
        "compat-prov".to_string(),
        ProviderEntry::openai_compat("https://placeholder.invalid/v1", "literal:k"),
    );
    config.aliases.insert(
        "alias".to_string(),
        AliasValue::Chain(vec!["compat".to_string()]),
    );

    let provider: Arc<dyn Provider> = Arc::new(NeverDispatched {
        id: "never-dispatched".to_string(),
    });
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "compat".to_string(),
        Arc::new(ResolvedModel::new(
            "compat",
            "compat-prov",
            provider,
            "upstream-compat",
        )),
    );

    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

fn forwarded_req() -> ChatRequest {
    let mut req = ChatRequest {
        model: "alias".into(),
        ..Default::default()
    };
    req.routectl_internal.forwarded_bearer =
        Some(ForwardedBearer::new(FORWARDED_TOKEN.to_string()));
    req
}

// ---- tracing capture ----

#[derive(Debug, Clone)]
struct CapturedEvent {
    level: tracing::Level,
    message: String,
    fields: Vec<(String, String)>,
}

#[derive(Default)]
struct FieldCollector {
    message: String,
    fields: Vec<(String, String)>,
}

impl Visit for FieldCollector {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push((field.name().into(), value.into()));
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let s = format!("{value:?}");
        if field.name() == "message" {
            self.message = s.trim_matches('"').to_string();
        } else {
            self.fields.push((field.name().into(), s));
        }
    }
}

#[derive(Default)]
struct CaptureSubscriber {
    captured: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl tracing::Subscriber for CaptureSubscriber {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let meta = event.metadata();
        let mut visitor = FieldCollector::default();
        event.record(&mut visitor);
        if let Ok(mut guard) = self.captured.lock() {
            guard.push(CapturedEvent {
                level: *meta.level(),
                message: visitor.message,
                fields: visitor.fields,
            });
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Drive `fut` under a thread-local capture subscriber and return its
/// output plus the captured events. `#[tokio::test]` defaults to a
/// current_thread runtime, so the subscriber spans the whole future.
async fn with_capture<F, T>(fut: F) -> (T, Vec<CapturedEvent>)
where
    F: std::future::Future<Output = T>,
{
    let captured: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber {
        captured: captured.clone(),
    };
    let _guard = tracing::subscriber::set_default(subscriber);
    let out = fut.await;
    let events = captured.lock().expect("capture lock poisoned").clone();
    (out, events)
}

#[tokio::test]
async fn refuse_logs_safe_dimensions_and_never_the_token() {
    let router = router_with_non_anthropic_target();

    let (result, events) = with_capture(router.complete(forwarded_req())).await;

    // The request was refused (mapped to HTTP 400 at the ingress).
    assert!(
        result.is_err(),
        "forwarded non-anthropic target must be refused"
    );

    // Exactly the refusal WARN, carrying SAFE dimensions only.
    let refusals: Vec<&CapturedEvent> = events
        .iter()
        .filter(|e| {
            e.fields
                .iter()
                .any(|(k, v)| k == "reason" && v == "non_anthropic_target")
        })
        .collect();
    assert_eq!(
        refusals.len(),
        1,
        "expected exactly one refusal WARN; got: {events:?}"
    );
    let refuse = refusals[0];
    assert_eq!(refuse.level, tracing::Level::WARN);

    let field = |name: &str| {
        refuse
            .fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(field("reason"), Some("non_anthropic_target"));
    assert_eq!(field("credential_source"), Some("forwarded"));
    assert_eq!(
        field("provider_kind"),
        Some("openai-compat"),
        "provider_kind is a safe dimension identifying the refused kind",
    );

    // The forwarded token must NOT appear in ANY captured event -- not in
    // a field value, not in the message.
    for e in &events {
        assert!(
            !e.message.contains(FORWARDED_TOKEN),
            "log message leaked the forwarded token: {}",
            e.message
        );
        for (k, v) in &e.fields {
            assert!(
                !v.contains(FORWARDED_TOKEN),
                "log field `{k}` leaked the forwarded token: {v}"
            );
        }
    }
}
