//! Where the three dispatch surfaces (complete, stream, count_tokens) answer
//! the same upstream outcome differently. Each test is one behavior; each
//! row names its surface, and every row is driven through the public
//! `Router` entry points so the table holds whatever shape the walks take.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::stream::{self, BoxStream, StreamExt};
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result, TokenCount, ToolDef,
    UpstreamReason,
};
use serde_json::{Value, json};

use super::Router;
use super::scripted_provider_test_support::{Answer, ScriptedProvider};
use crate::config::{AliasValue, Config, ProviderEntry, ProviderRuntimePolicy};
use crate::learned_capability::{EntryVerdict, ExportedEntry};
use crate::resolved::ResolvedModel;
use crate::runtime_state::CircuitPhase;

const ALIAS: &str = "a";
const FIRST: &str = "m1";
const SECOND: &str = "m2";
const ANTHROPIC_KIND: &str = "anthropic-api";

#[derive(Clone, Copy, Debug)]
enum Surface {
    Complete,
    Stream,
    CountTokens,
}

const SURFACES: [Surface; 3] = [Surface::Complete, Surface::Stream, Surface::CountTokens];

impl Surface {
    const fn label(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Stream => "stream",
            Self::CountTokens => "count_tokens",
        }
    }
}

/// How a dispatch ended, reduced to what the rows compare.
#[derive(Debug, PartialEq, Eq)]
enum Ended {
    Served,
    Failed(String),
}

fn ended(outcome: Result<()>) -> Ended {
    match outcome {
        Ok(()) => Ended::Served,
        Err(err) => Ended::Failed(error_shape(&err)),
    }
}

/// The variant and the one field a row tells apart, never a body.
fn error_shape(err: &Error) -> String {
    match err {
        Error::Upstream { status, .. } => format!("upstream {status}"),
        Error::Validation(..) => "validation".into(),
        Error::UnknownProvider(..) => "unknown provider".into(),
        Error::NotImplemented(_, detail) => format!("not implemented: {detail}"),
        Error::Credential { .. } => "credential".into(),
        other => format!("other: {other:?}"),
    }
}

fn failed(shape: &str) -> Ended {
    Ended::Failed(shape.into())
}

/// Dispatch `req` on `surface` through its public entry point.
async fn dispatch(router: &Router, surface: Surface, req: ChatRequest) -> Ended {
    let outcome = match surface {
        Surface::Complete => Box::pin(router.complete(req)).await.map(drop),
        Surface::Stream => Box::pin(router.stream(req)).await.map(drop),
        Surface::CountTokens => Box::pin(router.count_tokens(req)).await.map(drop),
    };
    ended(outcome)
}

fn request() -> ChatRequest {
    ChatRequest {
        model: ALIAS.into(),
        ..Default::default()
    }
}

fn upstream_of(nickname: &str) -> String {
    format!("up-{nickname}")
}

fn provider_of(nickname: &str) -> String {
    format!("p-{nickname}")
}

/// An anthropic-api entry (count-capable) whose breaker opens on
/// `circuit_failures` debits and then stays open for a minute.
fn anthropic_entry(circuit_failures: Option<u32>) -> ProviderEntry {
    ProviderEntry::anthropic_api("literal:k").with_runtime(ProviderRuntimePolicy {
        circuit_failures,
        circuit_cooldown_ms: circuit_failures.map(|_| 60_000),
        ..ProviderRuntimePolicy::default()
    })
}

/// An openai-compat entry: no seat on it can count tokens.
fn count_incapable_entry() -> ProviderEntry {
    ProviderEntry::openai_compat("https://openai-compat.invalid/v1", "literal:k")
}

struct Seat {
    nickname: &'static str,
    entry: ProviderEntry,
    provider: Arc<dyn Provider>,
}

fn seat(nickname: &'static str, entry: ProviderEntry, provider: Arc<dyn Provider>) -> Seat {
    Seat {
        nickname,
        entry,
        provider,
    }
}

