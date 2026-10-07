//! The per-target withheld beta set: its precedence chain, its probe
//! admissions, its scope, and its path onto every attempt's
//! `RoutectlInternal`.

use std::sync::Arc;
use std::time::Instant;

use futures::StreamExt;
use parking_lot::Mutex;
use routectl_core::capability::{EvidenceSource, FailurePhase, SignalTier};
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Provider, Result, TokenCount, Usage,
};
use serde_json::json;

use super::super::{DispatchTarget, Router};
use crate::beta_capability::beta_capability_key;
use crate::config::{Config, OverrideEntry, ProviderEntry};
use crate::learned_capability::{EntryVerdict, ExportedEntry, ProbeClaim};
use crate::resolved::ResolvedModel;
use crate::router::class_observe::DispatchSurface;

const BEDROCK: &str = "bedrock";

/// The fixture seed every test but the first-contact one runs under.
const FIXTURE_SEED: &[&str] = &["fx-pinned", "fx-forced", "fx-learned-pos", "fx-seed-only"];

/// Records every request a dispatch hands it, on every surface.
struct RecordingProvider {
    seen: Mutex<Vec<ChatRequest>>,
}

impl RecordingProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            seen: Mutex::new(Vec::new()),
        })
    }

    fn withheld(&self) -> Vec<Vec<String>> {
        self.seen
            .lock()
            .iter()
            .map(|req| req.routectl_internal.withheld_betas.to_vec())
            .collect()
    }
}

#[async_trait::async_trait]
impl Provider for RecordingProvider {
    fn id(&self) -> &'static str {
        "recording"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("recording", "unused"))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.seen.lock().push(req);
        Ok(ChatResponse {
            model: "wire".to_string(),
            usage: Some(Usage::default()),
            ..Default::default()
        })
    }
    async fn stream(
        &self,
        req: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk>>> {
        self.seen.lock().push(req);
        let chunk = ChatChunk {
            choices: vec![routectl_core::ChunkChoice {
                index: 0,
                delta: routectl_core::ChunkDelta {
                    content: Some("ok".into()),
                    ..Default::default()
                },
                finish_reason: None,
                matched_stop_sequence: None,
            }],
            ..Default::default()
        };
        Ok(futures::stream::iter(vec![Ok(chunk)]).boxed())
    }
    async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
        self.seen.lock().push(req);
        Ok(TokenCount {
            input_tokens: 5,
            extras: serde_json::Map::new(),
        })
    }
}

/// A router over one `bedrock` entry `bed`, one `openai-compat` entry `oc`
/// and one forwarded `anthropic-api` entry `fw`, with `extra` TOML appended.
/// Runs under [`FIXTURE_SEED`].
fn router(extra: &str) -> Router {
    let mut router = router_with_real_seed(extra);
    router.set_beta_seed_for_tests(FIXTURE_SEED);
    router
}

fn router_with_real_seed(extra: &str) -> Router {
    let body = format!(
        "version = 3\n\
         [providers.bed]\n\
         kind = \"bedrock\"\n\
         region = \"us-east-1\"\n\
         creds = {{ kind = \"default-chain\" }}\n\
         anthropic_beta = [\"fx-pinned\"]\n\
         [providers.oc]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://example.test/v1\"\n\
         api_key_ref = \"literal:k\"\n\
         [providers.fw]\n\
         kind = \"anthropic-api\"\n\
         credential_source = \"forwarded\"\n\
         {extra}"
    );
    let config: Config = toml::from_str(&body).expect("fixture config parses");
    Router::new(Arc::new(config))
}

fn capability(enabled: bool) -> String {
    format!("[capability]\nenabled = {enabled}\n")
}

fn target_on(router: &Router, provider_name: &str) -> DispatchTarget {
    let provider: Arc<dyn Provider> = RecordingProvider::new();
    let model = ResolvedModel::new("m", provider_name, provider, "anthropic.claude-test-v1:0");
    router
        .expand_chain_to_targets(vec![Arc::new(model)], None)
        .pop()
        .expect("a non-seat model expands to one target")
}

fn lane_of(target: &DispatchTarget) -> String {
    target
        .learned_key("")
        .expect("the fixture target has a lane")
        .to_string()
}

