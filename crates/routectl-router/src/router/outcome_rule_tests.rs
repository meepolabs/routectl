//! The shared failure-outcome rule: the pure park/debit table, the
//! same-provider retry table per surface, and one completion walk pinning
//! that a debit which opens the breaker still retries the same provider.
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use routectl_core::failure_class::{FailureClass, classify};
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, RefusalReason, Result};
use routectl_testkit::with_capture;

use super::*;
use crate::config::{AliasValue, Config, ProviderEntry, ProviderRuntimePolicy, RetryPolicy};
use crate::resolved::ResolvedModel;
use crate::router::Router;

const CAP: Duration = INLOOP_RETRY_AFTER_CAP;
const JUST_OVER_CAP: Duration = CAP.saturating_add(Duration::from_millis(1));
const LARGE: Duration = Duration::from_hours(1);

type BreakerRow = (
    &'static str,
    FailureClass,
    Option<Duration>,
    bool,
    BreakerOutcome,
);

#[test]
fn breaker_outcome_table() {
    let rows: [BreakerRow; 16] = [
        (
            "debiting / no hint / non-probe",
            FailureClass::RateLimited,
            None,
            false,
            BreakerOutcome::Debit,
        ),
        (
            "debiting / no hint / probe",
            FailureClass::RateLimited,
            None,
            true,
            BreakerOutcome::Debit,
        ),
        (
            "debiting / hint == cap / non-probe",
            FailureClass::RateLimited,
            Some(CAP),
            false,
            BreakerOutcome::Debit,
        ),
        (
            "debiting / hint == cap / probe",
            FailureClass::RateLimited,
            Some(CAP),
            true,
            BreakerOutcome::Debit,
        ),
        (
            "debiting / cap + 1ms / non-probe",
            FailureClass::RateLimited,
            Some(JUST_OVER_CAP),
            false,
            BreakerOutcome::Park(JUST_OVER_CAP),
        ),
        (
            "debiting / cap + 1ms / probe",
            FailureClass::RateLimited,
            Some(JUST_OVER_CAP),
            true,
            BreakerOutcome::Debit,
        ),
        (
            "debiting / large hint / non-probe",
            FailureClass::RateLimited,
            Some(LARGE),
            false,
            BreakerOutcome::Park(LARGE),
        ),
        (
            "debiting / large hint / probe",
            FailureClass::RateLimited,
            Some(LARGE),
            true,
            BreakerOutcome::Debit,
        ),
        (
            "non-debiting / no hint / non-probe",
            FailureClass::BadRequest,
            None,
            false,
            BreakerOutcome::None,
        ),
        (
            "non-debiting / no hint / probe",
            FailureClass::BadRequest,
            None,
            true,
            BreakerOutcome::None,
        ),
        (
            "non-debiting / hint == cap / non-probe",
            FailureClass::BadRequest,
            Some(CAP),
            false,
            BreakerOutcome::None,
        ),
        (
            "non-debiting / hint == cap / probe",
            FailureClass::BadRequest,
            Some(CAP),
            true,
            BreakerOutcome::None,
        ),
        (
            "non-debiting / cap + 1ms / non-probe",
            FailureClass::BadRequest,
            Some(JUST_OVER_CAP),
            false,
            BreakerOutcome::None,
        ),
        (
            "non-debiting / cap + 1ms / probe",
            FailureClass::BadRequest,
            Some(JUST_OVER_CAP),
            true,
            BreakerOutcome::None,
        ),
        (
            "non-debiting / large hint / non-probe",
            FailureClass::BadRequest,
            Some(LARGE),
            false,
            BreakerOutcome::None,
        ),
        (
            "non-debiting / large hint / probe",
            FailureClass::BadRequest,
            Some(LARGE),
            true,
            BreakerOutcome::None,
        ),
    ];
    for (name, class, hint, is_probe, expected) in rows {
        assert_eq!(
            breaker_outcome(&class, hint, is_probe),
            expected,
            "row: {name}"
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
    let policy = RetryPolicy::default();
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
