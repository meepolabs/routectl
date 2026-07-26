//! Consolidated within-target capability precedence matrix. One AAA test
//! per rule of the settled chain
//! `override hard-drop > force_supported mask > learned (F1/F2) > prior >
//! unknown`, plus the never-empty two-pass invariants, the F2-never-strips
//! guard, the same-chain-F1 F2 suppression, and demoted-target strip-key
//! survival. Sibling sidecars pin adjacent slices; this module is the single
//! place the whole precedence chain is asserted end to end.

use super::*;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use routectl_core::capability::{FailurePhase, SignalTier};
use routectl_core::failure_class::FailureClass;
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Provider, ToolDef};
use serde_json::json;

use super::super::capability_learn::{
    LearnDedupeKey, f2_class_is_deterministic, f2_evidence_is_mintable,
};
use super::super::{DispatchMeta, LearnedProbeGuard};
use crate::catalog::{CatalogRow, EffectiveRow, Source};
use crate::config::Config;
use crate::resolved::ResolvedModel;

/// Minimal provider stub; the matrix drives the filter decision seam
/// directly and never dispatches, so none of these methods run.
struct StubProvider;

#[async_trait::async_trait]
impl Provider for StubProvider {
    fn id(&self) -> &'static str {
        "stub"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("stub", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!("precedence matrix tests never dispatch")
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("precedence matrix tests never dispatch")
    }
}

/// The stable provider-kind these tests key learned negatives and overrides
/// on. Matches the `[providers.p]` kind in [`base_router`].
const KIND: &str = "openai-compat";

/// A router with the capability subsystem enabled and one `openai-compat`
/// provider `p`. `extra` appends further TOML tables (override cells, static
/// per-model lists) after the `[capability]` table.
fn base_router(extra: &str) -> Router {
    let body = format!(
        "version = 3\n\
         [providers.p]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://x\"\n\
         api_key_ref = \"literal:k\"\n\
         [capability]\n\
         enabled = true\n\
         {extra}"
    );
    let config: Config = toml::from_str(&body).expect("config parses");
    Router::new(Arc::new(config))
}

/// A dispatch target on provider `p` carrying the given catalog capability
/// priors on its baked effective row. `expand_chain_to_targets` fills the
/// provider kind from config, so the prior + learned passes both run.
fn target_with_priors(router: &Router, nickname: &str, priors: &[(&str, bool)]) -> DispatchTarget {
    let mut row = CatalogRow::sentinel();
    for (key, value) in priors {
        row.capabilities.insert((*key).to_string(), *value);
    }
    let provider: Arc<dyn Provider> = Arc::new(StubProvider);
    let model = ResolvedModel::new(nickname, "p", provider, "upstream").with_effective_row(
        EffectiveRow::Present {
            row,
            source: Source::Baked,
            verified_at: "seed".to_string(),
        },
    );
    router
        .expand_chain_to_targets(vec![Arc::new(model)], None)
        .pop()
        .expect("one target for a non-seat model")
}

/// Seed an acting (self-identifying) learned negative for `(nickname, feature)`
/// in the given detection phase.
fn seed_learned(router: &Router, nickname: &str, feature: &str, phase: FailurePhase) {
    router.learned_capabilities.observe(
        nickname,
        feature,
        KIND,
        SignalTier::SelfIdentifying,
        phase,
        Instant::now(),
    );
}

fn req_with_tool(tool_type: &str) -> ChatRequest {
    ChatRequest {
        model: "nick".into(),
        messages: vec![].into(),
        tools: Some(vec![ToolDef::Other(json!({ "type": tool_type }))]),
        ..Default::default()
    }
}

// --- override hard-drop > learned ---

