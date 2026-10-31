//! The per-attempt deadline on a token count: an attempt that outlives
//! `request_timeout_ms` fails with a completion's timeout error, debits the
//! seat, and the count moves to the next capable seat, while every other
//! status-0 health error still ends the count.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::stream::BoxStream;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result, TokenCount, UpstreamReason,
};
use serde_json::{Value, json};

use super::{Router, RouterOptions};
use crate::config::{AliasValue, Config, ProviderEntry, ProviderRuntimePolicy};
use crate::resolved::ResolvedModel;
use crate::runtime_state::CircuitPhase;

const ALIAS: &str = "a";
const FIRST: &str = "m1";
const SECOND: &str = "m2";
const TIMEOUT_MS: u64 = 200;
/// Long enough for the deadline to fire many times over, so a walk that
/// never enforces it fails here rather than hanging the test.
const OUTER_BOUND: Duration = Duration::from_secs(5);

/// How a seat answers each upstream call.
#[derive(Clone, Copy)]
enum Behaviour {
    /// Reaches the upstream and never answers.
    Hangs,
    /// Never answers the first call, answers every later one.
    HangsOnce,
    /// A status-0 transport failure that is not the walk's deadline.
    RefusesConnection,
    /// A wire 501: the upstream cannot count.
    CannotCount,
    Answers,
}

struct SeatProvider {
    behaviour: Behaviour,
    calls: AtomicUsize,
}

impl SeatProvider {
    fn new(behaviour: Behaviour) -> Arc<Self> {
        Arc::new(Self {
            behaviour,
            calls: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    async fn answer<T>(&self, ok: T) -> Result<T> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        match self.behaviour {
            Behaviour::Hangs => std::future::pending().await,
            Behaviour::HangsOnce if call == 0 => std::future::pending().await,
            Behaviour::RefusesConnection => {
                Err(Error::upstream("scripted", 0, "connection refused"))
            }
            Behaviour::CannotCount => Err(Error::upstream("scripted", 501, "cannot count")),
            Behaviour::HangsOnce | Behaviour::Answers => Ok(ok),
        }
    }
}

#[async_trait::async_trait]
impl Provider for SeatProvider {
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
        self.answer(ChatResponse::default()).await
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::normalize_response("scripted", "unused"))
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        self.answer(TokenCount {
            input_tokens: 1,
            ..Default::default()
        })
        .await
    }
}

fn provider_of(nickname: &str) -> String {
    format!("p-{nickname}")
}

/// An anthropic-api entry (count-capable) whose breaker opens on
/// `circuit_failures` debits and then stays open for a minute; `Some(0)`
/// turns it off, `None` leaves it on the shipped default.
fn anthropic_entry(circuit_failures: Option<u32>) -> ProviderEntry {
    ProviderEntry::anthropic_api("literal:k").with_runtime(ProviderRuntimePolicy {
        circuit_failures,
        circuit_cooldown_ms: circuit_failures.map(|_| 60_000),
        ..ProviderRuntimePolicy::default()
    })
}

