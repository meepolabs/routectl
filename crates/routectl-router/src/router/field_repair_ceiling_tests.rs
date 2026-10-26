// The max_tokens ceiling row: when the surface lowers a token limit, where a
// fidelity lane declares the edit, and the row end to end on a Bedrock
// Converse seat that assembles the real Converse body, so every assertion
// about the repair reads the body an upstream would receive.
// An `include!`d FRAGMENT of `field_repair_tests.rs`.

#[test]
fn a_max_tokens_clamp_lowers_only_a_value_above_the_ceiling() {
    let surface = super::FieldSurface::MaxTokens;
    // (name, client value, egress fills, ceiling slot, clamp to, present,
    //  changed, client after, slot after)
    let rows = [
        ("unset", None, false, 0, 8192, false, false, None, 0),
        (
            "above",
            Some(64_000),
            false,
            0,
            8192,
            true,
            true,
            Some(8192),
            0,
        ),
        (
            "equal",
            Some(8192),
            false,
            0,
            8192,
            true,
            false,
            Some(8192),
            0,
        ),
        (
            "below",
            Some(4096),
            false,
            0,
            8192,
            true,
            false,
            Some(4096),
            0,
        ),
        (
            "zero ceiling",
            Some(64_000),
            false,
            0,
            0,
            true,
            false,
            Some(64_000),
            0,
        ),
        (
            "fill above",
            None,
            true,
            64_000,
            8192,
            true,
            true,
            None,
            8192,
        ),
        (
            "fill equal",
            None,
            true,
            8192,
            8192,
            true,
            false,
            None,
            8192,
        ),
        (
            "fill below",
            None,
            true,
            4096,
            8192,
            true,
            false,
            None,
            4096,
        ),
        (
            "fill at the baseline",
            None,
            true,
            0,
            8192,
            true,
            true,
            None,
            8192,
        ),
        (
            "ceiling, no fill",
            None,
            false,
            64_000,
            8192,
            false,
            false,
            None,
            64_000,
        ),
        (
            "client wins over fill",
            Some(4096),
            true,
            64_000,
            8192,
            true,
            false,
            Some(4096),
            64_000,
        ),
        (
            "client above, slot kept",
            Some(64_000),
            true,
            4096,
            8192,
            true,
            true,
            Some(8192),
            4096,
        ),
    ];

    for (name, sent, fills, slot, ceiling, present, changed, after, slot_after) in rows {
        let mut req = req_on(ALIAS);
        req.max_tokens = sent;
        req.routectl_internal.fills_max_tokens = fills;
        req.routectl_internal.max_output_tokens = slot;
        assert_eq!(surface.present_in(&req), present, "{name}");
        assert_eq!(surface.clamp_writes_fill(&req), sent.is_none(), "{name}");

        assert_eq!(surface.clamp_to(&mut req, ceiling), changed, "{name}");

        assert_eq!(req.max_tokens, after, "{name}");
        assert_eq!(
            req.routectl_internal.max_output_tokens, slot_after,
            "{name}"
        );
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
    let named = super::rejected_field(
        router.field_repair_table(),
        &class,
        &err,
        "openai-compat",
        crate::router::field_repair::direct_egress("openai-compat"),
        crate::router::learn_engine::SentEffort::Unknown,
    )
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
        /// Refuse a limit above [`NAMED_LIMIT`]; stream content and then end
        /// on the capture at any other limit.
        CapsThenStreamsThenRefuses,
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
                Seat::CapsThenServes
                | Seat::CapsThenUnavailable
                | Seat::CapsThenStreamsThenRefuses
                    if above =>
                {
                    Err(ceiling_rejection())
                }
                Seat::CapsThenUnavailable => Err(Error::upstream(
                    "converse-seat",
                    503,
                    r#"{"message":"unavailable"}"#,
                )),
                Seat::CapsThenServes
                | Seat::ServesAll
                | Seat::StreamsThenRefuses
                | Seat::CapsThenStreamsThenRefuses => Ok(()),
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
            if self.seat == Seat::CapsThenStreamsThenRefuses {
                self.answer(&req)?;
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

    /// The field verdict key a committed `event` names.
    fn event_key(
        event: &crate::router::CapabilityLearnEvent,
    ) -> crate::field_verdict::FieldVerdictKey {
        crate::field_verdict::FieldVerdictKey::from_capability_key(
            crate::state_key::StateKey::parse(&event.state_key).expect("a lane key"),
            event.capability_key.clone(),
            event.provider_kind.clone(),
        )
    }

    /// The ceiling capture with its named limit, both copies, replaced by `limit`.
    fn ceiling_rejection_naming(limit: u32) -> Error {
        let fixture: serde_json::Value =
            serde_json::from_str(CEILING_FIXTURE).expect("the fixture is JSON");
        let body = fixture["body"]
            .as_str()
            .expect("the fixture carries its body");
        let named = NAMED_LIMIT.to_string();
        assert_eq!(
            body.matches(&named).count(),
            2,
            "premise: the limit appears twice"
        );
        Error::upstream_with_body(
            "converse-seat",
            400,
            routectl_core::UpstreamBody::new(body.replace(&named, &limit.to_string())),
            None,
            None,
            None,
        )
    }

    /// Mutation checks: drop the number capture from the Converse ceiling arm
    /// -> red (no value, so no edit and no retry); map the ceiling action to
    /// `Repaired` in the outcome join (`field_repair_outcome`) -> red on the outcome; make `repair_teaches`
    /// admit only rewrites and refused fields -> red (nothing learned); commit
    /// a ceiling edit with no value -> red on the persisted value.
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
                repair.learned,
                "stream={stream}: the summary reports the commit"
            );
            assert_eq!(
                router.field_repair_counters().verdicts_learned,
                1,
                "stream={stream}"
            );
            assert_eq!(meta.learned_capabilities.len(), 1, "stream={stream}");
            let event = &meta.learned_capabilities[0];
            assert_eq!(
                event.value,
                Some(crate::VerdictValue::Ceiling(NAMED_LIMIT)),
                "stream={stream}: the verdict carries the limit it clamped to",
            );
            assert_eq!(
                event
                    .value
                    .as_ref()
                    .map(crate::VerdictValue::encode)
                    .as_deref(),
                Some("ceiling:8192"),
                "stream={stream}",
            );
            assert_eq!(
                router
                    .field_verdicts()
                    .verdict_value(&event_key(event), event.incarnation),
                Some(crate::VerdictValue::Ceiling(NAMED_LIMIT)),
                "stream={stream}: the registry holds the value for the lane",
            );
            assert_eq!(router.field_verdicts().snapshot_len(), 1, "stream={stream}");
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
        assert!(router.learned_capability_snapshot().is_empty());
    }

    /// Mutation check: settle the field carry as a success when a repaired
    /// retry is rejected (call `settle_field_success` at the top of
    /// `on_rejection` while a reactive repair is pending) -> red.
    #[tokio::test]
    async fn a_failed_clamped_retry_leaves_no_resident_verdict() {
        let seat = ConverseSeat::new(Seat::CapsThenUnavailable);
        let router = converse_router(&[&seat]);

        let (meta, served) = dispatch(&router, false, client_req(CLIENT_MAX_TOKENS)).await;

        assert!(!served, "the retry's failure reaches the client");
        assert_eq!(
            seat.sent_limits(),
            vec![json!(CLIENT_MAX_TOKENS), json!(NAMED_LIMIT)],
            "premise: one refused attempt, one clamped retry",
        );
        assert_eq!(meta.field_repairs.len(), 1, "premise: the repair fired");
        assert!(!meta.field_repairs[0].repair_succeeded);
        assert!(!meta.field_repairs[0].learned);
        assert!(meta.learned_capabilities.is_empty(), "nothing committed");
        assert_eq!(router.field_verdicts().snapshot_len(), 0);
        assert!(router.learned_capability_snapshot().is_empty());
        assert_eq!(router.field_repair_counters().verdicts_learned, 0);
    }

    /// The operator WARN for a committed ceiling repair says it learned.
    ///
    /// Mutation check: make `repair_teaches` admit only rewrites and refused
    /// fields -> red (`learned=false`).
    #[test]
    fn a_committed_ceiling_repair_warns_learned() {
        let seat = ConverseSeat::new(Seat::CapsThenServes);
        let router = converse_router(&[&seat]);

        let events = routectl_testkit::capture_events(|| {
            let (_, served) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("a current-thread runtime with timers")
                .block_on(dispatch(&router, false, client_req(CLIENT_MAX_TOKENS)));
            assert!(served, "premise: the clamped retry serves");
        });

        let warns: Vec<_> = events
            .iter()
            .filter(|e| e.level == tracing::Level::WARN && e.message == "envelope_field_repaired")
            .collect();
        assert_eq!(warns.len(), 1, "one WARN per repaired request: {warns:?}");
        assert_eq!(
            warns[0].field("action"),
            Some(super::super::FIELD_ACTION_CEILING_REPAIR)
        );
        assert_eq!(warns[0].field("field_path"), Some("max_tokens"));
        assert_eq!(warns[0].field("learned"), Some("true"));
    }

    /// A later commit naming a different limit retires the resident verdict:
    /// the identity starts a new incarnation at one observation, carrying the
    /// new value, and the old incarnation's value is never answered again.
    /// The second repair is admitted past the first verdict's decay, the one
    /// moment a lane that holds an acting verdict repairs again.
    ///
    /// Mutation check: make `retire_on_value_change` return early
    /// unconditionally -> red (the new limit inherits the observation).
    #[tokio::test]
    async fn a_commit_naming_a_lower_limit_retires_the_earlier_incarnation() {
        const LOWER_LIMIT: u32 = 4096;
        let seat = ConverseSeat::new(Seat::CapsThenServes);
        let router = converse_router(&[&seat]);
        let (meta, served) = dispatch(&router, false, client_req(CLIENT_MAX_TOKENS)).await;
        assert!(served, "premise: the first repair served");
        let first = meta.learned_capabilities[0].clone();
        assert_eq!(
            first.value,
            Some(crate::VerdictValue::Ceiling(NAMED_LIMIT)),
            "premise"
        );
        let key = event_key(&first);
        let past_decay = std::time::Instant::now()
            + std::time::Duration::from_hours(router.config.capability.decay_hours)
            + std::time::Duration::from_secs(1);
        let req = client_req(CLIENT_MAX_TOKENS);
        let target = router
            .dispatch_chain_for_request(
                &req,
                crate::router::class_observe::DispatchSurface::Complete,
            )
            .expect("the fixture chain resolves")
            .0
            .into_iter()
            .next()
            .expect("one target");
        let mut plan = router
            .plan_field_carry(
                &target,
                &req,
                super::super::FieldSettlementMode::Settling,
                past_decay,
            )
            .expect("the lapsed verdict admits one repair");
        let rejection = ceiling_rejection_naming(LOWER_LIMIT);
        let class = routectl_core::failure_class::classify(&rejection, Some("bedrock")).class;
        let mut attempt = req.clone();
        let applied = plan.apply(
            &mut attempt,
            &mut crate::router::DispatchMeta::for_alias(ALIAS),
            &mut crate::router::repair_budget::RepairBudget::per_request(),
            &class,
            &rejection,
            "bedrock",
        );
        assert!(applied.is_some(), "premise: the lower limit repairs");
        assert_eq!(attempt.max_tokens, Some(LOWER_LIMIT), "premise");

        let second = plan
            .commit_pending(&[], past_decay)
            .pop()
            .and_then(|step| step.learned)
            .expect("the repaired success commits");

        assert_eq!(
            second.value,
            Some(crate::VerdictValue::Ceiling(LOWER_LIMIT))
        );
        assert_eq!(
            second.observations, 1,
            "a new limit restarts at one observation"
        );
        assert!(
            second.incarnation > first.incarnation,
            "and a new incarnation"
        );
        assert_eq!(
            router
                .field_verdicts()
                .verdict_value(&key, second.incarnation),
            Some(crate::VerdictValue::Ceiling(LOWER_LIMIT)),
        );
        assert_eq!(
            router
                .field_verdicts()
                .verdict_value(&key, first.incarnation),
            None,
            "the retired incarnation's value is gone",
        );
        assert_eq!(
            router.field_verdicts().snapshot_len(),
            1,
            "one identity resident"
        );
    }

    /// A ledger replaying exactly `rows`, with a tombstone at `router`'s own
    /// catalog stamps so no row is skipped as stale.
    struct EventLedger {
        catalog_version: u32,
        overlay_revision: u64,
        rows: Vec<crate::capability_rebuild::CapabilityEventRow>,
    }

    impl crate::capability_rebuild::CapabilityLedgerReader for EventLedger {
        fn tombstone(&self) -> Option<crate::capability_rebuild::ReplayTombstone> {
            Some(crate::capability_rebuild::ReplayTombstone::new(
                0,
                self.catalog_version,
                self.overlay_revision,
            ))
        }

        fn read_events(&self) -> Vec<crate::capability_rebuild::CapabilityEventRow> {
            self.rows.clone()
        }
    }

    /// A committed ceiling verdict's ledger row, replayed into a fresh router,
    /// restores the verdict with its value.
    ///
    /// Mutation check: give `FieldTransform::Ceiling` no value slot -> red (the
    /// replayed verdict carries no value).
    #[tokio::test]
    async fn a_warm_rebuild_restores_the_committed_ceiling() {
        let seat = ConverseSeat::new(Seat::CapsThenServes);
        let router = converse_router(&[&seat]);
        let (meta, served) = dispatch(&router, false, client_req(CLIENT_MAX_TOKENS)).await;
        assert!(served, "premise: the repair served");
        let event = meta.learned_capabilities[0].clone();
        let restarted = converse_router(&[&ConverseSeat::new(Seat::ServesAll)]);
        let row = crate::capability_rebuild::CapabilityEventRow::new(
            1,
            std::time::Instant::now(),
            "broken".to_string(),
            Some(event.phase.as_str().to_string()),
            event.source.as_str().to_string(),
            Some(event.signal_tier.as_str().to_string()),
            None,
            event.capability_key.clone(),
            event.state_key.clone(),
            event.provider_kind.clone(),
            restarted.catalog_version(),
            restarted.overlay_revision(),
        )
        .with_vocab_version(Some(crate::capability_vocab::CURRENT_VOCAB_VERSION))
        .with_upstream_token(event.value.as_ref().map(crate::VerdictValue::encode));

        let summary = restarted.rebuild_learned_from_ledger(&EventLedger {
            catalog_version: restarted.catalog_version(),
            overlay_revision: restarted.overlay_revision(),
            rows: vec![row],
        });

        assert_eq!(summary.replayed_negative, 1, "premise: the row replayed");
        assert_eq!(summary.degraded_value, 0, "the token decodes");
        let key = event_key(&event);
        let incarnation = restarted
            .learned_capabilities
            .resident_incarnation_for_tests(
                &event.state_key,
                &event.capability_key,
                &event.provider_kind,
            );
        assert_eq!(
            restarted.field_verdicts().verdict_value(&key, incarnation),
            Some(crate::VerdictValue::Ceiling(NAMED_LIMIT)),
        );
    }

    /// Converse forwards an omitted `max_tokens`: an acting learned ceiling on
    /// the lane lowers a client value above it but never makes an omitted limit
    /// appear on the wire. The client request after it is the positive
    /// control, proving the ceiling acts on this lane.
    ///
    /// Mutation check: make `emitted_max_tokens` answer the slot or the
    /// baseline when the egress does not fill (the `(None, false, _)` arm) ->
    /// red (pre-flight writes a limit the client never sent).
    #[tokio::test]
    async fn an_acting_learned_ceiling_never_adds_a_limit_to_an_omitted_converse_request() {
        // Arrange: learn the ceiling and acknowledge it, so it acts.
        let seat = ConverseSeat::new(Seat::CapsThenServes);
        let router = converse_router(&[&seat]);
        let (meta, served) = dispatch(&router, false, client_req(CLIENT_MAX_TOKENS)).await;
        assert!(served, "premise: the repair served");
        let event = meta.learned_capabilities[0].clone();
        assert!(
            router.acknowledge_durable_field_confirmation(
                &event.state_key,
                &event.capability_key,
                &event.provider_kind,
                event.persistence_generation,
                event.incarnation,
                event.observations,
            ),
            "premise: the durable write acknowledges the confirmation",
        );
        let mut omitted = client_req(CLIENT_MAX_TOKENS);
        omitted.max_tokens = None;
        omitted.routectl_internal.ingress_wire = None;

        // Act
        let (omitted_meta, omitted_served) = dispatch(&router, false, omitted).await;
        let (control_meta, control_served) =
            dispatch(&router, false, client_req(CLIENT_MAX_TOKENS)).await;

        // Assert
        assert!(omitted_served && control_served);
        let bodies = seat.bodies();
        assert_eq!(bodies.len(), 4, "one attempt each after the learning pair");
        assert!(
            bodies[2]["inferenceConfig"].get("maxTokens").is_none(),
            "an omitted limit stays omitted: {}",
            bodies[2]["inferenceConfig"],
        );
        assert!(omitted_meta.field_repairs.is_empty());
        assert!(
            !omitted_meta
                .field_preflight
                .iter()
                .any(|record| record.field_path == Some("max_tokens") && record.acted),
            "no pre-flight ceiling acts on an omitted limit",
        );
        assert_eq!(
            bodies[3]["inferenceConfig"]["maxTokens"],
            json!(NAMED_LIMIT),
            "control: the learned ceiling lowers a client value before sending",
        );
        assert!(
            control_meta.field_repairs.is_empty(),
            "control: no refusal first"
        );
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

    // A committed ceiling applied pre-flight on the next request.
    include!("field_repair_ceiling_preflight_tests.rs");
}
