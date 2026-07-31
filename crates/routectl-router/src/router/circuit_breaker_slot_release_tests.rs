//! Regression: a half-open probe must never leave `half_open_in_flight`
//! stuck `true`, or every later gate check returns CircuitOpen and the
//! breaker is permanently locked open for that provider until process
//! restart. Two leak classes are covered here:
//!   - synchronous early-returns (probe fast-fail on 429/529, 401-refresh)
//!     that must release the slot before returning/continuing;
//!   - async CANCELLATION: a dispatch future dropped while awaiting the
//!     upstream, after the gate claimed the slot but before any settle arm
//!     runs -- covered by the `ProbeSlotGuard` drop backstop. (A status-0
//!     transport error is NOT a synchronous leak: it is fallbackable, so
//!     record_failure already clears the slot and re-trips cleanly.)
use super::*;
use crate::config::Config;
use crate::config::{ProviderEntry, ProviderRuntimePolicy};
use crate::resolved::ResolvedModel;
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider};
use routectl_core::{Result, TokenCount};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Provider that counts `complete()` calls and always 429s, so the
/// test can distinguish "gate granted a probe and reached the
/// upstream" (call count rises) from "gate returned CircuitOpen and
/// skipped the upstream" (call count flat).
struct Probe429Provider {
    id: String,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for Probe429Provider {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(&self.id, "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream(&self.id, 429, "rate limited"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("not exercised by these tests")
    }
}

/// Provider that counts `complete()` calls and always fails with a
/// configurable status + reset hint, so the reset-honoring tests can
/// drive the park / in-loop-retry decision and assert the resulting
/// breaker state. `status` shapes fallbackability (429 fallbackable,
/// 400 not); `retry_after` is the reset hint threaded through
/// `Error::Upstream.retry_after`.
struct RetryAfterProvider {
    id: String,
    status: u16,
    retry_after: Option<Duration>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for RetryAfterProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(&self.id, "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream_with_retry_after(
            &self.id,
            self.status,
            "rate limited",
            self.retry_after,
        ))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("not exercised by these tests")
    }
}

/// Like `build_router_with_provider_and_retry` but lets the test pin
/// the breaker threshold + cooldown so a single recorded failure does
/// NOT necessarily trip the breaker. Used by the reset-honoring tests
/// that must distinguish a force-park (breaker open immediately) from
/// a sub-threshold `record_failure` (breaker still closed).
fn build_router_with_breaker(
    provider: Arc<dyn Provider>,
    retry: RetryPolicy,
    circuit_failures: u32,
    circuit_cooldown_ms: u64,
) -> Router {
    let mut config = Config {
        retry,
        ..Default::default()
    };
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
            reduction_enabled: None,
            #[cfg(feature = "bedrock")]
            bedrock_mantle: None,
            runtime: ProviderRuntimePolicy {
                circuit_failures: Some(circuit_failures),
                circuit_cooldown_ms: Some(circuit_cooldown_ms),
                ..Default::default()
            },
        },
    );
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m".into(),
        Arc::new(ResolvedModel::new("m", "p", provider, "u")),
    );
    router.install_resolved_models(models);
    router
}

/// True when the per-model breaker would refuse a dispatch at `now`
/// (CircuitOpen). The reset-honoring tests use this to assert a park
/// happened (open) or did not (allow).
fn breaker_open_at(router: &Router, now: Instant) -> bool {
    let st = router.state.get("m").expect("per-model state slot exists");
    st.lock().try_dispatch(now) == GateDecision::CircuitOpen
}

/// Build a single-entry-chain Router around `provider` with a
/// threshold-1, zero-cooldown breaker. Zero cooldown: the breaker is
/// immediately half-open-eligible on the next dispatch, so the tests
/// need no wall-clock sleep to "advance past cooldown".
fn build_router_with_provider(provider: Arc<dyn Provider>) -> Router {
    build_router_with_provider_and_retry(provider, RetryPolicy::default())
}

/// Like `build_router_with_provider` but lets the test pin the
/// top-level `[retry]` policy (`policy_for` returns `config.retry`).
fn build_router_with_provider_and_retry(provider: Arc<dyn Provider>, retry: RetryPolicy) -> Router {
    let mut config = Config {
        retry,
        ..Default::default()
    };
    // anthropic-api kind so `count_tokens` treats the "p" target as
    // count_tokens-capable (the capability walk keys on
    // provider_kind == "anthropic-api"). The kind is irrelevant to
    // the complete/stream breaker tests that also use this helper;
    // they exercise the half-open probe-slot release, not the
    // count_tokens capability gate.
    config.providers.insert(
        "p".into(),
        ProviderEntry::AnthropicApi {
            api_key_ref: "literal:k".into(),
            base_url: "https://placeholder.invalid".into(),
            anthropic_version: "2023-06-01".into(),
            auth_kind: routectl_providers::anthropic_api::AuthKind::default(),
            credential_source: Default::default(),
            header_extras: BTreeMap::new(),
            payload_extras: None,
            user_agent: None,
            allowed_betas: vec![],
            forward_client_headers: vec![],
            context_management: false,
            max_thinking_entry_bytes: None,
            cache_capability: None,
            auto_emit_top_level_breakpoint: None,
            reduction_enabled: None,
            cloak: routectl_providers::anthropic_api::CloakConfig::default(),
            #[cfg(feature = "bedrock")]
            bedrock_mantle: None,
            runtime: ProviderRuntimePolicy {
                circuit_failures: Some(1),
                circuit_cooldown_ms: Some(0),
                ..Default::default()
            },
        },
    );
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m".into(),
        Arc::new(ResolvedModel::new("m", "p", provider, "u")),
    );
    router.install_resolved_models(models);
    router
}

fn build_router(calls: Arc<AtomicUsize>) -> Router {
    let provider: Arc<dyn Provider> = Arc::new(Probe429Provider {
        id: "p".into(),
        calls,
    });
    build_router_with_provider(provider)
}

fn probe_req() -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: vec![].into(),
        // max_tokens <= probe_max_tokens (default 1) => probe-shaped.
        max_tokens: Some(1),
        ..Default::default()
    }
}

#[tokio::test]
async fn probe_fast_fail_does_not_permanently_lock_breaker() {
    let calls = Arc::new(AtomicUsize::new(0));
    let router = build_router(calls.clone());

    // Trip the breaker directly (threshold = 1 failure).
    {
        let st = router.state.get("m").expect("per-model state slot exists");
        st.lock()
            .record_failure(Instant::now(), LastOutcome::Http5xx);
    }

    // First probe after the trip: the breaker is half-open, the gate
    // grants the single probe, the upstream 429s, and the probe
    // fast-fail releases the slot.
    let _ = router.complete(probe_req()).await.unwrap_err();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "first probe must reach the upstream (gate granted the half-open probe)",
    );

    // Second probe: if the slot had leaked, the gate would return
    // CircuitOpen and the upstream would NOT be touched. With the
    // slot released, the gate grants a fresh probe and the upstream
    // is reached again.
    let _ = router.complete(probe_req()).await.unwrap_err();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "second probe must also reach the upstream; a leaked half-open \
             slot would have locked the breaker (CircuitOpen) and skipped it",
    );
}

