//! Shared rig for the opening-count handler tests: an in-process scripted
//! upstream on an ephemeral loopback port, routers over it, and one-turn
//! drivers through `ingress_handle` that return the client-visible frames.
//!
//! The upstream answers each path from a script that can hold the response
//! head back and space the body pieces out in time, which is what puts a
//! dispatch on either side of the stream flush grace. Scripts are selected
//! by a marker substring of the request body, so one path can serve a fast
//! and a slow turn of the same conversation.

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use axum::body::{Body, Bytes};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use parking_lot::Mutex;
use routectl_core::ChatRequest;
use routectl_router::{AliasValue, Config, ModelEntry, ProviderEntry, RetryPolicy, Router};
use serde_json::{Value, json};

use super::ingress_handle;
use crate::ingress::IngressAdapter;
use crate::ingress::anthropic::AnthropicIngress;
use crate::ingress::anthropic::context_anchor::{RequestIdentity, anchored_input};
use crate::server::AppState;

/// Longer than the handler's flush grace, so a head held this long always
/// resolves the dispatch after the grace expired.
pub(super) const PAST_GRACE: Duration = Duration::from_millis(3_200);

pub(super) const SESSION: &str = "meter-e2e-session";

/// One scripted upstream response.
#[derive(Clone)]
pub(super) struct Script {
    head_delay: Duration,
    status: u16,
    content_type: &'static str,
    pieces: Vec<(Duration, String)>,
}

impl Script {
    /// An immediate 200 SSE response carrying `body` in one piece.
    pub(super) fn sse(body: String) -> Self {
        Self {
            head_delay: Duration::ZERO,
            status: 200,
            content_type: "text/event-stream",
            pieces: vec![(Duration::ZERO, body)],
        }
    }

    /// An immediate 200 JSON response.
    pub(super) fn json(body: &Value) -> Self {
        Self {
            content_type: "application/json",
            ..Self::sse(body.to_string())
        }
    }

    /// An immediate JSON error response with HTTP `status`.
    pub(super) fn error(status: u16, body: &Value) -> Self {
        Self {
            status,
            ..Self::json(body)
        }
    }

    /// Hold the response head back for `delay`.
    pub(super) fn with_head_delay(mut self, delay: Duration) -> Self {
        self.head_delay = delay;
        self
    }

    /// Send `first` at once, then `rest` after `gap`.
    pub(super) fn split(first: String, gap: Duration, rest: String) -> Self {
        Self {
            pieces: vec![(Duration::ZERO, first), (gap, rest)],
            ..Self::sse(String::new())
        }
    }
}

#[derive(Default)]
struct Scripts {
    /// path -> (body marker, script); the first matching marker wins, and
    /// an empty marker matches every body.
    by_path: BTreeMap<String, Vec<(String, Script)>>,
    hits: Vec<String>,
}

/// The scripted upstream server.
#[derive(Clone)]
pub struct Upstream {
    base: String,
    scripts: Arc<Mutex<Scripts>>,
}

impl Upstream {
    pub async fn start() -> Self {
        let scripts: Arc<Mutex<Scripts>> = Arc::default();
        let app = axum::Router::new()
            .fallback(serve_script)
            .with_state(Arc::clone(&scripts));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let base = format!("http://{}", listener.local_addr().expect("local addr"));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Self { base, scripts }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// Serve `script` on `path` for bodies containing `marker`.
    pub(super) fn mount(&self, path: &str, marker: &str, script: Script) {
        self.scripts
            .lock()
            .by_path
            .entry(path.to_string())
            .or_default()
            .push((marker.to_string(), script));
    }

    /// How many requests reached `path`.
    pub(super) fn hits(&self, path: &str) -> usize {
        self.scripts
            .lock()
            .hits
            .iter()
            .filter(|hit| *hit == path)
            .count()
    }
}

async fn serve_script(
    axum::extract::State(scripts): axum::extract::State<Arc<Mutex<Scripts>>>,
    uri: Uri,
    body: Bytes,
) -> Response {
    let path = uri.path().to_string();
    let text = String::from_utf8_lossy(&body).into_owned();
    let script = {
        let mut scripts = scripts.lock();
        scripts.hits.push(path.clone());
        scripts.by_path.get(&path).and_then(|entries| {
            entries
                .iter()
                .find(|(marker, _)| text.contains(marker.as_str()))
                .map(|(_, script)| script.clone())
        })
    };
    let Some(script) = script else {
        return StatusCode::NOT_FOUND.into_response();
    };
    tokio::time::sleep(script.head_delay).await;
    let pieces = futures::stream::iter(script.pieces).then(|(gap, piece)| async move {
        tokio::time::sleep(gap).await;
        Ok::<Bytes, Infallible>(Bytes::from(piece))
    });
    Response::builder()
        .status(script.status)
        .header("content-type", script.content_type)
        .body(Body::from_stream(pieces))
        .expect("scripted response builds")
}

// ------------------------------------------------------------ upstream bodies

fn sse_event(name: &str, data: &Value) -> String {
    format!("event: {name}\ndata: {data}\n\n")
}

/// Input-side usage of an Anthropic event: `input_tokens` plus the disjoint
/// cache fields, with the per-TTL breakdown split 5m/1h from the write.
#[derive(Clone, Copy)]
pub(super) struct InputUsage {
    pub(super) input: u64,
    pub(super) cache_write_5m: u64,
    pub(super) cache_write_1h: u64,
    pub(super) cache_read: u64,
}

impl InputUsage {
    pub(super) const fn plain(input: u64) -> Self {
        Self {
            input,
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_read: 0,
        }
    }

