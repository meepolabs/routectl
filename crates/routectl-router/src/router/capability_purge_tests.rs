//! Coverage for the operator-initiated learned-capability purge delegate:
//! removal of one keyed resident entry, the cleared settlement it hands the
//! caller to persist, the distinguishable clean no-op on an absent key, the
//! provider-kind derivation that builds the registry key, and the scope
//! boundary (learned entries only -- an operator override cell is never
//! touched, and no baked provenance is invented).

use super::*;

use std::sync::Arc;
use std::time::Instant;

use routectl_core::capability::{
    EvidenceSource, FailurePhase, SignalTier, THINKING, Verdict, WEB_SEARCH,
};
use serde_json::Value;

use crate::config::{Config, ModelEntry, OverrideEntry, ProviderEntry};

/// The provider kind Stage 1 exercises. Its capability-key normalization is
/// the identity, which is why every fixture here plants under it: a raw key
/// and its normalized form coincide, so an assertion about the delegate is
/// not really an assertion about a provider quirk.
const ANTHROPIC_API: &str = "anthropic-api";

/// A router with one `anthropic-api` provider and `nicknames` routing to it,
/// so the state-key -> provider-kind derivation has real config to resolve.
fn router_with_models(nicknames: &[&str]) -> Router {
    let mut config = Config::default();
    config.providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
    );
    for nickname in nicknames {
        config.models.insert(
            (*nickname).to_string(),
            ModelEntry::new("anthropic", "claude-sonnet-4-5"),
        );
    }
    let mut router = Router::new(Arc::new(config));
    // The RESOLVED table, not just the config tables: the shared
    // `provider_kind_for_state_key` resolves a state key through
    // `override_identity_for`, which reads resolved models -- the same table the
    // learn path keys on when it mints the entry this purge removes. A fixture
    // that configured a model without resolving it would ask the purge to
    // address a target no dispatch could ever have produced.
    let mut models: std::collections::BTreeMap<String, Arc<crate::resolved::ResolvedModel>> =
        std::collections::BTreeMap::new();
    for nickname in nicknames {
        let provider: Arc<dyn routectl_core::Provider> = Arc::new(PurgeNoopProvider);
        models.insert(
            (*nickname).to_string(),
            Arc::new(crate::resolved::ResolvedModel::new(
                *nickname,
                "anthropic",
                provider,
                "claude-sonnet-4-5",
            )),
        );
    }
    router.install_resolved_models(models);
    router
}

/// A provider that is never invoked: these tests read and mutate the learned
/// registry only, so the resolved table needs a placeholder to be well-formed.
struct PurgeNoopProvider;

#[async_trait::async_trait]
impl routectl_core::Provider for PurgeNoopProvider {
    fn id(&self) -> &'static str {
        "anthropic"
    }
    fn normalize_request(&self, _: &routectl_core::ChatRequest) -> routectl_core::Result<Value> {
        Ok(Value::Null)
    }
    fn normalize_response(&self, _: Value) -> routectl_core::Result<routectl_core::ChatResponse> {
        Err(routectl_core::Error::normalize_response(
            "anthropic",
            "unused",
        ))
    }
    async fn complete(
        &self,
        _: routectl_core::ChatRequest,
    ) -> routectl_core::Result<routectl_core::ChatResponse> {
        Err(routectl_core::Error::upstream("anthropic", 500, "unused"))
    }
    async fn stream(
        &self,
        _: routectl_core::ChatRequest,
    ) -> routectl_core::Result<
        futures::stream::BoxStream<'static, routectl_core::Result<routectl_core::ChatChunk>>,
    > {
        Err(routectl_core::Error::upstream("anthropic", 500, "unused"))
    }
}

/// Plant one acting self-identifying negative for `(state_key, capability)`,
/// keyed exactly the way the learn path keys it.
fn plant_negative(router: &Router, state_key: &str, capability: &str) {
    router.learned_capabilities.observe(
        state_key,
        capability,
        ANTHROPIC_API,
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        // develop's `observe` gained an evidence-class argument; a learned
        // negative carries none (only positive / suspect rows do).
        None,
        Instant::now(),
    );
}

/// Whether a resident entry exists for `(state_key, capability)`.
fn resident(router: &Router, state_key: &str, capability: &str) -> bool {
    router
        .learned_capability_snapshot()
        .iter()
        .any(|e| e.state_key == state_key && e.feature_key == capability)
}

