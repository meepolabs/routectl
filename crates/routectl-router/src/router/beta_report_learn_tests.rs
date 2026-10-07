//! The router's read of the provider's per-attempt beta repair report: a
//! fresh slot per provider call, a `beta:<flag>` negative minted on success,
//! the mint gates, and the settle-rejections-before-success order.

use super::*;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::stream::{BoxStream, StreamExt};
use parking_lot::Mutex;
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use routectl_core::{
    ChatChunk, ChatResponse, ChunkChoice, ChunkDelta, Error, Provider, Result, TokenCount,
};
use serde_json::json;

use crate::config::Config;
use crate::learned_capability::{ExportedEntry, RoutingDecision};
use crate::resolved::ResolvedModel;
use crate::router::RouterOptions;
use crate::router::runtime_gate::ProbeAdmission;

/// A native-Bedrock provider entry with no same-provider retry backoff, so a
/// 5xx retries at once.
const BEDROCK_P1: &str = r#"
[retry]
max_attempts = 2
initial_backoff_ms = 0
jitter_ms = 0

[providers.p1]
kind = "bedrock"
region = "us-east-1"
creds = { kind = "default-chain" }
"#;

/// The learned lane every target the fixture router builds keys on.
const LANE: &str = "p1#anthropic.wire-model";

const FLAG: &str = "zz-flag-a";

/// The capability key the fixture flag learns under.
fn flag_key() -> String {
    crate::beta_capability::beta_capability_key(FLAG).expect("the fixture flag is well formed")
}

/// What one provider call does: the flags it records on the request's report
/// slot, and whether it then fails with a 500.
#[derive(Clone)]
struct Call {
    record: Vec<&'static str>,
    fail_5xx: bool,
}

const fn ok_recording(record: Vec<&'static str>) -> Call {
    Call {
        record,
        fail_5xx: false,
    }
}

/// A provider that plays a scripted call per attempt (the last entry repeats)
/// and records whether each call it received carried a report slot.
struct ReportingProvider {
    script: Vec<Call>,
    calls: AtomicUsize,
    slot_seen: Mutex<Vec<bool>>,
}

impl ReportingProvider {
    fn new(script: Vec<Call>) -> Arc<Self> {
        Arc::new(Self {
            script,
            calls: AtomicUsize::new(0),
            slot_seen: Mutex::new(Vec::new()),
        })
    }

    fn play(&self, req: &ChatRequest) -> Result<()> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let call = self.script[n.min(self.script.len() - 1)].clone();
        let slot = req.routectl_internal.beta_repair_report.as_ref();
        self.slot_seen.lock().push(slot.is_some());
        if let Some(report) = slot {
            let flags: Vec<String> = call.record.iter().map(|f| (*f).to_string()).collect();
            report.record(&flags);
        }
        if call.fail_5xx {
            return Err(Error::upstream("p1", 500, "upstream unavailable"));
        }
        Ok(())
    }
}

fn content_chunk() -> ChatChunk {
    ChatChunk {
        id: "c1".into(),
        choices: vec![ChunkChoice {
            index: 0,
            delta: ChunkDelta {
                content: Some("ok".into()),
                ..Default::default()
            },
            finish_reason: None,
            matched_stop_sequence: None,
        }],
        ..Default::default()
    }
}

#[async_trait::async_trait]
impl Provider for ReportingProvider {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p1", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.play(&req)?;
        Ok(ChatResponse {
            model: req.model,
            ..Default::default()
        })
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.play(&req)?;
        Ok(futures::stream::iter(vec![Ok(content_chunk())]).boxed())
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.play(&req)?;
        Ok(TokenCount::default())
    }
}

fn router_with(toml_text: &str, provider: Arc<dyn Provider>) -> Router {
    let config: Config = toml::from_str(toml_text).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m1".to_string(),
        Arc::new(ResolvedModel::new(
            "m1",
            "p1",
            provider,
            "anthropic.wire-model",
        )),
    );
    router.install_resolved_models(models);
    router
}

fn req_with_betas(betas: &[&str]) -> ChatRequest {
    ChatRequest {
        model: "m1".into(),
        messages: vec![].into(),
        anthropic_beta: betas.iter().map(|b| (*b).to_string()).collect(),
        ..Default::default()
    }
}