fn key(flag: &str) -> String {
    beta_capability_key(flag).expect("fixture flags are well formed")
}

fn seed_negative(router: &Router, target: &DispatchTarget, flag: &str) {
    router.learned_capabilities.observe(
        &lane_of(target),
        &key(flag),
        BEDROCK,
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        Instant::now(),
    );
}

fn seed_positive(router: &Router, target: &DispatchTarget, flag: &str) {
    router.learned_capabilities.observe_positive(
        &lane_of(target),
        &key(flag),
        BEDROCK,
        EvidenceSource::Live,
        None,
        Instant::now(),
    );
}

/// A negative whose decay has already lapsed, so the next read claims its
/// single re-probe slot.
fn seed_lapsed_negative(router: &Router, target: &DispatchTarget, flag: &str) {
    let base = Instant::now();
    router
        .learned_capabilities
        .import_entries(vec![ExportedEntry {
            provider_kind: BEDROCK.into(),
            state_key: lane_of(target),
            feature_key: key(flag),
            verdict: EntryVerdict::Negative,
            signal: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: base,
            last_seen: base,
            expires_at: base,
            evidence_class: None,
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
        }]);
}

fn betas(flags: &[&str]) -> ChatRequest {
    ChatRequest {
        model: "m".into(),
        messages: vec![].into(),
        anthropic_beta: flags.iter().map(ToString::to_string).collect(),
        ..Default::default()
    }
}

fn withheld(router: &Router, target: &DispatchTarget, flags: &[&str]) -> Vec<String> {
    let mut admissions = Vec::new();
    let out = router.withheld_betas_for_target(
        target,
        &betas(flags),
        ProbeClaim::Claim,
        &mut admissions,
        Instant::now(),
    );
    assert!(admissions.is_empty(), "no fixture here lapses a negative");
    out
}

#[test]
fn precedence_runs_pin_then_override_then_learned_then_seed() {
    // Arrange -- one flag per row, each carrying the tier under test plus
    // every weaker tier that would decide the other way.
    let overrides = "[capability.overrides.bed]\n\
                     force_supported = [\"beta:fx-forced\"]\n\
                     unsupported = [\"beta:fx-pinned\", \"beta:fx-override-off\"]\n";
    let router = router(&format!("{}{overrides}", capability(true)));
    let target = target_on(&router, "bed");
    seed_negative(&router, &target, "fx-pinned");
    seed_negative(&router, &target, "fx-forced");
    seed_positive(&router, &target, "fx-override-off");
    seed_negative(&router, &target, "fx-learned-neg");
    seed_positive(&router, &target, "fx-pos-then-neg");
    seed_negative(&router, &target, "fx-pos-then-neg");
    seed_positive(&router, &target, "fx-learned-pos");
    let rows: &[(&str, bool)] = &[
        // pin beats an unsupported override, a learned negative and the seed
        ("fx-pinned", false),
        // force_supported beats a learned negative and the seed
        ("fx-forced", false),
        // an unsupported override beats a learned positive
        ("fx-override-off", true),
        // a learned negative withholds a flag the seed does not name
        ("fx-learned-neg", true),
        // a negative observed after a positive supersedes it
        ("fx-pos-then-neg", true),
        // a learned positive beats the seed
        ("fx-learned-pos", false),
        // the seed alone withholds
        ("fx-seed-only", true),
        // no tier speaks
        ("fx-unknown", false),
    ];
    let flags: Vec<&str> = rows.iter().map(|(flag, _)| *flag).collect();

    // Act
    let out = withheld(&router, &target, &flags);

    // Assert
    for (flag, expect_withheld) in rows {
        assert_eq!(
            out.iter().any(|w| w == flag),
            *expect_withheld,
            "row {flag}: withheld set {out:?}",
        );
    }
}

