//! The learn path's forwarded-credential gate is per TARGET: in a chain that
//! mixes a forwarded-credential target with an own-credential one, a request
//! carrying a captured client bearer still teaches the own-credential lane,
//! while the forwarded lane learns nothing and bumps no parser counter.
//! Driven through the real complete and stream error arms.

use super::*;
use std::collections::BTreeMap;
use std::sync::Arc;

use futures::stream::BoxStream;
use routectl_core::capability::SignalTier;
use routectl_core::schema::ForwardedBearer;
use routectl_core::{ChatChunk, ChatResponse, Provider, Result, ToolDef};
use serde_json::{Value, json};

use crate::config::Config;
use crate::resolved::ResolvedModel;
use crate::router::RouterOptions;

const ALIAS: &str = "mixed-alias";
const LANE_A: &str = "pa#wire-a";
const LANE_B: &str = "pb#wire-b";

const FORCED_TOOL_CHOICE_CAPTURE: &str =
    include_str!("../../tests/fixtures/forced_tool_choice_capture.json");

/// A provider whose complete and stream both fail with one fixed 400.
struct Rejecting {
    id: &'static str,
    body: String,
    upstream_code: Option<String>,
}

impl Rejecting {
    fn err(&self) -> Error {
        Error::upstream_full(
            self.id,
            400,
            self.body.clone(),
            None,
            Some("invalid_request_error".into()),
            self.upstream_code.clone(),
        )
    }
}

#[async_trait::async_trait]
impl Provider for Rejecting {
    fn id(&self) -> &str {
        self.id
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(self.id, "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err(self.err())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err(self.err())
    }
}

/// Target A's answer: the captured anthropic forced-`tool_choice` 400. On an
/// own-credential lane it resolves to a learnable capability AND is a
/// field-eligible rejection no path is localized from, so it exercises both
/// the mint and the parser-unlocalized count.
fn anthropic_forced_tool_choice_rejection() -> Arc<Rejecting> {
    let capture: Value =
        serde_json::from_str(FORCED_TOOL_CHOICE_CAPTURE).expect("valid capture fixture");
    let body = capture["anthropic_api"]["body"]
        .as_str()
        .expect("anthropic_api.body")
        .to_string();
    Arc::new(Rejecting {
        id: "pa",
        body,
        upstream_code: None,
    })
}

/// Target B's answer: a self-identifying openai-compat 400 naming the
/// request's `web_search` tool.
fn openai_web_search_rejection() -> Arc<Rejecting> {
    Arc::new(Rejecting {
        id: "pb",
        body: r#"{"error":{"type":"invalid_request_error","code":"unsupported_parameter","param":"web_search","message":"Unsupported parameter."}}"#.into(),
        upstream_code: Some("unsupported_parameter".into()),
    })
}

/// Chain `[a, b]`: `a` on anthropic-api provider `pa` (forwarded or own
/// credential per `a_forwarded`), `b` on own-credential openai-compat `pb`.
fn mixed_chain_router(a_forwarded: bool) -> Router {
    let credential_line = if a_forwarded {
        "credential_source = \"forwarded\"\n"
    } else {
        ""
    };
    let toml_text = format!(
        "[providers.pa]\n\
         kind = \"anthropic-api\"\n\
         base_url = \"https://api.anthropic.com\"\n\
         {credential_line}\
         [providers.pb]\n\
         kind = \"openai-compat\"\n\
         base_url = \"https://example.test/v1\"\n\
         api_key_ref = \"literal:k\"\n\
         [aliases]\n\
         \"{ALIAS}\" = [\"a\", \"b\"]\n"
    );
    let config: Config = toml::from_str(&toml_text).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "a".to_string(),
        Arc::new(ResolvedModel::new(
            "a",
            "pa",
            anthropic_forced_tool_choice_rejection(),
            "wire-a",
        )),
    );
    models.insert(
        "b".to_string(),
        Arc::new(ResolvedModel::new(
            "b",
            "pb",
            openai_web_search_rejection(),
            "wire-b",
        )),
    );
    router.install_resolved_models(models);
    router
}

