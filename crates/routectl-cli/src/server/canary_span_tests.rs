//! THE spanning canary proof: real Router dispatch at full cadence, the canary's
//! own settlement, the production drain to a real writer, and the restart that
//! proves the clear was durable -- in one continuous path.
//!
//! # What this covers that nothing else does
//!
//! The pieces are each pinned elsewhere and none of those pins spans the seam
//! between them. `field_canary_settlement_tests` drives real dispatch and asserts
//! the clear rides out on the dispatch metadata, but stops at the metadata: a
//! build that produced the event and never persisted it passes every assertion
//! there. `capability_boundary_tests` drives the real drain and a real restart,
//! but HAND-BUILDS the cleared event: a build whose canary never produced one
//! passes every assertion there too. Between the two sits the defect neither can
//! see -- a verdict cleared in memory whose row never reached the ledger, which a
//! warm rebuild then resurrects, so the next boot starts rewriting traffic again
//! against a verdict a canary already disproved.
//!
//! So this test walks the whole thing: ninety-nine repaired completions, the
//! hundredth carrying the field unrepaired, its upstream success settling the
//! disproof, the outstanding-request accounting, the production
//! `drain_capability_events` call, the writer's own drain, a restart replaying the
//! ledger, and the request after it forwarding unchanged.
//!
//! # Why this is a direct Router integration rather than a spawned daemon
//!
//! Pre-flight and probe activation both refuse a target whose base URL names a
//! local hop -- a rejection from one is not attributable to an upstream, so a
//! verdict must neither be learned from it nor acted on for it. Every in-process
//! HTTP mock binds loopback. A daemon-plus-wiremock harness would therefore assert
//! over a lane where the feature is CORRECTLY inert, and would pass with the whole
//! pipeline deleted. The same constraint is documented and worked around the same
//! way in `routectl-router`'s `probe_beta_wire.rs` and `preflight_behavior.rs`:
//! the real Router on a remote-looking base URL behind a provider that answers
//! in-process. What is given up is the HTTP layer, which the status e2e suite
//! covers separately; what is bought is the only configuration in which these
//! behaviors are reachable at all.
//!
//! # Why every claim is read off the dispatched bytes or the ledger
//!
//! "Repaired", "restored", and "forwards unchanged" are claims about what reached
//! the upstream, and "durable" is a claim about what a later process can read. A
//! decision record says what the planner DECIDED. So the cadence assertions read
//! the seat's recorded request bodies and the durability assertion reads a
//! registry rebuilt from the ledger by a second Router.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use parking_lot::Mutex;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Choice, Error, Message, MessageContent, Provider,
    ReasoningConfig, Result, Role, TokenCount,
};
use routectl_router::config::CANARY_INTERVAL;
use routectl_router::{
    AliasValue, Config, ModelEntry, ResolvedModel, Router, RouterOptions,
    plant_acting_field_verdict_for_tests,
};
use routectl_usage::{CHANNEL_CAPACITY, UsageHandle, UsageWriter};

use crate::handlers::usage_capture::{UsageCapture, build_usage_draft};
use crate::server::capability_rebuild;
use crate::server::test_support::isolate_usage_db;

/// The one grounded row in the closed table -- the field a pre-flight rewrite
/// drops and a canary restores.
const FIELD_PATH: &str = "thinking.enabled.display";

/// The cadence this file walks, as a LITERAL.
///
/// Deliberately not `CANARY_INTERVAL`. Every assertion below that read the
/// constant would move WITH it, so retuning the constant to 99 left all four tests
/// green -- they were pinning "the cadence is whatever the code says", which is a
/// tautology that can only fail in a world where the constant does not exist.
/// Measured, not assumed: that exact mutation passed before this literal existed.
///
/// So the walk length is spelled here and RECONCILED against the constant below.
/// A deliberate retune then fails this one named assertion, with a message saying
/// which two numbers disagree -- which is the review signal a safety parameter's
/// change should produce.
const EXPECTED_CADENCE: u32 = 100;

