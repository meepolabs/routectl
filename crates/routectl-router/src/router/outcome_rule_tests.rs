//! The shared failure-outcome rule: the pure park/debit table, the
//! same-provider retry table per surface, and one completion walk pinning
//! that a debit which opens the breaker still retries the same provider.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use routectl_core::failure_class::{FailureClass, classify};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Provider, RefusalReason, Result, TokenCount,
};
use routectl_testkit::with_capture;

use super::*;
use crate::class_policy::ConfigFailureClass;
use crate::config::{AliasValue, Config, ProviderEntry, ProviderRuntimePolicy, RetryPolicy};
use crate::resolved::ResolvedModel;
use crate::router::Router;

const CAP: Duration = INLOOP_RETRY_AFTER_CAP;
const JUST_OVER_CAP: Duration = CAP.saturating_add(Duration::from_millis(1));
const LARGE: Duration = Duration::from_hours(1);

struct BreakerRow {
    name: &'static str,
    status: u16,
    class: FailureClass,
    hint: Option<Duration>,
    is_probe: bool,
    expected: BreakerOutcome,
}

const fn row(
    name: &'static str,
    status: u16,
    class: FailureClass,
    hint: Option<Duration>,
    is_probe: bool,
    expected: BreakerOutcome,
) -> BreakerRow {
    BreakerRow {
        name,
        status,
        class,
        hint,
        is_probe,
        expected,
    }
}

fn breaker_rows() -> Vec<BreakerRow> {
    use BreakerOutcome::{Debit, None as Untouched, Park};
    use FailureClass::{BadRequest, Overloaded, RateLimited, ServerError};
    vec![
        row(
            "debiting / no hint / non-probe",
            503,
            ServerError,
            None,
            false,
            Debit,
        ),
        row(
            "debiting / no hint / probe",
            503,
            ServerError,
            None,
            true,
            Debit,
        ),
        row(
            "debiting / hint == cap / non-probe",
            503,
            ServerError,
            Some(CAP),
            false,
            Debit,
        ),
        row(
            "debiting / hint == cap / probe",
            503,
            ServerError,
            Some(CAP),
            true,
            Debit,
        ),
        row(
            "debiting / cap + 1ms / non-probe",
            503,
            ServerError,
            Some(JUST_OVER_CAP),
            false,
            Park(JUST_OVER_CAP),
        ),
        row(
            "debiting / cap + 1ms / probe",
            503,
            ServerError,
            Some(JUST_OVER_CAP),
            true,
            Debit,
        ),
        row(
            "debiting / large hint / non-probe",
            503,
            ServerError,
            Some(LARGE),
            false,
            Park(LARGE),
        ),
        row(
            "debiting / large hint / probe",
            503,
            ServerError,
            Some(LARGE),
            true,
            Debit,
        ),
        row(
            "non-debiting / no hint / non-probe",
            400,
            BadRequest,
            None,
            false,
            Untouched,
        ),
        row(
            "non-debiting / no hint / probe",
            400,
            BadRequest,
            None,
            true,
            Untouched,
        ),
        row(
            "non-debiting / hint == cap / non-probe",
            400,
            BadRequest,
            Some(CAP),
            false,
            Untouched,
        ),
        row(
            "non-debiting / hint == cap / probe",
            400,
            BadRequest,
            Some(CAP),
            true,
            Untouched,
        ),
        row(
            "non-debiting / cap + 1ms / non-probe",
            400,
            BadRequest,
            Some(JUST_OVER_CAP),
            false,
            Untouched,
        ),
        row(
            "non-debiting / cap + 1ms / probe",
            400,
            BadRequest,
            Some(JUST_OVER_CAP),
            true,
            Untouched,
        ),
        row(
            "non-debiting / large hint / non-probe",
            400,
            BadRequest,
            Some(LARGE),
            false,
            Untouched,
        ),
        row(
            "non-debiting / large hint / probe",
            400,
            BadRequest,
            Some(LARGE),
            true,
            Untouched,
        ),
        row(
            "429 / no hint / non-probe",
            429,
            RateLimited,
            None,
            false,
            Debit,
        ),
        row(
            "429 / large hint / non-probe",
            429,
            RateLimited,
            Some(LARGE),
            false,
            Park(LARGE),
        ),
        row(
            "429 / no hint / probe",
            429,
            RateLimited,
            None,
            true,
            Untouched,
        ),
        row(
            "429 / large hint / probe",
            429,
            RateLimited,
            Some(LARGE),
            true,
            Untouched,
        ),
        row(
            "529 / no hint / non-probe",
            529,
            Overloaded,
            None,
            false,
            Debit,
        ),
        row(
            "529 / no hint / probe",
            529,
            Overloaded,
            None,
            true,
            Untouched,
        ),
        row(
            "529 / large hint / probe",
            529,
            Overloaded,
            Some(LARGE),
            true,
            Untouched,
        ),
        row(
            "429 remapped to a debiting class / probe",
            429,
            ServerError,
            None,
            true,
            Untouched,
        ),
        row(
            "503 overload class / probe",
            503,
            Overloaded,
            None,
            true,
            Debit,
        ),
    ]
}

