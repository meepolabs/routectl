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

    /// A committed anthropic-api capture's status and verbatim body.
    fn captured_parts(fixture: &str) -> (u16, String) {
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../routectl-router/tests/fixtures/rejections/anthropic-api")
            .join(fixture);
        let fixture: Value =
            serde_json::from_str(&std::fs::read_to_string(fixture).expect("fixture reads"))
                .expect("fixture parses");
        let status = u16::try_from(fixture["status"].as_u64().expect("fixture status"))
            .expect("an HTTP status");
        let body = fixture["body"].as_str().expect("fixture body").to_string();
        (status, body)
    }

    /// A committed anthropic-api capture as a mock response.
    fn captured(fixture: &str) -> ResponseTemplate {
        let (status, body) = captured_parts(fixture);
        ResponseTemplate::new(status).set_body_raw(body, "application/json")
    }

    fn success_body() -> Value {
        json!({
            "id": "msg_chain",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 3, "output_tokens": 1}
        })
    }

    fn messages_success() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(success_body())
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

    /// The same reject, repair and serve over real https: a TLS stub holding
    /// a leaf for the Anthropic host, signed by a throwaway local CA.
    mod https {
        use super::*;

        use std::convert::Infallible;

        use http_body_util::{BodyExt, Full};
        use hyper::body::Incoming;
        use hyper_util::rt::TokioIo;

        const ANTHROPIC_HOST: &str = "api.anthropic.com";

        /// What the stub saw: each decrypted request body in order, and how
        /// many TLS handshakes it refused.
        #[derive(Default)]
        struct StubLog {
            bodies: Mutex<Vec<Value>>,
            handshakes_refused: AtomicUsize,
        }

        /// A TLS stub on loopback answering `/v1/messages` with the committed
        /// max_tokens capture for the refused limit and a success otherwise.
        /// The CA and leaf live in a tempdir owned by the stub.
        struct TlsStub {
            addr: std::net::SocketAddr,
            ca_pem: Vec<u8>,
            log: Arc<StubLog>,
            task: tokio::task::JoinHandle<()>,
            _certs: tempfile::TempDir,
        }

        impl Drop for TlsStub {
            fn drop(&mut self) {
                self.task.abort();
            }
        }

        async fn answer(
            request: hyper::Request<Incoming>,
            log: Arc<StubLog>,
        ) -> hyper::Response<Full<bytes::Bytes>> {
            let bytes = request
                .into_body()
                .collect()
                .await
                .expect("the request body reads")
                .to_bytes();
            let body: Value = serde_json::from_slice(&bytes).expect("request body is JSON");
            let refused = body["max_tokens"] == json!(SENT);
            log.bodies.lock().push(body);
            let (status, payload) = if refused {
                captured_parts("max-tokens-above-model-limit-messages.json")
            } else {
                (200, success_body().to_string())
            };
            hyper::Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(Full::new(bytes::Bytes::from(payload)))
                .expect("a stub response")
        }

        async fn serve_connection(
            acceptor: tokio_rustls::TlsAcceptor,
            stream: tokio::net::TcpStream,
            log: Arc<StubLog>,
        ) {
            let Ok(tls) = acceptor.accept(stream).await else {
                log.handshakes_refused.fetch_add(1, Ordering::SeqCst);
                return;
            };
            let service = hyper::service::service_fn(move |request| {
                let log = Arc::clone(&log);
                async move { Ok::<_, Infallible>(answer(request, log).await) }
            });
            let _ = hyper::server::conn::http1::Builder::new()
                .serve_connection(TokioIo::new(tls), service)
                .await;
        }

        async fn tls_stub() -> TlsStub {
            let certs = tempfile::tempdir().expect("cert tempdir");
            let acceptor = crate::proxy::ca::load_or_create(certs.path(), ANTHROPIC_HOST)
                .expect("the stub CA and leaf mint");
            let ca_pem = std::fs::read(crate::proxy::ca::ca_cert_path(certs.path()))
                .expect("the stub CA reads");
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind the stub");
            let addr = listener.local_addr().expect("stub addr");
            let log = Arc::new(StubLog::default());
            let task_log = Arc::clone(&log);
            let task = tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    tokio::spawn(serve_connection(
                        acceptor.clone(),
                        stream,
                        Arc::clone(&task_log),
                    ));
                }
            });
            TlsStub {
                addr,
                ca_pem,
                log,
                task,
                _certs: certs,
            }
        }

        /// The provider's `https://` base URL: the Anthropic host on the
        /// stub's port.
        fn https_base(stub: &TlsStub) -> String {
            format!("https://{ANTHROPIC_HOST}:{}", stub.addr.port())
        }

        /// A turn carrying only the limit the capture was refused for.
        fn oversized_body() -> Value {
            json!({
                "model": ALIAS,
                "max_tokens": SENT,
                "stream": false,
                "messages": [{"role": "user", "content": "hello"}],
            })
        }

        fn stub_limits(stub: &TlsStub) -> Vec<Value> {
            stub.log
                .bodies
                .lock()
                .iter()
                .map(|body| body["max_tokens"].clone())
                .collect()
        }

        /// A refused client limit is repaired end to end over https: the
        /// stub decrypts two bodies, the refused one as sent and the retry at
        /// the limit the capture names, and the client gets 200.
        ///
        /// Mutation check: drop `add_root_certificate` from the provider's
        /// test client builder -> red (the handshake is refused, no body
        /// reaches the stub, the client gets an error status).
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_refused_limit_is_repaired_and_served_over_https() {
            // Arrange
            let stub = tls_stub().await;
            let (config, dir) = daemon_config();
            let provider = AnthropicApiProvider::new(provider_config(https_base(&stub)))
                .with_host_resolved_to_and_root_ca_for_tests(stub.addr, &stub.ca_pem);
            let router = router_over(&config, provider);
            let daemon = spawn_daemon(config, dir, router).await;

            // Act
            let status = post_inference(&daemon, &oversized_body()).await;

            // Assert
            assert_eq!(status, reqwest::StatusCode::OK, "the repaired retry served");
            assert_eq!(
                stub_limits(&stub),
                vec![json!(SENT), json!(NAMED_LIMIT)],
                "two TLS-served bodies: refused as sent, then retried at the named limit",
            );
            assert_eq!(
                stub.log.handshakes_refused.load(Ordering::SeqCst),
                0,
                "every handshake completed",
            );
            let counters = daemon.live_router().field_repair_counters();
            assert_eq!(counters.repair_succeeded, 1, "the limit was repaired");
            daemon.shutdown().await;
        }

        /// Paired control: the same stub and https base URL without the stub
        /// CA trusted. The stub accepts the connection and the handshake
        /// fails, so no body is decrypted and nothing is repaired, which
        /// proves the transport above is TLS.
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn without_the_stub_ca_the_request_fails_at_tls() {
            // Arrange
            let stub = tls_stub().await;
            let (config, dir) = daemon_config();
            let provider = AnthropicApiProvider::new(provider_config(https_base(&stub)))
                .with_host_resolved_to_for_tests(stub.addr);
            let router = router_over(&config, provider);
            let daemon = spawn_daemon(config, dir, router).await;

            // Act
            let status = post_inference(&daemon, &oversized_body()).await;

            // Assert
            assert_ne!(status, reqwest::StatusCode::OK, "the request failed");
            assert!(
                stub.log.handshakes_refused.load(Ordering::SeqCst) >= 1,
                "the stub was reached and refused the handshake",
            );
            assert!(stub_limits(&stub).is_empty(), "no body was decrypted");
            let counters = daemon.live_router().field_repair_counters();
            assert_eq!(counters.repair_attempted, 0, "nothing was repaired");
            daemon.shutdown().await;
        }
    }
}
