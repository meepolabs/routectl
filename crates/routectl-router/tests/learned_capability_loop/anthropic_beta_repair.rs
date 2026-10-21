//! The anthropic-api lane repairs a client `anthropic-beta` flag the upstream
//! rejects by name, with no provider-side repair of its own. A real `Router`
//! drives a real `AnthropicApiProvider` at a mock that answers any request
//! whose `anthropic-beta` header carries the flag with the committed Mantle
//! capture, and serves every other request. The provider keeps the exact
//! Anthropic host as its base URL (with the mock's port) and resolves it to
//! the mock, as in production. The rejection is repaired by the router's
//! learning engine, learned once, and the next request withholds the flag
//! before the wire.

use std::sync::{Mutex, PoisonError};

use routectl_providers::anthropic_api::{
    AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig,
};
use routectl_router::{
    CURRENT_CONFIG_VERSION, ResolvedModel, beta_capability_key, parse_config,
    preflight_config_version,
};

use super::*;

/// The committed Mantle beta-rejection capture.
const MANTLE_FIXTURE: &str =
    include_str!("../fixtures/rejections/anthropic-api/beta-prompt-caching-scope-mantle.json");

/// The flag the capture names.
const REJECTED: &str = "prompt-caching-scope-2026-01-05";
/// A client flag the mock accepts.
const KEPT: &str = "kept-2099-01-01";

const PROVIDER: &str = "anth";
const NICKNAME: &str = "m1";
const MODEL_ID: &str = "claude-opus-5-5";

/// The capture's status and body, checked to name [`REJECTED`].
fn capture() -> (u16, String) {
    let fixture: Value = serde_json::from_str(MANTLE_FIXTURE).expect("fixture is JSON");
    let status = u16::try_from(fixture["status"].as_u64().expect("status")).expect("u16 status");
    let body = fixture["body"].as_str().expect("body").to_string();
    assert!(
        body.contains(&format!("`{REJECTED}`")),
        "premise: the capture names {REJECTED}"
    );
    (status, body)
}

/// Records the `anthropic-beta` header of every request; answers the capture
/// while the header carries [`REJECTED`] and a Messages success otherwise.
struct MantleLikeUpstream {
    seen: Arc<Mutex<Vec<String>>>,
    rejection: (u16, String),
}

impl Respond for MantleLikeUpstream {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let beta = request
            .headers
            .get("anthropic-beta")
            .map(|v| v.to_str().expect("ascii header").to_string())
            .unwrap_or_default();
        let rejected = beta.split(',').any(|flag| flag.trim() == REJECTED);
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(beta);
        if rejected {
            let (status, body) = &self.rejection;
            return ResponseTemplate::new(*status).set_body_raw(body.clone(), "application/json");
        }
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_beta_repair",
            "type": "message",
            "role": "assistant",
            "model": MODEL_ID,
            "content": [{ "type": "text", "text": "ok" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 3, "output_tokens": 1 }
        }))
    }
}

/// A current-version config with one API-key anthropic-api lane.
fn lane_config() -> Config {
    let text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\n\
         [providers.{PROVIDER}]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"{key_ref}\"\n\n\
         [models.{NICKNAME}]\n\
         provider = \"{PROVIDER}\"\n\
         upstream = \"{MODEL_ID}\"\n",
        key_ref = common::file_ref("beta-repair-test-key"),
    );
    assert_eq!(
        preflight_config_version(&text).expect("version preflight"),
        CURRENT_CONFIG_VERSION
    );
    parse_config(&text).expect("valid test config")
}

struct Lane {
    router: Router,
    seen: Arc<Mutex<Vec<String>>>,
    _server: MockServer,
}

impl Lane {
    async fn start() -> Self {
        let server = MockServer::start().await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(MantleLikeUpstream {
                seen: Arc::clone(&seen),
                rejection: capture(),
            })
            .mount(&server)
            .await;

        let addr = *server.address();
        let cfg = AnthropicApiConfig {
            id: PROVIDER.into(),
            auth: Arc::new(routectl_core::StaticToken::new("beta-repair-test-key")),
            base_url: format!("http://api.anthropic.com:{}", addr.port()),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::ApiKey,
            header_extras: Vec::new(),
            user_agent: None,
            forward_client_headers: Vec::new(),
            context_management: false,
            max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
            session_id: None,
            cloak: CloakConfig::default(),
            use_forwarded_bearer: false,
            #[cfg(feature = "bedrock")]
            mantle: None,
        };
        let provider = AnthropicApiProvider::new(cfg).with_host_resolved_to_for_tests(addr);

        let mut router = Router::new(Arc::new(lane_config()));
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
            seen,
            _server: server,
        }
    }

    async fn send(&self) -> routectl_router::Dispatched {
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
            anthropic_beta: vec![KEPT.to_string(), REJECTED.to_string()],
            ..Default::default()
        };
        self.router
            .complete_with_options(req, RouterOptions::default())
            .await
    }

    fn headers(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[tokio::test]
async fn a_mantle_beta_rejection_is_repaired_learned_and_withheld_before_the_next_send() {
    // Arrange
    let lane = Lane::start().await;
    let both = [KEPT, REJECTED].join(",");

    // Act
    let first = lane.send().await;

    // Assert: the capture, then the same request without the named flag.
    assert!(
        first.result.is_ok(),
        "the client sees the repaired success: {:?}",
        first.result.err()
    );
    assert_eq!(lane.headers(), vec![both, KEPT.to_string()]);
    let learned: Vec<&str> = first
        .meta
        .learned_capabilities
        .iter()
        .map(|ev| ev.capability_key.as_str())
        .collect();
    let key = beta_capability_key(REJECTED).expect("well-formed flag");
    assert_eq!(learned, vec![key.as_str()], "one beta negative");
    assert_eq!(first.meta.learned_capabilities[0].upstream_status, 400);

    // Act: the next request on the same lane.
    let second = lane.send().await;

    // Assert: one wire call, already without the flag, nothing new learned.
    assert!(second.result.is_ok());
    assert_eq!(lane.headers()[2..], [KEPT.to_string()]);
    assert!(second.meta.learned_capabilities.is_empty());
}