fn beta_decision(router: &Router, key: &str) -> RoutingDecision {
    router
        .learned_capabilities
        .acting_negative_for(LANE, key, "bedrock", Instant::now())
}

fn assert_one_beta_event(events: &[crate::router::CapabilityLearnEvent]) {
    assert_eq!(events.len(), 1, "exactly one learn event");
    let ev = &events[0];
    assert_eq!(ev.state_key, LANE);
    assert_eq!(ev.capability_key, flag_key());
    assert_eq!(ev.provider_kind, "bedrock");
    assert_eq!(ev.signal_tier, SignalTier::SelfIdentifying);
    assert_eq!(ev.phase, FailurePhase::F1);
    assert_eq!(ev.source, EvidenceSource::Live);
    assert_eq!(ev.upstream_status, 400);
    assert!(!ev.remapped);
}

#[tokio::test]
async fn complete_success_with_a_reported_flag_mints_one_beta_negative() {
    let provider = ReportingProvider::new(vec![ok_recording(vec![FLAG])]);
    let router = router_with(BEDROCK_P1, provider);

    let dispatched = router
        .complete_with_options(req_with_betas(&[FLAG]), RouterOptions::default())
        .await;

    assert!(dispatched.result.is_ok());
    assert_one_beta_event(&dispatched.meta.learned_capabilities);
    assert!(matches!(
        beta_decision(&router, &flag_key()),
        RoutingDecision::RouteAway { .. }
    ));
}

#[tokio::test]
async fn stream_first_content_with_a_reported_flag_mints_one_beta_negative() {
    let provider = ReportingProvider::new(vec![ok_recording(vec![FLAG])]);
    let router = router_with(BEDROCK_P1, provider);

    let dispatched = router
        .stream_with_options(req_with_betas(&[FLAG]), RouterOptions::default())
        .await;

    assert!(dispatched.result.is_ok());
    assert_one_beta_event(&dispatched.meta.learned_capabilities);
    assert!(matches!(
        beta_decision(&router, &flag_key()),
        RoutingDecision::RouteAway { .. }
    ));
}

#[tokio::test]
async fn a_flag_recorded_by_a_failed_attempt_is_not_learned_on_a_clean_retry() {
    // Attempt 1 records the flag, then the upstream 500s; attempt 2 succeeds
    // and records nothing. Each provider call gets its own slot, so the
    // success reads an empty report.
    let provider = ReportingProvider::new(vec![
        Call {
            record: vec![FLAG],
            fail_5xx: true,
        },
        ok_recording(vec![]),
    ]);
    let router = router_with(BEDROCK_P1, provider.clone());

    let dispatched = router
        .complete_with_options(req_with_betas(&[FLAG]), RouterOptions::default())
        .await;

    assert!(dispatched.result.is_ok());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2, "the 5xx retried");
    assert!(dispatched.meta.learned_capabilities.is_empty());
    assert_eq!(beta_decision(&router, &flag_key()), RoutingDecision::Allow);
}

#[tokio::test]
async fn count_tokens_calls_carry_no_report_slot() {
    let provider = ReportingProvider::new(vec![ok_recording(vec![FLAG])]);
    let router = router_with(BEDROCK_P1, provider.clone());

    let counted = router.count_tokens_with_meta(req_with_betas(&[FLAG])).await;

    assert!(counted.result.is_ok());
    assert_eq!(*provider.slot_seen.lock(), vec![false]);
    assert!(counted.meta.learned_capabilities.is_empty());
    assert_eq!(beta_decision(&router, &flag_key()), RoutingDecision::Allow);
}

#[tokio::test]
async fn complete_calls_each_carry_a_report_slot() {
    // Positive control for the count_tokens case: the same provider sees a
    // slot on every messages call, including a retry.
    let provider = ReportingProvider::new(vec![
        Call {
            record: vec![],
            fail_5xx: true,
        },
        ok_recording(vec![]),
    ]);
    let router = router_with(BEDROCK_P1, provider.clone());

    let dispatched = router
        .complete_with_options(req_with_betas(&[FLAG]), RouterOptions::default())
        .await;

    assert!(dispatched.result.is_ok());
    assert_eq!(*provider.slot_seen.lock(), vec![true, true]);
}

