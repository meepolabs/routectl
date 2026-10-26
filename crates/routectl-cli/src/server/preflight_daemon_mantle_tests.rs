// The rows admitted on each Bedrock Mantle sub-lane, over real HTTP on both
// sides: the daemon's ingress in front, a real Mantle egress behind it talking
// to a mock origin that answers with the refusal committed for that sub-lane.
// Each config entry keeps its `bedrock_mantle` region, so attribution reads
// the https host the region derives; only the address the egress dials is the
// mock. `include!`d into preflight_daemon_tests.rs; the fixtures and imports
// live there, and this fragment's own imports stay inside its module.

mod mantle {
    use super::*;

    use routectl_providers::anthropic_api::{
        AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig,
    };
    use routectl_providers::bedrock::auth::ResolvedCreds;
    use routectl_providers::mantle::MantleAuth;
    use routectl_providers::openai_compat::{
        HistoryReasoning, OpenAiCompatConfig, OpenAiCompatProvider, ReasoningDialect,
    };
    use routectl_providers::openai_responses::{
        AuthKind as ResponsesAuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider,
    };
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const REGION: &str = "us-west-2";
    const SEAT: &str = "p0";
    const MANTLE_ALIAS: &str = "mantle-alias";
    const MANTLE_NICKNAME: &str = "mantle-model";

    /// One Mantle sub-lane: its provider kind, the upstream it serves, and the
    /// path its egress dials under the mock origin.
    #[derive(Clone, Copy)]
    enum SubLane {
        Anthropic,
        Compat,
        Responses,
    }

    impl SubLane {
        const fn kind(self) -> &'static str {
            match self {
                Self::Anthropic => "anthropic-api",
                Self::Compat => "openai-compat",
                Self::Responses => "openai-responses",
            }
        }