/// A router whose alias chains `seats` in order, each its own provider entry,
/// with one attempt per seat unless `tune` says otherwise.
fn router_over(seats: Vec<Seat>, tune: impl FnOnce(&mut Config)) -> Router {
    let mut config = Config::default();
    config.retry.max_attempts = 1;
    tune(&mut config);
    let mut models = BTreeMap::new();
    let mut chain = Vec::with_capacity(seats.len());
    for seat in seats {
        let provider_name = provider_of(seat.nickname);
        config.providers.insert(provider_name.clone(), seat.entry);
        let model = ResolvedModel::new(
            seat.nickname,
            provider_name,
            seat.provider,
            upstream_of(seat.nickname),
        );
        models.insert(seat.nickname.to_string(), Arc::new(model));
        chain.push(seat.nickname.to_string());
    }
    config
        .aliases
        .insert(ALIAS.into(), AliasValue::Chain(chain));
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

fn answering() -> Arc<ScriptedProvider> {
    ScriptedProvider::new(None, |_| Ok(()))
}

fn answering_with(answer: Answer) -> Arc<ScriptedProvider> {
    ScriptedProvider::new(None, answer)
}

fn unavailable(_: &ChatRequest) -> Result<()> {
    Err(Error::upstream("scripted", 503, "unavailable"))
}

fn not_implemented_on_the_wire(_: &ChatRequest) -> Result<()> {
    Err(Error::upstream("scripted", 501, "cannot count"))
}

fn not_implemented_locally(_: &ChatRequest) -> Result<()> {
    Err(Error::NotImplemented("scripted".into(), "count".into()))
}

fn unknown_provider(_: &ChatRequest) -> Result<()> {
    Err(Error::UnknownProvider("unregistered".into()))
}

/// Non-mutating breaker phase of the seat `nickname`.
fn phase(router: &Router, nickname: &str) -> CircuitPhase {
    router
        .capacity_snapshot_for(nickname, Instant::now())
        .expect("the seat has a state slot")
        .circuit
}

fn probe_slot_held(router: &Router, nickname: &str) -> bool {
    router
        .state
        .get(nickname)
        .expect("the seat has a state slot")
        .lock()
        .half_open_probe_in_flight()
}

fn advisor_request() -> ChatRequest {
    ChatRequest {
        tools: Some(vec![ToolDef::Other(
            json!({"type": "advisor", "name": "advisor"}),
        )]),
        ..request()
    }
}

/// A learned advisor negative on the seat `nickname`'s lane, expiring at
/// `expires_at`: in the future it acts, in the past it is due a re-probe.
fn advisor_negative(nickname: &str, expires_at: Instant) -> ExportedEntry {
    let seen = Instant::now();
    ExportedEntry {
        provider_kind: ANTHROPIC_KIND.into(),
        state_key: lane_key(nickname),
        feature_key: "advisor".into(),
        verdict: EntryVerdict::Negative,
        signal: SignalTier::SelfIdentifying,
        observations: 1,
        first_seen: seen,
        last_seen: seen,
        expires_at,
        evidence_class: None,
        phase: FailurePhase::F1,
        source: EvidenceSource::Live,
        in_flight: false,
        consecutive_failed_probes: 0,
    }
}

fn lane_key(nickname: &str) -> String {
    format!("{}#{}", provider_of(nickname), upstream_of(nickname))
}

/// `(surface, how it ended, calls the second seat took)`.
type WalkRow = (&'static str, Ended, usize);

/// `(surface, how it ended, calls the second seat took, first seat's breaker)`.
type HealthRow = (&'static str, Ended, usize, CircuitPhase);

/// Run one failing first seat ahead of an answering second seat on every
/// surface, each on a fresh router.
async fn walk_rows(first: Answer, circuit_failures: Option<u32>) -> Vec<HealthRow> {
    let mut observed = Vec::new();
    for surface in SURFACES {
        let second = answering();
        let router = router_over(
            vec![
                seat(
                    FIRST,
                    anthropic_entry(circuit_failures),
                    answering_with(first),
                ),
                seat(SECOND, anthropic_entry(None), second.clone()),
            ],
            |_| {},
        );

        let end = dispatch(&router, surface, request()).await;

        observed.push((surface.label(), end, second.calls(), phase(&router, FIRST)));
    }
    observed
}

#[tokio::test]
async fn a_strict_strip_refusal_walks_on_the_message_surfaces_and_is_terminal_on_count() {
    let mut observed: Vec<WalkRow> = Vec::new();
    for surface in SURFACES {
        let second = answering();
        let router = router_over(
            vec![
                seat(FIRST, anthropic_entry(None), answering()),
                seat(SECOND, anthropic_entry(None), second.clone()),
            ],
            |config| config.server.strict_translation = true,
        );
        router
            .learned_capabilities
            .import_entries(vec![advisor_negative(
                FIRST,
                Instant::now() + Duration::from_hours(48),
            )]);

        let end = dispatch(&router, surface, advisor_request()).await;

        observed.push((surface.label(), end, second.calls()));
    }

    let expected: Vec<WalkRow> = vec![
        ("complete", Ended::Served, 1),
        ("stream", Ended::Served, 1),
        ("count_tokens", failed("validation"), 0),
    ];
    assert_eq!(observed, expected);
}

#[tokio::test]
async fn an_unknown_provider_walks_on_the_message_surfaces_and_is_terminal_on_count() {
    let observed = walk_rows(unknown_provider, Some(1)).await;

    let expected: Vec<HealthRow> = vec![
        ("complete", Ended::Served, 1, CircuitPhase::Closed),
        ("stream", Ended::Served, 1, CircuitPhase::Closed),
        (
            "count_tokens",
            failed("unknown provider"),
            0,
            CircuitPhase::Closed,
        ),
    ];
    assert_eq!(observed, expected);
}

#[tokio::test]
async fn a_503_debits_on_every_surface_but_only_the_message_surfaces_walk_on() {
    let observed = walk_rows(unavailable, Some(1)).await;

    let expected: Vec<HealthRow> = vec![
        ("complete", Ended::Served, 1, CircuitPhase::Open),
        ("stream", Ended::Served, 1, CircuitPhase::Open),
        (
            "count_tokens",
            failed("upstream 503"),
            0,
            CircuitPhase::Open,
        ),
    ];
    assert_eq!(observed, expected);
}

#[tokio::test]
async fn a_wire_501_walks_without_a_debit_only_on_count() {
    let observed = walk_rows(not_implemented_on_the_wire, Some(1)).await;

    let expected: Vec<HealthRow> = vec![
        ("complete", Ended::Served, 1, CircuitPhase::Open),
        ("stream", Ended::Served, 1, CircuitPhase::Open),
        ("count_tokens", Ended::Served, 1, CircuitPhase::Closed),
    ];
    assert_eq!(observed, expected);
}

const BACKOFF: Duration = Duration::from_millis(250);

#[tokio::test(start_paused = true)]
async fn only_complete_retries_a_retryable_503_on_the_same_seat_after_a_backoff_sleep() {
    // `(surface, upstream calls, whether the walk slept at least one backoff)`.
    let mut observed: Vec<(&'static str, usize, bool)> = Vec::new();
    for surface in SURFACES {
        let only = answering_with(unavailable);
        let router = router_over(
            vec![seat(FIRST, anthropic_entry(None), only.clone())],
            |config| {
                config.retry.max_attempts = 2;
                config.retry.initial_backoff_ms = BACKOFF.as_millis() as u64;
                config.retry.jitter_ms = 0;
            },
        );
        let started = tokio::time::Instant::now();

        dispatch(&router, surface, request()).await;

        observed.push((surface.label(), only.calls(), started.elapsed() >= BACKOFF));
    }

    let expected = vec![
        ("complete", 2, true),
        ("stream", 1, false),
        ("count_tokens", 1, false),
    ];
    assert_eq!(observed, expected);
}

/// `(row, how its router is built, how the count ends)`.
type ExhaustionRow = (&'static str, fn() -> Router, Ended);

#[tokio::test]
async fn count_exhaustion_names_why_no_seat_counted_and_a_parked_seat_outranks_it() {
    let parked_then_refusing: fn() -> Router = || {
        let router = router_over(
            vec![
                seat(FIRST, anthropic_entry(None), answering()),
                seat(
                    SECOND,
                    anthropic_entry(None),
                    answering_with(not_implemented_on_the_wire),
                ),
            ],
            |_| {},
        );
        assert!(router.park_credential_for_tests(
            FIRST,
            UpstreamReason::CredentialInvalid,
            Instant::now() + Duration::from_hours(1),
        ));
        router
    };
    let rows: [ExhaustionRow; 3] = [
        (
            "no seat can count",
            || {
                router_over(
                    vec![seat(FIRST, count_incapable_entry(), answering())],
                    |_| {},
                )
            },
            failed("not implemented: count_tokens: no count_tokens-capable provider in chain"),
        ),
        (
            "every capable seat refuses",
            || {
                router_over(
                    vec![
                        seat(
                            FIRST,
                            anthropic_entry(None),
                            answering_with(not_implemented_on_the_wire),
                        ),
                        seat(
                            SECOND,
                            anthropic_entry(None),
                            answering_with(not_implemented_locally),
                        ),
                    ],
                    |_| {},
                )
            },
            failed(
                "not implemented: count_tokens: all capable providers returned a \
                 capability error (cannot count)",
            ),
        ),
        (
            "a parked seat outranks the refusal",
            parked_then_refusing,
            failed("credential"),
        ),
    ];

    for (name, build, expected) in rows {
        let router = build();

        let end = dispatch(&router, Surface::CountTokens, request()).await;

        assert_eq!(end, expected, "{name}");
    }
}

#[tokio::test]
async fn count_settles_a_re_probe_admission_up_front_while_the_message_surfaces_settle_it_on_transfer()
 {
    // `(surface, the re-probed negative's in_flight flag, or None once cleared)`.
    let mut observed: Vec<(&'static str, Option<bool>)> = Vec::new();
    for surface in SURFACES {
        let router = router_over(
            vec![seat(FIRST, anthropic_entry(None), answering())],
            |config| {
                config.capability.enabled = true;
            },
        );
        router
            .learned_capabilities
            .import_entries(vec![advisor_negative(
                FIRST,
                Instant::now()
                    .checked_sub(Duration::from_secs(1))
                    .expect("the clock is past one second"),
            )]);

        assert_eq!(
            dispatch(&router, surface, advisor_request()).await,
            Ended::Served
        );

        let negative = router
            .learned_capabilities
            .export_entries()
            .into_iter()
            .find(|e| e.state_key == lane_key(FIRST) && e.feature_key == "advisor");
        observed.push((surface.label(), negative.map(|e| e.in_flight)));
    }

    let expected = vec![
        ("complete", None),
        ("stream", None),
        ("count_tokens", Some(false)),
    ];
    assert_eq!(observed, expected);
}

/// A provider whose every surface reaches the upstream and then never
/// answers: a stream opens but yields no content.
#[derive(Default)]
struct PendingProvider {
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl Provider for PendingProvider {
    fn id(&self) -> &'static str {
        "pending"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("pending", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(stream::pending().boxed())
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
}

const CLIENT_GIVES_UP: Duration = Duration::from_millis(50);

#[tokio::test]
async fn a_dispatch_dropped_before_content_frees_the_probe_slot_without_a_debit() {
    // `(surface, dropped while pending, upstream calls, slot held, breaker)`.
    let mut observed: Vec<(&'static str, bool, usize, bool, CircuitPhase)> = Vec::new();
    for surface in SURFACES {
        let upstream = Arc::new(PendingProvider::default());
        let router = router_over(
            vec![seat(FIRST, anthropic_entry(Some(1)), upstream.clone())],
            |_| {},
        );
        assert!(router.force_open_breaker(FIRST, Duration::ZERO));

        let dropped = tokio::time::timeout(CLIENT_GIVES_UP, dispatch(&router, surface, request()))
            .await
            .is_err();

        observed.push((
            surface.label(),
            dropped,
            upstream.calls.load(Ordering::SeqCst),
            probe_slot_held(&router, FIRST),
            phase(&router, FIRST),
        ));
    }

    let expected: Vec<_> = SURFACES
        .iter()
        .map(|s| (s.label(), true, 1, false, CircuitPhase::HalfOpenReady))
        .collect();
    assert_eq!(observed, expected);
}

const TWICE: &str = "twice";

/// The committed first-party 400 refusing a `computer_20250124` tool: a
/// self-identifying capability rejection the learner records.
fn refuses_the_computer_tool(_: &ChatRequest) -> Result<()> {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../tests/fixtures/rejections/anthropic-api/tool-type-computer-unsupported-messages.json"
    ))
    .expect("valid tool-type fixture");
    Err(Error::upstream_full(
        "scripted",
        400,
        fixture["body"].as_str().expect("fixture body"),
        None,
        fixture["lifted_type"].as_str().map(str::to_string),
        None,
    ))
}

fn computer_tool_request() -> ChatRequest {
    ChatRequest {
        model: TWICE.into(),
        tools: Some(vec![ToolDef::Other(
            json!({"type": "computer_20250124", "name": "computer"}),
        )]),
        ..Default::default()
    }
}

/// `(surface, upstream calls, attempts, fallback hops, learn events, observations)`.
type RepeatRow = (&'static str, usize, u32, u32, usize, Option<u32>);

#[tokio::test]
async fn a_chain_naming_one_seat_twice_visits_it_twice_and_learns_its_rejection_once() {
    let mut observed: Vec<RepeatRow> = Vec::new();
    for surface in [Surface::Complete, Surface::Stream] {
        let only = answering_with(refuses_the_computer_tool);
        let router = router_over(
            vec![seat(FIRST, anthropic_entry(None), only.clone())],
            |config| {
                config.aliases.insert(
                    TWICE.into(),
                    AliasValue::Chain(vec![FIRST.into(), FIRST.into()]),
                );
            },
        );

        let meta = match surface {
            Surface::Complete => {
                Box::pin(router.complete_with_options(computer_tool_request(), Default::default()))
                    .await
                    .meta
            }
            Surface::Stream => {
                Box::pin(router.stream_with_options(computer_tool_request(), Default::default()))
                    .await
                    .meta
            }
            Surface::CountTokens => unreachable!("the count column is not pinned here"),
        };

        let observations = router
            .learned_capabilities
            .export_entries()
            .into_iter()
            .find(|e| e.state_key == lane_key(FIRST))
            .map(|e| e.observations);
        observed.push((
            surface.label(),
            only.calls(),
            meta.attempt_count,
            meta.fallback_count,
            meta.learned_capabilities.len(),
            observations,
        ));
    }

    let expected: Vec<RepeatRow> = vec![
        ("complete", 2, 2, 1, 1, Some(1)),
        ("stream", 2, 2, 1, 1, Some(1)),
    ];
    assert_eq!(observed, expected);
}
