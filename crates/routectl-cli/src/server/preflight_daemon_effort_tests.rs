// A refused effort level learned and pre-flighted over real HTTP on both
// sides: the daemon's Anthropic ingress in front, a real anthropic-api egress
// behind it talking to a mock upstream that answers `xhigh` with the
// committed `Supported levels` capture.
// `include!`d into preflight_daemon_tests.rs; the fixtures and imports live
// there, and this fragment's own imports stay inside its module.

mod effort {
    use super::*;

    use routectl_providers::anthropic_api::{
        AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig,
    };
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const XHIGH_PATH: &str = "output_config.effort.xhigh";

    /// The committed anthropic-api capture of a model whose levels stop at
    /// `high` refusing `xhigh`: its status and verbatim body.
    fn captured_rejection() -> ResponseTemplate {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "../routectl-router/tests/fixtures/rejections/anthropic-api/\
             effort-xhigh-unsupported-opus-4-5.json",
        );
        let fixture: Value =
            serde_json::from_str(&std::fs::read_to_string(fixture).expect("fixture reads"))
                .expect("fixture parses");
        let status = u16::try_from(fixture["status"].as_u64().expect("fixture status"))
            .expect("an HTTP status");
        let body = fixture["body"].as_str().expect("fixture body").to_string();
        ResponseTemplate::new(status).set_body_raw(body, "application/json")
    }

    fn messages_success() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "id": "msg_effort",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 1}
        }))
    }

    /// A mock upstream that refuses any body sending `xhigh` with the
    /// captured rejection and serves everything else.
    async fn high_ceiling_upstream() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_partial_json(
                json!({"output_config": {"effort": "xhigh"}}),
            ))
            .respond_with(captured_rejection())
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(messages_success())
            .mount(&server)
            .await;
        server
    }

    /// A Router over `config` whose `p0` is a real anthropic-api provider.
    ///
    /// The config entry keeps its remote https base URL, which is what the
    /// attributability gate reads; the provider dials the Anthropic host on
    /// the mock's port and resolves that host to the mock, so only the address
    /// the egress connects to is substituted.
    fn router_over_upstream(config: &Arc<Config>, upstream: &MockServer) -> Router {
        let addr = *upstream.address();
        let cfg = AnthropicApiConfig {
            id: "p0".into(),
            auth: Arc::new(routectl_core::StaticToken::new("effort-key")),
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
            mantle: None,
        };
        let provider = AnthropicApiProvider::new(cfg).with_host_resolved_to_for_tests(addr);
        let mut router = Router::new(Arc::clone(config));
        let mut models: BTreeMap<String, Arc<ResolvedModel>> = BTreeMap::new();
        models.insert(
            NICKNAME.to_string(),
            Arc::new(ResolvedModel::new(
                NICKNAME.to_string(),
                "p0".to_string(),
                Arc::new(provider) as Arc<dyn Provider>,
                "claude-sonnet-4-5".to_string(),
            )),
        );
        router.install_resolved_models(models);
        router
    }

    /// The shape a Claude Code turn sends at its highest effort: adaptive
    /// thinking beside `xhigh`.
    fn xhigh_body() -> Value {
        json!({
            "model": ALIAS,
            "max_tokens": 64,
            "stream": false,
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "xhigh"},
            "messages": [{"role": "user", "content": "hello"}],
        })
    }

    /// `(thinking, output_config.effort)` of every body the upstream received.
    async fn wire_shapes(upstream: &MockServer) -> Vec<(Value, Value)> {
        upstream
            .received_requests()
            .await
            .expect("request recording is on")
            .iter()
            .map(|request| {
                let body: Value =
                    serde_json::from_slice(&request.body).expect("request body is JSON");
                (
                    body["thinking"].clone(),
                    body["output_config"]["effort"].clone(),
                )
            })
            .collect()
    }

    fn field_prefix() -> String {
        format!("{}{}", "fie", "ld:")
    }

    /// The persisted value of every ledger row the daemon wrote for the
    /// `xhigh` verdict, in write order.
    fn xhigh_ledger_values(db_path: &std::path::Path) -> Vec<Option<String>> {
        let key = format!("{}{XHIGH_PATH}", field_prefix());
        let db = routectl_usage::open_readonly(db_path).expect("read-only open");
        let mut stmt = db
            .conn()
            .prepare(
                "SELECT upstream_token FROM capability_events \
                 WHERE lane_key = ?1 AND capability = ?2 ORDER BY id",
            )
            .expect("prepare");
        stmt.query_map([STATE_KEY, key.as_str()], |r| r.get(0))
            .expect("query")
            .collect::<std::result::Result<_, _>>()
            .expect("rows read")
    }

    /// Wait until the committed verdict's row is on the daemon's ledger and its
    /// confirmation advancement has finished, so the next request reads the
    /// acknowledged state rather than racing it.
    async fn await_acknowledged_verdict(daemon: &Daemon, db_path: &std::path::Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while xhigh_ledger_values(db_path).is_empty() || daemon.confirmations().in_flight() > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the committed verdict's row never reached the ledger",
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Every resident effort key in the daemon's live learned registry.
    fn resident_effort_keys(daemon: &Daemon) -> Vec<String> {
        let effort_prefix = format!("{}output_config.effort.", field_prefix());
        daemon
            .live_router()
            .learned_capability_snapshot()
            .into_iter()
            .filter(|entry| entry.state_key == STATE_KEY)
            .map(|entry| entry.feature_key)
            .filter(|key| key.starts_with(&effort_prefix))
            .collect()
    }

    /// An adaptive turn's refused `xhigh` is retried at `high`, the level the
    /// refusal's list names nearest below it, and the success is learned as
    /// `xhigh -> high`; the NEXT turn over HTTP leaves already at `high` and
    /// draws no refusal.
    ///
    /// Mutation check: make the ranked arm of the pre-flight transform answer
    /// no stored value -> red (the second turn goes out at `xhigh` and is
    /// refused again).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_refused_effort_level_is_learned_and_the_next_turn_preflights_the_listed_level() {
        // Arrange
        let upstream = high_ceiling_upstream().await;
        let (config, dir) = daemon_config();
        let db_path = config.usage.db_path.clone();
        let router = router_over_upstream(&config, &upstream);
        let daemon = spawn_daemon(config, dir, router).await;
        let adaptive = json!({"type": "adaptive"});

        // Act: the first turn draws the rejection and the repaired retry.
        let first = post_inference(&daemon, &xhigh_body()).await;

        // Assert
        assert_eq!(first, reqwest::StatusCode::OK, "the repaired retry served");
        assert_eq!(
            wire_shapes(&upstream).await,
            vec![
                (adaptive.clone(), json!("xhigh")),
                (adaptive.clone(), json!("high")),
            ],
            "one refused attempt as sent, one retry at the listed level with the \
             thinking object unchanged",
        );
        await_acknowledged_verdict(&daemon, &db_path).await;
        assert_eq!(
            xhigh_ledger_values(&db_path),
            vec![Some("shape:high".to_string())],
            "one ledger row carrying the learned level",
        );

        // Act: the second turn, same body.
        let second = post_inference(&daemon, &xhigh_body()).await;

        // Assert
        assert_eq!(second, reqwest::StatusCode::OK);
        assert_eq!(
            wire_shapes(&upstream).await,
            vec![
                (adaptive.clone(), json!("xhigh")),
                (adaptive.clone(), json!("high")),
                (adaptive, json!("high")),
            ],
            "the second turn went out once, already at the learned level",
        );
        let counters = daemon.live_router().field_repair_counters();
        assert_eq!(counters.repair_attempted, 1, "only the first turn repaired");
        assert_eq!(counters.repair_succeeded, 1, "and its retry served");
        assert_eq!(counters.verdicts_learned, 1, "one verdict committed");
        assert_eq!(counters.preflight_actions, 1, "the second turn preflighted");
        assert_eq!(
            resident_effort_keys(&daemon),
            vec![format!("{}{XHIGH_PATH}", field_prefix())],
            "the learned registry holds the refused level's key alone",
        );
        daemon.shutdown().await;
    }
}