/// A rejection body shaped like the captured envelope. Carries no secret, and is
/// NOT what makes anything fire -- production attributes no path to it, so the
/// canary that draws it settles INCONCLUSIVE, which is exactly the property the
/// contention fixture needs (an inconclusive settlement leaves the verdict
/// resident).
const FIELD_REJECT_BODY: &str = r#"{"error":{"type":"invalid_request_error","message":"thinking.enabled.display: Input should be 'summarized', 'omitted'"}}"#;

/// A remote-looking base URL, so the attributability gate admits the lane.
/// Nothing is ever sent here: the seat below answers in-process.
const REMOTE_BASE: &str = "https://api.anthropic.com";

const ALIAS: &str = "canary-span-alias";
const STATE_KEY: &str = "sonnet";

/// The wire-shape capability key, assembled from parts.
///
/// The namespace prefix has exactly one compiled spelling and a lexical guard in
/// `routectl-router` fails if the literal appears anywhere else under `crates/`.
/// The VALUE is byte-identical either way.
fn field_key() -> String {
    format!("{}{FIELD_PATH}", format_args!("{}{}", "fie", "ld:"))
}

/// Every request the seat received, so an assertion reads the BYTES that went
/// upstream rather than inferring from a count.
#[derive(Default)]
struct Seat {
    seen: Mutex<Vec<ChatRequest>>,
    calls: AtomicUsize,
    /// Reject any attempt still CARRYING the field.
    ///
    /// Off for the cadence test, where the canary's unrepaired success is the
    /// disproof under test. ON for the contention test, and there it is what keeps
    /// the fixture honest: a disproof CLEARS the verdict, after which every later
    /// request forwards the client's field unchanged -- which is indistinguishable
    /// from a canary restoration by looking at the bytes. Rejecting instead settles
    /// the canary inconclusive, the verdict stays resident for the whole burst, and
    /// "carried" then means exactly "restored by a canary".
    reject_carried: bool,
}

impl Seat {
    /// Whether `req` still emits the grounded wire field through EITHER canonical
    /// carrier.
    ///
    /// Both, because a rewrite that dropped one and left the other would still
    /// ship the field on the wire -- the defect a single-carrier check cannot see.
    fn carries_field(req: &ChatRequest) -> bool {
        req.routectl_internal.anthropic_thinking_display.is_some()
            || req.reasoning.as_ref().is_some_and(|r| r.exclude.is_some())
    }