#[test]
fn probe_admitted_sends_the_flag_and_yields_exactly_one_admission() {
    // Arrange -- a lapsed negative on a seeded flag.
    let router = router(&capability(true));
    let target = target_on(&router, "bed");
    seed_lapsed_negative(&router, &target, "fx-seed-only");
    let mut admissions = Vec::new();

    // Act
    let out = router.withheld_betas_for_target(
        &target,
        &betas(&["fx-seed-only"]),
        ProbeClaim::Claim,
        &mut admissions,
        Instant::now(),
    );

    // Assert -- the probe sends the flag past the seed and carries its slot.
    assert!(out.is_empty(), "the admitted probe sends the flag: {out:?}");
    assert_eq!(admissions.len(), 1);
    assert_eq!(admissions[0].feature, key("fx-seed-only"));
    assert_eq!(admissions[0].learned_key, lane_of(&target));
    assert_eq!(admissions[0].state_key, target.state_key);
    // The slot is held: a second request reads the negative as acting.
    assert_eq!(
        withheld(&router, &target, &["fx-seed-only"]),
        vec!["fx-seed-only".to_string()],
    );
}

#[test]
fn learning_switch_off_drops_learned_verdicts_but_keeps_the_seed() {
    // Arrange
    let router = router(&capability(false));
    let target = target_on(&router, "bed");
    seed_positive(&router, &target, "fx-learned-pos");
    seed_negative(&router, &target, "fx-learned-neg");
    seed_lapsed_negative(&router, &target, "fx-seed-only");

    // Act
    let out = withheld(
        &router,
        &target,
        &["fx-learned-pos", "fx-learned-neg", "fx-seed-only"],
    );

    // Assert -- seeded flags are withheld, the learned negative is ignored,
    // and no re-probe slot was claimed.
    assert_eq!(out, vec!["fx-learned-pos", "fx-seed-only"]);
}

#[test]
fn a_non_bedrock_target_gets_no_seed() {
    let router = router(&capability(true));
    let target = target_on(&router, "oc");

    let out = withheld(&router, &target, &["fx-seed-only"]);

    assert!(out.is_empty(), "the seed is bedrock-only: {out:?}");
}

#[test]
fn a_forwarded_target_withholds_nothing() {
    // Arrange -- an override that would withhold on any own-credential target.
    let router = router(&format!(
        "{}[capability.overrides.fw]\nunsupported = [\"beta:fx-seed-only\"]\n",
        capability(true),
    ));
    let target = target_on(&router, "fw");
    assert!(target.use_forwarded_credential, "premise: forwarded target");

    let out = withheld(&router, &target, &["fx-seed-only"]);

    assert!(
        out.is_empty(),
        "a forwarded target withholds nothing: {out:?}"
    );
}

/// The `bed` router with one installed model `m` whose provider records.
fn dispatch_router(seed_fixture: bool) -> (Router, Arc<RecordingProvider>) {
    let mut router = if seed_fixture {
        router("[aliases]\ndefault = \"m\"\n")
    } else {
        router_with_real_seed("[aliases]\ndefault = \"m\"\n")
    };
    let provider = RecordingProvider::new();
    let model = ResolvedModel::new("m", "bed", provider.clone(), "anthropic.claude-test-v1:0");
    router.install_resolved_models(std::iter::once(("m".to_string(), Arc::new(model))).collect());
    (router, provider)
}

#[tokio::test]
async fn the_withheld_set_reaches_every_dispatch_surface_without_feature_keys() {
    // Arrange -- a request with no tool or feature keys at all.
    let (router, provider) = dispatch_router(true);
    let req = betas(&["fx-seed-only", "fx-unknown"]);
    assert!(
        super::super::field_repair::request_feature_keys(&req).is_empty(),
        "premise: the request carries zero feature keys",
    );

    // Act
    Box::pin(router.complete(req.clone()))
        .await
        .expect("complete succeeds");
    let stream = Box::pin(router.stream(req.clone()))
        .await
        .expect("stream opens");
    let _: Vec<_> = stream.collect().await;
    Box::pin(router.count_tokens(req))
        .await
        .expect("count_tokens succeeds");

    // Assert
    let expected = vec!["fx-seed-only".to_string()];
    assert_eq!(
        provider.withheld(),
        vec![expected.clone(), expected.clone(), expected],
        "complete, stream and count_tokens each carry the set",
    );
}

/// On some Bedrock models a rejected beta comes back as a bare
/// `invalid beta flag` that names no flag, so no repair can identify what to
/// drop: the shipped seed is the only defense on first contact.
#[tokio::test]
async fn first_contact_seeded_beta_is_withheld_without_a_learned_verdict() {
    let (router, provider) = dispatch_router(false);
    let flag = crate::beta_seed::BEDROCK_BETA_SEED[0];
    assert!(
        router.learned_capability_snapshot().is_empty(),
        "premise: no learned verdict exists",
    );

    Box::pin(router.complete(betas(&[flag])))
        .await
        .expect("complete succeeds");

    assert_eq!(provider.withheld(), vec![vec![flag.to_string()]]);
}

