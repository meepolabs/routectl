//! Consolidated acceptance layer for the active capability probe, driving the
//! REAL components together end to end: the lib-shaped probe core
//! ([`run_capability_probe`]) with a fake only at the network seam
//! ([`CanaryDispatch`]), the real capability ledger (a migrated temp DB written
//! through `insert_capability_event`), and a real [`Router`] whose learned
//! registry is rebuilt from that ledger through the shared replay engine. Each
//! scenario is an independent AAA test that follows one probe outcome from
//! dispatch, through synchronous persistence, across a rebuild boundary, to the
//! routing decision the rebuilt registry drives.
//!
//! The one intentional test double is [`ScriptedDispatch`]: it stands in for a
//! bare `Provider` at the single completion seam, so no scenario touches the
//! network. Everything downstream of the seam -- classification through the
//! shared `detect`, the capability matcher, the ledger schema, the replay
//! admission arms, and the `acting_negative_for` decision -- is the production
//! code path.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

use async_trait::async_trait;
use routectl_auth::{MemoryStore, SecretStore};
use routectl_core::capability::{
    EvidenceSource, FailurePhase, STRUCTURED_OUTPUT, SignalTier, Verdict, WEB_SEARCH,
};
use routectl_core::{ChatResponse, Error as CoreError};
use routectl_router::{
    CapabilityEventRow as ReplayRow, CapabilityLedgerReader, Config, LearnedRegistryEntry,
    ReplayTombstone, Router,
};
use routectl_usage::{
    CapabilityEvent, insert_capability_event, latest_tombstone, open, open_readonly, open_rw,
    read_capability_events_after,
};
use serde_json::json;

use super::*;
use crate::commands::probe::{canary, resolve};

// --- network-seam double ------------------------------------------------

/// A fake [`CanaryDispatch`] returning canned responses in order -- the single
/// point where the acceptance layer replaces the network. Every other
/// component under test is real.
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

// --- canned canary responses --------------------------------------------

/// A schema-conforming structured-output response -- classifies as a verified
/// positive through the shared `detect`.
fn verified_structured_output() -> ChatResponse {
    serde_json::from_value(json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "{\"answer\": \"ok\"}"},
            "finish_reason": "stop"
        }]
    }))
    .expect("valid ChatResponse")
}

/// A clean web-search response carrying no search evidence -- classifies as a
/// suspected absence.
fn suspect_web_search() -> ChatResponse {
    serde_json::from_value(json!({
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "no search performed"},
            "finish_reason": "stop"
        }]
    }))
    .expect("valid ChatResponse")
}

/// An openai-compat 400 whose `error.param` names the constrained-decoding wire
/// param -- the deterministic capability-naming rejection the F1 matcher
/// attributes to `structured_output`.
fn openai_response_format_rejection() -> CoreError {
    let body = json!({
        "error": {
            "type": "invalid_request_error",
            "code": "unsupported_parameter",
            "param": "response_format",
            "message": "Unsupported parameter."
        }
    })
    .to_string();
    CoreError::upstream_full(
        "openai",
        400,
        body,
        None,
        Some("invalid_request_error".to_string()),
        Some("unsupported_parameter".to_string()),
    )
}

// --- shared harness -----------------------------------------------------

/// The default-config router this crate ships, over a fresh migrated ledger at
/// `db_path`. The router's boot revision is the stamp every probe event and the
/// ledger tombstone carry, so a same-revision rebuild admits them.
async fn router_over_fresh_ledger(db_path: &Path) -> Router {
    drop(open(db_path).expect("create migrated ledger"));
    let config = Arc::new(Config::default());
    let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
    crate::server::build_router_from_config(config, secrets)
        .await
        .expect("build router")
}

/// This boot's revision as the ledger's `i64` columns carry it.
fn boot_revision(router: &Router) -> (i64, i64) {
    (
        i64::from(router.catalog_version()),
        i64::try_from(router.overlay_revision()).expect("overlay fits i64"),
    )
}