#[test]
fn breaker_outcome_table() {
    for row in breaker_rows() {
        let err = Error::upstream("p", row.status, "body");
        assert_eq!(
            breaker_outcome(&err, &row.class, row.hint, row.is_probe),
            row.expected,
            "row: {}",
            row.name
        );
    }
}

struct RetryRow {
    name: &'static str,
    surface: DispatchSurface,
    err: Error,
    attempts_made: u32,
    is_probe: bool,
    transcript_rejection: bool,
    outcome: BreakerOutcome,
    hint: Option<Duration>,
    expected: SameProvider,
}

fn rate_limited(retry_after: Option<Duration>) -> Error {
    Error::upstream_with_retry_after("p", 429, "rate limited", retry_after)
}

fn retry_rows() -> Vec<RetryRow> {
    let base = |name: &'static str, surface: DispatchSurface| RetryRow {
        name,
        surface,
        err: Error::upstream("p", 500, "server error"),
        attempts_made: 0,
        is_probe: false,
        transcript_rejection: false,
        outcome: BreakerOutcome::Debit,
        hint: None,
        expected: SameProvider::Retry { sleep_floor: None },
    };
    vec![
        RetryRow {
            expected: SameProvider::No,
            ..base("stream never retries", DispatchSurface::Stream)
        },
        RetryRow {
            expected: SameProvider::No,
            ..base("count_tokens never retries", DispatchSurface::CountTokens)
        },
        base("complete retryable 500 retries", DispatchSurface::Complete),
        RetryRow {
            attempts_made: 2,
            expected: SameProvider::No,
            ..base("complete at the hard cap", DispatchSurface::Complete)
        },
        RetryRow {
            outcome: BreakerOutcome::Park(LARGE),
            err: rate_limited(Some(LARGE)),
            hint: Some(LARGE),
            expected: SameProvider::No,
            ..base("complete parked", DispatchSurface::Complete)
        },
        RetryRow {
            err: Error::upstream("p", 429, "rate limited"),
            is_probe: true,
            expected: SameProvider::No,
            ..base("complete probe 429", DispatchSurface::Complete)
        },
        RetryRow {
            err: Error::upstream("p", 529, "overloaded"),
            is_probe: true,
            expected: SameProvider::No,
            ..base("complete probe 529", DispatchSurface::Complete)
        },
        RetryRow {
            transcript_rejection: true,
            expected: SameProvider::No,
            ..base("complete transcript rejection", DispatchSurface::Complete)
        },
        RetryRow {
            err: Error::Validation("refused".into(), RefusalReason::ForwardedBearerMissing),
            outcome: BreakerOutcome::None,
            expected: SameProvider::No,
            ..base("complete refusal reason", DispatchSurface::Complete)
        },
        RetryRow {
            err: rate_limited(Some(CAP)),
            hint: Some(CAP),
            expected: SameProvider::Retry {
                sleep_floor: Some(CAP),
            },
            ..base("complete non-probe hint == cap", DispatchSurface::Complete)
        },
        RetryRow {
            err: Error::upstream_with_retry_after(
                "p",
                503,
                "unavailable",
                Some(Duration::from_secs(1)),
            ),
            hint: Some(Duration::from_secs(1)),
            is_probe: true,
            expected: SameProvider::Retry { sleep_floor: None },
            ..base("complete probe 503 small hint", DispatchSurface::Complete)
        },
        RetryRow {
            err: rate_limited(Some(JUST_OVER_CAP)),
            hint: Some(JUST_OVER_CAP),
            outcome: BreakerOutcome::Debit,
            expected: SameProvider::Retry { sleep_floor: None },
            ..base(
                "complete hint over cap without a park",
                DispatchSurface::Complete,
            )
        },
        RetryRow {
            hint: Some(Duration::from_secs(2)),
            err: Error::upstream_with_retry_after(
                "p",
                503,
                "unavailable",
                Some(Duration::from_secs(2)),
            ),
            outcome: BreakerOutcome::Debit,
            expected: SameProvider::Retry {
                sleep_floor: Some(Duration::from_secs(2)),
            },
            ..base("complete small 503 hint", DispatchSurface::Complete)
        },
        RetryRow {
            hint: Some(LARGE),
            err: rate_limited(Some(LARGE)),
            expected: SameProvider::No,
            ..base("stream large hint", DispatchSurface::Stream)
        },
    ]
}

