// The `thinking.type.adaptive` rewrite over real HTTP on both sides: the
// daemon's Anthropic ingress in front, a real Bedrock Converse egress behind
// it talking to a mock upstream that answers with the committed capture.
// `include!`d into preflight_daemon_tests.rs; the fixtures and imports live
// there, and this fragment's own imports stay inside its module.

mod thinking_adaptive {
    use super::*;

    use routectl_providers::bedrock::{
        BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth,
    };
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ADAPTIVE_PATH: &str = "thinking.type.adaptive";
    const REGION: &str = "us-east-1";
    const PROVIDER: &str = "p0";
    /// Synthetic ids keep every baked catalog prior out of play; the first
    /// classifies as a non-Anthropic model, the second as an Anthropic one.
    const NON_CLAUDE_MODEL: &str = "zz.thinking-variant-test";
    const CLAUDE_MODEL: &str = "us.anthropic.zz-thinking-variant-test";
    const ERROR_TYPE_HEADER: &str =
        "ValidationException:http://internal.amazon.com/coral/com.amazon.bedrock/";
    /// The client's `max_tokens`: large enough that the legacy enabled shape
    /// has a budget window, as a Claude Code thinking turn's is.
    const CLIENT_MAX_TOKENS: u32 = 32_000;
    /// The budget the legacy enabled shape composes for the client's `high`
    /// effort.
    const HIGH_EFFORT_BUDGET: u32 = 24_576;

    /// Assembled at runtime so no key-shaped literal sits in the source.
    fn bearer_key() -> String {
        ["thinking", "variant", "test", "key"].join("-")
    }