/// Streaming provider whose first chunk carries content, after which
/// the stream either completes cleanly (`mid_stream_error = false`)
/// or yields one error frame (`mid_stream_error = true`). Lets the
/// first-content-close tests separate the call-site close (on the
/// first content chunk) from the wrap's mid-stream accounting.
struct FirstChunkProvider {
    id: String,
    mid_stream_error: bool,
    stream_calls: Arc<AtomicUsize>,
}

/// A content-bearing chunk (non-empty text delta) carrying `id`. The
/// content-based commit boundary keys on this: a metadata-only chunk
/// would not commit the provider.
fn content_chunk(id: &str) -> ChatChunk {
    ChatChunk {
        id: id.into(),
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
    }
}

/// A content-free chunk carrying only a leading `delta.role` (the
/// OpenAI-Chat stream opener). Not the commit boundary: it must be
/// buffered until content arrives, never close a half-open breaker.
fn role_chunk() -> ChatChunk {
    ChatChunk {
        choices: vec![routectl_core::ChunkChoice {
            index: 0,
            delta: routectl_core::ChunkDelta {
                role: Some(routectl_core::Role::Assistant),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
}

#[async_trait]
impl Provider for FirstChunkProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(&self.id, "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!("not exercised by these tests")
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        let id = self.id.clone();
        let first = content_chunk("c0");
        if self.mid_stream_error {
            let err = Error::upstream(&id, 503, "mid-stream boom");
            let s = futures::stream::iter(vec![Ok(first), Err(err)]);
            Ok(s.boxed())
        } else {
            let second = content_chunk("c1");
            let s = futures::stream::iter(vec![Ok(first), Ok(second)]);
            Ok(s.boxed())
        }
    }
}

fn build_first_chunk_router(mid_stream_error: bool) -> (Router, Arc<AtomicUsize>) {
    let stream_calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(FirstChunkProvider {
        id: "p".into(),
        mid_stream_error,
        stream_calls: stream_calls.clone(),
    });
    // Threshold 1 with a long baseline cooldown: an OPEN breaker
    // reads CircuitOpen (so the re-trip / stays-closed assertions are
    // observable), while `force_open_breaker(.., ZERO)` still makes
    // the next dispatch the half-open probe. A re-trip via
    // record_failure restores the long baseline cooldown.
    let router = build_router_with_breaker(provider, RetryPolicy::default(), 1, 60_000);
    (router, stream_calls)
}

/// Put the model's breaker into the half-open state for the next
/// dispatch: open it with a zero-length park so the cooldown is
/// already elapsed and `try_dispatch` claims the single probe slot.
fn arm_half_open(router: &Router) {
    assert!(
        router.force_open_breaker("m", Duration::ZERO),
        "model breaker slot must exist",
    );
}

/// A half-open probe stream that succeeds on its first
/// chunk closes the breaker BEFORE the stream is fully consumed.
/// Before the fix the probe slot was held for the entire stream
/// duration, locking out concurrent requests.
#[tokio::test]
async fn first_chunk_success_closes_breaker_before_stream_consumed() {
    let (router, _calls) = build_first_chunk_router(false);
    arm_half_open(&router);

    let stream = router
        .stream(plain_req())
        .await
        .expect("first-chunk arrives -> Ok stream");

    // The returned stream is UNPOLLED here (not yet consumed). With
    // the first-chunk-close fix the breaker is already closed: the
    // half-open slot is released and the circuit is no longer open.
    assert!(
        !slot_in_flight(&router),
        "half-open probe slot must be released on first-chunk success, \
             not held for the whole stream",
    );
    // A closed breaker grants the next dispatch immediately.
    assert!(
        !breaker_open_at(&router, Instant::now()),
        "breaker must read CLOSED after first-chunk probe success",
    );

    drop(stream);
}

/// After the first-chunk close, N=threshold mid-stream
/// error frames re-trip the breaker (the wrap still records the
/// mid-stream failure via record_failure).
#[tokio::test]
async fn mid_stream_error_after_first_chunk_close_retrips_breaker() {
    let (router, _calls) = build_first_chunk_router(true);
    arm_half_open(&router);

    let stream = router
        .stream(plain_req())
        .await
        .expect("first-chunk arrives -> Ok stream");

    // Drain the stream: first chunk Ok (already closed the breaker),
    // then one error frame (threshold = 1) re-trips it.
    let items: Vec<_> = stream.collect().await;
    assert_eq!(items.len(), 2, "first chunk + one error frame");
    assert!(items[0].is_ok(), "first frame is the success chunk");
    assert!(items[1].is_err(), "second frame is the mid-stream error");

    // The mid-stream error re-tripped the breaker (baseline cooldown
    // restored): the next dispatch is refused.
    assert!(
        breaker_open_at(&router, Instant::now()),
        "a mid-stream error after first-chunk close must re-trip the breaker",
    );
}

/// Consumer cancellation (dropping the stream) AFTER a
/// first-chunk probe success must NOT re-trip the breaker. Proves the
/// `cancel_is_failure` removal is safe: the breaker was already
/// closed at the call site, so a cancel is irrelevant to the probe.
#[tokio::test]
async fn cancel_after_first_chunk_success_does_not_retrip_breaker() {
    let (router, _calls) = build_first_chunk_router(false);
    arm_half_open(&router);

    let stream = router
        .stream(plain_req())
        .await
        .expect("first-chunk arrives -> Ok stream");

    // Cancel by dropping the unconsumed stream.
    drop(stream);

    // The breaker stays CLOSED: the first-chunk success already
    // closed it, and the drop's benign record_success cannot reopen.
    assert!(
        !slot_in_flight(&router),
        "cancel after first-chunk success must leave the slot released",
    );
    assert!(
        !breaker_open_at(&router, Instant::now()),
        "cancel after first-chunk success must NOT re-trip the breaker",
    );
}

/// The three pre-content outcomes a half-open stream probe can take,
/// each preceded by a content-free `delta.role` opener: role-only then
/// EOS, role then content, or role then a mid-open error.
enum RoleOutcome {
    OnlyRole,
    ContentAfterRole,
    ErrorMidOpen,
}

/// Streaming provider that always opens with a content-free role chunk,
/// then takes the configured `RoleOutcome`. Lets the content-boundary
/// tests prove a role chunk does NOT close a half-open breaker while
/// first content does.
struct RoleProbeProvider {
    id: String,
    outcome: RoleOutcome,
    stream_calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for RoleProbeProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(&self.id, "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!("not exercised by these tests")
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        let id = self.id.clone();
        let items: Vec<Result<ChatChunk>> = match self.outcome {
            RoleOutcome::OnlyRole => vec![Ok(role_chunk())],
            RoleOutcome::ContentAfterRole => vec![Ok(role_chunk()), Ok(content_chunk("c1"))],
            RoleOutcome::ErrorMidOpen => {
                vec![
                    Ok(role_chunk()),
                    Err(Error::upstream(&id, 503, "mid-open boom")),
                ]
            }
        };
        Ok(futures::stream::iter(items).boxed())
    }
}

fn build_role_probe_router(outcome: RoleOutcome) -> Router {
    let provider: Arc<dyn Provider> = Arc::new(RoleProbeProvider {
        id: "p".into(),
        outcome,
        stream_calls: Arc::new(AtomicUsize::new(0)),
    });
    build_router_with_provider(provider)
}

#[tokio::test]
async fn half_open_role_only_does_not_close_breaker() {
    // A half-open probe stream that emits ONLY a content-free role chunk
    // then EOS is a pre-content empty stream: the role must NOT close the
    // breaker (no spurious record_success). It re-trips cleanly and the
    // probe slot is released -- recoverable, not latched closed or open.
    let router = build_role_probe_router(RoleOutcome::OnlyRole);
    arm_half_open(&router);

    let r = router.stream(plain_req()).await;
    assert!(
        r.is_err(),
        "role-only pre-content empty stream must fall over"
    );
    assert!(
        !slot_in_flight(&router),
        "the half-open probe slot must be released after a role-only probe",
    );
    assert_eq!(
        circuit_phase(&router),
        crate::runtime_state::CircuitPhase::HalfOpenReady,
        "a role chunk must NOT close the breaker; it re-trips and stays recoverable",
    );
}

#[tokio::test]
async fn half_open_first_content_closes_breaker() {
    // A half-open probe stream that emits a role opener THEN content
    // closes the breaker on first content (not on the role).
    let router = build_role_probe_router(RoleOutcome::ContentAfterRole);
    arm_half_open(&router);

    let stream = router
        .stream(plain_req())
        .await
        .expect("role + content commits -> Ok stream");
    assert!(
        !slot_in_flight(&router),
        "first content releases the half-open probe slot",
    );
    assert_eq!(
        circuit_phase(&router),
        crate::runtime_state::CircuitPhase::Closed,
        "first content must close the half-open breaker",
    );
    drop(stream);
}

#[tokio::test]
async fn half_open_role_then_error_records_failure_and_releases_slot() {
    // A half-open probe that opens with a role then errors before any
    // content records a breaker failure (re-trips) and releases the slot
    // -- the role never committed, so this is a pre-content failure.
    let router = build_role_probe_router(RoleOutcome::ErrorMidOpen);
    arm_half_open(&router);

    let r = router.stream(plain_req()).await;
    assert!(r.is_err(), "a pre-content error must fall over");
    assert!(
        !slot_in_flight(&router),
        "the half-open probe slot must be released after a role-then-error probe",
    );
    assert_eq!(
        circuit_phase(&router),
        crate::runtime_state::CircuitPhase::HalfOpenReady,
        "role-then-error records a failure (re-trips) and stays recoverable",
    );
}

/// Multi-surface mock for the half-open-probe-gets-401-then-refresh-
/// succeeds path the slot-release fix targets. Each of `complete`,
/// `stream`, and `count_tokens` returns `Error::Upstream { status:
/// 401, .. }` on its FIRST call and a success on every subsequent call
/// (independent per-surface counters). `on_auth_failure` always
/// succeeds and bumps its own counter.
struct Recovering401MultiProvider {
    id: String,
    complete_calls: Arc<AtomicUsize>,
    stream_calls: Arc<AtomicUsize>,
    count_tokens_calls: Arc<AtomicUsize>,
    on_auth_failure_calls: Arc<AtomicUsize>,
    /// When true, `on_auth_failure` returns `Error::Auth` instead of
    /// `Ok(())` -- the dead-OAuth-identity path.
    refresh_fails: bool,
}

#[async_trait]
impl Provider for Recovering401MultiProvider {
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
        let n = self.complete_calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            return Err(Error::upstream(&self.id, 401, "stale token"));
        }
        Ok(ChatResponse {
            id: format!("ok-{}", self.id),
            model: req.model,
            created: 0,
            choices: vec![routectl_core::Choice {
                logprobs: None,
                index: 0,
                message: routectl_core::Message {
                    refusal: None,
                    role: routectl_core::Role::Assistant,
                    content: routectl_core::MessageContent::Text("ok".into()),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
                finish_reason: Some("stop".into()),
                matched_stop_sequence: None,
            }],
            usage: Some(routectl_core::Usage::default()),
            routectl_provider: None,
            extras: Default::default(),
            upstream_meta: None,
        })
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        let n = self.stream_calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            return Err(Error::upstream(&self.id, 401, "stale token"));
        }
        let chunk = content_chunk(&format!("ok-{}", self.id));
        Ok(futures::stream::once(async move { Ok(chunk) }).boxed())
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        let n = self.count_tokens_calls.fetch_add(1, Ordering::SeqCst);
        if n == 0 {
            return Err(Error::upstream(&self.id, 401, "stale token"));
        }
        Ok(TokenCount {
            input_tokens: 7,
            ..Default::default()
        })
    }
    async fn on_auth_failure(&self) -> Result<()> {
        self.on_auth_failure_calls.fetch_add(1, Ordering::SeqCst);
        if self.refresh_fails {
            Err(Error::Auth("oauth refresh failed; re-run login".into()))
        } else {
            Ok(())
        }
    }
}

fn build_recovering_router() -> (Router, Arc<Recovering401MultiProvider>) {
    build_recovering_router_inner(false)
}

fn build_recovering_router_inner(refresh_fails: bool) -> (Router, Arc<Recovering401MultiProvider>) {
    let provider = Arc::new(Recovering401MultiProvider {
        id: "p".into(),
        complete_calls: Arc::new(AtomicUsize::new(0)),
        stream_calls: Arc::new(AtomicUsize::new(0)),
        count_tokens_calls: Arc::new(AtomicUsize::new(0)),
        on_auth_failure_calls: Arc::new(AtomicUsize::new(0)),
        refresh_fails,
    });
    let router = build_router_with_provider(provider.clone() as Arc<dyn Provider>);
    (router, provider)
}

/// A plain (non-max_tokens-probe) request so `is_probe` is false and
/// the only "probe" in play is the breaker's half-open probe slot.
fn plain_req() -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: vec![].into(),
        ..Default::default()
    }
}