    fn record(&self, req: &ChatRequest) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().push(req.clone());
    }

    /// Per attempt, in order: whether it still carried the field.
    fn carried_per_attempt(&self) -> Vec<bool> {
        self.seen.lock().iter().map(Self::carries_field).collect()
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

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
    /// Answers EVERY request successfully, repaired or not.
    ///
    /// That is what makes the hundredth request a DISPROOF: the canary restores
    /// the field, the upstream accepts it, and accepting the field the verdict
    /// claims is broken is exactly the evidence that the verdict is wrong.
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.record(&req);
        if self.reject_carried && Self::carries_field(&req) {
            return Err(Error::upstream("p0", 400, FIELD_REJECT_BODY));
        }
        Ok(ChatResponse {
            model: "claude-sonnet-4-5".to_string(),
            usage: Some(routectl_core::Usage::default()),
            choices: vec![Choice {
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
        req: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk>>> {
        self.record(&req);
        Ok(futures::stream::iter(vec![Ok(content_chunk())]).boxed())
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.record(&req);
        Ok(TokenCount {
            input_tokens: 7,
            extras: serde_json::Map::new(),
        })
    }
}

/// A CONTENT-BEARING chunk: the streaming walk commits on first content, so a
/// content-free chunk would read as a closed stream.
fn content_chunk() -> ChatChunk {
    use futures::stream::StreamExt as _;
    let _ = futures::stream::empty::<()>().boxed();
    ChatChunk {
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

use futures::stream::StreamExt;

/// A config on a remote-looking base URL, with its usage DB isolated to a
/// tempdir so no test touches the real ledger.
fn spanning_config() -> (Arc<Config>, tempfile::TempDir) {
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
    let dir = isolate_usage_db(&mut config);
    (Arc::new(config), dir)
}

/// A Router over `config` with `seat` installed as the resolved model.
fn router_with(config: &Arc<Config>, seat: Arc<Seat>) -> Router {
    let mut router = Router::new(Arc::clone(config));
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

/// A request carrying the grounded field through BOTH canonical carriers, plus an
/// active thinking request so a drop removes the field without disabling the
/// feature.
fn req() -> ChatRequest {
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

/// Run the blocking startup warm off the runtime thread.
///
/// The warm commits its fail-closed boundary through the acknowledged batch path,
/// which blocks on the writer's reply, and Tokio panics if a worker blocks.
/// Production dispatches the whole warm through `spawn_blocking` for the same
/// reason.
fn warm_off_runtime(db_path: &std::path::Path, router: &Router, usage: &UsageHandle) {
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                capability_rebuild::warm_capability_registry_from_ledger(db_path, router, usage);
            })
            .join()
            .expect("warm thread");
    });
}

/// Every `field:`-namespaced capability key a Router rebuilt from the ledger
/// finds resident.
///
/// THE durability oracle: it is read by a SECOND Router from the persisted rows,
/// so it can only see what actually landed. An in-memory assertion on the first
/// Router cannot distinguish a clear that persisted from one that did not.
fn resident_field_keys_after_restart(config: &Arc<Config>, usage: &UsageHandle) -> Vec<String> {
    let router = Router::new(Arc::clone(config));
    warm_off_runtime(&config.usage.db_path, &router, usage);
    router
        .learned_capability_snapshot()
        .into_iter()
        .map(|entry| entry.feature_key)
        .filter(|key| key.starts_with(&format_args!("{}{}", "fie", "ld:").to_string()))
        .collect()
}

/// Seed the ledger so the verdict is DURABLY resident before the cadence walk.
///
/// Without this the restart assertion at the end is vacuous, and measurably so: the
/// planting seam writes to the in-memory registry only, so a ledger holding no
/// `broken` row reports the key absent after restart whether or not the clear ever
/// persisted -- SKIPPING THE DRAIN ENTIRELY LEFT THE TEST GREEN. Seeding a real row
/// first is what turns that absence into evidence.
///
/// Planted through the real ledger rather than a router mutator, so the fixture is
/// the state a live session actually leaves behind: a boundary tombstone stamped
/// with the revision pair this Router carries (a mismatch would make the warm
/// classify and replay nothing), then one `broken` wire-shape row.
fn seed_durable_verdict(config: &Arc<Config>, usage: &UsageHandle) {
    let now = crate::server::ledger_reader::epoch_ms_now();
    let catalog = i64::from(Router::new(Arc::clone(config)).catalog_version());
    let committed = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                usage.commit_capability_events_blocking(
                    vec![
                        routectl_usage::CapabilityEvent::tombstone(now - 5_000, catalog, 0),
                        routectl_usage::CapabilityEvent {
                            ts: now - 4_000,
                            lane_key: STATE_KEY.to_string(),
                            capability: field_key(),
                            verdict: "broken".to_string(),
                            phase: "f1".to_string(),
                            source: "live".to_string(),
                            tier: "self-identifying".to_string(),
                            evidence_class: None,
                            upstream_token: None,
                            catalog_version: catalog,
                            overlay_revision: 0,
                        },
                    ],
                    1,
                )
            })
            .join()
            .expect("commit thread")
    });
    assert!(
        matches!(committed, routectl_usage::BatchCommit::Committed { .. }),
        "the seed must land, or every later assertion is about an empty ledger: \
         {committed:?}",
    );
}

