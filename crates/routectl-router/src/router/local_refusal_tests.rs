//! A request routectl refuses on its own counts once on
//! `rc_request_refused_local_total` under its lane and reason, whichever
//! router exit it leaves through, a missing forwarded bearer included; an
//! upstream rejection does not count.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::stream::{BoxStream, StreamExt};
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use routectl_core::wire_check::{IngressDialect, IngressWire};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, ChunkChoice, ChunkDelta, ContentPart, Error, Message,
    MessageContent, Provider, RefusalReason, RequestProvenance, Result, Role, TokenCount, ToolDef,
};
use routectl_providers::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use serde_json::{Value, json};

use crate::config::{AliasValue, Config, CredentialSource, ProviderEntry};
use crate::learned_capability::{EntryVerdict, ExportedEntry};
use crate::resolved::ResolvedModel;
use crate::router::Router;

const ALIAS: &str = "a";
const NICKNAME: &str = "m";
const PROVIDER: &str = "p";
const UPSTREAM: &str = "upstream";
const ANTHROPIC_LANE: &str = "anthropic->anthropic-api";

/// A provider whose every call fails with one fixed error.
struct FailingProvider {
    error: fn() -> Error,
}

#[async_trait::async_trait]
impl Provider for FailingProvider {
    fn id(&self) -> &'static str {
        "failing"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("failing", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err((self.error)())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err((self.error)())
    }
}

/// A provider whose `n`th call fails with the `n`th error of `errors`.
struct SequencedProvider {
    errors: &'static [fn() -> Error],
    calls: AtomicUsize,
}

impl SequencedProvider {
    fn next_error(&self) -> Error {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        (self.errors[n % self.errors.len()])()
    }
}

#[async_trait::async_trait]
impl Provider for SequencedProvider {
    fn id(&self) -> &'static str {
        "sequenced"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("sequenced", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(self.next_error())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(self.next_error())
    }
}

/// A provider that answers every call successfully, counting the calls.
#[derive(Default)]
struct AnsweringProvider {
    calls: AtomicUsize,
}

