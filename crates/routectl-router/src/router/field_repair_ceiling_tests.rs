// The max_tokens ceiling row: when the surface lowers a token limit, where a
// fidelity lane declares the edit, and the row end to end on a Bedrock
// Converse seat that assembles the real Converse body, so every assertion
// about the repair reads the body an upstream would receive.
// An `include!`d FRAGMENT of `field_repair_tests.rs`.

#[test]
fn a_max_tokens_clamp_lowers_only_a_value_above_the_ceiling() {
    let surface = super::FieldSurface::MaxTokens;
    let rows = [
        ("unset", None, 8192, false, None),
        ("above", Some(64_000), 8192, true, Some(8192)),
        ("equal", Some(8192), 8192, false, Some(8192)),
        ("below", Some(4096), 8192, false, Some(4096)),
        ("zero ceiling", Some(64_000), 0, false, Some(64_000)),
    ];

    for (name, sent, ceiling, changed, after) in rows {
        let mut req = req_on(ALIAS);
        req.max_tokens = sent;
        assert_eq!(surface.present_in(&req), sent.is_some(), "{name}");

        assert_eq!(surface.clamp_to(&mut req, ceiling), changed, "{name}");

        assert_eq!(req.max_tokens, after, "{name}");
        assert!(!surface.drop_from(&mut req), "{name}: never dropped");
    }
}

#[test]
fn a_max_tokens_clamp_declares_a_rewrite_of_its_root_pointer_on_a_root_spelling_wire() {
    use routectl_core::wire_check::IngressDialect;
    let surface = super::FieldSurface::MaxTokens;
    let rows = [
        (IngressDialect::Anthropic, Some("/max_tokens")),
        (IngressDialect::Chat, Some("/max_tokens")),
        (IngressDialect::Responses, None),
    ];

    for (wire, pointer) in rows {
        assert_eq!(
            super::super::edit_declare::field_wire_pointer(surface, wire),
            pointer,
            "{wire:?}",
        );
    }
    assert_eq!(
        super::FieldEdit::Ceiling(8192).declared_kind(),
        routectl_core::wire_check::EditKind::Rewrite,
    );
}

/// An openai-compat seat that refuses every request carrying `max_tokens`
/// with the structured unsupported-parameter envelope, and serves the rest.
struct UnsupportedMaxTokensSeat {
    calls: AtomicUsize,
}

impl UnsupportedMaxTokensSeat {
    fn answer(&self, req: &ChatRequest) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if req.max_tokens.is_some() {
            return Err(unsupported_max_tokens());
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Provider for UnsupportedMaxTokensSeat {
    fn id(&self) -> &'static str {
        "unsupported-max-tokens"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response(
            "unsupported-max-tokens",
            "unused",
        ))
    }
    async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
        self.answer(&req).map(|()| success_response())
    }
    async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
        self.answer(&req).map(|()| {
            stream::iter(vec![Ok(content_chunk())]).boxed() as BoxStream<'static, Result<ChatChunk>>
        })
    }
}

fn unsupported_max_tokens() -> Error {
    Error::upstream_with_body(
        "unsupported-max-tokens",
        400,
        routectl_core::UpstreamBody::new(
            json!({"error": {
                "message": "Unsupported parameter: 'max_tokens' is not supported with this model.",
                "type": "invalid_request_error",
                "param": "max_tokens",
                "code": OPENAI_UNSUPPORTED_CODE,
            }})
            .to_string(),
        ),
        None,
        Some("invalid_request_error".to_string()),
        Some(OPENAI_UNSUPPORTED_CODE.to_string()),
    )
}

/// A request carrying `max_tokens` and no other closed-table row.
fn max_tokens_only_req(max_tokens: u32) -> ChatRequest {
    let mut req = req_on(ALIAS);
    req.max_tokens = Some(max_tokens);
    req.reasoning = None;
    req.routectl_internal.anthropic_thinking_display = None;
    req.routectl_internal.anthropic_thinking = routectl_core::AnthropicThinking::SentNone;
    req
}

