//! `/status/health` panel: read-only per-dispatch-target circuit / RPM /
//! probe health plus the learned-capability negatives.
//!
//! The panel snapshots the live router through the read-only facade ONCE
//! per request (`state.router.view()`), reads per-target gate health and
//! the learned-negative registry from that single snapshot, and maps both
//! to a purpose-built DTO. The internal `CircuitPhase` enum is mapped to a
//! snake_case string OWNED HERE so a future breaker variant can never
//! silently change the wire shape. There is no network dial and no
//! mutation: `route_targets` is non-mutating by construction.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use routectl_core::failure_class::LastOutcome;
use routectl_router::LearnedRegistryEntry;
use routectl_router::router::RouteTargetStatus;
use routectl_router::runtime_state::CircuitPhase;

use super::field_verdict_log::{FidelityEmission, FidelityGate, log_field_verdict_snapshot};
use super::router_view::StatusRouterView;
use super::vocabulary::codes;
use super::wire_check_rows::{WireCheckRows, project as project_wire_check};
use super::{Panel, StatusState, guard_panel, now_utc_rfc3339};

/// Wire-shape version of the health panel payload.
pub const SCHEMA_VERSION: u32 = 6;

/// Per-target health plus learned negatives for the routing surface, and the
/// live wire check's rows inside the status window.
#[derive(Debug, Clone, Serialize)]
pub(super) struct HealthPanel {
    targets: Vec<TargetHealth>,
    learned_negatives: Vec<LearnedNegative>,
    wire_check: WireCheckRows,
}

/// One dispatch target's non-mutating gate health. Field names mirror the
/// event surface (`state_key`, `provider_name`, `upstream`, ...).
///
/// `state_key` is the runtime breaker / RPM key (`nickname[#member]`);
/// `learned_lane` is the `provider_entry#upstream` lane the target's
/// learned capability rows key on, so a client joins
/// `learned_negatives[].state_key` on it, never on `state_key`.
#[derive(Debug, Clone, Serialize)]
struct TargetHealth {
    state_key: String,
    /// The target's learned lane as dispatch mints it, or `None` for a
    /// provider entry carrying the reserved separator (refused by config
    /// validation), which learns nothing.
    learned_lane: Option<String>,
    nickname: String,
    provider_name: String,
    upstream: String,
    seat_label: Option<String>,
    /// Snake_case circuit phase owned by this module (see [`circuit_token`]).
    circuit: &'static str,
    /// Projected available RPM tokens; `None` under an unlimited policy.
    rpm_available: Option<f64>,
    half_open_probe_in_flight: bool,
    /// Epoch-ms the circuit last opened, or `None` when the circuit is closed
    /// (never tripped, or closed by a probe). Derived at DTO build from the
    /// gate's monotonic open-elapsed age; `None` is the honest closed state --
    /// never a `0`/epoch sentinel.
    open_since_ms: Option<i64>,
    /// Snake_case token of the most recent settled outcome (see
    /// [`last_outcome_token`]), or `None` before any dispatch has settled
    /// (fresh state / post-restart). Renders `circuit_open` when the circuit
    /// phase is open -- derived from the phase, never a stored value.
    last_outcome: Option<String>,
    /// Epoch-ms the last outcome was stamped, or `None` before any dispatch
    /// has settled. Derived from the gate's monotonic outcome-elapsed age;
    /// `None` is the honest never-seen state, never a `0`/epoch sentinel.
    last_outcome_at_ms: Option<i64>,
    /// Token of the reason the seat's credential is parked on (the core
    /// `UpstreamReason` vocabulary), or `None` when it is not parked.
    park_reason: Option<&'static str>,
    /// Epoch-ms the credential park lapses into a half-open re-test, or
    /// `None` when the seat is not parked. Derived from the park's monotonic
    /// remaining time against the same pinned clock as the other instants.
    parked_until_ms: Option<i64>,
}

