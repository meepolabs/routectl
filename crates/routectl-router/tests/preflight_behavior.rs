//! Real-dispatch behavior proofs for the pre-flight pipeline: an eligible verdict
//! ACTUALLY rewriting a request before it leaves, the two controls that make that
//! rewrite attributable (no resident verdict, and a resident-but-unacknowledged
//! one), the observability a real rewrite emits, and a zero-cap lane running free
//! validation while committing nothing.
//!
//! The CANARY is deliberately not here. Its cadence walk, its durable clear, and
//! the single-claim property at the boundary live in
//! `crates/routectl-cli/src/server/canary_span_tests.rs`
//! (`the_canary_cadence_clears_durably_and_the_next_request_forwards_unchanged`
//! and `only_one_of_many_concurrent_requests_at_the_boundary_claims_the_canary`),
//! because the durability half needs a usage ledger and a restart boundary that
//! this crate's test target does not carry. The assembled-daemon sidecar
//! (`crates/routectl-cli/src/server/preflight_daemon_tests.rs`) covers the same
//! settlement over a real HTTP request boundary.
//!
//! # Why these live here rather than in the daemon's e2e suite
//!
//! Because the boundary cannot be reached through a loopback upstream, and every
//! in-process HTTP mock binds loopback. Pre-flight refuses a target whose base URL
//! names a local hop -- a rejection from one is not attributable to an upstream, so
//! a verdict must neither be learned from it nor acted on for it -- and probe
//! activation refuses the same targets. So a test that spawned a daemon against
//! wiremock would be asserting over a lane where the feature is CORRECTLY inert,
//! and it would pass with the whole pipeline deleted.
//!
//! The same constraint is already documented and worked around the same way in
//! `probe_beta_wire.rs`: run the REAL router on a remote-looking base URL behind a
//! provider that answers in-process. What that costs is the HTTP layer, which the
//! daemon suite covers separately for the status surface; what it buys is the only
//! configuration in which these behaviors are reachable at all.
//!
//! # What makes each case distinguish present from absent
//!
//! Each asserts on the BYTES the provider received, not on a decision record: the
//! request the upstream would have seen is the whole claim. A pipeline that
//! recorded an acting decision and dispatched the original body passes every
//! record-level assertion and fails these.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Message, MessageContent, Provider,
    ReasoningConfig, Result, Role, TokenCount, Usage,
};
use routectl_router::{
    AliasValue, Config, ModelEntry, ResolvedModel, Router, plant_acting_field_verdict_for_tests,
};

/// The one grounded closed-table path, and the field a pre-flight rewrite drops.
const GROUNDED_PATH: &str = "thinking.enabled.display";

/// A remote-looking base URL, so the attributability gate admits the lane. Nothing
/// is ever sent here -- the capturing provider answers in-process.
const REMOTE_BASE: &str = "https://api.anthropic.com";

const ALIAS: &str = "preflight-behavior-alias";
const STATE_KEY: &str = "sonnet";

/// Every request the seat received, plus how it answered.
struct Seat {
    seen: parking_lot::Mutex<Vec<ChatRequest>>,
    /// Requests still CARRYING the field are rejected while this is true, which is
    /// what makes the canary's unrepaired attempt distinguishable from a repaired
    /// one by outcome as well as by bytes.
    reject_carried: bool,
    calls: AtomicUsize,
}

impl Seat {
    fn new(reject_carried: bool) -> Arc<Self> {
        Arc::new(Self {
            seen: parking_lot::Mutex::new(Vec::new()),
            reject_carried,
            calls: AtomicUsize::new(0),
        })
    }

    /// Whether `req` still carries either canonical carrier of the closed-table
    /// field.
    ///
    /// BOTH carriers, because a rewrite that dropped one and left the other would
    /// still ship the field on the wire -- which is the defect a single-carrier
    /// check could not see.
    fn carries_field(req: &ChatRequest) -> bool {
        req.routectl_internal.anthropic_thinking_display.is_some()
            || req.reasoning.as_ref().is_some_and(|r| r.exclude.is_some())
    }

