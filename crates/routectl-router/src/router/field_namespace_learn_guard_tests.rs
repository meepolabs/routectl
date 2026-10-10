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

/// Mutation checks: remove the namespace refusal in
/// `commit_learned_observation` AND the one in `tool_type_key` -> red (one
/// learn event, one resident row, one acting field verdict). Each guard alone
/// keeps this green, so each has its own unit pin below.
#[tokio::test]
async fn a_field_namespace_tool_type_echoed_by_bedrock_learns_nothing() {
    let key = field_key();
    let router = bedrock_router(&key);

    let dispatched = router
        .complete_with_options(request_with_tool_type(&key), RouterOptions::default())
        .await;

    assert!(dispatched.result.is_err(), "the rejection stands");
    assert!(
        dispatched.meta.learned_capabilities.is_empty(),
        "no learn event may carry a field-namespace key: {:?}",
        dispatched
            .meta
            .learned_capabilities
            .iter()
            .map(|event| &event.capability_key)
            .collect::<Vec<_>>(),
    );
    assert!(
        router.learned_capability_snapshot().is_empty(),
        "no learned row may be resident",
    );
    let lane = crate::state_key::StateKey::parse(&format!("p0#{WIRE}")).expect("lane");
    let verdict = FieldVerdictKey::new(&lane, FIELD_PATH, "bedrock").expect("identity");
    assert!(
        !router
            .field_verdicts()
            .is_negative_acting(&verdict, Instant::now()),
        "and no field verdict may become resident",
    );
}

/// The generic learn entry refuses a field-namespace key on its own, even
/// when the resolver hands it one the request's derived vocabulary carries.
///
/// Mutation check: remove the namespace refusal in
/// `commit_learned_observation` -> red (one learn event).
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
