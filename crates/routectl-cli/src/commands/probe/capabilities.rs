//! `routectl probe --capabilities` -- an out-of-band capability probe.
//!
//! A LIB-SHAPED core ([`run_capability_probe`]) plus a thin CLI wrapper
//! ([`run`]). The core takes its dispatch dependency as a seam
//! ([`CanaryDispatch`]) and contains no clap, no TTY, and no network of its
//! own, so it is drivable end to end with a fake provider -- and so a later
//! import-validation surface can call it directly. It performs no ledger
//! write itself: the events it produces ride out on the report for the
//! caller to persist synchronously.
//!
//! # Structural isolation
//!
//! Dispatch goes through [`build_provider`] + `Provider::complete` ONLY --
//! never `Router::complete`/`stream` or any ingress handler. A bare provider
//! has no breaker debits, K samples, usage rollups, sticky writes, RPM
//! buckets, or learned-probe slots, so there is nothing to bypass and no flag
//! to forget: the isolation is a type boundary, not a runtime check. This
//! module never imports `Router`.
//!
//! # Emission
//!
//! Per canary outcome the core produces capability-ledger events (the wrapper
//! then writes them synchronously on a read-write connection -- the async
//! writer actor is a serving-path concern; a one-shot CLI has no response
//! path):
//!   - Verified -> `cleared(probe)` THEN `verified(probe)`, the cleared event
//!     stamped strictly earlier so replay lands the clear before it admits the
//!     positive (the probe IS the forget verb; a cleared with no resident
//!     negative is a harmless no-op).
//!   - SuspectAbsence -> `suspect(probe)`, replaying as an F3+Probe negative.
//!   - A deterministic capability-naming rejection -> `broken(probe)` at
//!     F1/F2, attributed by the SAME [`resolve_requested_capability`] matcher
//!     the live learn path uses.
//!   - A clean-stop-gate reject, or any transport / availability failure
//!     (429 / timeout / 5xx / auth / network) -> NO event.
//!
//! Every event stamps `(catalog_version, overlay_revision)` computed the same
//! way the serve/reload boundary does (the baked `CATALOG_VERSION` const plus
//! [`routectl_router::overlay_revision`] on the loaded overlay). A stamp that
//! disagreed with the daemon's boot tombstone would make `should_replay`
//! silently drop every probe event, so the stamp is load-bearing.
//!
//! # Within-run lane health
//!
//! A 429 / timeout / 5xx / auth / network failure on any canary marks the lane
//! unhealthy; the remaining capability cells render "skipped: lane unhealthy"
//! rather than dispatch into a lane that is down. A capability-level 400 is
//! evidence, not lane health, so it never trips the lane.

use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use routectl_core::capability::{
    EvidenceSource, FailurePhase, PROMPT_CACHING, STRUCTURED_OUTPUT, THINKING, Verdict, WEB_SEARCH,
};
use routectl_core::failure_class::{FailureClass, classify};
use routectl_core::{ChatRequest, ChatResponse, Provider};
use routectl_router::{
    CATALOG_VERSION, DetectorContext, ObservationDirection, build_provider, detect,
    overlay_revision, resolve_requested_capability,
};
use routectl_usage::{CapabilityEvent, Rates};

use super::canary::{
    PROBE_PROFILE_V1, ProbeProfileV1, prompt_caching_canary, structured_output_canary,
    thinking_canary, web_search_canary,
};

/// One capability the probe can exercise, each a single lane cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeCapability {
    /// Constrained decoding via a strict `json_schema` output.
    StructuredOutput,
    /// Forced single web search.
    WebSearch,
    /// Prompt-cache prime + read pair.
    PromptCaching,
    /// Extended thinking / reasoning.
    Thinking,
}

impl ProbeCapability {
    /// Every capability, in the fixed order the probe iterates them.
    pub const ALL: [Self; 4] = [
        Self::StructuredOutput,
        Self::WebSearch,
        Self::PromptCaching,
        Self::Thinking,
    ];

