//! How far one failure reaches across a whole request, on the chain shape a
//! seat pool behind a passthrough exercises: pool A (five seats on origin X)
//! -> a passthrough entry on origin Y -> pool B (one seat on origin X). Every
//! row dials real base URLs through the public dispatch entry points and
//! counts upstream calls per seat and breaker failures per seat, so a row
//! reads exactly the calls one client request made.
//!
//! Also pinned here: the request-wide retry budget (`max_attempts = N` is at
//! most N - 1 same-target retries per client request, shared across the walk,
//! and neither a repair resend nor an auth refresh spends it), what a walk
//! that excluded every target returns, that an excluded target takes no
//! rate-limit token, no half-open slot and no skip tally, and the walk counts
//! a request stamps on its observation.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::{self, BoxStream, StreamExt};
use parking_lot::Mutex;
use routectl_core::schema::ForwardedBearer;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Provider, RequestObservation, Result, ScopeRow,
    SkipCounts, TokenCount, WalkCounts,
};
use serde_json::{Value, json};

use super::super::field_repair::{TEST_UNSUPPORTED_PATH, TEST_UNSUPPORTED_SENTINEL};
use super::super::scripted_provider_test_support::{content_chunk, ok_response};
use super::super::{Router, RouterOptions};
use crate::capability_matcher::FORCED_TOOL_CHOICE_REJECTION;
use crate::class_policy::ConfigFailureClass;
use crate::config::{AliasValue, Config, PoolEntry, ProviderEntry, ProviderRuntimePolicy};
use crate::resolved::ResolvedModel;
use crate::runtime_state::CircuitPhase;
use crate::seat_pool::{SeatTarget, seat_state_key};

const ALIAS: &str = "chain";
const ORIGIN_X: &str = "https://api.anthropic.com";
const ORIGIN_Y: &str = "https://passthrough.example.test";
const POOL_A_SEATS: usize = 5;
/// pool A's seats, the passthrough, pool B's seat.
const TARGETS: usize = POOL_A_SEATS + 2;
const PASSTHROUGH: usize = POOL_A_SEATS;
const POOL_B: usize = POOL_A_SEATS + 1;

/// One upstream answer.
#[derive(Clone, Copy, Debug)]
enum Step {
    Serve,
    Fail(Failure),
}

/// The failures a row scripts, each built as the egress surfaces it.
#[derive(Clone, Copy, Debug)]
enum Failure {
    /// A generic 400 naming no field and no capability.
    BadRequest,
    /// A 400 the classifier lifts to an unsupported feature.
    FeatureUnsupported,
    /// A 400 the classifier lifts to an exceeded context window.
    ContextWindow,
    /// An Anthropic 400 the classifier leaves as a bad request but the
    /// capability resolver recognizes: a model refusing a forced
    /// `tool_choice`.
    ForcedToolChoice,
    /// A bare OAuth 429 the provider recognized.
    RecognizedRateLimit,
    /// A 429 no recognizer matched.
    RateLimit,
    Server,
    /// A 401 the auth refresh answers.
    Unauthorized,
    /// A 400 refusing the test-only field, which the learner repairs.
    RefusedField,
    /// A 501: the upstream cannot count tokens.
    NotImplemented,
}

impl Failure {
    fn error(self) -> Error {
        match self {
            Self::BadRequest => anthropic_error(400, "invalid_request_error", "invalid request"),
            Self::FeatureUnsupported => Error::upstream_full(
                "scripted",
                400,
                json!({"error": {"type": "invalid_request_error", "code": "unsupported_parameter",
                    "message": "unsupported"}})
                .to_string(),
                None,
                Some("invalid_request_error".into()),
                Some("unsupported_parameter".into()),
            ),
            Self::ContextWindow => Error::upstream_full(
                "scripted",
                400,
                json!({"error": {"type": "invalid_request_error",
                    "code": "context_length_exceeded", "message": "too long"}})
                .to_string(),
                None,
                Some("invalid_request_error".into()),
                Some("context_length_exceeded".into()),
            ),
            Self::ForcedToolChoice => {
                anthropic_error(400, "invalid_request_error", FORCED_TOOL_CHOICE_REJECTION)
            }
            Self::RecognizedRateLimit => anthropic_error(429, "rate_limit_error", "Error")
                .with_scope_row(Some(ScopeRow::AnthropicBareRateLimit)),
            Self::RateLimit => anthropic_error(429, "rate_limit_error", "slow down"),
            Self::Server => anthropic_error(500, "api_error", "internal"),
            Self::Unauthorized => anthropic_error(401, "authentication_error", "expired"),
            Self::RefusedField => anthropic_error(
                400,
                "invalid_request_error",
                &format!("{TEST_UNSUPPORTED_PATH}: Extra inputs are not permitted"),
            ),
            Self::NotImplemented => anthropic_error(501, "api_error", "not implemented"),
        }
    }
}

fn anthropic_error(status: u16, kind: &str, message: &str) -> Error {
    let body = json!({"type": "error", "error": {"type": kind, "message": message}});
    Error::upstream_full(
        "scripted",
        status,
        body.to_string(),
        None,
        Some(kind.into()),
        None,
    )
}

/// One seat: answers from its script, one step per call, repeating the last
/// step once the script runs out; counts calls and auth refreshes.
struct Seat {
    script: Mutex<Vec<Step>>,
    calls: AtomicUsize,
    refreshes: AtomicUsize,
}