/// Trip the threshold-1 breaker directly so the next dispatch is
/// half-open (zero cooldown).
fn trip_breaker(router: &Router) {
    let st = router.state.get("m").expect("per-model state slot exists");
    st.lock()
        .record_failure(Instant::now(), LastOutcome::Http5xx);
}

fn slot_in_flight(router: &Router) -> bool {
    let st = router.state.get("m").expect("per-model state slot exists");
    st.lock().half_open_probe_in_flight()
}

#[tokio::test]
async fn complete_half_open_401_refresh_releases_slot() {
    // FAILS without the slot-release fix: the half-open probe 401s,
    // the refresh succeeds, and the Ok-path `continue` re-gates while
    // this caller still holds the slot -> CircuitOpen -> single-entry
    // chain exhausts -> Err with the slot stuck `true` forever. With
    // the fix the slot is released first, the re-gate claims a fresh
    // slot, the retry reaches the upstream and succeeds, and the probe
    // success closes the breaker.
    let (router, provider) = build_recovering_router();
    trip_breaker(&router);

    let resp = router
        .complete(plain_req())
        .await
        .expect("half-open 401 -> refresh -> retry must land on the success branch");
    assert_eq!(resp.routectl_provider.as_deref(), Some("p"));
    assert_eq!(
        provider.complete_calls.load(Ordering::SeqCst),
        2,
        "complete must run twice: the 401 probe and the post-refresh retry",
    );
    assert_eq!(
        provider.on_auth_failure_calls.load(Ordering::SeqCst),
        1,
        "on_auth_failure fires exactly once (the single 401 -> refresh)",
    );
    assert!(
        !slot_in_flight(&router),
        "half-open slot must be cleared after the recovered probe",
    );
}

