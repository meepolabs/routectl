use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use routectl_core::capability::{
    EvidenceSource, FailurePhase, SCHEMA_PARSE, STRUCTURED_OUTPUT, SignalTier, Verdict, WEB_SEARCH,
};
use routectl_core::{ChatResponse, Error as CoreError};
use routectl_usage::{CapabilityEvent, Rates, open, open_rw};
use serde_json::json;

use super::*;

// --- test doubles -------------------------------------------------------

struct ScriptedDispatch {
    responses: Mutex<VecDeque<routectl_core::Result<ChatResponse>>>,
}

impl ScriptedDispatch {
    fn new(responses: Vec<routectl_core::Result<ChatResponse>>) -> Self {
        Self {
            responses: Mutex::new(responses.into()),
        }
    }
}

#[async_trait]
impl CanaryDispatch for ScriptedDispatch {
    async fn complete(&self, _req: ChatRequest) -> routectl_core::Result<ChatResponse> {
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("ScriptedDispatch ran out of canned responses")
    }
}

// --- response builders --------------------------------------------------

fn verified_structured_output_response() -> ChatResponse {
    let value = json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "{\"answer\": \"ok\"}"},
            "finish_reason": "stop"
        }]
    });
    serde_json::from_value(value).expect("valid ChatResponse")
}

fn suspect_web_search_response() -> ChatResponse {
    let value = json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "no search results"},
            "finish_reason": "stop"
        }]
    });
    serde_json::from_value(value).expect("valid ChatResponse")
}

fn inconclusive_response() -> ChatResponse {
    let value = json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hi"},
            "finish_reason": "length"
        }]
    });
    serde_json::from_value(value).expect("valid ChatResponse")
}

fn default_plan() -> CapabilityProbePlan {
    CapabilityProbePlan {
        state_key: "opus".to_string(),
        provider_kind: "openai-compat".to_string(),
        model: "test-model".to_string(),
        catalog_version: i64::from(CATALOG_VERSION),
        overlay_revision: 0,
        rates: None,
    }
}

fn openai_feature_unsupported_body(param: &str) -> String {
    json!({
        "error": {
            "type": "invalid_request_error",
            "code": "unsupported_parameter",
            "param": param,
            "message": "Unsupported parameter."
        }
    })
    .to_string()
}

// --- core tests ---------------------------------------------------------

#[tokio::test]
async fn verified_emits_cleared_then_verified_with_ordered_timestamps() {
    let dispatcher = ScriptedDispatch::new(vec![Ok(verified_structured_output_response())]);
    let plan = default_plan();

    let report =
        run_capability_probe(&dispatcher, &plan, &[ProbeCapability::StructuredOutput]).await;

    assert_eq!(report.cells.len(), 1);
    assert_eq!(report.cells[0].outcome, CellOutcome::Verified);

    let events = &report.events;
    assert_eq!(events.len(), 2, "cleared then verified");
    assert_eq!(events[0].verdict, Verdict::Cleared.as_str());
    assert_eq!(events[0].source, EvidenceSource::Probe.as_str());
    assert_eq!(events[0].lane_key, "opus");
    assert_eq!(events[0].capability, STRUCTURED_OUTPUT);
    assert_eq!(events[1].verdict, Verdict::VerifiedWorking.as_str());
    assert_eq!(events[1].source, EvidenceSource::Probe.as_str());
    assert_eq!(events[1].phase, FailurePhase::F3.as_str());
    assert_eq!(events[1].tier, SignalTier::SelfIdentifying.as_str());
    assert_eq!(events[1].evidence_class.as_deref(), Some(SCHEMA_PARSE));
    assert!(
        events[0].ts < events[1].ts,
        "cleared ts ({}) must be strictly before verified ts ({})",
        events[0].ts,
        events[1].ts
    );
}