#[test]
fn same_provider_retry_table() {
    let policy = RetryPolicy {
        max_attempts: 2,
        ..RetryPolicy::default()
    };
    assert_eq!(policy.hard_retry_cap(), 2, "the hard-cap row assumes 2");
    for row in retry_rows() {
        let class = classify(&row.err, None).class;
        let got = same_provider_retry(
            row.surface,
            &row.err,
            &class,
            &policy,
            row.attempts_made,
            row.is_probe,
            row.transcript_rejection,
            row.outcome,
            row.hint,
        );
        assert_eq!(got, row.expected, "row: {}", row.name);
    }
}

/// Sole chain-entry provider failing every completion with a plain 500:
/// a debiting class with no reset hint, so the attempt takes the
/// threshold-gated debit.
struct ServerErrorProvider;

#[async_trait]
impl Provider for ServerErrorProvider {
    fn id(&self) -> &'static str {
        "p"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(Error::upstream("p", 500, "server error"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream("p", 500, "server error"))
    }
}

fn router_tripping_on_first_failure() -> Router {
    let mut config = Config::default();
    config
        .aliases
        .insert("solo".into(), AliasValue::Chain(vec!["seat".into()]));
    config.providers.insert(
        "p".into(),
        ProviderEntry::OpenaiCompat {
            base_url: "https://placeholder.invalid/v1".into(),
            api_key_ref: "literal:k".into(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            auto_emit_per_block_breakpoints: None,
            reduction_enabled: None,
            #[cfg(feature = "bedrock")]
            bedrock_mantle: None,
            runtime: ProviderRuntimePolicy {
                circuit_failures: Some(1),
                circuit_cooldown_ms: Some(60_000),
                ..Default::default()
            },
        },
    );
    let mut router = Router::new(Arc::new(config));
    let provider: Arc<dyn Provider> = Arc::new(ServerErrorProvider);
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "seat".into(),
        Arc::new(ResolvedModel::new("seat", "p", provider, "u")),
    );
    router.install_resolved_models(models);
    router
}

#[tokio::test]
async fn an_opened_debit_does_not_suppress_the_complete_retry() {
    let router = router_tripping_on_first_failure();
    let req = ChatRequest {
        model: "solo".into(),
        messages: vec![].into(),
        ..Default::default()
    };

    let (_result, events) = with_capture(Box::pin(router.complete(req))).await;

    let first = events
        .iter()
        .find(|e| e.message == "retry decision")
        .expect("the completion error arm emits a retry decision");
    assert_eq!(first.field("attempt"), Some("1"));
    assert_eq!(
        first.field("breaker_effect"),
        Some("opened"),
        "a one-failure threshold must open the breaker on the first 500"
    );
    assert_eq!(
        first.field("same_provider_retry"),
        Some("true"),
        "an opened debit is log-only: the same provider is still retried"
    );
}

/// Sole chain-entry provider failing every surface with `status` carrying
/// `reset` as its upstream reset hint.
struct ResetHintProvider {
    status: u16,
    reset: Option<Duration>,
}

impl ResetHintProvider {
    fn error(&self) -> Error {
        Error::upstream_with_retry_after("p", self.status, "unavailable", self.reset)
    }
}

#[async_trait]
impl Provider for ResetHintProvider {
    fn id(&self) -> &'static str {
        "p"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(self.error())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(self.error())
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        Err(self.error())
    }
}

