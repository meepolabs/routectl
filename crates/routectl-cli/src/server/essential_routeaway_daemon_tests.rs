//! A capability a chain's lanes refuse, end to end: a real daemon, real HTTP
//! into its Anthropic ingress, real Bedrock egresses against one mock origin
//! per lane. Every negative is learned by the daemon itself from the
//! upstream's answer (a committed capture body with its lifted
//! `x-amzn-errortype`, or a prose answer to a structured-output request),
//! never planted in the registry.
//!
//! The rows: an operator-essential droppable refused on one lane routes to
//! the other with the tool intact; a self-identifying refusal on every lane
//! answers the typed 400 without dialing; inferred negatives on every lane
//! still dispatch; an operator `force_supported` added after every lane
//! learned the refusal (across a restart that restores the negatives from the
//! ledger) sends the request to that lane again; and count_tokens is never
//! refused.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use routectl_auth::{MemoryStore, SecretStore};
use routectl_providers::bedrock::{
    BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth,
};
use routectl_router::{
    CURRENT_CONFIG_VERSION, CatalogOverlay, Config, ResolvedModel, Router, parse_config,
};
use routectl_testkit::ScopedEnv;
use serde_json::{Value, json};
use wiremock::matchers::{method, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const VALIDATION_CAPTURE: &str =
    include_str!("../../../routectl-router/tests/fixtures/bedrock_validation_capture.json");
const FORCED_TOOL_CHOICE_CAPTURE: &str =
    include_str!("../../../routectl-router/tests/fixtures/forced_tool_choice_capture.json");

const ALIAS: &str = "route";
/// The same two lanes in the other order.
const ALIAS_B_FIRST: &str = "route-b-first";
const PROVIDER: &str = "p";
const REGION: &str = "us-east-1";
/// Synthetic Anthropic-family ids: count_tokens admits the seats, and no
/// baked catalog prior matches them.
const LANE_A_ID: &str = "anthropic.lane-a";
const LANE_B_ID: &str = "anthropic.lane-b";
const INVOKE_PATH: &str = "/invoke$";
const COUNT_TOKENS_PATH: &str = "/count-tokens$";

fn capture(text: &str) -> Value {
    serde_json::from_str(text).expect("capture is JSON")
}

/// The discriminator header bedrock-runtime sends on a validation 400.
fn validation_error_type() -> String {
    capture(VALIDATION_CAPTURE)["error_type_header"]
        .as_str()
        .expect("captured error type header")
        .to_string()
}

fn advisor_rejection_body() -> String {
    capture(VALIDATION_CAPTURE)["canaries"]["advisor-tool"]["body"]
        .as_str()
        .expect("captured advisor body")
        .to_string()
}

fn forced_tool_choice_rejection_body() -> String {
    capture(FORCED_TOOL_CHOICE_CAPTURE)["bedrock"]["invoke_body"]
        .as_str()
        .expect("captured forced tool_choice invoke body")
        .to_string()
}

fn rejection(body: String) -> ResponseTemplate {
    ResponseTemplate::new(400)
        .insert_header("x-amzn-errortype", validation_error_type().as_str())
        .insert_header("content-type", "application/json")
        .set_body_string(body)
}

fn served(text: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(json!({
        "id": "msg_1", "type": "message", "role": "assistant", "model": LANE_A_ID,
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn", "stop_sequence": null,
        "usage": {"input_tokens": 3, "output_tokens": 1}
    }))
}

fn bearer_key() -> String {
    ["essential", "routeaway", "test", "key"].join("-")
}

/// Two Bedrock lanes (`a`, `b`) on one provider entry, chained under
/// [`ALIAS`] and, reversed, [`ALIAS_B_FIRST`]; `capability` is appended
/// verbatim.
fn two_lane_config(capability: &str) -> Config {
    let text = format!(
        "version = {CURRENT_CONFIG_VERSION}\n\n\
         [providers.{PROVIDER}]\n\
         kind = \"bedrock\"\n\
         region = \"{REGION}\"\n\
         api_shape = \"invoke\"\n\
         creds = {{ kind = \"bearer-key\", key_ref = \"{key_ref}\" }}\n\n\
         [models.a]\n\
         provider = \"{PROVIDER}\"\n\
         upstream = \"{LANE_A_ID}\"\n\n\
         [models.b]\n\
         provider = \"{PROVIDER}\"\n\
         upstream = \"{LANE_B_ID}\"\n\n\
         [aliases]\n\
         {ALIAS} = [\"a\", \"b\"]\n\
         {ALIAS_B_FIRST} = [\"b\", \"a\"]\n\n\
         [retry]\n\
         max_attempts = 1\n\n\
         {capability}",
        key_ref = crate::test_secret::file_ref(&bearer_key()),
    );
    parse_config(&text).expect("valid two-lane bedrock config")
}

async fn bedrock_model(nickname: &str, model_id: &str, origin: &str) -> Arc<ResolvedModel> {
    let creds = BedrockCreds::BearerKey { key: bearer_key() };
    let cfg = BedrockConfig {
        id: format!("bedrock:{PROVIDER}"),
        region: REGION.into(),
        model_id: model_id.into(),
        api_shape: BedrockApiShape::Invoke,
        creds: creds.clone(),
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
        adaptive_thinking: None,
    };
    let resolved = auth::resolve(&creds, REGION)
        .await
        .expect("bearer creds resolve");
    let provider = BedrockProvider::new(cfg, resolved)
        .expect("canonical region")
        .with_runtime_origin_for_tests(origin);
    Arc::new(ResolvedModel::new(
        nickname,
        PROVIDER,
        Arc::new(provider),
        model_id,
    ))
}

/// One boot of the daemon over an injected router, stopped gracefully so its
/// usage writer drains before the next boot reads the ledger.
struct Boot {
    base_url: String,
    stop: tokio::sync::oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<()>,
}

impl Boot {
    async fn start(config: Config, router: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let base_url = format!("http://{}", listener.local_addr().expect("read addr"));
        let (stop, stopped) = tokio::sync::oneshot::channel();
        let secrets: Arc<dyn SecretStore> = Arc::new(MemoryStore::new());
        let handle = tokio::spawn(async move {
            super::serve::serve_on_listener_with_injected_router(
                Arc::new(config),
                Arc::new(CatalogOverlay::default()),
                listener,
                None,
                Some(secrets),
                super::serve::DaemonTestSeams {
                    injected_router: Some(router),
                    shutdown: Some(stopped),
                    ..Default::default()
                },
            )
            .await
            .expect("the daemon serves");
        });
        let boot = Self {
            base_url,
            stop,
            handle,
        };
        boot.await_health().await;
        boot
    }

    async fn await_health(&self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if reqwest::get(format!("{}/health", self.base_url))
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the daemon never answered /health"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    async fn stop(self) {
        let _ = self.stop.send(());
        self.handle.await.expect("the daemon stops cleanly");
    }
}

/// A daemon serving the two-lane chain, each lane aimed at its own mock. The
/// usage ledger and config home outlive a restart.
struct Chain {
    boot: Boot,
    lane_a: MockServer,
    lane_b: MockServer,
    usage_dir: tempfile::TempDir,
    _xdg: ScopedEnv,
    _xdg_dir: tempfile::TempDir,
}

impl Chain {
    /// Spawn the daemon over `capability`, the lanes answering `/invoke`
    /// with `a` and `b` and `/count-tokens` with a count.
    async fn spawn(capability: &str, a: ResponseTemplate, b: ResponseTemplate) -> Self {
        let xdg_dir = tempfile::tempdir().expect("config tempdir");
        let xdg = ScopedEnv::set("XDG_CONFIG_HOME", xdg_dir.path());
        let usage_dir = tempfile::tempdir().expect("usage tempdir");
        let lane_a = lane(a).await;
        let lane_b = lane(b).await;
        let boot = boot(capability, &usage_dir, &lane_a, &lane_b).await;
        Self {
            boot,
            lane_a,
            lane_b,
            usage_dir,
            _xdg: xdg,
            _xdg_dir: xdg_dir,
        }
    }

    /// Stop the daemon and boot a fresh one over `capability` on the same
    /// ledger, config home and lanes.
    async fn restart(self, capability: &str) -> Self {
        let Self {
            boot: old,
            lane_a,
            lane_b,
            usage_dir,
            _xdg,
            _xdg_dir,
        } = self;
        old.stop().await;
        let boot = boot(capability, &usage_dir, &lane_a, &lane_b).await;
        Self {
            boot,
            lane_a,
            lane_b,
            usage_dir,
            _xdg,
            _xdg_dir,
        }
    }

    async fn post(&self, route: &str, body: &Value) -> (u16, Value) {
        let resp = reqwest::Client::new()
            .post(format!("{}{route}", self.boot.base_url))
            .header("anthropic-version", "2023-06-01")
            .json(body)
            .send()
            .await
            .expect("the daemon answers");
        let status = resp.status().as_u16();
        let text = resp.text().await.expect("response body");
        (
            status,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }

    async fn messages(&self, body: &Value) -> (u16, Value) {
        self.post("/v1/messages", body).await
    }

    /// Hits on each lane's `/invoke`, `(a, b)`.
    async fn invoke_hits(&self) -> (usize, usize) {
        (
            invoke_bodies(&self.lane_a).await.len(),
            invoke_bodies(&self.lane_b).await.len(),
        )
    }

    /// From now on, log each lane's `/invoke` dial in order, answering as
    /// before with `answer`.
    async fn log_dials(&self, answer: &ResponseTemplate) -> DialLog {
        let log = DialLog::default();
        for (name, server) in [("a", &self.lane_a), ("b", &self.lane_b)] {
            Mock::given(method("POST"))
                .and(path_regex(INVOKE_PATH))
                .respond_with(LoggedDial {
                    lane: name,
                    log: log.clone(),
                    answer: answer.clone(),
                })
                .with_priority(1)
                .mount(server)
                .await;
        }
        log
    }

    /// The learned rows the health panel reports, as `(lane, capability,
    /// tier, phase, observations)`.
    async fn learned_rows(&self) -> Vec<(String, String, String, String, u64)> {
        let resp = reqwest::get(format!("{}/status/health", self.boot.base_url))
            .await
            .expect("GET /status/health");
        assert!(resp.status().is_success(), "GET /status/health failed");
        let body: Value = resp.json().await.expect("health panel json");
        let mut rows: Vec<_> = body["data"]["learned_negatives"]
            .as_array()
            .expect("learned rows")
            .iter()
            .filter(|row| row["verdict"] == "broken")
            .map(|row| {
                let text = |name: &str| row[name].as_str().unwrap_or_default().to_string();
                (
                    text("state_key"),
                    text("capability_key"),
                    text("signal_tier"),
                    text("phase"),
                    row["observations"].as_u64().unwrap_or_default(),
                )
            })
            .collect();
        rows.sort();
        rows
    }

    async fn shutdown(self) {
        self.boot.stop().await;
    }
}

/// Boot the daemon over a fresh router for `capability`, its ledger in
/// `usage_dir`.
async fn boot(
    capability: &str,
    usage_dir: &tempfile::TempDir,
    lane_a: &MockServer,
    lane_b: &MockServer,
) -> Boot {
    let mut config = two_lane_config(capability);
    config.usage.db_path = usage_dir.path().join("usage.db");
    let mut router = Router::new(Arc::new(config.clone()));
    router.install_resolved_models(BTreeMap::from([
        (
            "a".to_string(),
            bedrock_model("a", LANE_A_ID, &lane_a.uri()).await,
        ),
        (
            "b".to_string(),
            bedrock_model("b", LANE_B_ID, &lane_b.uri()).await,
        ),
    ]));
    Boot::start(config, router).await
}

/// The lanes dialed, in order, since [`Chain::log_dials`].
#[derive(Clone, Default)]
struct DialLog(Arc<std::sync::Mutex<Vec<&'static str>>>);

impl DialLog {
    fn order(&self) -> Vec<&'static str> {
        self.0.lock().expect("dial log").clone()
    }
}

struct LoggedDial {
    lane: &'static str,
    log: DialLog,
    answer: ResponseTemplate,
}

impl wiremock::Respond for LoggedDial {
    fn respond(&self, _: &wiremock::Request) -> ResponseTemplate {
        self.log.0.lock().expect("dial log").push(self.lane);
        self.answer.clone()
    }
}

async fn lane(invoke: ResponseTemplate) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path_regex(INVOKE_PATH))
        .respond_with(invoke)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path_regex(COUNT_TOKENS_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"inputTokens": 7})))
        .mount(&server)
        .await;
    server
}