#[tokio::test]
async fn a_probe_payload_never_carries_a_withheld_client_beta() {
    // Arrange -- an attributable anthropic-api lane with one beta withheld by
    // override and one left alone.
    let mut config = Config::default();
    config
        .providers
        .insert("p1".to_string(), ProviderEntry::anthropic_api("literal:k"));
    config.models.insert(
        "m1".to_string(),
        crate::config::ModelEntry::new("p1", "claude-sonnet-4-5"),
    );
    config.aliases.insert(
        "default".to_string(),
        crate::config::AliasValue::Single("m1".to_string()),
    );
    config.capability.overrides.insert(
        "p1".to_string(),
        OverrideEntry {
            unsupported: vec![key("fx-withheld")],
            ..OverrideEntry::default()
        },
    );
    let mut router = Router::new(Arc::new(config));
    let provider = RecordingProvider::new();
    let model = ResolvedModel::new("m1", "p1", provider.clone(), "claude-sonnet-4-5");
    router.install_resolved_models(std::iter::once(("m1".to_string(), Arc::new(model))).collect());
    let mut admitted = super::super::probe_test_support::grounding_request();
    admitted.model = "m1".into();
    admitted.anthropic_beta = vec!["fx-withheld".to_string(), "fx-kept".to_string()];

    // Act
    Box::pin(router.complete(admitted))
        .await
        .expect("complete succeeds");
    assert_eq!(
        router.run_due_probes().await,
        1,
        "premise: the admitted request activated one probe",
    );

    // Assert -- the probe body (the last recorded call) keeps the other
    // client beta, so the exclusion is specific to the withheld flag.
    let probe = provider
        .seen
        .lock()
        .last()
        .cloned()
        .expect("the probe dialed the provider");
    assert_eq!(probe.anthropic_beta, vec!["fx-kept".to_string()]);
}

#[test]
fn a_lapsed_negative_admission_is_claimed_once_per_chain_pass() {
    // Arrange
    let router = router(&capability(true));
    let target = target_on(&router, "bed");
    seed_lapsed_negative(&router, &target, "fx-unknown");
    let mut admissions = Vec::new();

    // Act -- the chain pass is the production entry.
    let chain = router.withhold_betas_on_chain(
        vec![target],
        &betas(&["fx-unknown", "fx-seed-only"]),
        DispatchSurface::Complete,
        &mut admissions,
    );

    // Assert
    assert_eq!(admissions.len(), 1);
    assert_eq!(chain[0].withheld_betas.to_vec(), vec!["fx-seed-only"]);
}

#[test]
fn each_client_beta_takes_the_registry_entries_lock_once_per_target() {
    // Arrange -- one flag per learned shape: acting negative, verified
    // positive, nothing resident.
    let router = router(&capability(true));
    let target = target_on(&router, "bed");
    seed_negative(&router, &target, "fx-unknown");
    seed_positive(&router, &target, "fx-learned-pos");
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let sink = Arc::clone(&count);
    router
        .learned_capabilities
        .set_lock_acquire_hook(Box::new(move |lock| {
            if lock == "entries" {
                sink.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }));

    // Act
    let out = withheld(
        &router,
        &target,
        &["fx-unknown", "fx-learned-pos", "fx-other"],
    );

    // Assert
    assert_eq!(out, vec!["fx-unknown".to_string()]);
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 3);
}