#[test]
fn override_route_away_beats_acting_learned_negative() {
    // Arrange -- an override `unsupported` cell AND an acting learned negative
    // both name `web_search`.
    let router = base_router(
        "[capability.overrides.p]\n\
         unsupported = [\"web_search\"]\n",
    );
    let target = target_with_priors(&router, "nick", &[]);
    seed_learned(&router, "nick", "web_search", FailurePhase::F1);

    // Act
    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let verdict = router.unsupported_feature_for_target(
        &target,
        &["web_search".to_string()],
        &mut admissions,
        &mut strip_keys,
    );

    // Assert -- the override consult runs first, so it hard-drops with the
    // `override` label ahead of any learned signal.
    assert_eq!(
        verdict,
        Some(("web_search".to_string(), FilterSource::Override)),
    );
    assert!(strip_keys.is_empty(), "a hard-drop carries no strip keys");
    assert!(admissions.is_empty(), "a hard-drop admits no probe");
}

// --- force_supported masks learned AND prior ---

#[test]
fn force_supported_masks_both_learned_negative_and_catalog_prior() {
    // Arrange -- `web_search` carries BOTH an acting learned negative and a
    // `Some(false)` catalog prior, with a force_supported mask over it.
    let router = base_router(
        "[capability.overrides.p]\n\
         force_supported = [\"web_search\"]\n",
    );
    let target = target_with_priors(&router, "nick", &[("web_search", false)]);
    seed_learned(&router, "nick", "web_search", FailurePhase::F1);

    // Act
    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let verdict = router.unsupported_feature_for_target(
        &target,
        &["web_search".to_string()],
        &mut admissions,
        &mut strip_keys,
    );

    // Assert -- the mask short-circuits the learned pass and is skipped again
    // by the prior pass, so neither signal survives.
    assert_eq!(
        verdict, None,
        "force_supported masks both the learned negative and the catalog prior",
    );
    assert!(
        admissions.is_empty(),
        "a masked cell never claims a re-probe slot",
    );
    assert!(strip_keys.is_empty());
}

// --- learned (F1 and F2) > prior ---

#[test]
fn learned_f1_negative_outranks_catalog_prior() {
    // Arrange -- an acting F1 learned negative and a `Some(false)` prior both
    // name the essential `web_search`.
    let router = base_router("");
    let target = target_with_priors(&router, "nick", &[("web_search", false)]);
    seed_learned(&router, "nick", "web_search", FailurePhase::F1);

    // Act
    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let verdict = router.unsupported_feature_for_target(
        &target,
        &["web_search".to_string()],
        &mut admissions,
        &mut strip_keys,
    );

    // Assert -- the learned pass claims the feature and routes it away with the
    // `learned` label; the weaker prior never re-decides it.
    assert_eq!(
        verdict,
        Some(("web_search".to_string(), FilterSource::Learned)),
    );
}

#[test]
fn learned_f2_negative_outranks_catalog_prior() {
    // Arrange -- an acting F2 learned negative and a `Some(false)` prior both
    // name the essential `web_search`.
    let router = base_router("");
    let target = target_with_priors(&router, "nick", &[("web_search", false)]);
    seed_learned(&router, "nick", "web_search", FailurePhase::F2);

    // Act
    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let verdict = router.unsupported_feature_for_target(
        &target,
        &["web_search".to_string()],
        &mut admissions,
        &mut strip_keys,
    );

    // Assert -- an F2 negative routes away (never strips) with the `learned`
    // label, still outranking the catalog prior.
    assert_eq!(
        verdict,
        Some(("web_search".to_string(), FilterSource::Learned)),
    );
    assert!(strip_keys.is_empty(), "an F2 negative never strips");
}

// --- prior=false soft-tails; prior=true allows; None permissive ---

#[test]
fn prior_false_alone_soft_tails_with_prior_label() {
    // Arrange -- only a `Some(false)` prior, no learned negative.
    let router = base_router("");
    let target = target_with_priors(&router, "nick", &[("web_search", false)]);

    // Act
    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let verdict = router.unsupported_feature_for_target(
        &target,
        &["web_search".to_string()],
        &mut admissions,
        &mut strip_keys,
    );

    // Assert -- the prior soft-tails the target with the `prior` label and
    // never contributes a strip key.
    assert_eq!(
        verdict,
        Some(("web_search".to_string(), FilterSource::Prior))
    );
    assert!(
        strip_keys.is_empty(),
        "a prior never contributes a strip key"
    );
}