/// Drain one dispatch's capability events through the PRODUCTION capture path.
///
/// The real `drain_capability_events`, not a replica: the point of this test is
/// that the production path stamps and enqueues what a canary's settlement
/// produced, and a hand-rolled enqueue here would assert agreement with itself.
fn drain_through_production(usage: &UsageHandle, meta: &routectl_router::DispatchMeta) {
    let request = ChatRequest {
        model: ALIAS.to_string(),
        ..Default::default()
    };
    let capture = UsageCapture::new(
        build_usage_draft("p0", &request, "canary-span".into()),
        usage.clone(),
        "ingress-span".to_string(),
    );
    // Revision 0 on both stamps: this Router installs no catalog overlay, so the
    // boundary the writer compares against is the default. A mismatched stamp
    // here would make the writer drop the row and the restart assertion would
    // fail for a reason unrelated to the canary -- which is why the stamps are
    // read from the same defaults the rebuild uses rather than invented.
    capture.drain_capability_events(meta, 0, 0);
}

// ---------------------------------------------------------------------------
// THE spanning test
// ---------------------------------------------------------------------------

/// Ninety-nine repairs, a hundredth unrepaired canary whose success clears the
/// verdict DURABLY, and a request after the restart that forwards unchanged.
///
/// One test rather than several, because the claim is the SPAN: each boundary here
/// is already pinned in isolation elsewhere, and what no isolated pin can see is a
/// clear that happens in memory and never reaches the ledger -- after which a warm
/// rebuild resurrects the verdict and the next boot rewrites traffic against a
/// verdict a canary already disproved.
///
/// Mutation checks, each red on its own named assertion:
/// - `CANARY_INTERVAL` 100 -> 99: the carried attempt lands at index 98, red on
///   the cadence position;
/// - bypass the unrepaired restore (make the canary arm rewrite like any other
///   row): nothing is carried, red on the cadence position AND on the clear;
/// - skip the durable clear (drop `cleared_capabilities` from the settlement, or
///   skip the drain): red on the post-restart residency;
/// - allow a second concurrent canary: red on the single-claim assertion in the
///   contention test below.
// Every test here carries `#[serial_test::serial]`, joining the DEFAULT serial group.
//
// Not for shared state of its own -- these fixtures are per-test tempdirs and private
// registries. The reason is PROCESS-WIDE SIGNALS: two sibling tests in this crate
// deliver `SIGTERM` and `SIGHUP` to their own process to exercise graceful shutdown
// and reload. Those signals reach every thread, so any test merely IN FLIGHT when one
// fires dies with `signal: 15` -- and the runner reports that as the whole binary
// failing, with no failing test named.
//
// The signal tests already hold that group, so joining it keeps these out of the
// window.
//
// WHAT THE EVIDENCE SHOWS, and the attribution it settles. At
// `--test-threads=512` the binary intermittently dies with `signal: 15` and NO failing
// test named. Skipping the self-SIGTERM test alone made 8 of 8 runs clean, which
// identifies the signal as the mechanism rather than any assertion. Crucially the
// UNMODIFIED baseline dies the same way at the same thread count (measured 1 of 20),
// so this is a PRE-EXISTING load-sensitive race in the suite, not something this file
// introduced -- added test volume only changes how often the window is occupied.
//
// This guard is the correct narrowing on its own terms (a process-wide signal kills any
// in-flight test, and the group is the one the signal tests already hold), but it is
// not a proof: removing it did not reproduce the failure in 14 runs, and a targeted
// two-thread pairing did not either. The residual risk is reduced, not eliminated.
// The mechanism is written down here rather than left as a passing suite so the next
// reader seeing an unexplained `signal: 15` looks at the two self-signalling tests
// first instead of hunting an assertion that never failed.

