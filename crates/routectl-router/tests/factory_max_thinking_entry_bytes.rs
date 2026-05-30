//! Tests for `[providers.X] max_thinking_entry_bytes` validation in
//! the anthropic-api factory arm.
//!
//! Cases covered:
//! - `None`               -> default used, no warn
//! - `Some(0)`            -> WARN + default used (degraded mode)
//! - `Some(huge)`         -> WARN + clamped to ceiling
//! - `Some(reasonable)`   -> used as-is, no warn
//!
//! Reuses the in-process tracing capture pattern from
//! `factory_context_management_warning.rs`.

use std::sync::{Arc, Mutex};

use routectl_auth::{MemoryStore, SecretStore};
use routectl_router::{build_provider, ProviderEntry};
use tracing::field::{Field, Visit};

#[derive(Debug, Clone)]
#[allow(dead_code)] // target/level read via Debug on assert failure
struct CapturedEvent {
    level: tracing::Level,
    target: String,
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
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields.push((field.name().into(), value.to_string()));
    }
    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields.push((field.name().into(), value.to_string()));
    }
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields.push((field.name().into(), value.to_string()));
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
        let captured = CapturedEvent {
            level: *meta.level(),
            target: meta.target().to_string(),
            message: visitor.message,
            fields: visitor.fields,
        };
        if let Ok(mut guard) = self.captured.lock() {
            guard.push(captured);
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

/// Set `max_thinking_entry_bytes` on an anthropic-api entry.
fn anthropic_entry_with_cap(cap: Option<usize>) -> ProviderEntry {
    let mut entry = ProviderEntry::anthropic_api("literal:sk-test");
    if let ProviderEntry::AnthropicApi {
        ref mut max_thinking_entry_bytes,
        ..
    } = entry
    {
        *max_thinking_entry_bytes = cap;
    }
    entry
}

fn find_warn(events: &[CapturedEvent], needle: &str) -> Option<CapturedEvent> {
    events
        .iter()
        .find(|e| e.level == tracing::Level::WARN && e.message.contains(needle))
        .cloned()
}

/// `None` -> default used; no warn.
#[tokio::test]
async fn none_uses_default_no_warn() {
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
    let entry = anthropic_entry_with_cap(None);

    let (provider, events) = with_capture(build_provider("anthropic", &entry, store)).await;
    provider.expect("build");

    assert!(
        find_warn(&events, "max_thinking_entry_bytes").is_none(),
        "no warn expected for None; got: {events:?}"
    );
}

/// `Some(reasonable)` (e.g. 100 KiB) -> used as-is; no warn.
#[tokio::test]
async fn reasonable_value_used_as_is_no_warn() {
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
    let entry = anthropic_entry_with_cap(Some(100 * 1024));

    let (provider, events) = with_capture(build_provider("anthropic", &entry, store)).await;
    provider.expect("build");

    assert!(
        find_warn(&events, "max_thinking_entry_bytes").is_none(),
        "no warn expected for reasonable value; got: {events:?}"
    );
}

/// `Some(0)` -> WARN + default used.
#[tokio::test]
async fn zero_warns_and_uses_default() {
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
    let entry = anthropic_entry_with_cap(Some(0));

    let (provider, events) = with_capture(build_provider("anthropic", &entry, store)).await;
    provider.expect("build");

    let warn = find_warn(&events, "max_thinking_entry_bytes")
        .unwrap_or_else(|| panic!("expected WARN for cap=0; got: {events:?}"));
    assert!(
        warn.message.contains("must be > 0"),
        "warn must explain the rejection; got: {:?}",
        warn.message
    );
    assert!(
        warn.fields.iter().any(|(k, _)| k == "provider"),
        "warn must carry structured `provider` field; got: {warn:?}"
    );
    assert!(
        warn.fields.iter().any(|(k, _)| k == "default_bytes"),
        "warn must carry structured `default_bytes` field; got: {warn:?}"
    );
}

/// `Some(huge)` -> WARN + clamped to ceiling.
#[tokio::test]
async fn above_ceiling_warns_and_clamps() {
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
    // 16 MiB -- well above the 4 MiB documented ceiling.
    let entry = anthropic_entry_with_cap(Some(16 * 1024 * 1024));

    let (provider, events) = with_capture(build_provider("anthropic", &entry, store)).await;
    provider.expect("build");

    let warn = find_warn(&events, "max_thinking_entry_bytes")
        .unwrap_or_else(|| panic!("expected WARN above ceiling; got: {events:?}"));
    assert!(
        warn.message.contains("ceiling") && warn.message.contains("clamping"),
        "warn must announce clamping; got: {:?}",
        warn.message
    );
    assert!(
        warn.fields.iter().any(|(k, _)| k == "provider"),
        "warn must carry structured `provider` field; got: {warn:?}"
    );
    assert!(
        warn.fields.iter().any(|(k, _)| k == "configured_bytes"),
        "warn must carry structured `configured_bytes` field; got: {warn:?}"
    );
    assert!(
        warn.fields.iter().any(|(k, _)| k == "ceiling_bytes"),
        "warn must carry structured `ceiling_bytes` field; got: {warn:?}"
    );
}