    pub(super) const fn cache_write(self) -> u64 {
        self.cache_write_5m + self.cache_write_1h
    }

    /// The provider's cache-inclusive input total.
    pub(super) const fn total(self) -> u64 {
        self.input + self.cache_write() + self.cache_read
    }

    fn json(self, output: u64) -> Value {
        json!({
            "input_tokens": self.input,
            "output_tokens": output,
            "cache_creation_input_tokens": self.cache_write(),
            "cache_read_input_tokens": self.cache_read,
            "cache_creation": {
                "ephemeral_5m_input_tokens": self.cache_write_5m,
                "ephemeral_1h_input_tokens": self.cache_write_1h,
            },
        })
    }
}

/// Anthropic `message_start`, with the upstream's opening usage when given.
pub(super) fn anthropic_start(opening: Option<InputUsage>) -> String {
    let mut message = json!({
        "id": "msg_meter", "type": "message", "role": "assistant", "content": [],
        "model": "claude-opus-4-7", "stop_reason": null, "stop_sequence": null,
    });
    if let Some(opening) = opening {
        message["usage"] = opening.json(1);
    }
    sse_event(
        "message_start",
        &json!({"type": "message_start", "message": message}),
    )
}

/// One text content block.
pub(super) fn anthropic_content() -> String {
    [
        sse_event(
            "content_block_start",
            &json!({"type": "content_block_start", "index": 0,
                    "content_block": {"type": "text", "text": ""}}),
        ),
        sse_event(
            "content_block_delta",
            &json!({"type": "content_block_delta", "index": 0,
                    "delta": {"type": "text_delta", "text": "ok"}}),
        ),
        sse_event(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": 0}),
        ),
    ]
    .concat()
}

/// The terminal `message_delta` (with the terminal usage when given) and
/// `message_stop`.
pub(super) fn anthropic_end(terminal: Option<InputUsage>) -> String {
    let mut delta = json!({
        "type": "message_delta",
        "delta": {"stop_reason": "end_turn", "stop_sequence": null},
    });
    if let Some(terminal) = terminal {
        delta["usage"] = terminal.json(9);
    }
    sse_event("message_delta", &delta)
        + &sse_event("message_stop", &json!({"type": "message_stop"}))
}

/// A terminal `message_delta` reporting output only (the first-party
/// Anthropic shape), then `message_stop`.
pub(super) fn anthropic_end_output_only() -> String {
    sse_event(
        "message_delta",
        &json!({"type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": {"output_tokens": 9}}),
    ) + &sse_event("message_stop", &json!({"type": "message_stop"}))
}

/// Anthropic content through the terminal `message_stop`.
pub(super) fn anthropic_rest(terminal: Option<InputUsage>) -> String {
    anthropic_content() + &anthropic_end(terminal)
}

pub(super) fn anthropic_stream(
    opening: Option<InputUsage>,
    terminal: Option<InputUsage>,
) -> String {
    anthropic_start(opening) + &anthropic_rest(terminal)
}

/// An in-band Anthropic error event, which fails the attempt it rides on.
pub(super) fn anthropic_overloaded() -> String {
    sse_event(
        "error",
        &json!({"type": "error", "error": {"type": "overloaded_error", "message": "busy"}}),
    )
}

/// An OpenAI-compatible stream: one content chunk, a finish chunk, and --
/// when `prompt_tokens` is given -- a usage chunk with a cached-token detail.
pub(super) fn openai_stream(prompt_tokens: Option<u64>) -> String {
    let data = |v: Value| format!("data: {v}\n\n");
    let mut out = data(json!({"id": "c1", "model": "glm-4.6", "choices": [
        {"index": 0, "delta": {"role": "assistant", "content": "ok"}, "finish_reason": null}]}));
    out += &data(json!({"id": "c1", "model": "glm-4.6", "choices": [
        {"index": 0, "delta": {}, "finish_reason": "stop"}]}));
    if let Some(prompt) = prompt_tokens {
        out += &data(
            json!({"id": "c1", "model": "glm-4.6", "choices": [], "usage": {
            "prompt_tokens": prompt, "completion_tokens": 3, "total_tokens": prompt + 3,
            "prompt_tokens_details": {"cached_tokens": prompt / 2}}}),
        );
    }
    out + "data: [DONE]\n\n"
}

/// An OpenAI-compatible stream whose only usage is an INTERIM usage-only
/// chunk before the finish, then a clean end with no terminal usage.
pub(super) fn openai_interim_only_stream(interim_prompt: u64) -> String {
    let data = |v: Value| format!("data: {v}\n\n");
    let mut out = data(json!({"id": "c1", "model": "glm-4.6", "choices": [
        {"index": 0, "delta": {"role": "assistant", "content": "ok"}, "finish_reason": null}]}));
    out += &data(
        json!({"id": "c1", "model": "glm-4.6", "choices": [], "usage": {
        "prompt_tokens": interim_prompt, "completion_tokens": 1,
        "total_tokens": interim_prompt + 1}}),
    );
    out += &data(json!({"id": "c1", "model": "glm-4.6", "choices": [
        {"index": 0, "delta": {}, "finish_reason": "stop"}]}));
    out + "data: [DONE]\n\n"
}

// ------------------------------------------------------------------ routers

pub(super) fn anthropic_provider(base: &str, prefix: &str) -> ProviderEntry {
    ProviderEntry::anthropic_api(crate::test_secret::file_ref("k"))
        .with_base_url(format!("{base}{prefix}"))
}

pub(super) fn compat_provider(base: &str, prefix: &str) -> ProviderEntry {
    ProviderEntry::openai_compat(
        format!("{base}{prefix}/v1"),
        crate::test_secret::file_ref("k"),
    )
}

/// A config with the given providers, models (nickname -> provider,
/// upstream) and aliases, one attempt per target.
pub(super) fn config(
    providers: Vec<(&str, ProviderEntry)>,
    models: &[(&str, &str, &str)],
    aliases: &[(&str, &[&str])],
) -> Config {
    let mut retry = RetryPolicy::default();
    retry.max_attempts = 1;
    Config {
        providers: providers
            .into_iter()
            .map(|(name, entry)| (name.to_string(), entry))
            .collect(),
        models: models
            .iter()
            .map(|(nick, provider, upstream)| {
                ((*nick).to_string(), ModelEntry::new(*provider, *upstream))
            })
            .collect(),
        aliases: aliases
            .iter()
            .map(|(alias, chain)| {
                let value = match chain {
                    [one] => AliasValue::Single((*one).to_string()),
                    many => AliasValue::Chain(many.iter().map(|m| (*m).to_string()).collect()),
                };
                ((*alias).to_string(), value)
            })
            .collect(),
        retry,
        ..Default::default()
    }
}

pub(super) async fn build(config: Config) -> Router {
    let secrets: Arc<dyn routectl_auth::SecretStore> = Arc::new(TestSecrets);
    crate::server::build_router_from_config(Arc::new(config), secrets)
        .await
        .expect("router builds")
}

/// Resolves `file://` refs from disk and any `oauth://` ref to a fixed test
/// bearer, so a pool of OAuth seats builds without a login.
struct TestSecrets;

#[async_trait::async_trait]
impl routectl_auth::SecretStore for TestSecrets {
    async fn get(&self, secret_ref: &routectl_auth::SecretRef) -> routectl_core::Result<String> {
        match secret_ref {
            routectl_auth::SecretRef::OAuth { .. } => Ok("test-oauth-bearer".to_string()),
            other => routectl_auth::MemoryStore::new().get(other).await,
        }
    }
    async fn set(&self, _: &routectl_auth::SecretRef, _: &str) -> routectl_core::Result<()> {
        Ok(())
    }
    async fn delete(&self, _: &routectl_auth::SecretRef) -> routectl_core::Result<()> {
        Ok(())
    }
}

/// An OAuth-authenticated Anthropic seat at `prefix`.
pub(super) fn oauth_seat(base: &str, prefix: &str, name: &str) -> ProviderEntry {
    ProviderEntry::anthropic_api(format!("oauth://{name}")).with_base_url(format!("{base}{prefix}"))
}

/// A daemon state over `router`, published once so a later republication
/// draws a strictly newer generation.
pub(super) fn daemon(router: Router) -> (Arc<AppState>, tempfile::TempDir) {
    router.publish_probe_incarnation();
    AppState::for_test(Arc::new(ArcSwap::from_pointee(router)))
}

/// Hot-reload `state` onto `next`, carrying the published Router's shared
/// state across the way a reload does.
pub(super) fn reload(state: &AppState, mut next: Router) {
    let current = state.router.load_full();
    next.carry_over_learned_from(&current);
    next.publish_probe_incarnation();
    assert!(next.publication_generation() > current.publication_generation());
    state.router.store(Arc::new(next));
}

// ---------------------------------------------------------------- requests

pub(super) fn text(role: &str, content: &str) -> Value {
    json!({"role": role, "content": content})
}

/// An Anthropic Messages body for `model` over `messages`.
pub(super) fn messages_body(model: &str, messages: &[Value], stream: bool) -> Value {
    json!({
        "model": model,
        "max_tokens": 256,
        "stream": stream,
        "system": "You are a careful coding agent.",
        "messages": messages,
    })
}

pub(super) fn session_headers(session: Option<&str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    if let Some(session) = session {
        headers.insert(
            "x-claude-code-session-id",
            HeaderValue::from_str(session).expect("ascii session"),
        );
    }
    headers
}

pub(super) fn canonical(body: &Value) -> ChatRequest {
    AnthropicIngress
        .parse_request(
            &session_headers(Some(SESSION)),
            &serde_json::to_vec(body).unwrap(),
        )
        .expect("fixture parses")
}

/// The display estimate the handler reports for a cold turn.
pub(super) fn raw_estimate(body: &Value) -> u64 {
    routectl_router::estimate_meter_tokens(&canonical(body))
}

/// The anchored opening `next` gets from a prior turn over `prior` whose
/// provider-observed input was `actual`.
pub(super) fn expected_anchor(prior: &Value, next: &Value, actual: u64) -> u64 {
    let prior = RequestIdentity::measure(&canonical(prior), None);
    let next = RequestIdentity::measure(&canonical(next), Some(prior.message_count()));
    anchored_input(
        actual,
        prior.normalized_estimate(),
        next.normalized_estimate(),
    )
}

// ------------------------------------------------------------------ frames

/// One client-visible SSE frame and when it arrived.
pub(super) struct Frame {
    pub(super) at: Instant,
    pub(super) event: Option<String>,
    pub(super) data: Value,
}

/// A whole client-visible stream.
pub(super) struct Turn {
    pub(super) status: StatusCode,
    pub(super) started: Instant,
    pub(super) frames: Vec<Frame>,
    pub(super) raw: String,
}

impl Turn {
    pub(super) fn named(&self, name: &str) -> Vec<&Frame> {
        self.frames
            .iter()
            .filter(|f| f.event.as_deref() == Some(name))
            .collect()
    }

