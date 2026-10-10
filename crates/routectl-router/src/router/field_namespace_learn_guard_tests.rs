//! The envelope-field key namespace is minted only by the field-repair guard.
//! A client-chosen tool type spelled in that namespace, echoed back by a
//! Bedrock validation 400, must not become a learned capability on the
//! generic learn path: no learn event, no learned row, no resident verdict.

use super::*;
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use futures::stream::BoxStream;
use routectl_core::capability::{FailurePhase, SignalTier};
use routectl_core::failure_class::FailureClass;
use routectl_core::{ChatChunk, ChatResponse, Provider, Result, ToolDef, UpstreamBody};
use serde_json::{Value, json};

use crate::config::Config;
use crate::field_verdict::FieldVerdictKey;
use crate::resolved::ResolvedModel;
use crate::router::RouterOptions;
use crate::router::capability_learn::LearnedMint;

const ALIAS: &str = "namespace-guard-alias";
const WIRE: &str = "anthropic.claude-sonnet-4-5-20250929-v1:0";
const FIELD_PATH: &str = "thinking.enabled.display";

/// The field-namespace key a client can spell as a tool type, minted through
/// the namespace owner so this file carries no second prefix literal.
fn field_key() -> String {
    crate::field_capability::field_capability_key(FIELD_PATH).expect("a qualified path")
}

/// The captured pydantic tool-tag rejection shape, echoing `tool_type`.
fn tool_tag_rejection_body(tool_type: &str) -> String {
    json!({
        "message": format!(
            "tools.0: Input tag '{tool_type}' found using 'type' does not match any of \
             the expected tags: 'bash_20250124', 'custom', 'memory_20250818'"
        )
    })
    .to_string()
}

struct EchoingBedrockSeat {
    tool_type: String,
}

impl EchoingBedrockSeat {
    fn rejection(&self) -> Error {
        Error::upstream_with_body(
            "bedrock-mock",
            400,
            UpstreamBody::new(tool_tag_rejection_body(&self.tool_type)),
            None,
            Some("ValidationException".to_string()),
            None,
        )
    }
}

#[async_trait::async_trait]
impl Provider for EchoingBedrockSeat {
    fn id(&self) -> &'static str {
        "bedrock-mock"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("bedrock-mock", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(self.rejection())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(self.rejection())
    }
}

fn bedrock_router(tool_type: &str) -> Router {
    let config: Config = toml::from_str(&format!(
        "[providers.p0]\nkind = \"bedrock\"\nregion = \"us-east-1\"\n\
         creds = {{ kind = \"default-chain\" }}\n\
         [aliases]\n\"{ALIAS}\" = [\"m0\"]\n"
    ))
    .expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let seat: Arc<dyn Provider> = Arc::new(EchoingBedrockSeat {
        tool_type: tool_type.to_string(),
    });
    router.install_resolved_models(BTreeMap::from([(
        "m0".to_string(),
        Arc::new(ResolvedModel::new("m0", "p0", seat, WIRE)),
    )]));
    router
}

fn request_with_tool_type(tool_type: &str) -> ChatRequest {
    ChatRequest {
        model: ALIAS.into(),
        messages: vec![].into(),
        tools: Some(vec![ToolDef::Other(json!({ "type": tool_type }))]),
        ..Default::default()
    }
}

/// The control: an ordinary tool type through the same seat and rejection
/// shape IS learned, so the refusal below is about the namespace and not a
/// fixture that never reaches the learn path.
#[tokio::test]
async fn an_ordinary_echoed_tool_type_is_learned_on_bedrock() {
    let router = bedrock_router("web_search_20250305");

    let dispatched = router
        .complete_with_options(
            request_with_tool_type("web_search_20250305"),
            RouterOptions::default(),
        )
        .await;

    let learned: Vec<&str> = dispatched
        .meta
        .learned_capabilities
        .iter()
        .map(|event| event.capability_key.as_str())
        .collect();
    assert_eq!(learned, vec!["web_search"]);
}

/// The one-segment path the dotted rows embed: Bedrock keeps a single segment,
/// so a dotted type can only ever be stored as a one-segment field key.
const SEGMENT_PATH: &str = "thinking";

/// The tool types that reach the field namespace on a Bedrock lane, each with
/// the field path the registry would store it under: the key itself, and the
/// dotted spellings Bedrock key normalization reduces to the embedded field
/// key (head segment, or the segment after a request-bag prefix).
fn field_namespace_tool_types() -> Vec<(String, &'static str)> {
    let segment = crate::field_capability::field_capability_key(SEGMENT_PATH).expect("a path");
    vec![
        (field_key(), FIELD_PATH),
        (format!(".{segment}.enabled"), SEGMENT_PATH),
        (format!("toolConfig.{segment}"), SEGMENT_PATH),
        (
            format!("additionalModelRequestFields.{segment}"),
            SEGMENT_PATH,
        ),
    ]
}

