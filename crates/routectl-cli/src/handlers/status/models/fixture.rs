//! Test-only fully populated `/status/models` panel, built through the
//! panel's own projection from the shared fixture targets, lane cells the
//! shared lane-cell resolver resolves over the shared fixture config, and a
//! transit tally recorded on a real wire-check state.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use routectl_core::UpstreamReason;
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier, Verdict};
use routectl_core::wire_check::WirePath;
use routectl_router::router::RouteTargetStatus;
use routectl_router::runtime_state::UnusableCause;
use routectl_router::wire_check_state::{
    CheckOutcome, TRANSIT_PATH_CAP, WireCheckState, WireTransitSnapshot,
};
use routectl_router::{
    BetaSeedScope, Config, LearnedRegistryEntry, OverrideRegistry, StateKey, VerdictValue,
};
use serde_json::Value;

use super::super::health::fixture::field_key;
use super::super::panel_fixtures::{
    FIXTURE_NOW_MS, fixture_as_of, fixture_config, fixture_targets,
};
use super::super::router_view::AccountCredential;
use super::super::utc_rfc3339;
use super::{ModelInputs, ModelsPanel, Panel, SCHEMA_VERSION, models_panel};
use crate::commands::doctor::{SeatFix, capability_columns};
use crate::lane_cell::{CellSources, LearnedSnapshot, ResolvedLane, lane_metas, resolve_lanes};

/// The epoch-ms the fixture's wire-check state reads as built.
pub const FIXTURE_STARTED_MS: u64 = 1_794_996_000_000;

/// The serialized populated models envelope.
pub fn populated_models() -> Value {
    serde_json::to_value(populated_models_panel()).expect("the models panel serializes")
}

/// The shared targets as lanes: `sonnet` eligible with a learned refusal
/// carrying a value and a transit tally past its path cap, `seat-sonnet` an
/// OAuth seat whose login is unusable, `glm` open after a transport error
/// with a learned negative an override does not reach, and `bedclaude`
/// with no learned lane, so no row.
pub fn populated_models_panel() -> Panel<ModelsPanel> {
    let now = Instant::now();
    let config = fixture_config();
    let dto = models_panel(ModelInputs {
        now_ms: FIXTURE_NOW_MS,
        targets: fixture_targets(),
        credentials: credentials(&config),
        lanes: resolved_lanes(&config, &learned_entries(now), now),
        transit: transit(),
    });
    Panel::available(SCHEMA_VERSION, fixture_as_of(), dto)
}

/// The models panel over `targets` at `now_ms`, built through the panel's
/// own projection: lane cells resolved over `config` with nothing learned,
/// and a transit tally that has sampled nothing since `now_ms`.
pub fn models_panel_of(
    config: &Config,
    targets: Vec<RouteTargetStatus>,
    credentials: BTreeMap<String, AccountCredential>,
    now_ms: i64,
) -> Panel<ModelsPanel> {
    let mut transit = WireCheckState::default().transit_snapshot();
    transit.started_ms = u64::try_from(now_ms).expect("the fixture instant is after the epoch");
    let dto = models_panel(ModelInputs {
        now_ms,
        targets,
        credentials,
        lanes: resolved_lanes(config, &[], Instant::now()),
        transit,
    });
    Panel::available(SCHEMA_VERSION, utc_rfc3339(now_ms), dto)
}

/// Every routed lane of `config` resolved over `entries`, as the facade
/// resolves the resident registry, with no shipped seed.
pub fn resolved_lanes(
    config: &Config,
    entries: &[LearnedRegistryEntry],
    now: Instant,
) -> Vec<ResolvedLane> {
    let overrides = OverrideRegistry::build(config);
    let sources = CellSources {
        config,
        overrides: &overrides,
        learned: LearnedSnapshot {
            entries,
            now: Some((now, FIXTURE_NOW_MS)),
            seed_clears: &[],
        },
        beta_seed: BetaSeedScope::EMPTY,
    };
    let metas = lane_metas(config, entries);
    let columns = capability_columns(BetaSeedScope::EMPTY, &metas, entries, &overrides);
    resolve_lanes(&metas, &columns, &sources)
}

/// One learned entry on `lane` for `feature_key`, seen over the last ten
/// minutes, decaying in an hour.
pub fn learned_entry(
    lane: &str,
    feature_key: String,
    verdict: Verdict,
    now: Instant,
) -> LearnedRegistryEntry {
    LearnedRegistryEntry {
        state_key: lane.to_string(),
        feature_key,
        verdict,
        signal_tier: SignalTier::SelfIdentifying,
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

fn learned_entries(now: Instant) -> Vec<LearnedRegistryEntry> {
    vec![
        LearnedRegistryEntry {
            value: Some(VerdictValue::Ceiling(32_000)),
            ..learned_entry(
                "anthropic#claude-sonnet-4-5",
                field_key("max_tokens"),
                Verdict::LearnedBroken(FailurePhase::F1),
                now,
            )
        },
        learned_entry(
            "compat#glm-5",
            "web_search".to_string(),
            Verdict::LearnedBroken(FailurePhase::F1),
            now,
        ),
    ]
}

/// The seat logged out for 15 minutes; the API-key entries usable.
fn credentials(config: &Config) -> BTreeMap<String, AccountCredential> {
    BTreeMap::from([
        (
            "anthropic#a".to_string(),
            AccountCredential::unusable(
                UnusableCause::Expired,
                FIXTURE_NOW_MS - 900_000,
                &SeatFix::of(config, "claude-seat"),
            ),
        ),
        ("#anthropic".to_string(), AccountCredential::usable()),
        ("#compat".to_string(), AccountCredential::usable()),
        (
            "#bed".to_string(),
            AccountCredential::parked(UpstreamReason::CredentialInvalid, FIXTURE_NOW_MS + 120_000),
        ),
    ])
}

/// `sonnet`'s lane checked five times, two changed, with one more changed
/// path than the per-lane cap names; `glm`'s lane checked clean once. The
/// seat lane is never sampled.
fn transit() -> WireTransitSnapshot {
    let state = WireCheckState::default();
    let sonnet = StateKey::parse("anthropic#claude-sonnet-4-5").expect("a lane key");
    let paths: Vec<WirePath> = (0..=TRANSIT_PATH_CAP)
        .map(|i| WirePath::from_segments([format!("field_{i:02}").as_str()]))
        .collect();
    state.record_transit(Some(&sonnet), CheckOutcome::Loss, paths.iter());
    state.record_transit(Some(&sonnet), CheckOutcome::Loss, paths[..1].iter());
    for _ in 0..3 {
        state.record_transit(Some(&sonnet), CheckOutcome::Clean, std::iter::empty());
    }
    let glm = StateKey::parse("compat#glm-5").expect("a lane key");
    state.record_transit(Some(&glm), CheckOutcome::Clean, std::iter::empty());
    let mut snapshot = state.transit_snapshot();
    snapshot.started_ms = FIXTURE_STARTED_MS;
    snapshot
}