/// Apply `err` through the real admitted plan for `req`'s first target and
/// return whether it repaired, the request after, and how much of the
/// per-request allowance is left.
fn apply_on_first_target(
    router: &Router,
    req: &ChatRequest,
    err: &Error,
    provider_kind: &str,
) -> (bool, ChatRequest, usize) {
    let target = router
        .dispatch_chain_for_request(req, crate::router::class_observe::DispatchSurface::Complete)
        .expect("the fixture chain resolves")
        .0
        .into_iter()
        .next()
        .expect("one target");
    let mut plan = router
        .plan_field_carry(
            &target,
            req,
            super::FieldSettlementMode::Settling,
            std::time::Instant::now(),
        )
        .expect("a carried max_tokens admits a plan");
    let mut budget = crate::router::repair_budget::RepairBudget::per_request();
    let mut meta = crate::router::DispatchMeta::for_alias(ALIAS);
    let mut attempt = req.clone();
    let class = routectl_core::failure_class::classify(err, Some(provider_kind)).class;

    let applied = plan.apply(
        &mut attempt,
        &mut meta,
        &mut budget,
        &class,
        err,
        provider_kind,
    );

    let left = (0..=usize::from(REPAIRS_PER_REQUEST))
        .filter(|_| budget.draw())
        .count();
    (applied.is_some(), attempt, left)
}

#[tokio::test]
async fn an_unsupported_max_tokens_refusal_resolves_the_row_and_makes_no_edit() {
    let mut config: Config =
        toml::from_str(&format!("\n[providers.p0]\n{OPENAI_COMPAT_TOML}")).expect("valid toml");
    config
        .aliases
        .insert(ALIAS.to_string(), AliasValue::Chain(vec!["m0".to_string()]));
    let mut router = Router::new(Arc::new(config));
    let seat = Arc::new(UnsupportedMaxTokensSeat {
        calls: AtomicUsize::new(0),
    });
    router.install_resolved_models(BTreeMap::from([(
        "m0".to_string(),
        Arc::new(ResolvedModel::new(
            "m0",
            "p0",
            Arc::clone(&seat) as Arc<dyn Provider>,
            "wire-0",
        )),
    )]));
    let router = router.with_capability_writes_assumed_durable_for_tests();
    let req = max_tokens_only_req(1000);
    let err = unsupported_max_tokens();
    let class = routectl_core::failure_class::classify(&err, Some("openai-compat")).class;
    let named = super::rejected_field(router.field_repair_table(), &class, &err, "openai-compat")
        .expect("premise: the refusal names a row");
    assert_eq!(named.row.path, "max_tokens", "premise");
    assert_eq!(named.value, None, "premise: the refusal names no number");

    let (repaired, attempt, left) = apply_on_first_target(&router, &req, &err, "openai-compat");
    let dispatched = router
        .complete_with_options(req.clone(), RouterOptions::new())
        .await;

    assert!(!repaired, "a valueless resolution makes no edit");
    assert_eq!(attempt.max_tokens, Some(1000));
    assert_eq!(left, usize::from(REPAIRS_PER_REQUEST), "no budget drawn");
    assert!(dispatched.result.is_err(), "the refusal reaches the client");
    assert_eq!(seat.calls.load(Ordering::SeqCst), 1, "no retry");
    assert!(dispatched.meta.field_repairs.is_empty());
    assert!(
        dispatched.meta.learned_capabilities.is_empty(),
        "learns nothing"
    );
    assert_eq!(router.field_verdicts().snapshot_len(), 0);
    assert!(router.learned_capability_snapshot().is_empty());
}

#[cfg(feature = "bedrock")]
mod converse_ceiling {
    use super::*;