    /// The canonical capability key this cell attests to -- the key the
    /// detector emits, the ledger records, and the registry replays.
    pub const fn capability_key(self) -> &'static str {
        match self {
            Self::StructuredOutput => STRUCTURED_OUTPUT,
            Self::WebSearch => WEB_SEARCH,
            Self::PromptCaching => PROMPT_CACHING,
            Self::Thinking => THINKING,
        }
    }

    /// Parse an operator `--only` token (the capability key) into a
    /// capability, or `None` when it names no known capability.
    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.capability_key() == token)
    }

    /// How many canary calls this cell costs under `profile`.
    const fn canary_count(self, profile: &ProbeProfileV1) -> u8 {
        match self {
            Self::StructuredOutput => profile.structured_output_canaries,
            Self::WebSearch => profile.web_search_canaries,
            Self::PromptCaching => profile.prompt_caching_canaries,
            Self::Thinking => profile.thinking_canaries,
        }
    }

    /// Build this cell's dispatch plan: the request(s) to send and the
    /// detector context their response classifies through.
    fn dispatch_plan(self, model: &str) -> CanaryDispatchPlan {
        match self {
            Self::StructuredOutput => single(structured_output_canary(model)),
            Self::WebSearch => single(web_search_canary(model)),
            Self::Thinking => single(thinking_canary(model)),
            Self::PromptCaching => {
                let canary = prompt_caching_canary(model);
                CanaryDispatchPlan::Pair {
                    prime: Box::new(canary.prime),
                    read: Box::new(canary.read),
                    context: canary.context,
                }
            }
        }
    }
}

/// A cell's dispatch shape: one request, or the caching prime/read pair. The
/// requests are boxed -- a `ChatRequest` is large, and boxing keeps the enum
/// variants a uniform, small size.
enum CanaryDispatchPlan {
    Single {
        request: Box<ChatRequest>,
        context: DetectorContext,
    },
    Pair {
        prime: Box<ChatRequest>,
        read: Box<ChatRequest>,
        context: DetectorContext,
    },
}

fn single(canary: super::canary::Canary) -> CanaryDispatchPlan {
    CanaryDispatchPlan::Single {
        request: Box::new(canary.request),
        context: canary.context,
    }
}

/// The scoping + stamping inputs the core needs, resolved by the caller.
#[derive(Debug, Clone)]
pub struct CapabilityProbePlan {
    /// Routing state key (the `[models]` nickname) -- the ledger lane key.
    pub state_key: String,
    /// Provider-kind token, for the rejection matcher and normalization.
    pub provider_kind: String,
    /// Upstream model id every canary request targets.
    pub model: String,
    /// Baked catalog version stamped on every event (boot-boundary parity).
    pub catalog_version: i64,
    /// Catalog-overlay revision stamped on every event (boot-boundary parity).
    pub overlay_revision: i64,
    /// Rates for the pre-dispatch cost estimate, or `None` when unpriced.
    pub rates: Option<Rates>,
}

/// The outcome of one lane cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellOutcome {
    /// Structural proof the capability worked -- `cleared` + `verified` written.
    Verified,
    /// Requested/forced but the evidence was absent on a clean stop --
    /// `suspect` written.
    SuspectAbsence,
    /// A deterministic rejection named a capability -- `broken` written.
    Broken {
        /// The detection phase the matcher attributed.
        phase: FailurePhase,
        /// The canonical capability the rejection named.
        capability: String,
    },
    /// A clean but evidence-free response, or a rejection naming no capability
    /// -- no event written, nothing learned.
    Inconclusive,
    /// This canary's transport / availability failure tripped the lane; no
    /// event written and the remaining cells skip.
    Unhealthy {
        /// The failure-class token that tripped the lane.
        class: &'static str,
    },
    /// The lane was already unhealthy when this cell came up.
    SkippedLaneUnhealthy,
}

/// One resolved lane cell: the capability and its outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneCellResult {
    /// The capability this cell probed.
    pub capability: ProbeCapability,
    /// What the probe found.
    pub outcome: CellOutcome,
}

/// The pre-dispatch cost estimate, driven entirely by the baked profile.
#[derive(Debug, Clone, PartialEq)]
pub struct ProbeEstimate {
    /// Total canary calls across the selected capabilities.
    pub total_calls: u32,
    /// The baked per-call output-token ceiling.
    pub max_tokens: u32,
    /// Upper-bound output tokens: `total_calls * max_tokens`.
    pub estimated_output_tokens: i64,
    /// The estimated cost, or `None` when the target is unpriced.
    pub cost: Option<routectl_usage::CostBreakdown>,
    /// Per-capability canary-call breakdown.
    pub per_capability: Vec<(ProbeCapability, u32)>,
}