/// A single count-capable seat behind alias `solo` failing with `status`,
/// with a breaker that a lone debit cannot trip (threshold 5, 1s cooldown)
/// and one attempt per request, so every surface settles exactly one
/// failure.
fn router_failing_with(status: u16, reset: Option<Duration>) -> Router {
    router_failing_with_overrides(status, reset, BTreeMap::new())
}

/// [`router_failing_with`] whose provider remaps statuses by `class_overrides`.
fn router_failing_with_overrides(
    status: u16,
    reset: Option<Duration>,
    class_overrides: BTreeMap<u16, ConfigFailureClass>,
) -> Router {
    let mut config = Config {
        retry: RetryPolicy {
            max_attempts: 1,
            initial_backoff_ms: 0,
            jitter_ms: 0,
            ..RetryPolicy::default()
        },
        ..Config::default()
    };
    config
        .aliases
        .insert("solo".into(), AliasValue::Chain(vec!["seat".into()]));
    config.providers.insert(
        "p".into(),
        ProviderEntry::anthropic_api("literal:k").with_runtime(ProviderRuntimePolicy {
            circuit_failures: Some(5),
            circuit_cooldown_ms: Some(1_000),
            class_overrides,
            ..Default::default()
        }),
    );
    let mut router = Router::new(Arc::new(config));
    let provider: Arc<dyn Provider> = Arc::new(ResetHintProvider { status, reset });
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "seat".into(),
        Arc::new(ResolvedModel::new("seat", "p", provider, "u")),
    );
    router.install_resolved_models(models);
    router
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settled {
    Parked,
    Debited,
    Untouched,
}

/// What the one failed attempt did to the seat's breaker: a park opens
/// it without stamping an outcome, a sub-threshold debit leaves it
/// closed and stamps the failure, and no outcome leaves it closed and
/// unstamped.
fn settled(router: &Router) -> Settled {
    use crate::runtime_state::CircuitPhase::{Closed, Open};
    let gate = router.gate_status_for_tests("seat");
    match (gate.circuit, gate.last_outcome) {
        (Open, None) => Settled::Parked,
        (Closed, Some(_)) => Settled::Debited,
        (Closed, None) => Settled::Untouched,
        other => panic!("neither a park, a debit, nor untouched: {other:?}"),
    }
}

async fn settle_on(
    surface: DispatchSurface,
    status: u16,
    reset: Option<Duration>,
    req: ChatRequest,
) -> Settled {
    let router = router_failing_with(status, reset);
    let failed = match surface {
        DispatchSurface::Complete => router.complete(req).await.is_err(),
        DispatchSurface::Stream => router.stream(req).await.is_err(),
        DispatchSurface::CountTokens => router.count_tokens(req).await.is_err(),
    };
    assert!(failed, "{}: the attempt must fail", surface.as_str());
    settled(&router)
}

#[tokio::test]
async fn same_reset_hint_same_pre_content_breaker_outcome_on_every_surface() {
    let plain = ChatRequest {
        model: "solo".into(),
        ..Default::default()
    };
    let probe = ChatRequest {
        max_tokens: Some(1),
        ..plain.clone()
    };
    let rows: [(&str, u16, Option<Duration>, &ChatRequest, Settled); 8] = [
        ("no hint", 503, None, &plain, Settled::Debited),
        (
            "1ms hint",
            503,
            Some(Duration::from_millis(1)),
            &plain,
            Settled::Debited,
        ),
        (
            "60s hint",
            503,
            Some(Duration::from_mins(1)),
            &plain,
            Settled::Parked,
        ),
        (
            "60s hint on a probe",
            503,
            Some(Duration::from_mins(1)),
            &probe,
            Settled::Debited,
        ),
        ("429", 429, None, &plain, Settled::Debited),
        ("429 on a probe", 429, None, &probe, Settled::Untouched),
        (
            "429 with a 60s hint on a probe",
            429,
            Some(Duration::from_mins(1)),
            &probe,
            Settled::Untouched,
        ),
        ("529 on a probe", 529, None, &probe, Settled::Untouched),
    ];
    let mut mismatches = Vec::new();
    for (row, status, reset, req, expected) in rows {
        for surface in [
            DispatchSurface::Complete,
            DispatchSurface::Stream,
            DispatchSurface::CountTokens,
        ] {
            let got = settle_on(surface, status, reset, req.clone()).await;
            if got != expected {
                mismatches.push(format!(
                    "{} / {row}: expected {expected:?}, got {got:?}",
                    surface.as_str()
                ));
            }
        }
    }
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}