    use crate::wire_check_state::FieldRepairOutcome;
    use routectl_providers::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};

    /// The committed Converse capture of a model refusing a token limit above
    /// its own, which it names as 8192.
    const CEILING_FIXTURE: &str =
        include_str!("../../tests/fixtures/rejections/bedrock/max-tokens-above-model-limit.json");
    const NAMED_LIMIT: u32 = 8192;
    const CLIENT_MAX_TOKENS: u32 = 64_000;

    const CONVERSE_TOML: &str = "[wire_check]\nfirst_n = 8\nsample_one_in = 1\n\
         [retry]\nmax_attempts = 1\ninitial_backoff_ms = 0\njitter_ms = 0\n\n\
         [providers.p0]\nkind = \"bedrock\"\nregion = \"us-east-1\"\n\
         creds = { kind = \"default-chain\" }\napi_shape = \"converse\"\n";

    fn ceiling_rejection() -> Error {
        let fixture: serde_json::Value =
            serde_json::from_str(CEILING_FIXTURE).expect("the fixture is JSON");
        let status = u16::try_from(fixture["status"].as_u64().expect("a status")).expect("u16");
        Error::upstream_with_body(
            "converse-seat",
            status,
            routectl_core::UpstreamBody::new(
                fixture["body"]
                    .as_str()
                    .expect("the fixture carries its body"),
            ),
            None,
            None,
            None,
        )
    }

    /// How a seat answers each attempt.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Seat {
        /// Refuse a limit above [`NAMED_LIMIT`], serve the rest.
        CapsThenServes,
        /// Refuse a limit above [`NAMED_LIMIT`], 503 the rest.
        CapsThenUnavailable,
        /// Refuse every attempt with the capture, whatever it sent.
        AlwaysRefuses,
        /// Serve everything.
        ServesAll,
        /// Stream content, then end on the capture, while above the limit.
        StreamsThenRefuses,
    }

    struct ConverseSeat {
        cfg: BedrockConfig,
        seat: Seat,
        bodies: Mutex<Vec<serde_json::Value>>,
    }

    impl ConverseSeat {
        fn new(seat: Seat) -> Arc<Self> {
            Arc::new(Self {
                cfg: BedrockConfig {
                    id: "bedrock:p0".into(),
                    region: "us-east-1".into(),
                    model_id: "us.zz.output-capped".into(),
                    api_shape: BedrockApiShape::Converse,
                    creds: BedrockCreds::BearerKey {
                        key: ["converse", "seat", "test"].join("-"),
                    },
                    user_agent: None,
                    header_extras: Vec::new(),
                    anthropic_beta: Vec::new(),
                    additional_model_request_fields: None,
                    adaptive_thinking: None,
                },
                seat,
                bodies: Mutex::new(Vec::new()),
            })
        }

        fn bodies(&self) -> Vec<serde_json::Value> {
            self.bodies.lock().clone()
        }

        fn sent_limits(&self) -> Vec<serde_json::Value> {
            self.bodies()
                .iter()
                .map(|body| body["inferenceConfig"]["maxTokens"].clone())
                .collect()
        }

        /// Assemble and record the body; `true` when it is above the limit.
        fn record(&self, req: &ChatRequest) -> Result<bool> {
            let body = routectl_providers::bedrock::converse::normalize_request(&self.cfg, req)?;
            if let Some(tap) = req.routectl_internal.wire_tap.as_ref() {
                tap.deposit(serde_json::to_vec(&body).expect("serialize").into());
            }
            let above = body["inferenceConfig"]["maxTokens"]
                .as_u64()
                .is_some_and(|sent| sent > u64::from(NAMED_LIMIT));
            self.bodies.lock().push(body);
            Ok(above)
        }

        fn answer(&self, req: &ChatRequest) -> Result<()> {
            let above = self.record(req)?;
            match self.seat {
                Seat::AlwaysRefuses => Err(ceiling_rejection()),
                Seat::CapsThenServes | Seat::CapsThenUnavailable if above => {
                    Err(ceiling_rejection())
                }
                Seat::CapsThenUnavailable => Err(Error::upstream(
                    "converse-seat",
                    503,
                    r#"{"message":"unavailable"}"#,
                )),
                Seat::CapsThenServes | Seat::ServesAll | Seat::StreamsThenRefuses => Ok(()),
            }
        }
    }

    #[async_trait::async_trait]
    impl Provider for ConverseSeat {
        fn id(&self) -> &'static str {
            "converse-seat"
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response("converse-seat", "unused"))
        }
        async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
            self.answer(&req).map(|()| success_response())
        }
        async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            if self.seat == Seat::StreamsThenRefuses && self.record(&req)? {
                return Ok(
                    stream::iter(vec![Ok(content_chunk()), Err(ceiling_rejection())]).boxed(),
                );
            }
            self.answer(&req).map(|()| {
                stream::iter(vec![Ok(content_chunk())]).boxed()
                    as BoxStream<'static, Result<ChatChunk>>
            })
        }
    }

    /// A router whose alias walks one model per seat, in order.
    fn converse_router(seats: &[&Arc<ConverseSeat>]) -> Router {
        let mut config: Config = toml::from_str(CONVERSE_TOML).expect("valid test toml");
        let names: Vec<String> = (0..seats.len()).map(|i| format!("m{i}")).collect();
        config
            .aliases
            .insert(ALIAS.to_string(), AliasValue::Chain(names.clone()));
        let mut router = Router::new(Arc::new(config));
        router.install_resolved_models(
            names
                .iter()
                .zip(seats)
                .enumerate()
                .map(|(i, (name, seat))| {
                    (
                        name.clone(),
                        Arc::new(ResolvedModel::new(
                            name,
                            "p0",
                            Arc::clone(seat) as Arc<dyn Provider>,
                            format!("wire-{i}"),
                        )),
                    )
                })
                .collect(),
        );
        router.with_capability_writes_assumed_durable_for_tests()
    }

    /// An Anthropic client's `max_tokens` with no thinking and no sampling,
    /// as the ingress parses it, client bytes included for the wire check.
    fn client_req(max_tokens: u32) -> ChatRequest {
        let mut req = max_tokens_only_req(max_tokens);
        req.routectl_internal.ingress_wire = Some(routectl_core::wire_check::IngressWire {
            dialect: routectl_core::wire_check::IngressDialect::Anthropic,
            client_version: None,
            bytes: serde_json::to_vec(&json!({
                "model": ALIAS,
                "max_tokens": max_tokens,
                "messages": [{"role": "user", "content": "hello"}],
            }))
            .expect("serialize")
            .into(),
        });
        req
    }

    fn rejection_outcomes(router: &Router) -> Vec<FieldRepairOutcome> {
        router
            .wire_check_snapshot()
            .findings
            .iter()
            .filter_map(|finding| finding.field_repair_outcome)
            .collect()
    }

    /// Dispatch on `stream` or complete; the request's metadata and whether
    /// it was served with no error.
    async fn dispatch(
        router: &Router,
        stream: bool,
        req: ChatRequest,
    ) -> (crate::router::DispatchMeta, bool) {
        if stream {
            let opened = router.stream_with_options(req, RouterOptions::new()).await;
            let served = match opened.result {
                Ok(body) => {
                    let frames: Vec<Result<ChatChunk>> = body.collect().await;
                    frames.iter().all(std::result::Result::is_ok)
                }
                Err(_) => false,
            };
            (opened.meta, served)
        } else {
            let done = router
                .complete_with_options(req, RouterOptions::new())
                .await;
            (done.meta, done.result.is_ok())
        }
    }

    /// Mutation checks: drop the number capture from the Converse ceiling arm
    /// -> red (no value, so no edit and no retry); map the ceiling action to
    /// `Repaired` in the S8 join -> red on the outcome.
    #[tokio::test]
    async fn a_converse_ceiling_refusal_retries_once_at_the_named_limit() {
        for stream in [false, true] {
            let seat = ConverseSeat::new(Seat::CapsThenServes);
            let router = converse_router(&[&seat]);

            let (meta, served) = dispatch(&router, stream, client_req(CLIENT_MAX_TOKENS)).await;

            assert!(served, "stream={stream}");
            assert_eq!(
                seat.sent_limits(),
                vec![json!(CLIENT_MAX_TOKENS), json!(NAMED_LIMIT)],
                "stream={stream}: one refused attempt, one retry at the named limit",
            );
            let bodies = seat.bodies();
            assert_eq!(
                bodies[0]["messages"], bodies[1]["messages"],
                "stream={stream}: the retry changes the limit and nothing else",
            );
            assert_eq!(meta.field_repairs.len(), 1, "stream={stream}");
            let repair = &meta.field_repairs[0];
            assert!(repair.repair_succeeded, "stream={stream}");
            assert_eq!(repair.field_path, "max_tokens");
            assert_eq!(repair.action, super::super::FIELD_ACTION_CEILING_REPAIR);
            assert!(
                meta.learned_capabilities.is_empty(),
                "stream={stream}: a ceiling repair teaches nothing yet",
            );
            assert_eq!(router.field_verdicts().snapshot_len(), 0, "stream={stream}");
            assert_eq!(
                rejection_outcomes(&router),
                vec![FieldRepairOutcome::RepairedLossy],
                "stream={stream}: the served request carried a limit the client did not send",
            );
        }
    }

    #[tokio::test]
    async fn a_ceiling_at_or_above_the_sent_limit_makes_no_retry() {
        for sent in [NAMED_LIMIT, NAMED_LIMIT - 1] {
            let seat = ConverseSeat::new(Seat::AlwaysRefuses);
            let router = converse_router(&[&seat]);

            let (meta, served) = dispatch(&router, false, client_req(sent)).await;

            assert!(!served, "sent={sent}: the refusal reaches the client");
            assert_eq!(
                seat.sent_limits(),
                vec![json!(sent)],
                "sent={sent}: no retry"
            );
            assert!(meta.field_repairs.is_empty(), "sent={sent}");
            assert_eq!(router.field_repair_counters().repair_attempted, 0);
            assert!(meta.learned_capabilities.is_empty());
        }
    }

    /// The positive control is the same harness at a limit above the named
    /// one, which repairs and draws exactly one repair.
    #[test]
    fn a_ceiling_at_or_above_the_sent_limit_draws_no_budget() {
        let seat = ConverseSeat::new(Seat::AlwaysRefuses);
        let router = converse_router(&[&seat]);
        let full = usize::from(REPAIRS_PER_REQUEST);
        let rows = [
            ("equal", NAMED_LIMIT, false, NAMED_LIMIT, full),
            ("below", NAMED_LIMIT - 1, false, NAMED_LIMIT - 1, full),
            ("above", CLIENT_MAX_TOKENS, true, NAMED_LIMIT, full - 1),
        ];

        for (name, sent, repaired, after, left) in rows {
            let applied =
                apply_on_first_target(&router, &client_req(sent), &ceiling_rejection(), "bedrock");

            assert_eq!(
                (applied.0, applied.1.max_tokens, applied.2),
                (repaired, Some(after), left),
                "{name}",
            );
        }
    }

    #[tokio::test]
    async fn the_fallback_target_receives_the_clients_original_limit() {
        let first = ConverseSeat::new(Seat::CapsThenUnavailable);
        let fallback = ConverseSeat::new(Seat::ServesAll);
        let router = converse_router(&[&first, &fallback]);

        let (meta, served) = dispatch(&router, false, client_req(CLIENT_MAX_TOKENS)).await;

        assert!(served, "the fallback serves");
        assert_eq!(
            first.sent_limits(),
            vec![json!(CLIENT_MAX_TOKENS), json!(NAMED_LIMIT)],
            "premise: the first target repaired and its retry failed",
        );
        assert_eq!(
            fallback.sent_limits(),
            vec![json!(CLIENT_MAX_TOKENS)],
            "the repair belongs to the target that named the limit",
        );
        assert_eq!(meta.served_model.as_deref(), Some("m1"));
        assert!(meta.learned_capabilities.is_empty());
        assert_eq!(router.field_verdicts().snapshot_len(), 0);
    }

    #[tokio::test]
    async fn a_ceiling_refusal_after_streamed_content_is_not_repaired() {
        let seat = ConverseSeat::new(Seat::StreamsThenRefuses);
        let router = converse_router(&[&seat]);
        let opened = router
            .stream_with_options(client_req(CLIENT_MAX_TOKENS), RouterOptions::new())
            .await;
        let meta = opened.meta;

        let frames: Vec<Result<ChatChunk>> = opened
            .result
            .expect("content committed the stream")
            .collect()
            .await;

        assert!(frames.first().is_some_and(std::result::Result::is_ok));
        assert!(
            frames.last().is_some_and(std::result::Result::is_err),
            "the post-content refusal ends the stream",
        );
        assert_eq!(
            seat.sent_limits(),
            vec![json!(CLIENT_MAX_TOKENS)],
            "no retry"
        );
        assert!(meta.field_repairs.is_empty());
        assert!(meta.learned_capabilities.is_empty());
        assert_eq!(router.field_repair_counters().repair_attempted, 0);
        assert_eq!(
            router.field_verdicts().snapshot_len(),
            0,
            "nothing committed"
        );
    }
}