/// The full probe report: the estimate, one cell per capability, and the
/// capability events to persist (in write order -- a `cleared` always precedes
/// its `verified`). Dispatch/classification is decoupled from persistence: the
/// core produces the events, the caller writes them synchronously.
#[derive(Debug, Clone)]
pub struct CapabilityProbeReport {
    /// The lane (routing state key) that was probed.
    pub state_key: String,
    /// The pre-dispatch cost estimate.
    pub estimate: ProbeEstimate,
    /// One cell per probed capability, in probe order.
    pub cells: Vec<LaneCellResult>,
    /// Capability events to persist, in write order.
    pub events: Vec<CapabilityEvent>,
}

/// One-shot completion seam. Kept narrow (only `complete`) so the core is
/// drivable with a trivial fake in tests, while the production impl forwards
/// to a bare `Provider` -- the structural-isolation boundary.
#[async_trait]
pub trait CanaryDispatch: Send + Sync {
    /// Run one canary request to completion.
    async fn complete(&self, req: ChatRequest) -> routectl_core::Result<ChatResponse>;
}

#[async_trait]
impl CanaryDispatch for Arc<dyn Provider> {
    async fn complete(&self, req: ChatRequest) -> routectl_core::Result<ChatResponse> {
        Provider::complete(self.as_ref(), req).await
    }
}

/// Estimate the probe's cost from the baked profile and the model's rates.
/// Always computed BEFORE any dispatch: `total_calls * max_tokens` output
/// tokens priced against `rates` (input treated as zero -- the baked ceiling
/// bounds output, which is what the operator is being asked to authorize).
/// An unpriced target yields `cost: None`.
pub fn estimate_probe_cost(
    capabilities: &[ProbeCapability],
    rates: Option<&Rates>,
) -> ProbeEstimate {
    let profile = PROBE_PROFILE_V1;
    let mut per_capability = Vec::with_capacity(capabilities.len());
    let mut total_calls: u32 = 0;
    for &cap in capabilities {
        let calls = u32::from(cap.canary_count(&profile));
        per_capability.push((cap, calls));
        total_calls += calls;
    }
    let estimated_output_tokens = i64::from(total_calls) * i64::from(profile.max_tokens);
    let cost = rates.and_then(|r| {
        routectl_usage::estimate_cost_tokens(0, estimated_output_tokens, 0, 0, 0, 0, r)
    });
    ProbeEstimate {
        total_calls,
        max_tokens: profile.max_tokens,
        estimated_output_tokens,
        cost,
        per_capability,
    }
}

/// One cell's classified outcome plus the events it produced (empty for a
/// cell that learned nothing).
struct CellProbe {
    outcome: CellOutcome,
    events: Vec<CapabilityEvent>,
}

/// Run the capability probe end to end against one resolved lane.
///
/// Computes the estimate first, then iterates the capabilities in fixed order:
/// each cell dispatches its canary through `dispatcher`, classifies the
/// outcome, and collects any resulting events. The loop short-circuits the
/// remaining cells once a transport / availability failure marks the lane
/// unhealthy. This performs NO ledger write itself -- the events ride out on
/// the report for the caller to persist synchronously -- and touches no
/// network, config, router state, or clock beyond the event timestamps.
pub async fn run_capability_probe(
    dispatcher: &dyn CanaryDispatch,
    plan: &CapabilityProbePlan,
    capabilities: &[ProbeCapability],
) -> CapabilityProbeReport {
    let estimate = estimate_probe_cost(capabilities, plan.rates.as_ref());
    let mut cells = Vec::with_capacity(capabilities.len());
    let mut events = Vec::new();
    let mut lane_unhealthy = false;
    for &capability in capabilities {
        let outcome = if lane_unhealthy {
            CellOutcome::SkippedLaneUnhealthy
        } else {
            let cell = probe_one(dispatcher, plan, capability, &mut lane_unhealthy).await;
            events.extend(cell.events);
            cell.outcome
        };
        cells.push(LaneCellResult {
            capability,
            outcome,
        });
    }
    CapabilityProbeReport {
        state_key: plan.state_key.clone(),
        estimate,
        cells,
        events,
    }
}