/// A probe plan for `lane` on an openai-compat target, stamped at `revision`.
fn plan_for(lane: &str, revision: (i64, i64)) -> CapabilityProbePlan {
    CapabilityProbePlan {
        state_key: lane.to_string(),
        provider_kind: "openai-compat".to_string(),
        model: "test-model".to_string(),
        catalog_version: revision.0,
        overlay_revision: revision.1,
        rates: None,
    }
}

/// Write a boot tombstone at `revision` so a rebuild has a boundary to replay
/// after.
fn write_tombstone(db_path: &Path, revision: (i64, i64)) {
    let db = open_rw(db_path).expect("open rw");
    insert_capability_event(
        db.conn(),
        &CapabilityEvent::tombstone(now_ms(), revision.0, revision.1),
    )
    .expect("write tombstone");
}

/// Persist every event a probe report produced, in write order, on a
/// read-write connection -- exactly the synchronous path the CLI wrapper takes.
fn persist(db_path: &Path, events: &[CapabilityEvent]) {
    let db = open_rw(db_path).expect("open rw");
    for event in events {
        insert_capability_event(db.conn(), event).expect("persist event");
    }
}

/// Persist a single pre-built event (used to seed a LIVE negative alongside the
/// probe's own writes).
fn persist_one(db_path: &Path, event: &CapabilityEvent) {
    persist(db_path, std::slice::from_ref(event));
}

/// Current wall-clock epoch milliseconds.
fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_millis(),
    )
    .expect("epoch ms fits i64")
}

/// Rebuild `router`'s learned registry from the ledger at `db_path` through the
/// real replay engine, then return its post-rebuild snapshot. Reads the events
/// after the latest tombstone exactly as the warm bridge does.
fn rebuild_and_snapshot(db_path: &Path, router: &Router) -> Vec<LearnedRegistryEntry> {
    struct LedgerReader {
        tombstone: ReplayTombstone,
        rows: Vec<ReplayRow>,
    }
    impl CapabilityLedgerReader for LedgerReader {
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
        .expect("a boot tombstone was written");
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
    router.rebuild_learned_from_ledger(&LedgerReader { tombstone, rows });
    router.learned_capability_snapshot()
}

/// Look up one resident entry by its lane / capability keys.
fn find<'a>(
    snap: &'a [LearnedRegistryEntry],
    lane: &str,
    cap: &str,
) -> Option<&'a LearnedRegistryEntry> {
    snap.iter()
        .find(|e| e.state_key == lane && e.feature_key == cap)
}

/// The dispatch-path decision an entry drives, derived from its snapshot fields
/// exactly as `LearnedCapabilityRegistry::acting_negative_for` derives it: a
/// non-acting entry, a verified positive, and an advisory F3+Live negative all
/// allow; an acting negative routes away until its decay window lapses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActingDecision {
    Allow,
    RouteAway,
    Reprobe,
}

fn acting_decision(entry: &LearnedRegistryEntry, now: Instant) -> ActingDecision {
    let is_acting =
        matches!(entry.signal_tier, SignalTier::SelfIdentifying) || entry.observations >= 2;
    if !is_acting {
        return ActingDecision::Allow;
    }
    match entry.verdict {
        Verdict::VerifiedWorking => ActingDecision::Allow,
        Verdict::LearnedBroken(FailurePhase::F3) if entry.source == EvidenceSource::Live => {
            ActingDecision::Allow
        }
        Verdict::LearnedBroken(_) => {
            if now >= entry.expires_at {
                ActingDecision::Reprobe
            } else {
                ActingDecision::RouteAway
            }
        }
        _ => ActingDecision::Allow,
    }
}

// --- Scenario 1: fresh lane -> populated source:probe matrix row ---------