#[tokio::test]
async fn suspect_absence_emits_one_suspect_event() {
    let dispatcher = ScriptedDispatch::new(vec![Ok(suspect_web_search_response())]);
    let plan = default_plan();

    let report = run_capability_probe(&dispatcher, &plan, &[ProbeCapability::WebSearch]).await;

    assert_eq!(report.cells[0].outcome, CellOutcome::SuspectAbsence);
    let events = &report.events;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].verdict, Verdict::SuspectIgnored.as_str());
    assert_eq!(events[0].source, EvidenceSource::Probe.as_str());
    assert_eq!(events[0].phase, FailurePhase::F3.as_str());
    assert_eq!(events[0].capability, WEB_SEARCH);
}

#[tokio::test]
async fn broken_400_naming_a_capability_emits_broken_event() {
    let body = openai_feature_unsupported_body("response_format");
    let err = CoreError::upstream_full(
        "openai",
        400,
        body,
        None,
        Some("invalid_request_error".to_string()),
        Some("unsupported_parameter".to_string()),
    );
    let dispatcher = ScriptedDispatch::new(vec![Err(err)]);
    let plan = default_plan();

    let report =
        run_capability_probe(&dispatcher, &plan, &[ProbeCapability::StructuredOutput]).await;

    match &report.cells[0].outcome {
        CellOutcome::Broken { phase, capability } => {
            assert_eq!(*phase, FailurePhase::F1);
            assert_eq!(capability, "structured_output");
        }
        other => panic!("expected Broken, got {other:?}"),
    }
    let events = &report.events;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].verdict, "broken");
    assert_eq!(events[0].phase, "f1");
    assert_eq!(events[0].source, "probe");
    assert_eq!(events[0].capability, "structured_output");
}

#[tokio::test]
async fn lane_health_failure_429_marks_unhealthy_and_skips_remaining() {
    let err_429 = CoreError::upstream("provider", 429, "rate limited");
    let dispatcher = ScriptedDispatch::new(vec![
        Err(err_429),
        Ok(verified_structured_output_response()),
    ]);
    let plan = default_plan();

    let report = run_capability_probe(
        &dispatcher,
        &plan,
        &[
            ProbeCapability::StructuredOutput,
            ProbeCapability::WebSearch,
        ],
    )
    .await;

    match &report.cells[0].outcome {
        CellOutcome::Unhealthy { class } => assert_eq!(*class, "rate-limited"),
        other => panic!("expected Unhealthy, got {other:?}"),
    }
    assert_eq!(report.cells[1].outcome, CellOutcome::SkippedLaneUnhealthy);
    assert!(
        report.events.is_empty(),
        "no events emitted for lane-health failures or skipped cells"
    );
}

#[tokio::test]
async fn capability_level_400_does_not_mark_lane_unhealthy_d20() {
    let non_naming_400 = CoreError::upstream("provider", 400, "{}");
    let dispatcher = ScriptedDispatch::new(vec![
        Err(non_naming_400),
        Ok(verified_structured_output_response()),
    ]);
    let mut plan = default_plan();
    plan.provider_kind = "anthropic-api".to_string();

    let report = run_capability_probe(
        &dispatcher,
        &plan,
        &[
            ProbeCapability::StructuredOutput,
            ProbeCapability::WebSearch,
        ],
    )
    .await;

    assert_eq!(
        report.cells[0].outcome,
        CellOutcome::Inconclusive,
        "a non-naming 400 is inconclusive, not lane-unhealthy"
    );
    assert_ne!(
        report.cells[1].outcome,
        CellOutcome::SkippedLaneUnhealthy,
        "the second cell must NOT be skipped -- capability evidence never trips lane health"
    );
}

#[tokio::test]
async fn clean_stop_gate_reject_produces_inconclusive() {
    let dispatcher = ScriptedDispatch::new(vec![Ok(inconclusive_response())]);
    let plan = default_plan();

    let report =
        run_capability_probe(&dispatcher, &plan, &[ProbeCapability::StructuredOutput]).await;

    assert_eq!(report.cells[0].outcome, CellOutcome::Inconclusive);
    assert!(report.events.is_empty(), "no event on inconclusive");
}

