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
use crate::learned_capability::{EntryVerdict, ExportedEntry};
use crate::resolved::ResolvedModel;

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
    let out =
        router.withheld_betas_for_target(target, &betas(flags), &mut admissions, Instant::now());
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
        &mut admissions,
    );

    // Assert
    assert_eq!(admissions.len(), 1);
    assert_eq!(chain[0].withheld_betas.to_vec(), vec!["fx-seed-only"]);
}
