//! Filter-seam tests for the operator override consult: legacy static
//! lists keep their provenance labels, new override entries hard-drop
//! or mask, and a `force_supported` mask precedes probe admission.
use super::*;
use crate::config::Config;
use crate::resolved::ResolvedModel;
use crate::router::chain::into_one_dispatch_target;
use routectl_core::{ChatChunk, ChatResponse, Provider};
use std::sync::Arc;

/// Minimal provider stub so the fixtures can build a real
/// `Arc<ResolvedModel>`; none of its methods are exercised here.
struct StubProvider;

#[async_trait::async_trait]
impl Provider for StubProvider {
    fn id(&self) -> &'static str {
        "stub"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(serde_json::json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("stub", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        unreachable!("override filter tests never dispatch")
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("override filter tests never dispatch")
    }
}

fn override_router_from_toml(body: &str) -> Router {
    let config: Config = toml::from_str(&format!("version = 3\n{body}")).expect("config parses");
    Router::new(Arc::new(config))
}

/// A minimal openai-compat dispatch target keyed by `provider:nickname`.
/// `into_one_dispatch_target` leaves `provider_kind` unset (the legacy
/// path), so the kind is pinned here for the override / learned consult.
fn override_test_target(provider_name: &str, nickname: &str) -> DispatchTarget {
    let provider: Arc<dyn Provider> = Arc::new(StubProvider);
    let model = Arc::new(ResolvedModel::new(
        nickname,
        provider_name,
        provider,
        "upstream",
    ));
    let mut target = into_one_dispatch_target(model);
    target.provider_kind = Some("openai-compat");
    target
}

const OVERRIDE_PROVIDER_P: &str = "[providers.p]\n\
        kind = \"openai-compat\"\n\
        base_url = \"https://x\"\n\
        api_key_ref = \"literal:k\"\n";

#[test]
fn override_consult_legacy_provider_list_hard_drops_with_provider_label() {
    // Arrange -- a legacy per-provider list. The registry preserves its
    // ProviderStatic provenance so the consult reports the same
    // `provider` source label the raw scan always did.
    let router = override_router_from_toml(&format!(
        "{OVERRIDE_PROVIDER_P}unsupported_features = [\"web_search\"]\n"
    ));
    let target = override_test_target("p", "nick");

    // Act
    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let verdict = router.unsupported_feature_for_target(
        &target,
        &["web_search".to_string()],
        &mut admissions,
        &mut strip_keys,
    );

    // Assert
    assert_eq!(
        verdict,
        Some(("web_search".to_string(), FilterSource::ProviderStatic)),
    );
    assert_eq!(FilterSource::ProviderStatic.as_str(), "provider");
    assert!(admissions.is_empty());
    assert!(strip_keys.is_empty());
}

#[test]
fn override_consult_legacy_model_list_hard_drops_with_model_label() {
    // Arrange -- a legacy per-model list keyed by `provider:nickname`.
    let router = override_router_from_toml(&format!(
        "{OVERRIDE_PROVIDER_P}\
             [models.nick]\n\
             provider = \"p\"\n\
             upstream = \"gpt-x\"\n\
             unsupported_features = [\"computer_use\"]\n"
    ));
    let target = override_test_target("p", "nick");

    // Act
    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let verdict = router.unsupported_feature_for_target(
        &target,
        &["computer_use".to_string()],
        &mut admissions,
        &mut strip_keys,
    );

    // Assert
    assert_eq!(
        verdict,
        Some(("computer_use".to_string(), FilterSource::ModelStatic)),
    );
    assert_eq!(FilterSource::ModelStatic.as_str(), "model");
}

#[test]
fn override_unsupported_hard_drops_and_empties_chain_like_legacy_list() {
    // Arrange -- a NEW `[capability.overrides]` unsupported entry.
    let router = override_router_from_toml(&format!(
        "{OVERRIDE_PROVIDER_P}\
             [capability.overrides.p]\n\
             unsupported = [\"web_search\"]\n"
    ));
    let target = override_test_target("p", "nick");

    // Act / Assert -- the consult reports the `override` label and
    // hard-drops just as a static list does.
    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    assert_eq!(
        router.unsupported_feature_for_target(
            &target,
            &["web_search".to_string()],
            &mut admissions,
            &mut strip_keys,
        ),
        Some(("web_search".to_string(), FilterSource::Override)),
    );
    assert_eq!(FilterSource::Override.as_str(), "override");

    // The sole target hard-drops, so the chain filters to empty and
    // surfaces the learned-tail NotImplemented (501) -- byte-identical to a legacy
    // static list emptying the chain.
    let mut chain_admissions = Vec::new();
    match router.filter_chain_by_features(
        vec![target],
        &["web_search".to_string()],
        "alias-x",
        &mut chain_admissions,
    ) {
        Err(Error::NotImplemented(_, _)) => {}
        Err(other) => panic!("expected NotImplemented, got {other:?}"),
        Ok(_) => panic!("an override-unsupported sole target must empty the chain"),
    }
}

#[test]
fn force_supported_flips_acting_learned_route_away_to_allow() {
    use routectl_core::capability::{FailurePhase, SignalTier};
    use std::time::Instant;

    // Arrange -- capability enabled with a self-identifying (acting-now)
    // negative on the target's state_key, plus a force_supported mask
    // for the same capability.
    let masked = override_router_from_toml(&format!(
        "{OVERRIDE_PROVIDER_P}\
             [capability]\n\
             enabled = true\n\
             [capability.overrides.p]\n\
             force_supported = [\"web_search\"]\n"
    ));
    let target = override_test_target("p", "nick");
    masked.learned_capabilities.observe(
        "nick",
        "web_search",
        "openai-compat",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        Instant::now(),
    );

    // Act
    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let verdict = masked.unsupported_feature_for_target(
        &target,
        &["web_search".to_string()],
        &mut admissions,
        &mut strip_keys,
    );

    // Assert -- the mask suppresses the acting negative: the feature is
    // Allowed (None), not routed away.
    assert_eq!(
        verdict, None,
        "force_supported must flip the negative to Allow"
    );

    // Contrast: the SAME acting negative without the mask routes away
    // with the learned source, proving the mask is what flipped it.
    let unmasked = override_router_from_toml(&format!(
        "{OVERRIDE_PROVIDER_P}[capability]\nenabled = true\n"
    ));
    unmasked.learned_capabilities.observe(
        "nick",
        "web_search",
        "openai-compat",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        Instant::now(),
    );
    let mut ctrl_admissions = Vec::new();
    let mut ctrl_strip = Vec::new();
    assert_eq!(
        unmasked.unsupported_feature_for_target(
            &override_test_target("p", "nick"),
            &["web_search".to_string()],
            &mut ctrl_admissions,
            &mut ctrl_strip,
        ),
        Some(("web_search".to_string(), FilterSource::Learned)),
    );
}

#[test]
fn force_supported_mask_admits_no_probe_where_unmasked_would() {
    use routectl_core::capability::{FailurePhase, SignalTier};
    use std::time::Instant;

    // A zero-hour decay lapses an observed negative immediately, so the
    // next consult would claim a re-probe slot.
    let base = "[providers.p]\n\
            kind = \"openai-compat\"\n\
            base_url = \"https://x\"\n\
            api_key_ref = \"literal:k\"\n\
            [capability]\n\
            enabled = true\n\
            decay_hours = 0\n\
            inferred_window_hours = 0\n";

    // Control -- unmasked: the lapsed negative admits exactly one probe.
    let control = override_router_from_toml(base);
    control.learned_capabilities.observe(
        "nick",
        "web_search",
        "openai-compat",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        Instant::now(),
    );
    let mut ctrl_admissions = Vec::new();
    let mut ctrl_strip = Vec::new();
    let _ = control.unsupported_feature_for_target(
        &override_test_target("p", "nick"),
        &["web_search".to_string()],
        &mut ctrl_admissions,
        &mut ctrl_strip,
    );
    assert_eq!(
        ctrl_admissions.len(),
        1,
        "control: a lapsed negative must admit a re-probe",
    );

    // Masked: the force_supported short-circuit precedes
    // acting_negative_for, so a masked cell never claims a probe slot.
    let masked = override_router_from_toml(&format!(
        "{base}[capability.overrides.p]\n\
                 force_supported = [\"web_search\"]\n"
    ));
    masked.learned_capabilities.observe(
        "nick",
        "web_search",
        "openai-compat",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        Instant::now(),
    );
    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let verdict = masked.unsupported_feature_for_target(
        &override_test_target("p", "nick"),
        &["web_search".to_string()],
        &mut admissions,
        &mut strip_keys,
    );
    assert_eq!(verdict, None, "masked feature must Allow");
    assert!(
        admissions.is_empty(),
        "a masked cell must not claim a re-probe slot",
    );
}

#[test]
fn override_route_away_beats_learned_strip_for_non_overridden_precedence() {
    use routectl_core::capability::{FailurePhase, SignalTier};
    use std::time::Instant;

    // A per-provider override routes `web_search` away; a droppable
    // learned negative on `advisor` would otherwise strip in place.
    // Override RouteAway is consulted first, so it hard-drops (returns
    // the override label) ahead of the learned strip decision -- and the
    // non-overridden `advisor` cell keeps its strip-in-place behavior when
    // web_search is absent.
    let router = override_router_from_toml(&format!(
        "{OVERRIDE_PROVIDER_P}\
             [capability]\n\
             enabled = true\n\
             [capability.overrides.p]\n\
             unsupported = [\"web_search\"]\n"
    ));
    let target = override_test_target("p", "nick");
    router.learned_capabilities.observe(
        "nick",
        "advisor",
        "openai-compat",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        Instant::now(),
    );

    // With web_search present, the override hard-drops first.
    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    assert_eq!(
        router.unsupported_feature_for_target(
            &target,
            &["advisor".to_string(), "web_search".to_string()],
            &mut admissions,
            &mut strip_keys,
        ),
        Some(("web_search".to_string(), FilterSource::Override)),
    );
    assert!(strip_keys.is_empty(), "a hard-drop leaves strip_keys empty");

    // Without web_search, the non-overridden advisor cell still strips
    // in place (behavior unchanged): None with the advisor key.
    let mut advisor_admissions = Vec::new();
    let mut advisor_strip = Vec::new();
    assert_eq!(
        router.unsupported_feature_for_target(
            &target,
            &["advisor".to_string()],
            &mut advisor_admissions,
            &mut advisor_strip,
        ),
        None,
    );
    assert_eq!(advisor_strip, vec!["advisor".to_string()]);
}

/// Feature acceptance -- legacy-config filter-decision equivalence.
///
/// One config carrying ALL three legacy capability lists (a per-provider
/// `unsupported_features`, a per-model `unsupported_features`, and the
/// `[bedrock]` egress allowlists `allowed_betas` / `allowed_body_fields`
/// -- inert for routing but present so the whole legacy surface coexists)
/// must route away with the SAME `FilterSource` labels the earlier raw
/// static-list scan produced: a provider-scoped drop reports
/// `ProviderStatic` (`"provider"`) and a model-scoped drop reports
/// `ModelStatic` (`"model"`). Absolute expected labels, not a diff
/// against a rebuilt old binary. The egress-byte half of this acceptance
/// bar lives in `routectl-providers`
/// (`tests/legacy_capability_config_equivalence.rs`).
#[test]
fn legacy_config_lists_route_away_with_pre_f3_source_labels() {
    // Arrange -- every legacy list in one config.
    let router = override_router_from_toml(
        "[providers.p]\n\
             kind = \"openai-compat\"\n\
             base_url = \"https://x\"\n\
             api_key_ref = \"literal:k\"\n\
             unsupported_features = [\"web_search\"]\n\
             [models.nick]\n\
             provider = \"p\"\n\
             upstream = \"gpt-x\"\n\
             unsupported_features = [\"computer_use\"]\n\
             [bedrock]\n\
             allowed_betas = [\"some-beta\"]\n\
             allowed_body_fields = [\"messages\", \"anthropic_version\", \"max_tokens\"]\n",
    );
    let target = override_test_target("p", "nick");

    // Act / Assert -- the provider-scoped list keeps the `provider` label.
    let mut admissions = Vec::new();
    let mut strip_keys = Vec::new();
    let provider_verdict = router.unsupported_feature_for_target(
        &target,
        &["web_search".to_string()],
        &mut admissions,
        &mut strip_keys,
    );
    assert_eq!(
        provider_verdict,
        Some(("web_search".to_string(), FilterSource::ProviderStatic)),
    );
    assert_eq!(FilterSource::ProviderStatic.as_str(), "provider");

    // The model-scoped list keeps the `model` label.
    let mut model_admissions = Vec::new();
    let mut model_strip = Vec::new();
    let model_verdict = router.unsupported_feature_for_target(
        &target,
        &["computer_use".to_string()],
        &mut model_admissions,
        &mut model_strip,
    );
    assert_eq!(
        model_verdict,
        Some(("computer_use".to_string(), FilterSource::ModelStatic)),
    );
    assert_eq!(FilterSource::ModelStatic.as_str(), "model");

    // A request touching no listed feature passes through: no route-away,
    // no probe admission, no strip -- byte-identical to the legacy
    // no-match path.
    let mut clean_admissions = Vec::new();
    let mut clean_strip = Vec::new();
    assert_eq!(
        router.unsupported_feature_for_target(
            &target,
            &["structured_output".to_string()],
            &mut clean_admissions,
            &mut clean_strip,
        ),
        None,
    );
    assert!(clean_admissions.is_empty());
    assert!(clean_strip.is_empty());
}