#[test]
fn estimate_always_computed_from_profile() {
    let rates = Rates {
        input_per_mtok: Some(3.0),
        output_per_mtok: Some(15.0),
        cache_read_per_mtok: None,
        cache_write_5m_per_mtok: None,
        cache_write_1h_per_mtok: None,
    };

    let estimate = estimate_probe_cost(&ProbeCapability::ALL, Some(&rates));

    let expected_calls: u32 = [1, 1, 2, 1].iter().sum();
    assert_eq!(estimate.total_calls, expected_calls);
    assert_eq!(estimate.max_tokens, PROBE_PROFILE_V1.max_tokens);
    let expected_output = i64::from(expected_calls) * i64::from(PROBE_PROFILE_V1.max_tokens);
    assert_eq!(estimate.estimated_output_tokens, expected_output);
    assert!(estimate.cost.is_some(), "priced target must produce a cost");
}

#[test]
fn estimate_is_none_when_unpriced() {
    let estimate = estimate_probe_cost(&ProbeCapability::ALL, None);
    assert!(estimate.cost.is_none());
}

// --- revision stamp: write-then-rebuild ---------------------------------

/// Build a default router (baked catalog version, overlay revision zero) and
/// a fresh migrated usage DB at `db_path`. Returns the router and its boot
/// revision `(catalog_version, overlay_revision)`.
async fn router_and_fresh_db(tmp: &Path, db_path: &Path) -> (routectl_router::Router, (i64, i64)) {
    use routectl_auth::{MemoryStore, SecretStore};
    use routectl_router::Config;

    drop(open(db_path).expect("create db"));

    let mut config = Config::default();
    config.usage.db_path = tmp.join("router-usage.db");
    let config = Arc::new(config);
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    let router = crate::server::build_router_from_config(config, secrets)
        .await
        .expect("build router");
    let revision = (
        i64::from(router.catalog_version()),
        i64::try_from(router.overlay_revision()).unwrap(),
    );
    (router, revision)
}

/// Rebuild `router`'s registry from the ledger at `db_path` through the real
/// replay engine and return the summary. A probe-source row that SURVIVES the
/// boundary replays through the shared admission arms and lands in
/// `replayed_probe`; a row the stamp mismatch drops never reaches them --
/// so `replayed_probe` is a direct readout of whether the stamp matched.
fn rebuild_summary(
    db_path: &Path,
    router: &routectl_router::Router,
) -> routectl_router::CapabilityRebuildSummary {
    use routectl_router::{
        CapabilityEventRow as ReplayRow, CapabilityLedgerReader, ReplayTombstone,
    };
    use routectl_usage::{latest_tombstone, open_readonly, read_capability_events_after};

    struct TestReader {
        tombstone: ReplayTombstone,
        rows: Vec<ReplayRow>,
    }
    impl CapabilityLedgerReader for TestReader {
        fn tombstone(&self) -> Option<ReplayTombstone> {
            Some(self.tombstone)
        }
        fn read_events(&self) -> Vec<ReplayRow> {
            self.rows.clone()
        }
    }

    let db = open_readonly(db_path).expect("open readonly");
    let t = latest_tombstone(db.conn())
        .expect("read tombstone")
        .expect("a tombstone was written");
    let tombstone = ReplayTombstone::new(
        t.rowid,
        u32::try_from(t.catalog_version.expect("cv")).unwrap(),
        u64::try_from(t.overlay_revision.expect("ov")).unwrap(),
    );
    let rows = read_capability_events_after(db.conn(), t.rowid, 5000)
        .expect("read events")
        .into_iter()
        .map(|r| {
            ReplayRow::new(
                r.rowid,
                Instant::now(),
                r.verdict.expect("verdict"),
                r.phase,
                r.source.expect("source"),
                r.tier,
                r.evidence_class,
                r.capability.expect("capability"),
                r.lane_key.expect("lane_key"),
                String::new(),
                u32::try_from(r.catalog_version.expect("cv")).unwrap(),
                u64::try_from(r.overlay_revision.expect("ov")).unwrap(),
            )
        })
        .collect();
    router.rebuild_learned_from_ledger(&TestReader { tombstone, rows })
}

fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// Write a boot tombstone at `tombstone_rev`, then run a probe whose canary
/// 400s (a deterministic capability-naming rejection) so it writes a `broken`
/// negative stamped `event_rev`. A negative is resident and thus visible in
/// the learned snapshot after rebuild.
async fn probe_broken_into_ledger(
    db_path: &Path,
    tombstone_rev: (i64, i64),
    event_rev: (i64, i64),
) {
    use routectl_usage::insert_capability_event;

    let db = open_rw(db_path).expect("open rw");
    insert_capability_event(
        db.conn(),
        &CapabilityEvent::tombstone(now_ms(), tombstone_rev.0, tombstone_rev.1),
    )
    .expect("write tombstone");

    let body = openai_feature_unsupported_body("response_format");
    let err = CoreError::upstream_full(
        "openai",
        400,
        body,
        None,
        Some("invalid_request_error".to_string()),
        Some("unsupported_parameter".to_string()),
    );
    let dispatcher = ScriptedDispatch::new(vec![Err(err)]);
    let plan = CapabilityProbePlan {
        state_key: "opus".to_string(),
        provider_kind: "openai-compat".to_string(),
        model: "test-model".to_string(),
        catalog_version: event_rev.0,
        overlay_revision: event_rev.1,
        rates: None,
    };
    let report =
        run_capability_probe(&dispatcher, &plan, &[ProbeCapability::StructuredOutput]).await;
    assert!(
        matches!(report.cells[0].outcome, CellOutcome::Broken { .. }),
        "canary 400 must produce a broken cell"
    );
    for probe_event in &report.events {
        insert_capability_event(db.conn(), probe_event).expect("persist probe event");
    }
    drop(db);
}

#[tokio::test]
async fn revision_stamp_matches_boot_boundary_and_events_survive_the_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("probe-usage.db");

    let (router, boot_rev) = router_and_fresh_db(tmp.path(), &db_path).await;

    // Production code computes its stamp identically: the baked const plus
    // the overlay revision on the loaded (here default) overlay.
    let my_rev = (
        i64::from(CATALOG_VERSION),
        i64::try_from(routectl_router::overlay_revision(
            &routectl_router::CatalogOverlay::default(),
        ))
        .unwrap(),
    );
    assert_eq!(my_rev, boot_rev, "probe stamp must match the boot boundary");

    // Probe stamps its events with the SAME revision as the boot tombstone.
    probe_broken_into_ledger(&db_path, boot_rev, boot_rev).await;

    // The probe event survives `should_replay` and replays through the
    // shared admission arms -- proof the stamp matched.
    let summary = rebuild_summary(&db_path, &router);
    assert_eq!(
        summary.replayed_probe, 1,
        "a probe event stamped at the boot revision must survive the boundary; {summary:?}"
    );
}

#[tokio::test]
async fn revision_stamp_mismatch_drops_probe_events_at_the_boundary() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("probe-usage.db");

    let (router, boot_rev) = router_and_fresh_db(tmp.path(), &db_path).await;

    // Stamp the probe events with a revision that DIFFERS from the boot
    // tombstone's -- exactly the silent-drop failure the stamp guards against.
    let stale_rev = (boot_rev.0, boot_rev.1 + 1);
    probe_broken_into_ledger(&db_path, boot_rev, stale_rev).await;

    let summary = rebuild_summary(&db_path, &router);
    assert_eq!(
        summary.replayed_probe, 0,
        "an event whose stamp disagrees with the boundary is dropped before replay; {summary:?}"
    );
}

// --- misc ---------------------------------------------------------------

#[test]
fn probe_capability_from_token_round_trips() {
    for cap in ProbeCapability::ALL {
        assert_eq!(
            ProbeCapability::from_token(cap.capability_key()),
            Some(cap),
            "from_token must round-trip for {}",
            cap.capability_key()
        );
    }
    assert_eq!(ProbeCapability::from_token("made_up"), None);
}

#[test]
fn select_capabilities_empty_means_all() {
    let caps = select_capabilities(&[]).expect("empty means all");
    assert_eq!(caps.len(), 4);
}

#[test]
fn select_capabilities_rejects_unknown_token() {
    let err = select_capabilities(&["bogus".to_string()]).expect_err("unknown errors");
    assert!(err.contains("bogus"), "err: {err}");
}