/// Run the FULL purge protocol synchronously and report what happened, so the
/// tests below read as one action while exercising the real two-phase shape.
///
/// The durable commit is what licenses the finalize in production. These tests
/// are about the REGISTRY half -- key derivation, normalization, scope, lane
/// isolation -- so the commit is represented by the caller's decision to
/// finalize, and the handler tests own the durability ordering against a real
/// writer. A reservation is always settled here (finalize or abandon), so no
/// test can leak a lease into its siblings.
struct PurgeReport {
    removed: bool,
    state_key: String,
    capability_key: String,
    provider_kind: String,
    settlement: Option<CapabilityClearedEvent>,
}

fn purge(router: &Router, state_key: &str, capability_key: &str) -> PurgeReport {
    match router.reserve_learned_capability_purge(state_key, capability_key) {
        PurgeOutcome::Reserved(reserved) => {
            let settlement = reserved.settlement();
            let state_key = reserved.state_key.clone();
            let capability_key = reserved.capability_key.clone();
            let provider_kind = reserved.provider_kind.clone();
            let removed = router.finalize_learned_capability_purge(reserved);
            PurgeReport {
                removed,
                state_key,
                capability_key,
                provider_kind,
                settlement: Some(settlement),
            }
        }
        PurgeOutcome::Absent => PurgeReport {
            removed: false,
            state_key: state_key.to_string(),
            capability_key: routectl_core::capability::normalize_capability_key(
                capability_key,
                router.provider_kind_for_state_key(state_key),
            ),
            provider_kind: router.provider_kind_for_state_key(state_key).to_string(),
            settlement: None,
        },
        other => panic!(
            "these tests drive a single-threaded registry on the live generation, so a purge \
             must resolve reserved or absent; got {}",
            match other {
                PurgeOutcome::Busy => "busy",
                PurgeOutcome::Stale => "stale",
                _ => "unreachable",
            }
        ),
    }
}

#[test]
fn purging_a_resident_negative_removes_it_and_reports_the_removal() {
    // Arrange
    let router = router_with_models(&["sonnet"]);
    plant_negative(&router, "sonnet", WEB_SEARCH);
    assert!(
        resident(&router, "sonnet", WEB_SEARCH),
        "premise: the negative must be resident before the purge, or the \
         assertion below passes for the wrong reason"
    );

    // Act
    let report = purge(&router, "sonnet", WEB_SEARCH);

    // Assert
    assert!(report.removed, "a resident entry must report removed");
    assert!(
        !resident(&router, "sonnet", WEB_SEARCH),
        "the purged entry must no longer be resident"
    );
}

#[test]
fn purging_a_resident_negative_yields_a_cleared_settlement_to_persist() {
    // Arrange
    let router = router_with_models(&["sonnet"]);
    plant_negative(&router, "sonnet", WEB_SEARCH);

    // Act
    let report = purge(&router, "sonnet", WEB_SEARCH);
    let settlement = report
        .settlement
        .clone()
        .expect("a removal must yield a cleared settlement");

    // Assert: the settlement's keys are exactly what the warm rebuild needs to
    // remove the same entry on the next boot.
    assert_eq!(settlement.state_key, "sonnet");
    assert_eq!(settlement.capability_key, WEB_SEARCH);
    assert_eq!(settlement.provider_kind, ANTHROPIC_API);
}

#[test]
fn purging_an_absent_key_is_a_clean_no_op_with_no_settlement() {
    // Arrange: a registry holding a DIFFERENT capability, so "nothing removed"
    // is a statement about the requested key rather than about an empty
    // registry.
    let router = router_with_models(&["sonnet"]);
    plant_negative(&router, "sonnet", WEB_SEARCH);

    // Act
    let report = purge(&router, "sonnet", THINKING);

    // Assert
    assert!(!report.removed, "an absent key must report no removal");
    assert!(
        report.settlement.is_none(),
        "a clean no-op must not manufacture a cleared settlement to persist"
    );
    assert!(
        resident(&router, "sonnet", WEB_SEARCH),
        "the no-op must leave the unrelated resident entry alone"
    );
}