/// A request carrying a captured client bearer, the `web_search` tool B
/// names, and the forced `tool_choice` A's rejection names.
fn forwarded_req() -> ChatRequest {
    let mut req = ChatRequest {
        model: ALIAS.into(),
        messages: vec![].into(),
        tools: Some(vec![
            ToolDef::Other(json!({ "type": "web_search" })),
            ToolDef::Other(json!({
                "type": "function",
                "function": {"name": "lookup", "parameters": {"type": "object"}}
            })),
        ]),
        tool_choice: Some(json!({"type": "any"})),
        ..Default::default()
    };
    req.routectl_internal.forwarded_bearer = Some(ForwardedBearer::new("t".into()));
    req
}

#[derive(Clone, Copy, Debug)]
enum Surface {
    Complete,
    Stream,
}

const SURFACES: [Surface; 2] = [Surface::Complete, Surface::Stream];

/// Dispatch [`forwarded_req`] on `surface`, returning the learn events that
/// rode out on the dispatch metadata.
async fn dispatch(router: &Router, surface: Surface) -> Vec<CapabilityLearnEvent> {
    match surface {
        Surface::Complete => {
            let out = router
                .complete_with_options(forwarded_req(), RouterOptions::default())
                .await;
            assert!(out.result.is_err(), "premise: every target rejects");
            out.meta.learned_capabilities
        }
        Surface::Stream => {
            let out =
                Box::pin(router.stream_with_options(forwarded_req(), RouterOptions::default()))
                    .await;
            assert!(out.result.is_err(), "premise: every target rejects");
            out.meta.learned_capabilities
        }
    }
}

fn resident_lanes(router: &Router) -> Vec<String> {
    router
        .learned_capabilities
        .snapshot()
        .into_iter()
        .map(|entry| entry.state_key)
        .collect()
}

#[tokio::test]
async fn own_credential_target_learns_after_a_forwarded_sibling_on_a_forwarded_request() {
    for surface in SURFACES {
        let router = mixed_chain_router(true);

        let learned = dispatch(&router, surface).await;

        assert_eq!(learned.len(), 1, "{surface:?}: B's negative rides out");
        assert_eq!(learned[0].state_key, LANE_B, "{surface:?}");
        assert_eq!(learned[0].capability_key, "web_search", "{surface:?}");
        assert_eq!(
            learned[0].signal_tier,
            SignalTier::SelfIdentifying,
            "{surface:?}"
        );
        assert_eq!(
            router.learned_capabilities.acting_negative_for(
                LANE_B,
                "web_search",
                "openai-compat",
                Instant::now(),
            ),
            crate::learned_capability::RoutingDecision::RouteAway {
                signal: SignalTier::SelfIdentifying,
                phase: routectl_core::capability::FailurePhase::F1,
            },
            "{surface:?}: B's lane holds an acting negative",
        );
    }
}

#[tokio::test]
async fn forwarded_target_rejection_mints_nothing_and_counts_no_parser_blindness() {
    for surface in SURFACES {
        let router = mixed_chain_router(true);

        dispatch(&router, surface).await;

        assert_eq!(
            resident_lanes(&router),
            vec![LANE_B.to_string()],
            "{surface:?}: only the own-credential lane holds an entry",
        );
        assert_eq!(
            router.field_repair_counters().parser_unlocalized,
            0,
            "{surface:?}: a forwarded target's rejection is not parser evidence",
        );
    }
}

/// Positive control for the forwarded case: the SAME rejection on an
/// own-credential A records an observation on A's lane and counts once as
/// parser-unlocalized, so the forwarded assertions above cannot pass on a
/// fixture that never reaches the learn path.
#[tokio::test]
async fn own_credential_first_target_rejection_observes_and_counts_parser_blindness() {
    for surface in SURFACES {
        let router = mixed_chain_router(false);

        dispatch(&router, surface).await;

        let mut lanes = resident_lanes(&router);
        lanes.sort();
        assert_eq!(
            lanes,
            vec![LANE_A.to_string(), LANE_B.to_string()],
            "{surface:?}: both own-credential lanes record their rejection",
        );
        assert_eq!(
            router.field_repair_counters().parser_unlocalized,
            1,
            "{surface:?}: A's unlocalized rejection counts once",
        );
    }
}