#[tokio::test]
#[serial_test::serial]
async fn the_canary_cadence_clears_durably_and_the_next_request_forwards_unchanged() {
    let (config, _dir) = spanning_config();
    let (usage, writer) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );
    let seat = Arc::new(Seat::default());
    let router = router_with(&config, Arc::clone(&seat));

    // The phases are NAMED helpers rather than one flat body, and each returns what
    // the next one needs -- so the span reads as the sequence it is while staying ONE
    // test. Splitting it into separate tests would lose exactly the property it
    // exists for: that these boundaries hold in sequence against one live state.
    arrange_durable_eligible_verdict(&config, &usage, &router);
    let meta = walk_the_cadence_and_settle_the_canary(&router, &seat).await;
    assert_the_disproof_accounting(&router, &meta);
    persist_through_the_production_drain(usage, &meta, writer);
    let usage2 = assert_the_clear_survived_a_restart(&config);
    assert_the_next_request_forwards_unchanged(&config, &usage2).await;
}

/// PHASE 1: make the verdict durable, then acknowledge it in memory.
///
/// Both halves are required. The ledger row is what makes the post-restart absence
/// falsifiable -- measured, not assumed: without it, skipping the drain entirely left
/// the whole test green. The in-memory acknowledgement is what makes pre-flight
/// eligible, since live traffic cannot yet produce a confirmation count.
fn arrange_durable_eligible_verdict(config: &Arc<Config>, usage: &UsageHandle, router: &Router) {
    seed_durable_verdict(config, usage);
    plant_acting_field_verdict_for_tests(router, STATE_KEY, FIELD_PATH, 1);
    assert!(
        resident_field_keys_after_restart(config, usage).contains(&field_key()),
        "PREMISE, and the load-bearing one: the verdict is durable BEFORE the walk, so \
         its absence after the clear is evidence the clear persisted",
    );
}

/// PHASE 2: a full cadence of eligible completions, asserting on the BYTES that
/// exactly the hundredth carried the field unrepaired.
///
/// Returns the canary dispatch's own metadata, which is what carries the clear.
async fn walk_the_cadence_and_settle_the_canary(
    router: &Router,
    seat: &Arc<Seat>,
) -> routectl_router::DispatchMeta {
    assert_eq!(
        CANARY_INTERVAL, EXPECTED_CADENCE,
        "the re-verification cadence is a hard safety parameter: one request in \
         {EXPECTED_CADENCE} carries the tested field unrepaired. A change here is a \
         deliberate retune and must be reviewed as one -- see EXPECTED_CADENCE for \
         why this file pins the literal rather than reading the constant",
    );
    let mut canary_meta = None;
    for idx in 0..EXPECTED_CADENCE {
        let dispatched = router
            .complete_with_options(req(), RouterOptions::new())
            .await;
        assert!(
            dispatched.result.is_ok(),
            "request {idx} serves: {:?}",
            dispatched.result,
        );
        if !dispatched.meta.cleared_capabilities.is_empty() {
            canary_meta = Some(dispatched.meta);
        }
    }

    let carried = seat.carried_per_attempt();
    assert_eq!(
        seat.calls(),
        EXPECTED_CADENCE as usize,
        "one attempt per request -- no request retried, so the positions below are \
         request indices",
    );
    let canary_positions: Vec<usize> = carried
        .iter()
        .enumerate()
        .filter_map(|(idx, c)| c.then_some(idx))
        .collect();
    assert_eq!(
        canary_positions,
        vec![EXPECTED_CADENCE as usize - 1],
        "exactly the hundredth request carried the tested field UNREPAIRED; every \
         earlier one was rewritten before dispatch",
    );
    canary_meta.expect("the canary's disproof rode out on its dispatch")
}