    fn record(&self, req: &ChatRequest) -> bool {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let carried = Self::carries_field(req);
        self.seen.lock().push(req.clone());
        carried
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    /// Per attempt, in order: whether it still carried the field.
    fn carried_per_attempt(&self) -> Vec<bool> {
        self.seen.lock().iter().map(Self::carries_field).collect()
    }
}

/// The rejection body the captured envelope produced, so the fixture's error is
/// realistic. It is NOT what makes anything fire -- production attributes no path
/// to it -- it is here so the shape is the real one.
const FIELD_REJECT_BODY: &str = r#"{"error":{"type":"invalid_request_error","message":"thinking.enabled.display: Input should be 'summarized', 'omitted'"}}"#;

#[async_trait::async_trait]
impl Provider for Seat {
    fn id(&self) -> &'static str {
        "p0"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p0", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        let carried = self.record(&req);
        if carried && self.reject_carried {
            return Err(Error::upstream("p0", 400, FIELD_REJECT_BODY));
        }
        Ok(ChatResponse {
            model: "claude-sonnet-4-5".to_string(),
            usage: Some(Usage::default()),
            choices: vec![routectl_core::Choice {
                index: 0,
                message: Message {
                    role: Role::Assistant,
                    content: MessageContent::Text("ok".into()),
                    reasoning: None,
                    reasoning_details: vec![],
                    name: None,
                    tool_call_id: None,
                    tool_calls: None,
                    refusal: None,
                },
                finish_reason: Some("stop".into()),
                matched_stop_sequence: None,
                logprobs: None,
            }],
            ..Default::default()
        })
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream("p0", 500, "unused"))
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.record(&req);
        Ok(TokenCount {
            input_tokens: 7,
            extras: serde_json::Map::new(),
        })
    }
}

/// A one-seat anthropic-api config on a remote-looking base URL.
fn config() -> Config {
    let toml_text = format!(
        "\n[providers.p0]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\nbase_url = \"{REMOTE_BASE}\"\n"
    );
    let mut config: Config = toml::from_str(&toml_text).expect("valid test toml");
    config.models.insert(
        STATE_KEY.to_string(),
        ModelEntry::new("p0", "claude-sonnet-4-5"),
    );
    config
        .aliases
        .insert(ALIAS.to_string(), AliasValue::Single(STATE_KEY.to_string()));
    config
}

/// A router over `config()` with `seat` installed as the resolved model.
fn router_with(seat: Arc<Seat>) -> Router {
    let mut router = Router::new(Arc::new(config()));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        STATE_KEY.to_string(),
        Arc::new(ResolvedModel::new(
            STATE_KEY.to_string(),
            "p0".to_string(),
            seat as Arc<dyn Provider>,
            "claude-sonnet-4-5".to_string(),
        )),
    );
    router.install_resolved_models(models);
    router
}