/// Premise for the dotted rows: the registry would store each one under the
/// field key of its stored path, so only the namespace refusals stand between
/// it and a resident field verdict.
#[test]
fn bedrock_normalization_reduces_each_row_to_its_field_key() {
    for (tool_type, stored_path) in field_namespace_tool_types() {
        assert_eq!(
            routectl_core::capability::normalize_capability_key(&tool_type, "bedrock"),
            crate::field_capability::field_capability_key(stored_path).expect("a path"),
            "{tool_type}",
        );
    }
}

/// Mutation checks: remove every namespace refusal on the path
/// (`tool_type_key`, `commit_learned_observation`, the generic mint, and the
/// registry's `observe_in_generation_with_observations`) -> red (a learn
/// event, a resident row, an acting field verdict). Any one refusal alone
/// keeps this green, so each has its own unit pin.
#[tokio::test]
async fn a_field_namespace_tool_type_echoed_by_bedrock_learns_nothing() {
    for (tool_type, stored_path) in field_namespace_tool_types() {
        let router = bedrock_router(&tool_type);

        let dispatched = router
            .complete_with_options(request_with_tool_type(&tool_type), RouterOptions::default())
            .await;

        assert!(
            dispatched.result.is_err(),
            "{tool_type}: the rejection stands"
        );
        assert!(
            dispatched.meta.learned_capabilities.is_empty(),
            "{tool_type}: no learn event may carry a field-namespace key: {:?}",
            dispatched
                .meta
                .learned_capabilities
                .iter()
                .map(|event| &event.capability_key)
                .collect::<Vec<_>>(),
        );
        let field_rows: Vec<String> = router
            .learned_capability_snapshot()
            .into_iter()
            .map(|entry| entry.feature_key)
            .filter(|key| crate::field_capability::capability_key_is_field_verdict(key))
            .collect();
        assert!(
            field_rows.is_empty(),
            "{tool_type}: no field-namespace row may be resident: {field_rows:?}",
        );
        let lane = crate::state_key::StateKey::parse(&format!("p0#{WIRE}")).expect("lane");
        let verdict = FieldVerdictKey::new(&lane, stored_path, "bedrock").expect("identity");
        assert!(
            !router
                .field_verdicts()
                .is_negative_acting(&verdict, Instant::now()),
            "{tool_type}: no field verdict may become resident",
        );
    }
}

/// The generic learn path refuses a field-namespace key even when the resolver
/// hands it one the request's derived vocabulary carries.
///
/// Mutation check: remove the namespace refusal in
/// `commit_learned_observation`, the one in `mint_learned_negative`, AND the
/// registry's in `observe_in_generation_with_observations` -> red (one learn
/// event). Each alone is pinned by its own row.
#[test]
fn the_generic_learn_entry_refuses_a_field_namespace_key() {
    let router = bedrock_router("unused");
    let model: Arc<ResolvedModel> = router
        .resolved_models
        .values()
        .next()
        .map(Arc::clone)
        .expect("one model");
    let target = router
        .expand_chain_to_targets(vec![model], None)
        .pop()
        .expect("one target");
    let mut meta = DispatchMeta::for_alias(ALIAS);
    // A request that carries the grounded field, so the act-side vocabulary
    // contains the key and only the namespace refusal can stop the mint.
    let mut req = request_with_tool_type("web_search");
    req.routectl_internal.anthropic_thinking_display = Some("summarized".into());
    req.reasoning = Some(routectl_core::ReasoningConfig {
        enabled: Some(true),
        ..Default::default()
    });
    assert!(
        super::field_repair::request_feature_keys(&req).contains(&field_key()),
        "premise: the membership gate would admit the key",
    );

    router.commit_learned_observation(
        (field_key(), SignalTier::SelfIdentifying, FailurePhase::F1),
        &FailureClass::BadRequest,
        &Error::upstream("bedrock-mock", 400, "{}"),
        400,
        None,
        "bedrock",
        &target,
        &req,
        false,
        &mut HashSet::new(),
        &mut meta,
        &mut LearnedProbeGuard::inert(),
    );

    assert!(meta.learned_capabilities.is_empty());
    assert!(router.learned_capability_snapshot().is_empty());
}