impl Seat {
    fn new(script: &[Step]) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(script.iter().rev().copied().collect()),
            calls: AtomicUsize::new(0),
            refreshes: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn answer(&self) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let step = {
            let mut script = self.script.lock();
            if script.len() > 1 {
                script.pop()
            } else {
                script.last().copied()
            }
        };
        match step.unwrap_or(Step::Serve) {
            Step::Serve => Ok(()),
            Step::Fail(failure) => Err(failure.error()),
        }
    }
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
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        self.answer().map(|()| TokenCount {
            input_tokens: 1,
            ..Default::default()
        })
    }
    async fn on_auth_failure(&self) -> Result<()> {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// How the chain is laid out: whether pool B is its own `[models]` entry or
/// the same entry as pool A, and what its entry carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PoolB {
    /// Pool B is another `[models]` entry with another upstream id.
    OtherModel,
    /// Pool B is another `[models]` entry with the SAME upstream id plus a
    /// beta header: a larger-window variant of pool A's model.
    BetaVariant,
}

struct Chain {
    router: Router,
    seats: Vec<Arc<Seat>>,
}

impl Chain {
    fn calls(&self) -> Vec<usize> {
        self.seats.iter().map(|seat| seat.calls()).collect()
    }

    fn state_key(idx: usize) -> String {
        match idx {
            i if i < POOL_A_SEATS => seat_state_key("pool-a", Some(&seat_member(i))),
            PASSTHROUGH => "passthrough".into(),
            _ => seat_state_key("pool-b", Some("b-0")),
        }
    }

    fn failures(&self, idx: usize) -> u32 {
        self.router
            .state
            .get(&Self::state_key(idx))
            .expect("every chain target has a state slot")
            .lock()
            .consecutive_failures_for_tests()
    }

    fn debits(&self) -> Vec<u32> {
        (0..TARGETS).map(|idx| self.failures(idx)).collect()
    }
}

fn seat_member(idx: usize) -> String {
    format!("a-{idx}")
}

/// The egress kind every chain target is configured with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Lane {
    Anthropic,
    /// An OpenAI-compatible gateway, whose classifier table lifts a 400 to
    /// an unsupported feature or an exceeded window; the anthropic-api table
    /// lifts neither.
    OpenaiCompat,
    /// The anthropic-api lane relaying the client's credential on origin X;
    /// the passthrough on origin Y keeps its own key, since a forwarded
    /// entry must egress to api.anthropic.com.
    AnthropicForwarded,
}

fn anthropic_entry(base_url: &str, runtime: ProviderRuntimePolicy) -> ProviderEntry {
    entry(Lane::Anthropic, base_url, runtime)
}

fn entry(lane: Lane, origin: &str, runtime: ProviderRuntimePolicy) -> ProviderEntry {
    let toml_text = match lane {
        Lane::Anthropic => format!(
            "kind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\nbase_url = \"{origin}\"\n"
        ),
        Lane::OpenaiCompat => format!(
            "kind = \"openai-compat\"\napi_key_ref = \"literal:k\"\nbase_url = \"{origin}/v1\"\n"
        ),
        Lane::AnthropicForwarded if origin == ORIGIN_X => format!(
            "kind = \"anthropic-api\"\ncredential_source = \"forwarded\"\nbase_url = \"{origin}\"\n"
        ),
        Lane::AnthropicForwarded => return entry(Lane::Anthropic, origin, runtime),
    };
    let parsed: ProviderEntry = toml::from_str(&toml_text).expect("valid provider entry");
    parsed.with_runtime(runtime)
}

/// A breaker no single failure trips, so a debit shows as a failure count.
fn quiet_breaker() -> ProviderRuntimePolicy {
    ProviderRuntimePolicy {
        circuit_failures: Some(100),
        ..Default::default()
    }
}

/// The chain shape with each target's script, under `max_attempts`.
fn chain(scripts: &[&[Step]; TARGETS], max_attempts: u32, pool_b: PoolB) -> Chain {
    chain_with(scripts, max_attempts, pool_b, Lane::Anthropic, |_| {
        quiet_breaker()
    })
}

fn chain_with(
    scripts: &[&[Step]; TARGETS],
    max_attempts: u32,
    pool_b: PoolB,
    lane: Lane,
    runtime: impl Fn(usize) -> ProviderRuntimePolicy,
) -> Chain {
    chain_configured(scripts, max_attempts, pool_b, lane, runtime, |_| {})
}