#[tokio::test]
async fn count_tokens_keeps_a_lapsed_negative_withheld_and_claims_no_probe() {
    // Arrange -- an unseeded flag, so only the lapsed learned negative can
    // withhold it.
    let (router, provider) = dispatch_router(true);
    let target = target_on(&router, "bed");
    seed_lapsed_negative(&router, &target, "fx-unknown");
    let flagged = vec!["fx-unknown".to_string()];

    // Act -- repeated token counts.
    for _ in 0..2 {
        Box::pin(router.count_tokens(betas(&["fx-unknown"])))
            .await
            .expect("count_tokens succeeds");
    }

    // Assert -- each count withholds the flag and none claimed the slot.
    assert_eq!(provider.withheld(), vec![flagged.clone(), flagged]);
    assert_eq!(router.metrics.probe_attempts_total(), 0);

    // Act -- an inference request on the same lane.
    Box::pin(router.complete(betas(&["fx-unknown"])))
        .await
        .expect("complete succeeds");

    // Assert -- the slot was still free, so exactly one probe sends the flag.
    assert_eq!(provider.withheld().last(), Some(&Vec::new()));
    assert_eq!(router.metrics.probe_attempts_total(), 1);
}

fn mark_seed_cleared(router: &Router, target: &DispatchTarget, flag: &str) {
    let _ = router
        .learned_capabilities
        .replay_cleared(&lane_of(target), &key(flag), BEDROCK);
}

/// An acting negative planted without passing through `observe`, so a marker
/// already on the cell stays put beside it.
fn plant_acting_negative(router: &Router, target: &DispatchTarget, flag: &str) {
    let base = Instant::now();
    router
        .learned_capabilities
        .import_entries(vec![ExportedEntry {
            provider_kind: BEDROCK.into(),
            state_key: lane_of(target),
            feature_key: key(flag),
            verdict: EntryVerdict::Negative,
            signal: SignalTier::SelfIdentifying,
            observations: 1,
            first_seen: base,
            last_seen: base,
            expires_at: base + std::time::Duration::from_hours(1),
            evidence_class: None,
            phase: FailurePhase::F1,
            source: EvidenceSource::Live,
            in_flight: false,
            consecutive_failed_probes: 0,
        }]);
}

#[test]
fn a_cleared_seed_is_sent_unless_a_learned_negative_resides() {
    // Arrange
    let router = router(&capability(true));
    let target = target_on(&router, "bed");
    mark_seed_cleared(&router, &target, "fx-seed-only");
    mark_seed_cleared(&router, &target, "fx-learned-pos");
    plant_acting_negative(&router, &target, "fx-learned-pos");
    assert!(
        router.learned_capabilities.seed_cleared(
            &lane_of(&target),
            &key("fx-learned-pos"),
            BEDROCK
        ),
        "premise: the marker sits beside the negative",
    );

    // Act
    let out = withheld(&router, &target, &["fx-seed-only", "fx-learned-pos"]);

    // Assert
    assert_eq!(out, vec!["fx-learned-pos"]);
}

#[test]
fn a_cleared_seed_is_sent_with_the_learning_switch_off() {
    let router = router(&capability(false));
    let target = target_on(&router, "bed");
    mark_seed_cleared(&router, &target, "fx-seed-only");

    let out = withheld(&router, &target, &["fx-seed-only", "fx-learned-pos"]);

    assert_eq!(out, vec!["fx-learned-pos"]);
}

#[test]
fn a_config_only_reload_keeps_a_cleared_seed() {
    // Arrange
    let before = router(&capability(true));
    let target = target_on(&before, "bed");
    mark_seed_cleared(&before, &target, "fx-seed-only");
    let mut after = router(&capability(true));
    assert_eq!(after.catalog_version, before.catalog_version);
    assert_eq!(after.overlay_revision, before.overlay_revision);

    // Act
    after.carry_over_learned_from(&before);

    // Assert
    let target = target_on(&after, "bed");
    assert!(withheld(&after, &target, &["fx-seed-only"]).is_empty());
}

/// A learned positive keeps the marker beside it: a seed-withheld flag is
/// never sent, so never re-tested, and losing the marker with the positive
/// would withhold the flag for good once the lane cap evicts that positive.
#[test]
fn a_cleared_seed_stays_sent_after_its_accepted_positive_is_evicted() {
    // Arrange -- marker, then an accepted positive on the same seeded cell.
    let router = router(&capability(true));
    let target = target_on(&router, "bed");
    let lane = lane_of(&target);
    let base = Instant::now();
    mark_seed_cleared(&router, &target, "fx-seed-only");
    let accepted = router
        .learned_capabilities
        .observe_accepted_beta_in_generation(
            router.learned_capabilities.generation(),
            &lane,
            &key("fx-seed-only"),
            BEDROCK,
            base,
        );
    assert!(accepted.value().is_some(), "premise: the positive lands");

    // Act -- newer beta negatives fill the lane past its bound.
    for n in 0..crate::learned_capability::MAX_BETA_ENTRIES_PER_LANE {
        router.learned_capabilities.observe(
            &lane,
            &key(&format!("zz-fill-{n}")),
            BEDROCK,
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            EvidenceSource::Live,
            None,
            base + std::time::Duration::from_secs(1 + u64::try_from(n).expect("small")),
        );
    }

    // Assert
    assert!(
        !router
            .learned_capabilities
            .snapshot()
            .iter()
            .any(|e| e.feature_key == key("fx-seed-only")),
        "premise: the lane cap evicted the positive",
    );
    assert!(withheld(&router, &target, &["fx-seed-only"]).is_empty());
}