#[test]
fn a_purge_report_carries_the_normalized_capability_key() {
    // Arrange
    let router = router_with_models(&["sonnet"]);
    plant_negative(&router, "sonnet", WEB_SEARCH);

    // Act
    let report = purge(&router, "sonnet", WEB_SEARCH);

    // Assert: the report's key is the one the registry keys on, which is what
    // makes it safe to log and to persist.
    assert_eq!(
        report.capability_key,
        routectl_core::capability::normalize_capability_key(WEB_SEARCH, ANTHROPIC_API),
    );
    assert_eq!(report.state_key, "sonnet");
}

#[test]
fn purging_one_lane_leaves_the_same_capability_on_another_lane_resident() {
    // Arrange: the same capability learned on two lanes.
    let router = router_with_models(&["front", "back"]);
    plant_negative(&router, "front", WEB_SEARCH);
    plant_negative(&router, "back", WEB_SEARCH);

    // Act
    let report = purge(&router, "front", WEB_SEARCH);

    // Assert
    assert!(report.removed);
    assert!(!resident(&router, "front", WEB_SEARCH));
    assert!(
        resident(&router, "back", WEB_SEARCH),
        "a keyed purge must not widen into a sibling lane"
    );
}

#[test]
fn a_purged_negative_leaves_no_resident_verdict_of_any_kind() {
    // Arrange
    let router = router_with_models(&["sonnet"]);
    plant_negative(&router, "sonnet", WEB_SEARCH);
    let before = router.learned_capability_snapshot();
    let entry = before
        .iter()
        .find(|e| e.state_key == "sonnet")
        .expect("premise: planted entry resident");
    assert!(
        matches!(entry.verdict, Verdict::LearnedBroken(_)),
        "premise: the planted entry must be an acting negative, else the \
         post-purge assertion is vacuous"
    );

    // Act
    let _ = purge(&router, "sonnet", WEB_SEARCH);

    // Assert: nothing in the registry can steer routing for this key anymore.
    assert!(
        router
            .learned_capability_snapshot()
            .iter()
            .all(|e| !(e.state_key == "sonnet" && e.feature_key == WEB_SEARCH)),
        "a purged key must leave no resident verdict of any kind"
    );
}

/// The provider kind is DERIVED from the router's own config, never taken from
/// the caller. A caller-supplied kind would be a second source of truth for
/// the normalization that builds the registry key: get it wrong and the purge
/// addresses a different key than the learn path minted, so it reads as a
/// clean no-op while the entry stays resident and keeps steering routing.
#[test]
fn the_provider_kind_is_derived_from_the_configured_target() {
    // Arrange
    let router = router_with_models(&["sonnet"]);
    plant_negative(&router, "sonnet", WEB_SEARCH);

    // Act: the caller names only the target and the capability.
    let report = purge(&router, "sonnet", WEB_SEARCH);

    // Assert
    assert_eq!(report.provider_kind, ANTHROPIC_API);
    assert!(
        report.removed,
        "the derived key must address the real entry"
    );
}

/// A provider-scoped state key (a legacy or directly-constructed target with
/// no model scope) resolves the provider's own kind.
#[test]
fn a_provider_scoped_state_key_derives_that_providers_kind() {
    // Arrange
    let router = router_with_models(&["sonnet"]);
    plant_negative(&router, "anthropic", WEB_SEARCH);

    // Act
    let report = purge(&router, "anthropic", WEB_SEARCH);

    // Assert
    assert_eq!(report.provider_kind, ANTHROPIC_API);
    assert!(report.removed);
}

/// A pooled seat's state key is `nickname#label`; the derivation recovers the
/// base model so a seat-keyed entry resolves the same kind its base model does.
#[test]
fn a_pooled_seat_state_key_derives_the_base_models_provider_kind() {
    // Arrange
    let router = router_with_models(&["sonnet"]);
    plant_negative(&router, "sonnet#second", WEB_SEARCH);

    // Act
    let report = purge(&router, "sonnet#second", WEB_SEARCH);

    // Assert
    assert_eq!(report.provider_kind, ANTHROPIC_API);
    assert!(report.removed);
}