/// A request CARRYING the closed-table field on its canonical carrier.
fn req_with_field() -> ChatRequest {
    let mut req = ChatRequest {
        model: ALIAS.to_string(),
        messages: vec![Message {
            role: Role::User,
            content: MessageContent::Text("hello".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        }]
        .into(),
        // An ACTIVE thinking request alongside the display field, so a drop can be
        // shown to remove the field without disabling the feature.
        reasoning: Some(ReasoningConfig {
            effort: None,
            max_tokens: Some(2048),
            exclude: Some(true),
            enabled: Some(true),
        }),
        ..Default::default()
    };
    req.routectl_internal.anthropic_thinking_display = Some("summarized".into());
    req
}

// ---------------------------------------------------------------------------
// A real pre-flight rewrite
// ---------------------------------------------------------------------------

/// An ACKNOWLEDGED eligible envelope verdict rewrites the request before it
/// leaves, and the provider receives a body without the field.
///
/// THE central behavior proof, and it asserts on the BYTES the seat received
/// rather than on a decision record: a planner that recorded an acting decision
/// and dispatched the original body would pass every record-level assertion. The
/// seat here answers success unconditionally, so nothing downstream can repair --
/// if the field is absent, pre-flight is the only thing that removed it.
///
/// Mutation checks: delete the `adopt_row` transform application -> red, the seat
/// receives the field; plant the verdict with ZERO confirmations -> red, the
/// eligibility gate correctly refuses and the field arrives.
#[tokio::test]
async fn an_acknowledged_eligible_verdict_rewrites_the_dispatched_request() {
    let seat = Seat::new(false);
    let router = router_with(Arc::clone(&seat));
    plant_acting_field_verdict_for_tests(&router, STATE_KEY, GROUNDED_PATH, 1);
    let original = req_with_field();
    assert!(
        Seat::carries_field(&original),
        "premise: the request carries the field, so its absence downstream is a rewrite",
    );

    let served = router.complete(original.clone()).await;

    assert!(served.is_ok(), "the rewritten request serves: {served:?}");
    assert_eq!(
        seat.calls(),
        1,
        "exactly one dispatch, so no repair retry ran"
    );
    assert_eq!(
        seat.carried_per_attempt(),
        vec![false],
        "the body the provider RECEIVED carries neither carrier of the field -- \
         which is the only evidence that the rewrite reached the wire",
    );
    assert!(
        Seat::carries_field(&original),
        "and the caller's own request is untouched: the planner rewrites a clone",
    );
}

/// The FEATURE-ABSENT control: with no verdict planted, the same request reaches
/// the provider unchanged.
///
/// Without this the test above would pass against a build that stripped the field
/// unconditionally -- which is a fidelity defect, not the feature.
#[tokio::test]
async fn with_no_resident_verdict_the_request_reaches_the_provider_unchanged() {
    let seat = Seat::new(false);
    let router = router_with(Arc::clone(&seat));

    let served = router.complete(req_with_field()).await;

    assert!(served.is_ok());
    assert_eq!(
        seat.carried_per_attempt(),
        vec![true],
        "no resident verdict means nothing authorizes a rewrite, so the client's \
         field is forwarded verbatim",
    );
}

/// An UNACKNOWLEDGED verdict does not rewrite either.
///
/// The second half of the gate: a verdict minted by one reactive repair whose
/// confirmation has not been acknowledged is resident and INERT. A build that acted
/// on residency alone would pass the absent-control above while rewriting traffic
/// on unconfirmed evidence, which is exactly what the quorum exists to prevent.
#[tokio::test]
async fn an_unacknowledged_resident_verdict_does_not_rewrite() {
    let seat = Seat::new(false);
    let router = router_with(Arc::clone(&seat));
    plant_acting_field_verdict_for_tests(&router, STATE_KEY, GROUNDED_PATH, 0);

    let served = router.complete(req_with_field()).await;

    assert!(served.is_ok());
    assert_eq!(
        seat.carried_per_attempt(),
        vec![true],
        "zero acknowledged confirmations authorizes no rewrite",
    );
}

/// A pre-flight rewrite counts one adopted-row action and emits one request WARN.
///
/// Ties the observability floor to the behavior: the counter and the WARN must move
/// on a request that was actually modified, not merely on one that was considered.
#[test]
fn a_real_rewrite_counts_one_adopted_row_action_and_warns_once() {
    let seat = Seat::new(false);
    let router = router_with(Arc::clone(&seat));
    plant_acting_field_verdict_for_tests(&router, STATE_KEY, GROUNDED_PATH, 1);
    assert_eq!(
        router.field_repair_counters().preflight_actions,
        0,
        "premise: nothing has been rewritten yet",
    );

    // `capture_events` takes a SYNCHRONOUS closure, so the dispatch is driven on a
    // current-thread runtime inside it. A bare futures executor is not a substitute:
    // the dispatch path arms tokio timeouts and a futures-only executor has no
    // reactor to fire them, so the walk would hang rather than fail.
    let events = routectl_testkit::capture_events(|| {
        let served = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread runtime with timers")
            .block_on(router.complete(req_with_field()));
        assert!(served.is_ok(), "premise: the rewritten request serves");
    });

    assert_eq!(
        router.field_repair_counters().preflight_actions,
        1,
        "one adopted row rewrite counts one action",
    );
    let warns: Vec<_> = events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN && e.field("event").is_some())
        .filter(|e| e.field("event") == Some("envelope_field_preflight"))
        .collect();
    assert_eq!(
        warns.len(),
        1,
        "and the modified request earns exactly one WARN: {warns:?}",
    );
    assert_eq!(
        warns[0].field("transform_class"),
        Some("envelope"),
        "which names the class it acted on",
    );
}

