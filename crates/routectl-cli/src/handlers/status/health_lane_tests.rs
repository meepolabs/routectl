//! The health panel's join key, end to end: a negative learned through real
//! dispatch serializes under the same lane every target on that lane carries
//! as `learned_lane`, so a client joining on it finds the row from each
//! nickname's card.

use super::*;

use std::collections::BTreeMap;

use arc_swap::ArcSwap;
use futures::stream::BoxStream;
use routectl_core::{
    ChatChunk, ChatRequest, ChatResponse, Error, Message, MessageContent, Provider, Result, Role,
    ToolDef,
};
use routectl_router::{Config, ResolvedModel, Router, RouterOptions};
use serde_json::{Value, json};

use crate::handlers::status::DaemonMeta;
use crate::server::AppState;

const PROVIDER: &str = "p";
const UPSTREAM: &str = "shared-upstream";
const LANE: &str = "p#shared-upstream";
const NICKNAMES: [&str; 2] = ["alpha", "beta"];

/// Rejects every request with an openai-compat 400 naming `web_search`, the
/// shape the classifier lifts to a self-identifying feature rejection.
struct RejectsWebSearch;

#[async_trait::async_trait]
impl Provider for RejectsWebSearch {
    fn id(&self) -> &'static str {
        "p"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        let body = json!({
            "error": {
                "type": "invalid_request_error",
                "code": "unsupported_parameter",
                "param": "web_search",
                "message": "Unsupported parameter."
            }
        });
        Err(Error::upstream_full(
            "p",
            400,
            body.to_string(),
            None,
            Some("invalid_request_error".to_string()),
            Some("unsupported_parameter".to_string()),
        ))
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(Error::upstream("p", 500, "unused"))
    }
}

/// Two nicknames on one upstream of one provider entry: one lane, two
/// runtime targets.
fn router() -> Router {
    let config: Config = toml::from_str(&format!(
        "version = 3\n\
         [providers.{PROVIDER}]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://example.invalid/v1\"\n\
         api_key_ref = \"env://ROUTECTL_HEALTH_LANE_TEST_KEY\"\n\
         [capability]\n\
         enabled = true\n"
    ))
    .expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let provider: Arc<dyn Provider> = Arc::new(RejectsWebSearch);
    let models: BTreeMap<String, Arc<ResolvedModel>> = NICKNAMES
        .iter()
        .map(|nickname| {
            (
                (*nickname).to_string(),
                Arc::new(ResolvedModel::new(
                    *nickname,
                    PROVIDER,
                    Arc::clone(&provider),
                    UPSTREAM,
                )),
            )
        })
        .collect();
    router.install_resolved_models(models);
    router
}

fn web_search_request(model: &str) -> ChatRequest {
    ChatRequest {
        model: model.to_string(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        tools: Some(vec![ToolDef::Other(
            json!({ "type": "web_search", "name": "t" }),
        )]),
        ..Default::default()
    }
}

fn globals() -> AccountingGlobals {
    AccountingGlobals {
        writer_degraded: false,
        consumed_unauthorized_total: 0,
    }
}

#[tokio::test]
async fn each_target_learned_lane_is_the_state_key_of_the_row_learned_on_it() {
    // Arrange: learn a negative through real dispatch on one nickname.
    let router = router();
    let dispatched = router
        .complete_with_options(web_search_request("alpha"), RouterOptions::default())
        .await;
    assert!(dispatched.result.is_err(), "the only target rejects");
    let app = AppState::for_test(Arc::new(ArcSwap::from_pointee(router)));
    let state = StatusState::from_app(&app, None, DaemonMeta::for_test());

    // Act
    let panel = build_from_view(
        &state.router.view(),
        &[],
        globals(),
        FidelityEmission::always(),
    );
    let wire = serde_json::to_value(&panel).expect("panel serializes");

    // Assert
    let rows = wire["learned_negatives"]
        .as_array()
        .expect("learned rows array");
    assert_eq!(rows.len(), 1, "one negative learned: {rows:?}");
    assert_eq!(rows[0]["verdict"], "broken");
    assert_eq!(rows[0]["state_key"], LANE);
    let targets = wire["targets"].as_array().expect("targets array");
    let mut nicknames: Vec<&str> = targets
        .iter()
        .map(|t| t["nickname"].as_str().expect("nickname"))
        .collect();
    nicknames.sort_unstable();
    assert_eq!(nicknames, NICKNAMES);
    for target in targets {
        assert_eq!(
            target["learned_lane"], rows[0]["state_key"],
            "target {} joins its lane's row",
            target["nickname"]
        );
        assert_ne!(
            target["state_key"], rows[0]["state_key"],
            "the runtime key never matches a lane row"
        );
    }
}
