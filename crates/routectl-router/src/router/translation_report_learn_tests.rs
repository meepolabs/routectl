//! A tool the egress withheld from the wire never teaches. The upstream
//! answered a request without that tool, so neither its success nor its
//! rejection says anything about whether the lane supports it: a forced
//! web search that succeeds on a lane that dropped the tool mints no
//! suspected absence, and a 400 after the withhold mints no negative. A
//! sibling lane in the same request that sent the tool still learns, and an
//! error raised before anything reached the wire never teaches at all.

use super::*;
use std::collections::BTreeMap;
use std::sync::Arc;

use futures::stream::BoxStream;
use routectl_core::{
    ChatChunk, ChatResponse, Message, MessageContent, Provider, Result, Role, ToolDef,
};
use serde_json::{Value, json};

use crate::config::Config;
use crate::resolved::ResolvedModel;
use crate::router::RouterOptions;

const ALIAS: &str = "search-alias";

fn forced_web_search_request() -> ChatRequest {
    ChatRequest {
        model: ALIAS.into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("search for it".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(16),
        tools: Some(vec![ToolDef::Other(json!({
            "type": "web_search_20250305",
            "name": "web_search",
            "max_uses": 1
        }))]),
        tool_choice: Some(json!({"type": "any"})),
        ..Default::default()
    }
}

#[cfg(feature = "bedrock")]
mod bedrock_lanes {
    use super::*;

    use crate::router::CapabilityLearnEvent;
    use routectl_core::capability::{FailurePhase, Verdict, WEB_SEARCH};
    use routectl_providers::bedrock::{
        BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth,
    };
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const REGION: &str = "us-east-1";

    /// Synthetic wire ids keep every baked catalog prior out of play.
    const WIRE_A: &str = "us.anthropic.zz-withhold-a";
    const WIRE_B: &str = "us.anthropic.zz-withhold-b";

    fn web_search_rows(router: &Router) -> Vec<crate::learned_capability::LearnedRegistryEntry> {
        router
            .learned_capability_snapshot()
            .into_iter()
            .filter(|row| row.feature_key == WEB_SEARCH)
            .collect()
    }

    fn web_search_learns(dispatched: &Dispatched) -> Vec<&CapabilityLearnEvent> {
        dispatched
            .meta
            .learned_capabilities
            .iter()
            .filter(|ev| ev.capability_key == WEB_SEARCH)
            .collect()
    }

    const ERROR_TYPE_HEADER: &str =
        "ValidationException:http://internal.amazon.com/coral/com.amazon.bedrock/";

    /// The captured Invoke rejection of a web-search tool, and the prefix
    /// Converse wraps it in.
    const WEB_SEARCH_REJECTION: &str =
        "tool type 'web_search_20250305' is not supported for this model";
    const CONVERSE_ERRORS_PREFIX: &str = "The model returned the following errors: ";

    const CONFIG: &str = r#"
[retry]
max_attempts = 1
initial_backoff_ms = 0
jitter_ms = 0

[providers.p1]
kind = "bedrock"
region = "us-east-1"
creds = { kind = "default-chain" }

[aliases]
"search-alias" = ["m1", "m2"]
"#;

    /// What the mock upstream answers every request with.
    #[derive(Clone, Copy)]
    enum Answer {
        Success,
        WebSearchRejection,
    }

    /// Assembled at runtime so no key-shaped literal sits in the source.
    fn bearer_key() -> String {
        ["withhold", "test", "key"].join("-")
    }

    fn success_body(shape: BedrockApiShape, wire: &str) -> Value {
        match shape {
            BedrockApiShape::Converse => json!({
                "output": { "message": { "role": "assistant", "content": [{ "text": "ok" }] } },
                "stopReason": "end_turn",
                "usage": { "inputTokens": 3, "outputTokens": 1, "totalTokens": 4 }
            }),
            _ => json!({
                "id": "msg_withhold",
                "type": "message",
                "role": "assistant",
                "model": wire,
                "content": [{ "type": "text", "text": "ok" }],
                "stop_reason": "end_turn",
                "usage": { "input_tokens": 3, "output_tokens": 1 }
            }),
        }
    }

    fn rejection_message(shape: BedrockApiShape) -> String {
        match shape {
            BedrockApiShape::Converse => format!("{CONVERSE_ERRORS_PREFIX}{WEB_SEARCH_REJECTION}"),
            _ => WEB_SEARCH_REJECTION.to_string(),
        }
    }

    fn response(shape: BedrockApiShape, wire: &str, answer: Answer) -> ResponseTemplate {
        match answer {
            Answer::Success => ResponseTemplate::new(200).set_body_json(success_body(shape, wire)),
            Answer::WebSearchRejection => ResponseTemplate::new(400)
                .insert_header("x-amzn-ErrorType", ERROR_TYPE_HEADER)
                .set_body_raw(
                    json!({ "message": rejection_message(shape) }).to_string(),
                    "application/json",
                ),
        }
    }

    /// One chain member: a real Bedrock provider of `shape` aimed at its own
    /// mock upstream.
    struct LaneSpec {
        nickname: &'static str,
        wire: &'static str,
        shape: BedrockApiShape,
        answer: Answer,
    }

    struct Lanes {
        router: Router,
        servers: Vec<MockServer>,
    }

    impl Lanes {
        async fn start(specs: &[LaneSpec]) -> Self {
            let config: Config = toml::from_str(CONFIG).expect("valid test toml");
            let mut router = Router::new(Arc::new(config));
            let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
            let mut servers = Vec::new();
            for spec in specs {
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .respond_with(response(spec.shape, spec.wire, spec.answer))
                    .mount(&server)
                    .await;
                let provider = bedrock_provider(spec, &server.uri()).await;
                models.insert(
                    spec.nickname.to_string(),
                    Arc::new(ResolvedModel::new(
                        spec.nickname,
                        "p1",
                        Arc::new(provider),
                        spec.wire,
                    )),
                );
                servers.push(server);
            }
            router.install_resolved_models(models);
            Self { router, servers }
        }

        async fn send(&self) -> Dispatched {
            self.router
                .complete_with_options(forced_web_search_request(), RouterOptions::default())
                .await
        }

        async fn wire_tool_types(&self, lane: usize) -> Vec<Vec<String>> {
            let requests = self.servers[lane]
                .received_requests()
                .await
                .expect("request recording is on");
            requests
                .iter()
                .map(|request| {
                    let body: Value =
                        serde_json::from_slice(&request.body).expect("request body is JSON");
                    wire_tool_types(&body)
                })
                .collect()
        }
    }

    /// Every tool type (Invoke) or tool spec name (Converse) on the wire.
    fn wire_tool_types(body: &Value) -> Vec<String> {
        let invoke = body["tools"].as_array().into_iter().flatten();
        let converse = body["toolConfig"]["tools"].as_array().into_iter().flatten();
        invoke
            .filter_map(|tool| tool["type"].as_str())
            .chain(converse.filter_map(|tool| tool["toolSpec"]["name"].as_str()))
            .map(str::to_string)
            .collect()
    }

    async fn bedrock_provider(spec: &LaneSpec, origin: &str) -> BedrockProvider {
        let creds = BedrockCreds::BearerKey { key: bearer_key() };
        let cfg = BedrockConfig {
            id: "bedrock:p1".into(),
            region: REGION.into(),
            model_id: spec.wire.into(),
            api_shape: spec.shape,
            creds: creds.clone(),
            user_agent: None,
            header_extras: Vec::new(),
            anthropic_beta: Vec::new(),
            additional_model_request_fields: None,
            adaptive_thinking: None,
        };
        let resolved = auth::resolve(&creds, REGION).await.expect("resolve");
        BedrockProvider::new(cfg, resolved)
            .expect("canonical region")
            .with_runtime_origin_for_tests(origin)
    }

    const fn lane(
        nickname: &'static str,
        wire: &'static str,
        shape: BedrockApiShape,
        answer: Answer,
    ) -> LaneSpec {
        LaneSpec {
            nickname,
            wire,
            shape,
            answer,
        }
    }

    fn lane_key(wire: &str) -> String {
        format!("p1#{wire}")
    }

    #[tokio::test]
    async fn forced_web_search_the_converse_lane_withheld_mints_no_suspected_absence() {
        // Arrange
        let lanes = Lanes::start(&[lane(
            "m1",
            WIRE_A,
            BedrockApiShape::Converse,
            Answer::Success,
        )])
        .await;

        // Act: twice, so an inferred suspicion would be corroborated.
        let first = lanes.send().await;
        let second = lanes.send().await;

        // Assert
        assert!(first.result.is_ok(), "{:?}", first.result.err());
        assert!(second.result.is_ok(), "{:?}", second.result.err());
        assert_eq!(
            lanes.wire_tool_types(0).await,
            vec![Vec::<String>::new(), Vec::new()],
            "the Converse egress kept web search off the wire",
        );
        for dispatched in [&first, &second] {
            assert!(
                dispatched
                    .meta
                    .capability_observations
                    .iter()
                    .all(|ev| ev.capability_key != WEB_SEARCH),
                "no web-search observation rides out: {:?}",
                dispatched.meta.capability_observations,
            );
        }
        assert!(
            web_search_rows(&lanes.router).is_empty(),
            "no web-search entry is resident: {:?}",
            web_search_rows(&lanes.router),
        );
    }

    #[tokio::test]
    async fn forced_web_search_the_invoke_lane_sent_still_mints_a_suspected_absence() {
        // Arrange
        let lanes =
            Lanes::start(&[lane("m1", WIRE_A, BedrockApiShape::Invoke, Answer::Success)]).await;

        // Act
        lanes.send().await;
        let second = lanes.send().await;

        // Assert
        assert_eq!(
            lanes.wire_tool_types(0).await,
            vec![
                vec!["web_search_20250305".to_string()],
                vec!["web_search_20250305".to_string()]
            ],
            "the Invoke egress sent web search",
        );
        let observed: Vec<_> = second
            .meta
            .capability_observations
            .iter()
            .filter(|ev| ev.capability_key == WEB_SEARCH)
            .collect();
        assert_eq!(observed.len(), 1, "the corroborated suspicion acts");
        assert_eq!(observed[0].state_key, lane_key(WIRE_A));
        let rows = web_search_rows(&lanes.router);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verdict, Verdict::LearnedBroken(FailurePhase::F3));
    }

    #[tokio::test]
    async fn a_rejection_after_the_converse_lane_withheld_web_search_mints_no_negative() {
        // Arrange
        let lanes = Lanes::start(&[lane(
            "m1",
            WIRE_A,
            BedrockApiShape::Converse,
            Answer::WebSearchRejection,
        )])
        .await;

        // Act
        let dispatched = lanes.send().await;

        // Assert
        assert!(dispatched.result.is_err());
        assert_eq!(lanes.wire_tool_types(0).await, vec![Vec::<String>::new()]);
        assert!(
            web_search_learns(&dispatched).is_empty(),
            "no web-search learn event: {:?}",
            dispatched.meta.learned_capabilities,
        );
        assert!(web_search_rows(&lanes.router).is_empty());
    }

    #[tokio::test]
    async fn a_sibling_lane_that_sent_web_search_still_learns_after_a_withholding_lane() {
        // Arrange: the Converse lane withholds the tool and is rejected;
        // the Invoke lane sends it and is rejected naming it.
        let lanes = Lanes::start(&[
            lane(
                "m1",
                WIRE_A,
                BedrockApiShape::Converse,
                Answer::WebSearchRejection,
            ),
            lane(
                "m2",
                WIRE_B,
                BedrockApiShape::Invoke,
                Answer::WebSearchRejection,
            ),
        ])
        .await;

        // Act
        let dispatched = lanes.send().await;

        // Assert
        assert_eq!(lanes.wire_tool_types(0).await, vec![Vec::<String>::new()]);
        assert_eq!(
            lanes.wire_tool_types(1).await,
            vec![vec!["web_search_20250305".to_string()]],
        );
        let learned = web_search_learns(&dispatched);
        assert_eq!(learned.len(), 1, "{learned:?}");
        assert_eq!(learned[0].state_key, lane_key(WIRE_B));
        let rows = web_search_rows(&lanes.router);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].state_key, lane_key(WIRE_B));
    }
}