/// An unrecognized target is not an error: the registry can hold an entry for a
/// lane whose config entry the operator has since removed, and purging that
/// stale entry is exactly what an operator wants. The derivation falls back to
/// the empty kind, which normalizes as the identity -- the same fallback the
/// ledger replay bridge relies on for an already-normalized key.
#[test]
fn an_unconfigured_target_still_purges_under_the_identity_normalization() {
    // Arrange
    let router = router_with_models(&["sonnet"]);
    plant_negative(&router, "orphan", WEB_SEARCH);

    // Act
    let report = purge(&router, "orphan", WEB_SEARCH);

    // Assert
    assert!(report.provider_kind.is_empty());
    assert!(report.removed);
    assert!(!resident(&router, "orphan", WEB_SEARCH));
}

/// Scope boundary. The purge acts on LEARNED entries only: it never edits,
/// shadows, or invents an operator override or a baked catalog prior. An
/// operator who wants a durable decision edits the override configuration --
/// which is exactly why this asserts the override registry is untouched.
#[test]
fn purging_a_learned_entry_does_not_touch_the_operator_override_registry() {
    // Arrange: an override cell plus a learned negative on the same key.
    let mut config = Config::default();
    config.providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
    );
    config.capability.overrides.insert(
        "anthropic".to_string(),
        OverrideEntry {
            unsupported: vec![WEB_SEARCH.to_string()],
            force_supported: Vec::new(),
        },
    );
    let router = Router::new(Arc::new(config));
    plant_negative(&router, "anthropic", WEB_SEARCH);
    let before = router
        .override_registry
        .resolve("anthropic", "", WEB_SEARCH, ANTHROPIC_API)
        .map(|(verdict, _)| verdict);
    assert!(
        before.is_some(),
        "premise: the override cell must resolve before the purge, else this \
         guard cannot observe a change to it"
    );

    // Act
    let report = purge(&router, "anthropic", WEB_SEARCH);

    // Assert
    assert!(report.removed, "the learned entry itself must be purged");
    let after = router
        .override_registry
        .resolve("anthropic", "", WEB_SEARCH, ANTHROPIC_API)
        .map(|(verdict, _)| verdict);
    assert_eq!(
        before, after,
        "a learned purge must leave the operator override cell exactly as it was"
    );
}

// --- Provider-kind resolution when a configured model failed to build ---

/// A router whose model is CONFIGURED but absent from the resolved table --
/// exactly what a provider that failed to build leaves behind. The learn path
/// still keys entries on such a target, so the purge must still address them.
fn router_with_unresolved_model(nickname: &str, kind: &str) -> Router {
    let mut config = Config::default();
    let entry = match kind {
        "bedrock" => bedrock_provider_entry(),
        _ => ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
    };
    config.providers.insert("prov".to_string(), entry);
    config
        .models
        .insert(nickname.to_string(), ModelEntry::new("prov", "upstream-id"));
    // Deliberately NO install_resolved_models: the build failed, so the
    // resolved table has no row for this nickname while `[models]` still does.
    Router::new(Arc::new(config))
}

/// A Bedrock provider entry, for the normalization regression below. Bedrock is
/// the one kind whose capability-key normalization is not the identity, so it is
/// the only kind that can expose a wrong provider-kind resolution.
#[cfg(feature = "bedrock")]
fn bedrock_provider_entry() -> ProviderEntry {
    use crate::config::{BedrockApiShapeConfig, BedrockCredsConfig, ProviderRuntimePolicy};

    ProviderEntry::Bedrock {
        region: "us-west-2".into(),
        api_shape: BedrockApiShapeConfig::Invoke,
        creds: BedrockCredsConfig::DefaultChain,
        user_agent: None,
        header_extras: std::collections::BTreeMap::new(),
        payload_extras: None,
        anthropic_beta: vec![],
        cache_capability: None,
        auto_emit_top_level_breakpoint: None,
        auto_emit_per_block_breakpoints: None,
        reduction_enabled: None,
        runtime: ProviderRuntimePolicy::default(),
    }
}

#[cfg(not(feature = "bedrock"))]
fn bedrock_provider_entry() -> ProviderEntry {
    ProviderEntry::anthropic_api(crate::test_secret::file_ref("k"))
}

