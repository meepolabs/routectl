// The `thinking.type.disabled` rewrite over real HTTP on both sides: the
// daemon's Anthropic ingress in front, a real anthropic-api egress behind it
// talking to a mock upstream that answers with the committed capture.
// `include!`d into preflight_daemon_tests.rs; the fixtures and imports live
// there, and this fragment's own imports stay inside its module.

mod thinking_disabled {
    use super::*;

    use routectl_providers::anthropic_api::{
        AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig,
    };
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const DISABLED_PATH: &str = "thinking.type.disabled";

    /// The committed anthropic-api capture of an adaptive-only model refusing
    /// `thinking.type: disabled`: its status and verbatim body.
    fn captured_rejection() -> ResponseTemplate {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(
            "../routectl-router/tests/fixtures/rejections/anthropic-api/\
             thinking-type-disabled-unsupported.json",
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
            "id": "msg_thinking_disabled",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 1}
        }))
    }

    /// A mock upstream that refuses any body still carrying disabled thinking
    /// with the captured rejection and serves everything else.
    async fn adaptive_only_upstream() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_partial_json(json!({"thinking": {"type": "disabled"}})))
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
            auth: Arc::new(routectl_core::StaticToken::new("thinking-disabled-key")),
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

    /// The shape a Claude Code plain turn sends: thinking off beside a
    /// positive effort.
    fn disabled_body() -> Value {
        json!({
            "model": ALIAS,
            "max_tokens": 64,
            "stream": false,
            "thinking": {"type": "disabled"},
            "output_config": {"effort": "high"},
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

    /// How many ledger rows the daemon wrote for the disabled-thinking verdict.
    fn disabled_ledger_rows(db_path: &std::path::Path) -> usize {
        let key = format!("{}{DISABLED_PATH}", format_args!("{}{}", "fie", "ld:"));
        let db = routectl_usage::open_readonly(db_path).expect("read-only open");
        let mut stmt = db
            .conn()
            .prepare(
                "SELECT COUNT(*) FROM capability_events WHERE lane_key = ?1 AND capability = ?2",
            )
            .expect("prepare");
        let count: i64 = stmt
            .query_row([STATE_KEY, key.as_str()], |r| r.get(0))
            .expect("count");
        usize::try_from(count).expect("a non-negative count")
    }

    /// Wait until the committed verdict's row is on the daemon's ledger and its
    /// confirmation advancement has finished, so the next request reads the
    /// acknowledged state rather than racing it.
    async fn await_acknowledged_verdict(daemon: &Daemon, db_path: &std::path::Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while disabled_ledger_rows(db_path) == 0 || daemon.confirmations().in_flight() > 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the committed verdict's row never reached the ledger",
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// A refused disabled-thinking request is repaired to adaptive behind the
    /// client's back, and the NEXT request over HTTP leaves already adaptive.
    ///
    /// Mutation check: delete the `thinking.type.disabled` row from
    /// `FIELD_REPAIRS` -> red (the captured 400 reaches the client).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_refused_disabled_turn_is_repaired_and_the_next_turn_preflights_adaptive() {
        // Arrange
        let upstream = adaptive_only_upstream().await;
        let (config, dir) = daemon_config();
        let db_path = config.usage.db_path.clone();
        let router = router_over_upstream(&config, &upstream);
        let daemon = spawn_daemon(config, dir, router).await;

        // Act: the first turn draws the rejection and the repaired retry.
        let first = post_inference(&daemon, &disabled_body()).await;

        // Assert
        assert_eq!(first, reqwest::StatusCode::OK, "the repaired retry served");
        assert_eq!(
            wire_shapes(&upstream).await,
            vec![
                (json!({"type": "disabled"}), json!("high")),
                (json!({"type": "adaptive"}), json!("high")),
            ],
            "one refused attempt as sent, one adaptive retry keeping the client's effort",
        );
        await_acknowledged_verdict(&daemon, &db_path).await;

        // Act: the second turn, same body.
        let second = post_inference(&daemon, &disabled_body()).await;

        // Assert
        assert_eq!(second, reqwest::StatusCode::OK);
        let shapes = wire_shapes(&upstream).await;
        assert_eq!(
            shapes.len(),
            3,
            "the second turn went out once, drawing no rejection: {shapes:?}",
        );
        assert_eq!(
            shapes[2],
            (json!({"type": "adaptive"}), json!("high")),
            "the second turn left already adaptive, at the client's effort",
        );
        let counters = daemon.live_router().field_repair_counters();
        assert_eq!(counters.repair_succeeded, 1, "only the first turn repaired");
        assert_eq!(counters.preflight_actions, 1, "the second turn preflighted");
        daemon.shutdown().await;
    }
}