    /// The single `message_start`, asserting it is the first frame and
    /// appears exactly once.
    pub(super) fn opening(&self) -> &Value {
        let starts = self.named("message_start");
        assert_eq!(starts.len(), 1, "exactly one message_start: {}", self.raw);
        assert_eq!(
            self.frames.first().and_then(|f| f.event.as_deref()),
            Some("message_start"),
            "message_start is the first frame: {}",
            self.raw
        );
        &starts[0].data["message"]["usage"]
    }

    pub(super) fn opening_input(&self) -> u64 {
        self.opening()["input_tokens"]
            .as_u64()
            .expect("opening input_tokens")
    }

    pub(super) fn terminal_usage(&self) -> &Value {
        let deltas = self.named("message_delta");
        &deltas.last().expect("a terminal message_delta").data["usage"]
    }
}

/// Drive one streamed or non-streamed request through the handler and read
/// the response to its end, stamping when each frame arrived.
pub(super) async fn send<A: IngressAdapter + 'static>(
    state: &Arc<AppState>,
    adapter: A,
    headers: HeaderMap,
    body: &Value,
) -> Turn {
    let started = Instant::now();
    let bytes = Bytes::from(serde_json::to_vec(body).unwrap());
    let resp = ingress_handle(Arc::clone(state), headers, None, Ok(bytes), adapter).await;
    let status = resp.status();
    let mut stream = resp.into_body().into_data_stream();
    let mut raw = String::new();
    let mut frames = Vec::new();
    let mut pending = String::new();
    while let Some(piece) = stream.next().await {
        let piece = String::from_utf8(piece.expect("body piece").to_vec()).expect("utf8 body");
        raw.push_str(&piece);
        pending.push_str(&piece);
        while let Some(end) = pending.find("\n\n") {
            let block: String = pending.drain(..end + 2).collect();
            if let Some(frame) = parse_frame(&block) {
                frames.push(frame);
            }
        }
    }
    Turn {
        status,
        started,
        frames,
        raw,
    }
}