/// Probe one capability cell: dispatch its canary (the caching cell primes
/// then reads), then classify the success or failure.
async fn probe_one(
    dispatcher: &dyn CanaryDispatch,
    plan: &CapabilityProbePlan,
    capability: ProbeCapability,
    lane_unhealthy: &mut bool,
) -> CellProbe {
    match capability.dispatch_plan(&plan.model) {
        CanaryDispatchPlan::Single { request, context } => {
            match dispatcher.complete(*request).await {
                Ok(resp) => classify_success(plan, capability, &context, &resp),
                Err(err) => classify_failure(plan, &err, lane_unhealthy),
            }
        }
        CanaryDispatchPlan::Pair {
            prime,
            read,
            context,
        } => {
            if let Err(err) = dispatcher.complete(*prime).await {
                return classify_failure(plan, &err, lane_unhealthy);
            }
            match dispatcher.complete(*read).await {
                Ok(resp) => classify_success(plan, capability, &context, &resp),
                Err(err) => classify_failure(plan, &err, lane_unhealthy),
            }
        }
    }
}

/// Classify a successful canary response through the SAME `detect` path the
/// live observe uses, scoped to the capability under test so a stray
/// cross-capability signal never leaks into this cell.
fn classify_success(
    plan: &CapabilityProbePlan,
    capability: ProbeCapability,
    context: &DetectorContext,
    resp: &ChatResponse,
) -> CellProbe {
    let observation = detect(context, resp)
        .into_iter()
        .find(|obs| obs.capability_key == capability.capability_key());
    let Some(obs) = observation else {
        return CellProbe {
            outcome: CellOutcome::Inconclusive,
            events: Vec::new(),
        };
    };
    match obs.direction {
        ObservationDirection::Verified => emit_verified(plan, &obs),
        ObservationDirection::SuspectAbsence => emit_suspect(plan, &obs),
    }
}

/// Build the resurrect-proof `cleared` -> `verified` pair. The cleared event is
/// stamped strictly earlier so replay lands the clear (removing any resident
/// negative) before admitting the positive; the tie-break would fall out of
/// insertion order too, but the strict timestamp pins it independently.
fn emit_verified(
    plan: &CapabilityProbePlan,
    obs: &routectl_router::CapabilityObservation,
) -> CellProbe {
    let ts = epoch_ms_now();
    let cleared = event(
        plan,
        ts,
        obs.capability_key,
        Verdict::Cleared.as_str(),
        "",
        "",
        None,
    );
    let verified = event(
        plan,
        ts + 1,
        obs.capability_key,
        Verdict::VerifiedWorking.as_str(),
        FailurePhase::F3.as_str(),
        obs.tier.as_str(),
        Some(obs.evidence_class.to_string()),
    );
    CellProbe {
        outcome: CellOutcome::Verified,
        events: vec![cleared, verified],
    }
}

/// Build a `suspect` (F3+Probe) negative for a suspected-absence observation.
fn emit_suspect(
    plan: &CapabilityProbePlan,
    obs: &routectl_router::CapabilityObservation,
) -> CellProbe {
    let suspect = event(
        plan,
        epoch_ms_now(),
        obs.capability_key,
        Verdict::SuspectIgnored.as_str(),
        FailurePhase::F3.as_str(),
        obs.tier.as_str(),
        Some(obs.evidence_class.to_string()),
    );
    CellProbe {
        outcome: CellOutcome::SuspectAbsence,
        events: vec![suspect],
    }
}