        const fn upstream(self) -> &'static str {
            match self {
                Self::Anthropic => "anthropic.claude-haiku-4-5",
                Self::Compat => "openai.gpt-5.6-terra",
                Self::Responses => "openai.gpt-5.4",
            }
        }

        const fn wire_path(self) -> &'static str {
            match self {
                Self::Anthropic => "/anthropic/v1/messages",
                Self::Compat => "/openai/v1/chat/completions",
                Self::Responses => "/openai/v1/responses",
            }
        }

        const fn ingress(self) -> &'static str {
            match self {
                Self::Anthropic => "/v1/messages",
                Self::Compat => "/v1/chat/completions",
                Self::Responses => "/v1/responses",
            }
        }

        /// The 2xx the origin answers with once the refused field is gone.
        fn success(self, upstream: &str) -> ResponseTemplate {
            match self {
                Self::Anthropic => ResponseTemplate::new(200).set_body_json(json!({
                    "id": "msg_mantle", "type": "message", "role": "assistant",
                    "model": upstream,
                    "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn",
                    "usage": {"input_tokens": 3, "output_tokens": 1}
                })),
                Self::Compat => ResponseTemplate::new(200).set_body_json(json!({
                    "id": "c1", "object": "chat.completion", "created": 1_700_000_000_i64,
                    "model": upstream, "system_fingerprint": null,
                    "choices": [{"index": 0, "finish_reason": "stop",
                                 "message": {"role": "assistant", "content": "ok"}}],
                    "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
                })),
                Self::Responses => ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(responses_stream(upstream)),
            }
        }
    }

    /// The Responses egress reads every answer as a stream: created, the
    /// output text, completed.
    fn responses_stream(model: &str) -> String {
        let done = json!({
            "id": "resp_1", "object": "response", "created_at": 1_700_000_000_i64,
            "status": "completed", "model": model,
            "output": [{"type": "message", "id": "m1", "role": "assistant",
                        "status": "completed",
                        "content": [{"type": "output_text", "text": "ok", "annotations": []}]}],
            "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}
        });
        let mut created = done.clone();
        created["status"] = json!("in_progress");
        created["output"] = json!([]);
        [
            (
                "response.created",
                json!({"type": "response.created", "response": created}),
            ),
            (
                "response.output_text.delta",
                json!({"type": "response.output_text.delta", "output_index": 0,
                       "content_index": 0, "delta": "ok"}),
            ),
            (
                "response.completed",
                json!({"type": "response.completed", "response": done}),
            ),
        ]
        .iter()
        .map(|(event, data)| format!("event: {event}\ndata: {data}\n\n"))
        .collect()
    }

    /// The committed `rejections/<key>.json` capture: its status and verbatim
    /// body.
    fn captured(key: &str) -> ResponseTemplate {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../routectl-router/tests/fixtures/rejections")
            .join(format!("{key}.json"));
        let fixture: Value =
            serde_json::from_str(&std::fs::read_to_string(fixture).expect("fixture reads"))
                .expect("fixture parses");
        let status = u16::try_from(fixture["status"].as_u64().expect("fixture status"))
            .expect("an HTTP status");
        let body = fixture["body"].as_str().expect("fixture body").to_string();
        ResponseTemplate::new(status).set_body_raw(body, "application/json")
    }

    /// The ledger key of the lane serving `upstream`.
    fn state_key(upstream: &str) -> String {
        format!("{SEAT}#{upstream}")
    }

    /// An origin answering `refusal` to any body matching `refused` (every
    /// body when `None`) and the sub-lane's 2xx, naming `upstream`, to
    /// everything else.
    async fn origin(
        lane: SubLane,
        upstream: &str,
        refused: Option<Value>,
        refusal: ResponseTemplate,
    ) -> MockServer {
        let server = MockServer::start().await;
        let refusing = Mock::given(method("POST")).and(path(lane.wire_path()));
        match refused {
            Some(shape) => refusing.and(body_partial_json(shape)),
            None => refusing,
        }
        .respond_with(refusal)
        .with_priority(1)
        .mount(&server)
        .await;
        Mock::given(method("POST"))
            .and(path(lane.wire_path()))
            .respond_with(lane.success(upstream))
            .mount(&server)
            .await;
        server
    }

    /// A config whose one entry is the sub-lane's Mantle entry in `REGION`,
    /// serving `upstream`.
    fn mantle_config(lane: SubLane, upstream: &str) -> (Arc<Config>, tempfile::TempDir) {
        let key_ref = crate::test_secret::file_ref("mantle-daemon-key");
        let toml_text = format!(
            "\n[providers.{SEAT}]\nkind = \"{}\"\napi_key_ref = \"\"\n\n\
             [providers.{SEAT}.bedrock_mantle]\nregion = \"{REGION}\"\n\
             creds = {{ kind = \"bearer-key\", key_ref = \"{key_ref}\" }}\n",
            lane.kind()
        );
        let mut config: Config = toml::from_str(&toml_text).expect("valid mantle test toml");
        config
            .models
            .insert(MANTLE_NICKNAME.to_string(), ModelEntry::new(SEAT, upstream));
        for alias in [MANTLE_ALIAS, "default"] {
            config.aliases.insert(
                alias.to_string(),
                AliasValue::Single(MANTLE_NICKNAME.to_string()),
            );
        }
        let dir = tempfile::tempdir().expect("usage tempdir");
        config.usage.db_path = dir.path().join("usage.db");
        (Arc::new(config), dir)
    }

    fn mantle_auth() -> MantleAuth {
        MantleAuth {
            region: REGION.into(),
            creds: ResolvedCreds::Bearer {
                key: "mantle-daemon-key".into(),
            },
        }
    }

    /// The compat Mantle egress's config, dialing `base_url`.
    fn compat_config(base_url: String) -> OpenAiCompatConfig {
        OpenAiCompatConfig {
            id: SEAT.into(),
            base_url,
            api_key: String::new(),
            header_extras: Vec::new(),
            payload_extras: None,
            reasoning_dialect: ReasoningDialect::OpenAi,
            history_reasoning: HistoryReasoning::Auto,
            user_agent: None,
            strict_translation: false,
            mantle: Some(mantle_auth()),
        }
    }

    /// The sub-lane's real Mantle egress, dialing the mock origin.
    fn provider(lane: SubLane, origin: &MockServer) -> Arc<dyn Provider> {
        match lane {
            SubLane::Anthropic => anthropic_provider(origin),
            SubLane::Compat => Arc::new(OpenAiCompatProvider::new(compat_config(format!(
                "{}/openai/v1",
                origin.uri()
            )))),
            SubLane::Responses => {
                let mut cfg = OpenAiResponsesConfig::new_with_auth(
                    SEAT,
                    Arc::new(routectl_core::StaticToken::new(String::new())),
                );
                cfg.base_url = format!("{}/openai/v1", origin.uri());
                cfg.auth_kind = ResponsesAuthKind::BedrockMantle;
                cfg.mantle = Some(mantle_auth());
                Arc::new(OpenAiResponsesProvider::new(cfg))
            }
        }
    }

    /// `base`, an `https://<host>/<path>` URL, as plain http to `<host>` on
    /// `port`.
    fn plain_http_on_port(base: &str, port: u16) -> String {
        let mut url = reqwest::Url::parse(base).expect("a base URL");
        assert_eq!(url.scheme(), "https", "an https base");
        url.set_scheme("http").expect("https to http is allowed");
        url.set_port(Some(port)).expect("a host that takes a port");
        url.to_string()
    }

    /// The anthropic Mantle egress dials the Mantle host its region derives,
    /// on the mock's port, and resolves that host to the mock.
    fn anthropic_provider(origin: &MockServer) -> Arc<dyn Provider> {
        let addr = *origin.address();
        let cfg = AnthropicApiConfig {
            id: SEAT.into(),
            auth: Arc::new(routectl_core::StaticToken::new(String::new())),
            base_url: plain_http_on_port(
                &routectl_providers::mantle::mantle_anthropic_base(REGION)
                    .expect("a canonical region"),
                addr.port(),
            ),
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
            mantle: Some(mantle_auth()),
        };
        Arc::new(AnthropicApiProvider::new(cfg).with_host_resolved_to_for_tests(addr))
    }

    fn mantle_router(
        config: &Arc<Config>,
        lane: SubLane,
        upstream: &str,
        origin: &MockServer,
    ) -> Router {
        let mut router = Router::new(Arc::clone(config));
        router.install_resolved_models(BTreeMap::from([(
            MANTLE_NICKNAME.to_string(),
            Arc::new(ResolvedModel::new(
                MANTLE_NICKNAME.to_string(),
                SEAT.to_string(),
                provider(lane, origin),
                upstream.to_string(),
            )),
        )]));
        router
    }

    /// POST `body` at the sub-lane's ingress.
    async fn post(daemon: &Daemon, lane: SubLane, body: &Value) -> reqwest::StatusCode {
        daemon.assert_alive();
        reqwest::Client::new()
            .post(format!("{}{}", daemon.base_url, lane.ingress()))
            .header("anthropic-version", "2023-06-01")
            .json(body)
            .send()
            .await
            .expect("the daemon answers")
            .status()
    }

    /// Every body the origin received, in order.
    async fn wire_bodies(origin: &MockServer) -> Vec<Value> {
        origin
            .received_requests()
            .await
            .expect("request recording is on")
            .iter()
            .map(|request| serde_json::from_slice(&request.body).expect("request body is JSON"))
            .collect()
    }

    /// The persisted value of every ledger row for `path` on the lane serving
    /// `upstream`.
    fn ledger_values(db_path: &std::path::Path, upstream: &str, path: &str) -> Vec<Option<String>> {
        let key = format!("{}{}{path}", "fie", "ld:");
        let Ok(db) = routectl_usage::open_readonly(db_path) else {
            return Vec::new();
        };
        let mut stmt = db
            .conn()
            .prepare(
                "SELECT upstream_token FROM capability_events \
                 WHERE lane_key = ?1 AND capability = ?2 ORDER BY id",
            )
            .expect("prepare");
        stmt.query_map([state_key(upstream).as_str(), key.as_str()], |r| r.get(0))
            .expect("query")
            .collect::<std::result::Result<_, _>>()
            .expect("rows read")
    }

    /// Wait until the verdict on `path` is on the ledger and its confirmation
    /// advancement has finished, so the next request reads the acknowledged
    /// state.
    async fn await_acknowledged(
        daemon: &Daemon,
        db_path: &std::path::Path,
        upstream: &str,
        path: &str,
    ) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while ledger_values(db_path, upstream, path).is_empty()
            || daemon.confirmations().in_flight() > 0
        {
            assert!(
                std::time::Instant::now() < deadline,
                "the committed verdict's row never reached the ledger",
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// One admitted row, replayed: the client body, what the refused attempt
    /// carries, the capture that refuses it, the row, and how to read the
    /// field off a wire body.
    struct Replay {
        lane: SubLane,
        upstream: &'static str,
        body: Value,
        refused: Value,
        capture: &'static str,
        path: &'static str,
        read: fn(&Value) -> Value,
        /// The field as the refused attempt carried it, and as the retry and
        /// the pre-flighted next turn carry it.
        sent: Value,
        repaired: Value,
        /// The value the verdict persists, `None` for a row that persists
        /// none.
        ledger: Option<&'static str>,
    }

    fn replays() -> [Replay; 4] {
        [
            Replay {
                lane: SubLane::Anthropic,
                upstream: SubLane::Anthropic.upstream(),
                body: json!({
                    "model": MANTLE_ALIAS, "max_tokens": 1_000_000, "stream": false,
                    "messages": [{"role": "user", "content": "hi"}],
                }),
                refused: json!({"max_tokens": 1_000_000}),
                capture: "anthropic-api/max-tokens-above-model-limit-mantle",
                path: "max_tokens",
                read: |body| body["max_tokens"].clone(),
                sent: json!(1_000_000),
                repaired: json!(64_000),
                ledger: Some("ceiling:64000"),
            },
            // Not the captured `request_shape`: the mock keys only on `top_p`.
            // The refusal body is replayed from a captured Mantle refusal and
            // names no model. The upstream is an id outside the modern OpenAI
            // chat shape, so the egress still sends `top_p` and the replay
            // pins the repair path, not any one model's behavior.
            Replay {
                lane: SubLane::Compat,
                upstream: "llama-3.3-70b",
                body: json!({
                    "model": MANTLE_ALIAS, "max_tokens": 16, "top_p": 0.5,
                    "messages": [{"role": "user", "content": "hi"}],
                }),
                refused: json!({"top_p": 0.5}),
                capture: "openai-compat/unsupported-parameter-top-p-mantle",
                path: "top_p",
                read: |body| body["top_p"].clone(),
                sent: json!(0.5),
                repaired: Value::Null,
                ledger: None,
            },
            Replay {
                lane: SubLane::Responses,
                upstream: SubLane::Responses.upstream(),
                body: json!({
                    "model": MANTLE_ALIAS, "input": "hi", "max_output_tokens": 16,
                    "reasoning": {"effort": "minimal"},
                }),
                refused: json!({"reasoning": {"effort": "minimal"}}),
                capture: "openai-responses/reasoning-effort-minimal-unsupported-mantle",
                path: "output_config.effort.minimal",
                read: |body| body["reasoning"]["effort"].clone(),
                sent: json!("minimal"),
                repaired: json!("low"),
                ledger: Some("shape:low"),
            },
            Replay {
                lane: SubLane::Responses,
                upstream: SubLane::Responses.upstream(),
                body: json!({
                    "model": MANTLE_ALIAS, "input": "hi", "max_output_tokens": 16,
                    "reasoning": {"effort": "max"},
                }),
                refused: json!({"reasoning": {"effort": "max"}}),
                capture: "openai-responses/reasoning-effort-max-unsupported-mantle",
                path: "output_config.effort.max",
                read: |body| body["reasoning"]["effort"].clone(),
                sent: json!("max"),
                repaired: json!("xhigh"),
                ledger: Some("shape:xhigh"),
            },
        ]
    }

    fn replay_of(path: &str) -> Replay {
        replays()
            .into_iter()
            .find(|replay| replay.path == path)
            .expect("one replay per admitted row")
    }

    /// The captured refusal of an admitted row draws exactly one repaired
    /// retry, the client gets 200, the verdict is minted, and the next turn
    /// leaves already repaired, drawing no refusal.
    async fn replay_and_preflight(replay: Replay) {
        let name = replay.lane.kind();
        // Arrange
        let origin = origin(
            replay.lane,
            replay.upstream,
            Some(replay.refused),
            captured(replay.capture),
        )
        .await;
        let (config, dir) = mantle_config(replay.lane, replay.upstream);
        let db_path = config.usage.db_path.clone();
        let router = mantle_router(&config, replay.lane, replay.upstream, &origin);
        let daemon = spawn_daemon(config, dir, router).await;

        // Act: the first turn draws the refusal and one repaired retry.
        let first = post(&daemon, replay.lane, &replay.body).await;

        // Assert
        assert_eq!(
            first,
            reqwest::StatusCode::OK,
            "{name}: the repaired retry served"
        );
        let fields: Vec<Value> = wire_bodies(&origin).await.iter().map(replay.read).collect();
        assert_eq!(
            fields,
            vec![replay.sent.clone(), replay.repaired.clone()],
            "{name}: one refused attempt as sent, one repaired retry",
        );
        await_acknowledged(&daemon, &db_path, replay.upstream, replay.path).await;
        let values = ledger_values(&db_path, replay.upstream, replay.path);
        assert_eq!(
            values.iter().map(Option::as_deref).collect::<Vec<_>>(),
            vec![replay.ledger],
            "{name}: one verdict row on the ledger, with the value it persists",
        );

        // Act: the second turn, same body.
        let second = post(&daemon, replay.lane, &replay.body).await;

        // Assert
        assert_eq!(second, reqwest::StatusCode::OK, "{name}");
        let fields: Vec<Value> = wire_bodies(&origin).await.iter().map(replay.read).collect();
        assert_eq!(
            fields,
            vec![
                replay.sent.clone(),
                replay.repaired.clone(),
                replay.repaired.clone()
            ],
            "{name}: the second turn went out once, already repaired",
        );
        let counters = daemon.live_router().field_repair_counters();
        assert_eq!(
            counters.repair_attempted, 1,
            "{name}: one repair, on the first turn"
        );
        assert_eq!(counters.repair_succeeded, 1, "{name}");
        assert_eq!(
            counters.preflight_actions, 1,
            "{name}: the second turn preflighted"
        );
        daemon.shutdown().await;
    }

    /// The anthropic Mantle ceiling row.
    ///
    /// Mutation check: empty `ANTHROPIC_API_MANTLE_ROWS` -> red (the refusal
    /// reaches the client).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn anthropic_mantle_repairs_its_captured_ceiling_and_preflights_the_next_turn() {
        replay_and_preflight(replay_of("max_tokens")).await;
    }

    /// The compat Mantle drop row.
    ///
    /// Mutation check: empty `OPENAI_COMPAT_MANTLE_ROWS` -> red (the refusal
    /// reaches the client).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn compat_mantle_drops_its_captured_parameter_and_preflights_the_next_turn() {
        let replay = replay_of("top_p");
        let sent = OpenAiCompatProvider::new(compat_config(
            "https://bedrock-mantle.invalid/openai/v1".into(),
        ))
        .normalize_request(&routectl_core::ChatRequest {
            model: replay.upstream.into(),
            top_p: Some(0.5),
            ..Default::default()
        })
        .expect("the egress builds a body");
        assert_eq!(
            sent["top_p"],
            json!(0.5),
            "premise: the compat egress sends top_p for {}; the replayed refusal \
             is drawn only on an id outside the modern chat shape",
            replay.upstream,
        );
        replay_and_preflight(replay).await;
    }

    /// A compat Mantle reasoning model refuses `max_tokens` before any other
    /// field. A client sending an output limit and `top_p` is served on the
    /// first attempt: the egress sends the limit as `max_completion_tokens`
    /// and drops `top_p`, so no refusal is drawn and nothing is learned.
    ///
    /// Mutation check: stop `uses_modern_chat_shape` stripping the `openai.` vendor
    /// segment -> red (the refusal reaches the client).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn compat_mantle_reasoning_model_sends_its_output_limit_renamed_on_the_first_attempt() {
        let lane = SubLane::Compat;
        let upstream = lane.upstream();
        // Arrange
        let origin = origin(
            lane,
            upstream,
            Some(json!({"max_tokens": 16})),
            captured("openai-compat/unsupported-parameter-max-tokens-mantle"),
        )
        .await;
        let (config, dir) = mantle_config(lane, upstream);
        let db_path = config.usage.db_path.clone();
        let router = mantle_router(&config, lane, upstream, &origin);
        let daemon = spawn_daemon(config, dir, router).await;
        let body = json!({
            "model": MANTLE_ALIAS, "max_completion_tokens": 16, "top_p": 0.5,
            "messages": [{"role": "user", "content": "hi"}],
        });

        // Act
        let status = post(&daemon, lane, &body).await;

        // Assert
        assert_eq!(
            status,
            reqwest::StatusCode::OK,
            "served on the first attempt"
        );
        let wire = wire_bodies(&origin).await;
        assert_eq!(wire.len(), 1, "one dial, no retry: {wire:?}");
        assert_eq!(wire[0]["max_completion_tokens"], json!(16), "{}", wire[0]);
        assert!(wire[0].get("max_tokens").is_none(), "{}", wire[0]);
        assert!(wire[0].get("top_p").is_none(), "{}", wire[0]);
        let counters = daemon.live_router().field_repair_counters();
        assert_eq!(counters.repair_attempted, 0, "nothing repaired");
        assert_eq!(counters.preflight_actions, 0, "nothing preflighted");
        for path in ["max_tokens", "top_p"] {
            assert!(
                ledger_values(&db_path, upstream, path).is_empty(),
                "no verdict on {path}"
            );
        }
        daemon.shutdown().await;
    }

    /// The Responses Mantle ranked row that picks a higher level.
    ///
    /// Mutation check: empty `OPENAI_RESPONSES_MANTLE_ROWS` -> red (the
    /// refusal reaches the client).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn responses_mantle_lowers_its_captured_effort_and_preflights_the_next_turn() {
        replay_and_preflight(replay_of("output_config.effort.minimal")).await;
    }

    /// The Responses Mantle ranked row that picks a lower level: the refusal
    /// lists the model's levels, and the nearest one below the refused level
    /// is sent.
    ///
    /// Mutation check: drop `"output_config.effort.max"` from
    /// `OPENAI_RESPONSES_MANTLE_ROWS` -> red (the refusal reaches the client).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn responses_mantle_lowers_its_captured_max_effort_and_preflights_the_next_turn() {
        replay_and_preflight(replay_of("output_config.effort.max")).await;
    }

    /// Each sub-lane's pass-through control: a refusal captured on that
    /// sub-lane that names no field is forwarded to the client unrepaired,
    /// once, and counted as one the parser did not localize.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn each_mantle_control_capture_is_forwarded_unrepaired_and_counted_unlocalized() {
        let controls = [
            (
                SubLane::Anthropic,
                "anthropic-api/effort-parameter-unsupported-control-mantle",
                json!({
                    "model": MANTLE_ALIAS, "max_tokens": 16, "stream": false,
                    "output_config": {"effort": "high"},
                    "messages": [{"role": "user", "content": "hi"}],
                }),
            ),
            (
                SubLane::Compat,
                "openai-compat/model-unsupported-on-route-control-mantle",
                json!({
                    "model": MANTLE_ALIAS, "max_tokens": 16, "top_p": 0.5,
                    "messages": [{"role": "user", "content": "hi"}],
                }),
            ),
            (
                SubLane::Responses,
                "openai-responses/model-unsupported-api-control-mantle",
                json!({
                    "model": MANTLE_ALIAS, "input": "hi", "max_output_tokens": 16,
                    "reasoning": {"effort": "minimal"},
                }),
            ),
        ];
        for (lane, capture, body) in controls {
            let name = lane.kind();
            // Arrange
            let origin = origin(lane, lane.upstream(), None, captured(capture)).await;
            let (config, dir) = mantle_config(lane, lane.upstream());
            let router = mantle_router(&config, lane, lane.upstream(), &origin);
            let daemon = spawn_daemon(config, dir, router).await;

            // Act
            let status = post(&daemon, lane, &body).await;

            // Assert
            assert_eq!(
                status,
                reqwest::StatusCode::BAD_REQUEST,
                "{name}: forwarded"
            );
            assert_eq!(
                wire_bodies(&origin).await.len(),
                1,
                "{name}: sent once, no retry"
            );
            let counters = daemon.live_router().field_repair_counters();
            assert_eq!(counters.repair_attempted, 0, "{name}: nothing repaired");
            assert_eq!(
                counters.parser_unlocalized, 1,
                "{name}: counted unlocalized"
            );
            daemon.shutdown().await;
        }
    }
}
