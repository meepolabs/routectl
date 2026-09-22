//! The whole paid-probe path composed ONCE, with nothing doubled between the
//! SQLite file and the outbound body: a real file-backed usage database, the
//! real writer actor, the production accounting adapter this module builds, the
//! daemon's own probe pass, and a provider that retains the request it is
//! handed.
//!
//! # Why this is not two adjacent unit tests
//!
//! Every other paid-probe sidecar doubles one side or the other -- the router's
//! dial tests hold the ordering against a recording ledger, and this module's
//! adapter sidecar holds the durable commit against no dial at all. Neither can
//! observe the fact the feature rests on: that the unit a REAL writer put on
//! disk is already durable when the outbound call is made. Here the two are one
//! path, and the observation is taken INSIDE the provider call.
//!
//! # The acknowledgement marker is the ordering instrument
//!
//! Reading the database from inside `complete` proves the row was durable, but
//! a durable row alone cannot say the ROUTER had been told: an implementation
//! that dialed while the reservation's acknowledgement was still in flight
//! could pass whenever the writer's transaction happened to land first. So the
//! marker is what the ordering assertion reads -- it is set by
//! [`AcknowledgingLedger`] only after the real adapter's awaited answer has
//! RETURNED, so a call made before that boundary records a cleared marker
//! regardless of how the writer thread was scheduled.
//!
//! The durable read stays alongside it and answers the other half: the marker
//! says the router was told, and the row read through a second connection says
//! what it was told was already true on disk.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use routectl_router::{PaidProbeLedger, PaidProbeReservation, Router};
use routectl_usage::{UsageHandle, UsageWriter};
use tempfile::TempDir;

use super::{UsagePaidProbeLedger, paid_probe_ledger};
use crate::server::test_support::{added_control_rows, live_usage_writer, usage_control_rows};

/// The lanes, their shared `[providers]` key, and the wire id both resolve to.
///
/// TWO lanes on ONE provider, which is what makes the second pass a budget
/// boundary rather than an empty list: the cap is per provider-day, so lane B's
/// candidate meets a day already spent by lane A. A single lane would leave the
/// second pass nothing to claim, and "no second call" would be free.
const FIRST_LANE: &str = "m1";
const SECOND_LANE: &str = "m2";
const WIRE_PROVIDER: &str = "p1";
const WIRE_UPSTREAM: &str = "claude-sonnet-4-5";

/// The closed-table path and value an admitted request grounds, retained on the
/// candidate and expected back on the paid body.
const WIRE_FIELD_VALUE: &str = "summarized";

/// A generous catalog output ceiling: well above either wire shape's floor, so
/// the profile is admitted on the strength of the floor rather than clamped by
/// the ceiling.
const GENEROUS_OUTPUT_CEILING: u32 = 64_000;

/// The free path's `max_tokens` fill, named so the body assertion can forbid it.
///
/// A literal rather than an import, for the reason the router-side wire sidecar
/// spells out: the producing constant is private to the field-repair module, and
/// what is asserted here is that this number does NOT appear on a paid body.
const FREE_PATH_DEFAULT_MAX_TOKENS: u32 = 2048;

/// The production accounting adapter, wrapped so the moment its answer REACHES
/// the router is observable.
///
/// A delegating wrapper, never a substitute: the inner value is built by
/// [`paid_probe_ledger`] from the live writer's handle, which is exactly what
/// `install_paid_probe_ledger` installs at boot. Every reservation still runs
/// through the real adapter, the real channel, and the real writer transaction;
/// what this adds is one flag flipped AFTER the awaited answer has returned.
///
/// The flag is set only on a COMMIT. A refusal authorizes no call, so marking it
/// would make the flag mean "the adapter answered" rather than "a unit is
/// committed and the router has been told" -- and the second is what an
/// ordering assertion on a money-spending path needs.
struct AcknowledgingLedger {
    inner: Arc<UsagePaidProbeLedger>,
    acknowledged: Arc<AtomicBool>,
}

#[async_trait::async_trait]
impl PaidProbeLedger for AcknowledgingLedger {
    async fn reserve_paid_probe_unit(
        &self,
        provider: &str,
        daily_cap: u32,
    ) -> PaidProbeReservation {
        // THE REAL ADAPTER, awaited to completion. Nothing is recorded before
        // this returns: the marker's whole contract is that it cannot be set
        // while an answer is still in flight.
        let outcome = self
            .inner
            .reserve_paid_probe_unit(provider, daily_cap)
            .await;
        if matches!(outcome, PaidProbeReservation::Committed { .. }) {
            self.acknowledged.store(true, Ordering::SeqCst);
        }
        outcome
    }
}