#[tokio::test]
async fn fresh_lane_probe_populates_source_probe_cells_after_rebuild() {
    // Arrange: a fresh migrated ledger + default router, its boot tombstone.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("usage.db");
    let router = router_over_fresh_ledger(&db).await;
    let rev = boot_revision(&router);
    write_tombstone(&db, rev);

    // Act: probe two capabilities on a fresh lane -- one verifies, one suspects
    // -- then persist the events and rebuild the registry at the same revision.
    let dispatcher = ScriptedDispatch::new(vec![
        Ok(verified_structured_output()),
        Ok(suspect_web_search()),
    ]);
    let plan = plan_for("opus", rev);
    let report = run_capability_probe(
        &dispatcher,
        &plan,
        &[
            ProbeCapability::StructuredOutput,
            ProbeCapability::WebSearch,
        ],
    )
    .await;
    persist(&db, &report.events);
    let snap = rebuild_and_snapshot(&db, &router);

    // Assert: both cells landed as truth-matrix rows stamped source=probe.
    let so = find(&snap, "opus", STRUCTURED_OUTPUT).expect("structured_output cell resident");
    assert_eq!(so.source, EvidenceSource::Probe);
    assert_eq!(so.verdict, Verdict::VerifiedWorking);

    let ws = find(&snap, "opus", WEB_SEARCH).expect("web_search cell resident");
    assert_eq!(ws.source, EvidenceSource::Probe);
    assert_eq!(ws.verdict, Verdict::LearnedBroken(FailurePhase::F3));
    assert!(
        snap.iter().all(|e| e.source == EvidenceSource::Probe),
        "every cell a fresh probe run mints is source=probe"
    );
}

// --- Scenario 2: probe success clears a resident negative across restart -

#[tokio::test]
async fn probe_success_clears_a_resident_negative_and_the_clear_survives_restart() {
    // Arrange: a fresh ledger with a boot tombstone and a resident LIVE
    // self-identifying negative on `opus`/structured_output.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("usage.db");
    let seed_router = router_over_fresh_ledger(&db).await;
    let rev = boot_revision(&seed_router);
    write_tombstone(&db, rev);
    persist_one(&db, &live_broken(now_ms(), "opus", STRUCTURED_OUTPUT, rev));

    // A fresh restart before the probe: the negative is resident and routes away.
    let before_router = router_over_fresh_ledger(&db).await;
    let before = rebuild_and_snapshot(&db, &before_router);
    let neg = find(&before, "opus", STRUCTURED_OUTPUT).expect("live negative resident");
    assert_eq!(
        acting_decision(neg, Instant::now()),
        ActingDecision::RouteAway,
        "the resident live negative routes away before the probe settles it"
    );

    // Act: a probe verifies the capability, emitting cleared-then-verified; the
    // cleared event must be stamped strictly earlier.
    let dispatcher = ScriptedDispatch::new(vec![Ok(verified_structured_output())]);
    let report = run_capability_probe(
        &dispatcher,
        &plan_for("opus", rev),
        &[ProbeCapability::StructuredOutput],
    )
    .await;
    assert_eq!(report.events.len(), 2, "cleared then verified");
    assert_eq!(report.events[0].verdict, Verdict::Cleared.as_str());
    assert_eq!(report.events[1].verdict, Verdict::VerifiedWorking.as_str());
    assert!(
        report.events[0].ts < report.events[1].ts,
        "cleared ts ({}) must be strictly before verified ts ({})",
        report.events[0].ts,
        report.events[1].ts,
    );
    persist(&db, &report.events);

    // Assert: after a restart the negative is gone (not resurrected) and the
    // lane is no longer routed away -- the settlement survived the boundary.
    let after_router = router_over_fresh_ledger(&db).await;
    let after = rebuild_and_snapshot(&db, &after_router);
    let settled = find(&after, "opus", STRUCTURED_OUTPUT).expect("the probe positive is resident");
    assert_eq!(settled.verdict, Verdict::VerifiedWorking);
    assert_eq!(settled.source, EvidenceSource::Probe);
    assert_eq!(
        acting_decision(settled, Instant::now()),
        ActingDecision::Allow,
        "a settled lane is not routed away after restart"
    );
}

// --- Scenario 3: suspect(probe) F3 route-away vs advisory F3+Live ---------