#[test]
fn prior_true_is_permissive_noop() {
    // Arrange -- a `Some(true)` prior asserting support.
    let router = base_router("");
    let target = target_with_priors(&router, "nick", &[("web_search", true)]);

    // Act
    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let verdict = router.unsupported_feature_for_target(
        &target,
        &["web_search".to_string()],
        &mut admissions,
        &mut strip_keys,
    );

    // Assert -- a supporting prior is a no-op.
    assert_eq!(verdict, None);
}

#[test]
fn absent_prior_is_permissive_noop() {
    // Arrange -- no prior for the queried feature (absent key = NO PRIOR,
    // distinct from `Some(false)`).
    let router = base_router("");
    let target = target_with_priors(&router, "nick", &[]);

    // Act
    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let verdict = router.unsupported_feature_for_target(
        &target,
        &["web_search".to_string()],
        &mut admissions,
        &mut strip_keys,
    );

    // Assert -- an absent prior leaves the feature open.
    assert_eq!(verdict, None);
}

// --- never-empty two-pass ---

#[test]
fn all_lanes_demoted_leaves_a_non_empty_chain() {
    // Arrange -- every lane carries a `Some(false)` prior for the requested
    // feature, so all demote to the tail.
    let router = base_router("");
    let a = target_with_priors(&router, "a", &[("web_search", false)]);
    let b = target_with_priors(&router, "b", &[("web_search", false)]);

    // Act
    let out = router
        .filter_chain_by_features(
            vec![a, b],
            &["web_search".to_string()],
            "alias",
            &mut Vec::new(),
        )
        .expect("a fully demoted chain still survives via the tail");

    // Assert -- both lanes survive in the de-prioritized tail; the chain is
    // never emptied by soft demotion.
    let order: Vec<&str> = out.iter().map(|t| t.state_key.as_str()).collect();
    assert_eq!(order, vec!["a", "b"]);
}

#[test]
fn one_hard_drop_with_rest_demoted_leaves_a_non_empty_chain() {
    // Arrange -- one lane is hard-dropped by a per-model static list; the
    // other is soft-demoted by a `Some(false)` prior.
    let router = base_router(
        "[models.dropme]\n\
         provider = \"p\"\n\
         upstream = \"gpt-x\"\n\
         unsupported_features = [\"web_search\"]\n",
    );
    let hard = target_with_priors(&router, "dropme", &[]);
    let soft = target_with_priors(&router, "keep", &[("web_search", false)]);

    // Act
    let out = router
        .filter_chain_by_features(
            vec![hard, soft],
            &["web_search".to_string()],
            "alias",
            &mut Vec::new(),
        )
        .expect("the demoted survivor keeps the chain non-empty");

    // Assert -- the static hard-drop is removed; the prior-demoted lane
    // survives in the tail.
    let order: Vec<&str> = out.iter().map(|t| t.state_key.as_str()).collect();
    assert_eq!(order, vec!["keep"]);
}

// --- F2 never strips ---

#[test]
fn f2_negative_on_droppable_capability_routes_away_and_strips_nothing() {
    // Arrange -- `advisor` is a droppable tool-shape strip, but its acting
    // negative was detected in F2 (a feature-naming fault, not a wire token).
    let router = base_router("");
    let target = target_with_priors(&router, "nick", &[]);
    seed_learned(&router, "nick", "advisor", FailurePhase::F2);

    // Act
    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let verdict = router.unsupported_feature_for_target(
        &target,
        &["advisor".to_string()],
        &mut admissions,
        &mut strip_keys,
    );

    // Assert -- an F2 negative never strips even a droppable capability; the
    // whole target routes away.
    assert_eq!(
        verdict,
        Some(("advisor".to_string(), FilterSource::Learned))
    );
    assert!(
        strip_keys.is_empty(),
        "an F2 negative is not a droppable wire token",
    );
}