/// What was true at the instant one paid body was handed to the upstream.
#[derive(Debug, Clone)]
struct ProbeCallObservation {
    /// Whether the router had already received a committed acknowledgement.
    acknowledged: bool,
    /// Every control row readable through a connection the writer does not own.
    rows: BTreeMap<String, String>,
}

/// A provider that answers the free step with a well-formed ZERO -- the shape
/// that spends a one-step free plan without settling it, surfacing a paid
/// candidate -- and that RETAINS every paid body together with what was true
/// when it arrived.
///
/// Both halves of the observation are taken BEFORE the body is answered, so
/// what is captured is the state as of the call rather than as of the pass
/// returning.
struct WireRecordingProvider {
    /// The database the daemon's writer owns, read here through a second,
    /// read-only connection.
    path: PathBuf,
    /// Whether a committed acknowledgement has reached the router yet.
    acknowledged: Arc<AtomicBool>,
    /// Bodies carrying the background marker: the paid probe's own calls.
    probe_bodies: parking_lot::Mutex<Vec<routectl_core::ChatRequest>>,
    /// One observation per paid call, in call order.
    observations: parking_lot::Mutex<Vec<ProbeCallObservation>>,
    /// Calls WITHOUT the background marker: the client-facing dispatches the
    /// probe lanes ride in on. Counted separately so "one paid call" is never
    /// read off a tally the grounding requests also bump.
    client_calls: AtomicUsize,
}

impl WireRecordingProvider {
    fn new(path: &Path, acknowledged: &Arc<AtomicBool>) -> Arc<Self> {
        Arc::new(Self {
            path: path.to_path_buf(),
            acknowledged: Arc::clone(acknowledged),
            probe_bodies: parking_lot::Mutex::new(Vec::new()),
            observations: parking_lot::Mutex::new(Vec::new()),
            client_calls: AtomicUsize::new(0),
        })
    }

    /// How many paid bodies reached the upstream.
    fn probe_calls(&self) -> usize {
        self.probe_bodies.lock().len()
    }

    /// Every paid call's observation, in call order.
    fn observations(&self) -> Vec<ProbeCallObservation> {
        self.observations.lock().clone()
    }

    /// The ONE paid body, or a panic naming the real count.
    fn only_probe_body(&self) -> routectl_core::ChatRequest {
        let bodies = self.probe_bodies.lock();
        assert_eq!(
            bodies.len(),
            1,
            "exactly one paid body must have reached the upstream",
        );
        bodies[0].clone()
    }
}

#[async_trait::async_trait]
impl routectl_core::Provider for WireRecordingProvider {
    fn id(&self) -> &'static str {
        WIRE_PROVIDER
    }
    fn normalize_request(
        &self,
        _: &routectl_core::ChatRequest,
    ) -> routectl_core::Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(
        &self,
        _: serde_json::Value,
    ) -> routectl_core::Result<routectl_core::ChatResponse> {
        Err(routectl_core::Error::normalize_response(
            WIRE_PROVIDER,
            "unused",
        ))
    }
    async fn complete(
        &self,
        req: routectl_core::ChatRequest,
    ) -> routectl_core::Result<routectl_core::ChatResponse> {
        if req.routectl_internal.background_probe {
            // OBSERVED BEFORE ANSWERING, and the marker is read FIRST: it is
            // the fact that cannot be reproduced by a lucky interleaving.
            self.observations.lock().push(ProbeCallObservation {
                acknowledged: self.acknowledged.load(Ordering::SeqCst),
                rows: usage_control_rows(&self.path),
            });
            self.probe_bodies.lock().push(req);
        } else {
            self.client_calls.fetch_add(1, Ordering::SeqCst);
        }
        Ok(routectl_core::ChatResponse {
            model: WIRE_UPSTREAM.to_string(),
            usage: Some(routectl_core::Usage::default()),
            ..Default::default()
        })
    }
    async fn stream(
        &self,
        _: routectl_core::ChatRequest,
    ) -> routectl_core::Result<
        futures::stream::BoxStream<'static, routectl_core::Result<routectl_core::ChatChunk>>,
    > {
        Err(routectl_core::Error::upstream(WIRE_PROVIDER, 500, "body"))
    }
    async fn count_tokens(
        &self,
        _: routectl_core::ChatRequest,
    ) -> routectl_core::Result<routectl_core::TokenCount> {
        Ok(routectl_core::TokenCount {
            input_tokens: 0,
            extras: serde_json::Map::new(),
        })
    }
}

