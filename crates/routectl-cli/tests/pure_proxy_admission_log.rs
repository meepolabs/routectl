//! Structured-log safety for the forwarded-mode (pure-proxy) INGRESS
//! admission rejections (`first-party-passthrough.f2.06`, decision-doc
//! Section 6).
//!
//! When `ingress_handle` rejects a forwarded request at admission it emits
//! ONE WARN. This test pins the operator-grep contract: the WARN carries
//! SAFE dimensions only (`reason`, `status`, `credential_source`,
//! `has_client_session_id`) and NEVER the forwarded token -- in a field, in
//! the message, or anywhere.
//!
//! Lives in its own integration-test binary (not the CLI lib's unit tests)
//! on purpose: a thread-local capture subscriber over a shared `warn!`
//! callsite is unreliable inside the 600+-test lib binary, because sibling
//! tests hit the same callsite under the default `NoSubscriber` first and
//! poison tracing's global per-callsite `Interest` cache. In a dedicated
//! binary the callsite is only ever evaluated under this capture subscriber.
//! Mirrors the router-side `forwarded_gate_log.rs` capture pattern.

use std::sync::{Arc, Mutex};

use arc_swap::ArcSwap;
use axum::Json;
use axum::http::{HeaderMap, HeaderName, HeaderValue, header::AUTHORIZATION};
use routectl_cli::handlers::ingress_handle::ingress_handle;
use routectl_cli::ingress::anthropic::AnthropicIngress;
use routectl_cli::server::AppState;
use routectl_router::config::CredentialSource;
use routectl_router::{Config, MitmConfig, Router};
use routectl_usage::UsageWriter;
use serde_json::{Value, json};
use tracing::field::{Field, Visit};

/// The forwarded token. Distinctive so any leak into a log field, the log
/// message, or the client response is unmistakable.
const FORWARDED_TOKEN: &str = "sk-ant-oat01-FORWARDED-INGRESS-must-never-surface";

/// The MITM seam header the forwarded-mode admission gate treats as the hint
/// that a request arrived through the f1 proxy leg.
const MITM_PROXIED_HEADER: &str = "x-routectl-mitm-proxied";

/// Build a forwarded-mode `AppState` (`[mitm] credential_source = forwarded`)
/// with an isolated in-tempdir usage writer. `AppState::for_test` is
/// `#[cfg(test)]`-only (not visible to an integration crate), so we build the
/// pub-field struct directly.
fn forwarded_app_state() -> (Arc<AppState>, tempfile::TempDir) {
    let mitm = MitmConfig {
        credential_source: CredentialSource::Forwarded,
        ..Default::default()
    };
    let router = Router::new(Arc::new(Config {
        mitm: Some(mitm),
        ..Default::default()
    }));
    let dir = tempfile::tempdir().expect("usage tempdir");
    // enabled=false: the admission-rejection path never writes a usage row,
    // but the handle must be constructible. Drop the owning writer; the
    // handle stays usable (accepts-and-drops once the channel closes).
    let (usage, _writer) = UsageWriter::start(dir.path().join("usage.db"), 128, 0, false);
    let state = Arc::new(AppState {
        router: Arc::new(ArcSwap::from_pointee(router)),
        usage,
    });
    (state, dir)
}

/// Headers for a MITM-marked forwarded request carrying the distinctive
/// bearer but NO `x-claude-code-session-id`: that is the `identity_missing`
/// case, which is the strongest leak probe because it carries a token.
fn identity_missing_headers() -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        HeaderName::from_static(MITM_PROXIED_HEADER),
        HeaderValue::from_static("1"),
    );
    h.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {FORWARDED_TOKEN}")).unwrap(),
    );
    h
}

// ---- tracing capture (mirrors router-side forwarded_gate_log.rs) ----

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

/// Drive `fut` under a thread-local capture subscriber and return its output
/// plus the captured events. `#[tokio::test]` defaults to a current_thread
/// runtime, so the subscriber spans the whole future.
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
async fn rejection_logs_safe_dimensions_and_never_the_token() {
    let (state, _dir) = forwarded_app_state();
    let headers = identity_missing_headers();
    // Admission runs before body parse, so the body is never inspected on
    // the rejection path.
    let body: std::result::Result<Json<Value>, axum::extract::rejection::JsonRejection> =
        Ok(Json(json!({})));

    let (resp, events) = with_capture(Box::pin(ingress_handle(
        state,
        headers,
        None,
        body,
        AnthropicIngress,
    )))
    .await;

    // The request was rejected at admission (HTTP 400 identity_missing).
    assert_eq!(resp.status().as_u16(), 400);

    // Exactly one rejection WARN, carrying SAFE dimensions only.
    let rejections: Vec<&CapturedEvent> = events
        .iter()
        .filter(|e| {
            e.fields
                .iter()
                .any(|(k, v)| k == "reason" && v == "identity_missing")
        })
        .collect();
    assert_eq!(
        rejections.len(),
        1,
        "expected exactly one rejection WARN; got: {events:?}"
    );
    let rejection = rejections[0];
    assert_eq!(rejection.level, tracing::Level::WARN);

    let field = |name: &str| {
        rejection
            .fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(field("reason"), Some("identity_missing"));
    assert_eq!(field("status"), Some("400"));
    assert_eq!(field("credential_source"), Some("forwarded"));
    assert_eq!(
        field("has_client_session_id"),
        Some("false"),
        "the identity_missing case carries no client session id",
    );

    // The forwarded token must NOT appear in ANY captured event -- not in a
    // field value, not in the message.
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
