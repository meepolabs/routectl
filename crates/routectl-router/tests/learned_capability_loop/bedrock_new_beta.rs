//! A beta flag routectl has never heard of crosses a real Bedrock lane with
//! no routectl change: no allowlist entry, no seed entry, no `[bedrock]`
//! table. A real `Router` drives a real `BedrockProvider` aimed at a mock
//! bedrock-runtime that rejects any body carrying the flag with the captured
//! naming `ValidationException` and accepts everything else. The client's
//! flag is sent verbatim, the rejection is repaired and learned once, and the
//! next request withholds it before the wire.

use std::sync::{Mutex, PoisonError};

use routectl_providers::bedrock::{
    BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth,
};
use routectl_router::{
    CURRENT_CONFIG_VERSION, ResolvedModel, beta_capability_key, parse_config,
    preflight_config_version, preflight_retired_capability_keys,
};

use super::*;

/// A flag no Anthropic release issued and the shipped seed does not name.
const NEW_BETA: &str = "zz-probe-2099-01-01";

/// A flag the mock upstream accepts.
const ACCEPTED_BETA: &str = "context-management-2025-06-27";

/// The Invoke naming rejection for [`NEW_BETA`], as captured from AWS.
const INVOKE_NAMING_REJECTION: &str = "Unexpected value(s) `zz-probe-2099-01-01` for the `anthropic-beta` header. Please consult our documentation at platform.claude.com/docs or try again without the header.";

/// Converse wraps the Invoke message in this captured prefix.
const CONVERSE_ERRORS_PREFIX: &str = "The model returned the following errors: ";

const ERROR_TYPE_HEADER: &str =
    "ValidationException:http://internal.amazon.com/coral/com.amazon.bedrock/";

const PROVIDER: &str = "bed";
const NICKNAME: &str = "m1";
const MODEL_ID: &str = "us.anthropic.claude-opus-5-5";
const REGION: &str = "us-east-1";

const SHAPES: [BedrockApiShape; 2] = [BedrockApiShape::Invoke, BedrockApiShape::Converse];

/// One request the mock received: its `anthropic_beta` array (`None` when
/// the body carries no such field) and the status it was answered with.
#[derive(Debug, Clone, PartialEq, Eq)]
struct WireCall {
    betas: Option<Vec<String>>,
    status: u16,
}

/// A mock bedrock-runtime: 400 with the naming envelope when the body
/// carries [`NEW_BETA`], the shape's success body otherwise.
struct AwsLikeUpstream {
    shape: BedrockApiShape,
    calls: Arc<Mutex<Vec<WireCall>>>,
}

impl Respond for AwsLikeUpstream {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let body: Value = serde_json::from_slice(&request.body).expect("request body is JSON");
        let betas = wire_betas(self.shape, &body);
        let rejected = betas
            .as_ref()
            .is_some_and(|flags| flags.iter().any(|f| f == NEW_BETA));
        let status = if rejected { 400 } else { 200 };
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(WireCall { betas, status });
        if rejected {
            ResponseTemplate::new(400)
                .insert_header("x-amzn-ErrorType", ERROR_TYPE_HEADER)
                .set_body_raw(
                    json!({ "message": naming_rejection(self.shape) }).to_string(),
                    "application/json",
                )
        } else {
            ResponseTemplate::new(200).set_body_json(success_body(self.shape))
        }
    }
}

fn naming_rejection(shape: BedrockApiShape) -> String {
    match shape {
        BedrockApiShape::Converse => format!("{CONVERSE_ERRORS_PREFIX}{INVOKE_NAMING_REJECTION}"),
        _ => INVOKE_NAMING_REJECTION.to_string(),
    }
}

/// The `anthropic_beta` array where this carrier puts it.
fn wire_betas(shape: BedrockApiShape, body: &Value) -> Option<Vec<String>> {
    let bag = match shape {
        BedrockApiShape::Converse => &body["additionalModelRequestFields"]["anthropic_beta"],
        _ => &body["anthropic_beta"],
    };
    bag.as_array().map(|arr| {
        arr.iter()
            .map(|v| v.as_str().expect("beta entries are strings").to_string())
            .collect()
    })
}

