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
use std::time::Instant;

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use routectl_router::LearnedRegistryEntry;
use routectl_router::router::RouteTargetStatus;
use routectl_router::runtime_state::CircuitPhase;

use super::router_view::StatusRouterView;
use super::vocabulary::codes;
use super::{Panel, StatusState, guard_panel, now_utc_rfc3339};

/// Wire-shape version of the health panel payload.
const SCHEMA_VERSION: u32 = 1;

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
}

/// One resident learned-capability negative. Field names are the
/// `docs/LOGGING.md` contract tokens: `state_key`, `capability_key`
/// (the internal `feature_key`, RENAMED to the contract token),
/// `signal_tier`.
#[derive(Debug, Clone, Serialize)]
struct LearnedNegative {
    state_key: String,
    capability_key: String,
    signal_tier: &'static str,
    observations: u32,
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

fn map_target(target: RouteTargetStatus) -> TargetHealth {
    TargetHealth {
        state_key: target.state_key,
        nickname: target.nickname,
        provider_name: target.provider_name,
        upstream: target.upstream,
        seat_label: target.seat_label,
        circuit: circuit_token(target.gate.circuit),
        rpm_available: target.gate.rpm_available,
        half_open_probe_in_flight: target.gate.half_open_probe_in_flight,
    }
}

fn map_learned(entry: LearnedRegistryEntry) -> LearnedNegative {
    LearnedNegative {
        state_key: entry.state_key,
        capability_key: entry.feature_key,
        signal_tier: entry.signal_tier.as_str(),
        observations: entry.observations,
    }
}

/// Build the panel from ONE router snapshot. The single `view` drives both
/// reads, so the target health and the learned negatives are internally
/// consistent (no interleaved hot-swap).
fn build_from_view(view: &StatusRouterView) -> HealthPanel {
    let targets = view
        .route_targets(Instant::now())
        .into_iter()
        .map(map_target)
        .collect();
    let learned_negatives = view
        .learned_capabilities()
        .into_iter()
        .map(map_learned)
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
    use routectl_core::capability::SignalTier;
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

    #[test]
    fn map_target_carries_event_surface_fields() {
        let mapped = map_target(sample_target(CircuitPhase::HalfOpenReady));
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
            signal_tier: SignalTier::SelfIdentifying,
            observations: 2,
            first_seen: Instant::now(),
            last_seen: Instant::now(),
            expires_at: Instant::now(),
        };
        let mapped = map_learned(entry);
        assert_eq!(mapped.state_key, "opus");
        assert_eq!(mapped.capability_key, "structured_output");
        assert_eq!(mapped.signal_tier, "self-identifying");
        assert_eq!(mapped.observations, 2);
    }

    /// The DTO field names MUST equal the `docs/LOGGING.md` contract tokens,
    /// mirrored by the shared `vocabulary` consts. If a rename drifts the
    /// wire away from the event surface, this trips.
    #[test]
    fn learned_negative_field_names_match_logging_contract() {
        let value = serde_json::to_value(LearnedNegative {
            state_key: "k".into(),
            capability_key: "web_search".into(),
            signal_tier: "inferred",
            observations: 1,
        })
        .unwrap();
        let obj = value.as_object().unwrap();
        assert!(obj.contains_key(vocabulary::STATE_KEY));
        assert!(obj.contains_key(vocabulary::CAPABILITY_KEY));
        assert!(obj.contains_key(vocabulary::SIGNAL_TIER));
        assert!(obj.contains_key("observations"));
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
            signal_tier: "inferred",
            observations: 1,
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
            targets: vec![map_target(sample_target(CircuitPhase::Open))],
            learned_negatives: vec![map_learned(LearnedRegistryEntry {
                state_key: "opus".into(),
                feature_key: "web_search".into(),
                signal_tier: SignalTier::Inferred,
                observations: 1,
                first_seen: Instant::now(),
                last_seen: Instant::now(),
                expires_at: Instant::now(),
            })],
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