/// Classify a canary failure: attribute a deterministic capability-naming
/// rejection to a `broken` event via the shared live matcher; otherwise mark
/// the lane unhealthy on a transport / availability failure (a capability-level
/// 400 is capability evidence, never lane health), or record an inconclusive cell.
fn classify_failure(
    plan: &CapabilityProbePlan,
    err: &routectl_core::Error,
    lane_unhealthy: &mut bool,
) -> CellProbe {
    let classified = classify(err, Some(&plan.provider_kind));
    if let Some((capability, tier, phase)) =
        resolve_requested_capability(&plan.provider_kind, err, &classified)
    {
        let broken = event(
            plan,
            epoch_ms_now(),
            &capability,
            Verdict::LearnedBroken(phase).as_str(),
            phase.as_str(),
            tier.as_str(),
            None,
        );
        return CellProbe {
            outcome: CellOutcome::Broken { phase, capability },
            events: vec![broken],
        };
    }
    if is_lane_health_failure(&classified.class) {
        *lane_unhealthy = true;
        return CellProbe {
            outcome: CellOutcome::Unhealthy {
                class: classified.class.class_token().unwrap_or("unknown"),
            },
            events: Vec::new(),
        };
    }
    CellProbe {
        outcome: CellOutcome::Inconclusive,
        events: Vec::new(),
    }
}

/// Whether a failure class reflects the lane's availability rather than a
/// per-capability request fault. Rate limits, server errors, overload,
/// timeouts, and transport errors are lane health; a dead credential (`Auth`)
/// blocks every subsequent canary identically, so it too fails the lane. A
/// capability-level 400 (`BadRequest` / `FeatureUnsupported` / policy /
/// context) is evidence, not lane health, and never trips the lane.
const fn is_lane_health_failure(class: &FailureClass) -> bool {
    matches!(
        class,
        FailureClass::RateLimited
            | FailureClass::ServerError
            | FailureClass::Overloaded
            | FailureClass::Timeout
            | FailureClass::NetworkError
            | FailureClass::Auth
    )
}

/// Build one capability event stamped with the plan's lane key and revision.
/// `source` is always the probe. Never carries a body / message / prompt.
fn event(
    plan: &CapabilityProbePlan,
    ts: i64,
    capability: &str,
    verdict: &str,
    phase: &str,
    tier: &str,
    evidence_class: Option<String>,
) -> CapabilityEvent {
    CapabilityEvent {
        ts,
        lane_key: plan.state_key.clone(),
        capability: capability.to_string(),
        verdict: verdict.to_string(),
        phase: phase.to_string(),
        source: EvidenceSource::Probe.as_str().to_string(),
        tier: tier.to_string(),
        evidence_class,
        upstream_token: None,
        catalog_version: plan.catalog_version,
        overlay_revision: plan.overlay_revision,
    }
}

/// Current wall-clock time in epoch milliseconds, saturating rather than
/// panicking on a pre-epoch or overflowing clock.
fn epoch_ms_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

// --- CLI wrapper -------------------------------------------------------