#[tokio::test]
async fn forced_search_absent_routes_away_under_probe_authority_but_not_under_live() {
    // Arrange: a fresh ledger + boot tombstone.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("usage.db");
    let router = router_over_fresh_ledger(&db).await;
    let rev = boot_revision(&router);
    write_tombstone(&db, rev);

    // Act: a forced-search-empty probe suspects the absence -> suspect(probe),
    // an F3 negative at inferred tier. A single inferred observation does not
    // act, so pin the source-based routing authority on a CORROBORATED (acting)
    // pair: two same-shape suspects per lane, identical but for the evidence
    // source -- probe on `probe-lane`, live on `live-lane`.
    let dispatcher = ScriptedDispatch::new(vec![Ok(suspect_web_search())]);
    let report = run_capability_probe(
        &dispatcher,
        &plan_for("probe-lane", rev),
        &[ProbeCapability::WebSearch],
    )
    .await;
    assert_eq!(report.cells[0].outcome, CellOutcome::SuspectAbsence);
    let probe_suspect = report.events[0].clone();
    assert_eq!(probe_suspect.source, EvidenceSource::Probe.as_str());
    assert_eq!(probe_suspect.phase, FailurePhase::F3.as_str());

    let corroborating = |base: &CapabilityEvent, lane: &str, source: EvidenceSource| {
        let mut a = base.clone();
        a.lane_key = lane.to_string();
        a.source = source.as_str().to_string();
        let mut b = a.clone();
        b.ts = a.ts + 1;
        vec![a, b]
    };

    persist(
        &db,
        &corroborating(&probe_suspect, "probe-lane", EvidenceSource::Probe),
    );
    persist(
        &db,
        &corroborating(&probe_suspect, "live-lane", EvidenceSource::Live),
    );
    let snap = rebuild_and_snapshot(&db, &router);
    let now = Instant::now();

    // Assert: the corroborated probe-sourced F3 negative carries route-away
    // authority; the identical corroborated live-sourced F3 negative stays
    // advisory (allows) -- the only difference is the evidence source.
    let probe_neg = find(&snap, "probe-lane", WEB_SEARCH).expect("probe negative resident");
    assert_eq!(probe_neg.phase, FailurePhase::F3);
    assert_eq!(probe_neg.source, EvidenceSource::Probe);
    assert_eq!(acting_decision(probe_neg, now), ActingDecision::RouteAway);

    let live_neg = find(&snap, "live-lane", WEB_SEARCH).expect("live negative resident");
    assert_eq!(live_neg.phase, FailurePhase::F3);
    assert_eq!(live_neg.source, EvidenceSource::Live);
    assert_eq!(
        acting_decision(live_neg, now),
        ActingDecision::Allow,
        "an F3+Live suspect is advisory-only -- it never routes away on its own"
    );
}

// --- Scenario 4: capability-naming 400 -> broken(probe), F1 vs F2 ---------

#[tokio::test]
async fn deterministic_naming_400_learns_broken_at_f1_while_f2_is_withheld() {
    // Arrange: a fresh ledger + boot tombstone.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("usage.db");
    let router = router_over_fresh_ledger(&db).await;
    let rev = boot_revision(&router);
    write_tombstone(&db, rev);

    // Act 1: an openai-compat rejection naming the wire param resolves through
    // the shared matcher to a broken(probe) negative at F1.
    let f1_dispatcher = ScriptedDispatch::new(vec![Err(openai_response_format_rejection())]);
    let f1 = run_capability_probe(
        &f1_dispatcher,
        &plan_for("opus", rev),
        &[ProbeCapability::StructuredOutput],
    )
    .await;
    match &f1.cells[0].outcome {
        CellOutcome::Broken { phase, capability } => {
            assert_eq!(*phase, FailurePhase::F1);
            assert_eq!(capability, STRUCTURED_OUTPUT);
        }
        other => panic!("expected Broken at F1, got {other:?}"),
    }
    assert_eq!(f1.events[0].phase, FailurePhase::F1.as_str());
    assert_eq!(f1.events[0].source, EvidenceSource::Probe.as_str());
    persist(&db, &f1.events);

    // Act 2: an anthropic-api prose rejection that only a (shipped-empty) F2
    // feature-naming table could attribute mints nothing -- the live matcher
    // withholds F2 on real traffic.
    let anthropic_400 = CoreError::upstream(
        "anthropic",
        400,
        r#"{"error":{"type":"invalid_request_error","message":"The feature response_schema is not supported for this model."}}"#,
    );
    let f2_dispatcher = ScriptedDispatch::new(vec![Err(anthropic_400)]);
    let mut anthropic_plan = plan_for("sonnet", rev);
    anthropic_plan.provider_kind = "anthropic-api".to_string();
    let f2 = run_capability_probe(
        &f2_dispatcher,
        &anthropic_plan,
        &[ProbeCapability::StructuredOutput],
    )
    .await;
    assert_eq!(
        f2.cells[0].outcome,
        CellOutcome::Inconclusive,
        "a prose feature-naming rejection is inconclusive while the F2 tables ship empty"
    );
    assert!(f2.events.is_empty(), "F2 withheld mints no event");

    // Assert: after rebuild only the F1 negative is resident and routes away.
    let snap = rebuild_and_snapshot(&db, &router);
    let broken = find(&snap, "opus", STRUCTURED_OUTPUT).expect("F1 broken negative resident");
    assert_eq!(broken.verdict, Verdict::LearnedBroken(FailurePhase::F1));
    assert_eq!(broken.source, EvidenceSource::Probe);
    assert_eq!(
        acting_decision(broken, Instant::now()),
        ActingDecision::RouteAway
    );
    assert!(
        find(&snap, "sonnet", STRUCTURED_OUTPUT).is_none(),
        "the withheld F2 rejection left no resident cell"
    );
}