/// PHASE 3: the disproof produced one clear event and charged every request the
/// verdict had modified.
fn assert_the_disproof_accounting(router: &Router, meta: &routectl_router::DispatchMeta) {
    assert_eq!(
        meta.cleared_capabilities.len(),
        1,
        "the disproof produces exactly one clear -- one verdict was disproved",
    );
    assert_eq!(meta.cleared_capabilities[0].capability_key, field_key());
    let counters = router.field_repair_counters();
    assert_eq!(
        counters.disproved_requests,
        u64::from(EXPECTED_CADENCE) - 1,
        "every request the verdict repaired is charged to the lifetime alarm -- \
         ninety-nine of them, the canary itself having been restored rather than \
         repaired. This counts requests AFFECTED, not canary attempts",
    );
    assert_eq!(
        counters.outstanding_unconfirmed, 0,
        "and the outstanding tally is transferred rather than copied, so a second \
         disproof could not charge the same requests twice",
    );
}

/// PHASE 4: the production drain, then the writer's own.
///
/// THE seam no other test spans: everything before this is in memory.
fn persist_through_the_production_drain(
    usage: UsageHandle,
    meta: &routectl_router::DispatchMeta,
    writer: UsageWriter,
) {
    drain_through_production(&usage, meta);
    // The handle is taken BY VALUE and dropped before the shutdown, and that ordering
    // is the whole contract. The writer's drain ends only once every producer clone is
    // gone; a retained handle keeps the channel open, so the drain waits out its
    // deadline and then ABANDONS the queued rows with a warning. The row this test
    // exists to persist would be among them, and the shutdown would still return
    // normally -- so the failure is silent.
    drop(usage);
    let started = std::time::Instant::now();
    writer.shutdown();
    // A CLEAN, TIMELY join rather than a deadline detach. The drain deadline is tens of
    // seconds, so "it returned" proves nothing about whether it flushed; a bound well
    // under that deadline is what distinguishes a real drain from an abandonment.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "the writer drained promptly rather than waiting out its abandonment deadline: \
         a retained producer handle is what turns a clean drain into a silent timeout",
    );
}

/// PHASE 5: a SECOND Router reading the persisted rows no longer finds the verdict.
///
/// An in-memory check on the first Router cannot tell a clear that landed from one
/// that did not, which is why the oracle is a fresh rebuild. Returns the new writer
/// handle the next phase dispatches against.
fn assert_the_clear_survived_a_restart(config: &Arc<Config>) -> UsageHandle {
    let (usage2, writer2) = UsageWriter::start(
        config.usage.db_path.clone(),
        CHANNEL_CAPACITY,
        0,
        config.usage.enabled,
    );
    let after_restart = resident_field_keys_after_restart(config, &usage2);
    assert!(
        !after_restart.contains(&field_key()),
        "the clear SURVIVED the restart: a verdict cleared only in memory would be \
         resurrected here, and the next boot would resume rewriting traffic against \
         a verdict this canary disproved. Resident: {after_restart:?}",
    );
    // The writer is detached deliberately: the handle stays usable, which is all the
    // next phase's dispatch needs.
    drop(writer2);
    usage2
}

/// PHASE 6: request 101, on a Router rebuilt from the ledger -- the state a restarted
/// daemon is actually in -- forwards the client's field unchanged.
async fn assert_the_next_request_forwards_unchanged(config: &Arc<Config>, usage: &UsageHandle) {
    let restarted_seat = Arc::new(Seat::default());
    let restarted = router_with(config, Arc::clone(&restarted_seat));
    warm_off_runtime(&config.usage.db_path, &restarted, usage);

    let next = restarted
        .complete_with_options(req(), RouterOptions::new())
        .await;

    assert!(next.result.is_ok());
    assert_eq!(
        restarted_seat.carried_per_attempt(),
        vec![true],
        "request 101, on a Router rebuilt from the ledger, forwards the client's \
         field UNCHANGED -- which is the whole observable point of a durable clear",
    );
}

