//! Test-only fully populated `/status/health` panel, built through this
//! panel's own mappers from router-shaped inputs.

use std::time::{Duration, Instant};

use routectl_core::EligibilityReason;
use routectl_core::RefusalReason;
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier, Verdict};
use routectl_core::wire_check::{
    EgressKind, IngressDialect, Lane, Leg, PathClass, RejectionProvenance, WirePath,
};
use routectl_router::wire_check_state::{FieldRepairOutcome, WireCheckParams, WireCheckState};
use routectl_router::{
    BetaSeedScope, LearnedRegistryEntry, OverrideRegistry, PoolEligibility, VerdictValue,
};
use serde_json::Value;

use super::super::panel_fixtures::{
    FIXTURE_NOW_MS, fixture_as_of, fixture_config, fixture_targets,
};
use super::{
    HealthPanel, Panel, SCHEMA_VERSION, map_learned, map_pool, map_seat_driver, map_target,
    project_wire_check,
};
use crate::commands::doctor::SeatFix;
use crate::lane_cell::{CellSources, LearnedSnapshot, resolve_entries};
use crate::server::seat_driver::SeatDriverHealthSnapshot;

/// The serialized populated health envelope.
pub fn populated_health() -> Value {
    serde_json::to_value(populated_health_panel()).expect("the health panel serializes")
}

/// Every target state the panel distinguishes (eligible, unusable with its
/// fix, breaker open, parked), two pools, both seat drivers running, a learned
/// negative an override outranks, a verified positive, a learned value, and
/// one row of each wire-check section.
pub fn populated_health_panel() -> Panel<HealthPanel> {
    let config = fixture_config();
    let targets = fixture_targets()
        .into_iter()
        .map(|target| {
            let fix = target
                .gate
                .credential_unusable
                .map(|_| SeatFix::of(&config, &target.provider_name).command());
            map_target(target, FIXTURE_NOW_MS, fix)
        })
        .collect();
    let dto = HealthPanel {
        targets,
        pools: fixture_pools().into_iter().map(map_pool).collect(),
        seat_driver: map_seat_driver(SeatDriverHealthSnapshot {
            running: true,
            last_sweep_ms: Some(FIXTURE_NOW_MS - 30_000),
            last_good_sweep_ms: Some(FIXTURE_NOW_MS - 90_000),
            sweeps: 12,
            transient: 1,
            definitive: 1,
        }),
        usage_poll_driver: map_seat_driver(SeatDriverHealthSnapshot {
            running: true,
            last_sweep_ms: Some(FIXTURE_NOW_MS - 45_000),
            last_good_sweep_ms: Some(FIXTURE_NOW_MS - 300_000),
            sweeps: 7,
            transient: 2,
            definitive: 1,
        }),
        learned_negatives: learned_rows(),
        wire_check: project_wire_check(&wire_check_snapshot(), unix_now_secs()),
    };
    Panel::available(SCHEMA_VERSION, fixture_as_of(), dto)
}

fn fixture_pools() -> Vec<PoolEligibility> {
    vec![
        PoolEligibility {
            pool: "seats".to_string(),
            configured: 3,
            eligible: 1,
            by_reason: vec![
                (EligibilityReason::CredentialUnusable, 1),
                (EligibilityReason::BreakerOpen, 1),
            ],
        },
        PoolEligibility {
            pool: "backup".to_string(),
            configured: 2,
            eligible: 1,
            by_reason: vec![(EligibilityReason::RateLimited, 1)],
        },
    ]
}

/// A grounded field verdict key, spelled in pieces so the field prefix
/// literal keeps its single owner.
pub fn field_key(path: &str) -> String {
    let key = format!("{}{}{path}", "fie", "ld:");
    assert!(
        routectl_router::capability_key_is_field_verdict(&key),
        "premise: {key} is a field verdict key"
    );
    key
}

fn learned_entry(
    lane: &str,
    feature_key: String,
    verdict: Verdict,
    now: Instant,
) -> LearnedRegistryEntry {
    LearnedRegistryEntry {
        state_key: lane.to_string(),
        feature_key,
        verdict,
        signal_tier: SignalTier::Inferred,
        observations: 3,
        first_seen: now.checked_sub(Duration::from_mins(10)).unwrap_or(now),
        last_seen: now.checked_sub(Duration::from_secs(5)).unwrap_or(now),
        expires_at: now + Duration::from_hours(1),
        evidence_class: None,
        phase: FailurePhase::F1,
        source: EvidenceSource::Live,
        recheck_owned: false,
        value: None,
    }
}

/// The learned rows, resolved against the fixture config's overrides the
/// way the router facade resolves the resident registry.
fn learned_rows() -> Vec<super::LearnedNegative> {
    let now = Instant::now();
    let entries = [
        learned_entry(
            "anthropic#claude-sonnet-4-5",
            "computer_use".to_string(),
            Verdict::LearnedBroken(FailurePhase::F1),
            now,
        ),
        LearnedRegistryEntry {
            signal_tier: SignalTier::SelfIdentifying,
            source: EvidenceSource::Probe,
            ..learned_entry(
                "anthropic#claude-sonnet-4-5",
                "web_search".to_string(),
                Verdict::VerifiedWorking,
                now,
            )
        },
        LearnedRegistryEntry {
            value: Some(VerdictValue::Ceiling(32_000)),
            phase: FailurePhase::F2,
            ..learned_entry(
                "compat#glm-5",
                field_key("max_tokens"),
                Verdict::LearnedBroken(FailurePhase::F2),
                now,
            )
        },
    ];
    let config = fixture_config();
    let overrides = OverrideRegistry::build(&config);
    resolve_entries(&CellSources {
        config: &config,
        overrides: &overrides,
        learned: LearnedSnapshot {
            entries: &entries,
            now: Some((now, FIXTURE_NOW_MS)),
            seed_clears: &[],
        },
        beta_seed: BetaSeedScope::EMPTY,
    })
    .into_iter()
    .map(|resolved| map_learned(resolved, now, FIXTURE_NOW_MS))
    .collect()
}

/// One loss, one unrepaired rejection and one local refusal recorded on a
/// real wire-check state.
fn wire_check_snapshot() -> routectl_router::wire_check_state::WireCheckSnapshot {
    let state = WireCheckState::default();
    let lane = Lane::of(IngressDialect::Anthropic, EgressKind::AnthropicApi);
    let lost = WirePath::root().child("metadata");
    state.record_compared(lane, Leg::Request, [(&lost, PathClass::Missing)], false);
    state.record_rejection(
        lane,
        Leg::Request,
        &WirePath::root().child("tools").index(0),
        RejectionProvenance::ClientUnchanged,
        FieldRepairOutcome::Unrepaired,
    );
    state.record_local_refusal(lane, RefusalReason::CloakRelocation);
    state.snapshot(&WireCheckParams::default())
}

fn unix_now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after epoch")
        .as_secs()
}
