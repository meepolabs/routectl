//! The settlement of the engine's beta repair: a `beta:<flag>` negative minted
//! when the repaired attempt succeeds, the mint gates, and the
//! settle-rejections-before-success order.

use super::*;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use futures::stream::{BoxStream, StreamExt};
use parking_lot::Mutex;
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use routectl_core::{
    BetaAssembly, ChatChunk, ChatResponse, ChunkChoice, ChunkDelta, Error, Provider, Result,
    TokenCount,
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

/// What one provider call does: whether the upstream names the call's client
/// flags in a beta rejection, or fails it with another status, or serves it.
#[derive(Clone)]
enum Call {
    /// A 400 naming these flags, when every one of them is still on the
    /// attempt's wire (not withheld); a success otherwise.
    RejectNamed(Vec<&'static str>),
    Fail(u16),
    Ok,
}

/// A Bedrock 400 `ValidationException` naming `flags` in the exact envelope.
fn beta_rejection(flags: &[&str]) -> Error {
    let list: Vec<String> = flags.iter().map(|f| format!("`{f}`")).collect();
    let message = format!(
        "Unexpected value(s) {} for the `anthropic-beta` header. Please consult our \
         documentation at platform.claude.com/docs or try again without the header.",
        list.join(", ")
    );
    Error::upstream_full(
        "p1",
        400,
        json!({ "message": message }).to_string(),
        None,
        Some("ValidationException".into()),
        None,
    )
}

/// A provider that plays a scripted call per attempt (the last entry repeats),
/// deposits an empty beta assembly the way an anthropic-vocabulary egress
/// does, and records the withheld beta set each call carried.
struct ReportingProvider {
    script: Vec<Call>,
    calls: AtomicUsize,
    withheld_seen: Mutex<Vec<Vec<String>>>,
}

impl ReportingProvider {
    fn new(script: Vec<Call>) -> Arc<Self> {
        Arc::new(Self {
            script,
            calls: AtomicUsize::new(0),
            withheld_seen: Mutex::new(Vec::new()),
        })
    }

    fn last_withheld(&self) -> Vec<String> {
        self.withheld_seen
            .lock()
            .last()
            .cloned()
            .expect("the provider was called")
    }

    fn play(&self, req: &ChatRequest) -> Result<()> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        let call = self.script[n.min(self.script.len() - 1)].clone();
        let withheld = req.routectl_internal.withheld_betas.to_vec();
        self.withheld_seen.lock().push(withheld.clone());
        if let Some(slot) = req.routectl_internal.beta_assembly.as_ref() {
            slot.record(BetaAssembly::new(Vec::new(), Vec::new()));
        }
        match call {
            Call::RejectNamed(flags) if !flags.iter().any(|f| withheld.iter().any(|w| w == f)) => {
                Err(beta_rejection(&flags))
            }
            Call::Fail(status) => Err(Error::upstream("p1", status, "upstream refused")),
            _ => Ok(()),
        }
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

/// The fixture seed: the shipped seed would withhold the fixture flag on a
/// bedrock lane exactly like this.
const FIXTURE_SEED: &[&str] = &[FLAG];

fn seeded_router_with(provider: Arc<dyn Provider>) -> Router {
    let config: Config = toml::from_str(BEDROCK_P1).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    router.set_beta_seed_for_tests(FIXTURE_SEED);
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

/// The provider saw the flag rejected once and the repaired attempt once.
fn assert_repaired_once(provider: &ReportingProvider) {
    assert_eq!(
        *provider.withheld_seen.lock(),
        vec![Vec::<String>::new(), vec![FLAG.to_string()]],
        "one rejected call carrying the flag, then one call withholding it",
    );
}

#[tokio::test]
async fn complete_success_after_a_beta_repair_mints_one_beta_negative() {
    let provider = ReportingProvider::new(vec![Call::RejectNamed(vec![FLAG])]);
    let router = router_with(BEDROCK_P1, provider.clone());

    let dispatched = router
        .complete_with_options(req_with_betas(&[FLAG]), RouterOptions::default())
        .await;

    assert!(dispatched.result.is_ok());
    assert_repaired_once(&provider);
    assert_one_beta_event(&dispatched.meta.learned_capabilities);
    assert!(matches!(
        beta_decision(&router, &flag_key()),
        RoutingDecision::RouteAway { .. }
    ));
}

#[tokio::test]
async fn stream_first_content_after_a_beta_repair_mints_one_beta_negative() {
    let provider = ReportingProvider::new(vec![Call::RejectNamed(vec![FLAG])]);
    let router = router_with(BEDROCK_P1, provider.clone());

    let dispatched = router
        .stream_with_options(req_with_betas(&[FLAG]), RouterOptions::default())
        .await;

    assert!(dispatched.result.is_ok());
    assert_repaired_once(&provider);
    assert_one_beta_event(&dispatched.meta.learned_capabilities);
    assert!(matches!(
        beta_decision(&router, &flag_key()),
        RoutingDecision::RouteAway { .. }
    ));
}

#[tokio::test]
async fn a_5xx_retry_with_no_beta_repair_learns_nothing() {
    // Control for the repaired-flag settlement: a same-target retry that
    // succeeds after a non-beta failure withheld nothing and learns nothing.
    let provider = ReportingProvider::new(vec![Call::Fail(500), Call::Ok]);
    let router = router_with(BEDROCK_P1, provider.clone());

    let dispatched = router
        .complete_with_options(req_with_betas(&[FLAG]), RouterOptions::default())
        .await;

    assert!(dispatched.result.is_ok());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2, "the 5xx retried");
    assert!(provider.last_withheld().is_empty());
    assert!(dispatched.meta.learned_capabilities.is_empty());
    assert_eq!(beta_decision(&router, &flag_key()), RoutingDecision::Allow);
}

#[tokio::test]
async fn a_count_tokens_beta_repair_serves_the_count_and_mints_nothing() {
    let provider = ReportingProvider::new(vec![Call::RejectNamed(vec![FLAG])]);
    let router = router_with(BEDROCK_P1, provider.clone());

    let counted = router.count_tokens_with_meta(req_with_betas(&[FLAG])).await;

    assert!(counted.result.is_ok(), "{:?}", counted.result.err());
    assert_repaired_once(&provider);
    assert!(counted.meta.learned_capabilities.is_empty());
    assert_eq!(beta_decision(&router, &flag_key()), RoutingDecision::Allow);
}

#[tokio::test]
async fn ineligible_repaired_flags_mint_nothing() {
    let disabled = format!("{BEDROCK_P1}\n[capability]\nenabled = false\n");
    let masked = format!(
        "{BEDROCK_P1}\n[capability.overrides.p1]\nforce_supported = [\"{}\"]\n",
        flag_key()
    );
    // (case, config, client betas, named flags, repaired)
    let cases: [(&str, &str, &[&str], &'static str, bool); 4] = [
        ("dotted flag", BEDROCK_P1, &["zz.flag"], "zz.flag", true),
        (
            "flag absent from client betas",
            BEDROCK_P1,
            &["zz-other"],
            FLAG,
            false,
        ),
        ("learning switch off", &disabled, &[FLAG], FLAG, true),
        ("force_supported mask", &masked, &[FLAG], FLAG, true),
    ];
    for (case, toml_text, client, named, repaired) in cases {
        let provider = ReportingProvider::new(vec![Call::RejectNamed(vec![named])]);
        let router = router_with(toml_text, provider.clone());

        let dispatched = router
            .complete_with_options(req_with_betas(client), RouterOptions::default())
            .await;

        assert_eq!(dispatched.result.is_ok(), repaired, "{case}: served");
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            if repaired { 2 } else { 1 },
            "{case}: upstream calls",
        );
        assert!(
            dispatched.meta.learned_capabilities.is_empty(),
            "{case}: no learn event",
        );
        assert!(router.learned_capabilities.is_empty(), "{case}: no entry");
    }
}

/// Import a lapsed acting negative for the fixture flag on the fixture lane,
/// so the next request claims its single re-probe.
fn seed_lapsed_beta_negative(router: &Router) {
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
fn a_repaired_flag_settles_its_admitted_probe_as_a_rejection_not_a_success() {
    let provider = ReportingProvider::new(vec![Call::Ok]);
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
    let repaired = [FLAG.to_string()];
    let req = req_with_betas(&[FLAG]);
    let mut dedupe = HashSet::new();
    let mut meta = DispatchMeta::for_alias("m1");

    router.settle_attempt_success(
        &repaired,
        &UntranslatedCapabilities::default(),
        &target,
        &req,
        &mut dedupe,
        &mut meta,
        &mut guard,
    );

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

#[tokio::test]
async fn a_repaired_flag_still_settles_when_a_same_target_auth_retry_serves() {
    // Streams retry the same target after a 401. The repaired attempt 401s;
    // the retry still withholds the flag and reaches first content, so the
    // success the flag was absent from settles it.
    let provider = ReportingProvider::new(vec![
        Call::RejectNamed(vec![FLAG]),
        Call::Fail(401),
        Call::Ok,
    ]);
    let router = router_with(BEDROCK_P1, provider.clone());

    let dispatched = router
        .stream_with_options(req_with_betas(&[FLAG]), RouterOptions::default())
        .await;

    assert!(dispatched.result.is_ok());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    assert_eq!(provider.last_withheld(), vec![FLAG.to_string()]);
    assert_one_beta_event(&dispatched.meta.learned_capabilities);
}

/// Which messages surface a test dispatches through.
#[derive(Debug, Clone, Copy)]
enum Surface {
    Complete,
    Stream,
}

/// The capability ride-alongs of one dispatch, without the result.
struct Ridealongs {
    learned: usize,
    cleared: usize,
    observations: Vec<crate::router::CapabilityObserveEvent>,
}

async fn dispatch_on(router: &Router, surface: Surface, req: ChatRequest) -> Ridealongs {
    let meta = match surface {
        Surface::Complete => {
            let dispatched = router
                .complete_with_options(req, RouterOptions::default())
                .await;
            assert!(dispatched.result.is_ok(), "{surface:?}: dispatch succeeds");
            dispatched.meta
        }
        Surface::Stream => {
            let dispatched = router
                .stream_with_options(req, RouterOptions::default())
                .await;
            assert!(dispatched.result.is_ok(), "{surface:?}: dispatch succeeds");
            dispatched.meta
        }
    };
    Ridealongs {
        learned: meta.learned_capabilities.len(),
        cleared: meta.cleared_capabilities.len(),
        observations: meta.capability_observations,
    }
}

#[tokio::test]
async fn the_seed_withholds_the_fixture_flag_with_no_learned_verdict() {
    // Control for the acceptance tests below: the fixture router really does
    // withhold the seeded flag, so "the next request sends it" is evidence.
    let provider = ReportingProvider::new(vec![Call::Ok]);
    let router = seeded_router_with(provider.clone());

    let _ = dispatch_on(&router, Surface::Complete, req_with_betas(&[FLAG])).await;

    assert_eq!(provider.last_withheld(), vec![FLAG.to_string()]);
}

#[tokio::test]
async fn an_accepted_beta_reprobe_records_a_positive_that_beats_the_seed() {
    for surface in [Surface::Complete, Surface::Stream] {
        // Arrange: a seeded flag whose learned negative has lapsed, so the
        // next request claims the re-probe and sends the flag.
        let provider = ReportingProvider::new(vec![Call::Ok]);
        let router = seeded_router_with(provider.clone());
        seed_lapsed_beta_negative(&router);

        // Act: the re-probe succeeds and nothing is repaired.
        let first = dispatch_on(&router, surface, req_with_betas(&[FLAG])).await;

        // Assert: the flag was sent, and the acceptance became one verified
        // observation -- not a clear.
        assert!(
            provider.last_withheld().is_empty(),
            "{surface:?}: the re-probe sends the flag",
        );
        assert_eq!(first.cleared, 0, "{surface:?}: no clear event");
        assert_eq!(first.learned, 0, "{surface:?}: no learn event");
        assert_eq!(
            first.observations.len(),
            1,
            "{surface:?}: one positive event"
        );
        let ev = &first.observations[0];
        assert_eq!(ev.state_key, LANE);
        assert_eq!(ev.capability_key, flag_key());
        assert_eq!(ev.provider_kind, "bedrock");
        assert_eq!(
            ev.direction,
            crate::capability_detect::ObservationDirection::Verified
        );
        assert_eq!(ev.evidence_class, routectl_core::capability::BETA_ACCEPTED);
        assert!(routectl_core::capability::is_known_evidence_class(
            &ev.evidence_class
        ));
        assert!(router.learned_capabilities.is_verified_working(
            LANE,
            &flag_key(),
            "bedrock",
            Instant::now()
        ));

        // Act: the next request on the same lane.
        let second = dispatch_on(&router, surface, req_with_betas(&[FLAG])).await;

        // Assert: the learned positive beats the seed, and restates nothing.
        assert!(
            provider.last_withheld().is_empty(),
            "{surface:?}: the next request sends the flag",
        );
        assert!(
            second.observations.is_empty(),
            "{surface:?}: no second positive event",
        );
    }
}

#[test]
fn a_repeated_accepted_beta_settlement_emits_no_second_event() {
    let provider = ReportingProvider::new(vec![Call::Ok]);
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
    let req = req_with_betas(&[FLAG]);
    let mut dedupe = HashSet::new();
    let empty: [String; 0] = [];

    let mut first_meta = DispatchMeta::for_alias("m1");
    let mut first_guard = admit_beta_probe(&router);
    router.settle_attempt_success(
        &empty,
        &UntranslatedCapabilities::default(),
        &target,
        &req,
        &mut dedupe,
        &mut first_meta,
        &mut first_guard,
    );
    // A second admission for the same key, now that a positive resides.
    let mut second_meta = DispatchMeta::for_alias("m1");
    let mut second_guard = LearnedProbeGuard::armed(
        router.learned_capabilities.clone(),
        vec![ProbeAdmission {
            state_key: "m1".into(),
            learned_key: LANE.into(),
            feature: flag_key(),
            provider_kind: "bedrock",
            generation: router.registry_generation(),
        }],
        "complete",
    );
    router.settle_attempt_success(
        &empty,
        &UntranslatedCapabilities::default(),
        &target,
        &req,
        &mut dedupe,
        &mut second_meta,
        &mut second_guard,
    );

    assert_eq!(first_meta.capability_observations.len(), 1);
    assert!(
        second_meta.capability_observations.is_empty(),
        "a refresh emits nothing"
    );
    assert!(second_meta.cleared_capabilities.is_empty());
    assert!(router.learned_capabilities.is_verified_working(
        LANE,
        &flag_key(),
        "bedrock",
        Instant::now()
    ));
}

/// Fill the fixture lane with its full bound of `beta:` negatives and lease
/// every one, as a purge in progress would.
fn fill_and_lease_lane(router: &Router) -> Vec<crate::learned_capability::PurgeLease> {
    use crate::learned_capability::{MAX_BETA_ENTRIES_PER_LANE, PurgePreparation};

    let registry = &router.learned_capabilities;
    (0..MAX_BETA_ENTRIES_PER_LANE)
        .map(|n| {
            let key = crate::beta_capability::beta_capability_key(&format!("zz-fill-{n}"))
                .expect("the filler flag is well formed");
            let _ = registry.observe(
                LANE,
                &key,
                "bedrock",
                SignalTier::SelfIdentifying,
                FailurePhase::F1,
                EvidenceSource::Live,
                None,
                Instant::now(),
            );
            match registry.prepare_purge(registry.generation(), LANE, &key, "bedrock") {
                PurgePreparation::Reserved(lease) => lease,
                other => panic!("expected a reservation, got {other:?}"),
            }
        })
        .collect()
}

fn flag_resident(router: &Router) -> bool {
    router
        .learned_capabilities
        .export_entries()
        .iter()
        .any(|e| e.state_key == LANE && e.feature_key == flag_key())
}

#[test]
fn a_lease_refused_beta_is_learned_by_a_same_request_retry_once_a_lease_releases() {
    // Arrange
    let provider = ReportingProvider::new(vec![Call::Ok]);
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
    let mut leases = fill_and_lease_lane(&router);
    let req = req_with_betas(&[FLAG]);
    let mut dedupe = HashSet::new();
    let settle = |dedupe: &mut HashSet<LearnDedupeKey>, meta: &mut DispatchMeta| {
        router.settle_attempt_success(
            &[FLAG.to_string()],
            &UntranslatedCapabilities::default(),
            &target,
            &req,
            dedupe,
            meta,
            &mut LearnedProbeGuard::inert(),
        );
    };

    // Act -- every resident beta on the lane is leased.
    let mut refused_meta = DispatchMeta::for_alias("m1");
    let refused_events =
        routectl_testkit::capture_events(|| settle(&mut dedupe, &mut refused_meta));

    // Assert
    assert!(!flag_resident(&router), "the refused key must stay absent");
    assert!(refused_meta.learned_capabilities.is_empty());
    assert_eq!(router.metrics.learned_negatives_f1_total(), 0);
    assert!(
        !refused_events
            .iter()
            .any(|e| e.field("event") == Some("learn")),
        "a refused observation emits no learn WARN"
    );

    // Act -- release one lease and retry with the SAME dedupe set.
    router
        .learned_capabilities
        .restore_purge(leases.pop().expect("the lane holds leases"));
    let mut learned_meta = DispatchMeta::for_alias("m1");
    settle(&mut dedupe, &mut learned_meta);

    // Assert
    assert!(flag_resident(&router), "the retry learns the key");
    assert_one_beta_event(&learned_meta.learned_capabilities);
    assert_eq!(router.metrics.learned_negatives_f1_total(), 1);
    for lease in leases {
        router.learned_capabilities.restore_purge(lease);
    }
}