/// One resident learned-capability row. Field names are the
/// `docs/LOGGING.md` contract tokens: `state_key`, `capability_key`
/// (the internal `feature_key`, RENAMED to the contract token),
/// `signal_tier`. `state_key` is the learned lane
/// (`provider_entry#upstream`) the row is keyed on -- the value a target's
/// `learned_lane` carries -- not a runtime target key. The registry is
/// verdict-discriminated, so a row is EITHER a learned negative OR a
/// VerifiedWorking positive; `verdict` distinguishes them, so a positive is
/// never mistaken for a negative that merely carries `phase=f3`.
#[derive(Debug, Clone, Serialize)]
struct LearnedNegative {
    state_key: String,
    capability_key: String,
    /// Read-model verdict token from the core `Verdict` (`verified` for a
    /// VerifiedWorking positive, `broken` for a learned negative).
    verdict: &'static str,
    signal_tier: &'static str,
    observations: u32,
    /// Detection-phase token (`f1`/`f2`/`f3`) from the core `FailurePhase`.
    phase: &'static str,
    /// Evidence-source token (`live`/`probe`) from the core `EvidenceSource`.
    source: &'static str,
    /// Epoch-ms the row was last observed. Derived from the entry's monotonic
    /// last-seen age against the single pinned build clock; a future-dated
    /// last-seen clamps the age to zero, never yielding a negative age.
    last_seen_ms: Option<i64>,
}

/// Map the internal `CircuitPhase` to its snake_case wire token. Owned by
/// the status module (not a `Serialize` derive on the router enum) so a new
/// breaker variant is a compile error here rather than a silent wire change.
const fn circuit_token(phase: CircuitPhase) -> &'static str {
    match phase {
        CircuitPhase::Closed => "closed",
        CircuitPhase::Open => "open",
        CircuitPhase::HalfOpenReady => "half_open_ready",
    }
}

/// Map a [`LastOutcome`] to its snake_case wire token. Owned by the status
/// module (not the router enum's `Serialize` derive) so a new outcome variant
/// is a compile error here rather than a silent wire change. A tripwire test
/// pins each token to the core enum's `serde` output.
const fn last_outcome_token(outcome: LastOutcome) -> &'static str {
    match outcome {
        LastOutcome::Ok => "ok",
        LastOutcome::RateLimited => "rate_limited",
        LastOutcome::Timeout => "timeout",
        LastOutcome::TransportError => "transport_error",
        LastOutcome::Http4xx => "http_4xx",
        LastOutcome::Http5xx => "http_5xx",
        LastOutcome::CircuitOpen => "circuit_open",
    }
}

/// Render the DTO's `last_outcome` token. An open circuit yields the derived
/// `circuit_open` (the stored value never holds it, per the runtime-state
/// contract); otherwise the stored outcome's token, or `None` when nothing
/// has settled yet.
fn last_outcome_wire(circuit: CircuitPhase, stored: Option<LastOutcome>) -> Option<String> {
    let outcome = match circuit {
        CircuitPhase::Open => Some(LastOutcome::CircuitOpen),
        CircuitPhase::Closed | CircuitPhase::HalfOpenReady => stored,
    };
    outcome.map(|o| last_outcome_token(o).to_string())
}

/// Convert a monotonic elapsed age into an epoch-ms instant relative to
/// `now_ms`. The caller keeps the `Option`, so a `None` age (closed /
/// never-seen) stays `None` -- never a `0`/epoch sentinel.
fn epoch_ms_of(now_ms: i64, elapsed: Duration) -> i64 {
    now_ms - i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
}

fn map_target(target: RouteTargetStatus, now_ms: i64) -> TargetHealth {
    let park = target.gate.credential_park;
    TargetHealth {
        state_key: target.state_key,
        learned_lane: target
            .learned_lane
            .as_ref()
            .map(|lane| lane.as_lane_key().to_string()),
        nickname: target.nickname,
        provider_name: target.provider_name,
        upstream: target.upstream,
        seat_label: target.seat_label,
        circuit: circuit_token(target.gate.circuit),
        rpm_available: target.gate.rpm_available,
        half_open_probe_in_flight: target.gate.half_open_probe_in_flight,
        open_since_ms: target
            .gate
            .circuit_open_elapsed
            .map(|d| epoch_ms_of(now_ms, d)),
        last_outcome: last_outcome_wire(target.gate.circuit, target.gate.last_outcome),
        last_outcome_at_ms: target
            .gate
            .last_outcome_elapsed
            .map(|d| epoch_ms_of(now_ms, d)),
        park_reason: park.map(|p| p.reason.as_str()),
        parked_until_ms: park.map(|p| {
            now_ms.saturating_add(i64::try_from(p.remaining.as_millis()).unwrap_or(i64::MAX))
        }),
    }
}

