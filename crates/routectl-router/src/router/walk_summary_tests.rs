//! The walk summary line: one INFO per request whose walk retried a target,
//! skipped an excluded one or recognized an upstream pattern, carrying counts
//! and closed tokens only.
//!
//! The chain is three single-seat models: `first` and `sibling` on one
//! origin, `other` on another. Every input the line must never carry (the
//! upstream body, its request-id header, an inbound client header, the seat's
//! account id and the shared origin) carries [`SENTINEL`].

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use routectl_auth::SecretRef;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result, ScopeRow};
use routectl_testkit::{CapturedEvent, with_capture};
use serde_json::{Value, json};

use super::endpoint_key;
use crate::config::{AliasValue, Config, ProviderEntry};
use crate::resolved::ResolvedModel;
use crate::router::scripted_provider_test_support::{content_chunk, ok_response};
use crate::router::{Router, RouterOptions};
use crate::seat_pool::SeatTarget;

const SENTINEL: &str = "zq7sentinel";
const ALIAS: &str = "chain";
const SHARED_ORIGIN: &str = "https://zq7sentinel.example.test";
const OTHER_ORIGIN: &str = "https://other.example.test";
const MESSAGE: &str = "walk summary";
const TARGET: &str = "routectl_router::router::walk_state";
const FIELDS: [&str; 9] = [
    "surface",
    "attempts",
    "retries",
    "request_caused",
    "excluded_endpoint",
    "excluded_model",
    "excluded_target",
    "excluded_probe_run",
    "recognizer",
];

/// How the head target answers every call; the other two always serve.
#[derive(Clone, Copy, Debug)]
enum Head {
    Serves,
    /// A generic 400: endpoint scope, not retried.
    BadRequest,
    /// A bare OAuth 429 the provider recognized: endpoint scope.
    RecognizedRateLimit,
    /// A 500: target scope, retried within the request's budget.
    ServerError,
}

struct Seat {
    head: Option<Head>,
    calls: AtomicUsize,
}

impl Seat {
    fn new(head: Option<Head>) -> Arc<Self> {
        Arc::new(Self {
            head,
            calls: AtomicUsize::new(0),
        })
    }

    fn answer(&self) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match self.head {
            None | Some(Head::Serves) => Ok(()),
            Some(Head::BadRequest) => Err(sentinel_error(400, "invalid_request_error")),
            Some(Head::RecognizedRateLimit) => Err(sentinel_error(429, "rate_limit_error")
                .with_scope_row(Some(ScopeRow::AnthropicBareRateLimit))),
            Some(Head::ServerError) => Err(sentinel_error(500, "api_error")),
        }
    }
}

/// An upstream error whose body and request-id header both carry the
/// sentinel.
fn sentinel_error(status: u16, kind: &str) -> Error {
    let body = json!({"type": "error", "error": {"type": kind, "message": SENTINEL}});
    Error::upstream_full(
        "scripted",
        status,
        body.to_string(),
        None,
        Some(kind.into()),
        None,
    )
    .with_upstream_request_id(Some(format!("req-{SENTINEL}")))
}

#[async_trait]
impl Provider for Seat {
    fn id(&self) -> &'static str {
        "scripted"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("scripted", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        self.answer().map(|()| ok_response())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.answer()
            .map(|()| stream::iter([Ok(content_chunk())]).boxed())
    }
}

fn entry(origin: &str) -> ProviderEntry {
    let text =
        format!("kind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\nbase_url = \"{origin}\"\n");
    toml::from_str(&text).expect("valid provider entry")
}

/// A one-seat model on provider entry `member`, its seat logged in under an
/// OAuth label that carries the sentinel.
fn seated(nickname: &str, member: &str, seat: &Arc<Seat>) -> Arc<ResolvedModel> {
    let provider = Arc::clone(seat) as Arc<dyn Provider>;
    let target = SeatTarget::new(
        member.to_owned(),
        Arc::clone(&provider),
        Some(SecretRef::OAuth {
            provider: "anthropic".into(),
            label: Some(SENTINEL.into()),
        }),
        false,
    );
    Arc::new(
        ResolvedModel::new(nickname, member, provider, format!("claude-{nickname}"))
            .with_seats(Arc::from(vec![target])),
    )
}