fn markers_on(router: &Router) -> Vec<(String, String)> {
    router
        .learned_capabilities
        .seed_clear_snapshot()
        .into_iter()
        .map(|m| (m.state_key, m.provider_kind))
        .collect()
}

#[test]
fn the_owner_sweep_drops_a_marker_whose_entry_was_removed_and_a_re_add_does_not_restore_it() {
    // Arrange
    let before = router(&capability(true));
    let target = target_on(&before, "bed");
    mark_seed_cleared(&before, &target, "fx-seed-only");
    let without_bed: Config = toml::from_str(
        "version = 3\n\
         [providers.oc]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://example.test/v1\"\n\
         api_key_ref = \"literal:k\"\n",
    )
    .expect("fixture config parses");
    let mut removed = Router::new(Arc::new(without_bed));
    removed.set_beta_seed_for_tests(FIXTURE_SEED);

    // Act -- remove the entry, then add it back.
    removed.carry_over_learned_from(&before);
    let swept = markers_on(&removed);
    let mut re_added = router(&capability(true));
    re_added.carry_over_learned_from(&removed);

    // Assert
    assert!(swept.is_empty(), "the removal sweeps the marker: {swept:?}");
    let target = target_on(&re_added, "bed");
    assert_eq!(
        withheld(&re_added, &target, &["fx-seed-only"]),
        vec!["fx-seed-only"],
        "the re-added entry starts from the seed",
    );
}

#[test]
fn the_owner_sweep_drops_a_marker_after_a_kind_flip() {
    // Arrange
    let before = router(&capability(true));
    let target = target_on(&before, "bed");
    mark_seed_cleared(&before, &target, "fx-seed-only");
    let flipped: Config = toml::from_str(
        "version = 3\n\
         [providers.bed]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"literal:k\"\n",
    )
    .expect("fixture config parses");
    let mut reloaded = Router::new(Arc::new(flipped));
    reloaded.set_beta_seed_for_tests(FIXTURE_SEED);
    assert_eq!(markers_on(&before).len(), 1, "premise: one marker");

    // Act
    reloaded.carry_over_learned_from(&before);

    // Assert
    assert!(markers_on(&reloaded).is_empty());
}

#[test]
fn the_owner_sweep_keeps_an_owned_marker() {
    let before = router(&capability(true));
    let target = target_on(&before, "bed");
    mark_seed_cleared(&before, &target, "fx-seed-only");
    let mut reloaded = router(&capability(true));

    reloaded.carry_over_learned_from(&before);

    assert_eq!(
        markers_on(&reloaded),
        vec![(lane_of(&target), BEDROCK.to_string())]
    );
}

/// The fixture router with model `m` installed on `bed`, so the lane
/// `target_on(.., "bed")` names is one dispatch reaches.
fn routed_router() -> Router {
    let mut router = router(&capability(true));
    let model = ResolvedModel::new(
        "m",
        "bed",
        RecordingProvider::new(),
        "anthropic.claude-test-v1:0",
    );
    router.install_resolved_models(std::iter::once(("m".to_string(), Arc::new(model))).collect());
    router
}

fn purge_on(router: &Router, target: &DispatchTarget, flag: &str) -> super::super::PurgeOutcome {
    let lane = crate::state_key::StateKey::parse(&lane_of(target)).expect("fixture lane parses");
    router.reserve_learned_capability_purge(&lane, &key(flag))
}