// --- Scenario 5: estimate printed; unhealthy lane skips + mints nothing ---

#[tokio::test]
async fn unhealthy_lane_renders_the_estimate_and_skipped_cells_and_mints_nothing() {
    // Arrange: a fresh ledger + boot tombstone; the first canary rate-limits.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("usage.db");
    let router = router_over_fresh_ledger(&db).await;
    let rev = boot_revision(&router);
    write_tombstone(&db, rev);

    // Act: a 429 on the first canary trips the lane; the remaining cell skips.
    let dispatcher = ScriptedDispatch::new(vec![
        Err(CoreError::upstream("provider", 429, "rate limited")),
        Ok(verified_structured_output()),
    ]);
    let report = run_capability_probe(
        &dispatcher,
        &plan_for("opus", rev),
        &[
            ProbeCapability::StructuredOutput,
            ProbeCapability::WebSearch,
        ],
    )
    .await;

    // Assert: the estimate is always computed and printed; the downstream cell
    // renders skipped; and the run mints nothing (no event, empty ledger).
    let lines = render_report(&report);
    assert!(
        lines.iter().any(|l| l.starts_with("estimate:")),
        "the estimate line is always printed: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("skipped: lane unhealthy")),
        "the cell after the unhealthy canary renders skipped: {lines:?}"
    );
    assert_eq!(report.cells[1].outcome, CellOutcome::SkippedLaneUnhealthy);
    assert!(report.events.is_empty(), "an unhealthy lane mints nothing");

    persist(&db, &report.events);
    let snap = rebuild_and_snapshot(&db, &router);
    assert!(
        snap.is_empty(),
        "an unhealthy probe run leaves an empty learned registry"
    );
}

// --- Scenario 6: wizard offer scoped to the just-added provider ----------