/// The whole composed rig: one temp database, one live writer, one Router
/// carrying the production accounting adapter behind its observation wrapper,
/// and the provider both lanes resolve to.
struct ComposedProbeRig {
    /// Kept for the test's duration; dropping it removes the database.
    _dir: TempDir,
    path: PathBuf,
    handle: UsageHandle,
    writer: UsageWriter,
    router: Router,
    provider: Arc<WireRecordingProvider>,
}

impl ComposedProbeRig {
    /// Build the rig with `daily_cap` units for the shared provider-day and
    /// `output_ceiling` as the catalog-confirmed output ceiling on both lanes.
    fn build(daily_cap: u32, output_ceiling: u32) -> Self {
        let (dir, path, handle, writer) = live_usage_writer();
        let acknowledged = Arc::new(AtomicBool::new(false));
        let provider = WireRecordingProvider::new(&path, &acknowledged);
        let mut config = routectl_router::config::Config::default();
        config.providers.insert(
            WIRE_PROVIDER.to_string(),
            routectl_router::config::ProviderEntry::anthropic_api("literal:k"),
        );
        for lane in [FIRST_LANE, SECOND_LANE] {
            config.models.insert(
                lane.to_string(),
                routectl_router::config::ModelEntry::new(WIRE_PROVIDER, WIRE_UPSTREAM),
            );
        }
        config.aliases.insert(
            "default".to_string(),
            routectl_router::config::AliasValue::Single(FIRST_LANE.to_string()),
        );
        config
            .fidelity
            .paid_probe_daily_caps
            .insert(WIRE_PROVIDER.to_string(), daily_cap);
        let mut router = Router::new(Arc::new(config));
        let mut models = std::collections::BTreeMap::new();
        for lane in [FIRST_LANE, SECOND_LANE] {
            models.insert(
                lane.to_string(),
                Arc::new(
                    routectl_router::ResolvedModel::new(
                        lane,
                        WIRE_PROVIDER,
                        Arc::clone(&provider) as Arc<dyn routectl_core::Provider>,
                        WIRE_UPSTREAM,
                    )
                    .with_effective_row(priced_row(output_ceiling)),
                ),
            );
        }
        router.install_resolved_models(models);
        // THE PRODUCTION ADAPTER, from the same private constructor
        // `install_paid_probe_ledger` calls, over the handle of the writer
        // started above -- wrapped so the acknowledgement boundary is
        // observable, never replaced. Nothing between the Router and the SQLite
        // file is a double.
        let router = router.with_paid_probe_ledger(Arc::new(AcknowledgingLedger {
            inner: paid_probe_ledger(&handle),
            acknowledged,
        }));
        Self {
            _dir: dir,
            path,
            handle,
            writer,
            router,
            provider,
        }
    }

    /// Release the producer side and drain the writer, in the order the serve
    /// loop uses: the Router holds the adapter, and the adapter holds a producer
    /// clone the drain waits on.
    fn stop(self) {
        drop(self.router);
        drop(self.handle);
        self.writer.shutdown();
    }
}

/// Ground and activate both lanes through admitted client dispatches, so the
/// candidates the paid half claims are the ones real traffic produced.
///
/// Asserts its own premise: without two queued free jobs every assertion
/// downstream would pass on an empty schedule.
///
/// Takes the two pieces it needs rather than the whole rig: the rig owns the
/// [`UsageWriter`], whose join channel is not `Sync`, so borrowing it across an
/// await would make every caller's future non-`Send`.
async fn activate_both_lanes(router: &Router, provider: &WireRecordingProvider) {
    for lane in [FIRST_LANE, SECOND_LANE] {
        let _ = router.complete(grounding_request_for(lane)).await;
    }
    assert_eq!(
        router.probe_scheduler_snapshot().queued,
        2,
        "fixture premise: each admitted request must queue its lane's free job",
    );
    assert_eq!(
        provider.client_calls.load(Ordering::SeqCst),
        2,
        "fixture premise: both grounding dispatches must have reached the upstream",
    );
}

/// A request grounding the one closed-table capability on `lane`, addressed to
/// that lane's nickname so each lane activates its own identity.
fn grounding_request_for(lane: &str) -> routectl_core::ChatRequest {
    let mut req = routectl_core::ChatRequest {
        model: lane.to_string(),
        ..Default::default()
    };
    req.routectl_internal.anthropic_thinking_display = Some(WIRE_FIELD_VALUE.to_string());
    req
}

