//! End-to-end learned-capability loop, driven at the router level against
//! wiremock upstreams with REAL upstream error envelopes.
//!
//! The headline behavior this suite proves: an openai-compat upstream rejects
//! a capability the request NATURALLY carries AND that actually survives
//! egress onto the wire -- a structured-output request whose
//! `output_config.format` the egress lifts to a top-level `response_format`
//! field -- with a byte-accurate 400 whose `/error/param` names that same
//! wire field. The router translates the rejected param to its canonical key
//! (`structured_output`), the capture-side request-membership gate admits it
//! because the request derived that same key, the negative is learned under
//! it, and the chain filter routes away from the rejecting target on
//! subsequent matching requests. A wire-body assertion on the OUTBOUND request
//! confirms the rejected surface actually crossed the wire -- the guard
//! against a synthetic proof where the rejected capability was dropped at
//! egress and a real upstream could never have rejected it.
//!
//! The loop is observed through the PUBLIC router surface only:
//! `Dispatched.meta.learned_capabilities` (the per-request learn events),
//! `Dispatched.meta.served_provider`, the dispatch result, and wiremock
//! per-server hit counts + captured request bodies. Each upstream gets its
//! own `MockServer`, so a hit count is exactly that target's dial count.
//!
//! The scenarios are split into sibling submodules (one test binary): the
//! shared fixtures below back every scenario via `use super::*`.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

mod common;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures::StreamExt;

use routectl_auth::{MemoryStore, SecretStore};
use routectl_core::capability::SignalTier;
use routectl_core::schema::ForwardedBearer;
use routectl_core::{ChatRequest, Error, Message, MessageContent, Role, ToolDef};
use routectl_router::class_policy::{ClassPolicy, ConfigFailureClass};
use routectl_router::{
    AliasValue, BuildOptions, Config, DispatchedStream, ModelEntry, ProviderEntry,
    ProviderRuntimePolicy, RetryPolicy, Router, RouterOptions, build_resolved_models,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

/// A route-away capability the request carries as a built-in tool `type`
/// and the upstream names in `/error/param`. `web_search` is a policy
/// essential (`action_for` -> RouteAway), so a learned negative
/// de-prioritizes the target rather than stripping.
const WEB_SEARCH: &str = "web_search";

/// A second distinct route-away capability, so one target can carry two
/// independent learned negatives.
const COMPUTER_USE: &str = "computer_use";

/// The canonical constrained-decoding capability key. A structured-output
/// request derives this key; the openai wire param is `response_format`, and
/// the resolver's closed-set translation table maps the two.
const STRUCTURED_OUTPUT: &str = "structured_output";

/// A byte-accurate openai-compat `unsupported_parameter` 400 whose
/// `/error/param` is `response_format` -- the top-level wire field the
/// openai-compat egress emits for a structured-output request. This is the
/// surface that SURVIVES egress: a real upstream can genuinely reject it,
/// unlike a built-in tool the egress drops before the wire. The resolver
/// translates `response_format` onto the canonical `structured_output` key
/// the request side derives from `output_config.format`.
const OPENAI_UNSUPPORTED_RESPONSE_FORMAT_400: &str = r#"{"error":{"message":"Unsupported parameter: 'response_format' is not supported with this model.","type":"invalid_request_error","param":"response_format","code":"unsupported_parameter"}}"#;

/// A wiremock responder that walks a fixed sequence of `(status, body)`
/// steps across successive calls, repeating the last step once the
/// sequence is exhausted. Bodies are raw strings so a captured envelope is
/// served byte-for-byte. Deterministic and order-independent (a single
/// mounted mock).
struct SequencedResponder {
    calls: AtomicUsize,
    steps: Vec<(u16, String)>,
}

impl Respond for SequencedResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let i = self.calls.fetch_add(1, Ordering::SeqCst);
        let (status, body) = self.steps.get(i).unwrap_or_else(|| {
            self.steps
                .last()
                .expect("SequencedResponder needs at least one step")
        });
        ResponseTemplate::new(*status)
            .insert_header("content-type", "application/json")
            .set_body_string(body.clone())
    }
}

/// A wiremock upstream that answers `POST /chat/completions` with the given
/// raw response sequence.
async fn upstream_server(steps: Vec<(u16, Value)>) -> MockServer {
    let steps = steps
        .into_iter()
        .map(|(status, body)| (status, body.to_string()))
        .collect();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(SequencedResponder {
            calls: AtomicUsize::new(0),
            steps,
        })
        .mount(&server)
        .await;
    server
}

/// A wiremock upstream that serves the exact captured `body` string on
/// every call (byte-for-byte), for the byte-accurate real-envelope proof.
async fn raw_upstream_server(status: u16, body: &str) -> MockServer {
    let server = MockServer::start().await;
    let body = body.to_string();
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(SequencedResponder {
            calls: AtomicUsize::new(0),
            steps: vec![(status, body)],
        })
        .mount(&server)
        .await;
    server
}

/// Number of requests this upstream has received.
async fn hits(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .map_or(0, |reqs| reqs.len())
}

/// The parsed JSON body of the most recent request this upstream received.
/// Used to assert on the OUTBOUND wire: what routectl actually sent, not
/// what the request carried before egress.
async fn last_request_body(server: &MockServer) -> Value {
    let reqs = server.received_requests().await.expect("received requests");
    let last = reqs.last().expect("at least one request received");
    serde_json::from_slice(&last.body).expect("outbound request body is JSON")
}