impl AnsweringProvider {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Provider for AnsweringProvider {
    fn id(&self) -> &'static str {
        "answering"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("answering", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(ChatResponse::default())
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(TokenCount {
            input_tokens: 1,
            extras: serde_json::Map::new(),
        })
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let chunk = ChatChunk {
            choices: vec![ChunkChoice {
                index: 0,
                delta: ChunkDelta {
                    content: Some("ok".into()),
                    ..Default::default()
                },
                finish_reason: None,
                matched_stop_sequence: None,
            }],
            ..Default::default()
        };
        Ok(futures::stream::iter([Ok(chunk)]).boxed())
    }
}

/// A router whose alias names one anthropic-api model served by `provider`,
/// with no retries and the given `[server] strict_translation`.
fn router_over(entry: ProviderEntry, provider: Arc<dyn Provider>, strict: bool) -> Router {
    let mut config = Config::default();
    config.server.strict_translation = strict;
    config.retry.max_attempts = 1;
    config.providers.insert(PROVIDER.into(), entry);
    config
        .aliases
        .insert(ALIAS.into(), AliasValue::Single(NICKNAME.into()));
    let mut router = Router::new(Arc::new(config));
    let model = ResolvedModel::new(NICKNAME, PROVIDER, provider, UPSTREAM);
    router.install_resolved_models(BTreeMap::from([(NICKNAME.to_string(), Arc::new(model))]));
    router
}

fn failing_router(error: fn() -> Error) -> Router {
    router_over(
        ProviderEntry::anthropic_api("literal:k"),
        Arc::new(FailingProvider { error }),
        false,
    )
}

/// A request that arrived through `dialect`'s ingress.
fn request_from(dialect: IngressDialect) -> ChatRequest {
    let mut req = ChatRequest {
        model: ALIAS.into(),
        ..Default::default()
    };
    req.routectl_internal.ingress_wire = Some(IngressWire {
        dialect,
        client_version: None,
        bytes: br#"{"model":"a","messages":[]}"#.as_slice().into(),
    });
    req
}

/// A request that arrived through the Anthropic ingress.
fn anthropic_request() -> ChatRequest {
    request_from(IngressDialect::Anthropic)
}

/// Expected `(lane, reason, count)` local-refusal keys.
type Refusals = &'static [(&'static str, &'static str, u64)];

/// One refusal-counting case: its name, the error the provider fails with,
/// and the keys the dispatch must leave.
type RefusalCase = (&'static str, fn() -> Error, Refusals);

/// `(lane, reason, count)` for every local-refusal key the router holds.
fn refusals(router: &Router) -> Vec<(&'static str, &'static str, u64)> {
    router
        .wire_check_snapshot()
        .local_refusals
        .iter()
        .map(|c| (c.lane.label(), c.reason.as_str(), c.reading.count))
        .collect()
}

#[derive(Clone, Copy, Debug)]
enum Surface {
    Complete,
    Stream,
    CountTokens,
}

const SURFACES: [Surface; 2] = [Surface::Complete, Surface::Stream];

const ALL_SURFACES: [Surface; 3] = [Surface::Complete, Surface::Stream, Surface::CountTokens];

/// Dispatch `req` on `surface` and return the error the client receives.
async fn dispatch_err(router: &Router, surface: Surface, req: ChatRequest) -> Error {
    match surface {
        Surface::Complete => Box::pin(router.complete(req))
            .await
            .expect_err("the dispatch fails"),
        Surface::Stream => Box::pin(router.stream(req))
            .await
            .err()
            .expect("the stream fails to open"),
        Surface::CountTokens => Box::pin(router.count_tokens(req))
            .await
            .expect_err("the token count fails"),
    }
}

/// Dispatch `req` on `surface` and require it to succeed.
async fn dispatch_ok(router: &Router, surface: Surface, req: ChatRequest) {
    let outcome = match surface {
        Surface::Complete => Box::pin(router.complete(req)).await.map(drop),
        Surface::Stream => Box::pin(router.stream(req)).await.map(drop),
        Surface::CountTokens => Box::pin(router.count_tokens(req)).await.map(drop),
    };
    outcome.unwrap_or_else(|err| panic!("{surface:?} succeeds: {err:?}"));
}

/// A request that arrived through the Anthropic ingress as `surface` sees
/// it: a token count is parsed by that ingress but carries no captured
/// ingress body, so only its provenance names the dialect.
fn anthropic_request_on(surface: Surface) -> ChatRequest {
    match surface {
        Surface::Complete | Surface::Stream => anthropic_request(),
        Surface::CountTokens => {
            let mut req = ChatRequest {
                model: ALIAS.into(),
                ..Default::default()
            };
            req.routectl_internal.provenance = RequestProvenance::AnthropicIngress;
            req
        }
    }
}

fn egress_normalize_refusal() -> Error {
    Error::normalize_request(
        "failing",
        "request cannot be expressed on this egress",
        RefusalReason::Unrepresentable,
    )
}

fn egress_local_refusal() -> Error {
    Error::LocalRefusal {
        provider: "failing".into(),
        detail: "request shape refused before egress".into(),
        reason: RefusalReason::CloakRelocation,
    }
}

fn upstream_bad_request() -> Error {
    Error::upstream("failing", 400, "upstream rejected the request")
}

#[tokio::test]
async fn an_egress_refusal_counts_once_for_its_lane_and_an_upstream_400_does_not() {
    let cases: [RefusalCase; 3] = [
        (
            "normalize_request",
            egress_normalize_refusal,
            &[(ANTHROPIC_LANE, "unrepresentable", 1)],
        ),
        (
            "local_refusal",
            egress_local_refusal,
            &[(ANTHROPIC_LANE, "cloak-relocation", 1)],
        ),
        ("upstream_400", upstream_bad_request, &[]),
    ];

    for (name, error, expected) in cases {
        for surface in SURFACES {
            let router = failing_router(error);

            dispatch_err(&router, surface, anthropic_request()).await;

            assert_eq!(refusals(&router), expected, "{name} on {surface:?}");
        }
    }
}

#[tokio::test]
async fn two_egress_refusals_with_different_reasons_on_one_lane_are_two_keys() {
    for surface in SURFACES {
        let router = router_over(
            ProviderEntry::anthropic_api("literal:k"),
            Arc::new(SequencedProvider {
                errors: &[egress_normalize_refusal, egress_local_refusal],
                calls: AtomicUsize::new(0),
            }),
            false,
        );

        dispatch_err(&router, surface, anthropic_request()).await;
        dispatch_err(&router, surface, anthropic_request()).await;

        assert_eq!(
            refusals(&router),
            [
                (ANTHROPIC_LANE, "cloak-relocation", 1),
                (ANTHROPIC_LANE, "unrepresentable", 1),
            ],
            "{surface:?}"
        );
    }
}

/// An openai-compat egress with `strict_translation` on. Its base URL is
/// never dialed: the strict check refuses before any request is built.
fn strict_openai_compat() -> OpenAiCompatProvider {
    OpenAiCompatProvider::new(OpenAiCompatConfig {
        id: PROVIDER.into(),
        base_url: "https://openai-compat.invalid/v1".into(),
        api_key: "k".into(),
        header_extras: vec![],
        payload_extras: None,
        reasoning_dialect: Default::default(),
        history_reasoning: Default::default(),
        user_agent: None,
        strict_translation: true,
        disable_stream_include_usage: false,
        #[cfg(feature = "bedrock")]
        mantle: None,
    })
}

/// A chat-ingress request carrying a content block the openai-compat wire
/// has no slot for.
fn chat_request_with_an_unmodeled_block() -> ChatRequest {
    let part: ContentPart = serde_json::from_value(json!({"type": "future_widget_block"}))
        .expect("an unknown block type deserializes");
    let mut req = request_from(IngressDialect::Chat);
    req.messages = vec![Message {
        role: Role::User,
        content: MessageContent::Parts(vec![part]),
        tool_call_id: None,
        refusal: None,
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_calls: None,
    }]
    .into();
    req
}

#[tokio::test]
async fn a_provider_raised_strict_validation_counts_once_as_strict_translation() {
    for surface in SURFACES {
        let router = router_over(
            ProviderEntry::openai_compat("https://openai-compat.invalid/v1", "literal:k"),
            Arc::new(strict_openai_compat()),
            true,
        );

        let err = dispatch_err(&router, surface, chat_request_with_an_unmodeled_block()).await;

        assert!(matches!(err, Error::Validation(..)), "{surface:?}: {err:?}");
        assert_eq!(
            refusals(&router),
            [("chat->openai-compat", "strict-translation", 1)],
            "{surface:?}"
        );
    }
}

#[tokio::test]
async fn an_egress_refusal_without_an_ingress_body_has_no_lane_and_is_not_counted() {
    let router = failing_router(egress_local_refusal);
    let req = ChatRequest {
        model: ALIAS.into(),
        ..Default::default()
    };

    dispatch_err(&router, Surface::Complete, req).await;

    assert!(refusals(&router).is_empty());
}

/// An acting learned negative for the advisor tool on `provider`'s
/// `upstream` seat.
fn acting_advisor_negative(provider: &str, upstream: &str) -> ExportedEntry {
    let base = Instant::now();
    ExportedEntry {
        provider_kind: "anthropic-api".into(),
        state_key: format!("{provider}#{upstream}"),
        feature_key: "advisor".into(),
        verdict: EntryVerdict::Negative,
        signal: SignalTier::SelfIdentifying,
        observations: 1,
        first_seen: base,
        last_seen: base,
        expires_at: base + Duration::from_hours(48),
        evidence_class: None,
        phase: FailurePhase::F1,
        source: EvidenceSource::Live,
        in_flight: false,
        consecutive_failed_probes: 0,
    }
}

fn advisor_request() -> ChatRequest {
    ChatRequest {
        tools: Some(vec![ToolDef::Other(
            json!({"type": "advisor", "name": "advisor"}),
        )]),
        ..anthropic_request()
    }
}

#[tokio::test]
async fn a_strict_strip_refusal_counts_exactly_once() {
    for surface in SURFACES {
        let router = router_over(
            ProviderEntry::anthropic_api("literal:k"),
            Arc::new(AnsweringProvider::default()),
            true,
        );
        router
            .learned_capabilities
            .import_entries(vec![acting_advisor_negative(PROVIDER, UPSTREAM)]);

        let err = dispatch_err(&router, surface, advisor_request()).await;

        assert!(matches!(err, Error::Validation(..)), "{surface:?}: {err:?}");
        assert_eq!(router.metrics.strip_strict_rejected_total(), 1);
        assert_eq!(
            refusals(&router),
            [(ANTHROPIC_LANE, "strict-translation", 1)],
            "{surface:?}"
        );
    }
}

#[tokio::test]
async fn a_tool_pairing_refusal_carries_its_reason() {
    for surface in SURFACES {
        let router = router_over(
            ProviderEntry::anthropic_api("literal:k"),
            Arc::new(AnsweringProvider::default()),
            false,
        );
        let mut req = anthropic_request();
        req.messages = vec![Message {
            role: Role::Tool,
            content: MessageContent::Text("r".into()),
            tool_call_id: Some("call_missing".into()),
            refusal: None,
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_calls: None,
        }]
        .into();

        let err = dispatch_err(&router, surface, req).await;

        assert_eq!(
            err.refusal_reason(),
            Some(RefusalReason::ToolPairing),
            "{surface:?}: {err:?}"
        );
    }
}

const SECOND_NICKNAME: &str = "m2";
const SECOND_PROVIDER: &str = "p2";
const SECOND_UPSTREAM: &str = "upstream2";

/// A router whose alias chains the anthropic-api model served by `first`
/// (configured as `first_entry`) ahead of an own-credential anthropic-api
/// model served by `second`, with no retries.
fn chain_router(
    first_entry: ProviderEntry,
    first: Arc<dyn Provider>,
    second: Arc<dyn Provider>,
) -> Router {
    let mut config = Config::default();
    config.retry.max_attempts = 1;
    config.providers.insert(PROVIDER.into(), first_entry);
    config.providers.insert(
        SECOND_PROVIDER.into(),
        ProviderEntry::anthropic_api("literal:k"),
    );
    config.aliases.insert(
        ALIAS.into(),
        AliasValue::Chain(vec![NICKNAME.into(), SECOND_NICKNAME.into()]),
    );
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(BTreeMap::from([
        (
            NICKNAME.to_string(),
            Arc::new(ResolvedModel::new(NICKNAME, PROVIDER, first, UPSTREAM)),
        ),
        (
            SECOND_NICKNAME.to_string(),
            Arc::new(ResolvedModel::new(
                SECOND_NICKNAME,
                SECOND_PROVIDER,
                second,
                SECOND_UPSTREAM,
            )),
        ),
    ]));
    router
}

fn forwarded_anthropic_api() -> ProviderEntry {
    ProviderEntry::anthropic_api("literal:k").with_credential_source(CredentialSource::Forwarded)
}

/// An advisor request whose `tool_choice` forces the advisor, so stripping
/// it on a target with an acting negative is a hazard the strip rolls back.
fn forcing_advisor_request_on(surface: Surface) -> ChatRequest {
    ChatRequest {
        tools: Some(vec![ToolDef::Other(
            json!({"type": "advisor", "name": "advisor"}),
        )]),
        tool_choice: Some(json!({"type": "tool", "name": "advisor"})),
        ..anthropic_request_on(surface)
    }
}

/// One row of a per-surface outcome table: the surface, the local-refusal
/// keys its dispatch left, and how many calls reached the second target.
type SurfaceRow = (&'static str, Vec<(&'static str, &'static str, u64)>, usize);

const fn surface_label(surface: Surface) -> &'static str {
    match surface {
        Surface::Complete => "complete",
        Surface::Stream => "stream",
        Surface::CountTokens => "count_tokens",
    }
}

#[tokio::test]
async fn a_missing_forwarded_bearer_counts_once_and_never_reaches_the_upstream() {
    let mut observed = Vec::new();
    for surface in ALL_SURFACES {
        let upstream = Arc::new(AnsweringProvider::default());
        let router = router_over(forwarded_anthropic_api(), upstream.clone(), false);

        let err = dispatch_err(&router, surface, anthropic_request_on(surface)).await;

        assert_eq!(
            err.refusal_reason(),
            Some(RefusalReason::ForwardedBearerMissing),
            "{surface:?}: {err:?}"
        );
        observed.push((surface_label(surface), refusals(&router), upstream.calls()));
    }

    let expected: Vec<SurfaceRow> = ALL_SURFACES
        .iter()
        .map(|&s| {
            let keys = vec![(ANTHROPIC_LANE, "forwarded-bearer-missing", 1)];
            (surface_label(s), keys, 0)
        })
        .collect();
    assert_eq!(observed, expected);
}

#[tokio::test]
async fn a_terminal_refusal_on_the_first_target_counts_once_and_stops_the_chain() {
    let mut observed = Vec::new();
    for surface in ALL_SURFACES {
        let second = Arc::new(AnsweringProvider::default());
        let router = chain_router(
            forwarded_anthropic_api(),
            Arc::new(AnsweringProvider::default()),
            second.clone(),
        );

        dispatch_err(&router, surface, anthropic_request_on(surface)).await;

        observed.push((surface_label(surface), refusals(&router), second.calls()));
    }

    let expected: Vec<SurfaceRow> = ALL_SURFACES
        .iter()
        .map(|&s| {
            let keys = vec![(ANTHROPIC_LANE, "forwarded-bearer-missing", 1)];
            (surface_label(s), keys, 0)
        })
        .collect();
    assert_eq!(observed, expected);
}

#[tokio::test]
async fn a_rolled_back_strip_on_the_first_target_counts_once_when_the_second_succeeds() {
    let mut observed = Vec::new();
    for surface in ALL_SURFACES {
        let second = Arc::new(AnsweringProvider::default());
        let router = chain_router(
            ProviderEntry::anthropic_api("literal:k"),
            Arc::new(AnsweringProvider::default()),
            second.clone(),
        );
        router
            .learned_capabilities
            .import_entries(vec![acting_advisor_negative(PROVIDER, UPSTREAM)]);

        dispatch_ok(&router, surface, forcing_advisor_request_on(surface)).await;

        observed.push((surface_label(surface), refusals(&router), second.calls()));
    }

    let expected: Vec<SurfaceRow> = ALL_SURFACES
        .iter()
        .map(|&s| {
            let keys = vec![(ANTHROPIC_LANE, "unrepresentable", 1)];
            (surface_label(s), keys, 1)
        })
        .collect();
    assert_eq!(observed, expected);
}

#[tokio::test]
async fn a_rolled_back_strip_that_exhausts_the_chain_counts_once_per_target() {
    let mut observed = Vec::new();
    for surface in ALL_SURFACES {
        let second = Arc::new(AnsweringProvider::default());
        let router = chain_router(
            ProviderEntry::anthropic_api("literal:k"),
            Arc::new(AnsweringProvider::default()),
            second.clone(),
        );
        router.learned_capabilities.import_entries(vec![
            acting_advisor_negative(PROVIDER, UPSTREAM),
            acting_advisor_negative(SECOND_PROVIDER, SECOND_UPSTREAM),
        ]);

        dispatch_err(&router, surface, forcing_advisor_request_on(surface)).await;

        observed.push((surface_label(surface), refusals(&router), second.calls()));
    }

    let expected: Vec<SurfaceRow> = ALL_SURFACES
        .iter()
        .map(|&s| {
            let keys = vec![(ANTHROPIC_LANE, "unrepresentable", 2)];
            (surface_label(s), keys, 0)
        })
        .collect();
    assert_eq!(observed, expected);
}

#[tokio::test]
async fn a_provider_raised_refusal_on_the_first_target_counts_once_when_the_second_succeeds() {
    let mut observed = Vec::new();
    for surface in SURFACES {
        let second = Arc::new(AnsweringProvider::default());
        let router = chain_router(
            ProviderEntry::anthropic_api("literal:k"),
            Arc::new(FailingProvider {
                error: egress_local_refusal,
            }),
            second.clone(),
        );

        dispatch_ok(&router, surface, anthropic_request()).await;

        observed.push((surface_label(surface), refusals(&router), second.calls()));
    }

    let expected: Vec<SurfaceRow> = SURFACES
        .iter()
        .map(|&s| {
            let keys = vec![(ANTHROPIC_LANE, "cloak-relocation", 1)];
            (surface_label(s), keys, 1)
        })
        .collect();
    assert_eq!(observed, expected);
}