#[tokio::test]
async fn count_tokens_half_open_401_refresh_does_not_lock_breaker() {
    // FAILS without the fix: count_tokens propagates the re-gate's
    // CircuitOpen as its gate error and the slot stays `true` forever,
    // so neither this dispatch nor any later one reaches the upstream.
    let (router, provider) = build_recovering_router();
    trip_breaker(&router);

    let first = router.count_tokens(plain_req()).await;
    let calls_after_first = provider.count_tokens_calls.load(Ordering::SeqCst);

    // A leaked half-open slot would have locked the breaker; the
    // second dispatch must still reach the upstream.
    let second = router.count_tokens(plain_req()).await;
    let calls_after_second = provider.count_tokens_calls.load(Ordering::SeqCst);

    assert!(
        first.is_ok(),
        "first count_tokens must recover via refresh+retry, got: {first:?}",
    );
    assert!(
        second.is_ok(),
        "second count_tokens must not hit a permanently-locked breaker, got: {second:?}",
    );
    assert!(
        calls_after_second > calls_after_first,
        "second dispatch must reach the upstream; a leaked slot locks the \
             breaker (CircuitOpen) and skips it: {calls_after_first} -> {calls_after_second}",
    );
    assert_eq!(
        provider.on_auth_failure_calls.load(Ordering::SeqCst),
        1,
        "on_auth_failure fires exactly once (the single 401 -> refresh)",
    );
    assert!(
        !slot_in_flight(&router),
        "half-open slot must be released, not stuck open",
    );
}

#[tokio::test]
async fn stream_half_open_401_refresh_does_not_lock_breaker() {
    // FAILS without the fix: provider.stream() 401s pre-first-chunk,
    // the refresh succeeds, the Ok-path `continue` re-gates while this
    // caller still holds the slot -> CircuitOpen -> single-entry chain
    // exhausts -> Err with the slot stuck `true` forever.
    let (router, provider) = build_recovering_router();
    trip_breaker(&router);

    let first = router.stream(plain_req()).await;
    let first_is_ok = first.is_ok();
    // Drain the recovered stream to completion so the half-open probe's
    // breaker accounting records success and closes the breaker. (When
    // the fix is absent `first` is the CircuitOpen Err -- nothing to
    // drain.)
    if let Ok(mut s) = first {
        while s.next().await.is_some() {}
    }
    let calls_after_first = provider.stream_calls.load(Ordering::SeqCst);

    let second = router.stream(plain_req()).await;
    let second_is_ok = second.is_ok();
    if let Ok(mut s) = second {
        while s.next().await.is_some() {}
    }
    let calls_after_second = provider.stream_calls.load(Ordering::SeqCst);

    assert!(
        first_is_ok,
        "first stream must recover via refresh+retry, not fail with CircuitOpen",
    );
    assert!(
        second_is_ok,
        "second stream must not hit a permanently-locked breaker",
    );
    assert!(
        calls_after_second > calls_after_first,
        "second dispatch must reach the upstream; a leaked slot locks the \
             breaker (CircuitOpen) and skips it: {calls_after_first} -> {calls_after_second}",
    );
    assert_eq!(
        provider.on_auth_failure_calls.load(Ordering::SeqCst),
        1,
        "on_auth_failure fires exactly once (the single 401 -> refresh)",
    );
    assert!(
        !slot_in_flight(&router),
        "half-open slot must be released, not stuck open",
    );
}

#[tokio::test]
async fn complete_half_open_non_fallbackable_429_does_not_lock_breaker() {
    // Regression: a NON-probe request hits a half-open provider that
    // returns 429 under a policy that excludes 429 from fallback
    // (`[retry.classes.rate-limited] fallback = false, retry = 0`).
    // do_fallback is false and the 429 is also non-retryable, so the
    // dispatch surfaces verbatim. Under class-based debit the excluded
    // 429 DOES debit (RateLimited is a health class -- accounting is
    // decoupled from routing), but the half-open slot must still be
    // settled exactly once so the breaker is not left locked open.
    // With a zero cooldown the re-trip is immediately half-open-eligible,
    // so the second dispatch must still reach the upstream.
    use crate::class_policy::{ClassPolicy, ConfigFailureClass};
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(Probe429Provider {
        id: "p".into(),
        calls: calls.clone(),
    });
    // rate-limited class fallback=false + retry=0: do_fallback=false AND
    // the 429 is non-retryable, so the attempt is neither retried nor
    // fallen back -- it hits the terminal non-fallbackable release. Zero
    // backoff/jitter keep the test instant.
    let mut classes = std::collections::BTreeMap::new();
    classes.insert(
        ConfigFailureClass::RateLimited,
        ClassPolicy {
            retry: Some(0),
            fallback: Some(false),
        },
    );
    let retry = RetryPolicy {
        max_attempts: 1,
        initial_backoff_ms: 0,
        backoff_multiplier: 1.0,
        jitter_ms: 0,
        classes,
        ..RetryPolicy::default()
    };
    let router = build_router_with_provider_and_retry(provider, retry);
    trip_breaker(&router);

    // Non-probe: max_tokens above the probe threshold (default 1), so
    // is_probe=false and the probe-fast-fail path is not taken.
    let req = ChatRequest {
        model: "m".into(),
        messages: vec![].into(),
        max_tokens: Some(1024),
        ..Default::default()
    };

    let first = router.complete(req.clone()).await;
    let calls_after_first = calls.load(Ordering::SeqCst);

    // A leaked half-open slot would have locked the breaker; the second
    // dispatch must still reach the upstream.
    let second = router.complete(req.clone()).await;
    let calls_after_second = calls.load(Ordering::SeqCst);

    assert!(
        calls_after_second > calls_after_first,
        "second dispatch must reach the upstream; a leaked slot locks the \
             breaker (CircuitOpen) and skips it: {calls_after_first} -> {calls_after_second}",
    );
    // Both dispatches must terminate in the upstream 429, never the
    // gate's status-0 "circuit breaker open" error.
    for (label, r) in [("first", &first), ("second", &second)] {
        match r {
            Err(Error::Upstream { status, .. }) => assert_eq!(
                *status, 429,
                "{label} dispatch must surface the upstream 429, not the \
                     gate circuit-breaker error (status 0)",
            ),
            other => panic!("{label} dispatch expected Err(Upstream 429), got: {other:?}"),
        }
    }
    assert!(
        !slot_in_flight(&router),
        "half-open slot must be released after the retry-without-fallback path",
    );
}