fn map_learned(entry: LearnedRegistryEntry, now: Instant, now_ms: i64) -> LearnedNegative {
    LearnedNegative {
        state_key: entry.state_key,
        capability_key: entry.feature_key,
        verdict: entry.verdict.as_str(),
        signal_tier: entry.signal_tier.as_str(),
        observations: entry.observations,
        phase: entry.phase.as_str(),
        source: entry.source.as_str(),
        last_seen_ms: Some(epoch_ms_of(
            now_ms,
            now.saturating_duration_since(entry.last_seen),
        )),
    }
}

/// Build the panel from ONE router snapshot. The single `view` drives both
/// reads, so the target health and the learned negatives are internally
/// consistent (no interleaved hot-swap).
///
/// `writer_degraded` is read by the caller, which holds the counters facade --
/// keeping this function a projection over the pinned snapshot.
fn build_from_view(
    view: &StatusRouterView,
    writer_degraded: bool,
    emission: FidelityEmission,
    gate: &FidelityGate,
) -> HealthPanel {
    // Pin a single monotonic read time and its epoch-ms anchor together, so
    // every target's elapsed-age conversion shares one clock reading.
    let now = Instant::now();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let targets = view
        .route_targets(now)
        .into_iter()
        .map(|target| map_target(target, now_ms))
        .collect();
    let entries = view.learned_capabilities();
    log_field_verdict_snapshot(view, writer_degraded, emission, gate);
    let learned_negatives = entries
        .into_iter()
        .map(|entry| map_learned(entry, now, now_ms))
        .collect();
    let now_unix_secs = u64::try_from(now_ms / 1000).unwrap_or(0);
    let wire_check = project_wire_check(&view.wire_check_snapshot(), now_unix_secs);
    HealthPanel {
        targets,
        learned_negatives,
        wire_check,
    }
}

pub(super) async fn build(state: &StatusState) -> Panel<HealthPanel> {
    build_with_emission(state, FidelityEmission::always()).await
}

/// The panel build, with the caller stating whether THIS build emits the shared
/// fidelity line.
///
/// Split so the `/status` aggregate -- which builds this panel AND the doctor panel
/// in one request, both of which carry the surface -- can emit once. An explicit
/// argument rather than a flag derived here, because only the caller knows what its
/// sibling builders did.
pub(super) async fn build_with_emission(
    state: &StatusState,
    emission: FidelityEmission,
) -> Panel<HealthPanel> {
    let view = state.router.view();
    // A counter read, not I/O, so it stays out here beside the router snapshot --
    // one read time for the whole build.
    let writer_degraded = state.usage_health.writer_degraded();
    let gate = Arc::clone(&state.fidelity_gate);
    // The snapshot is pinned at `view()`, so request time IS the read time.
    let as_of = now_utc_rfc3339();
    // The daemon's own fidelity observer, when a test installed one. Attached here
    // rather than passed in by the caller: the aggregate shares ONE emission across
    // both builders, so a caller-side attach would have to be repeated at every call
    // site and a missed one would silently stop observing that path.
    #[cfg(test)]
    let emission = emission.with_daemon_observer(state);
    // Fail BEFORE the fidelity emitter, which is what makes this the real degraded
    // shape: the aggregate's contract is that health leaving the shared claim
    // untouched lets doctor still satisfy the observability floor. A failure injected
    // after the emit would prove the opposite of what it looks like.
    #[cfg(test)]
    let fail = state.test_hooks.fail_health_builder;
    let panel = guard_panel(
        &state.builder_capacity,
        SCHEMA_VERSION,
        codes::DB_UNAVAILABLE,
        move || {
            #[cfg(test)]
            assert!(!fail, "injected health-builder failure");
            let dto = build_from_view(&view, writer_degraded, emission, &gate);
            Panel::available(SCHEMA_VERSION, as_of, dto)
        },
    )
    .await;
    state.observability.health.record(&panel);
    panel
}

pub(super) async fn handler(State(state): State<Arc<StatusState>>) -> Json<Panel<HealthPanel>> {
    Json(build(&state).await)
}

#[cfg(test)]
#[path = "health_lane_tests.rs"]
mod lane_tests;

#[cfg(test)]
#[path = "health_wire_check_tests.rs"]
mod wire_check_tests;

#[cfg(test)]
#[path = "health_tests.rs"]
mod tests;