/// A provider that fails every call with a fixed local error, before any
/// byte reaches an upstream.
struct LocalErrorProvider {
    error: fn() -> Error,
}

#[async_trait::async_trait]
impl Provider for LocalErrorProvider {
    fn id(&self) -> &'static str {
        "p1"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<Value> {
        Err((self.error)())
    }
    fn normalize_response(&self, _: Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("p1", "unused"))
    }
    async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
        Err((self.error)())
    }
    async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        Err((self.error)())
    }
}

const OPENAI_P1: &str = r#"
[retry]
max_attempts = 1

[providers.p1]
kind = "openai-compat"
base_url = "https://example.test/v1"
api_key_ref = "literal:k"

[aliases]
"search-alias" = ["m1"]
"#;

fn naming_normalize_error() -> Error {
    Error::normalize_request(
        "p1",
        "tool type 'web_search_20250305' is not supported for this model",
    )
}

fn naming_local_refusal() -> Error {
    Error::LocalRefusal {
        provider: "p1".into(),
        detail: "tool type 'web_search_20250305' is not supported for this model".into(),
    }
}

fn local_error_router(error: fn() -> Error) -> Router {
    let config: Config = toml::from_str(OPENAI_P1).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
    models.insert(
        "m1".to_string(),
        Arc::new(ResolvedModel::new(
            "m1",
            "p1",
            Arc::new(LocalErrorProvider { error }),
            "zz-local-wire",
        )),
    );
    router.install_resolved_models(models);
    router
}

#[tokio::test]
async fn a_local_error_naming_the_tool_never_teaches() {
    for (label, error) in [
        ("normalize_request", naming_normalize_error as fn() -> Error),
        ("local_refusal", naming_local_refusal),
    ] {
        // Arrange
        let router = local_error_router(error);

        // Act
        let complete = router
            .complete_with_options(forced_web_search_request(), RouterOptions::default())
            .await;
        let stream = router
            .stream_with_options(forced_web_search_request(), RouterOptions::default())
            .await;

        // Assert
        assert!(complete.result.is_err(), "{label}: complete fails");
        assert!(stream.result.is_err(), "{label}: stream fails");
        assert!(
            complete.meta.learned_capabilities.is_empty(),
            "{label}: complete learned {:?}",
            complete.meta.learned_capabilities,
        );
        assert!(
            stream.meta.learned_capabilities.is_empty(),
            "{label}: stream learned {:?}",
            stream.meta.learned_capabilities,
        );
        assert!(
            router.learned_capability_snapshot().is_empty(),
            "{label}: nothing is resident",
        );
    }
}