#[tokio::test]
async fn complete_half_open_401_refresh_failure_releases_slot() {
    // Coverage for the auth-refresh-FAILURE release path: a half-open
    // probe gets a 401, `on_auth_failure()` returns Err (dead OAuth
    // identity), and the router must release the half-open slot before
    // surfacing the error. If it did not, the breaker would be locked
    // open forever; here we assert the slot is freed and a later
    // dispatch can still probe.
    let (router, provider) = build_recovering_router_inner(true);
    trip_breaker(&router);

    let first = router.complete(plain_req()).await;
    let calls_after_first = provider.complete_calls.load(Ordering::SeqCst);
    match &first {
        Err(Error::Auth(msg)) => assert!(
            msg.contains("oauth refresh failed"),
            "expected the refresh-failure auth error, got: {msg}",
        ),
        other => panic!("expected Err(Auth), got: {other:?}"),
    }
    assert!(
        !slot_in_flight(&router),
        "half-open slot must be released when on_auth_failure errors",
    );
    assert_eq!(
        provider.on_auth_failure_calls.load(Ordering::SeqCst),
        1,
        "on_auth_failure fires exactly once before the error propagates",
    );

    // Breaker is NOT locked: a second dispatch (zero cooldown) still
    // claims a fresh probe and reaches the upstream.
    let _ = router.complete(plain_req()).await;
    let calls_after_second = provider.complete_calls.load(Ordering::SeqCst);
    assert!(
        calls_after_second > calls_after_first,
        "second dispatch must reach the upstream; a leaked slot would lock \
             the breaker (CircuitOpen) and skip it: {calls_after_first} -> {calls_after_second}",
    );
}

/// A non-probe dispatch whose upstream returns a LARGE reset hint
/// (> INLOOP_RETRY_AFTER_CAP) parks the provider via `force_open` for
/// the honored duration, rather than blocking the request thread.
/// The failure threshold is high (5) so the ONLY way the breaker can
/// be open afterward is the force-park, not a counter-driven trip.
#[tokio::test]
async fn large_retry_after_parks_provider_via_force_open() {
    // Arrange: 60s reset, well above the 5s in-loop cap, on a 429.
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
        id: "p".into(),
        status: 429,
        retry_after: Some(Duration::from_mins(1)),
        calls: calls.clone(),
    });
    // High threshold + a non-zero default cooldown (1s) so a stray
    // record_failure would open for only ~1s, distinguishable from a
    // 60s force-park.
    let router = build_router_with_breaker(provider, RetryPolicy::default(), 5, 1_000);
    let t0 = Instant::now();

    // Act.
    let _ = router.complete(plain_req()).await.unwrap_err();

    // Assert: open now, still open at +59s (a 1s record_failure trip
    // would already have elapsed), allowed only after the 60s park.
    assert!(
        breaker_open_at(&router, t0),
        "large reset must park the provider open immediately",
    );
    assert!(
        breaker_open_at(&router, t0 + Duration::from_secs(59)),
        "park must outlast the default cooldown -- proving force_open, not a record_failure trip",
    );
    assert!(
        !breaker_open_at(&router, t0 + Duration::from_secs(61)),
        "park must release once the honored 60s reset elapses",
    );
}

/// A SMALL reset (<= INLOOP_RETRY_AFTER_CAP) on a retryable error is
/// honored as an in-loop sleep, NOT a force-park: the same provider is
/// retried (call count rises to the retry cap) and a high failure
/// threshold leaves the breaker closed (no force_open).
#[tokio::test]
async fn small_retry_after_honored_in_loop_not_parked() {
    // Arrange: 1ms reset (tiny, keeps the in-loop sleep negligible),
    // 429 -> retryable (default max_attempts = 2). Threshold 5 so a
    // single recorded failure cannot trip the breaker.
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
        id: "p".into(),
        status: 429,
        retry_after: Some(Duration::from_millis(1)),
        calls: calls.clone(),
    });
    let router = build_router_with_breaker(provider, RetryPolicy::default(), 5, 1_000);
    let t0 = Instant::now();

    // Act.
    let _ = router.complete(plain_req()).await.unwrap_err();

    // Assert: the same provider was retried in-loop (2 = max_attempts),
    // and the breaker was NOT force-parked (closed under the high
    // threshold). A force-park would have opened it after the first
    // attempt and skipped the second.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "a small reset must be honored as an in-loop retry, not a park",
    );
    assert!(
        !breaker_open_at(&router, t0),
        "a small reset must NOT force-park the provider (breaker stays closed under a high threshold)",
    );
}

/// A reset far larger than `max_honored_retry_after` parks for the
/// CEILING, not the raw value: open before the ceiling, allowed after.
#[tokio::test]
async fn retry_after_clamped_to_ceiling() {
    // Arrange: a 1-hour raw reset, a 10s ceiling.
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
        id: "p".into(),
        status: 429,
        retry_after: Some(Duration::from_hours(1)),
        calls: calls.clone(),
    });
    let retry = RetryPolicy {
        max_honored_retry_after_ms: Some(10_000),
        ..RetryPolicy::default()
    };
    let router = build_router_with_breaker(provider, retry, 5, 1_000);
    let t0 = Instant::now();

    // Act.
    let _ = router.complete(plain_req()).await.unwrap_err();

    // Assert: parked for the 10s ceiling, NOT the raw 1h. Still open at
    // +9s; released at +11s (the raw 1h value would still be open).
    assert!(
        breaker_open_at(&router, t0 + Duration::from_secs(9)),
        "park must hold until the ceiling elapses",
    );
    assert!(
        !breaker_open_at(&router, t0 + Duration::from_secs(11)),
        "park must release at the ceiling, not the raw 1h reset",
    );
}

/// A probe (max_tokens <= probe_max_tokens) that 429s with a reset hint
/// fast-fails: NO retry, NO fallback, NO breaker debit, NO park.
#[tokio::test]
async fn probe_with_retry_after_does_not_park() {
    // Arrange: a probe-shaped request, a large reset that would park a
    // non-probe. Threshold 5 so any stray debit/park is observable.
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
        id: "p".into(),
        status: 429,
        retry_after: Some(Duration::from_mins(1)),
        calls: calls.clone(),
    });
    let router = build_router_with_breaker(provider, RetryPolicy::default(), 5, 1_000);
    let t0 = Instant::now();

    // Act: probe_req has max_tokens = 1 <= probe_max_tokens (default 1).
    let _ = router.complete(probe_req()).await.unwrap_err();

    // Assert: fast-fail -- exactly one upstream call (no retry), and the
    // breaker was neither parked nor debited (slot released cleanly).
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a probe must fast-fail on 429 with no retry",
    );
    assert!(
        !breaker_open_at(&router, t0),
        "a probe reset must NOT park the provider",
    );
}

/// A reset on a NON-fallbackable error (a 400 whose class is pinned
/// non-fallbackable) does not force a retry or a park: the error
/// terminates exactly as today (the reset never changes a
/// fallback/retry decision).
#[tokio::test]
async fn non_fallbackable_error_with_retry_after_still_terminates() {
    // Arrange: a 400 (client error) that is NOT fallbackable, carrying
    // a large reset hint. Threshold 5 so any stray park is observable.
    use crate::class_policy::{ClassPolicy, ConfigFailureClass};
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
        id: "p".into(),
        status: 400,
        retry_after: Some(Duration::from_mins(1)),
        calls: calls.clone(),
    });
    let mut classes = std::collections::BTreeMap::new();
    classes.insert(
        ConfigFailureClass::BadRequest,
        ClassPolicy {
            retry: Some(0),
            fallback: Some(false),
        },
    );
    let retry = RetryPolicy {
        classes,
        ..RetryPolicy::default()
    };
    let router = build_router_with_breaker(provider, retry, 5, 1_000);
    let t0 = Instant::now();

    // Act.
    let result = router.complete(plain_req()).await;

    // Assert: terminated with the 400 (no retry walk), exactly one
    // upstream call, and the breaker was not parked.
    match result {
        Err(Error::Upstream { status: 400, .. }) => {}
        other => panic!("expected terminal Err(Upstream 400), got: {other:?}"),
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "a non-fallbackable error must not be retried despite a reset hint",
    );
    assert!(
        !breaker_open_at(&router, t0),
        "a non-fallbackable error must not park the provider",
    );
}