fn parse_frame(block: &str) -> Option<Frame> {
    let mut event = None;
    let mut data = None;
    for line in block.lines() {
        if let Some(v) = line.strip_prefix("event:") {
            event = Some(v.trim().to_string());
        } else if let Some(v) = line.strip_prefix("data:") {
            data = Some(v.trim().to_string());
        }
    }
    let data = data?;
    Some(Frame {
        at: Instant::now(),
        event,
        data: serde_json::from_str(&data).unwrap_or(Value::String(data)),
    })
}

/// One Anthropic-dialect turn for `SESSION`.
pub(super) async fn anthropic_turn(state: &Arc<AppState>, body: &Value) -> Turn {
    let turn = send(
        state,
        AnthropicIngress,
        session_headers(Some(SESSION)),
        body,
    )
    .await;
    assert_eq!(turn.status, StatusCode::OK, "turn failed: {}", turn.raw);
    turn
}

// ----------------------------------------------------- shared conversation

pub(super) const GRACE: Duration = Duration::from_millis(2_500);

/// A cache-heavy opening with a split TTL breakdown.
pub(super) const OPENER: InputUsage = InputUsage {
    input: 13,
    cache_write_5m: 300,
    cache_write_1h: 17,
    cache_read: 41_000,
};

/// Terminal usage that legitimately differs from the opening (server-side
/// work added input mid-response).
pub(super) const TERMINAL: InputUsage = InputUsage {
    input: 29,
    cache_write_5m: 300,
    cache_write_1h: 17,
    cache_read: 41_000,
};