/// A fully-priced effective row whose confirmed output ceiling is `ceiling`.
///
/// Built through the public override merge, like its sibling fixtures: the
/// per-token rate fields are crate-visible on `CatalogRow`, so an operator's
/// `[cache_pricing]` shape is how this crate reaches them.
fn priced_row(ceiling: u32) -> routectl_router::EffectiveRow {
    let row = routectl_router::CatalogRow::sentinel()
        .with_overrides(&routectl_router::CachePricingOverride {
            input_cost_per_token: Some(3.0e-6),
            output_cost_per_token: Some(1.5e-5),
            max_output_tokens: Some(ceiling),
            ..Default::default()
        })
        .expect("a well-formed priced override merges cleanly");
    routectl_router::EffectiveRow::Present {
        row,
        source: routectl_router::Source::Baked,
        verified_at: "2026-01-01".to_string(),
    }
}

/// THE composed proof: the daemon's probe pass commits one unit through the real
/// file-backed writer, the router holds that committed acknowledgement BEFORE
/// the one outbound body is handed over, the unit is already durable on disk at
/// that instant, and the next pass finds the PERSISTED day spent and dials
/// nothing.
///
/// Every stage is the production one: the writer actor, the adapter this module
/// builds, `Router::run_probe_pass` as the driver tick calls it, and the seat's
/// own provider handle. The only doubles are the upstream -- doubling it is what
/// keeps the test hermetic -- and the ledger WRAPPER, which delegates every
/// reservation to that production adapter and only records when its answer
/// returned.
#[tokio::test]
async fn a_probe_pass_commits_through_the_real_writer_before_its_one_wire_call() {
    // Arrange: a one-unit provider-day, two activated lanes, an empty ledger.
    let rig = ComposedProbeRig::build(1, GENEROUS_OUTPUT_CEILING);
    let before = usage_control_rows(&rig.path);
    activate_both_lanes(&rig.router, &rig.provider).await;

    // Act: the pass the daemon's driver runs, once.
    let summary = rig.router.run_probe_pass().await;

    // Assert: the free half ran both lanes to exhaustion, surfacing candidates.
    assert_eq!(
        summary.free_validators_run, 2,
        "fixture premise: the pass must run both due free validators",
    );
    assert_eq!(
        rig.router.probe_scheduler_snapshot().free_exhausted_total,
        2,
        "fixture premise: each one-step free plan must exhaust, surfacing a paid \
         candidate",
    );
    assert!(
        summary.paid_probe_attempted,
        "the pass must attempt the paid half on a candidate the free half surfaced",
    );

    // THE ORDERING, read off the acknowledgement marker rather than off a
    // durable row that a lucky interleaving could have produced. Asserted over
    // EVERY paid call rather than over a single expected one, so a call
    // dispatched ahead of the boundary reds here -- naming the ordering --
    // instead of surfacing as a surprising call count further down.
    let observations = rig.provider.observations();
    assert!(
        !observations.is_empty(),
        "premise: the pass must have dialed, or the ordering assertion below is \
         free",
    );
    for (nth, observation) in observations.iter().enumerate() {
        assert!(
            observation.acknowledged,
            "paid call {nth} was dispatched before the router held a committed \
             reservation acknowledgement",
        );
    }
    assert_eq!(
        rig.provider.probe_calls(),
        1,
        "exactly one paid body must have reached the upstream",
    );

    // THE DURABLE UNIT, as it was readable from a second connection AT THE
    // INSTANT the outbound body was handed over: the acknowledgement the router
    // acted on described a unit already on disk, not one still in flight.
    let observed = added_control_rows(&before, &observations[0].rows);
    assert_eq!(
        observed.values().collect::<Vec<_>>(),
        vec!["1"],
        "when the call was made, the committed unit must ALREADY be durable and \
         readable by a connection the writer does not own: {observed:?}",
    );

    // THE BODY, built from the authorization's own seat and the retained payload.
    let body = rig.provider.only_probe_body();
    assert_eq!(
        body.model, WIRE_UPSTREAM,
        "the body must name the seat's upstream wire id, not the lane nickname",
    );
    assert_eq!(
        body.routectl_internal.anthropic_thinking_display.as_deref(),
        Some(WIRE_FIELD_VALUE),
        "the body must carry the field and value the admitted request grounded, \
         or the paid call asks nothing about the field",
    );
    let allowance = body.max_tokens.expect(
        "a paid body must carry the profile's own allowance, or the free path's \
         default fills it",
    );
    assert_ne!(
        allowance, FREE_PATH_DEFAULT_MAX_TOKENS,
        "the free path's default overwrote the catalog-derived allowance",
    );
    assert!(
        allowance <= GENEROUS_OUTPUT_CEILING,
        "the allowance must sit within the catalog-confirmed ceiling: {allowance}",
    );

    // THE MILESTONES, each recorded where it happened, agreeing with the one
    // call and the one committed unit.
    let snapshot = rig.router.probe_scheduler_snapshot();
    assert_eq!(snapshot.paid_candidate_attempts_total, 1);
    assert_eq!(snapshot.paid_reservations_committed_total, 1);
    assert_eq!(snapshot.paid_provider_calls_started_total, 1);
    assert_eq!(snapshot.paid_completed_total, 1);
    assert_eq!(
        snapshot.paid_authorization_refusals_total
            + snapshot.paid_gate_deferrals_total
            + snapshot.paid_provider_failed_total
            + snapshot.paid_timeouts_total,
        0,
        "the one pass settled as completed, so no other terminal counter may move",
    );

    // Act: a SECOND pass, against the same Router and the same database. The
    // other lane's candidate is still waiting, so there is something to claim --
    // and the only thing standing between it and a call is the day the first
    // pass durably spent.
    let second = rig.router.run_probe_pass().await;

    // Assert: the persisted cap refused it, and nothing reached the upstream.
    assert!(
        second.paid_probe_attempted,
        "premise: the second pass must claim the other lane's candidate, or its \
         zero calls would be an empty list rather than a spent budget",
    );
    assert_eq!(
        rig.provider.probe_calls(),
        1,
        "the exhausted provider-day must dial nothing: the cap is read from the \
         database the first pass wrote",
    );
    let after = added_control_rows(&before, &usage_control_rows(&rig.path));
    assert_eq!(
        after.values().collect::<Vec<_>>(),
        vec!["1"],
        "a refused reservation commits nothing, and the spent unit is never \
         refunded: {after:?}",
    );
    let snapshot = rig.router.probe_scheduler_snapshot();
    assert_eq!(
        snapshot.paid_candidate_attempts_total, 2,
        "the second claim is irreversible and counted",
    );
    assert_eq!(
        snapshot.paid_reservations_committed_total, 1,
        "no second unit was committed",
    );
    assert_eq!(
        snapshot.paid_provider_calls_started_total, 1,
        "no second call was dispatched",
    );
    assert_eq!(
        snapshot.paid_authorization_refusals_total, 1,
        "the refusal the accounting layer returned is the pass's settlement",
    );

    rig.stop();
}