fn success_body(shape: BedrockApiShape) -> Value {
    match shape {
        BedrockApiShape::Converse => json!({
            "output": { "message": { "role": "assistant", "content": [{ "text": "ok" }] } },
            "stopReason": "end_turn",
            "usage": { "inputTokens": 3, "outputTokens": 1, "totalTokens": 4 }
        }),
        _ => json!({
            "id": "msg_new_beta",
            "type": "message",
            "role": "assistant",
            "model": MODEL_ID,
            "content": [{ "type": "text", "text": "ok" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 3, "output_tokens": 1 }
        }),
    }
}

const fn shape_toml(shape: BedrockApiShape) -> &'static str {
    match shape {
        BedrockApiShape::Converse => "converse",
        _ => "invoke",
    }
}

/// What the operator wrote beyond the bare lane.
#[derive(Default)]
struct Operator<'a> {
    floor: &'a [&'a str],
    unsupported: &'a [&'a str],
}

/// A current-version config with one native Bedrock lane and no `[bedrock]`
/// table; the preflights a real load runs must admit it.
fn lane_config(shape: BedrockApiShape, operator: &Operator<'_>) -> Config {
    let key_ref = common::file_ref(&bearer_key());
    let quoted = |flags: &[&str]| {
        flags
            .iter()
            .map(|f| format!("\"{f}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\n\
         [providers.{PROVIDER}]\n\
         kind = \"bedrock\"\n\
         region = \"{REGION}\"\n\
         api_shape = \"{shape}\"\n\
         creds = {{ kind = \"bearer-key\", key_ref = \"{key_ref}\" }}\n\
         anthropic_beta = [{floor}]\n\n\
         [models.{NICKNAME}]\n\
         provider = \"{PROVIDER}\"\n\
         upstream = \"{MODEL_ID}\"\n",
        shape = shape_toml(shape),
        floor = quoted(operator.floor),
    );
    if !operator.unsupported.is_empty() {
        let keys: Vec<String> = operator
            .unsupported
            .iter()
            .map(|f| beta_capability_key(f).expect("well-formed flag"))
            .collect();
        let keys: Vec<&str> = keys.iter().map(String::as_str).collect();
        text.push_str(&format!(
            "\n[capability.overrides.{PROVIDER}]\nunsupported = [{}]\n",
            quoted(&keys)
        ));
    }
    assert_eq!(
        preflight_config_version(&text).expect("version preflight"),
        CURRENT_CONFIG_VERSION
    );
    preflight_retired_capability_keys(&text).expect("no retired key in the test config");
    parse_config(&text).expect("valid test config")
}

/// Assembled at runtime so no key-shaped literal sits in the source.
fn bearer_key() -> String {
    ["new", "beta", "test", "key"].join("-")
}

/// A real router over a real Bedrock provider aimed at the mock upstream.
struct Lane {
    router: Router,
    calls: Arc<Mutex<Vec<WireCall>>>,
    _server: MockServer,
}

impl Lane {
    async fn start(shape: BedrockApiShape, operator: &Operator<'_>) -> Self {
        let server = MockServer::start().await;
        let calls = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .respond_with(AwsLikeUpstream {
                shape,
                calls: Arc::clone(&calls),
            })
            .mount(&server)
            .await;

        let creds = BedrockCreds::BearerKey { key: bearer_key() };
        let cfg = BedrockConfig {
            id: format!("bedrock:{PROVIDER}"),
            region: REGION.into(),
            model_id: MODEL_ID.into(),
            api_shape: shape,
            creds: creds.clone(),
            user_agent: None,
            header_extras: Vec::new(),
            anthropic_beta: operator.floor.iter().map(|f| (*f).to_string()).collect(),
            additional_model_request_fields: None,
            adaptive_thinking: None,
        };
        let resolved = auth::resolve(&creds, REGION).await.expect("resolve");
        let provider = BedrockProvider::new(cfg, resolved)
            .expect("canonical region")
            .with_runtime_origin_for_tests(&server.uri());

        let mut router = Router::new(Arc::new(lane_config(shape, operator)));
        let mut models = BTreeMap::new();
        models.insert(
            NICKNAME.to_string(),
            Arc::new(ResolvedModel::new(
                NICKNAME,
                PROVIDER,
                Arc::new(provider),
                MODEL_ID,
            )),
        );
        router.install_resolved_models(models);
        Self {
            router,
            calls,
            _server: server,
        }
    }

    async fn send(&self, client_betas: &[&str]) -> routectl_router::Dispatched {
        let req = ChatRequest {
            model: NICKNAME.into(),
            messages: vec![Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Text("hello".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            }]
            .into(),
            max_tokens: Some(16),
            anthropic_beta: client_betas.iter().map(|b| (*b).to_string()).collect(),
            ..Default::default()
        };
        self.router
            .complete_with_options(req, RouterOptions::default())
            .await
    }

    fn calls(&self) -> Vec<WireCall> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

fn betas(flags: &[&str]) -> Option<Vec<String>> {
    Some(flags.iter().map(|f| (*f).to_string()).collect())
}

fn new_beta_key() -> String {
    beta_capability_key(NEW_BETA).expect("well-formed flag")
}

#[tokio::test]
async fn first_contact_sends_a_new_beta_once_verbatim() {
    for shape in SHAPES {
        // Arrange
        let lane = Lane::start(shape, &Operator::default()).await;

        // Act
        lane.send(&[ACCEPTED_BETA, NEW_BETA]).await;

        // Assert
        let first = lane.calls().first().cloned().expect("one wire call");
        assert_eq!(
            first.betas,
            betas(&[ACCEPTED_BETA, NEW_BETA]),
            "{shape:?}: the new flag reaches the wire once, verbatim, in client order",
        );
    }
}

#[tokio::test]
async fn named_rejection_retries_without_the_flag_and_learns_one_beta_negative() {
    for shape in SHAPES {
        // Arrange
        let lane = Lane::start(shape, &Operator::default()).await;

        // Act
        let dispatched = lane.send(&[ACCEPTED_BETA, NEW_BETA]).await;

        // Assert
        assert!(
            dispatched.result.is_ok(),
            "{shape:?}: the client sees the repaired success: {:?}",
            dispatched.result.err()
        );
        assert_eq!(
            lane.calls(),
            vec![
                WireCall {
                    betas: betas(&[ACCEPTED_BETA, NEW_BETA]),
                    status: 400
                },
                WireCall {
                    betas: betas(&[ACCEPTED_BETA]),
                    status: 200
                },
            ],
            "{shape:?}: the wire sees the flag, then the same request without it",
        );
        let learned = &dispatched.meta.learned_capabilities;
        assert_eq!(learned.len(), 1, "{shape:?}: exactly one learn event");
        assert_eq!(learned[0].capability_key, new_beta_key(), "{shape:?}");
        assert_eq!(
            learned[0].signal_tier,
            SignalTier::SelfIdentifying,
            "{shape:?}"
        );
        assert_eq!(learned[0].upstream_status, 400, "{shape:?}");
    }
}

#[tokio::test]
async fn request_after_learning_omits_the_flag_and_sees_no_rejection() {
    for shape in SHAPES {
        // Arrange
        let lane = Lane::start(shape, &Operator::default()).await;
        lane.send(&[ACCEPTED_BETA, NEW_BETA]).await;

        // Act
        let dispatched = lane.send(&[ACCEPTED_BETA, NEW_BETA]).await;

        // Assert
        assert!(dispatched.result.is_ok(), "{shape:?}");
        assert_eq!(
            lane.calls()[2..],
            [WireCall {
                betas: betas(&[ACCEPTED_BETA]),
                status: 200
            }],
            "{shape:?}: one wire call, already without the learned flag",
        );
        assert!(
            dispatched.meta.learned_capabilities.is_empty(),
            "{shape:?}: nothing new to learn",
        );
    }
}

#[tokio::test]
async fn operator_unsupported_override_omits_the_flag_from_the_first_body() {
    for shape in SHAPES {
        // Arrange
        let operator = Operator {
            unsupported: &[NEW_BETA],
            ..Operator::default()
        };
        let lane = Lane::start(shape, &operator).await;

        // Act
        let dispatched = lane.send(&[ACCEPTED_BETA, NEW_BETA]).await;

        // Assert
        assert!(dispatched.result.is_ok(), "{shape:?}");
        assert_eq!(
            lane.calls(),
            vec![WireCall {
                betas: betas(&[ACCEPTED_BETA]),
                status: 200
            }],
            "{shape:?}: the overridden flag never reaches the wire",
        );
    }
}

#[tokio::test]
async fn flag_in_floor_and_client_ships_once_on_invoke() {
    // Arrange
    let operator = Operator {
        floor: &[ACCEPTED_BETA],
        ..Operator::default()
    };
    let lane = Lane::start(BedrockApiShape::Invoke, &operator).await;

    // Act
    let dispatched = lane.send(&[ACCEPTED_BETA]).await;

    // Assert
    assert!(dispatched.result.is_ok());
    assert_eq!(
        lane.calls(),
        vec![WireCall {
            betas: betas(&[ACCEPTED_BETA]),
            status: 200
        }],
    );
}