pub(super) fn turn_one() -> Vec<Value> {
    vec![text(
        "user",
        "Summarize the repository layout, caf\u{e9} \u{1f600}.",
    )]
}

pub(super) fn turn_two() -> Vec<Value> {
    let mut messages = turn_one();
    messages.push(text("assistant", "The workspace has one member crate."));
    messages.push(text("user", "Now list its dependencies."));
    messages
}

pub(super) fn turn_three() -> Vec<Value> {
    let mut messages = turn_two();
    messages.push(text("assistant", "It depends on serde."));
    messages.push(text("user", "Which version?"));
    messages
}

/// A single-lane Anthropic router at `/anth`.
pub(super) async fn anthropic_daemon(upstream: &Upstream) -> (Arc<AppState>, tempfile::TempDir) {
    daemon(
        build(config(
            vec![("anth", anthropic_provider(upstream.base(), "/anth"))],
            &[("opus", "anth", "claude-opus-4-7")],
            &[("claude-opus", &["opus"])],
        ))
        .await,
    )
}

pub(super) fn translated_config(base: &str, upstream_model: &str) -> routectl_router::Config {
    config(
        vec![("compat", compat_provider(base, "/compat"))],
        &[("glm", "compat", upstream_model)],
        &[("claude-opus", &["glm"]), ("claude-haiku", &["glm"])],
    )
}

/// A single translated (openai-compat) lane at `/compat`.
pub(super) async fn translated_daemon(upstream: &Upstream) -> (Arc<AppState>, tempfile::TempDir) {
    daemon(build(translated_config(upstream.base(), "glm-4.6")).await)
}

pub(super) fn body(messages: &[Value]) -> Value {
    messages_body("claude-opus", messages, true)
}
