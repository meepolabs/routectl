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

use super::router_view::StatusRouterView;
use super::vocabulary::codes;
use super::{Panel, StatusState, guard_panel, now_utc_rfc3339};

/// Wire-shape version of the health panel payload.
pub const SCHEMA_VERSION: u32 = 5;

/// Per-target health plus learned negatives for the routing surface.
#[derive(Debug, Clone, Serialize)]
pub(super) struct HealthPanel {
    targets: Vec<TargetHealth>,
    learned_negatives: Vec<LearnedNegative>,
}

/// One dispatch target's non-mutating gate health. Field names mirror the
/// event surface (`state_key`, `provider_name`, `upstream`, ...).
#[derive(Debug, Clone, Serialize)]
struct TargetHealth {
    state_key: String,
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
}

/// One resident learned-capability row. Field names are the
/// `docs/LOGGING.md` contract tokens: `state_key`, `capability_key`
/// (the internal `feature_key`, RENAMED to the contract token),
/// `signal_tier`. The registry is verdict-discriminated, so a row is
/// EITHER a learned negative OR a VerifiedWorking positive; `verdict`
/// distinguishes them, so a positive is never mistaken for a negative
/// that merely carries `phase=f3`.
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
    TargetHealth {
        state_key: target.state_key,
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
fn build_from_view(view: &StatusRouterView) -> HealthPanel {
    // Pin a single monotonic read time and its epoch-ms anchor together, so
    // every target's elapsed-age conversion shares one clock reading.
    let now = Instant::now();
    let now_ms = chrono::Utc::now().timestamp_millis();
    let targets = view
        .route_targets(now)
        .into_iter()
        .map(|target| map_target(target, now_ms))
        .collect();
    let learned_negatives = view
        .learned_capabilities()
        .into_iter()
        .map(|entry| map_learned(entry, now, now_ms))
        .collect();
    HealthPanel {
        targets,
        learned_negatives,
    }
}

pub(super) async fn build(state: &StatusState) -> Panel<HealthPanel> {
    let view = state.router.view();
    // The snapshot is pinned at `view()`, so request time IS the read time.
    let as_of = now_utc_rfc3339();
    let panel = guard_panel(SCHEMA_VERSION, codes::DB_UNAVAILABLE, move || {
        let dto = build_from_view(&view);
        Panel::available(SCHEMA_VERSION, as_of, dto)
    })
    .await;
    state.observability.health.record(&panel);
    panel
}

pub(super) async fn handler(State(state): State<Arc<StatusState>>) -> Json<Panel<HealthPanel>> {
    Json(build(&state).await)
}

#[cfg(test)]
mod tests {
    use super::super::vocabulary;
    use super::*;
    use crate::server::AppState;
    use arc_swap::ArcSwap;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier, Verdict};
    use routectl_router::runtime_state::ProviderGateStatus;
    use routectl_router::{Config, Router};
    use serde_json::Value;
    use tower::ServiceExt;

    fn test_state() -> Arc<StatusState> {
        let router = Router::new(Arc::new(Config::default()));
        let (app, _dir) = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
        Arc::new(StatusState::from_app(&app, None))
    }

    fn sample_target(circuit: CircuitPhase) -> RouteTargetStatus {
        RouteTargetStatus {
            state_key: "opus#seat-a".into(),
            nickname: "opus".into(),
            provider_name: "anthropic".into(),
            upstream: "claude-opus-wire".into(),
            seat_label: Some("seat-a".into()),
            gate: ProviderGateStatus {
                rpm_available: Some(12.0),
                circuit,
                half_open_probe_in_flight: true,
                circuit_open_elapsed: None,
                last_outcome: None,
                last_outcome_elapsed: None,
            },
        }
    }

    /// A target with the outcome/open ages populated for the epoch-ms and
    /// derivation tests.
    fn target_with_ages(
        circuit: CircuitPhase,
        circuit_open_elapsed: Option<Duration>,
        last_outcome: Option<LastOutcome>,
        last_outcome_elapsed: Option<Duration>,
    ) -> RouteTargetStatus {
        RouteTargetStatus {
            state_key: "opus#seat-a".into(),
            nickname: "opus".into(),
            provider_name: "anthropic".into(),
            upstream: "claude-opus-wire".into(),
            seat_label: Some("seat-a".into()),
            gate: ProviderGateStatus {
                rpm_available: Some(12.0),
                circuit,
                half_open_probe_in_flight: false,
                circuit_open_elapsed,
                last_outcome,
                last_outcome_elapsed,
            },
        }
    }

    #[test]
    fn circuit_token_pins_every_phase_to_snake_case() {
        assert_eq!(circuit_token(CircuitPhase::Closed), "closed");
        assert_eq!(circuit_token(CircuitPhase::Open), "open");
        assert_eq!(
            circuit_token(CircuitPhase::HalfOpenReady),
            "half_open_ready"
        );
    }

    /// Tripwire: the module-owned `last_outcome_token` must match the core
    /// enum's `serde` output for EVERY variant, byte for byte. A rename on
    /// either side drifts the wire away from the taxonomy and trips here.
    #[test]
    fn last_outcome_token_matches_core_enum_serde() {
        for outcome in [
            LastOutcome::Ok,
            LastOutcome::RateLimited,
            LastOutcome::Timeout,
            LastOutcome::TransportError,
            LastOutcome::Http4xx,
            LastOutcome::Http5xx,
            LastOutcome::CircuitOpen,
        ] {
            let serde_token = serde_json::to_value(outcome).unwrap();
            assert_eq!(
                Value::String(last_outcome_token(outcome).to_string()),
                serde_token,
                "token drift for {outcome:?}"
            );
        }
    }

    #[test]
    fn open_circuit_derives_circuit_open_and_open_since() {
        let target = target_with_ages(
            CircuitPhase::Open,
            Some(Duration::from_millis(400)),
            Some(LastOutcome::Http5xx),
            Some(Duration::from_millis(400)),
        );
        let mapped = map_target(target, 10_000);
        // The phase overrides the stored kind: an open circuit renders
        // `circuit_open`, never the stored failure family.
        assert_eq!(mapped.last_outcome.as_deref(), Some("circuit_open"));
        assert_eq!(mapped.open_since_ms, Some(9_600));
        assert_eq!(mapped.last_outcome_at_ms, Some(9_600));
    }

    #[test]
    fn closed_never_seen_target_has_all_none() {
        let target = target_with_ages(CircuitPhase::Closed, None, None, None);
        let mapped = map_target(target, 10_000);
        assert_eq!(mapped.open_since_ms, None);
        assert_eq!(mapped.last_outcome, None);
        assert_eq!(mapped.last_outcome_at_ms, None);
    }

    #[test]
    fn closed_target_renders_stored_outcome_not_circuit_open() {
        let target = target_with_ages(
            CircuitPhase::Closed,
            None,
            Some(LastOutcome::RateLimited),
            Some(Duration::from_secs(2)),
        );
        let mapped = map_target(target, 10_000);
        // Closed circuit: the stored outcome surfaces verbatim, open_since
        // stays None.
        assert_eq!(mapped.last_outcome.as_deref(), Some("rate_limited"));
        assert_eq!(mapped.open_since_ms, None);
        assert_eq!(mapped.last_outcome_at_ms, Some(8_000));
    }

    #[test]
    fn half_open_target_renders_stored_outcome() {
        let target = target_with_ages(
            CircuitPhase::HalfOpenReady,
            Some(Duration::from_millis(100)),
            Some(LastOutcome::Timeout),
            Some(Duration::from_millis(100)),
        );
        let mapped = map_target(target, 10_000);
        // Only an OPEN phase derives circuit_open; half-open surfaces the
        // stored kind while still exposing its open-since age.
        assert_eq!(mapped.last_outcome.as_deref(), Some("timeout"));
        assert_eq!(mapped.open_since_ms, Some(9_900));
    }

    /// Wire-shape golden: the new fields serialize under their exact snake_case
    /// keys with the derived values, and the closed/never-seen state serializes
    /// each to JSON `null` (never a `0`/epoch sentinel).
    #[test]
    fn new_fields_serialize_under_stable_keys_with_null_semantics() {
        let open = serde_json::to_value(map_target(
            target_with_ages(
                CircuitPhase::Open,
                Some(Duration::from_millis(400)),
                Some(LastOutcome::Http5xx),
                Some(Duration::from_millis(400)),
            ),
            10_000,
        ))
        .unwrap();
        assert_eq!(open["open_since_ms"], Value::from(9_600));
        assert_eq!(open["last_outcome"], Value::from("circuit_open"));
        assert_eq!(open["last_outcome_at_ms"], Value::from(9_600));

        let closed = serde_json::to_value(map_target(
            target_with_ages(CircuitPhase::Closed, None, None, None),
            10_000,
        ))
        .unwrap();
        let obj = closed.as_object().unwrap();
        // The keys are present on the wire (stable shape), each carrying null.
        assert!(obj.contains_key("open_since_ms"));
        assert!(obj.contains_key("last_outcome"));
        assert!(obj.contains_key("last_outcome_at_ms"));
        assert!(closed["open_since_ms"].is_null());
        assert!(closed["last_outcome"].is_null());
        assert!(closed["last_outcome_at_ms"].is_null());
    }

    #[test]
    fn map_target_carries_event_surface_fields() {
        let mapped = map_target(sample_target(CircuitPhase::HalfOpenReady), 1_000);
        assert_eq!(mapped.state_key, "opus#seat-a");
        assert_eq!(mapped.nickname, "opus");
        assert_eq!(mapped.provider_name, "anthropic");
        assert_eq!(mapped.upstream, "claude-opus-wire");
        assert_eq!(mapped.seat_label.as_deref(), Some("seat-a"));
        assert_eq!(mapped.circuit, "half_open_ready");
        assert_eq!(mapped.rpm_available, Some(12.0));
        assert!(mapped.half_open_probe_in_flight);
    }

    #[test]
    fn map_learned_renames_feature_key_to_capability_key() {
        let entry = LearnedRegistryEntry {
            state_key: "opus".into(),
            feature_key: "structured_output".into(),
            verdict: Verdict::LearnedBroken(FailurePhase::F2),
            signal_tier: SignalTier::SelfIdentifying,
            observations: 2,
            first_seen: Instant::now(),
            last_seen: Instant::now(),
            expires_at: Instant::now(),
            phase: FailurePhase::F2,
            source: EvidenceSource::Live,
        };
        let mapped = map_learned(entry, Instant::now(), 10_000);
        assert_eq!(mapped.state_key, "opus");
        assert_eq!(mapped.capability_key, "structured_output");
        assert_eq!(mapped.verdict, "broken");
        assert_eq!(mapped.signal_tier, "self-identifying");
        assert_eq!(mapped.observations, 2);
        assert_eq!(mapped.phase, "f2");
        assert_eq!(mapped.source, "live");
    }

    /// A VerifiedWorking snapshot row maps to the `verified` verdict token,
    /// so a positive is distinguishable from a learned negative that merely
    /// carries `phase=f3` -- the two share that phase but differ by verdict.
    #[test]
    fn map_learned_surfaces_verified_verdict() {
        let entry = LearnedRegistryEntry {
            state_key: "opus".into(),
            feature_key: "web_search".into(),
            verdict: Verdict::VerifiedWorking,
            signal_tier: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: Instant::now(),
            last_seen: Instant::now(),
            expires_at: Instant::now(),
            phase: FailurePhase::F3,
            source: EvidenceSource::Live,
        };
        let mapped = map_learned(entry, Instant::now(), 10_000);
        assert_eq!(mapped.verdict, "verified");
        assert_eq!(mapped.phase, "f3");
    }

    fn learned_entry(last_seen: Instant) -> LearnedRegistryEntry {
        LearnedRegistryEntry {
            state_key: "opus".into(),
            feature_key: "web_search".into(),
            verdict: Verdict::LearnedBroken(FailurePhase::F1),
            signal_tier: SignalTier::Inferred,
            observations: 1,
            first_seen: last_seen,
            last_seen,
            expires_at: last_seen,
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
        }
    }

    /// `last_seen_ms` is derived from the entry's monotonic last-seen age
    /// against the single pinned build clock. A past observation lands behind
    /// `now_ms`; a future-dated last-seen clamps the age to zero, so the
    /// instant never exceeds `now_ms` and never goes negative.
    #[test]
    fn map_learned_derives_last_seen_ms_and_clamps_future() {
        let now = Instant::now();
        let past = now
            .checked_sub(Duration::from_millis(400))
            .expect("test clock is well past boot");
        let mapped = map_learned(learned_entry(past), now, 10_000);
        assert_eq!(mapped.last_seen_ms, Some(9_600));

        let future = now
            .checked_add(Duration::from_millis(400))
            .expect("no monotonic-clock overflow");
        let clamped = map_learned(learned_entry(future), now, 10_000);
        assert_eq!(clamped.last_seen_ms, Some(10_000));
    }

    /// Wire-shape golden: `last_seen_ms` serializes under its exact snake_case
    /// key with the derived epoch-ms value.
    #[test]
    fn last_seen_ms_serializes_under_stable_key() {
        let now = Instant::now();
        let past = now
            .checked_sub(Duration::from_millis(250))
            .expect("test clock is well past boot");
        let value = serde_json::to_value(map_learned(learned_entry(past), now, 5_000)).unwrap();
        assert_eq!(value["last_seen_ms"], Value::from(4_750));
    }

    /// The DTO field names MUST equal the `docs/LOGGING.md` contract tokens,
    /// mirrored by the shared `vocabulary` consts. If a rename drifts the
    /// wire away from the event surface, this trips.
    #[test]
    fn learned_negative_field_names_match_logging_contract() {
        let value = serde_json::to_value(LearnedNegative {
            state_key: "k".into(),
            capability_key: "web_search".into(),
            verdict: "broken",
            signal_tier: "inferred",
            observations: 1,
            phase: "f1",
            source: "live",
            last_seen_ms: Some(1_000),
        })
        .unwrap();
        let obj = value.as_object().unwrap();
        assert!(obj.contains_key(vocabulary::STATE_KEY));
        assert!(obj.contains_key(vocabulary::CAPABILITY_KEY));
        assert!(obj.contains_key(vocabulary::SIGNAL_TIER));
        assert!(obj.contains_key("observations"));
        // The attribution fields added by the phase-on-entry migration
        // serialize under their own stable snake_case keys.
        assert!(obj.contains_key("phase"));
        assert!(obj.contains_key("source"));
        assert_eq!(obj["phase"], Value::from("f1"));
        assert_eq!(obj["source"], Value::from("live"));
        // The verdict discriminator surfaces under its own stable key.
        assert!(obj.contains_key("verdict"));
        assert_eq!(obj["verdict"], Value::from("broken"));
        // The last-observation timestamp surfaces under its own stable key.
        assert!(obj.contains_key("last_seen_ms"));
        // The pre-rename token must NOT surface on the wire.
        assert!(!obj.contains_key("feature_key"));
    }

    /// Vocabulary-drift guard against the `docs/LOGGING.md` contract. The
    /// health DTOs REUSE the capability event surface's field names, and those
    /// names are the stable wire contract documented under "Capability
    /// intelligence events". This reads that doc and asserts each reused token
    /// (the `vocabulary::` const the DTO serializes with) is present as a
    /// documented field name in that section. A rename/removal on EITHER side
    /// -- the DTO const OR the docs -- trips this, forcing a deliberate matched
    /// contract change instead of a silent drift.
    #[test]
    fn dto_vocabulary_matches_logging_contract_section() {
        // Embedded at compile time, so a moved/renamed LOGGING.md is a build
        // error rather than a silently-skipped test.
        const LOGGING_DOC: &str = include_str!("../../../../../docs/LOGGING.md");
        const SECTION_HEADING: &str = "## Capability intelligence events";

        // Scope to the capability event-contract section so unrelated prose
        // elsewhere in the doc can never satisfy the assertion.
        let section_start = LOGGING_DOC
            .find(SECTION_HEADING)
            .expect("LOGGING.md must carry the capability events section");
        let after_heading = section_start + SECTION_HEADING.len();
        let section_end = LOGGING_DOC[after_heading..]
            .find("\n## ")
            .map_or(LOGGING_DOC.len(), |idx| after_heading + idx);
        let section = &LOGGING_DOC[section_start..section_end];

        // Each reused token is documented as a backticked field name. Building
        // the needle FROM the `vocabulary::` const binds both directions: a
        // rename of the const changes the needle (docs stops matching), and a
        // rename in the docs removes the needle (const stops matching).
        for token in [
            vocabulary::STATE_KEY,
            vocabulary::CAPABILITY_KEY,
            vocabulary::SIGNAL_TIER,
        ] {
            let documented = format!("`{token}`");
            assert!(
                section.contains(&documented),
                "LOGGING.md capability section must document the field name {documented}"
            );
        }

        // Bind the documented tokens to what the DTO actually serializes with:
        // the `LearnedNegative` wire keys ARE these vocabulary consts.
        let wire = serde_json::to_value(LearnedNegative {
            state_key: "k".into(),
            capability_key: "web_search".into(),
            verdict: "broken",
            signal_tier: "inferred",
            observations: 1,
            phase: "f1",
            source: "live",
            last_seen_ms: Some(1_000),
        })
        .unwrap();
        let obj = wire.as_object().unwrap();
        assert!(obj.contains_key(vocabulary::STATE_KEY));
        assert!(obj.contains_key(vocabulary::CAPABILITY_KEY));
        assert!(obj.contains_key(vocabulary::SIGNAL_TIER));
    }

    #[test]
    fn build_from_view_uses_a_single_snapshot() {
        // A fresh router has no resolved targets and no learned negatives;
        // the point is that ONE view drives both reads and the DTO builds.
        let state = test_state();
        let view = state.router.view();
        let panel = build_from_view(&view);
        assert!(panel.targets.is_empty());
        assert!(panel.learned_negatives.is_empty());
    }

    #[tokio::test]
    async fn handler_returns_available_panel_with_rfc3339_as_of() {
        let app = super::super::status_router().with_state(test_state());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/status/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["schema_version"], SCHEMA_VERSION);
        assert!(json["unavailable"].is_null());
        let as_of = json["as_of"].as_str().expect("as_of present");
        assert!(
            chrono::DateTime::parse_from_rfc3339(as_of).is_ok(),
            "as_of must be RFC3339: {as_of}"
        );
        assert!(json["data"]["targets"].is_array());
        assert!(json["data"]["learned_negatives"].is_array());
    }

    #[test]
    fn payload_carries_only_stable_identifiers_and_fixed_vocabulary() {
        let panel = HealthPanel {
            targets: vec![map_target(sample_target(CircuitPhase::Open), 1_000)],
            learned_negatives: vec![map_learned(
                LearnedRegistryEntry {
                    state_key: "opus".into(),
                    feature_key: "web_search".into(),
                    verdict: Verdict::LearnedBroken(FailurePhase::F1),
                    signal_tier: SignalTier::Inferred,
                    observations: 1,
                    first_seen: Instant::now(),
                    last_seen: Instant::now(),
                    expires_at: Instant::now(),
                    phase: FailurePhase::F1,
                    source: EvidenceSource::Live,
                },
                Instant::now(),
                1_000,
            )],
        };
        let text = serde_json::to_string(&panel).unwrap();
        for forbidden in ["body", "error_text", "prompt", "message", "raw"] {
            assert!(
                !text.contains(forbidden),
                "health payload must not carry `{forbidden}`: {text}"
            );
        }
    }
}