/// One chain member: a `[models.<nickname>]` pointed at a
/// `[providers.<provider_name>]` openai-compat entry whose `base_url` is a
/// wiremock URL, plus the runtime policy for that provider.
struct Upstream {
    nickname: String,
    provider_name: String,
    base_url: String,
    runtime: ProviderRuntimePolicy,
}

impl Upstream {
    fn openai(nickname: &str, provider_name: &str, base_url: &str) -> Self {
        Self {
            nickname: nickname.to_string(),
            provider_name: provider_name.to_string(),
            base_url: base_url.to_string(),
            runtime: ProviderRuntimePolicy::default(),
        }
    }
}

/// Single-attempt, fast-backoff retry policy so a chain walk falls back
/// promptly without wall-clock sleeps.
fn fast_retry() -> RetryPolicy {
    let mut r = RetryPolicy::default();
    r.max_attempts = 1;
    r.initial_backoff_ms = 1;
    r.backoff_multiplier = 1.0;
    r
}

/// Build a router whose chain resolves `alias` to `chain` (nicknames in
/// order), with `[capability]` enabled at `decay_hours`. Providers are real
/// openai-compat egresses pointed at the wiremock URLs.
async fn build_router(
    upstreams: Vec<Upstream>,
    alias: &str,
    chain: &[&str],
    decay_hours: u64,
) -> Router {
    let mut providers = BTreeMap::new();
    let mut models = BTreeMap::new();
    for u in &upstreams {
        providers.insert(
            u.provider_name.clone(),
            ProviderEntry::openai_compat(&u.base_url, common::file_ref("test-key"))
                .with_runtime(u.runtime.clone()),
        );
        models.insert(
            u.nickname.clone(),
            ModelEntry::new(&u.provider_name, "upstream-model"),
        );
    }

    let mut aliases = BTreeMap::new();
    let value = if chain.len() == 1 {
        AliasValue::Single(chain[0].to_string())
    } else {
        AliasValue::Chain(chain.iter().map(|s| (*s).to_string()).collect())
    };
    aliases.insert(alias.to_string(), value);

    let mut cfg = Config {
        providers,
        models,
        aliases,
        retry: fast_retry(),
        ..Config::default()
    };
    cfg.capability.enabled = true;
    cfg.capability.decay_hours = decay_hours;

    let store: Arc<dyn SecretStore> = Arc::new(MemoryStore);
    let (resolved, failed) = build_resolved_models(&cfg, store, BuildOptions::default())
        .await
        .expect("build_resolved_models");
    assert!(failed.is_empty(), "provider build failures: {failed:?}");

    let mut router = Router::new(Arc::new(cfg));
    router.install_resolved_models(resolved);
    router
}

/// A request against `alias` carrying a built-in tool whose `type` is
/// `feature`, so `derive_feature_keys` yields `[feature]`.
fn req_with_feature(alias: &str, feature: &str) -> ChatRequest {
    req_with_features(alias, &[feature])
}

/// A request against `alias` carrying built-in tools whose `type` is each
/// entry of `features`, so `derive_feature_keys` yields `features`.
fn req_with_features(alias: &str, features: &[&str]) -> ChatRequest {
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
        }],
        tools: Some(
            features
                .iter()
                .map(|f| ToolDef::Other(json!({"type": *f, "name": "t"})))
                .collect(),
        ),
        ..Default::default()
    }
}

/// A request against `alias` carrying an Anthropic-shape structured-output
/// `output_config.format`. `derive_feature_keys` yields `[structured_output]`,
/// and the openai-compat egress lifts the format to a top-level
/// `response_format` on the wire body -- so the capability the upstream
/// rejects by `error.param="response_format"` actually crosses the wire.
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
        }],
        provider_extras: Some(json!({
            "output_config": { "format": {"type": "json_object"} }
        })),
        ..Default::default()
    }
}

/// An openai-compat 400 whose `/error/param` is `param`; the classifier
/// lifts the `unsupported_parameter` code to `FeatureUnsupported` and the
/// resolver reads `param` as the real capability.
fn unsupported_body_for(param: &str) -> Value {
    json!({
        "error": {
            "type": "invalid_request_error",
            "code": "unsupported_parameter",
            "param": param,
            "message": format!("Unsupported parameter: '{param}' is not supported with this model.")
        }
    })
}

/// A generic upstream error body (health-status responses).
fn health_body() -> Value {
    json!({"error": {"type": "server_error", "message": "upstream trouble"}})
}

/// A minimal, valid OpenAI chat-completion success body.
fn ok_body() -> Value {
    json!({
        "id": "cmpl-test",
        "object": "chat.completion",
        "created": 1,
        "model": "upstream-model",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "ok"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
}

async fn complete(router: &Router, alias: &str) -> routectl_router::Dispatched {
    complete_with(router, alias, &[WEB_SEARCH]).await
}

/// Dispatch a request against `alias` carrying `features` as derived tool
/// types.
async fn complete_with(
    router: &Router,
    alias: &str,
    features: &[&str],
) -> routectl_router::Dispatched {
    router
        .complete_with_options(req_with_features(alias, features), RouterOptions::default())
        .await
}

#[path = "learned_capability_loop/real_envelope.rs"]
mod real_envelope;

#[path = "learned_capability_loop/learn_and_decay.rs"]
mod learn_and_decay;

#[path = "learned_capability_loop/never_learn.rs"]
mod never_learn;

#[path = "learned_capability_loop/learned_tail.rs"]
mod learned_tail;

#[path = "learned_capability_loop/streaming.rs"]
mod streaming;
