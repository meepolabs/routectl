// Two refused fields in one client request over real HTTP on both sides:
// the daemon's Anthropic ingress in front, a real anthropic-api egress behind
// it talking to a mock upstream that answers with the committed captures of
// the disabled-thinking refusal and the max_tokens refusal, in that order.
// `include!`d into preflight_daemon_tests.rs; the fixtures and imports live
// there, and this fragment's own imports stay inside its module.

mod chain {
    use super::*;

    use routectl_providers::anthropic_api::{
        AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig,
    };
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const DISABLED_PATH: &str = "thinking.type.disabled";
    const MAX_TOKENS_PATH: &str = "max_tokens";
    /// The value the committed max_tokens capture was refused for.
    const SENT: u64 = 1_000_000;
    /// The limit the committed max_tokens capture names.
    const NAMED_LIMIT: u64 = 128_000;

    /// A committed anthropic-api capture: its status and verbatim body.
    fn captured(fixture: &str) -> ResponseTemplate {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../routectl-router/tests/fixtures/rejections/anthropic-api")
            .join(fixture);
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
            "id": "msg_chain",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 1}
        }))
    }

    /// A mock upstream that refuses disabled thinking first, then a
    /// max_tokens above the model's limit, and serves everything else.
    async fn two_key_upstream() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_partial_json(json!({"thinking": {"type": "disabled"}})))
            .respond_with(captured("thinking-type-disabled-unsupported.json"))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(body_partial_json(json!({"max_tokens": SENT})))
            .respond_with(captured("max-tokens-above-model-limit-messages.json"))
            .with_priority(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(messages_success())
            .mount(&server)
            .await;
        server
    }

    fn provider_config(base_url: String) -> AnthropicApiConfig {
        AnthropicApiConfig {
            id: "p0".into(),
            auth: Arc::new(routectl_core::StaticToken::new("chain-key")),
            base_url,
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
        }
    }

    fn router_over(config: &Arc<Config>, provider: AnthropicApiProvider) -> Router {
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

    /// A Router over `config` whose `p0` is a real anthropic-api provider.
    ///
    /// The config entry keeps its remote https base URL, which is what the
    /// attributability gate reads; the provider dials the Anthropic host on
    /// the mock's port and resolves that host to the mock, so only the address
    /// the egress connects to is substituted.
    fn router_over_upstream(config: &Arc<Config>, upstream: &MockServer) -> Router {
        let addr = *upstream.address();
        let cfg = provider_config(format!("http://api.anthropic.com:{}", addr.port()));
        router_over(
            config,
            AnthropicApiProvider::new(cfg).with_host_resolved_to_for_tests(addr),
        )
    }

    /// An Anthropic client's plain turn with thinking off and an explicit
    /// limit above the model's.
    fn two_key_body() -> Value {
        json!({
            "model": ALIAS,
            "max_tokens": SENT,
            "stream": false,
            "thinking": {"type": "disabled"},
            "messages": [{"role": "user", "content": "hello"}],
        })
    }

    /// `(thinking.type, max_tokens)` of every body the upstream received.
    async fn wire_shapes(upstream: &MockServer) -> Vec<(Value, Value)> {
        upstream
            .received_requests()
            .await
            .expect("request recording is on")
            .iter()
            .map(|request| {
                let body: Value =
                    serde_json::from_slice(&request.body).expect("request body is JSON");
                (body["thinking"]["type"].clone(), body["max_tokens"].clone())
            })
            .collect()
    }

    fn key(path: &str) -> String {
        format!("{}{path}", format_args!("{}{}", "fie", "ld:"))
    }

    /// How many ledger rows the daemon wrote for the verdict at `path`.
    fn ledger_rows(db_path: &std::path::Path, path: &str) -> usize {
        let db = routectl_usage::open_readonly(db_path).expect("read-only open");
        let mut stmt = db
            .conn()
            .prepare(
                "SELECT COUNT(*) FROM capability_events WHERE lane_key = ?1 AND capability = ?2",
            )
            .expect("prepare");
        let count: i64 = stmt
            .query_row([STATE_KEY, key(path).as_str()], |r| r.get(0))
            .expect("count");
        usize::try_from(count).expect("a non-negative count")
    }

    /// Wait until both verdicts' rows are on the daemon's ledger and every
    /// confirmation advancement has finished.
    async fn await_both_rows(daemon: &Daemon, db_path: &std::path::Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while ledger_rows(db_path, DISABLED_PATH) == 0
            || ledger_rows(db_path, MAX_TOKENS_PATH) == 0
            || daemon.confirmations().in_flight() > 0
        {
            assert!(
                std::time::Instant::now() < deadline,
                "both verdicts' rows never reached the ledger",
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Two refused fields converge in ONE client request: the disabled
    /// thinking is rewritten, the retry is refused for max_tokens (which the
    /// first body also carried), the max_tokens refusal commits the thinking
    /// step, and the second retry at the named limit serves and commits the
    /// ceiling. Both verdicts reach the daemon's ledger.
    ///
    /// Mutation check: refuse a second field repair on a target that already
    /// applied one (in `FieldRepairPlan::apply_named`) -> red (two upstream
    /// calls, the max_tokens 400 reaches the client).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_refused_fields_converge_in_one_daemon_request_with_two_ledger_rows() {
        // Arrange
        let upstream = two_key_upstream().await;
        let (config, dir) = daemon_config();
        let db_path = config.usage.db_path.clone();
        let router = router_over_upstream(&config, &upstream);
        let daemon = spawn_daemon(config, dir, router).await;

        // Act
        let status = post_inference(&daemon, &two_key_body()).await;

        // Assert
        assert_eq!(
            status,
            reqwest::StatusCode::OK,
            "the twice-repaired retry served"
        );
        assert_eq!(
            wire_shapes(&upstream).await,
            vec![
                (json!("disabled"), json!(SENT)),
                (json!("adaptive"), json!(SENT)),
                (json!("adaptive"), json!(NAMED_LIMIT)),
            ],
            "refused as sent, refused again for the field the first repair left \
             alone, then served with both edits on the body",
        );
        await_both_rows(&daemon, &db_path).await;
        assert_eq!(
            (
                ledger_rows(&db_path, DISABLED_PATH),
                ledger_rows(&db_path, MAX_TOKENS_PATH),
            ),
            (1, 1),
            "one acknowledged row per field",
        );
        let counters = daemon.live_router().field_repair_counters();
        assert_eq!(counters.repair_succeeded, 2, "both repairs succeeded");
        assert_eq!(counters.verdicts_learned, 2, "and each committed a verdict");
        daemon.shutdown().await;
    }

    /// Positive control for the row above: the same daemon and request on a
    /// LOOPBACK base URL repairs nothing, because a rejection from a local
    /// hop is not attributable, and the client gets the upstream's 400.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_loopback_lane_repairs_neither_field_and_returns_the_refusal() {
        // Arrange
        let upstream = two_key_upstream().await;
        let (config, dir) = daemon_config_on(&upstream.uri());
        let router = router_over(
            &config,
            AnthropicApiProvider::new(provider_config(upstream.uri())),
        );
        let daemon = spawn_daemon(config, dir, router).await;

        // Act
        let status = post_inference(&daemon, &two_key_body()).await;

        // Assert
        assert_eq!(
            status,
            reqwest::StatusCode::BAD_REQUEST,
            "the refusal reaches the client"
        );
        assert_eq!(
            wire_shapes(&upstream).await,
            vec![(json!("disabled"), json!(SENT))],
            "one attempt as sent, no repaired retry",
        );
        let counters = daemon.live_router().field_repair_counters();
        assert_eq!(counters.repair_attempted, 0, "nothing was repaired");
        assert_eq!(counters.verdicts_learned, 0, "and nothing learned");
        daemon.shutdown().await;
    }

    /// Hold the daemon's ledger write lock and fill the writer's channel with
    /// acknowledged batches until one is refused, so every capability write
    /// admitted while the returned connection is open is refused at
    /// admission. The writer is stuck on its first batch behind the lock, so
    /// nothing drains.
    fn block_capability_writes(
        daemon: &Daemon,
        db_path: &std::path::Path,
    ) -> (rusqlite::Connection, Vec<routectl_usage::BatchReceipt>) {
        let lock = rusqlite::Connection::open(db_path).expect("open the ledger");
        lock.busy_timeout(Duration::from_secs(5))
            .expect("busy timeout");
        lock.execute_batch("BEGIN EXCLUSIVE")
            .expect("take the write lock");
        let mut held = Vec::new();
        for _ in 0..(routectl_usage::CHANNEL_CAPACITY * 2 + 8) {
            match daemon.usage().admit_capability_batch(
                vec![routectl_usage::CapabilityEvent::tombstone(900, 8, 1)],
                1,
            ) {
                Ok(receipt) => held.push(receipt),
                Err(_) => break,
            }
        }
        (lock, held)
    }

    /// `(field_path, learned)` of every field-repair WARN, in order.
    fn repair_warns(events: &[routectl_testkit::CapturedEvent]) -> Vec<(String, String)> {
        events
            .iter()
            .filter(|e| e.level == tracing::Level::WARN && e.message == "envelope_field_repaired")
            .map(|e| {
                (
                    e.field("field_path").expect("a field path").to_string(),
                    e.field("learned").expect("a learned flag").to_string(),
                )
            })
            .collect()
    }

    /// A refused capability write never fails the user request: the request
    /// converges and serves, the refusal is on the doctor's
    /// `capability_writes`, no ledger row lands, no verdict counts a durable
    /// confirmation, and each repair WARN's `learned` is exactly whether its
    /// commit returned a verdict, which the live registry holds.
    ///
    /// On the current-thread runtime so the capture subscriber sees the
    /// daemon's handler, which runs on the test's own thread.
    ///
    /// Mutation checks, each red here:
    ///   - report zero full-channel refusals from
    ///     `UsageHealthView::capability_writes` -> the doctor shows none;
    ///   - pass `false` instead of the persisted flag to
    ///     `note_field_repair_succeeded` in `commit_field_steps` -> the WARNs
    ///     say nothing was learned while the registry holds both verdicts.
    #[tokio::test]
    async fn a_refused_verdict_write_still_serves_and_claims_no_persistence() {
        // Arrange
        let upstream = two_key_upstream().await;
        let (config, dir) = daemon_config();
        let db_path = config.usage.db_path.clone();
        let config_dir = tempfile::tempdir().expect("config tempdir");
        let config_path = write_config_file(config_dir.path());
        let router = router_over_upstream(&config, &upstream);
        let daemon = spawn_daemon_full(
            config,
            dir,
            router,
            crate::handlers::status::test_hooks::StatusTestHooks::default(),
            Some(config_path),
        )
        .await;
        let (lock, held) = block_capability_writes(&daemon, &db_path);
        let refused_before = daemon.usage().counters().capability_events_dropped_full();
        assert!(refused_before >= 1, "premise: the channel refused a batch");

        // Act
        let (status, events) =
            routectl_testkit::with_capture(post_inference(&daemon, &two_key_body())).await;
        let refused = daemon.usage().counters().capability_events_dropped_full() - refused_before;
        lock.execute_batch("ROLLBACK")
            .expect("release the write lock");
        drop(lock);
        drop(held);

        // Assert
        assert_eq!(
            status,
            reqwest::StatusCode::OK,
            "the write failure does not fail the request"
        );
        assert_eq!(
            wire_shapes(&upstream).await.len(),
            3,
            "premise: both fields repaired"
        );
        assert_eq!(
            refused, 2,
            "each verdict's acknowledged write was refused at admission"
        );
        let doctor = get_status(&daemon, "/status/doctor").await;
        let writes = &available_data(&doctor)["capability_writes"];
        assert!(
            writes["capability_events_dropped_full"].as_u64() >= Some(refused_before + refused),
            "the doctor reports the refused writes: {writes}",
        );
        assert_eq!(
            (
                ledger_rows(&db_path, DISABLED_PATH),
                ledger_rows(&db_path, MAX_TOKENS_PATH),
            ),
            (0, 0),
            "no verdict row landed",
        );
        let live = daemon.live_router();
        let resident: Vec<String> = live
            .learned_capability_snapshot()
            .into_iter()
            .filter(|entry| entry.state_key == STATE_KEY)
            .map(|entry| entry.feature_key)
            .collect();
        assert!(
            resident.contains(&key(DISABLED_PATH)) && resident.contains(&key(MAX_TOKENS_PATH)),
            "premise: both commits returned a verdict the registry holds: {resident:?}",
        );
        let committed = |path: &str| resident.contains(&key(path)).to_string();
        assert_eq!(
            repair_warns(&events),
            vec![
                (DISABLED_PATH.to_string(), committed(DISABLED_PATH)),
                (MAX_TOKENS_PATH.to_string(), committed(MAX_TOKENS_PATH)),
            ],
            "each WARN's learned is whether its commit left a verdict: {resident:?}",
        );
        assert!(
            live.fidelity_snapshot()
                .verdicts
                .iter()
                .filter(|row| row.state_key == STATE_KEY)
                .all(|row| row.confirmations == 0),
            "no verdict counts a durable confirmation",
        );
        assert_eq!(
            daemon.confirmations().in_flight(),
            0,
            "and none is pending one"
        );
        daemon.shutdown().await;
    }
}