/// A SMALL non-probe reset actually LENGTHENS the in-loop retry sleep
/// (the backoff-bump path), not merely "does not park". With a 1ms
/// baseline backoff, the 300ms hint must dominate the inter-attempt
/// wait -- proving the bump took effect (without it the retry would
/// fire almost immediately off the 1ms baseline).
#[tokio::test]
async fn small_retry_after_lengthens_inloop_sleep() {
    // Arrange: 300ms reset (<= the 5s in-loop cap), 429 -> retryable,
    // baseline backoff 1ms so the bump (not the baseline) drives the
    // wait. Threshold 5 so the breaker is not parked.
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
        id: "p".into(),
        status: 429,
        retry_after: Some(Duration::from_millis(300)),
        calls: calls.clone(),
    });
    let retry = RetryPolicy {
        max_attempts: 2,
        initial_backoff_ms: 1,
        ..RetryPolicy::default()
    };
    let router = build_router_with_breaker(provider, retry, 5, 1_000);

    // Act: time the whole two-attempt dispatch.
    let start = Instant::now();
    let _ = router.complete(plain_req()).await.unwrap_err();
    let elapsed = start.elapsed();

    // Assert: retried once (2 calls), and the inter-attempt sleep was
    // lengthened to honor the 300ms reset, far above the 1ms baseline.
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "a small reset on a retryable error must still retry the same provider",
    );
    assert!(
        elapsed >= Duration::from_millis(250),
        "the in-loop sleep must be lengthened to ~the 300ms reset (got {elapsed:?}); \
             without the bump the 1ms baseline would fire the retry almost immediately",
    );
}

// ---- MEE: cancellation-safety of the half-open probe slot ----
//
// A half-open probe claims the single probe slot at the gate BEFORE the
// dispatch awaits the upstream. If that future is DROPPED while awaiting a
// hung upstream (client disconnect / client-side timeout), none of the
// synchronous settle arms run; without `ProbeSlotGuard` the slot stays
// claimed forever and every later probe sees CircuitOpen -- a permanent
// latch until restart. These tests drop the dispatch future mid-await and
// assert the slot is freed and the breaker recovers.

/// Multi-surface provider that hangs (long sleep) on every surface while
/// `hang` is set, then succeeds once it is cleared. Per-surface call
/// counters record that a dispatch reached the (hung) upstream.
struct HangUntilClearedProvider {
    id: String,
    hang: Arc<std::sync::atomic::AtomicBool>,
    complete_calls: Arc<AtomicUsize>,
    stream_calls: Arc<AtomicUsize>,
    count_tokens_calls: Arc<AtomicUsize>,
}