/// [`chain_with`], with `configure` applied to the config last.
fn chain_configured(
    scripts: &[&[Step]; TARGETS],
    max_attempts: u32,
    pool_b: PoolB,
    lane: Lane,
    runtime: impl Fn(usize) -> ProviderRuntimePolicy,
    configure: impl FnOnce(&mut Config),
) -> Chain {
    let mut config = Config::default();
    config.retry.max_attempts = max_attempts;
    config.retry.initial_backoff_ms = 0;
    config.retry.jitter_ms = 0;
    for idx in 0..POOL_A_SEATS {
        config
            .providers
            .insert(seat_member(idx), entry(lane, ORIGIN_X, runtime(idx)));
    }
    config.providers.insert(
        "passthrough".into(),
        entry(lane, ORIGIN_Y, runtime(PASSTHROUGH)),
    );
    config
        .providers
        .insert("b-0".into(), entry(lane, ORIGIN_X, runtime(POOL_B)));
    config.pools.insert(
        "pool-a-seats".into(),
        PoolEntry::new((0..POOL_A_SEATS).map(seat_member).collect()),
    );
    config
        .pools
        .insert("pool-b-seats".into(), PoolEntry::new(vec!["b-0".into()]));
    config.aliases.insert(
        ALIAS.into(),
        AliasValue::Chain(vec!["pool-a".into(), "passthrough".into(), "pool-b".into()]),
    );
    configure(&mut config);

    let forwarded = lane == Lane::AnthropicForwarded;
    let seats: Vec<Arc<Seat>> = scripts.iter().map(|script| Seat::new(script)).collect();
    let mut router = Router::new(Arc::new(config));
    let pool_a: Vec<SeatTarget> = (0..POOL_A_SEATS)
        .map(|idx| SeatTarget::new(seat_member(idx), provider(&seats[idx]), None, forwarded))
        .collect();
    let pool_b_upstream = "claude-pool-b";
    let mut b_model = ResolvedModel::new(
        "pool-b",
        "pool-b-seats",
        provider(&seats[POOL_B]),
        match pool_b {
            PoolB::OtherModel => pool_b_upstream,
            PoolB::BetaVariant => "claude-pool-a",
        },
    );
    if pool_b == PoolB::BetaVariant {
        b_model = b_model.with_header_extras(BTreeMap::from([(
            "anthropic-beta".to_string(),
            "context-1m-2025-08-07".to_string(),
        )]));
    }
    let b_model = b_model.with_seats(Arc::from(vec![SeatTarget::new(
        "b-0".into(),
        provider(&seats[POOL_B]),
        None,
        forwarded,
    )]));
    let models = BTreeMap::from([
        (
            "pool-a".to_string(),
            Arc::new(
                ResolvedModel::new(
                    "pool-a",
                    "pool-a-seats",
                    provider(&seats[0]),
                    "claude-pool-a",
                )
                .with_seats(Arc::from(pool_a)),
            ),
        ),
        (
            "passthrough".to_string(),
            Arc::new(ResolvedModel::new(
                "passthrough",
                "passthrough",
                provider(&seats[PASSTHROUGH]),
                "claude-passthrough",
            )),
        ),
        ("pool-b".to_string(), Arc::new(b_model)),
    ]);
    router.install_resolved_models(models);
    Chain { router, seats }
}

fn provider(seat: &Arc<Seat>) -> Arc<dyn Provider> {
    Arc::clone(seat) as Arc<dyn Provider>
}

fn request() -> ChatRequest {
    ChatRequest {
        model: ALIAS.into(),
        messages: vec![].into(),
        max_tokens: Some(64),
        ..Default::default()
    }
}

fn probe_request() -> ChatRequest {
    ChatRequest {
        max_tokens: Some(1),
        ..request()
    }
}

const SERVE: &[Step] = &[Step::Serve];

fn failing(failure: Failure) -> &'static [Step] {
    match failure {
        Failure::BadRequest => &[Step::Fail(Failure::BadRequest)],
        Failure::FeatureUnsupported => &[Step::Fail(Failure::FeatureUnsupported)],
        Failure::ContextWindow => &[Step::Fail(Failure::ContextWindow)],
        Failure::ForcedToolChoice => &[Step::Fail(Failure::ForcedToolChoice)],
        Failure::RecognizedRateLimit => &[Step::Fail(Failure::RecognizedRateLimit)],
        Failure::RateLimit => &[Step::Fail(Failure::RateLimit)],
        Failure::Server => &[Step::Fail(Failure::Server)],
        Failure::Unauthorized => &[Step::Fail(Failure::Unauthorized)],
        Failure::RefusedField => &[Step::Fail(Failure::RefusedField)],
        Failure::NotImplemented => &[Step::Fail(Failure::NotImplemented)],
    }
}

/// Every target serves except the first seat of pool A, which fails with
/// `failure` on every call.
fn first_seat_failing(failure: Failure) -> [&'static [Step]; TARGETS] {
    let mut scripts = [SERVE; TARGETS];
    scripts[0] = failing(failure);
    scripts
}

/// Every target fails with `failure` on every call.
fn all_failing(failure: Failure) -> [&'static [Step]; TARGETS] {
    [failing(failure); TARGETS]
}

/// The calls of a walk where pool A's first seat failed once and target
/// `idx` served.
fn first_seat_then(idx: usize) -> Vec<usize> {
    let mut calls = vec![0; TARGETS];
    calls[0] = 1;
    calls[idx] += 1;
    calls
}

// ---------------------------------------------------------------------------
// One failure, one reach
// ---------------------------------------------------------------------------