#[tokio::test]
async fn wizard_offer_scopes_the_probe_to_the_just_added_provider() {
    // Arrange: a config where two providers each own a distinct routable lane.
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("usage.db");
    drop(open(&db).expect("create migrated ledger"));
    let config_path = tmp.path().join("config.toml");
    let body = format!(
        "version = 3\n\n\
         [usage]\ndb_path = \"{}\"\n\n\
         [providers.incumbent]\nkind = \"openai-compat\"\n\
         base_url = \"http://127.0.0.1:1\"\napi_key_ref = \"literal:k\"\n\n\
         [providers.added]\nkind = \"openai-compat\"\n\
         base_url = \"http://127.0.0.1:2\"\napi_key_ref = \"literal:k\"\n\n\
         [models.incumbent-lane]\nprovider = \"incumbent\"\nupstream = \"incumbent-model\"\n\n\
         [models.added-lane]\nprovider = \"added\"\nupstream = \"added-model\"\n\n\
         [aliases]\ndefault = \"incumbent-lane\"\n",
        db.display()
    );
    std::fs::write(&config_path, &body).unwrap();

    // The scoping contract: the offer resolves ONLY the just-added provider's
    // lane, never a sibling's.
    let loaded = crate::server::load_effective_config_unvalidated(&config_path).expect("load");
    let target = resolve::resolve_probe_target(&loaded.config, Some("added"), None)
        .expect("added provider resolves");
    assert_eq!(target.state_key, "added-lane");
    assert_eq!(target.model_id, "added-model");
    assert_eq!(target.provider, "added");

    // Act: run the offer for `added`, declining at the confirm seam so nothing
    // dispatches. A resolvable lane consults confirm (the estimate is printed).
    let consulted = std::sync::atomic::AtomicBool::new(false);
    offer_scoped_probe(&config_path, "added", |_estimate| {
        consulted.store(true, std::sync::atomic::Ordering::SeqCst);
        false
    })
    .await;

    // Assert: the offer reached the confirm seam for the added lane, and the
    // decline wrote nothing to the shared ledger (no lane, added or incumbent,
    // was probed).
    assert!(
        consulted.load(std::sync::atomic::Ordering::SeqCst),
        "a resolvable just-added lane consults the confirm seam"
    );
    let reopened = open_rw(&db).expect("reopen ledger");
    let events = read_capability_events_after(reopened.conn(), 0, 100).expect("read ledger");
    assert!(
        events.is_empty(),
        "a declined offer probes no lane and mints nothing"
    );
}

// --- Scenario 7: ProbeProfileV1 ceiling + canary counts by literal --------

#[test]
fn probe_profile_ceiling_and_canary_counts_are_pinned_by_literal() {
    // The baked profile: exact literals so a refactor cannot silently widen a
    // probe's blast radius.
    assert_eq!(PROBE_PROFILE_V1.max_tokens, 1536);
    assert_eq!(PROBE_PROFILE_V1.structured_output_canaries, 1);
    assert_eq!(PROBE_PROFILE_V1.web_search_canaries, 1);
    assert_eq!(PROBE_PROFILE_V1.prompt_caching_canaries, 2);
    assert_eq!(PROBE_PROFILE_V1.thinking_canaries, 1);

    // The estimate over every capability reads those same literals: five canary
    // calls (1 + 1 + 2 + 1), each bounded by the baked ceiling.
    let estimate = estimate_probe_cost(&ProbeCapability::ALL, None);
    assert_eq!(estimate.total_calls, 5);
    assert_eq!(estimate.max_tokens, 1536);
    assert_eq!(estimate.estimated_output_tokens, 5 * 1536);

    // The ceiling is actually applied at request-build time, not just in the
    // estimate: every canary request carries the baked max_tokens.
    let model = "test-model";
    assert_eq!(
        canary::structured_output_canary(model).request.max_tokens,
        Some(1536)
    );
    assert_eq!(
        canary::web_search_canary(model).request.max_tokens,
        Some(1536)
    );
    assert_eq!(
        canary::thinking_canary(model).request.max_tokens,
        Some(1536)
    );
    let caching = canary::prompt_caching_canary(model);
    assert_eq!(caching.prime.max_tokens, Some(1536));
    assert_eq!(caching.read.max_tokens, Some(1536));
}

/// A self-identifying LIVE `broken` F1 negative -- the common wire-token shape,
/// used to seed a resident negative a probe success later clears.
fn live_broken(ts: i64, lane: &str, cap: &str, revision: (i64, i64)) -> CapabilityEvent {
    CapabilityEvent {
        ts,
        lane_key: lane.to_string(),
        capability: cap.to_string(),
        verdict: Verdict::LearnedBroken(FailurePhase::F1)
            .as_str()
            .to_string(),
        phase: FailurePhase::F1.as_str().to_string(),
        source: EvidenceSource::Live.as_str().to_string(),
        tier: SignalTier::SelfIdentifying.as_str().to_string(),
        evidence_class: None,
        upstream_token: None,
        catalog_version: revision.0,
        overlay_revision: revision.1,
    }
}
