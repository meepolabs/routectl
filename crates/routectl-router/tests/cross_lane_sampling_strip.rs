//! Cross-lane fallback hop onto routectl's own OAuth Anthropic seat.
//!
//! The sampling strip is per-lane, so its riskiest shape is a chain that
//! crosses lanes mid-request: a first hop that must receive the caller's
//! sampling verbatim, then a fallback hop onto the own-OAuth
//! api.anthropic.com seat that must not. This binary pins that the strip
//! is scoped to the hop that needs it and leaks neither way -- the first
//! hop's request is untouched, the OAuth hop's outbound body carries no
//! sampling, and the caller's own canonical request survives the walk
//! unmutated.
//!
//! REACHABLE SUBSET. The OAuth lane is host-pinned: `is_cloak_lane`
//! requires the exact `api.anthropic.com` host, so no mock server can
//! serve the fallback hop and a REAL 2xx from it is unreachable in-tree.
//! The hop is therefore driven with a failing token source, which halts
//! it at token resolution -- AFTER `normalize_request`, `cloak_body`, the
//! lane-gated strip, and the outgoing-body trace have all run, so every
//! body-shaping assertion below observes the exact bytes that would have
//! gone on the wire. What is NOT proven here is the hop returning a
//! successful upstream response; that half belongs to the live
//! acceptance gate against the real seat.
//!
//! Lives in its own integration-test binary (not the router lib's unit
//! tests) for the same reason as the forwarded-auth terminal log tests: a
//! thread-local capture subscriber over a shared callsite is unreliable
//! inside a large test binary, because sibling tests hit the callsite
//! under the default `NoSubscriber` first and poison tracing's global
//! per-callsite `Interest` cache.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use routectl_core::error::{Error, Result};
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Provider, TokenSource};
use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider, AuthKind};
use routectl_router::{
    AliasValue, Config, Dispatched, ProviderEntry, ResolvedModel, RetryPolicy, Router,
    RouterOptions,
};
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::registry::LookupSpan;

mod common;

/// The logical request id the server's request-id layer would stamp on
/// the per-request span. Distinctive so the correlation assertion cannot
/// pass on an unrelated value.
const REQUEST_ID: &str = "req-cross-lane-0001";

const CALLER_TEMPERATURE: f64 = 0.31;
const CALLER_STOP: &str = "HALT";

// -- span-aware capture ------------------------------------------------

/// One captured event plus the `request_id` inherited from the enclosing
/// span scope. The shared testkit subscriber is event-only; correlating a
/// provider-emitted WARN with the request that produced it needs the
/// ambient span fields, which is what this layer adds.
#[derive(Debug, Clone)]
struct ScopedEvent {
    level: tracing::Level,
    message: String,
    fields: Vec<(String, String)>,
    request_id: Option<String>,
}

impl ScopedEvent {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
struct FieldCollector {
    message: String,
    fields: Vec<(String, String)>,
}

impl tracing::field::Visit for FieldCollector {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push((field.name().into(), value.into()));
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let rendered = format!("{value:?}");
        if field.name() == "message" {
            self.message = rendered.trim_matches('"').to_string();
        } else {
            self.fields.push((field.name().into(), rendered));
        }
    }
}

/// Span-local storage for the `request_id` field, read back by
/// `on_event` when walking the event's scope.
struct SpanRequestId(String);

#[derive(Clone, Default)]
struct ScopeCapture {
    events: Arc<Mutex<Vec<ScopedEvent>>>,
}

impl<S> tracing_subscriber::Layer<S> for ScopeCapture
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut collector = FieldCollector::default();
        attrs.record(&mut collector);
        if let Some((_, value)) = collector.fields.iter().find(|(k, _)| k == "request_id")
            && let Some(span) = ctx.span(id)
        {
            span.extensions_mut()
                .insert(SpanRequestId(value.trim_matches('"').to_string()));
        }
    }

    fn on_event(&self, event: &tracing::Event<'_>, ctx: tracing_subscriber::layer::Context<'_, S>) {
        let mut collector = FieldCollector::default();
        event.record(&mut collector);
        let request_id = ctx.event_scope(event).and_then(|scope| {
            scope.from_root().find_map(|span| {
                span.extensions()
                    .get::<SpanRequestId>()
                    .map(|rid| rid.0.clone())
            })
        });
        self.events
            .lock()
            .expect("capture poisoned")
            .push(ScopedEvent {
                level: *event.metadata().level(),
                message: collector.message,
                fields: collector.fields,
                request_id,
            });
    }
}