#[test]
fn a_finalized_seed_lift_sends_the_flag_and_carries_a_zero_incarnation_clear() {
    // Arrange
    let router = routed_router();
    let target = target_on(&router, "bed");
    let generation = router.registry_generation();

    // Act
    let super::super::PurgeOutcome::SeedLift(lift) = purge_on(&router, &target, "fx-seed-only")
    else {
        panic!("premise: an unlifted seeded flag with nothing resident reserves a lift");
    };
    let settlement = lift.settlement();
    assert_eq!(
        withheld(&router, &target, &["fx-seed-only"]),
        vec!["fx-seed-only"],
        "the seed still withholds until the lift is finalized",
    );
    let events = routectl_testkit::capture_events(|| router.finalize_seed_lift(lift));

    // Assert
    assert_eq!(settlement.incarnation, 0);
    assert_eq!(settlement.persistence_generation, generation);
    assert_eq!(settlement.state_key, lane_of(&target));
    assert_eq!(settlement.capability_key, key("fx-seed-only"));
    assert_eq!(settlement.provider_kind, BEDROCK);
    assert!(withheld(&router, &target, &["fx-seed-only"]).is_empty());
    let audit: Vec<_> = events
        .iter()
        .filter(|e| e.field("event") == Some("purge"))
        .collect();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].field("removed"), Some("false"));
    assert_eq!(audit[0].field("seed_lifted"), Some("true"));
    assert!(matches!(
        purge_on(&router, &target, "fx-seed-only"),
        super::super::PurgeOutcome::Absent
    ));
}

#[test]
fn an_abandoned_seed_lift_keeps_the_seed_withholding() {
    let router = routed_router();
    let target = target_on(&router, "bed");
    let super::super::PurgeOutcome::SeedLift(lift) = purge_on(&router, &target, "fx-seed-only")
    else {
        panic!("premise: the lift reserves");
    };

    router.abandon_seed_lift(lift);

    assert_eq!(
        withheld(&router, &target, &["fx-seed-only"]),
        vec!["fx-seed-only"]
    );
    assert!(markers_on(&router).is_empty());
}

#[test]
fn purging_an_unseeded_or_non_bedrock_beta_with_nothing_resident_stays_absent() {
    let router = routed_router();
    let rows = [
        ("unseeded flag on bedrock", "bed", "fx-unknown"),
        ("seeded flag on another kind", "oc", "fx-seed-only"),
    ];

    for (name, provider, flag) in rows {
        let target = target_on(&router, provider);

        let outcome = purge_on(&router, &target, flag);

        assert!(
            matches!(outcome, super::super::PurgeOutcome::Absent),
            "{name}: expected absent",
        );
    }
    assert!(markers_on(&router).is_empty());
}

#[test]
fn purging_a_resident_seeded_negative_stays_a_learned_purge_and_lifts_the_seed() {
    // Arrange
    let router = router(&capability(true));
    let target = target_on(&router, "bed");
    plant_acting_negative(&router, &target, "fx-seed-only");

    // Act
    let super::super::PurgeOutcome::Reserved(reserved) = purge_on(&router, &target, "fx-seed-only")
    else {
        panic!("a resident negative reserves a learned purge, not a lift");
    };
    assert!(router.finalize_learned_capability_purge(reserved));

    // Assert
    assert!(withheld(&router, &target, &["fx-seed-only"]).is_empty());
    assert_eq!(
        markers_on(&router),
        vec![(lane_of(&target), BEDROCK.to_string())]
    );
}

#[test]
fn a_seed_lift_on_an_upstream_no_model_routes_is_absent() {
    // Arrange -- `bed` is configured and routes `m`, but nothing routes this
    // upstream on it.
    let router = routed_router();
    let unrouted =
        crate::state_key::StateKey::parse("bed#not-a-routed-upstream").expect("the lane parses");
    let routed = target_on(&router, "bed");
    assert!(
        matches!(
            purge_on(&router, &routed, "fx-seed-only"),
            super::super::PurgeOutcome::SeedLift(_)
        ),
        "premise: the routed lane on the same entry lifts",
    );

    // Act
    let outcome = router.reserve_learned_capability_purge(&unrouted, &key("fx-seed-only"));

    // Assert
    assert!(matches!(outcome, super::super::PurgeOutcome::Absent));
    assert!(markers_on(&router).is_empty());
}