async fn bodies_on(server: &MockServer, suffix: &str) -> Vec<Value> {
    server
        .received_requests()
        .await
        .expect("recording")
        .iter()
        .filter(|req| req.url.path().ends_with(suffix))
        .map(|req| serde_json::from_slice(&req.body).expect("upstream body json"))
        .collect()
}

async fn invoke_bodies(server: &MockServer) -> Vec<Value> {
    bodies_on(server, "/invoke").await
}

fn lane_key(model_id: &str) -> String {
    format!("{PROVIDER}#{model_id}")
}

fn base_request() -> Value {
    json!({
        "model": ALIAS, "max_tokens": 16,
        "messages": [{"role": "user", "content": "hi"}]
    })
}

fn advisor_request() -> Value {
    let mut body = base_request();
    body["tools"] = json!([{"type": "advisor", "name": "advisor"}]);
    body
}

fn forced_tool_choice_request() -> Value {
    let mut body = base_request();
    body["tools"] = json!([{
        "name": "lookup", "description": "look a thing up",
        "input_schema": {"type": "object", "properties": {}}
    }]);
    body["tool_choice"] = json!({"type": "any"});
    body
}

fn structured_output_request() -> Value {
    let mut body = base_request();
    body["output_config"] = json!({"format": {
        "type": "json_schema",
        "schema": {
            "type": "object",
            "properties": {"answer": {"type": "string"}},
            "required": ["answer"],
            "additionalProperties": false
        }
    }});
    body
}