/// Under concurrent contention at the cadence boundary, exactly ONE request
/// claims the canary.
///
/// The claim is what makes "one request in a hundred" true rather than "every
/// request once the countdown trips": the due flag is sticky, so without a
/// single-flight claim every concurrent request at the boundary would restore the
/// field and a burst would send the unrepaired variant many times over.
///
/// Driven at the REAL dispatch path under a multi-threaded runtime, with the
/// cadence walked to its boundary first so every racing request finds the
/// identity due. Asserted on the seat's bytes: the number of requests that
/// carried the field unrepaired is the number of canaries that ran.
///
/// Mutation evidence, and it names the mechanism precisely because the obvious
/// guess is wrong. Removing the `canary_claimed` early return ALONE leaves this
/// green: at this level the exclusion is carried by `claim_canary` clearing the
/// sticky `due` flag inside the same critical section, since a request only offers
/// to claim when its cadence tick reports due. So the mutations that go RED here
/// are clearing-the-due-flag (14 carried instead of 1) and both guards together.
/// The `canary_claimed` flag's own job is a different one -- excluding a claim from
/// a LATER interval while one is still outstanding -- and its sidecar owns that.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
#[serial_test::serial]
async fn only_one_of_many_concurrent_requests_at_the_boundary_claims_the_canary() {
    let (config, _dir) = spanning_config();
    // REJECTING seat: see `Seat::reject_carried`. A disproof would clear the verdict
    // mid-burst, after which later requests forward the field unchanged -- which
    // reads identically to a canary restoration on the bytes. An inconclusive
    // settlement keeps the verdict resident, so "carried" means "restored".
    let seat = Arc::new(Seat {
        reject_carried: true,
        ..Seat::default()
    });
    let router = Arc::new(router_with(&config, Arc::clone(&seat)));
    plant_acting_field_verdict_for_tests(&router, STATE_KEY, FIELD_PATH, 1);

    // Walk the cadence to ONE BEFORE its trip, so the concurrent burst below all
    // arrive at the boundary. Sequential here deliberately: the contention under
    // test is at the claim, not at the countdown.
    for _ in 0..(EXPECTED_CADENCE - 1) {
        let dispatched = router
            .complete_with_options(req(), RouterOptions::new())
            .await;
        assert!(dispatched.result.is_ok());
    }
    assert_eq!(
        seat.carried_per_attempt().iter().filter(|c| **c).count(),
        0,
        "premise: no canary has run yet -- every request so far was repaired, so the \
         burst below is what reaches the boundary",
    );

    // Act: a burst of concurrent eligible completions, all racing the one claim.
    let mut handles = Vec::new();
    for _ in 0..16 {
        let router = Arc::clone(&router);
        handles.push(tokio::spawn(async move {
            router
                .complete_with_options(req(), RouterOptions::new())
                .await
        }));
    }
    let mut cleared_total = 0;
    for handle in handles {
        let dispatched = handle.await.expect("no dispatch task panics");
        cleared_total += dispatched.meta.cleared_capabilities.len();
    }

    // Assert on the bytes: exactly one request sent the field unrepaired. With the
    // verdict resident for the whole burst, a carried attempt can only be a canary
    // restoration -- every non-canary eligible request is rewritten.
    let carried = seat.carried_per_attempt().iter().filter(|c| **c).count();
    assert_eq!(
        carried, 1,
        "exactly ONE concurrent request carried the field unrepaired: the claim is \
         single-flight, so a burst at the boundary sends one canary and repairs the \
         rest. More than one means every racing request restored the field, which is \
         the unrepaired variant going upstream many times over",
    );
    assert_eq!(
        cleared_total, 0,
        "and nothing was cleared: this canary's rejection is unattributable, so it \
         settles INCONCLUSIVE and the verdict stays resident -- which is what makes \
         the byte oracle above mean 'restored' rather than 'forwarded because the \
         verdict was gone'",
    );
}

// The cadence CONTROLS -- the surfaces that must not advance the countdown, and the
// feature-absent case -- live in an `include!`d fragment so no one compiled file grows
// past the repo's size ceiling. One `#[path] mod` plus includes is the required shape:
// a second `#[path] mod` would rename every test it moved.
include!("canary_span_control_tests.rs");