fn router(head: Head, max_attempts: u32) -> Router {
    router_over(head, max_attempts, &["first", "sibling", "other"])
}

/// [`router`] with the alias naming `chain` (a subset of the three models,
/// in order).
fn router_over(head: Head, max_attempts: u32, chain: &[&str]) -> Router {
    let mut config = Config::default();
    config.retry.max_attempts = max_attempts;
    config.retry.initial_backoff_ms = 0;
    config.retry.jitter_ms = 0;
    config
        .providers
        .insert("first-seat".into(), entry(SHARED_ORIGIN));
    config
        .providers
        .insert("sibling-seat".into(), entry(SHARED_ORIGIN));
    config
        .providers
        .insert("other-seat".into(), entry(OTHER_ORIGIN));
    config.aliases.insert(
        ALIAS.into(),
        AliasValue::Chain(chain.iter().map(|name| (*name).into()).collect()),
    );
    let models = BTreeMap::from([
        (
            "first".to_owned(),
            seated("first", "first-seat", &Seat::new(Some(head))),
        ),
        (
            "sibling".to_owned(),
            seated("sibling", "sibling-seat", &Seat::new(None)),
        ),
        (
            "other".to_owned(),
            seated("other", "other-seat", &Seat::new(None)),
        ),
    ]);
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

/// A request whose inbound client header carries the sentinel.
fn request() -> ChatRequest {
    let mut req = ChatRequest {
        model: ALIAS.into(),
        messages: vec![].into(),
        max_tokens: Some(64),
        ..Default::default()
    };
    req.routectl_internal.claude_code_headers =
        vec![("x-claude-code-session-id".into(), SENTINEL.into())];
    req
}

#[derive(Clone, Copy, Debug)]
enum Surface {
    Complete,
    Stream,
}

async fn walk(router: &Router, surface: Surface) -> Vec<CapturedEvent> {
    match surface {
        Surface::Complete => {
            let (dispatched, events) = with_capture(Box::pin(
                router.complete_with_options(request(), RouterOptions::new()),
            ))
            .await;
            assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
            events
        }
        Surface::Stream => {
            let (dispatched, events) = with_capture(Box::pin(
                router.stream_with_options(request(), RouterOptions::new()),
            ))
            .await;
            assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
            events
        }
    }
}

fn summaries(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events.iter().filter(|e| e.message == MESSAGE).collect()
}

fn fields_of(event: &CapturedEvent) -> Vec<(&str, &str)> {
    event
        .fields
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect()
}

/// A walk that excluded a target logs the summary once, at INFO, with exactly
/// the documented fields; each row's counts and recognizer differ, so a field
/// read from the wrong tally fails a row.
#[tokio::test]
async fn a_walk_that_took_a_detour_logs_one_summary_with_the_exact_fields() {
    type Row = (&'static str, Head, u32, Surface, [&'static str; 9]);
    let rows: [Row; 4] = [
        (
            "generic 400 skips the sibling on its origin",
            Head::BadRequest,
            3,
            Surface::Complete,
            ["complete", "2", "0", "1", "1", "0", "0", "0", "none"],
        ),
        (
            "the same walk on a stream",
            Head::BadRequest,
            3,
            Surface::Stream,
            ["stream", "2", "0", "1", "1", "0", "0", "0", "none"],
        ),
        (
            "recognized 429 names its recognizer",
            Head::RecognizedRateLimit,
            3,
            Surface::Complete,
            [
                "complete",
                "2",
                "0",
                "1",
                "1",
                "0",
                "0",
                "0",
                "anthropic-bare-rate-limit",
            ],
        ),
        (
            "500 retried once then walked on",
            Head::ServerError,
            2,
            Surface::Complete,
            ["complete", "3", "1", "0", "0", "0", "0", "0", "none"],
        ),
    ];
    for (name, head, max_attempts, surface, expected) in rows {
        let router = router(head, max_attempts);

        let events = walk(&router, surface).await;

        let found = summaries(&events);
        assert_eq!(
            found.len(),
            1,
            "{name}: one summary per request: {found:#?}"
        );
        let summary = found[0];
        assert_eq!(summary.level, tracing::Level::INFO, "{name}");
        assert_eq!(summary.target, TARGET, "{name}");
        let want: Vec<(&str, &str)> = FIELDS.into_iter().zip(expected).collect();
        assert_eq!(fields_of(summary), want, "{name}");
    }
}

