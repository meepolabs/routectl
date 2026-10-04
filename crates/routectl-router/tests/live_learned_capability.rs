//! Live-network smoke for the learned-capability loop: a real openai-compat
//! upstream rejects a structured-output request with a 400 whose
//! `/error/param` is `response_format` -- the surface that actually SURVIVES
//! egress (a built-in tool the egress drops never crosses the wire, so no real
//! upstream could reject it). The resolver translates `response_format` onto
//! the canonical `structured_output` key the request derives, and the capture
//! membership gate admits it because the request carried that capability. The
//! wiremock counterpart is `learned_capability_loop`'s `real_envelope` module.
//!
//! Needs `ROUTECTL_LIVE_BASE_URL` (the provider's base URL) and
//! `ROUTECTL_LIVE_API_KEY`; panics when either is unset. Run with:
//!
//!   ROUTECTL_LIVE_BASE_URL=... ROUTECTL_LIVE_API_KEY=... \
//!     cargo test -p routectl-router --features live-integration \
//!       --test live_learned_capability -- --nocapture
//!
//! This target is `test = false` in Cargo.toml, so only a `--test` that
//! names it or a `--test` glob that matches it runs it.

#![cfg(feature = "live-integration")]

use std::collections::BTreeMap;
use std::sync::Arc;

mod common;

use routectl_auth::{MemoryStore, SecretStore};
use routectl_core::{ChatRequest, Message, MessageContent, Role};
use routectl_router::{
    AliasValue, BuildOptions, Config, ModelEntry, ProviderEntry, RetryPolicy, Router,
    RouterOptions, build_resolved_models,
};
use serde_json::json;

const ENV_BASE_URL: &str = "ROUTECTL_LIVE_BASE_URL";
const ENV_API_KEY: &str = "ROUTECTL_LIVE_API_KEY";
const MODEL: &str = "gpt-4o-mini";

/// Single-attempt retry policy: one rejection is the whole observation.
fn single_attempt() -> RetryPolicy {
    let mut r = RetryPolicy::default();
    r.max_attempts = 1;
    r.initial_backoff_ms = 1;
    r.backoff_multiplier = 1.0;
    r
}

/// A request carrying an Anthropic-shape structured-output
/// `output_config.format`, which the openai-compat egress lifts to a
/// top-level `response_format` on the wire.
fn req_with_structured_output(alias: &str) -> ChatRequest {
    ChatRequest {
        model: alias.to_string(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        provider_extras: Some(json!({
            "output_config": { "format": {"type": "json_object"} }
        })),
        ..Default::default()
    }
}

#[tokio::test]
async fn live_openai_unsupported_parameter_is_learned() {
    let (Ok(base_url), Ok(api_key)) = (std::env::var(ENV_BASE_URL), std::env::var(ENV_API_KEY))
    else {
        panic!("set {ENV_BASE_URL} and {ENV_API_KEY} to run the live smoke");
    };

    let mut providers = BTreeMap::new();
    providers.insert(
        "live".to_string(),
        ProviderEntry::openai_compat(&base_url, common::file_ref(&api_key)),
    );
    let mut models = BTreeMap::new();
    models.insert("m_live".to_string(), ModelEntry::new("live", MODEL));
    let mut aliases = BTreeMap::new();
    aliases.insert("live".to_string(), AliasValue::Single("m_live".to_string()));

    let mut cfg = Config {
        providers,
        models,
        aliases,
        retry: single_attempt(),
        ..Config::default()
    };
    cfg.capability.enabled = true;
    cfg.capability.decay_hours = 48;

    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
    let (resolved, failed) = build_resolved_models(&cfg, store, BuildOptions::default())
        .await
        .expect("build_resolved_models");
    assert!(failed.is_empty(), "provider build failures: {failed:?}");
    let mut router = Router::new(Arc::new(cfg));
    router.install_resolved_models(resolved);

    let d = router
        .complete_with_options(req_with_structured_output("live"), RouterOptions::default())
        .await;
    assert!(
        !d.meta.learned_capabilities.is_empty(),
        "a real upstream unsupported-parameter 400 must produce a learn event: {:?}",
        d.result,
    );
}