/// A router over `seats` with `request_timeout_ms` set and one attempt per
/// seat. `chain` names the seats the alias walks, in order.
fn router_over(
    seats: &[(&'static str, Arc<SeatProvider>)],
    chain: &[&str],
    circuit_failures: Option<u32>,
) -> Router {
    let mut config = Config::default();
    config.retry.max_attempts = 1;
    config.retry.request_timeout_ms = Some(TIMEOUT_MS);
    let mut models = BTreeMap::new();
    for (nickname, provider) in seats {
        let provider_name = provider_of(nickname);
        config
            .providers
            .insert(provider_name.clone(), anthropic_entry(circuit_failures));
        let provider: Arc<dyn Provider> = provider.clone();
        let model =
            ResolvedModel::new(*nickname, provider_name, provider, format!("up-{nickname}"));
        models.insert((*nickname).to_string(), Arc::new(model));
    }
    config.aliases.insert(
        ALIAS.into(),
        AliasValue::Chain(chain.iter().map(|s| (*s).to_string()).collect()),
    );
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    router
}

fn request() -> ChatRequest {
    ChatRequest {
        model: ALIAS.into(),
        ..Default::default()
    }
}

fn phase(router: &Router, nickname: &str) -> CircuitPhase {
    router
        .capacity_snapshot_for(nickname, None, Instant::now())
        .expect("the seat has a state slot")
        .circuit
}

fn park(router: &Router, nickname: &str) {
    assert!(router.park_credential_for_tests(
        nickname,
        UpstreamReason::CredentialInvalid,
        Instant::now() + Duration::from_hours(1),
    ));
}

async fn count(router: &Router) -> Result<TokenCount> {
    tokio::time::timeout(OUTER_BOUND, Box::pin(router.count_tokens(request())))
        .await
        .expect("the attempt deadline ends a hanging count")
}

/// How a count ended: the variant plus the fields a row tells apart.
fn shape(outcome: &Result<TokenCount>) -> String {
    match outcome {
        Ok(_) => "served".into(),
        Err(Error::Upstream {
            provider,
            status,
            body,
            ..
        }) => format!("{provider} upstream {status}: {body}"),
        Err(Error::NotImplemented(_, detail)) => format!("not implemented: {detail}"),
        Err(Error::Credential { .. }) => "credential".into(),
        Err(other) => format!("other: {other:?}"),
    }
}

fn timed_out_on(nickname: &str) -> String {
    format!(
        "{} upstream 0: request timed out after {TIMEOUT_MS}ms",
        provider_of(nickname)
    )
}

/// `(row, how the count ended, calls the second seat took, first seat's breaker)`.
type StatusZeroRow = (&'static str, String, usize, CircuitPhase);

#[tokio::test(start_paused = true)]
async fn only_an_expired_count_deadline_walks_and_every_status_zero_failure_debits() {
    let mut observed: Vec<StatusZeroRow> = Vec::new();
    for (name, first) in [
        ("connection refused", Behaviour::RefusesConnection),
        ("deadline expiry", Behaviour::Hangs),
    ] {
        let second = SeatProvider::new(Behaviour::Answers);
        let router = router_over(
            &[(FIRST, SeatProvider::new(first)), (SECOND, second.clone())],
            &[FIRST, SECOND],
            Some(1),
        );

        let end = count(&router).await;

        observed.push((name, shape(&end), second.calls(), phase(&router, FIRST)));
    }

    let expected: Vec<StatusZeroRow> = vec![
        (
            "connection refused",
            "scripted upstream 0: connection refused".into(),
            0,
            CircuitPhase::Open,
        ),
        ("deadline expiry", "served".into(), 1, CircuitPhase::Open),
    ];
    assert_eq!(observed, expected);
}

/// `(row, the seats and their behaviours, which seats are parked, how the count ends)`.
type ExhaustionRow = (
    &'static str,
    [(&'static str, Behaviour); 2],
    &'static [&'static str],
    String,
);

#[tokio::test(start_paused = true)]
async fn an_exhausted_count_returns_the_last_timeout_then_a_parked_seat_then_the_501() {
    let all_refused = "not implemented: count_tokens: all capable providers returned a \
                       capability error (cannot count)";
    let rows: [ExhaustionRow; 6] = [
        (
            "the last of two timeouts",
            [(FIRST, Behaviour::Hangs), (SECOND, Behaviour::Hangs)],
            &[],
            timed_out_on(SECOND),
        ),
        (
            "a timeout outranks a later 501",
            [(FIRST, Behaviour::Hangs), (SECOND, Behaviour::CannotCount)],
            &[],
            timed_out_on(FIRST),
        ),
        (
            "a timeout outranks a later parked seat",
            [(FIRST, Behaviour::Hangs), (SECOND, Behaviour::Answers)],
            &[SECOND],
            timed_out_on(FIRST),
        ),
        (
            "a timeout outranks an earlier parked seat",
            [(FIRST, Behaviour::Answers), (SECOND, Behaviour::Hangs)],
            &[FIRST],
            timed_out_on(SECOND),
        ),
        (
            "a parked seat outranks the 501",
            [
                (FIRST, Behaviour::Answers),
                (SECOND, Behaviour::CannotCount),
            ],
            &[FIRST],
            "credential".into(),
        ),
        (
            "every capable seat refuses",
            [
                (FIRST, Behaviour::CannotCount),
                (SECOND, Behaviour::CannotCount),
            ],
            &[],
            all_refused.into(),
        ),
    ];

    for (name, seats, parked, expected) in rows {
        let seats = seats.map(|(nickname, behaviour)| (nickname, SeatProvider::new(behaviour)));
        let router = router_over(&seats, &[FIRST, SECOND], Some(0));
        for nickname in parked {
            park(&router, nickname);
        }

        let end = count(&router).await;

        assert_eq!(shape(&end), expected, "{name}");
    }
}

#[tokio::test(start_paused = true)]
async fn an_expired_count_carries_the_same_error_text_as_an_expired_completion() {
    let complete_router = router_over(
        &[(FIRST, SeatProvider::new(Behaviour::Hangs))],
        &[FIRST],
        Some(0),
    );
    let count_router = router_over(
        &[(FIRST, SeatProvider::new(Behaviour::Hangs))],
        &[FIRST],
        Some(0),
    );

    let completed = tokio::time::timeout(
        OUTER_BOUND,
        Box::pin(complete_router.complete_with_options(request(), RouterOptions::default())),
    )
    .await
    .expect("the attempt deadline ends a hanging completion")
    .result;
    let counted = count(&count_router).await;

    let body = |outcome: Option<&Error>| match outcome {
        Some(Error::Upstream {
            status: 0, body, ..
        }) => Some(body.clone()),
        _ => None,
    };
    let expected = format!("request timed out after {TIMEOUT_MS}ms");
    assert_eq!(body(completed.as_ref().err()), Some(expected.clone()));
    assert_eq!(body(counted.as_ref().err()), Some(expected));
}

#[tokio::test(start_paused = true)]
async fn a_count_chain_naming_one_seat_twice_revisits_it_after_a_timeout() {
    let only = SeatProvider::new(Behaviour::HangsOnce);
    let router = router_over(&[(FIRST, only.clone())], &[FIRST, FIRST], Some(0));

    let counted = tokio::time::timeout(
        OUTER_BOUND,
        Box::pin(router.count_tokens_with_meta(request())),
    )
    .await
    .expect("the attempt deadline ends the hanging first visit");

    assert!(counted.result.is_ok(), "{:?}", counted.result);
    assert_eq!(
        (
            only.calls(),
            counted.meta.attempt_count,
            counted.meta.fallback_count
        ),
        (2, 2, 1),
    );
    assert_ne!(phase(&router, FIRST), CircuitPhase::Open);
}