/// Drive `fut` inside a `request_id`-carrying span with the span-aware
/// capture installed as the thread-local default, mirroring the shape the
/// server's request-id layer produces around a dispatch.
async fn capture_under_request_span<F: std::future::Future>(
    fut: F,
) -> (F::Output, Vec<ScopedEvent>) {
    let capture = ScopeCapture::default();
    let subscriber = tracing_subscriber::registry().with(capture.clone());
    let _guard = tracing::subscriber::set_default(subscriber);
    let span = tracing::info_span!("request", request_id = REQUEST_ID);
    let out = {
        let _entered = span.enter();
        fut.await
    };
    let events = capture.events.lock().expect("capture poisoned").clone();
    (out, events)
}

// -- test doubles ------------------------------------------------------

/// First hop: records the `ChatRequest` it was handed, then fails with a
/// fallbackable upstream 503 so the chain walks to the OAuth entry.
struct RecordingFirstHop {
    id: String,
    seen: Mutex<Vec<ChatRequest>>,
}

impl RecordingFirstHop {
    fn new(id: &str) -> Arc<Self> {
        Arc::new(Self {
            id: id.into(),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn seen(&self) -> Vec<ChatRequest> {
        self.seen.lock().expect("seen poisoned").clone()
    }
}

#[async_trait]
impl Provider for RecordingFirstHop {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(&self.id, "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.seen.lock().expect("seen poisoned").push(req);
        Err(Error::upstream(&self.id, 503, "first hop unavailable"))
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.seen.lock().expect("seen poisoned").push(req);
        Err(Error::upstream(&self.id, 503, "first hop unavailable"))
    }
}

/// Token source that always errors. Halts the OAuth hop at token
/// resolution -- after every body mutation and the outgoing-body trace,
/// before any network I/O against the host-pinned seat.
#[derive(Debug)]
struct FailingTokenSource;

#[async_trait]
impl TokenSource for FailingTokenSource {
    async fn token(&self) -> Result<String> {
        Err(Error::Auth("no token in this test".into()))
    }
}

/// The REAL anthropic-api provider on the own-OAuth lane: `OauthBearer`
/// plus the exact `api.anthropic.com` host and no forwarded bearer, which
/// is precisely `is_cloak_lane`. Built through the public constructor so
/// the lane predicate reads production defaults, not a hand-assembled
/// struct.
fn oauth_lane_provider(id: &str) -> Arc<AnthropicApiProvider> {
    let mut cfg = AnthropicApiConfig::new_with_auth(id, Arc::new(FailingTokenSource));
    cfg.auth_kind = AuthKind::OauthBearer;
    cfg.base_url = "https://api.anthropic.com".into();
    Arc::new(AnthropicApiProvider::new(cfg))
}

/// Two-entry chain: a non-OAuth first hop, then the own-OAuth seat.
fn cross_lane_router(first: Arc<dyn Provider>, oauth: Arc<dyn Provider>) -> Router {
    let mut config = Config::default();
    config.providers.insert(
        "p-first".into(),
        ProviderEntry::openai_compat("http://example.invalid", common::file_ref("k")),
    );
    config.providers.insert(
        "p-oauth".into(),
        ProviderEntry::anthropic_api(common::file_ref("k")),
    );
    config.aliases.insert(
        "fast".into(),
        AliasValue::Chain(vec!["m-first".into(), "m-oauth".into()]),
    );
    // One attempt per entry: the walk to the second entry must be a
    // FALLBACK hop, not a within-provider retry.
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    retry.initial_backoff_ms = 1;
    retry.backoff_multiplier = 1.0;
    config.retry = retry;

    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m-first".into(),
        Arc::new(ResolvedModel::new("m-first", "p-first", first, "up-first")),
    );
    models.insert(
        "m-oauth".into(),
        Arc::new(ResolvedModel::new(
            "m-oauth",
            "p-oauth",
            oauth,
            "claude-sonnet-4-5",
        )),
    );

    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

/// The caller's canonical request, carrying sampling the OAuth seat
/// rejects plus a `stop_sequences`-bound stop the seat accepts.
fn sampling_req() -> ChatRequest {
    ChatRequest {
        model: "fast".into(),
        max_tokens: Some(2048),
        temperature: Some(CALLER_TEMPERATURE),
        stop: Some(vec![CALLER_STOP.into()]),
        ..Default::default()
    }
}

fn strip_warns(events: &[ScopedEvent]) -> Vec<&ScopedEvent> {
    events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN && e.field("dropped_params").is_some())
        .collect()
}

fn outgoing_bodies(events: &[ScopedEvent]) -> Vec<&str> {
    events
        .iter()
        .filter(|e| e.message == "outgoing request body")
        .filter_map(|e| e.field("body"))
        .collect()
}

// -- the cross-lane hop ------------------------------------------------

/// The whole contract in one dispatch: the first hop sees the caller's
/// sampling verbatim, fails fallbackably, and the own-OAuth fallback hop
/// ships a body with sampling stripped -- while the caller's canonical
/// request is left untouched and the strip WARN correlates to the same
/// logical request id.
#[tokio::test]
async fn cross_lane_fallback_strips_sampling_only_on_the_oauth_hop() {
    let first = RecordingFirstHop::new("p-first");
    let oauth = oauth_lane_provider("p-oauth");
    let router = cross_lane_router(
        first.clone() as Arc<dyn Provider>,
        oauth as Arc<dyn Provider>,
    );

    let caller_req = sampling_req();
    let (Dispatched { meta, result }, events) = capture_under_request_span(
        router.complete_with_options(caller_req.clone(), RouterOptions::new()),
    )
    .await;

    // The OAuth hop halts at token resolution (the host-pinned seat is
    // unreachable in-tree), so the chain is fully walked and terminal.
    result.expect_err("the OAuth hop cannot resolve a token in-tree");

    // -- dispatch metadata: exactly one hop, served by the OAuth target.
    assert_eq!(meta.fallback_count, 1, "exactly one fallback hop");
    assert_eq!(meta.attempt_count, 2, "one attempt per chain entry");
    assert_eq!(
        meta.served_provider.as_deref(),
        Some("p-oauth"),
        "the OAuth entry is the target that served (attempted) last"
    );
    assert_eq!(meta.served_model.as_deref(), Some("m-oauth"));
    assert_eq!(meta.resolved_alias, "fast");

    // -- hop 1 received the ORIGINAL sampling, untouched.
    let seen = first.seen();
    assert_eq!(seen.len(), 1, "the first hop is dispatched exactly once");
    assert_eq!(
        seen[0].temperature,
        Some(CALLER_TEMPERATURE),
        "the first hop must receive the caller's temperature verbatim -- the \
         strip is scoped to the OAuth lane and must not reach back"
    );

    // -- hop 2 shipped a body with NO sampling and the stop preserved.
    let bodies = outgoing_bodies(&events);
    assert_eq!(
        bodies.len(),
        1,
        "only the OAuth hop assembles an outbound body; got: {bodies:?}"
    );
    let body = bodies[0];
    assert!(
        !body.contains("temperature"),
        "the OAuth hop must strip temperature before the wire; got: {body}"
    );
    assert!(
        !body.contains("top_p"),
        "the OAuth hop must strip top_p before the wire; got: {body}"
    );
    assert!(
        body.contains(CALLER_STOP),
        "stop_sequences is accepted by the seat and must survive; got: {body}"
    );

    // -- the caller's canonical request survived the walk unmutated.
    assert_eq!(
        caller_req.temperature,
        Some(CALLER_TEMPERATURE),
        "the strip mutates the assembled body, never the canonical request"
    );
    assert_eq!(
        caller_req.stop.as_deref(),
        Some(&[CALLER_STOP.to_string()][..])
    );

    // -- exactly one strip WARN, correlated to the logical request id.
    let warns = strip_warns(&events);
    assert_eq!(
        warns.len(),
        1,
        "one WARN for the one stripped hop; got: {warns:?}"
    );
    let warn = warns[0];
    assert_eq!(warn.field("provider"), Some("p-oauth"));
    assert_eq!(warn.field("lane"), Some("oauth-own-anthropic"));
    assert_eq!(
        warn.field("dropped_params"),
        Some("temperature"),
        "names only, and only the key actually present"
    );
    assert_eq!(
        warn.request_id.as_deref(),
        Some(REQUEST_ID),
        "the strip WARN must inherit the dispatching request's id so a \
         grep by request id shows the hop that dropped the knob"
    );
    assert!(
        !warn.message.contains(&CALLER_TEMPERATURE.to_string()),
        "the removed value must never be logged: {}",
        warn.message
    );
}

/// The streaming path takes the same cross-lane walk: the first hop fails
/// to open its stream, and the OAuth fallback hop assembles a stripped
/// body before halting. Pins that `stream` is not a second dispatch site
/// that forgot the strip.
#[tokio::test]
async fn cross_lane_stream_fallback_strips_sampling_on_the_oauth_hop() {
    let first = RecordingFirstHop::new("p-first");
    let oauth = oauth_lane_provider("p-oauth");
    let router = cross_lane_router(
        first.clone() as Arc<dyn Provider>,
        oauth as Arc<dyn Provider>,
    );

    let caller_req = sampling_req();
    let (dispatched, events) = capture_under_request_span(
        router.stream_with_options(caller_req.clone(), RouterOptions::new()),
    )
    .await;

    dispatched
        .result
        .err()
        .expect("the OAuth hop cannot resolve a token in-tree");
    assert_eq!(dispatched.meta.fallback_count, 1);
    assert_eq!(dispatched.meta.served_provider.as_deref(), Some("p-oauth"));

    assert_eq!(
        first.seen()[0].temperature,
        Some(CALLER_TEMPERATURE),
        "the first hop must receive the caller's temperature verbatim"
    );

    let bodies = outgoing_bodies(&events);
    assert_eq!(bodies.len(), 1, "got: {bodies:?}");
    assert!(
        !bodies[0].contains("temperature") && !bodies[0].contains("top_p"),
        "the streaming OAuth hop must strip sampling too; got: {}",
        bodies[0]
    );

    let warns = strip_warns(&events);
    assert_eq!(warns.len(), 1, "got: {warns:?}");
    assert_eq!(warns[0].request_id.as_deref(), Some(REQUEST_ID));
    assert_eq!(warns[0].field("dropped_params"), Some("temperature"));
}

/// The same walk with a `top_p`-only request. The assembly emits `top_p`
/// only when `temperature` is absent, so a temperature-seeded test proves
/// nothing about it -- this is the case that makes the cross-lane `top_p`
/// assertion load-bearing.
#[tokio::test]
async fn cross_lane_fallback_strips_top_p_when_temperature_is_absent() {
    let first = RecordingFirstHop::new("p-first");
    let oauth = oauth_lane_provider("p-oauth");
    let router = cross_lane_router(
        first.clone() as Arc<dyn Provider>,
        oauth as Arc<dyn Provider>,
    );

    let caller_req = ChatRequest {
        model: "fast".into(),
        max_tokens: Some(2048),
        top_p: Some(0.87),
        stop: Some(vec![CALLER_STOP.into()]),
        ..Default::default()
    };
    let (Dispatched { meta, result }, events) = capture_under_request_span(
        router.complete_with_options(caller_req.clone(), RouterOptions::new()),
    )
    .await;
    result.expect_err("the OAuth hop cannot resolve a token in-tree");
    assert_eq!(meta.fallback_count, 1);

    assert_eq!(
        first.seen()[0].top_p,
        Some(0.87),
        "the first hop must receive the caller's top_p verbatim"
    );

    let bodies = outgoing_bodies(&events);
    assert_eq!(bodies.len(), 1, "got: {bodies:?}");
    assert!(
        !bodies[0].contains("top_p"),
        "the OAuth hop must strip top_p; got: {}",
        bodies[0]
    );
    assert!(
        bodies[0].contains(CALLER_STOP),
        "stop_sequences must survive; got: {}",
        bodies[0]
    );

    let warns = strip_warns(&events);
    assert_eq!(warns.len(), 1, "got: {warns:?}");
    assert_eq!(warns[0].field("dropped_params"), Some("top_p"));
    assert_eq!(warns[0].request_id.as_deref(), Some(REQUEST_ID));

    assert_eq!(
        caller_req.top_p,
        Some(0.87),
        "the canonical request is never mutated by the strip"
    );
}