/// Sole chain-entry provider whose stream emits one content chunk, then a
/// frame error with `status`: the failure lands after the stream committed.
struct ContentThenErrorProvider {
    status: u16,
}

#[async_trait]
impl Provider for ContentThenErrorProvider {
    fn id(&self) -> &'static str {
        "p"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(Error::normalize_response("p", "unused"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let content = ChatChunk {
            choices: vec![routectl_core::ChunkChoice {
                index: 0,
                delta: routectl_core::ChunkDelta {
                    content: Some("ok".into()),
                    ..Default::default()
                },
                finish_reason: None,
                matched_stop_sequence: None,
            }],
            ..Default::default()
        };
        let frames = vec![
            Ok(content),
            Err(Error::upstream("p", self.status, "unavailable")),
        ];
        Ok(Box::pin(futures::stream::iter(frames)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PostContent {
    Debited,
    Succeeded,
    Untouched,
}

/// Drain a stream whose frame error follows first content, drop it, and
/// read what the wrapper settled. The breaker cannot trip on one failure
/// and was closed at admission, so first content recorded nothing: a
/// debit stamps a failure outcome, an unsettled wrapper's drop stamps
/// success, and a settled no-op leaves no outcome at all.
async fn settle_after_content(
    status: u16,
    req: ChatRequest,
    class_overrides: BTreeMap<u16, ConfigFailureClass>,
) -> PostContent {
    use futures::StreamExt as _;
    let mut router = router_failing_with_overrides(status, None, class_overrides);
    let provider: Arc<dyn Provider> = Arc::new(ContentThenErrorProvider { status });
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "seat".into(),
        Arc::new(ResolvedModel::new("seat", "p", provider, "u")),
    );
    router.install_resolved_models(models);

    let frames: Vec<Result<ChatChunk>> = router
        .stream(req)
        .await
        .expect("first content commits the stream")
        .collect()
        .await;
    assert!(frames.first().is_some_and(Result::is_ok), "content first");
    assert!(frames.last().is_some_and(Result::is_err), "error last");

    use crate::runtime_state::CircuitPhase::Closed;
    let gate = router.gate_status_for_tests("seat");
    match (gate.circuit, gate.last_outcome) {
        (Closed, None) => PostContent::Untouched,
        (Closed, Some(routectl_core::failure_class::LastOutcome::Ok)) => PostContent::Succeeded,
        (Closed, Some(_)) => PostContent::Debited,
        other => panic!("unexpected breaker state after one frame error: {other:?}"),
    }
}

#[tokio::test]
async fn post_content_frame_error_settles_by_the_shared_rule() {
    let plain = ChatRequest {
        model: "solo".into(),
        ..Default::default()
    };
    let probe = ChatRequest {
        max_tokens: Some(1),
        ..plain.clone()
    };
    let none = BTreeMap::new();
    let remap_503 = BTreeMap::from([(503, ConfigFailureClass::BadRequest)]);
    type Row<'a> = (
        &'a str,
        u16,
        &'a ChatRequest,
        &'a BTreeMap<u16, ConfigFailureClass>,
        PostContent,
    );
    let rows: [Row; 9] = [
        ("probe 429", 429, &probe, &none, PostContent::Untouched),
        ("probe 529", 529, &probe, &none, PostContent::Untouched),
        ("probe 503", 503, &probe, &none, PostContent::Debited),
        ("probe 400", 400, &probe, &none, PostContent::Untouched),
        (
            "probe 503 remapped to bad-request",
            503,
            &probe,
            &remap_503,
            PostContent::Untouched,
        ),
        ("non-probe 429", 429, &plain, &none, PostContent::Debited),
        ("non-probe 400", 400, &plain, &none, PostContent::Untouched),
        ("non-probe 503", 503, &plain, &none, PostContent::Debited),
        (
            "non-probe 503 remapped to bad-request",
            503,
            &plain,
            &remap_503,
            PostContent::Untouched,
        ),
    ];
    let mut mismatches = Vec::new();
    for (row, status, req, overrides, expected) in rows {
        let got = settle_after_content(status, req.clone(), overrides.clone()).await;
        if got != expected {
            mismatches.push(format!("{row}: expected {expected:?}, got {got:?}"));
        }
    }
    assert!(mismatches.is_empty(), "{mismatches:#?}");
}