impl HangUntilClearedProvider {
    fn new(id: &str, hang: Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self {
            id: id.into(),
            hang,
            complete_calls: Arc::new(AtomicUsize::new(0)),
            stream_calls: Arc::new(AtomicUsize::new(0)),
            count_tokens_calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    async fn maybe_hang(&self) {
        if self.hang.load(Ordering::SeqCst) {
            // Far longer than any test timeout: the dispatch future is
            // dropped while parked here, exercising the cancellation path.
            tokio::time::sleep(Duration::from_hours(1)).await;
        }
    }
}

#[async_trait]
impl Provider for HangUntilClearedProvider {
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
        self.complete_calls.fetch_add(1, Ordering::SeqCst);
        self.maybe_hang().await;
        Ok(ChatResponse {
            id: format!("ok-{}", self.id),
            model: req.model,
            created: 0,
            choices: vec![routectl_core::Choice {
                logprobs: None,
                index: 0,
                message: routectl_core::Message {
                    refusal: None,
                    role: routectl_core::Role::Assistant,
                    content: routectl_core::MessageContent::Text("ok".into()),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                },
                finish_reason: Some("stop".into()),
                matched_stop_sequence: None,
            }],
            usage: Some(routectl_core::Usage::default()),
            routectl_provider: None,
            extras: Default::default(),
            upstream_meta: None,
        })
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        self.maybe_hang().await;
        let chunk = content_chunk(&format!("ok-{}", self.id));
        Ok(futures::stream::once(async move { Ok(chunk) }).boxed())
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        self.count_tokens_calls.fetch_add(1, Ordering::SeqCst);
        self.maybe_hang().await;
        Ok(TokenCount {
            input_tokens: 7,
            ..Default::default()
        })
    }
}

/// Multi-surface provider that always fails with a status-0 transport
/// error ("never reached the upstream HTTP layer") on every surface, with
/// per-surface call counters.
struct Status0Provider {
    id: String,
    complete_calls: Arc<AtomicUsize>,
    stream_calls: Arc<AtomicUsize>,
    count_tokens_calls: Arc<AtomicUsize>,
}

impl Status0Provider {
    fn new(id: &str) -> Self {
        Self {
            id: id.into(),
            complete_calls: Arc::new(AtomicUsize::new(0)),
            stream_calls: Arc::new(AtomicUsize::new(0)),
            count_tokens_calls: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[async_trait]
impl Provider for Status0Provider {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(&self.id, "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        self.complete_calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream(&self.id, 0, "error sending request"))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream(&self.id, 0, "error sending request"))
    }
    async fn count_tokens(&self, _: ChatRequest) -> Result<TokenCount> {
        self.count_tokens_calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream(&self.id, 0, "error sending request"))
    }
}

/// CircuitPhase the breaker reads at `now` WITHOUT mutating it (unlike
/// `breaker_open_at`, which claims a probe slot via `try_dispatch`).
fn circuit_phase(router: &Router) -> crate::runtime_state::CircuitPhase {
    router
        .capacity_snapshot_for("m", Instant::now())
        .expect("per-model state slot exists")
        .circuit
}

#[tokio::test]
async fn complete_half_open_cancelled_probe_releases_slot() {
    let hang = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let provider = Arc::new(HangUntilClearedProvider::new("p", hang.clone()));
    let router = build_router_with_provider_and_retry(
        provider.clone() as Arc<dyn Provider>,
        RetryPolicy::default(),
    );
    arm_half_open(&router);

    // The half-open probe reaches the hung upstream and stalls; drop the
    // dispatch future mid-await via a short timeout.
    let cancelled =
        tokio::time::timeout(Duration::from_millis(20), router.complete(plain_req())).await;
    assert!(
        cancelled.is_err(),
        "the probe must still be awaiting the hung upstream when the timeout fires",
    );
    assert_eq!(
        provider.complete_calls.load(Ordering::SeqCst),
        1,
        "the probe must have reached the (hung) upstream",
    );
    // Before the guard, the dropped future skipped every settle arm and the
    // half-open slot stayed `true` forever -> permanent CircuitOpen latch.
    assert!(
        !slot_in_flight(&router),
        "a cancelled half-open probe must release the slot",
    );

    // Recovery: clear the hang; the next dispatch is admitted as a fresh
    // probe (a leaked slot would have latched CircuitOpen and skipped it).
    hang.store(false, Ordering::SeqCst);
    let recovered = router.complete(plain_req()).await;
    assert!(
        recovered.is_ok(),
        "breaker must recover: next dispatch admitted + succeeds, got {recovered:?}",
    );
    assert_eq!(
        provider.complete_calls.load(Ordering::SeqCst),
        2,
        "the recovery dispatch must reach the upstream, not a latched breaker",
    );
}

#[tokio::test]
async fn count_tokens_half_open_cancelled_probe_releases_slot() {
    let hang = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let provider = Arc::new(HangUntilClearedProvider::new("p", hang.clone()));
    let router = build_router_with_provider_and_retry(
        provider.clone() as Arc<dyn Provider>,
        RetryPolicy::default(),
    );
    arm_half_open(&router);

    let cancelled =
        tokio::time::timeout(Duration::from_millis(20), router.count_tokens(plain_req())).await;
    assert!(
        cancelled.is_err(),
        "the count_tokens probe must still be awaiting the hung upstream",
    );
    assert_eq!(provider.count_tokens_calls.load(Ordering::SeqCst), 1);
    assert!(
        !slot_in_flight(&router),
        "a cancelled count_tokens probe must release the slot",
    );

    hang.store(false, Ordering::SeqCst);
    let recovered = router.count_tokens(plain_req()).await;
    assert!(
        recovered.is_ok(),
        "count_tokens must recover after a cancelled probe, got {recovered:?}",
    );
    assert_eq!(
        provider.count_tokens_calls.load(Ordering::SeqCst),
        2,
        "the recovery count_tokens must reach the upstream",
    );
}

#[tokio::test]
async fn stream_half_open_cancelled_probe_releases_slot() {
    let hang = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let provider = Arc::new(HangUntilClearedProvider::new("p", hang.clone()));
    let router = build_router_with_provider_and_retry(
        provider.clone() as Arc<dyn Provider>,
        RetryPolicy::default(),
    );
    arm_half_open(&router);

    let cancelled =
        tokio::time::timeout(Duration::from_millis(20), router.stream(plain_req())).await;
    assert!(
        cancelled.is_err(),
        "the stream probe must still be awaiting the hung upstream",
    );
    assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);
    assert!(
        !slot_in_flight(&router),
        "a cancelled stream probe must release the slot",
    );

    hang.store(false, Ordering::SeqCst);
    let recovered = router.stream(plain_req()).await;
    assert!(
        recovered.is_ok(),
        "stream must recover after a cancelled probe, got {:?}",
        recovered.as_ref().err(),
    );
    assert_eq!(
        provider.stream_calls.load(Ordering::SeqCst),
        2,
        "the recovery stream must reach the upstream",
    );
}

#[tokio::test]
async fn complete_half_open_status0_retrips_and_recovers() {
    let provider = Arc::new(Status0Provider::new("p"));
    let router = build_router_with_provider_and_retry(
        provider.clone() as Arc<dyn Provider>,
        RetryPolicy::default(),
    );
    arm_half_open(&router);

    let r1 = router.complete(plain_req()).await;
    assert!(r1.is_err(), "status-0 probe surfaces an error");
    let calls_after_first = provider.complete_calls.load(Ordering::SeqCst);
    assert!(
        calls_after_first >= 1,
        "the probe (and any same-provider retries) reached the upstream",
    );
    assert!(
        !slot_in_flight(&router),
        "a status-0 half-open probe must release the slot (record_failure clears it)",
    );
    // Re-tripped (circuit_opened_at set) yet half-open-ready (slot free,
    // baseline cooldown elapsed) -- recovered, NOT latched Open.
    assert_eq!(
        circuit_phase(&router),
        crate::runtime_state::CircuitPhase::HalfOpenReady,
        "status-0 probe must re-trip cleanly and leave the breaker recoverable",
    );

    // A fresh probe is admitted and reaches the upstream again.
    let _ = router.complete(plain_req()).await;
    assert!(
        provider.complete_calls.load(Ordering::SeqCst) > calls_after_first,
        "the post-cooldown probe must reach the upstream, not a latched breaker",
    );
}

#[tokio::test]
async fn count_tokens_half_open_status0_retrips_and_recovers() {
    let provider = Arc::new(Status0Provider::new("p"));
    let router = build_router_with_provider_and_retry(
        provider.clone() as Arc<dyn Provider>,
        RetryPolicy::default(),
    );
    arm_half_open(&router);

    let r1 = router.count_tokens(plain_req()).await;
    assert!(r1.is_err());
    assert_eq!(provider.count_tokens_calls.load(Ordering::SeqCst), 1);
    assert!(
        !slot_in_flight(&router),
        "a status-0 count_tokens probe must release the slot",
    );
    assert_eq!(
        circuit_phase(&router),
        crate::runtime_state::CircuitPhase::HalfOpenReady,
        "status-0 count_tokens probe must re-trip cleanly and stay recoverable",
    );

    let _ = router.count_tokens(plain_req()).await;
    assert_eq!(
        provider.count_tokens_calls.load(Ordering::SeqCst),
        2,
        "the post-cooldown count_tokens probe must reach the upstream",
    );
}

#[tokio::test]
async fn stream_half_open_status0_retrips_and_recovers() {
    let provider = Arc::new(Status0Provider::new("p"));
    let router = build_router_with_provider_and_retry(
        provider.clone() as Arc<dyn Provider>,
        RetryPolicy::default(),
    );
    arm_half_open(&router);

    let r1 = router.stream(plain_req()).await;
    assert!(r1.is_err());
    assert_eq!(provider.stream_calls.load(Ordering::SeqCst), 1);
    assert!(
        !slot_in_flight(&router),
        "a status-0 stream probe must release the slot",
    );
    assert_eq!(
        circuit_phase(&router),
        crate::runtime_state::CircuitPhase::HalfOpenReady,
        "status-0 stream probe must re-trip cleanly and stay recoverable",
    );

    let _ = router.stream(plain_req()).await;
    assert_eq!(
        provider.stream_calls.load(Ordering::SeqCst),
        2,
        "the post-cooldown stream probe must reach the upstream",
    );
}

// ---- Class-based breaker debit (accounting decoupled from routing) ----
//
// The debit keys off the failure CLASS, not the fallback decision:
// only the transient-health set (RateLimited, ServerError, Timeout,
// NetworkError, Overloaded) debits the per-seat breaker. Caller-shaped
// 4xx, auth, and capability faults fall back but never debit.

/// A `[retry]` policy with no same-provider retry and no backoff, so
/// one dispatch equals exactly one outcome (the debit count is not
/// inflated by in-loop retries and the tests run instantly).
fn no_retry_policy() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 1,
        initial_backoff_ms: 0,
        backoff_multiplier: 1.0,
        jitter_ms: 0,
        ..RetryPolicy::default()
    }
}

/// Provider whose `complete` always fails with a canonical
/// `Error::Streaming` (classifies `NetworkError`, a health class).
struct AlwaysStreamingErrProvider {
    id: String,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for AlwaysStreamingErrProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(&self.id, "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::Streaming("wire reset before first chunk".into()))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("not exercised by these tests")
    }
}