#[tokio::test]
async fn ineligible_reported_flags_mint_nothing() {
    let disabled = format!("{BEDROCK_P1}\n[capability]\nenabled = false\n");
    let masked = format!(
        "{BEDROCK_P1}\n[capability.overrides.p1]\nforce_supported = [\"{}\"]\n",
        flag_key()
    );
    // (case, config, client betas, reported flags)
    let cases: [(&str, &str, &[&str], &[&'static str]); 4] = [
        ("dotted flag", BEDROCK_P1, &["zz.flag"], &["zz.flag"]),
        (
            "flag absent from client betas",
            BEDROCK_P1,
            &["zz-other"],
            &[FLAG],
        ),
        ("learning switch off", &disabled, &[FLAG], &[FLAG]),
        ("force_supported mask", &masked, &[FLAG], &[FLAG]),
    ];
    for (case, toml_text, client, reported) in cases {
        let provider = ReportingProvider::new(vec![ok_recording(reported.to_vec())]);
        let router = router_with(toml_text, provider);

        let dispatched = router
            .complete_with_options(req_with_betas(client), RouterOptions::default())
            .await;

        assert!(dispatched.result.is_ok(), "{case}: dispatch succeeds");
        assert!(
            dispatched.meta.learned_capabilities.is_empty(),
            "{case}: no learn event",
        );
        assert!(router.learned_capabilities.is_empty(), "{case}: no entry");
    }
}

/// Seed a lapsed acting `beta:` negative on the fixture lane and claim its
/// single re-probe, as the chain filter would for an admitted request.
fn admit_beta_probe(router: &Router) -> LearnedProbeGuard {
    let past = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .expect("test clock is well past boot");
    router
        .learned_capabilities
        .import_entries(vec![ExportedEntry {
            provider_kind: "bedrock".into(),
            state_key: LANE.into(),
            feature_key: flag_key(),
            verdict: crate::learned_capability::EntryVerdict::Negative,
            signal: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: past,
            last_seen: past,
            expires_at: past,
            evidence_class: None,
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
        }]);
    assert_eq!(
        beta_decision(router, &flag_key()),
        RoutingDecision::ProbeAdmitted
    );
    LearnedProbeGuard::armed(
        router.learned_capabilities.clone(),
        vec![ProbeAdmission {
            state_key: "m1".into(),
            learned_key: LANE.into(),
            feature: flag_key(),
            provider_kind: "bedrock",
            generation: router.registry_generation(),
        }],
        "complete",
    )
}

#[test]
fn a_reported_flag_settles_its_admitted_probe_as_a_rejection_not_a_success() {
    let provider = ReportingProvider::new(vec![ok_recording(vec![])]);
    let router = router_with(BEDROCK_P1, provider.clone());
    let target = router
        .expand_chain_to_targets(
            vec![Arc::new(ResolvedModel::new(
                "m1",
                "p1",
                provider,
                "anthropic.wire-model",
            ))],
            None,
        )
        .pop()
        .expect("one target");
    let mut guard = admit_beta_probe(&router);
    let report = routectl_core::BetaRepairReport::default();
    report.record(&[FLAG.to_string()]);
    let req = req_with_betas(&[FLAG]);
    let mut dedupe = HashSet::new();
    let mut meta = DispatchMeta::for_alias("m1");

    router.settle_attempt_success(&report, &target, &req, &mut dedupe, &mut meta, &mut guard);

    assert!(
        meta.cleared_capabilities.is_empty(),
        "the re-confirmed negative must not be cleared",
    );
    assert!(
        meta.learned_capabilities.is_empty(),
        "the probe settles; nothing is minted on top of it",
    );
    assert_eq!(router.metrics.probe_failures_total(), 1);
    let entry = router
        .learned_capabilities
        .export_entries()
        .into_iter()
        .find(|e| e.state_key == LANE && e.feature_key == flag_key())
        .expect("the negative is still resident");
    assert_eq!(entry.observations, 2, "the rejection refreshed the entry");
    assert_eq!(entry.consecutive_failed_probes, 1);
    assert!(!entry.in_flight, "the probe slot is released");
}