/// Run `probe --capabilities` against `config_path` and render the report.
///
/// Loads config + overlay, resolves the scoped target, computes and prints
/// the estimate BEFORE any dispatch, gates on operator confirmation (unless
/// `assume_yes`), then opens the existing usage DB read-write (never creating
/// or migrating it), builds a bare provider, and runs the lib core. Returns
/// the process exit code: nonzero only on a setup failure (bad target,
/// unbuildable provider, or a missing / unmigrated ledger), zero for any
/// completed probe regardless of what it found.
pub async fn run(
    config_path: &Path,
    provider: Option<String>,
    alias: Option<String>,
    only: &[String],
    assume_yes: bool,
    json: bool,
) -> i32 {
    let loaded = match crate::server::load_effective_config_unvalidated(config_path) {
        Ok(loaded) => loaded,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    let target = match super::resolve::resolve_probe_target(
        &loaded.config,
        provider.as_deref(),
        alias.as_deref(),
    ) {
        Ok(target) => target,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    let entry = match loaded.config.providers.get(&target.provider) {
        Some(entry) => entry,
        None => {
            eprintln!(
                "error: no provider named `{}` is configured",
                target.provider
            );
            return 1;
        }
    };

    let capabilities = match select_capabilities(only) {
        Ok(caps) => caps,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    let rates = loaded
        .config
        .pricing_for(&target.model_id, &target.provider)
        .map(rates_from_pricing);
    let estimate = estimate_probe_cost(&capabilities, rates.as_ref());
    for line in render_estimate(&estimate) {
        println!("{line}");
    }

    if !assume_yes && !confirm_after_estimate() {
        println!("aborted; no probe dispatched");
        return 0;
    }

    dispatch_and_persist(&loaded, entry, &target, &capabilities, rates, json).await
}

/// Build the bare provider, open the ledger read-write, run the scoped probe,
/// persist its events synchronously, and render the report. Returns the
/// process exit code: nonzero only on a ledger-open / build / write failure --
/// never on what the probe itself found. Shared by the `probe --capabilities`
/// verb and the post-add wizard offer so both dispatch, persist, and render
/// through one path.
async fn dispatch_and_persist(
    loaded: &crate::server::LoadedConfig,
    entry: &routectl_router::ProviderEntry,
    target: &super::resolve::ResolvedProbeTarget,
    capabilities: &[ProbeCapability],
    rates: Option<Rates>,
    json: bool,
) -> i32 {
    let db = match routectl_usage::open_rw(&loaded.config.usage.db_path) {
        Ok(db) => db,
        Err(e) => {
            eprintln!(
                "error: cannot open the usage ledger read-write at {}: {e}",
                loaded.config.usage.db_path.display()
            );
            eprintln!("       start the service once so it creates and migrates the ledger");
            return 1;
        }
    };

    let store = match crate::server::CompositeStore::open_default().await {
        Ok(store) => store,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };
    let secrets: Arc<dyn routectl_auth::SecretStore> = Arc::new(store);
    let dispatcher = match build_provider(&target.provider, entry, secrets).await {
        Ok(provider) => provider,
        Err(e) => {
            eprintln!("error: could not build provider `{}`: {e}", target.provider);
            return 1;
        }
    };

    let plan = CapabilityProbePlan {
        state_key: target.state_key.clone(),
        provider_kind: entry.kind_str().to_string(),
        model: target.model_id.clone(),
        catalog_version: i64::from(CATALOG_VERSION),
        overlay_revision: i64::try_from(overlay_revision(&loaded.catalog_overlay))
            .unwrap_or(i64::MAX),
        rates,
    };

    let report = run_capability_probe(&dispatcher, &plan, capabilities).await;

    // Persist the events synchronously on the read-write connection -- the
    // CLI is the writer here (no serving-path async writer actor). A write
    // failure is surfaced but does not discard the rendered findings.
    let mut write_failed = false;
    for probe_event in &report.events {
        if let Err(e) = routectl_usage::insert_capability_event(db.conn(), probe_event) {
            write_failed = true;
            eprintln!("error: failed to persist a capability event: {e}");
        }
    }

    if json {
        match serde_json::to_string_pretty(&report_json(&report)) {
            Ok(text) => println!("{text}"),
            Err(e) => {
                eprintln!("error: failed to serialize probe report: {e}");
                return 1;
            }
        }
    } else {
        for line in render_report(&report) {
            println!("{line}");
        }
    }
    i32::from(write_failed)
}

/// Offer a scoped capability probe for a provider that was JUST added, driven
/// by the caller's `confirm` seam. Loads the effective config, resolves the
/// provider's single routable lane, prints the cost line, then consults
/// `confirm`; on a yes it dispatches the probe and persists what it finds.
///
/// Silently does nothing -- never an error, never a panic -- when the config
/// cannot be loaded or the provider has no single selectable model to scope to
/// yet. A probe is a bonus after a successful add, not a precondition for it,
/// so a lane that cannot be resolved simply skips the offer. The probe touches
/// only the capability ledger and never rewrites config or credentials, so it
/// cannot undo the add that preceded it.
pub async fn offer_scoped_probe(
    config_path: &Path,
    provider: &str,
    confirm: impl FnOnce(&ProbeEstimate) -> bool,
) {
    let Ok(loaded) = crate::server::load_effective_config_unvalidated(config_path) else {
        return;
    };
    let Ok(target) = super::resolve::resolve_probe_target(&loaded.config, Some(provider), None)
    else {
        return;
    };
    let Some(entry) = loaded.config.providers.get(&target.provider) else {
        return;
    };

    let capabilities = ProbeCapability::ALL.to_vec();
    let rates = loaded
        .config
        .pricing_for(&target.model_id, &target.provider)
        .map(rates_from_pricing);
    let estimate = estimate_probe_cost(&capabilities, rates.as_ref());
    for line in render_estimate(&estimate) {
        println!("{line}");
    }

    if !confirm(&estimate) {
        return;
    }

    dispatch_and_persist(&loaded, entry, &target, &capabilities, rates, false).await;
}

/// Resolve the `--only` tokens to a capability set, or all capabilities when
/// none were given. An unknown token is an actionable error naming the value.
fn select_capabilities(only: &[String]) -> Result<Vec<ProbeCapability>, String> {
    if only.is_empty() {
        return Ok(ProbeCapability::ALL.to_vec());
    }
    only.iter()
        .map(|token| {
            ProbeCapability::from_token(token)
                .ok_or_else(|| format!("unknown capability `{token}` in --only"))
        })
        .collect()
}

/// Convert the router's per-million-token pricing into the usage crate's
/// leaf-safe `Rates`.
const fn rates_from_pricing(pricing: &routectl_router::PricingConfig) -> Rates {
    Rates {
        input_per_mtok: pricing.input_per_mtok,
        output_per_mtok: pricing.output_per_mtok,
        cache_read_per_mtok: pricing.cache_read_per_mtok,
        cache_write_5m_per_mtok: pricing.cache_write_5m_per_mtok,
        cache_write_1h_per_mtok: pricing.cache_write_1h_per_mtok,
    }
}

/// Prompt for confirmation after the estimate. A `y`/`yes` reply proceeds;
/// anything else (including EOF or a read error on a non-interactive stdin)
/// declines, so the probe never dispatches without an explicit go-ahead.
fn confirm_after_estimate() -> bool {
    use std::io::Write;
    print!("proceed with the probe? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

// --- rendering ---------------------------------------------------------

/// Human-readable estimate lines.
fn render_estimate(estimate: &ProbeEstimate) -> Vec<String> {
    let cost = match &estimate.cost {
        Some(breakdown) => format!("~${:.4}", breakdown.total_usd),
        None => "unpriced".to_string(),
    };
    vec![format!(
        "estimate: {} canary calls, up to {} output tokens, {}",
        estimate.total_calls, estimate.estimated_output_tokens, cost
    )]
}

/// Human-readable report: a header, the estimate, and one line per cell.
fn render_report(report: &CapabilityProbeReport) -> Vec<String> {
    let mut out = vec![format!("capability probe: {}", report.state_key)];
    out.extend(render_estimate(&report.estimate));
    for cell in &report.cells {
        out.push(format!(
            "  {}: {}",
            cell.capability.capability_key(),
            cell_label(&cell.outcome)
        ));
    }
    out
}

/// Operator-facing label for one cell outcome.
fn cell_label(outcome: &CellOutcome) -> String {
    match outcome {
        CellOutcome::Verified => "verified".to_string(),
        CellOutcome::SuspectAbsence => "suspected absent".to_string(),
        CellOutcome::Broken { phase, capability } => {
            format!("broken ({}: {capability})", phase.as_str())
        }
        CellOutcome::Inconclusive => "inconclusive".to_string(),
        CellOutcome::Unhealthy { class } => format!("lane unhealthy ({class})"),
        CellOutcome::SkippedLaneUnhealthy => "skipped: lane unhealthy".to_string(),
    }
}

/// UNSTABLE `--json` report schema version. Bumped when the shape changes.
const JSON_SCHEMA_VERSION: u32 = 1;

/// The `--json` report body.
fn report_json(report: &CapabilityProbeReport) -> serde_json::Value {
    serde_json::json!({
        "schema_version": JSON_SCHEMA_VERSION,
        "lane": report.state_key,
        "estimate": {
            "total_calls": report.estimate.total_calls,
            "max_tokens": report.estimate.max_tokens,
            "estimated_output_tokens": report.estimate.estimated_output_tokens,
            "cost_usd": report.estimate.cost.as_ref().map(|c| c.total_usd),
        },
        "cells": report.cells.iter().map(|cell| serde_json::json!({
            "capability": cell.capability.capability_key(),
            "outcome": cell_label(&cell.outcome),
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
#[path = "capabilities_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "capability_acceptance_tests.rs"]
mod acceptance;
