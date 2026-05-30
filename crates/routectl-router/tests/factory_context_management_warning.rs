//! Tests for the `context_management` -> `history_reasoning = "preserve"`
//! consistency warning emitted by `build_resolved_models`.
//!
//! Cases covered:
//! - cm=true + history!=preserve  -> WARN fires exactly once
//! - cm=true + history=preserve   -> silent
//! - cm=false                      -> silent
//! - non-anthropic-api kind       -> silent
//!
//! Uses an in-process tracing subscriber (no `tracing-test` dev-dep)
//! so the test can drive `build_resolved_models` and read back the
//! captured events directly. Mirrors the capture pattern already used
//! in `crates/routectl-cli/tests/anthropic_forward_compat_stream.rs`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use routectl_auth::{MemoryStore, SecretStore};
use routectl_router::{
    build_resolved_models, BuildOptions, Config, HistoryReasoning, ModelEntry, ProviderEntry,
};
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

/// Drive `fut` under the capture subscriber and return its output
/// alongside the captured events. `#[tokio::test]` defaults to a
/// current_thread runtime so the thread-local subscriber spans the
/// whole future.
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

/// Build a single-model `Config` whose anthropic-api provider has the
/// given `context_management` flag and whose model carries the given
/// `history_reasoning`.
fn anthropic_api_config(
    context_management: bool,
    history_reasoning: Option<HistoryReasoning>,
) -> Config {
    let mut entry = ProviderEntry::anthropic_api("literal:sk-test");
    if let ProviderEntry::AnthropicApi {
        context_management: ref mut cm,
        ..
    } = entry
    {
        *cm = context_management;
    }
    let mut providers: BTreeMap<String, ProviderEntry> = BTreeMap::new();
    providers.insert("anthropic".into(), entry);

    let mut model = ModelEntry::new("anthropic", "claude-sonnet-4-6");
    model.history_reasoning = history_reasoning;

    let mut models: BTreeMap<String, ModelEntry> = BTreeMap::new();
    models.insert("claude".into(), model);

    Config {
        providers,
        models,
        ..Config::default()
    }
}

/// Pick the first WARN event whose message names BOTH
/// `context_management` and `history_reasoning` (so an unrelated WARN
/// from another code path can't false-positive). Returns `None` when
/// no such event was captured.
fn find_cm_history_warn(events: &[CapturedEvent]) -> Option<&CapturedEvent> {
    events.iter().find(|e| {
        e.level == tracing::Level::WARN
            && e.message.contains("context_management")
            && e.message.contains("history_reasoning")
    })
}

/// cm=true + history=Strip -> WARN must fire exactly once.
#[tokio::test]
async fn cm_true_history_not_preserve_warns_once() {
    let cfg = anthropic_api_config(true, Some(HistoryReasoning::Strip));
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);

    let (result, events) =
        with_capture(build_resolved_models(&cfg, store, BuildOptions::default())).await;
    let (resolved, failed) = result.expect("build");
    assert!(failed.is_empty(), "expected no build failures: {failed:?}");
    assert!(resolved.contains_key("claude"));

    let matching: Vec<&CapturedEvent> = events
        .iter()
        .filter(|e| {
            e.level == tracing::Level::WARN
                && e.message.contains("context_management")
                && e.message.contains("history_reasoning")
        })
        .collect();
    assert_eq!(
        matching.len(),
        1,
        "expected exactly one WARN naming both context_management and \
         history_reasoning; got: {matching:?}"
    );
}

/// cm=true + history=None (unset) -> WARN must fire (None != Preserve).
#[tokio::test]
async fn cm_true_history_unset_warns() {
    let cfg = anthropic_api_config(true, None);
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);

    let (result, events) =
        with_capture(build_resolved_models(&cfg, store, BuildOptions::default())).await;
    let _ = result.expect("build");

    let warn = find_cm_history_warn(&events);
    assert!(
        warn.is_some(),
        "expected WARN when history_reasoning is unset; got: {events:?}"
    );
}

/// cm=true + history=Preserve -> silent.
#[tokio::test]
async fn cm_true_history_preserve_is_silent() {
    let cfg = anthropic_api_config(true, Some(HistoryReasoning::Preserve));
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);

    let (result, events) =
        with_capture(build_resolved_models(&cfg, store, BuildOptions::default())).await;
    let _ = result.expect("build");

    let warn = find_cm_history_warn(&events);
    assert!(
        warn.is_none(),
        "expected NO WARN when history_reasoning = Preserve; got: {warn:?}"
    );
}

/// cm=false -> silent regardless of history_reasoning.
#[tokio::test]
async fn cm_false_is_silent() {
    let cfg = anthropic_api_config(false, Some(HistoryReasoning::Strip));
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);

    let (result, events) =
        with_capture(build_resolved_models(&cfg, store, BuildOptions::default())).await;
    let _ = result.expect("build");

    let warn = find_cm_history_warn(&events);
    assert!(
        warn.is_none(),
        "expected NO WARN when context_management = false; got: {warn:?}"
    );
}

/// Non-anthropic-api kind -> silent. The guard is scoped to anthropic-api
/// providers; an openai-compat provider can never carry a
/// `context_management` flag and must not trigger the warning.
#[tokio::test]
async fn non_anthropic_api_kind_is_silent() {
    let mut providers: BTreeMap<String, ProviderEntry> = BTreeMap::new();
    providers.insert(
        "host".into(),
        ProviderEntry::openai_compat("https://example.com/v1", "literal:sk-test"),
    );
    let mut model = ModelEntry::new("host", "some-model");
    // history_reasoning != Preserve so a buggy guard would trip.
    model.history_reasoning = Some(HistoryReasoning::Strip);
    let mut models: BTreeMap<String, ModelEntry> = BTreeMap::new();
    models.insert("m".into(), model);
    let cfg = Config {
        providers,
        models,
        ..Config::default()
    };
    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);

    let (result, events) =
        with_capture(build_resolved_models(&cfg, store, BuildOptions::default())).await;
    let _ = result.expect("build");

    let warn = find_cm_history_warn(&events);
    assert!(
        warn.is_none(),
        "expected NO WARN for non-anthropic-api kind; got: {warn:?}"
    );
}