/// Provider whose `stream` always fails pre-first-chunk with a
/// configurable upstream status, so the stream dispatch loop's error
/// arm (not the mid-stream wrap) decides the breaker debit.
struct PreChunkStatusErrProvider {
    id: String,
    status: u16,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl Provider for PreChunkStatusErrProvider {
    fn id(&self) -> &str {
        &self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(&self.id, "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!("not exercised by these tests")
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Err(Error::upstream(
            &self.id,
            self.status,
            "pre-first-chunk failure",
        ))
    }
}

#[tokio::test]
async fn non_retryable_4xx_storm_leaves_breaker_closed() {
    // The intended feature delta on the completion path. A caller-shaped
    // 4xx is not upstream health, so a storm of them must never trip the
    // per-seat breaker. Before the class rewire a fallbackable 4xx
    // debited (do_fallback was true), so threshold+ consecutive 4xx
    // would trip; now class_debits is false across the whole 4xx
    // caller-error row, so the breaker stays closed and the alias stays
    // in rotation.
    for status in [400u16, 404, 422] {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
            id: "p".into(),
            status,
            retry_after: None,
            calls: calls.clone(),
        });
        // Threshold 2, four consecutive 4xx: a health-debiting error
        // would trip twice over.
        let router = build_router_with_breaker(provider, no_retry_policy(), 2, 60_000);

        for _ in 0..4 {
            let _ = router.complete(plain_req()).await.unwrap_err();
        }

        assert_eq!(
            circuit_phase(&router),
            crate::runtime_state::CircuitPhase::Closed,
            "status {status}: a non-retryable 4xx storm must leave the breaker CLOSED",
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            4,
            "status {status}: every dispatch must reach the upstream \
                 (alias stays in rotation, never gated by a tripped breaker)",
        );
    }
}

#[tokio::test]
async fn status_health_errors_still_trip_breaker_after_threshold() {
    // The complementary pin: the transient-health status row still
    // debits and trips at threshold, exactly as before the rewire.
    for status in [429u16, 503, 500, 0] {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
            id: "p".into(),
            status,
            retry_after: None,
            calls: calls.clone(),
        });
        let router = build_router_with_breaker(provider, no_retry_policy(), 2, 60_000);

        // First health failure is sub-threshold: still closed.
        let _ = router.complete(plain_req()).await.unwrap_err();
        assert_eq!(
            circuit_phase(&router),
            crate::runtime_state::CircuitPhase::Closed,
            "status {status}: one health failure is below threshold 2",
        );
        // Second reaches the threshold: the breaker trips open.
        let _ = router.complete(plain_req()).await.unwrap_err();
        assert!(
            breaker_open_at(&router, Instant::now()),
            "status {status}: a health-error storm must trip the breaker at threshold",
        );
    }
}

#[tokio::test]
async fn streaming_transport_error_still_debits_breaker() {
    // Error::Streaming classifies NetworkError (a health class), so it
    // must debit like a status-0 transport failure.
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(AlwaysStreamingErrProvider {
        id: "p".into(),
        calls: calls.clone(),
    });
    let router = build_router_with_breaker(provider, no_retry_policy(), 1, 60_000);

    let _ = router.complete(plain_req()).await.unwrap_err();

    assert!(
        breaker_open_at(&router, Instant::now()),
        "a Streaming transport error (NetworkError class) must debit and trip the breaker",
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn completion_501_debits_breaker() {
    // Contrast with the count_tokens capability walk: on the COMPLETION
    // path a wire-501 is a ServerError (health), not a capability
    // signal, so it debits and trips the breaker. Only count_tokens
    // treats a 501 from a capable-by-kind seat as a capability signal.
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
        id: "p".into(),
        status: 501,
        retry_after: None,
        calls: calls.clone(),
    });
    let router = build_router_with_breaker(provider, no_retry_policy(), 1, 60_000);

    let _ = router.complete(plain_req()).await.unwrap_err();

    assert!(
        breaker_open_at(&router, Instant::now()),
        "a completion-path 501 (ServerError class) must debit and trip the breaker",
    );
}

#[tokio::test]
async fn non_fallbackable_429_still_debits_breaker() {
    // Intended delta: health accounting is decoupled from routing. An
    // operator pinning the rate-limited class non-fallbackable
    // (`[retry.classes.rate-limited] fallback = false`) makes
    // do_fallback false -- before the rewire that suppressed the debit.
    // Now the debit keys off the RateLimited class, not the fallback
    // decision, so a non-fallbackable 429 STILL debits the breaker while
    // surfacing verbatim (no fallback, no retry).
    use crate::class_policy::{ClassPolicy, ConfigFailureClass};
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(RetryAfterProvider {
        id: "p".into(),
        status: 429,
        retry_after: None,
        calls: calls.clone(),
    });
    let mut classes = std::collections::BTreeMap::new();
    classes.insert(
        ConfigFailureClass::RateLimited,
        ClassPolicy {
            retry: Some(0),
            fallback: Some(false),
        },
    );
    let retry = RetryPolicy {
        classes,
        ..no_retry_policy()
    };
    let router = build_router_with_breaker(provider, retry, 1, 60_000);

    let err = router.complete(plain_req()).await.unwrap_err();

    assert!(
        matches!(err, Error::Upstream { status: 429, .. }),
        "a non-fallbackable 429 must surface verbatim (no fallback); got {err:?}",
    );
    assert!(
        breaker_open_at(&router, Instant::now()),
        "a non-fallbackable 429 must STILL debit the breaker (accounting decoupled from routing)",
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "non-fallbackable 429 is terminal: one upstream call, no retry, no fallback",
    );
}

#[tokio::test]
async fn stream_non_retryable_4xx_leaves_breaker_closed() {
    // The intended delta on the STREAM dispatch loop: a pre-first-chunk
    // 4xx falls back but must not debit. Exercises the stream error
    // arm's class-gated debit and the debit-skipped fallback release.
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(PreChunkStatusErrProvider {
        id: "p".into(),
        status: 400,
        calls: calls.clone(),
    });
    let router = build_router_with_breaker(provider, no_retry_policy(), 2, 60_000);

    for _ in 0..4 {
        router
            .stream(plain_req())
            .await
            .err()
            .expect("a pre-first-chunk 4xx must error");
    }

    assert_eq!(
        circuit_phase(&router),
        crate::runtime_state::CircuitPhase::Closed,
        "a pre-first-chunk 4xx storm must leave the stream-path breaker CLOSED",
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        4,
        "every stream dispatch must reach the upstream (alias stays in rotation)",
    );
}

#[tokio::test]
async fn stream_health_error_still_debits_breaker() {
    // Complement to the 4xx case: a pre-first-chunk 5xx is a health
    // failure and must still debit + trip the stream-path breaker.
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(PreChunkStatusErrProvider {
        id: "p".into(),
        status: 503,
        calls: calls.clone(),
    });
    let router = build_router_with_breaker(provider, no_retry_policy(), 1, 60_000);

    router
        .stream(plain_req())
        .await
        .err()
        .expect("a pre-first-chunk 5xx must error");

    assert!(
        breaker_open_at(&router, Instant::now()),
        "a pre-first-chunk 5xx (ServerError class) must debit and trip the breaker",
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
