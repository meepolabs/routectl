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
use std::sync::Arc;

use routectl_auth::{MemoryStore, SecretStore};
use routectl_router::{
    BuildOptions, Config, HistoryReasoning, ModelEntry, ProviderEntry, build_resolved_models,
};
use routectl_testkit::{CapturedEvent, with_capture};

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

/// Assert the captured WARN carries the expected structured fields.
/// Pins the operator-grep contract: the warn site emits `provider`
/// and `model` fields with the configured nicknames so tail-side
/// filtering (e.g. by provider) keeps working through future refactors.
fn assert_warn_carries_provider_and_model(warn: &CapturedEvent, provider: &str, model: &str) {
    let provider_field = warn
        .fields
        .iter()
        .find(|(k, _)| k == "provider")
        .unwrap_or_else(|| panic!("warn event missing structured `provider` field; got: {warn:?}"));
    let model_field = warn
        .fields
        .iter()
        .find(|(k, _)| k == "model")
        .unwrap_or_else(|| panic!("warn event missing structured `model` field; got: {warn:?}"));
    assert!(
        provider_field.1.contains(provider),
        "warn `provider` field must contain {provider:?}; got: {:?}",
        provider_field.1
    );
    assert!(
        model_field.1.contains(model),
        "warn `model` field must contain {model:?}; got: {:?}",
        model_field.1
    );
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
    assert_warn_carries_provider_and_model(matching[0], "anthropic", "claude");
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
    let warn = warn.unwrap_or_else(|| {
        panic!("expected WARN when history_reasoning is unset; got: {events:?}")
    });
    assert_warn_carries_provider_and_model(warn, "anthropic", "claude");
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
