//! Catalog capability data never steers dispatch: a chain whose models resolve
//! baked catalog rows carrying capability data dispatches in the same order,
//! with the same strip and withhold sets, as the same chain with no catalog
//! row at all. The rows come from the real baked table, so the comparison
//! covers exactly the data a configured model resolves.

use super::capability_precedence_matrix_tests::{KIND, base_config};
use super::*;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

use routectl_core::capability::{
    EvidenceSource, FailurePhase, STRUCTURED_OUTPUT, SignalTier, WEB_SEARCH,
};
use routectl_core::{ChatChunk, ChatRequest, ChatResponse, Provider, ToolDef};
use serde_json::json;

use crate::beta_capability::beta_capability_key;
use crate::catalog::{EffectiveRow, resolve_effective_row};
use crate::catalog_overlay::CatalogOverlay;
use crate::learned_capability::{LearnKey, NegativeEvidence};
use crate::resolved::ResolvedModel;

const ALIAS: &str = "chain";
const BETA_FLAG: &str = "fx-equivalence";
/// `(nickname, upstream)` in alias order. Each upstream matches a baked
/// openai-compat row that carries capability data.
const MODELS: [(&str, &str); 3] = [
    ("routed_away", "grok-4"),
    ("stripped", "gemini-2.5-pro"),
    ("withheld", "kimi-k2"),
];

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
        unreachable!("the equivalence fixture never dispatches")
    }
    async fn stream(
        &self,
        _: ChatRequest,
    ) -> Result<futures::stream::BoxStream<'static, Result<ChatChunk>>> {
        unreachable!("the equivalence fixture never dispatches")
    }
}

/// The row the real catalog resolves for `upstream` on the fixture's kind,
/// with no operator override and an empty overlay.
fn baked_row(upstream: &str) -> EffectiveRow {
    resolve_effective_row(
        KIND,
        upstream,
        None,
        &BTreeMap::new(),
        &CatalogOverlay::default(),
    )
}

/// A router over [`MODELS`] behind one alias. `with_catalog` stamps each
/// model's baked row; without it every model resolves no catalog row.
fn router(with_catalog: bool) -> Router {
    let members = MODELS
        .iter()
        .map(|(nickname, _)| format!("\"{nickname}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let config = base_config(&format!("[aliases]\n\"{ALIAS}\" = [{members}]\n"));
    let mut router = Router::new(Arc::new(config));
    let models = MODELS
        .iter()
        .map(|&(nickname, upstream)| {
            let provider: Arc<dyn Provider> = Arc::new(StubProvider);
            let row = if with_catalog {
                baked_row(upstream)
            } else {
                EffectiveRow::Missing
            };
            let model =
                ResolvedModel::new(nickname, "p", provider, upstream).with_effective_row(row);
            (nickname.to_string(), Arc::new(model))
        })
        .collect();
    router.install_resolved_models(models);
    seed_negatives(&router);
    router
}

/// One acting self-identifying negative per model: a route-away on web
/// search, a droppable strip on the advisor tool, and a beta withhold.
fn seed_negatives(router: &Router) {
    let beta_key = beta_capability_key(BETA_FLAG).expect("a well-formed flag");
    let cells = [
        (MODELS[0].1, WEB_SEARCH, FailurePhase::F2),
        (MODELS[1].1, "advisor", FailurePhase::F1),
        (MODELS[2].1, beta_key.as_str(), FailurePhase::F1),
    ];
    for (upstream, feature, phase) in cells {
        router.learned_capabilities.observe(
            LearnKey {
                state_key: &format!("p#{upstream}"),
                feature_key_raw: feature,
                provider_kind: KIND,
            },
            NegativeEvidence {
                tier: SignalTier::SelfIdentifying,
                phase,
                source: EvidenceSource::Live,
                evidence_class: None,
            },
            Instant::now(),
        );
    }
}

/// A request carrying every feature the baked rows name, the stripped tool
/// and the withheld beta flag.
fn request() -> ChatRequest {
    ChatRequest {
        model: ALIAS.into(),
        messages: vec![].into(),
        tools: Some(vec![
            ToolDef::Other(json!({"type": "web_search"})),
            ToolDef::Other(json!({"type": "advisor"})),
        ]),
        response_format: Some(json!({"type": "json_object"})),
        anthropic_beta: vec![BETA_FLAG.to_string()],
        ..Default::default()
    }
}

/// The dispatch chain as `(state key, strip keys, withheld betas)` per target.
type Shape = Vec<(String, Vec<String>, Vec<String>)>;

fn dispatch_shape(router: &Router) -> Shape {
    let (chain, _) = router
        .dispatch_chain_for_request(&request(), DispatchSurface::Complete)
        .expect("the fixture chain resolves");
    chain
        .into_iter()
        .map(|target| {
            (
                target.state_key.clone(),
                target.strip_capabilities.to_vec(),
                target.withheld_betas.to_vec(),
            )
        })
        .collect()
}

/// Mutation check: make the feature filter tail a target whenever its catalog
/// row names the feature (a catalog read standing in for a learned verdict)
/// -> red.
#[test]
fn catalog_capability_data_leaves_order_strip_and_withhold_unchanged() {
    // Arrange -- the fixture rows really carry capability data, every value
    // true, covering each requested feature on some model.
    let mut catalog_keys: Vec<String> = Vec::new();
    for (nickname, upstream) in MODELS {
        let row = baked_row(upstream);
        let capabilities = &row.priced().expect("a baked row").capabilities;
        assert!(!capabilities.is_empty(), "{nickname}: catalog data");
        assert!(
            capabilities.values().all(|supported| *supported),
            "{nickname}: every baked capability is true: {capabilities:?}",
        );
        catalog_keys.extend(capabilities.keys().cloned());
    }
    for feature in [WEB_SEARCH, STRUCTURED_OUTPUT] {
        assert!(
            catalog_keys.iter().any(|key| key == feature),
            "the catalog names {feature}"
        );
    }
    let with_catalog = router(true);
    let without_catalog = router(false);

    // Act
    let shape = dispatch_shape(&with_catalog);
    let control = dispatch_shape(&without_catalog);

    // Assert -- the learned negatives alone decide, identically either way.
    let expected: Shape = vec![
        ("stripped".into(), vec!["advisor".into()], Vec::new()),
        ("withheld".into(), Vec::new(), vec![BETA_FLAG.into()]),
        ("routed_away".into(), Vec::new(), Vec::new()),
    ];
    assert_eq!(control, expected, "the learned verdicts shape the chain");
    assert_eq!(shape, control, "catalog data changes nothing on the wire");
}