/// The allowance on the wire is CATALOG-DERIVED: a ceiling one token below the
/// value the composed path put on the body admits no profile at all, so nothing
/// is committed and nothing is dialed.
///
/// This is what makes the body assertion above about a derivation rather than
/// about a number: the allowance is discovered by running the path, and then the
/// same path is rebuilt with the ceiling one below it. A body sized from anything
/// but the catalog-constrained floor would still dial here.
#[tokio::test]
async fn a_catalog_ceiling_one_below_the_bodys_allowance_dials_nothing() {
    // Arrange: learn the allowance the composed path actually sends.
    let generous = ComposedProbeRig::build(1, GENEROUS_OUTPUT_CEILING);
    activate_both_lanes(&generous.router, &generous.provider).await;
    generous.router.run_probe_pass().await;
    let allowance = generous
        .provider
        .only_probe_body()
        .max_tokens
        .expect("premise: the composed path must send an allowance to derive from");
    generous.stop();

    // Act: the same path, with the catalog confirming one token less.
    let starved = ComposedProbeRig::build(1, allowance - 1);
    let before = usage_control_rows(&starved.path);
    activate_both_lanes(&starved.router, &starved.provider).await;
    let summary = starved.router.run_probe_pass().await;

    // Assert: the candidate was claimed and refused before any accounting call.
    assert_eq!(
        summary.free_validators_run, 2,
        "premise: the free half must still run, so the refusal is the ceiling",
    );
    assert!(
        summary.paid_probe_attempted,
        "premise: a candidate must have been claimed, or zero calls is an empty list",
    );
    assert_eq!(
        starved.provider.probe_calls(),
        0,
        "a ceiling below the body's own allowance can carry no paid body",
    );
    assert!(
        added_control_rows(&before, &usage_control_rows(&starved.path)).is_empty(),
        "a refusal before the accounting call must commit no unit",
    );
    let snapshot = starved.router.probe_scheduler_snapshot();
    assert_eq!(snapshot.paid_reservations_committed_total, 0);
    assert_eq!(snapshot.paid_provider_calls_started_total, 0);
    assert_eq!(
        snapshot.paid_authorization_refusals_total, 1,
        "the pass settled as an authorization refusal",
    );

    starved.stop();
}