/// The client message of the typed refusal for `capability`.
fn refusal_message(capability: &str) -> String {
    format!(
        "no provider configured for this route supports the requested capability `{capability}`"
    )
}

fn is_typed_refusal(body: &Value, capability: &str) -> bool {
    body["error"]["type"] == "invalid_request_error"
        && body["error"]["message"] == refusal_message(capability).as_str()
}

/// Learn the forced tool_choice negative on every lane with one request both
/// lanes refuse.
async fn learn_forced_tool_choice_everywhere(chain: &Chain) {
    let (status, body) = chain.messages(&forced_tool_choice_request()).await;
    assert_eq!(
        status, 400,
        "the learning request is refused upstream: {body}"
    );
    assert!(
        !is_typed_refusal(&body, "forced_tool_choice"),
        "nothing was known before the first request: {body}"
    );
    assert_eq!(chain.invoke_hits().await, (1, 1), "both lanes were dialed");
}

#[tokio::test]
#[serial_test::serial]
async fn an_essential_droppable_refused_on_one_lane_is_served_by_the_other_with_the_tool_intact() {
    // Arrange
    let chain = Chain::spawn(
        "[capability]\nessential = [\"advisor\"]\n",
        rejection(advisor_rejection_body()),
        served("ok"),
    )
    .await;

    // Act
    let (first, first_body) = chain.messages(&advisor_request()).await;
    let (second, second_body) = chain.messages(&advisor_request()).await;

    // Assert
    assert_eq!(
        first, 200,
        "the fallback serves the learning request: {first_body}"
    );
    assert_eq!(
        second, 200,
        "the fallback serves the next request: {second_body}"
    );
    let (a_hits, b_hits) = chain.invoke_hits().await;
    assert_eq!(a_hits, 1, "lane a is dialed once, then routed away from");
    assert_eq!(b_hits, 2);
    for sent in invoke_bodies(&chain.lane_b).await {
        let types: Vec<&str> = sent["tools"]
            .as_array()
            .expect("tools on the wire")
            .iter()
            .filter_map(|tool| tool["type"].as_str())
            .collect();
        assert_eq!(
            types,
            ["advisor"],
            "the advisor tool reaches lane b: {sent}"
        );
    }
    assert_eq!(
        chain.learned_rows().await,
        [(
            lane_key(LANE_A_ID),
            "advisor".to_string(),
            "self-identifying".to_string(),
            "f1".to_string(),
            1
        )]
    );
    chain.shutdown().await;
}