/// A first-try success took no detour and logs no summary. The control is the
/// same chain with a failing head, which does.
#[tokio::test]
async fn a_first_try_success_logs_no_summary() {
    for surface in [Surface::Complete, Surface::Stream] {
        let quiet = walk(&router(Head::Serves, 3), surface).await;
        let control = walk(&router(Head::BadRequest, 3), surface).await;

        assert!(summaries(&quiet).is_empty(), "{surface:?}: {quiet:#?}");
        assert_eq!(
            summaries(&control).len(),
            1,
            "{surface:?}: the control logs"
        );
    }
}

/// No summary field carries the upstream body, its request-id header, an
/// inbound header, the seat's account id or its endpoint, nor anything
/// derived from them: every value is a count or a closed token.
///
/// Positive control: each sentinel-bearing input is asserted to carry the
/// sentinel, and the same capture surfaces it on another line of the walk
/// (the fallback WARN prints the upstream error), so a summary that copied
/// any of them would be seen.
#[tokio::test]
async fn the_summary_carries_no_upstream_request_credential_or_endpoint_text() {
    let router = router(Head::BadRequest, 3);
    let chain = router
        .dispatch_chain(ALIAS, None)
        .expect("the chain resolves");
    let head = &chain[0];
    let req = request();
    let err = sentinel_error(400, "invalid_request_error");
    let endpoint = endpoint_key("first-seat", router.config.providers.get("first-seat"));

    let events = walk(&router, Surface::Complete).await;

    let inputs = [
        ("upstream body", format!("{err:?}")),
        (
            "upstream request-id header",
            err.upstream_request_id().unwrap_or_default().to_owned(),
        ),
        (
            "inbound header",
            format!("{:?}", req.routectl_internal.claude_code_headers),
        ),
        (
            "account id",
            head.account.as_deref().unwrap_or_default().to_owned(),
        ),
        ("endpoint", endpoint.to_string()),
    ];
    for (input, text) in &inputs {
        assert!(
            text.contains(SENTINEL),
            "premise: the {input} carries the sentinel: {text}"
        );
    }
    let elsewhere = events
        .iter()
        .filter(|e| e.message != MESSAGE)
        .any(|e| e.fields.iter().any(|(_, v)| v.contains(SENTINEL)));
    assert!(
        elsewhere,
        "control: the capture surfaces the sentinel on another line"
    );
    let found = summaries(&events);
    assert_eq!(found.len(), 1);
    for (name, value) in fields_of(found[0]) {
        assert!(!value.contains(SENTINEL), "{name} = {value}");
        let closed = value.parse::<u32>().is_ok()
            || matches!(value, "complete" | "stream" | "count_tokens" | "none")
            || ScopeRow::from_token(value).is_some();
        assert!(
            closed,
            "{name} = {value} is neither a count nor a closed token"
        );
    }
}

/// A walk whose only detour is a failure the request caused still logs the
/// summary, carrying the count: a generic 400 on `first` with `other`, on
/// another origin, next in the chain skips nothing, retries nothing and
/// recognizes nothing.
///
/// Mutation check: drop the request-caused clause from `took_detours` -> red
/// (no summary is logged).
#[tokio::test]
async fn walk_summary_line_carries_request_caused() {
    for surface in [Surface::Complete, Surface::Stream] {
        let router = router_over(Head::BadRequest, 3, &["first", "other"]);

        let events = walk(&router, surface).await;

        let found = summaries(&events);
        assert_eq!(found.len(), 1, "{surface:?}: {events:#?}");
        let surface_token = match surface {
            Surface::Complete => "complete",
            Surface::Stream => "stream",
        };
        let expected = [surface_token, "2", "0", "1", "0", "0", "0", "0", "none"];
        let want: Vec<(&str, &str)> = FIELDS.into_iter().zip(expected).collect();
        assert_eq!(fields_of(found[0]), want, "{surface:?}");
    }
}