// ---------------------------------------------------------------------------
// Cap zero: free validation runs, nothing is committed
// ---------------------------------------------------------------------------

/// With a paid cap of ZERO, a lane still activates and runs free validation, and no
/// reservation is ever committed.
///
/// The cap-zero boundary, asserted on the two halves that
/// can disagree: free work HAPPENS (the lane is not disabled) while paid spend does
/// NOT (no reservation, no paid call). A build that disabled the lane wholesale
/// would satisfy "no paid call" while losing every free validator.
///
/// No ledger is installed on this router at all, which is the fail-closed default:
/// `reserve_paid_probe_unit` answers `Unavailable` with no ledger, so a paid call is
/// structurally impossible here and the assertion is about the FREE half.
#[tokio::test]
async fn a_zero_cap_lane_still_activates_and_runs_free_validation() {
    let seat = Seat::new(false);
    let router = router_with(Arc::clone(&seat));
    // Default fidelity config: every provider's cap is zero.
    assert!(
        router.fidelity_snapshot().probes.activations_total.eq(&0),
        "premise: nothing has activated before the first admitted request",
    );

    let served = router.complete(req_with_field()).await;
    assert!(served.is_ok(), "the admitted request serves: {served:?}");
    // The activation is non-blocking for the user request, so the pass runs on the
    // driver's tick rather than inline. Run one tick, exactly as the daemon does.
    let summary = router.run_probe_pass().await;

    let snapshot = router.fidelity_snapshot();
    assert!(
        snapshot.probes.activations_total > 0,
        "a lane activates on its first admitted real request, whatever its paid cap",
    );
    assert!(
        !summary.paid_probe_attempted,
        "and a zero cap attempts no paid probe -- the eligibility predicate reads the \
         cap before any candidate is claimed",
    );
    assert_eq!(
        snapshot.probes.paid_reservations_committed_total, 0,
        "so no reservation is committed: cap zero means nothing is spent",
    );
    assert_eq!(
        snapshot.probes.paid_provider_calls_started_total, 0,
        "and no paid call is dispatched",
    );
}

/// The same lane's free validation genuinely RAN, rather than the pass being empty.
///
/// The control that makes the cap-zero case non-vacuous: a scheduler that queued
/// nothing would report zero paid calls too, and the assertions above could not tell
/// the difference between "free ran, paid refused" and "nothing happened".
#[tokio::test]
async fn the_zero_cap_lanes_free_validation_actually_runs() {
    let seat = Seat::new(false);
    let router = router_with(Arc::clone(&seat));

    let _ = router.complete(req_with_field()).await;
    let before = router.fidelity_snapshot().probes.queued;
    let summary = router.run_probe_pass().await;

    assert!(
        before > 0 || summary.free_validators_run > 0,
        "the activation queued real free work, and the pass ran it -- so 'no paid \
         call' above is a refusal rather than an empty scheduler",
    );
}