#[tokio::test]
#[serial_test::serial]
async fn a_capability_every_lane_refuses_outright_is_answered_without_dialing() {
    // Arrange
    let refused = || rejection(forced_tool_choice_rejection_body());
    let chain = Chain::spawn("", refused(), refused()).await;
    learn_forced_tool_choice_everywhere(&chain).await;
    let learned: Vec<_> = [LANE_A_ID, LANE_B_ID]
        .iter()
        .map(|id| {
            (
                lane_key(id),
                "forced_tool_choice".to_string(),
                "self-identifying".to_string(),
                "f1".to_string(),
                1,
            )
        })
        .collect();
    assert_eq!(chain.learned_rows().await, learned);

    // Act
    let (complete, complete_body) = chain.messages(&forced_tool_choice_request()).await;
    let mut streamed = forced_tool_choice_request();
    streamed["stream"] = json!(true);
    let (stream, stream_body) = chain.messages(&streamed).await;

    // Assert
    assert_eq!(complete, 400, "{complete_body}");
    assert!(
        is_typed_refusal(&complete_body, "forced_tool_choice"),
        "the typed refusal names the capability: {complete_body}"
    );
    assert_eq!(stream, 400, "{stream_body}");
    assert!(
        is_typed_refusal(&stream_body, "forced_tool_choice"),
        "the streamed request is refused the same way: {stream_body}"
    );
    assert_eq!(
        chain.invoke_hits().await,
        (1, 1),
        "no lane is dialed once every lane refuses outright"
    );
    chain.shutdown().await;
}

