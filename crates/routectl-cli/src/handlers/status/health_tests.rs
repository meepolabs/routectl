//! Unit tests for the `/status/health` panel's DTO mapping and handler.

use super::super::vocabulary;
use super::*;
use crate::handlers::status::DaemonMeta;
use crate::server::AppState;
use arc_swap::ArcSwap;
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use futures::stream::BoxStream;
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier, Verdict};
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Error, Provider, UpstreamReason};
use routectl_router::runtime_state::{CredentialParkStatus, ProviderGateStatus};
use routectl_router::{Config, Router, StateKey};
use serde_json::Value;
use tower::ServiceExt;

fn test_state() -> Arc<StatusState> {
    let router = Router::new(Arc::new(Config::default()));
    let app = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
    Arc::new(StatusState::from_app(&app, None, DaemonMeta::for_test()))
}

fn sample_target(circuit: CircuitPhase) -> RouteTargetStatus {
    RouteTargetStatus {
        state_key: "opus#seat-a".into(),
        nickname: "opus".into(),
        provider_name: "anthropic".into(),
        upstream: "claude-opus-wire".into(),
        seat_label: Some("seat-a".into()),
        learned_lane: StateKey::parse("anthropic#claude-opus-wire"),
        gate: ProviderGateStatus {
            rpm_available: Some(12.0),
            circuit,
            half_open_probe_in_flight: true,
            circuit_open_elapsed: None,
            last_outcome: None,
            last_outcome_elapsed: None,
            credential_park: None,
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
        learned_lane: StateKey::parse("anthropic#claude-opus-wire"),
        gate: ProviderGateStatus {
            rpm_available: Some(12.0),
            circuit,
            half_open_probe_in_flight: false,
            circuit_open_elapsed,
            last_outcome,
            last_outcome_elapsed,
            credential_park: None,
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
    assert!(obj.contains_key("park_reason"));
    assert!(obj.contains_key("parked_until_ms"));
    assert!(closed["park_reason"].is_null());
    assert!(closed["parked_until_ms"].is_null());
}

/// A parked seat carries its reason token and the epoch-ms its park lapses,
/// derived from the park's remaining time against the pinned build clock.
#[test]
fn a_parked_target_carries_its_reason_and_deadline() {
    let mut target = sample_target(CircuitPhase::Closed);
    target.gate.credential_park = Some(CredentialParkStatus {
        reason: UpstreamReason::CredentialExpired,
        remaining: Duration::from_secs(3),
    });

    let value = serde_json::to_value(map_target(target, 10_000)).unwrap();

    assert_eq!(value["park_reason"], Value::from("credential-expired"));
    assert_eq!(value["parked_until_ms"], Value::from(13_000));
}

/// The panel reads the park through the live router's own status read: a
/// seat parked in the router's runtime state shows on its health row, and
/// its unparked sibling shows null.
#[tokio::test]
async fn the_health_panel_reads_the_park_from_the_live_router() {
    let router = parked_router();
    let app = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
    let state = Arc::new(StatusState::from_app(&app, None, DaemonMeta::for_test()));
    let before_ms = chrono::Utc::now().timestamp_millis();

    let panel = build(&state).await;

    let json = serde_json::to_value(&panel).unwrap();
    let targets = json["data"]["targets"].as_array().expect("targets");
    let row = |key: &str| {
        targets
            .iter()
            .find(|t| t["state_key"] == key)
            .unwrap_or_else(|| panic!("no target {key}: {json}"))
            .clone()
    };
    let parked = row("parked");
    assert_eq!(parked["park_reason"], Value::from("credential-invalid"));
    let until = parked["parked_until_ms"].as_i64().expect("deadline");
    assert!(
        until >= before_ms + 590_000 && until <= before_ms + 610_000,
        "deadline is about ten minutes out: {until} vs {before_ms}"
    );
    let free = row("free");
    assert!(free["park_reason"].is_null(), "{free}");
    assert!(free["parked_until_ms"].is_null(), "{free}");
}

/// Two non-pooled targets, `parked` and `free`, with `parked`'s credential
/// parked for ten minutes.
fn parked_router() -> Router {
    use routectl_router::ResolvedModel;
    use std::collections::BTreeMap;

    let mut router = Router::new(Arc::new(Config::default()));
    let provider: Arc<dyn Provider> = Arc::new(Unused);
    let models: BTreeMap<String, Arc<ResolvedModel>> = ["parked", "free"]
        .into_iter()
        .map(|nickname| {
            (
                nickname.to_string(),
                Arc::new(ResolvedModel::new(
                    nickname,
                    "p",
                    Arc::clone(&provider),
                    "u",
                )),
            )
        })
        .collect();
    router.install_resolved_models(models);
    assert!(router.park_credential_for_tests(
        "parked",
        UpstreamReason::CredentialInvalid,
        Instant::now() + Duration::from_mins(10),
    ));
    router
}

/// A provider the panel never dials.
struct Unused;

#[async_trait::async_trait]
impl Provider for Unused {
    fn id(&self) -> &'static str {
        "p"
    }
    fn normalize_request(&self, _: &ChatRequest) -> routectl_core::Result<Value> {
        Ok(Value::Null)
    }
    fn normalize_response(&self, _: Value) -> routectl_core::Result<ChatResponse> {
        Err(Error::normalize_response("p", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> routectl_core::Result<ChatResponse> {
        Err(Error::upstream("p", 500, "unused"))
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> routectl_core::Result<BoxStream<'static, routectl_core::Result<ChatChunk>>> {
        Err(Error::upstream("p", 500, "unused"))
    }
}

#[test]
fn map_target_carries_event_surface_fields() {
    let mapped = map_target(sample_target(CircuitPhase::HalfOpenReady), 1_000);
    assert_eq!(mapped.state_key, "opus#seat-a");
    assert_eq!(mapped.nickname, "opus");
    assert_eq!(mapped.provider_name, "anthropic");
    assert_eq!(mapped.upstream, "claude-opus-wire");
    assert_eq!(mapped.seat_label.as_deref(), Some("seat-a"));
    assert_eq!(
        mapped.learned_lane.as_deref(),
        Some("anthropic#claude-opus-wire")
    );
    assert_eq!(mapped.circuit, "half_open_ready");
    assert_eq!(mapped.rpm_available, Some(12.0));
    assert!(mapped.half_open_probe_in_flight);
}

#[test]
fn map_learned_renames_feature_key_to_capability_key() {
    let entry = LearnedRegistryEntry {
        state_key: "anthropic#claude-opus-wire".into(),
        feature_key: "structured_output".into(),
        verdict: Verdict::LearnedBroken(FailurePhase::F2),
        signal_tier: SignalTier::SelfIdentifying,
        observations: 2,
        first_seen: Instant::now(),
        last_seen: Instant::now(),
        expires_at: Instant::now(),
        evidence_class: None,
        phase: FailurePhase::F2,
        source: EvidenceSource::Live,
        recheck_owned: false,
    };
    let mapped = map_learned(entry, Instant::now(), 10_000);
    assert_eq!(mapped.state_key, "anthropic#claude-opus-wire");
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
        state_key: "anthropic#claude-opus-wire".into(),
        feature_key: "web_search".into(),
        verdict: Verdict::VerifiedWorking,
        signal_tier: SignalTier::SelfIdentifying,
        observations: 1,
        first_seen: Instant::now(),
        last_seen: Instant::now(),
        expires_at: Instant::now(),
        evidence_class: None,
        phase: FailurePhase::F3,
        source: EvidenceSource::Live,
        recheck_owned: false,
    };
    let mapped = map_learned(entry, Instant::now(), 10_000);
    assert_eq!(mapped.verdict, "verified");
    assert_eq!(mapped.phase, "f3");
}

fn learned_entry(last_seen: Instant) -> LearnedRegistryEntry {
    LearnedRegistryEntry {
        state_key: "anthropic#claude-opus-wire".into(),
        feature_key: "web_search".into(),
        verdict: Verdict::LearnedBroken(FailurePhase::F1),
        signal_tier: SignalTier::Inferred,
        observations: 1,
        first_seen: last_seen,
        last_seen,
        expires_at: last_seen,
        evidence_class: None,
        phase: FailurePhase::F1,
        source: EvidenceSource::Live,
        recheck_owned: false,
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

/// `build_from_view` wires the shared field-verdict snapshot log into
/// its build path -- the log's own behavior (counters, acting filter,
/// provenance) is pinned by `field_verdict_log`'s tests; this pins only
/// that the health build actually calls it.
#[test]
fn build_from_view_emits_the_field_verdict_snapshot_log() {
    let state = test_state();
    let view = state.router.view();

    let events = routectl_testkit::capture_events(|| {
        build_from_view(
            &view,
            false,
            FidelityEmission::always(),
            &FidelityGate::default(),
        );
    });

    assert!(
        events
            .iter()
            .any(|e| e.message == "envelope field verdict snapshot"),
        "health panel build must emit the field verdict snapshot log"
    );
}

#[test]
fn build_from_view_uses_a_single_snapshot() {
    // A fresh router has no resolved targets and no learned negatives;
    // the point is that ONE view drives both reads and the DTO builds.
    let state = test_state();
    let view = state.router.view();
    let panel = build_from_view(
        &view,
        false,
        FidelityEmission::always(),
        &FidelityGate::default(),
    );
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
                state_key: "anthropic#claude-opus-wire".into(),
                feature_key: "web_search".into(),
                verdict: Verdict::LearnedBroken(FailurePhase::F1),
                signal_tier: SignalTier::Inferred,
                observations: 1,
                first_seen: Instant::now(),
                last_seen: Instant::now(),
                expires_at: Instant::now(),
                evidence_class: None,
                phase: FailurePhase::F1,
                source: EvidenceSource::Live,
                recheck_owned: false,
            },
            Instant::now(),
            1_000,
        )],
        wire_check: project_wire_check(&test_state().router.view().wire_check_snapshot(), 0),
    };
    let text = serde_json::to_string(&panel).unwrap();
    for forbidden in ["body", "error_text", "prompt", "message", "raw"] {
        assert!(
            !text.contains(forbidden),
            "health payload must not carry `{forbidden}`: {text}"
        );
    }
}