    /// The committed Converse capture of a model refusing `thinking.type:
    /// adaptive`: its status and verbatim body behind the exception header.
    fn captured_rejection() -> ResponseTemplate {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "../routectl-router/tests/fixtures/rejections/bedrock/\
             thinking-type-adaptive-unknown-variant-converse.json",
        );
        let fixture: Value =
            serde_json::from_str(&std::fs::read_to_string(fixture).expect("fixture reads"))
                .expect("fixture parses");
        let status = u16::try_from(fixture["status"].as_u64().expect("fixture status"))
            .expect("an HTTP status");
        let body = fixture["body"].as_str().expect("fixture body").to_string();
        ResponseTemplate::new(status)
            .insert_header("x-amzn-ErrorType", ERROR_TYPE_HEADER)
            .set_body_raw(body, "application/json")
    }

    fn converse_success() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "output": {"message": {"role": "assistant", "content": [{"text": "ok"}]}},
            "stopReason": "end_turn",
            "usage": {"inputTokens": 3, "outputTokens": 1, "totalTokens": 4},
        }))
    }

    /// A mock upstream that, when `refuses_adaptive`, refuses any bag still
    /// carrying adaptive thinking with the captured rejection, and serves
    /// everything else.
    async fn converse_upstream(refuses_adaptive: bool) -> MockServer {
        let server = MockServer::start().await;
        if refuses_adaptive {
            Mock::given(method("POST"))
                .and(body_partial_json(json!({
                    "additionalModelRequestFields": {"thinking": {"type": "adaptive"}},
                })))
                .respond_with(captured_rejection())
                .with_priority(1)
                .mount(&server)
                .await;
        }
        Mock::given(method("POST"))
            .respond_with(converse_success())
            .mount(&server)
            .await;
        server
    }

    /// A servable Bedrock Converse config serving `model`, with its ledger in
    /// a tempdir. The region-derived runtime host is what the attributability
    /// gate reads; only the provider's transport is aimed at the mock.
    fn converse_config(model: &str) -> (Arc<Config>, tempfile::TempDir) {
        let toml_text = format!(
            "[retry]\nmax_attempts = 1\ninitial_backoff_ms = 0\njitter_ms = 0\n\n\
             [providers.{PROVIDER}]\nkind = \"bedrock\"\nregion = \"{REGION}\"\n\
             creds = {{ kind = \"default-chain\" }}\napi_shape = \"converse\"\n"
        );
        let mut config: Config = toml::from_str(&toml_text).expect("valid test toml");
        config
            .models
            .insert(NICKNAME.to_string(), ModelEntry::new(PROVIDER, model));
        config
            .aliases
            .insert(ALIAS.to_string(), AliasValue::Single(NICKNAME.to_string()));
        let dir = tempfile::tempdir().expect("usage tempdir");
        config.usage.db_path = dir.path().join("usage.db");
        (Arc::new(config), dir)
    }

    /// A Router over `config` whose `p0` is a real Bedrock Converse provider
    /// for `model`, dialing `upstream`.
    async fn router_over_upstream(
        config: &Arc<Config>,
        upstream: &MockServer,
        model: &str,
    ) -> Router {
        let creds = BedrockCreds::BearerKey { key: bearer_key() };
        let cfg = BedrockConfig {
            id: format!("bedrock:{PROVIDER}"),
            region: REGION.into(),
            model_id: model.into(),
            api_shape: BedrockApiShape::Converse,
            creds: creds.clone(),
            user_agent: None,
            header_extras: Vec::new(),
            anthropic_beta: Vec::new(),
            additional_model_request_fields: None,
        };
        let resolved = auth::resolve(&creds, REGION)
            .await
            .expect("bearer creds resolve");
        let provider = BedrockProvider::new(cfg, resolved)
            .expect("canonical region")
            .with_runtime_origin_for_tests(&upstream.uri());
        let mut router = Router::new(Arc::clone(config));
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            NICKNAME.to_string(),
            Arc::new(ResolvedModel::new(
                NICKNAME.to_string(),
                PROVIDER.to_string(),
                Arc::new(provider) as Arc<dyn Provider>,
                model.to_string(),
            )),
        );
        router.install_resolved_models(models);
        router
    }

    /// The shape a Claude Code thinking turn sends: adaptive thinking with
    /// its display choice, beside a positive effort.
    fn adaptive_body() -> Value {
        json!({
            "model": ALIAS,
            "max_tokens": CLIENT_MAX_TOKENS,
            "stream": false,
            "thinking": {"type": "adaptive", "display": "omitted"},
            "output_config": {"effort": "high"},
            "messages": [{"role": "user", "content": "hello"}],
        })
    }

    /// The Converse bag's `output_config` of every body the upstream
    /// received.
    async fn bag_output_config(upstream: &MockServer) -> Vec<Value> {
        upstream
            .received_requests()
            .await
            .expect("request recording is on")
            .iter()
            .map(|request| {
                let body: Value =
                    serde_json::from_slice(&request.body).expect("request body is JSON");
                body["additionalModelRequestFields"]["output_config"].clone()
            })
            .collect()
    }

    /// The Converse bag's `thinking` of every body the upstream received.
    async fn bag_thinking(upstream: &MockServer) -> Vec<Value> {
        upstream
            .received_requests()
            .await
            .expect("request recording is on")
            .iter()
            .map(|request| {
                let body: Value =
                    serde_json::from_slice(&request.body).expect("request body is JSON");
                body["additionalModelRequestFields"]["thinking"].clone()
            })
            .collect()
    }

    /// How many ledger rows the daemon wrote for the adaptive-thinking
    /// verdict on the lane serving `model`.
    fn adaptive_ledger_rows(db_path: &std::path::Path, model: &str) -> usize {
        let key = format!("{}{ADAPTIVE_PATH}", format_args!("{}{}", "fie", "ld:"));
        let lane = format!("{PROVIDER}#{model}");
        let db = routectl_usage::open_readonly(db_path).expect("read-only open");
        let mut stmt = db
            .conn()
            .prepare(
                "SELECT COUNT(*) FROM capability_events WHERE lane_key = ?1 AND capability = ?2",
            )
            .expect("prepare");
        let count: i64 = stmt
            .query_row([lane.as_str(), key.as_str()], |r| r.get(0))
            .expect("count");
        usize::try_from(count).expect("a non-negative count")
    }

    /// Wait until the committed verdict's row is on the daemon's ledger and its
    /// confirmation advancement has finished, so the next request reads the
    /// acknowledged state rather than racing it.
    async fn await_acknowledged_verdict(daemon: &Daemon, db_path: &std::path::Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while adaptive_ledger_rows(db_path, NON_CLAUDE_MODEL) == 0
            || daemon.confirmations().in_flight() > 0
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the committed verdict's row never reached the ledger",
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A non-Claude Converse model refusing adaptive thinking is repaired to
    /// the legacy enabled shape behind the client's back, and the NEXT request
    /// over HTTP leaves already enabled.
    ///
    /// Mutation check: delete `thinking.type.enabled` from the adaptive row's
    /// accepted set in `FIELD_REPAIRS` -> red (the captured 400 reaches the
    /// client).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_refused_adaptive_turn_on_a_non_claude_converse_model_is_repaired_and_preflights() {
        // Arrange
        let upstream = converse_upstream(true).await;
        let (config, dir) = converse_config(NON_CLAUDE_MODEL);
        let db_path = config.usage.db_path.clone();
        let router = router_over_upstream(&config, &upstream, NON_CLAUDE_MODEL).await;
        let daemon = spawn_daemon(config, dir, router).await;
        let enabled = json!({
            "type": "enabled",
            "budget_tokens": HIGH_EFFORT_BUDGET,
            "display": "omitted",
        });

        // Act: the first turn draws the rejection and the repaired retry.
        let first = post_inference(&daemon, &adaptive_body()).await;

        // Assert
        assert_eq!(first, reqwest::StatusCode::OK, "the repaired retry served");
        assert_eq!(
            bag_thinking(&upstream).await,
            vec![
                json!({"type": "adaptive", "display": "omitted"}),
                enabled.clone(),
            ],
            "one refused attempt as sent, one legacy enabled retry keeping the display",
        );
        await_acknowledged_verdict(&daemon, &db_path).await;

        // Act: the second turn, same body.
        let second = post_inference(&daemon, &adaptive_body()).await;

        // Assert
        assert_eq!(second, reqwest::StatusCode::OK);
        let shapes = bag_thinking(&upstream).await;
        assert_eq!(
            shapes.len(),
            3,
            "the second turn went out once, drawing no rejection: {shapes:?}",
        );
        assert_eq!(shapes[2], enabled, "the second turn left already enabled");
        assert_eq!(
            bag_output_config(&upstream).await,
            vec![Value::Null; 3],
            "a non-Anthropic bag carries no output_config, as the shape these models \
             served before adaptive was forwarded",
        );
        let counters = daemon.live_router().field_repair_counters();
        assert_eq!(counters.repair_succeeded, 1, "only the first turn repaired");
        assert_eq!(counters.preflight_actions, 1, "the second turn preflighted");
        daemon.shutdown().await;
    }

    /// Control: a Claude model on the same lane shape takes adaptive thinking,
    /// so the client's object goes out as sent, once, and nothing is repaired
    /// or learned.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn an_accepted_adaptive_turn_on_a_claude_converse_model_is_forwarded_as_sent() {
        // Arrange
        let upstream = converse_upstream(false).await;
        let (config, dir) = converse_config(CLAUDE_MODEL);
        let db_path = config.usage.db_path.clone();
        let router = router_over_upstream(&config, &upstream, CLAUDE_MODEL).await;
        let daemon = spawn_daemon(config, dir, router).await;

        // Act
        let status = post_inference(&daemon, &adaptive_body()).await;

        // Assert
        assert_eq!(status, reqwest::StatusCode::OK);
        assert_eq!(
            bag_thinking(&upstream).await,
            vec![json!({"type": "adaptive", "display": "omitted"})],
            "sent once, as the client sent it",
        );
        let counters = daemon.live_router().field_repair_counters();
        assert_eq!(counters.repair_attempted, 0);
        assert_eq!(counters.preflight_actions, 0);
        assert_eq!(adaptive_ledger_rows(&db_path, CLAUDE_MODEL), 0);
        daemon.shutdown().await;
    }
}