#[tokio::test]
#[serial_test::serial]
async fn inferred_negatives_on_every_lane_still_dispatch() {
    // Arrange -- a prose answer to a strict structured-output request is a
    // suspected absence: an inferred live negative, recorded and shown but
    // advisory, so it never routes. Each lane leads one alias, so each
    // answers twice and records a corroborated negative.
    let chain = Chain::spawn("", served("not json"), served("not json")).await;
    for alias in [ALIAS, ALIAS, ALIAS_B_FIRST, ALIAS_B_FIRST] {
        let mut request = structured_output_request();
        request["model"] = json!(alias);
        let (status, body) = chain.messages(&request).await;
        assert_eq!(status, 200, "{alias}: {body}");
    }
    assert_eq!(chain.invoke_hits().await, (2, 2));
    let learned: Vec<_> = [LANE_A_ID, LANE_B_ID]
        .iter()
        .map(|id| {
            (
                lane_key(id),
                "structured_output".to_string(),
                "inferred".to_string(),
                "f3".to_string(),
                2,
            )
        })
        .collect();
    assert_eq!(
        chain.learned_rows().await,
        learned,
        "both lanes recorded it"
    );

    // Act
    let (status, body) = chain.messages(&structured_output_request()).await;

    // Assert
    assert_eq!(status, 200, "an inferred negative never refuses: {body}");
    let (a_hits, b_hits) = chain.invoke_hits().await;
    assert_eq!(a_hits + b_hits, 5, "the request reached a lane");
    chain.shutdown().await;
}

#[tokio::test]
#[serial_test::serial]
async fn force_supported_added_after_every_lane_refused_sends_the_request_to_that_lane() {
    // Arrange -- every lane learns the refusal and the chain answers the
    // typed 400; the operator then forces the capability on lane b and the
    // daemon restarts on the same ledger, restoring both negatives.
    let refused = || rejection(forced_tool_choice_rejection_body());
    let chain = Chain::spawn("", refused(), refused()).await;
    learn_forced_tool_choice_everywhere(&chain).await;
    let (status, body) = chain.messages(&forced_tool_choice_request()).await;
    assert!(
        status == 400 && is_typed_refusal(&body, "forced_tool_choice"),
        "before the override the chain is refused: {body}"
    );
    let learned: Vec<_> = [LANE_A_ID, LANE_B_ID]
        .iter()
        .map(|id| {
            (
                lane_key(id),
                "forced_tool_choice".to_string(),
                "self-identifying".to_string(),
                "f1".to_string(),
                1,
            )
        })
        .collect();
    assert_eq!(chain.learned_rows().await, learned);
    let chain = chain
        .restart(
            "[capability.overrides.\"p:b\"]\n\
             force_supported = [\"forced_tool_choice\"]\n",
        )
        .await;
    assert_eq!(
        chain.learned_rows().await,
        learned,
        "the restart restored both negatives from the ledger"
    );
    let dials = chain.log_dials(&refused()).await;

    // Act
    let (status, body) = chain.messages(&forced_tool_choice_request()).await;

    // Assert -- lane b leads; lane a, still refused, is only the tail.
    assert_eq!(status, 400, "{body}");
    assert!(
        !is_typed_refusal(&body, "forced_tool_choice"),
        "the overridden lane is dialed and its own answer comes back: {body}"
    );
    assert_eq!(
        dials.order(),
        ["b", "a"],
        "the request was dispatched to lane b first"
    );
    let b_row = chain
        .learned_rows()
        .await
        .into_iter()
        .find(|row| row.0 == lane_key(LANE_B_ID));
    assert_eq!(
        b_row.map(|row| row.4),
        Some(1),
        "the masked rejection on lane b refreshes nothing"
    );
    chain.shutdown().await;
}

#[tokio::test]
#[serial_test::serial]
async fn count_tokens_dispatches_a_capability_every_lane_refuses() {
    // Arrange
    let refused = || rejection(forced_tool_choice_rejection_body());
    let chain = Chain::spawn("", refused(), refused()).await;
    learn_forced_tool_choice_everywhere(&chain).await;
    let (refused_status, refused_body) = chain.messages(&forced_tool_choice_request()).await;
    assert!(
        refused_status == 400 && is_typed_refusal(&refused_body, "forced_tool_choice"),
        "inference is refused on this chain: {refused_body}"
    );

    // Act
    let (status, body) = chain
        .post("/v1/messages/count_tokens", &forced_tool_choice_request())
        .await;

    // Assert
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["input_tokens"], 7, "{body}");
    let counted = bodies_on(&chain.lane_a, "/count-tokens").await.len()
        + bodies_on(&chain.lane_b, "/count-tokens").await.len();
    assert_eq!(counted, 1, "one lane counted the request");
    chain.shutdown().await;
}
