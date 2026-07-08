//! Structured-log safety for the f2.09 forwarded-token terminal path.
//!
//! When a forwarded (pure-passthrough) request draws an upstream
//! 401/403/429, the router surfaces it VERBATIM -- no on_auth_failure
//! refresh, no fallback hop -- and emits ONE WARN. This test pins the
//! operator-grep contract: the WARN carries SAFE dimensions only
//! (`status`, `credential_source`, `has_client_session_id`) and NEVER
//! the forwarded token -- in a field, in the message, or anywhere.
//! `has_client_session_id` is derived from whether an inbound session
//! key was captured, NEVER from the token.
//!
//! Lives in its own integration-test binary (not the router lib's unit
//! tests) on purpose: a thread-local capture subscriber over a shared
//! `warn!` callsite is unreliable inside the 700+-test lib binary,
//! because sibling tests hit the same callsite under the default
//! `NoSubscriber` first and poison tracing's global per-callsite
//! `Interest` cache. In a dedicated binary the callsite is only ever
//! evaluated under this capture subscriber. Mirrors `forwarded_gate_log.rs`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use routectl_core::error::{Error, Result};
use routectl_core::schema::ForwardedBearer;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Provider};
use routectl_router::{AliasValue, Config, ProviderEntry, ResolvedModel, Router};
use tracing::field::{Field, Visit};

/// The forwarded token. Distinctive so any leak into a log field, the
/// log message, or the client error is unmistakable.
const FORWARDED_TOKEN: &str = "sk-ant-oat01-FORWARDED-SECRET-must-never-surface";

/// The inbound per-conversation session key. Distinctive so a leak of
/// the raw key (only the boolean presence is allowed) is unmistakable.
const SESSION_KEY: &str = "sess-CONVERSATION-KEY-must-never-surface-raw";

/// Provider that always 401s on the first-party (Anthropic) seat, so the
/// forwarded terminal path fires. Never rotates: `on_auth_failure` would
/// be a bug for a forwarded request and is asserted absent by the lib
/// tests; here we only care about the surfaced WARN.
struct Always401 {
    id: String,
}

#[async_trait]
impl Provider for Always401 {
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
        Err(Error::upstream(
            &self.id,
            401,
            "forwarded upstream rejected",
        ))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream(
            &self.id,
            401,
            "forwarded upstream rejected",
        ))
    }
}

/// Router whose alias `"alias"` resolves to a single `anthropic-api`
/// target on `api.anthropic.com`, so the forwarded gate ADMITS the
/// request and dispatch reaches the 401ing seat.
fn router_with_anthropic_target() -> Router {
    let mut config = Config::default();
    // A forwarded 401 short-circuits before any retry/backoff, so the
    // default retry policy adds no delay here.
    config.providers.insert(
        "p-anthropic".to_string(),
        ProviderEntry::anthropic_api("literal:k"),
    );
    config.aliases.insert(
        "alias".to_string(),
        AliasValue::Chain(vec!["m-anthropic".to_string()]),
    );

    let provider: Arc<dyn Provider> = Arc::new(Always401 {
        id: "p-anthropic".to_string(),
    });
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m-anthropic".to_string(),
        Arc::new(ResolvedModel::new(
            "m-anthropic",
            "p-anthropic",
            provider,
            "claude-x",
        )),
    );

    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

fn forwarded_req(with_session: bool) -> ChatRequest {
    let mut req = ChatRequest {
        model: "alias".into(),
        ..Default::default()
    };
    req.routectl_internal.forwarded_bearer =
        Some(ForwardedBearer::new(FORWARDED_TOKEN.to_string()));
    if with_session {
        req.routectl_internal.inbound_session_key = Some(SESSION_KEY.to_string());
    }
    req
}

// ---- tracing capture (mirrors forwarded_gate_log.rs) ----

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

/// The single f2.09 surfaced-verbatim WARN in `events`, if present.
fn terminal_warn(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|e| {
            e.fields
                .iter()
                .any(|(k, v)| k == "credential_source" && v == "forwarded")
        })
        .collect()
}

fn field<'a>(e: &'a CapturedEvent, name: &str) -> Option<&'a str> {
    e.fields
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

fn assert_no_secret_leak(events: &[CapturedEvent]) {
    for e in events {
        assert!(
            !e.message.contains(FORWARDED_TOKEN),
            "log message leaked the forwarded token: {}",
            e.message
        );
        assert!(
            !e.message.contains(SESSION_KEY),
            "log message leaked the raw session key: {}",
            e.message
        );
        for (k, v) in &e.fields {
            assert!(
                !v.contains(FORWARDED_TOKEN),
                "log field `{k}` leaked the forwarded token: {v}"
            );
            assert!(
                !v.contains(SESSION_KEY),
                "log field `{k}` leaked the raw session key: {v}"
            );
        }
    }
}

#[tokio::test]
async fn forwarded_terminal_warn_carries_safe_dimensions_with_session() {
    let router = router_with_anthropic_target();

    let (result, events) = with_capture(router.complete(forwarded_req(true))).await;

    assert!(
        result.is_err(),
        "forwarded 401 must surface verbatim as an error"
    );

    let warns = terminal_warn(&events);
    assert_eq!(
        warns.len(),
        1,
        "expected exactly one forwarded-terminal WARN; got: {events:?}"
    );
    let warn = warns[0];
    assert_eq!(warn.level, tracing::Level::WARN);
    assert_eq!(
        field(warn, "status"),
        Some("401"),
        "verbatim upstream status"
    );
    assert_eq!(field(warn, "credential_source"), Some("forwarded"));
    assert_eq!(
        field(warn, "has_client_session_id"),
        Some("true"),
        "session id was present on the request",
    );

    assert_no_secret_leak(&events);
}

#[tokio::test]
async fn forwarded_terminal_warn_reports_false_when_no_session_id() {
    let router = router_with_anthropic_target();

    let (result, events) = with_capture(router.complete(forwarded_req(false))).await;

    assert!(result.is_err());

    let warns = terminal_warn(&events);
    assert_eq!(warns.len(), 1, "expected exactly one WARN; got: {events:?}");
    let warn = warns[0];
    assert_eq!(field(warn, "credential_source"), Some("forwarded"));
    assert_eq!(
        field(warn, "has_client_session_id"),
        Some("false"),
        "no session id was present on the request",
    );

    assert_no_secret_leak(&events);
}
