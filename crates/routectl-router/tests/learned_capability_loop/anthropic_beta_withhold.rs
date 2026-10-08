//! An operator `unsupported = ["beta:<flag>"]` override keeps the flag off a
//! real anthropic-api lane's `anthropic-beta` header. A real `Router` drives
//! a real `AnthropicApiProvider` at a mock that records the header; the
//! provider keeps the exact Anthropic host as its base URL (with the mock's
//! port) and resolves it to the mock, so the own-OAuth floor gates fire as
//! they do in production.

use std::sync::{Mutex, PoisonError};

use routectl_core::identity::anthropic::{
    OAUTH_ANTHROPIC_BETA, default_claude_code_anthropic_betas,
};
use routectl_providers::anthropic_api::{
    AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig,
};
use routectl_router::{
    CURRENT_CONFIG_VERSION, ResolvedModel, beta_capability_key, parse_config,
    preflight_config_version, preflight_retired_capability_keys,
};

use super::*;

const NEVER: &str = "zz-never-send-2099-01-01";
const KEPT: &str = "kept-2099-01-01";
const CC_FLAG: &str = "claude-code-20250219";

const PROVIDER: &str = "anth";
const NICKNAME: &str = "m1";
const MODEL_ID: &str = "claude-sonnet-4-5";

/// Records the `anthropic-beta` header of every request and answers with a
/// minimal Messages success body.
struct BetaHeaderCapture {
    seen: Arc<Mutex<Vec<String>>>,
}

impl Respond for BetaHeaderCapture {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        let beta = request
            .headers
            .get("anthropic-beta")
            .map(|v| v.to_str().expect("ascii header").to_string())
            .unwrap_or_default();
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(beta);
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_withhold",
            "type": "message",
            "role": "assistant",
            "model": MODEL_ID,
            "content": [{ "type": "text", "text": "ok" }],
            "stop_reason": "end_turn",
            "usage": { "input_tokens": 3, "output_tokens": 1 }
        }))
    }
}

/// A current-version config with one anthropic-api lane whose operator
/// override withholds `unsupported`; the preflights a real load runs must
/// admit it.
fn lane_config(unsupported: &[&str]) -> Config {
    let keys: Vec<String> = unsupported
        .iter()
        .map(|f| format!("\"{}\"", beta_capability_key(f).expect("well-formed flag")))
        .collect();
    let text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\n\
         [providers.{PROVIDER}]\n\
         kind = \"anthropic-api\"\n\
         api_key_ref = \"{key_ref}\"\n\
         auth_kind = \"oauth-bearer\"\n\n\
         [models.{NICKNAME}]\n\
         provider = \"{PROVIDER}\"\n\
         upstream = \"{MODEL_ID}\"\n\n\
         [capability.overrides.{PROVIDER}]\n\
         unsupported = [{keys}]\n",
        key_ref = common::file_ref("withhold-test-token"),
        keys = keys.join(", "),
    );
    assert_eq!(
        preflight_config_version(&text).expect("version preflight"),
        CURRENT_CONFIG_VERSION
    );
    preflight_retired_capability_keys(&text).expect("no retired key in the test config");
    parse_config(&text).expect("valid test config")
}

struct Lane {
    router: Router,
    seen: Arc<Mutex<Vec<String>>>,
    _server: MockServer,
}

impl Lane {
    async fn start(unsupported: &[&str]) -> Self {
        let server = MockServer::start().await;
        let seen = Arc::new(Mutex::new(Vec::new()));
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(BetaHeaderCapture {
                seen: Arc::clone(&seen),
            })
            .mount(&server)
            .await;

        let addr = *server.address();
        let cfg = AnthropicApiConfig {
            id: PROVIDER.into(),
            auth: Arc::new(routectl_core::StaticToken::new("withhold-test-token")),
            base_url: format!("http://api.anthropic.com:{}", addr.port()),
            anthropic_version: "2023-06-01".into(),
            auth_kind: AuthKind::OauthBearer,
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

        let mut router = Router::new(Arc::new(lane_config(unsupported)));
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

    async fn send(&self, client_betas: &[&str], claude_code: bool) -> String {
        let mut req = ChatRequest {
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
        if claude_code {
            req.routectl_internal.claude_code_headers =
                vec![("x-claude-code-session-id".into(), "sess-1".into())];
        }
        let before = self.headers().len();
        let dispatched = self
            .router
            .complete_with_options(req, RouterOptions::default())
            .await;
        assert!(
            dispatched.result.is_ok(),
            "the lane must serve the request: {:?}",
            dispatched.result.err()
        );
        let headers = self.headers();
        assert_eq!(headers.len(), before + 1, "exactly one wire call");
        headers[before].clone()
    }

    fn headers(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// `base` followed by every minted floor flag it does not already hold, in
/// floor order: the header a non-CC request composes on this lane.
fn with_cc_floor(base: &[&str]) -> String {
    let mut out: Vec<&str> = base.to_vec();
    for flag in default_claude_code_anthropic_betas() {
        if !out.contains(flag) {
            out.push(flag);
        }
    }
    out.join(",")
}

#[tokio::test]
async fn unsupported_override_withholds_the_flag_from_a_non_cc_request() {
    // Arrange
    let lane = Lane::start(&[NEVER]).await;

    // Act
    let header = lane.send(&[KEPT, NEVER, CC_FLAG], false).await;

    // Assert
    assert_eq!(
        header,
        with_cc_floor(&[KEPT, CC_FLAG]),
        "the overridden flag never reaches the wire; the rest keep client \
         order and the minted floor follows",
    );
}

#[tokio::test]
async fn unsupported_override_withholds_the_flag_from_a_claude_code_request() {
    // Arrange: the override also names the OAuth gate, which this lane
    // asserts itself and so must still send.
    let lane = Lane::start(&[NEVER, OAUTH_ANTHROPIC_BETA]).await;

    // Act
    let header = lane
        .send(&[CC_FLAG, OAUTH_ANTHROPIC_BETA, NEVER, KEPT], true)
        .await;

    // Assert
    assert_eq!(
        header,
        [CC_FLAG, OAUTH_ANTHROPIC_BETA, KEPT].join(","),
        "a genuine Claude Code request keeps its own set, minus the \
         overridden flag, with the OAuth gate in place",
    );
}

#[tokio::test]
async fn lane_without_an_override_sends_every_client_flag() {
    // Arrange: an override on an unrelated flag, so the config shape matches.
    let lane = Lane::start(&["unrelated-2099-01-01"]).await;

    // Act
    let header = lane.send(&[KEPT, NEVER, CC_FLAG], false).await;

    // Assert
    assert_eq!(header, with_cc_floor(&[KEPT, NEVER, CC_FLAG]));
}