// --- same-chain F1 suppresses F2 ---

#[test]
fn same_chain_f1_suppresses_a_later_f2_candidate() {
    // Arrange (pure predicate half) -- the F2 mintability gate: only a
    // self-identifying, deterministic-class candidate is mintable.
    assert!(f2_evidence_is_mintable(
        SignalTier::SelfIdentifying,
        &FailureClass::BadRequest
    ));
    assert!(!f2_evidence_is_mintable(
        SignalTier::Inferred,
        &FailureClass::BadRequest
    ));
    assert!(f2_class_is_deterministic(&FailureClass::BadRequest));
    assert!(!f2_class_is_deterministic(&FailureClass::RateLimited));

    // Arrange (mint-pipeline half) -- an F1 negative for `web_search` was
    // already observed earlier in this attempt chain (F1Seen resident).
    let router = base_router("");
    let target = target_with_priors(&router, "nick", &[]);
    let mut dedupe = HashSet::new();
    dedupe.insert(LearnDedupeKey::F1Seen {
        feature_key: "web_search".to_string(),
    });
    let mut meta = DispatchMeta::for_alias("nick");
    let mut guard = LearnedProbeGuard::inert();
    let req = req_with_tool("web_search");
    let err = Error::upstream_full("p", 400, "{}".to_string(), None, None, None);

    // Act -- drive the mint pipeline with a provisional F2 resolution.
    let events = routectl_testkit::capture_events(|| {
        router.commit_learned_observation(
            (
                "web_search".to_string(),
                SignalTier::SelfIdentifying,
                FailurePhase::F2,
            ),
            &FailureClass::BadRequest,
            &err,
            400,
            None,
            KIND,
            &target,
            &req,
            false,
            &mut dedupe,
            &mut meta,
            &mut guard,
        );
    });

    // Assert -- the same-chain F1 suppresses the F2 candidate: nothing mints,
    // one deduped suppression WARN fires, and the suppression counter bumps.
    assert!(
        meta.learned_capabilities.is_empty(),
        "a same-chain-F1 F2 candidate must never mint",
    );
    assert!(
        router.learned_capabilities.is_empty(),
        "no registry entry for a suppressed F2 candidate",
    );
    assert_eq!(router.metrics.learned_negatives_f2_total(), 0);
    assert_eq!(router.metrics.f2_same_chain_suppressed_total(), 1);
    let suppressions: Vec<_> = events
        .iter()
        .filter(|e| e.field("event") == Some("suppression") && e.field("phase") == Some("f2"))
        .collect();
    assert_eq!(suppressions.len(), 1);
    assert_eq!(suppressions[0].field("capability_key"), Some("web_search"));
}

// --- demoted-target strip-key survival ---

#[test]
fn prior_demoted_target_keeps_learned_f1_strip_keys() {
    // Arrange -- `advisor` carries an acting F1 strip negative (a droppable
    // wire token) while `web_search` carries a `Some(false)` prior with no
    // learned evidence, so the prior pass demotes the whole target.
    let router = base_router("");
    let target = target_with_priors(&router, "nick", &[("web_search", false)]);
    seed_learned(&router, "nick", "advisor", FailurePhase::F1);

    // Act
    let out = router
        .filter_chain_by_features(
            vec![target],
            &["advisor".to_string(), "web_search".to_string()],
            "alias",
            &mut Vec::new(),
        )
        .expect("the prior-demoted lane survives in the tail");

    // Assert -- the lane is demoted for the prior yet still carries the F1
    // strip key, so a demoted lane that gets attempted strips its known token.
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].state_key, "nick");
    let strip: Vec<&str> = out[0]
        .strip_capabilities
        .iter()
        .map(String::as_str)
        .collect();
    assert_eq!(strip, vec!["advisor"]);
}