/// An exact model nickname absent from the resolved table still resolves its
/// configured provider's kind.
///
/// A provider that fails to build leaves its models configured but unresolved,
/// and the learn path keys entries on those targets regardless -- the negative
/// it mints is precisely the evidence an operator would purge. Falling through
/// to the provider-name lookup yields the empty kind, which silently changes
/// the registry key on any provider whose normalization is not the identity.
#[test]
fn an_unresolved_model_nickname_resolves_its_configured_provider_kind() {
    // Arrange
    let router = router_with_unresolved_model("sonnet", "anthropic-api");

    // Act + Assert
    assert_eq!(
        router.provider_kind_for_state_key("sonnet"),
        ANTHROPIC_API,
        "a configured-but-unresolved model must resolve through \
         `[models].provider` rather than falling through to the provider-name \
         lookup"
    );
}

/// The pooled-seat form of the same key resolves through its base model.
#[test]
fn an_unresolved_pooled_seat_resolves_its_base_models_kind() {
    // Arrange
    let router = router_with_unresolved_model("sonnet", "anthropic-api");

    // Act + Assert
    assert_eq!(
        router.provider_kind_for_state_key("sonnet#second"),
        ANTHROPIC_API,
        "a pooled seat on a configured-but-unresolved model must recover its \
         base model's provider kind"
    );
}

/// The RESOLVED identity stays authoritative when present: a live row wins over
/// the config tables, so a reload that repointed a nickname is honoured.
#[test]
fn a_resolved_model_identity_remains_authoritative() {
    // Arrange: resolved under `anthropic`, while `[models]` names a DIFFERENT
    // provider. The resolved row is the live truth.
    let mut config = Config::default();
    config.providers.insert(
        "anthropic".to_string(),
        ProviderEntry::anthropic_api(crate::test_secret::file_ref("k")),
    );
    config.providers.insert(
        "other".to_string(),
        ProviderEntry::openai_compat("http://127.0.0.1:1", crate::test_secret::file_ref("k")),
    );
    config
        .models
        .insert("sonnet".to_string(), ModelEntry::new("other", "upstream"));
    let mut router = Router::new(Arc::new(config));
    let mut models: std::collections::BTreeMap<String, Arc<crate::resolved::ResolvedModel>> =
        std::collections::BTreeMap::new();
    let provider: Arc<dyn routectl_core::Provider> = Arc::new(PurgeNoopProvider);
    models.insert(
        "sonnet".to_string(),
        Arc::new(crate::resolved::ResolvedModel::new(
            "sonnet",
            "anthropic",
            provider,
            "upstream",
        )),
    );
    router.install_resolved_models(models);

    // Act + Assert
    assert_eq!(
        router.provider_kind_for_state_key("sonnet"),
        ANTHROPIC_API,
        "a resolved row is the live identity and must win over `[models]`"
    );
}

/// A PROVIDER-scoped state key still resolves that provider's kind -- the
/// third shape, checked after the two model shapes.
#[test]
fn a_provider_scoped_key_still_resolves_through_the_provider_name() {
    // Arrange
    let router = router_with_unresolved_model("sonnet", "anthropic-api");

    // Act + Assert
    assert_eq!(
        router.provider_kind_for_state_key("prov"),
        ANTHROPIC_API,
        "a provider-scoped key resolves through the provider name"
    );
}

/// An unknown key resolves to the empty kind, which is inert in normalization.
#[test]
fn an_unknown_state_key_resolves_to_the_inert_empty_kind() {
    let router = router_with_unresolved_model("sonnet", "anthropic-api");
    assert_eq!(router.provider_kind_for_state_key("nothing-like-this"), "");
}

// --- Canary-state reset on finalize (field-namespace keys only) ---

/// A finalized purge of a field-namespace key must drop its resident canary
/// state -- otherwise a later re-learn of the same identity would inherit a
/// stale confirmation count or cadence from the incarnation the operator just
/// removed.
#[test]
fn purging_a_field_negative_resets_its_canary_state() {
    use crate::field_capability::field_capability_key;
    use crate::field_verdict::FieldVerdictKey;

    // Arrange
    let router = router_with_models(&["sonnet"]);
    let field_key = field_capability_key("thinking.enabled.display")
        .expect("a qualified dotted path mints a key");
    plant_negative(&router, "sonnet", &field_key);
    let canary_key = FieldVerdictKey::new("sonnet", "thinking.enabled.display", ANTHROPIC_API)
        .expect("a qualified path mints a canary key");
    router
        .field_verdicts
        .canaries()
        .acknowledge_confirmation(&canary_key, 1, 3);
    assert!(
        router
            .field_verdicts
            .canaries()
            .snapshot(&canary_key)
            .is_some(),
        "premise: canary state must be resident before the purge, or the \
         assertion below passes for the wrong reason"
    );

    // Act
    let report = purge(&router, "sonnet", &field_key);

    // Assert
    assert!(report.removed, "premise: the field negative must be purged");
    assert!(
        router
            .field_verdicts
            .canaries()
            .snapshot(&canary_key)
            .is_none(),
        "finalizing a field-key purge must drop its resident canary state"
    );
}