/// The learn entry's refusal runs before any of its own side effects, not only
/// before the mint: a field-namespace key arriving as an F2 candidate after a
/// same-chain F1 must not bump the suppression counter the mint's own refusal
/// would never reach.
///
/// Mutation check: remove the namespace refusal in
/// `commit_learned_observation` -> red (the counter reads 1). The mint keeps
/// its own refusal, so this is the row that pins the learn entry alone.
#[test]
fn the_generic_learn_entry_refuses_before_its_own_side_effects() {
    let router = bedrock_router("unused");
    let target = only_target(&router);
    let mut req = request_with_tool_type("web_search");
    req.routectl_internal.anthropic_thinking_display = Some("summarized".into());
    req.reasoning = Some(routectl_core::ReasoningConfig {
        enabled: Some(true),
        ..Default::default()
    });
    let mut dedupe = HashSet::from([super::capability_learn::LearnDedupeKey::F1Seen {
        feature_key: field_key(),
    }]);
    let mut meta = DispatchMeta::for_alias(ALIAS);

    router.commit_learned_observation(
        (field_key(), SignalTier::SelfIdentifying, FailurePhase::F2),
        &FailureClass::BadRequest,
        &Error::upstream("bedrock-mock", 400, "{}"),
        400,
        None,
        "bedrock",
        &target,
        &req,
        false,
        &mut dedupe,
        &mut meta,
        &mut LearnedProbeGuard::inert(),
    );

    assert_eq!(router.metrics.f2_same_chain_suppressed_total(), 0);
    assert!(meta.learned_capabilities.is_empty());
}

/// The single target of the namespace-guard fixture's one model.
fn only_target(router: &Router) -> DispatchTarget {
    let model: Arc<ResolvedModel> = router
        .resolved_models
        .values()
        .next()
        .map(Arc::clone)
        .expect("one model");
    router
        .expand_chain_to_targets(vec![model], None)
        .pop()
        .expect("one target")
}

/// The generic mint refuses a key whose Bedrock-normalized form is in the field
/// namespace, whichever learner hands it over.
///
/// Mutation check: remove the refusal at the top of `mint_learned_negative`
/// AND the registry's in `observe_in_generation_with_observations` -> red (one
/// learn event, one resident field row). The registry refusal alone is pinned
/// in its own module.
#[test]
fn the_generic_mint_refuses_a_key_bedrock_stores_in_the_field_namespace() {
    let router = bedrock_router("unused");
    let target = only_target(&router);
    let segment = crate::field_capability::field_capability_key(SEGMENT_PATH).expect("a path");
    let dotted = format!("toolConfig.{segment}");
    let learned_key = target
        .learned_key(&dotted)
        .expect("a learned lane")
        .to_string();
    let mut meta = DispatchMeta::for_alias(ALIAS);

    router.mint_learned_negative(
        LearnedMint {
            learned_key,
            feature_key: dotted.clone(),
            state_key: &target.state_key,
            provider_kind: "bedrock",
            tier: SignalTier::SelfIdentifying,
            phase: FailurePhase::F1,
            upstream_status: 400,
            upstream_code: None,
            upstream_param: None,
            remapped: false,
            request_features: vec![dotted],
        },
        &mut HashSet::new(),
        &mut meta,
    );

    assert!(meta.learned_capabilities.is_empty());
    assert!(router.learned_capability_snapshot().is_empty());
}

/// The public learn entry refuses a key Bedrock would store in the field
/// namespace: it reports the refusal, records nothing, and leaves no field
/// verdict acting. The ordinary key on the same lane is the control.
///
/// Mutation check: remove the namespace refusal at the top of
/// `observe_learned_capability` AND the one in the registry's
/// `observe_in_generation` -> red (applied, one resident field row).
#[test]
fn the_public_learn_entry_refuses_a_key_bedrock_stores_in_the_field_namespace() {
    let router = bedrock_router("unused");
    let lane_key = format!("p0#{WIRE}");
    let segment = crate::field_capability::field_capability_key(SEGMENT_PATH).expect("a path");
    let now = Instant::now();
    let observe = |feature_key: &str| {
        router.observe_learned_capability(
            &lane_key,
            feature_key,
            "bedrock",
            SignalTier::SelfIdentifying,
            FailurePhase::F1,
            routectl_core::capability::EvidenceSource::Live,
            None,
            now,
        )
    };

    let refused = observe(&format!("toolConfig.{segment}"));
    let control = observe("web_search");

    assert_eq!(
        refused,
        crate::learned_capability::GenerationOutcome::FieldNamespace
    );
    assert!(control.applied().is_some(), "the control key is recorded");
    let resident: Vec<String> = router
        .learned_capability_snapshot()
        .into_iter()
        .map(|entry| entry.feature_key)
        .collect();
    assert_eq!(resident, vec!["web_search".to_string()]);
    let lane = crate::state_key::StateKey::parse(&lane_key).expect("lane");
    let verdict = FieldVerdictKey::new(&lane, SEGMENT_PATH, "bedrock").expect("identity");
    assert!(!router.field_verdicts().is_negative_acting(&verdict, now));
}