/// A generic 400 on pool A's first seat excludes its origin: pool A's
/// siblings and pool B, both on origin X, are never dialed, and the
/// passthrough on origin Y serves. Two calls, no debit.
///
/// Mutation check: drop the endpoint exclusion in `exclusions_for` -> red
/// (the sibling seat on origin X serves instead).
#[tokio::test]
async fn a_generic_400_makes_two_calls_and_reaches_only_another_origin() {
    let chain = chain(
        &first_seat_failing(Failure::BadRequest),
        3,
        PoolB::OtherModel,
    );

    let dispatched = chain
        .router
        .complete_with_options(request(), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    assert_eq!(chain.calls(), first_seat_then(PASSTHROUGH));
    assert_eq!(chain.debits(), vec![0; TARGETS], "a 400 never debits");
}

/// A native 400 never debits, whatever class a remap gives it: the request
/// caused it. The remap below is one validation refuses, set on the runtime
/// policy directly, so only the debit rule's own status check spares the
/// seat.
///
/// Mutation check: drop the native 400/422 arm of `breaker_outcome` -> red
/// (the seat takes a debit).
#[tokio::test]
async fn a_400_remapped_to_a_debiting_class_still_never_debits() {
    let chain = chain_with(
        &first_seat_failing(Failure::BadRequest),
        3,
        PoolB::OtherModel,
        Lane::Anthropic,
        |idx| ProviderRuntimePolicy {
            class_overrides: if idx == 0 {
                BTreeMap::from([(400, ConfigFailureClass::ServerError)])
            } else {
                BTreeMap::new()
            },
            ..quiet_breaker()
        },
    );

    let dispatched = chain
        .router
        .complete_with_options(request(), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    assert_eq!(
        chain.calls(),
        first_seat_then(PASSTHROUGH),
        "the native 400 keeps its endpoint scope and is not retried"
    );
    assert_eq!(chain.debits(), vec![0; TARGETS]);
}

/// A recognized bare 429 is scoped to its endpoint and not retried: two
/// calls, and exactly one debit, on the seat that answered it.
#[tokio::test]
async fn a_recognized_bare_429_makes_two_calls_and_debits_once() {
    let chain = chain(
        &first_seat_failing(Failure::RecognizedRateLimit),
        3,
        PoolB::OtherModel,
    );

    let dispatched = chain
        .router
        .complete_with_options(request(), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    assert_eq!(chain.calls(), first_seat_then(PASSTHROUGH));
    let mut debits = vec![0; TARGETS];
    debits[0] = 1;
    assert_eq!(chain.debits(), debits);
}

/// An unrecognized 429 on every target is target-scoped and retryable: the
/// request spends its `max_attempts - 1` retries once across the walk and
/// dials every other target once.
#[tokio::test]
async fn an_unrecognized_429_is_bounded_by_the_request_wide_retries_plus_targets() {
    for max_attempts in [1, 3] {
        let chain = chain(
            &all_failing(Failure::RateLimit),
            max_attempts,
            PoolB::OtherModel,
        );

        let dispatched = chain
            .router
            .complete_with_options(request(), RouterOptions::new())
            .await;

        assert!(dispatched.result.is_err());
        let total: usize = chain.calls().iter().sum();
        let bound = (max_attempts as usize - 1) + TARGETS;
        assert_eq!(total, bound, "max_attempts = {max_attempts}");
        assert_eq!(
            chain.calls()[0],
            max_attempts as usize,
            "max_attempts = {max_attempts}: the retries land on the first target"
        );
    }
}

/// A typed capability 400 or an exceeded window is scoped to its `[models]`
/// entry: pool A's siblings are skipped, and pool B, another entry on the
/// SAME origin, is reached, whether it names another upstream or the same
/// upstream as a beta-gated variant. On a lane whose classifier lifts both
/// classes.
#[tokio::test]
async fn a_capability_or_window_400_reaches_pool_b_on_the_same_origin() {
    let rows = [
        (
            "feature unsupported",
            Failure::FeatureUnsupported,
            PoolB::OtherModel,
        ),
        ("context window", Failure::ContextWindow, PoolB::OtherModel),
        (
            "feature unsupported, beta variant",
            Failure::FeatureUnsupported,
            PoolB::BetaVariant,
        ),
        (
            "context window, beta variant",
            Failure::ContextWindow,
            PoolB::BetaVariant,
        ),
    ];
    for (name, failure, pool_b) in rows {
        let mut scripts = all_failing(failure);
        scripts[POOL_B] = SERVE;
        let chain = chain_with(&scripts, 3, pool_b, Lane::OpenaiCompat, |_| quiet_breaker());

        let dispatched = chain
            .router
            .complete_with_options(request(), RouterOptions::new())
            .await;

        assert!(
            dispatched.result.is_ok(),
            "{name}: {:?}",
            dispatched.result.err()
        );
        let mut calls = vec![0; TARGETS];
        calls[0] = 1;
        calls[PASSTHROUGH] = 1;
        calls[POOL_B] = 1;
        assert_eq!(chain.calls(), calls, "{name}");
    }
}

/// How a row of the resolver-recognized 400 test dispatches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Variant {
    Complete,
    /// `[capability] enabled = false`.
    LearningOff,
    Stream,
    /// Pool A and pool B relay the client's credential.
    Forwarded,
}

impl Variant {
    async fn dispatch(self, scripts: &[&[Step]; TARGETS]) -> (Chain, bool) {
        let lane = if self == Self::Forwarded {
            Lane::AnthropicForwarded
        } else {
            Lane::Anthropic
        };
        let chain = chain_configured(
            scripts,
            3,
            PoolB::OtherModel,
            lane,
            |_| quiet_breaker(),
            |config| config.capability.enabled = self != Self::LearningOff,
        );
        let mut req = request();
        if self == Self::Forwarded {
            let unforwarded = chain
                .router
                .complete_with_options(request(), RouterOptions::new())
                .await;
            assert!(
                unforwarded.result.is_err() && chain.calls() == vec![0; TARGETS],
                "premise: pool A relays the client's credential"
            );
            req.routectl_internal.forwarded_bearer =
                Some(ForwardedBearer::new("client-bearer".into()));
        }
        let served = if self == Self::Stream {
            chain
                .router
                .stream_with_options(req, RouterOptions::new())
                .await
                .result
                .is_ok()
        } else {
            chain
                .router
                .complete_with_options(req, RouterOptions::new())
                .await
                .result
                .is_ok()
        };
        (chain, served)
    }
}

/// On the anthropic-api lane, whose classifier lifts neither class, a 400
/// is scoped to the model only when the capability resolver names a
/// capability in it: a model refusing a forced `tool_choice` lets the walk
/// reach pool B on the same origin, while a generic 400 on every target
/// stops at origin X (the passthrough on origin Y is its only other dial).
/// The scope holds with capability learning off, on the stream surface, and
/// on a lane that relays the client's credential, none of which learns.
///
/// Mutation check: set the resolver flag to false at the dispatch walk's
/// failure build -> red (the capability rows never reach pool B).
#[tokio::test]
async fn a_resolver_recognized_400_reaches_pool_b_and_a_generic_one_does_not() {
    let rows = [
        (
            "forced tool_choice",
            Failure::ForcedToolChoice,
            Variant::Complete,
            1,
        ),
        (
            "forced tool_choice, learning off",
            Failure::ForcedToolChoice,
            Variant::LearningOff,
            1,
        ),
        (
            "forced tool_choice, stream",
            Failure::ForcedToolChoice,
            Variant::Stream,
            1,
        ),
        (
            "forced tool_choice, forwarded credential",
            Failure::ForcedToolChoice,
            Variant::Forwarded,
            1,
        ),
        ("generic 400", Failure::BadRequest, Variant::Complete, 0),
    ];
    for (name, failure, variant, pool_b_calls) in rows {
        let mut scripts = all_failing(failure);
        scripts[POOL_B] = SERVE;

        let (chain, served) = variant.dispatch(&scripts).await;

        assert_eq!(served, pool_b_calls == 1, "{name}");
        let mut calls = vec![0; TARGETS];
        calls[0] = 1;
        calls[PASSTHROUGH] = 1;
        calls[POOL_B] = pool_b_calls;
        assert_eq!(chain.calls(), calls, "{name}");
        assert_eq!(
            chain.debits(),
            vec![0; TARGETS],
            "{name}: a 400 never debits"
        );
    }
}

/// A probe's 429 stays inside pool A, walking its seats, never retrying and
/// never debiting; it never reaches the passthrough or pool B.
#[tokio::test]
async fn a_probe_429_stays_in_pool_a_with_no_debit() {
    let chain = chain(&all_failing(Failure::RateLimit), 3, PoolB::OtherModel);

    let dispatched = chain
        .router
        .complete_with_options(probe_request(), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_err());
    let mut calls = vec![1; POOL_A_SEATS];
    calls.extend([0, 0]);
    assert_eq!(chain.calls(), calls);
    assert_eq!(chain.debits(), vec![0; TARGETS]);
}

/// A probe's 500 is a broken seat, not a quota answer: it debits.
#[tokio::test]
async fn a_probe_500_debits() {
    let chain = chain(&first_seat_failing(Failure::Server), 3, PoolB::OtherModel);

    let dispatched = chain
        .router
        .complete_with_options(probe_request(), RouterOptions::new())
        .await;

    assert!(
        dispatched.result.is_ok(),
        "the sibling seat answers the probe"
    );
    let mut calls = vec![0; TARGETS];
    calls[0] = 1;
    calls[1] = 1;
    assert_eq!(chain.calls(), calls, "no retry, the sibling seat serves");
    assert_eq!(chain.failures(0), 1);
}

/// A token count's 500 ends the request on the seat that answered it.
#[tokio::test]
async fn a_count_tokens_500_is_terminal() {
    let chain = chain(&first_seat_failing(Failure::Server), 3, PoolB::OtherModel);

    let counted = chain.router.count_tokens_with_meta(request()).await;

    assert!(
        matches!(counted.result, Err(Error::Upstream { status: 500, .. })),
        "{:?}",
        counted.result
    );
    let mut calls = vec![0; TARGETS];
    calls[0] = 1;
    assert_eq!(chain.calls(), calls);
}

/// A target-scoped failure excludes the seat that answered it under every
/// later chain entry naming it, so a chain that names the same pool again
/// does not re-dial it.
///
/// Mutation check: drop the target exclusion in `exclusions_for` -> red (the
/// failed seat is dialed again under the second entry).
#[tokio::test]
async fn a_failed_seat_is_not_redialed_when_the_chain_names_it_again() {
    let only = Seat::new(&[Step::Fail(Failure::Server)]);
    let other = Seat::new(&[Step::Fail(Failure::Server)]);
    let mut config = Config::default();
    config.retry.max_attempts = 1;
    for name in ["p-only", "p-other"] {
        let base = if name == "p-only" { ORIGIN_X } else { ORIGIN_Y };
        config
            .providers
            .insert(name.into(), anthropic_entry(base, quiet_breaker()));
    }
    config.aliases.insert(
        ALIAS.into(),
        AliasValue::Chain(vec!["only".into(), "other".into(), "only".into()]),
    );
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(BTreeMap::from([
        (
            "only".to_string(),
            Arc::new(ResolvedModel::new(
                "only",
                "p-only",
                provider(&only),
                "u-only",
            )),
        ),
        (
            "other".to_string(),
            Arc::new(ResolvedModel::new(
                "other",
                "p-other",
                provider(&other),
                "u-other",
            )),
        ),
    ]));

    let dispatched = router
        .complete_with_options(request(), RouterOptions::new())
        .await;

    assert!(matches!(
        dispatched.result,
        Err(Error::Upstream { status: 500, .. })
    ));
    assert_eq!((only.calls(), other.calls()), (1, 1));
}

// ---------------------------------------------------------------------------
// The request-wide retry budget
// ---------------------------------------------------------------------------

/// A single-target chain retries exactly `max_attempts - 1` times.
#[tokio::test]
async fn a_single_target_chain_retries_exactly_max_attempts_minus_one() {
    for max_attempts in [1, 2, 4] {
        let seat = Seat::new(&[Step::Fail(Failure::Server)]);
        let mut config = Config::default();
        config.retry.max_attempts = max_attempts;
        config.retry.initial_backoff_ms = 0;
        config.retry.jitter_ms = 0;
        config
            .providers
            .insert("p".into(), anthropic_entry(ORIGIN_X, quiet_breaker()));
        config
            .aliases
            .insert(ALIAS.into(), AliasValue::Chain(vec!["solo".into()]));
        let mut router = Router::new(Arc::new(config));
        router.install_resolved_models(BTreeMap::from([(
            "solo".to_string(),
            Arc::new(ResolvedModel::new("solo", "p", provider(&seat), "u")),
        )]));

        let dispatched = router
            .complete_with_options(request(), RouterOptions::new())
            .await;

        assert!(dispatched.result.is_err());
        assert_eq!(
            seat.calls(),
            max_attempts as usize,
            "max_attempts = {max_attempts}"
        );
    }
}

/// A repair resend does not spend the request's retries: with one retry
/// allowed, a seat that is repaired and then answers a 500 still retries
/// once.
///
/// Mutation check: spend the retry budget on a repair loop-back as well ->
/// red (the 500 is not retried).
#[tokio::test]
async fn a_repair_resend_does_not_spend_the_retry_budget() {
    let mut scripts = [SERVE; TARGETS];
    scripts[0] = &[
        Step::Fail(Failure::RefusedField),
        Step::Fail(Failure::Server),
        Step::Serve,
    ];
    let chain = chain(&scripts, 2, PoolB::OtherModel);
    let mut req = request();
    req.user = Some(TEST_UNSUPPORTED_SENTINEL.to_string());

    let dispatched = chain
        .router
        .complete_with_options(req, RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    assert_eq!(
        dispatched.meta.field_repairs.len(),
        1,
        "premise: one repair"
    );
    assert_eq!(
        chain.calls()[0],
        3,
        "the carried attempt, the repaired resend, and the one retry"
    );
}

/// The 401 refresh-and-retry does not spend the request's retries: with one
/// retry allowed, a seat refreshed after a 401 that then answers a 500 still
/// retries once.
#[tokio::test]
async fn an_auth_refresh_does_not_spend_the_retry_budget() {
    let mut scripts = [SERVE; TARGETS];
    scripts[0] = &[
        Step::Fail(Failure::Unauthorized),
        Step::Fail(Failure::Server),
        Step::Serve,
    ];
    let chain = chain(&scripts, 2, PoolB::OtherModel);

    let dispatched = chain
        .router
        .complete_with_options(request(), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    assert_eq!(chain.seats[0].refreshes.load(Ordering::SeqCst), 1);
    assert_eq!(
        chain.calls()[0],
        3,
        "the 401, the refreshed resend, and the one retry"
    );
}

/// The budget is shared across the walk: a retry spent on the first target
/// is not available to a later one.
///
/// Mutation check: count retries per chain entry again -> red (the
/// passthrough is retried too).
#[tokio::test]
async fn a_retry_spent_on_one_target_is_not_spent_again_on_the_next() {
    let mut scripts = [SERVE; TARGETS];
    scripts[0] = failing(Failure::Server);
    for script in scripts.iter_mut().take(POOL_A_SEATS).skip(1) {
        *script = failing(Failure::Server);
    }
    let chain = chain(&scripts, 2, PoolB::OtherModel);

    let dispatched = chain
        .router
        .complete_with_options(request(), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    assert_eq!(chain.calls(), vec![2, 1, 1, 1, 1, 1, 0]);
}

/// A failed seat whose breaker reads open after the failure's debit does not
/// spend the retry budget on a dial the re-gate would refuse, whoever opened
/// it: with one retry allowed, pool A's first seat is dialed once, and the
/// next seat still retries its own 500 once. In one row the seat's own 500
/// opens a breaker that trips on its first failure; in the other a concurrent
/// admission force-opens the seat between this request's admission and its
/// failure, so this request's debit carries an outdated epoch and opens
/// nothing itself.
///
/// Mutation check: ignore the open breaker when settling the disposition ->
/// red on both rows; key the retry drop on whether this debit opened the
/// breaker -> red on the concurrent row.
#[tokio::test]
async fn a_debit_that_opens_the_breaker_does_not_spend_the_retry() {
    let rows = [
        ("this debit opens it", 1, false),
        ("opened concurrently", 100, true),
    ];
    for (name, first_seat_threshold, opened_concurrently) in rows {
        let mut scripts = [SERVE; TARGETS];
        scripts[0] = failing(Failure::Server);
        scripts[1] = &[Step::Fail(Failure::Server), Step::Serve];
        let chain = chain_with(&scripts, 2, PoolB::OtherModel, Lane::Anthropic, |idx| {
            ProviderRuntimePolicy {
                circuit_failures: Some(if idx == 0 { first_seat_threshold } else { 100 }),
                ..Default::default()
            }
        });
        if opened_concurrently {
            force_open_after_next_admission(&chain, 0);
        }

        let dispatched = chain
            .router
            .complete_with_options(request(), RouterOptions::new())
            .await;

        assert!(
            dispatched.result.is_ok(),
            "{name}: {:?}",
            dispatched.result.err()
        );
        assert_eq!(chain.calls(), vec![1, 2, 0, 0, 0, 0, 0], "{name}");
        assert_eq!(
            chain
                .router
                .gate_status_for_tests(&Chain::state_key(0))
                .circuit,
            CircuitPhase::Open,
            "{name}: premise: the first seat's breaker is open"
        );
        let own_debit = u32::from(!opened_concurrently);
        assert_eq!(
            chain.failures(0),
            own_debit,
            "{name}: premise: only the seat's own trip counts this debit"
        );
    }
}

/// Force the breaker of chain target `idx` open for a minute right after the
/// next admission this thread makes, as a concurrent request would, which
/// moves the breaker to a new epoch.
fn force_open_after_next_admission(chain: &Chain, idx: usize) {
    let state = Arc::clone(
        chain
            .router
            .state
            .get(&Chain::state_key(idx))
            .expect("every chain target has a state slot"),
    );
    super::super::runtime_gate::set_after_admission_hook(move || {
        state
            .lock()
            .force_open(std::time::Instant::now(), Duration::from_mins(1));
    });
}

// ---------------------------------------------------------------------------
// Full exclusion and admission
// ---------------------------------------------------------------------------

/// A walk whose failures excluded every remaining target returns the genuine
/// upstream error that excluded them, not a local refusal.
#[tokio::test]
async fn full_exclusion_returns_the_genuine_upstream_error() {
    let mut scripts = all_failing(Failure::BadRequest);
    scripts[PASSTHROUGH] = failing(Failure::BadRequest);
    let chain = chain(&scripts, 3, PoolB::OtherModel);

    let dispatched = chain
        .router
        .complete_with_options(request(), RouterOptions::new())
        .await;

    assert!(
        matches!(dispatched.result, Err(Error::Upstream { status: 400, .. })),
        "{:?}",
        dispatched.result
    );
    let mut calls = vec![0; TARGETS];
    calls[0] = 1;
    calls[PASSTHROUGH] = 1;
    assert_eq!(chain.calls(), calls);
}

/// An excluded target is passed over before the gate: its rate-limit token
/// is untouched, its half-open slot is not claimed, and the request records
/// no skip for it. Pool A's second seat has one RPM token and a breaker
/// ready for a half-open probe; after the 400 excludes it, a second request
/// can still use both.
#[tokio::test]
async fn an_excluded_target_takes_no_rpm_token_no_probe_slot_and_no_skip() {
    let chain = chain_with(
        &first_seat_failing(Failure::BadRequest),
        3,
        PoolB::OtherModel,
        Lane::Anthropic,
        |idx| match idx {
            1 => ProviderRuntimePolicy {
                rpm_limit: Some(1),
                ..quiet_breaker()
            },
            _ => quiet_breaker(),
        },
    );
    assert!(
        chain
            .router
            .force_open_breaker(&Chain::state_key(1), Duration::ZERO)
    );
    let observation = RequestObservation::new();
    let mut req = request();
    req.routectl_internal.observation = Some(observation.clone());

    let first = chain
        .router
        .complete_with_options(req, RouterOptions::new())
        .await;

    assert!(first.result.is_ok(), "{:?}", first.result.err());
    assert_eq!(chain.calls(), first_seat_then(PASSTHROUGH));
    assert_eq!(observation.snapshot().skips, SkipCounts::default());
    let gate = chain.router.gate_status_for_tests(&Chain::state_key(1));
    assert!(!gate.half_open_probe_in_flight);
    assert_eq!(gate.circuit, CircuitPhase::HalfOpenReady);
    assert_eq!(gate.rpm_available.map(f64::floor), Some(1.0));
}

// ---------------------------------------------------------------------------
// Walk counts on the request observation
// ---------------------------------------------------------------------------

/// Three single-seat models: `a` and `b` on origin X, `c` on origin Y.
const SHORT_CHAIN: [(&str, &str); 3] = [("a", ORIGIN_X), ("b", ORIGIN_X), ("c", ORIGIN_Y)];

/// The short chain on `lane`, each target answering its script, with no
/// same-target retry.
fn short_chain(scripts: [&'static [Step]; 3], lane: Lane) -> (Router, Vec<Arc<Seat>>) {
    let mut config = Config::default();
    config.retry.max_attempts = 1;
    for (nickname, origin) in SHORT_CHAIN {
        config.providers.insert(
            format!("{nickname}-seat"),
            entry(lane, origin, quiet_breaker()),
        );
    }
    config.aliases.insert(
        ALIAS.into(),
        AliasValue::Chain(SHORT_CHAIN.iter().map(|(n, _)| (*n).into()).collect()),
    );
    let seats: Vec<Arc<Seat>> = scripts.iter().map(|script| Seat::new(script)).collect();
    let models = SHORT_CHAIN
        .iter()
        .zip(&seats)
        .map(|(&(nickname, _), seat)| {
            let model = ResolvedModel::new(
                nickname,
                format!("{nickname}-seat"),
                provider(seat),
                format!("claude-{nickname}"),
            );
            (nickname.to_owned(), Arc::new(model))
        })
        .collect();
    let mut router = Router::new(Arc::new(config));
    router.install_resolved_models(models);
    (router, seats)
}

/// The surface a walk-count row dispatches on.
#[derive(Clone, Copy, Debug)]
enum CountSurface {
    Complete,
    CountTokens,
}

/// Run one request on `surface` with an observation attached; the served
/// model and the walk counts the observation recorded.
async fn observed_walk(router: &Router, surface: CountSurface) -> (Option<String>, WalkCounts) {
    let observation = RequestObservation::new();
    let mut req = request();
    req.routectl_internal.observation = Some(observation.clone());
    let served = match surface {
        CountSurface::Complete => {
            let dispatched = router
                .complete_with_options(req, RouterOptions::new())
                .await;
            assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
            dispatched.meta.served_model
        }
        CountSurface::CountTokens => {
            let counted = router.count_tokens_with_meta(req).await;
            assert!(counted.result.is_ok(), "{:?}", counted.result.err());
            counted.meta.served_model
        }
    };
    (served, observation.snapshot().walk)
}

/// A generic 400 on `a` is the request's own failure: it counts once, and its
/// endpoint exclusion passes over `b` on the same origin, so `c` serves.
///
/// Mutation check: delete the `set_walk_counts` call in `finish_dispatch` ->
/// red (the snapshot reads all zeros).
#[tokio::test]
async fn a_generic_400_on_origin_x_counts_one_request_caused_failure_and_excludes_the_sibling_endpoint()
 {
    let (router, seats) = short_chain(
        [failing(Failure::BadRequest), SERVE, SERVE],
        Lane::Anthropic,
    );

    let (served, walk) = observed_walk(&router, CountSurface::Complete).await;

    assert_eq!(served.as_deref(), Some("c"));
    assert_eq!(
        seats.iter().map(|seat| seat.calls()).collect::<Vec<_>>(),
        vec![1, 0, 1]
    );
    assert_eq!(walk, WalkCounts::new(1, 1, 0, 0, 0));
}

/// A 400 the classifier lifts to an unsupported feature is scoped to its
/// `[models]` entry and still counts as caused by the request: `b`, another
/// entry on the same origin, serves, and nothing was passed over.
///
/// Mutation check: drop `Model` from the request-caused bump in
/// `Disposition::record_on` -> red (the count reads 0).
#[tokio::test]
async fn a_feature_unsupported_400_counts_under_model_scope() {
    let (router, _seats) = short_chain(
        [failing(Failure::FeatureUnsupported), SERVE, SERVE],
        Lane::OpenaiCompat,
    );

    let (served, walk) = observed_walk(&router, CountSurface::Complete).await;

    assert_eq!(served.as_deref(), Some("b"));
    assert_eq!(walk, WalkCounts::new(1, 0, 0, 0, 0));
}

/// Only a failure the request caused counts. A target-scoped 5xx or 429
/// counts nothing; the recognized bare 429, endpoint-scoped, counts one and
/// passes over `b`; a token count's 501 walks on without a failure decision
/// and counts nothing.
#[tokio::test]
async fn only_an_endpoint_or_model_scoped_failure_counts_as_request_caused() {
    let rows = [
        (
            "target-scoped 500",
            Failure::Server,
            CountSurface::Complete,
            "b",
            WalkCounts::new(0, 0, 0, 0, 0),
        ),
        (
            "unrecognized 429",
            Failure::RateLimit,
            CountSurface::Complete,
            "b",
            WalkCounts::new(0, 0, 0, 0, 0),
        ),
        (
            "recognized bare 429",
            Failure::RecognizedRateLimit,
            CountSurface::Complete,
            "c",
            WalkCounts::new(1, 1, 0, 0, 0),
        ),
        (
            "count_tokens 501 walks without a debit",
            Failure::NotImplemented,
            CountSurface::CountTokens,
            "b",
            WalkCounts::new(0, 0, 0, 0, 0),
        ),
    ];
    for (name, failure, surface, served_by, expected) in rows {
        let (router, seats) = short_chain([failing(failure), SERVE, SERVE], Lane::Anthropic);

        let (served, walk) = observed_walk(&router, surface).await;

        assert_eq!(served.as_deref(), Some(served_by), "{name}");
        assert_eq!(seats[0].calls(), 1, "{name}: the head failed once");
        assert_eq!(walk, expected, "{name}");
    }
}