/// A finalized purge of a catalog-scoped key must leave canary state alone:
/// canary state exists only for field-namespace identities, and the reset
/// must be scoped to those, never widen to a capability-key coincidence.
#[test]
fn purging_a_catalog_scoped_key_does_not_touch_canary_state() {
    use crate::field_verdict::FieldVerdictKey;

    // Arrange: canary state resident under a directly-constructed key that
    // shares the purged identity's `(state_key, provider_kind)` but carries
    // the catalog-scoped capability name -- a shape only reachable directly
    // in a test, since the normal constructor rejects a non-field path, but
    // exactly the shape the scope guard in `finalize_learned_capability_purge`
    // must reject on the capability key alone.
    let router = router_with_models(&["sonnet"]);
    plant_negative(&router, "sonnet", WEB_SEARCH);
    let canary_key = FieldVerdictKey::from_capability_key(
        "sonnet".to_string(),
        WEB_SEARCH.to_string(),
        ANTHROPIC_API.to_string(),
    );
    router
        .field_verdicts
        .canaries()
        .acknowledge_confirmation(&canary_key, 1, 3);

    // Act
    let report = purge(&router, "sonnet", WEB_SEARCH);

    // Assert
    assert!(
        report.removed,
        "premise: the catalog-scoped negative must purge"
    );
    let snap = router
        .field_verdicts
        .canaries()
        .snapshot(&canary_key)
        .expect("a catalog-scoped purge must not reset canary state");
    assert_eq!(snap.confirmations, 3);
}

/// THE regression this resolution exists for: a Bedrock learned key survives
/// into a Router where its model is configured but unresolved, and a purge by
/// the RAW dotted key still removes it.
///
/// Bedrock is the one kind whose normalization is not the identity -- it reduces
/// a dotted request-bag key to a single segment. So the resolved kind is what
/// decides which string the registry keyed on. Resolve it as empty and the
/// purge normalizes the operator's raw dotted key to itself, looks up a key the
/// learn path never wrote, and reports a clean no-op while the negative stays
/// resident and keeps steering routing -- the worst answer available, because it
/// tells the operator the entry is gone.
#[cfg(feature = "bedrock")]
#[test]
fn a_bedrock_dotted_key_purges_on_an_unresolved_model() {
    use routectl_core::capability::normalize_capability_key;

    // Arrange: the raw key an upstream names, and the reduced form the registry
    // stores for a bedrock target.
    let raw = "additionalModelRequestFields.thinking";
    let normalized = normalize_capability_key(raw, "bedrock");
    assert_ne!(
        normalized, raw,
        "premise: bedrock normalization must actually reduce this key, or the \
         regression it guards cannot occur"
    );

    let router = router_with_unresolved_model("bedrock-nick", "bedrock");
    assert_eq!(
        router.provider_kind_for_state_key("bedrock-nick"),
        "bedrock",
        "premise: the unresolved bedrock model must resolve its kind, else the \
         purge below succeeds for the wrong reason"
    );
    // Plant the way the learn path does: under the bedrock kind, so the stored
    // key is the REDUCED one.
    router.learned_capabilities.observe(
        "bedrock-nick",
        raw,
        "bedrock",
        SignalTier::SelfIdentifying,
        FailurePhase::F1,
        EvidenceSource::Live,
        None,
        Instant::now(),
    );
    assert!(
        resident(&router, "bedrock-nick", &normalized),
        "premise: the registry must hold the REDUCED key"
    );

    // Act: the operator purges by the RAW dotted key they saw in the log.
    let report = purge(&router, "bedrock-nick", raw);

    // Assert
    assert!(
        report.removed,
        "the raw dotted key must normalize the same way the learn path did and \
         remove the resident entry"
    );
    assert_eq!(report.capability_key, normalized);
    assert!(!resident(&router, "bedrock-nick", &normalized));
}
