// The sampling-parameter drop rows: which requests carry them, where a
// fidelity lane declares the drop, the temperature row end to end on a
// Bedrock Converse seat, and each openai-compat row end to end on a seat
// that assembles the real openai-compat body, so every assertion about the
// repair reads the body an upstream would receive.
// An `include!`d FRAGMENT of `field_repair_tests.rs`.

/// Each sampling parameter's canonical field, read back after a drop.
type SamplingField = fn(&ChatRequest) -> Option<f64>;

fn sampling_params() -> [(super::SamplingParam, &'static str, SamplingField); 4] {
    use super::SamplingParam::{FrequencyPenalty, PresencePenalty, Temperature, TopP};
    [
        (Temperature, "temperature", |req| req.temperature),
        (TopP, "top_p", |req| req.top_p),
        (PresencePenalty, "presence_penalty", |req| {
            req.presence_penalty
        }),
        (FrequencyPenalty, "frequency_penalty", |req| {
            req.frequency_penalty
        }),
    ]
}

fn set_sampling_param(req: &mut ChatRequest, param: super::SamplingParam, value: f64) {
    use super::SamplingParam::{FrequencyPenalty, PresencePenalty, Temperature, TopP};
    match param {
        Temperature => req.temperature = Some(value),
        TopP => req.top_p = Some(value),
        PresencePenalty => req.presence_penalty = Some(value),
        FrequencyPenalty => req.frequency_penalty = Some(value),
    }
}

#[test]
fn a_sampling_param_is_present_while_set_and_dropped_by_taking_it() {
    for (param, name, field) in sampling_params() {
        let surface = super::FieldSurface::SamplingParam(param);
        let mut req = req_on(ALIAS);
        req.temperature = Some(0.25);
        req.top_p = Some(0.25);
        req.presence_penalty = Some(0.25);
        req.frequency_penalty = Some(0.25);
        let mut unset = req_on(ALIAS);
        assert!(!surface.present_in(&unset), "{name}: unset is absent");
        assert!(!surface.drop_from(&mut unset), "{name}: nothing to drop");

        set_sampling_param(&mut req, param, 0.0);
        assert!(
            surface.present_in(&req),
            "{name}: a zero value is still sent"
        );
        assert!(surface.drop_from(&mut req), "{name}");

        assert_eq!(field(&req), None, "{name}");
        assert!(!surface.present_in(&req), "{name}");
        let untouched = sampling_params()
            .into_iter()
            .filter(|(other, ..)| *other != param)
            .all(|(_, _, other_field)| other_field(&req) == Some(0.25));
        assert!(untouched, "{name}: the drop takes only its own field");
    }
}

#[test]
fn a_sampling_param_drop_declares_its_root_pointer_on_a_root_spelling_wire() {
    use routectl_core::wire_check::IngressDialect;
    for (param, name, _) in sampling_params() {
        let surface = super::FieldSurface::SamplingParam(param);
        let root = format!("/{name}");
        let rows = [
            (IngressDialect::Anthropic, Some(root.as_str())),
            (IngressDialect::Chat, Some(root.as_str())),
            (IngressDialect::Responses, None),
        ];

        for (wire, pointer) in rows {
            assert_eq!(
                super::super::edit_declare::field_wire_pointer(surface, wire),
                pointer,
                "{name} on {wire:?}",
            );
        }
    }
}

#[test]
fn no_sampling_param_row_has_a_recheck_payload() {
    let mut req = req_on(ALIAS);
    for (param, name, _) in sampling_params() {
        set_sampling_param(&mut req, param, 0.5);
        assert!(
            super::FieldSurface::SamplingParam(param).present_in(&req),
            "premise: the request carries the {name} row",
        );
    }

    let grounded: Vec<_> =
        super::recheck_payloads(crate::router::FieldRepairTable::grounded_only(), &req).collect();

    assert!(
        grounded.is_empty(),
        "a row no token count validates is never re-checked: {grounded:?}",
    );
    let mut control = req;
    control.chat_template_kwargs = Some(json!({ super::TEST_RECHECK_KWARG: "v" }));
    assert_eq!(
        super::recheck_payloads(crate::router::FieldRepairTable::default(), &control)
            .map(|(path, _)| path)
            .collect::<Vec<_>>(),
        vec![super::TEST_RECHECK_PATH],
    );
}

#[cfg(feature = "bedrock")]
mod converse_temperature {
    use super::*;

    use crate::wire_check_state::FieldRepairOutcome;
    use routectl_providers::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};

    const TEMPERATURE_ROW_PATH: &str = "temperature";

    /// The committed Converse capture of a model refusing `temperature`.
    const TEMPERATURE_FIXTURE: &str =
        include_str!("../../tests/fixtures/rejections/bedrock/temperature-field-unsupported.json");

    const CONVERSE_TOML: &str = "[wire_check]\nfirst_n = 8\nsample_one_in = 1\n\
         [retry]\nmax_attempts = 1\ninitial_backoff_ms = 0\njitter_ms = 0\n\n\
         [providers.p0]\nkind = \"bedrock\"\nregion = \"us-east-1\"\n\
         creds = { kind = \"default-chain\" }\napi_shape = \"converse\"\n";

    fn temperature_key() -> String {
        crate::field_capability::field_capability_key(TEMPERATURE_ROW_PATH)
            .expect("the grounded path mints a key")
    }

    fn temperature_rejection() -> Error {
        let fixture: serde_json::Value =
            serde_json::from_str(TEMPERATURE_FIXTURE).expect("the fixture is JSON");
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

    /// How the seat answers a body that carries no temperature.
    #[derive(Clone, Copy)]
    enum Retry {
        Serve,
        Unavailable,
    }

    /// A Converse seat: assembles each attempt's body with the real Converse
    /// translation, deposits it into the attempt's wire tap, refuses it with
    /// the committed capture while `inferenceConfig.temperature` is present,
    /// and otherwise answers per `retry`.
    struct ConverseSeat {
        cfg: BedrockConfig,
        retry: Retry,
        bodies: Mutex<Vec<serde_json::Value>>,
    }

    impl ConverseSeat {
        fn new(retry: Retry) -> Arc<Self> {
            Arc::new(Self {
                cfg: BedrockConfig {
                    id: "bedrock:p0".into(),
                    region: "us-east-1".into(),
                    model_id: "us.zz.temperature-refusing".into(),
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
                retry,
                bodies: Mutex::new(Vec::new()),
            })
        }

        fn bodies(&self) -> Vec<serde_json::Value> {
            self.bodies.lock().clone()
        }

        fn answer(&self, req: &ChatRequest) -> Result<()> {
            let body = routectl_providers::bedrock::converse::normalize_request(&self.cfg, req)?;
            if let Some(tap) = req.routectl_internal.wire_tap.as_ref() {
                tap.deposit(serde_json::to_vec(&body).expect("serialize").into());
            }
            let refused = body["inferenceConfig"].get("temperature").is_some();
            self.bodies.lock().push(body);
            if refused {
                return Err(temperature_rejection());
            }
            match self.retry {
                Retry::Serve => Ok(()),
                Retry::Unavailable => Err(Error::upstream(
                    "converse-seat",
                    503,
                    r#"{"message":"unavailable"}"#,
                )),
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
            self.answer(&req).map(|()| {
                stream::iter(vec![Ok(content_chunk())]).boxed()
                    as BoxStream<'static, Result<ChatChunk>>
            })
        }
    }

    fn converse_router(seat: &Arc<ConverseSeat>) -> Router {
        let mut config: Config = toml::from_str(CONVERSE_TOML).expect("valid test toml");
        config
            .aliases
            .insert(ALIAS.to_string(), AliasValue::Chain(vec!["m0".to_string()]));
        let mut router = Router::new(Arc::new(config));
        router.install_resolved_models(BTreeMap::from([(
            "m0".to_string(),
            Arc::new(ResolvedModel::new(
                "m0",
                "p0",
                Arc::clone(seat) as Arc<dyn Provider>,
                "upstream",
            )),
        )]));
        router.with_capability_writes_assumed_durable_for_tests()
    }

    /// An Anthropic client's `temperature: 0.5` with no thinking, as the
    /// ingress parses it, client bytes included for the wire check.
    fn temperature_req() -> ChatRequest {
        let mut req = req_on(ALIAS);
        req.max_tokens = Some(1024);
        req.temperature = Some(0.5);
        req.reasoning = None;
        req.routectl_internal.anthropic_thinking_display = None;
        req.routectl_internal.anthropic_thinking = routectl_core::AnthropicThinking::SentNone;
        req.routectl_internal.ingress_wire = Some(routectl_core::wire_check::IngressWire {
            dialect: routectl_core::wire_check::IngressDialect::Anthropic,
            client_version: None,
            bytes: serde_json::to_vec(&json!({
                "model": ALIAS,
                "max_tokens": 1024,
                "temperature": 0.5,
                "messages": [{"role": "user", "content": "hello"}],
            }))
            .expect("serialize")
            .into(),
        });
        req
    }

    fn sent_temperature(body: &serde_json::Value) -> Option<&serde_json::Value> {
        body["inferenceConfig"].get("temperature")
    }

    fn rejection_outcomes(router: &Router) -> Vec<FieldRepairOutcome> {
        router
            .wire_check_snapshot()
            .findings
            .iter()
            .filter_map(|finding| finding.field_repair_outcome)
            .collect()
    }

    /// Mutation check: delete the Bedrock temperature arm -> red (the
    /// refusal localizes nothing, so no retry is sent).
    #[tokio::test]
    async fn a_converse_temperature_refusal_retries_once_without_it_and_commits() {
        let seat = ConverseSeat::new(Retry::Serve);
        let router = converse_router(&seat);

        let dispatched = router
            .complete_with_options(temperature_req(), RouterOptions::new())
            .await;

        assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
        let bodies = seat.bodies();
        assert_eq!(bodies.len(), 2, "one refused attempt, one retry");
        assert_eq!(sent_temperature(&bodies[0]), Some(&json!(0.5)));
        assert_eq!(sent_temperature(&bodies[1]), None, "{}", bodies[1]);
        assert_eq!(
            bodies[1]["inferenceConfig"]["maxTokens"],
            json!(1024),
            "the retry drops temperature and nothing else",
        );
        assert_eq!(dispatched.meta.field_repairs.len(), 1);
        let repair = &dispatched.meta.field_repairs[0];
        assert!(repair.repair_succeeded);
        assert_eq!(repair.field_path, TEMPERATURE_ROW_PATH);
        assert_eq!(repair.action, super::super::FIELD_ACTION_DROP_REPAIR);
        assert_eq!(
            dispatched
                .meta
                .learned_capabilities
                .iter()
                .map(|event| (
                    event.capability_key.clone(),
                    event.provider_kind.clone(),
                    event.value
                ))
                .collect::<Vec<_>>(),
            vec![(temperature_key(), "bedrock".to_string(), None)],
        );
        assert_eq!(
            rejection_outcomes(&router),
            vec![FieldRepairOutcome::Repaired]
        );
    }

    /// Mutation check: make the pre-flight planner skip drop rows -> red (the
    /// second request is refused before it is repaired).
    #[tokio::test]
    async fn the_committed_drop_preflights_the_next_converse_request() {
        let seat = ConverseSeat::new(Retry::Serve);
        let router = converse_router(&seat);
        let first = router
            .complete_with_options(temperature_req(), RouterOptions::new())
            .await;
        assert!(first.result.is_ok(), "{:?}", first.result.err());
        let event = first
            .meta
            .learned_capabilities
            .first()
            .expect("the repair committed a verdict");
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
        let key = crate::field_verdict::FieldVerdictKey::new(
            &crate::state_key::StateKey::parse(&event.state_key).expect("the event names a lane"),
            TEMPERATURE_ROW_PATH,
            "bedrock",
        )
        .expect("a grounded path mints a key");
        let cadence = router
            .field_verdicts()
            .canaries()
            .snapshot(&key)
            .expect("the confirmation seeded the canary state")
            .cadence;
        assert!(cadence > 1, "premise: the next request is not the canary");

        let second = router
            .complete_with_options(temperature_req(), RouterOptions::new())
            .await;

        assert!(second.result.is_ok(), "{:?}", second.result.err());
        let bodies = seat.bodies();
        assert_eq!(bodies.len(), 3, "the second request is sent once");
        assert_eq!(sent_temperature(&bodies[2]), None, "{}", bodies[2]);
        assert!(
            second.meta.field_repairs.is_empty(),
            "no rejection, so no reactive repair",
        );
        assert!(
            second
                .meta
                .field_preflight
                .iter()
                .any(|record| record.acted && record.field_path == Some(TEMPERATURE_ROW_PATH)),
            "the pre-flight drop acted: {:?}",
            second.meta.field_preflight,
        );
    }

    #[tokio::test]
    async fn a_failed_temperature_retry_commits_nothing() {
        let seat = ConverseSeat::new(Retry::Unavailable);
        let router = converse_router(&seat);

        let dispatched = router
            .complete_with_options(temperature_req(), RouterOptions::new())
            .await;

        assert!(
            dispatched.result.is_err(),
            "the retry's failure reaches the client"
        );
        let bodies = seat.bodies();
        assert_eq!(bodies.len(), 2, "one repaired retry and no further attempt");
        assert_eq!(sent_temperature(&bodies[1]), None);
        assert_eq!(dispatched.meta.field_repairs.len(), 1);
        assert!(!dispatched.meta.field_repairs[0].repair_succeeded);
        assert!(
            dispatched.meta.learned_capabilities.is_empty(),
            "a failed repaired retry commits no verdict",
        );
        assert_eq!(router.field_repair_counters().repair_attempted, 1);
        assert_eq!(
            router.field_verdicts().snapshot_len(),
            0,
            "nothing is resident"
        );
    }
}

mod openai_compat_sampling {
    use super::*;

    use routectl_providers::openai_compat::request::normalize;
    use routectl_providers::openai_compat::{HistoryReasoning, ReasoningDialect};

    /// An upstream model name no reasoning-model profile matches, so the
    /// egress forwards every sampling parameter the client sent.
    const UNPROFILED_MODEL: &str = "acme-chat-1";

    const OPENAI_COMPAT_SEAT_TOML: &str = "[wire_check]\nfirst_n = 8\nsample_one_in = 1\n\
         [retry]\nmax_attempts = 1\ninitial_backoff_ms = 0\njitter_ms = 0\n\n\
         [providers.p0]\nkind = \"openai-compat\"\nbase_url = \"https://example.test/v1\"\n\
         api_key_ref = \"literal:k\"\n";

    /// One openai-compat row: its key, the committed refusal of it, and how
    /// a client sets it.
    struct Case {
        key: &'static str,
        fixture: &'static str,
        set: fn(&mut ChatRequest),
    }

    fn cases() -> [Case; 4] {
        [
            Case {
                key: "temperature",
                fixture: include_str!(
                    "../../tests/fixtures/rejections/openai-compat/unsupported-parameter-temperature.json"
                ),
                set: |req| req.temperature = Some(0.5),
            },
            Case {
                key: "top_p",
                fixture: include_str!(
                    "../../tests/fixtures/rejections/openai-compat/unsupported-parameter-top-p.json"
                ),
                set: |req| req.top_p = Some(0.5),
            },
            Case {
                key: "presence_penalty",
                fixture: include_str!(
                    "../../tests/fixtures/rejections/openai-compat/unsupported-parameter-presence-penalty.json"
                ),
                set: |req| req.presence_penalty = Some(0.5),
            },
            Case {
                key: "frequency_penalty",
                fixture: include_str!(
                    "../../tests/fixtures/rejections/openai-compat/unsupported-parameter-frequency-penalty.json"
                ),
                set: |req| req.frequency_penalty = Some(0.5),
            },
        ]
    }

    /// The committed refusal as the openai-compat provider surfaces it: the
    /// body with its lifted `error.type` and `error.code`.
    fn fixture_rejection(fixture: &str) -> Error {
        let fixture: serde_json::Value =
            serde_json::from_str(fixture).expect("the fixture is JSON");
        let status = u16::try_from(fixture["status"].as_u64().expect("a status")).expect("u16");
        let body = fixture["body"]
            .as_str()
            .expect("the fixture carries its body");
        let parsed: serde_json::Value = serde_json::from_str(body).expect("the body is JSON");
        let token = |pointer: &str| {
            parsed
                .pointer(pointer)
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        Error::upstream_with_body(
            "openai-compat-seat",
            status,
            routectl_core::UpstreamBody::new(body),
            None,
            token("/error/type"),
            token("/error/code"),
        )
    }

    /// An openai-compat seat: assembles each attempt's body with the real
    /// openai-compat translation, deposits it into the attempt's wire tap,
    /// refuses it with the committed refusal while the body carries `key`,
    /// and otherwise serves.
    struct CompatSeat {
        key: &'static str,
        fixture: &'static str,
        bodies: Mutex<Vec<serde_json::Value>>,
    }

    impl CompatSeat {
        fn bodies(&self) -> Vec<serde_json::Value> {
            self.bodies.lock().clone()
        }

        fn answer(&self, req: &ChatRequest) -> Result<()> {
            let body = normalize(
                "openai-compat-seat",
                req,
                ReasoningDialect::OpenAi,
                HistoryReasoning::default(),
                None,
                false,
            )?;
            if let Some(tap) = req.routectl_internal.wire_tap.as_ref() {
                tap.deposit(serde_json::to_vec(&body).expect("serialize").into());
            }
            let refused = body.get(self.key).is_some();
            self.bodies.lock().push(body);
            if refused {
                return Err(fixture_rejection(self.fixture));
            }
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl Provider for CompatSeat {
        fn id(&self) -> &'static str {
            "openai-compat-seat"
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response("openai-compat-seat", "unused"))
        }
        async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
            self.answer(&req).map(|()| success_response())
        }
        async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            self.answer(&req).map(|()| {
                stream::iter(vec![Ok(content_chunk())]).boxed()
                    as BoxStream<'static, Result<ChatChunk>>
            })
        }
    }

    fn compat_router(seat: &Arc<CompatSeat>) -> Router {
        let mut config: Config = toml::from_str(OPENAI_COMPAT_SEAT_TOML).expect("valid test toml");
        config
            .aliases
            .insert(ALIAS.to_string(), AliasValue::Chain(vec!["m0".to_string()]));
        let mut router = Router::new(Arc::new(config));
        router.install_resolved_models(BTreeMap::from([(
            "m0".to_string(),
            Arc::new(ResolvedModel::new(
                "m0",
                "p0",
                Arc::clone(seat) as Arc<dyn Provider>,
                UNPROFILED_MODEL,
            )),
        )]));
        router.with_capability_writes_assumed_durable_for_tests()
    }

    /// A Chat Completions client's request setting `case.key`, with no
    /// thinking, as the ingress parses it, client bytes included for the
    /// wire check.
    fn sampling_req(case: &Case) -> ChatRequest {
        let mut req = req_on(ALIAS);
        req.max_tokens = Some(1024);
        (case.set)(&mut req);
        req.reasoning = None;
        req.routectl_internal.anthropic_thinking_display = None;
        req.routectl_internal.anthropic_thinking = routectl_core::AnthropicThinking::SentNone;
        let mut client = json!({
            "model": ALIAS,
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hello"}],
        });
        client[case.key] = json!(0.5);
        req.routectl_internal.ingress_wire = Some(routectl_core::wire_check::IngressWire {
            dialect: routectl_core::wire_check::IngressDialect::Chat,
            client_version: None,
            bytes: serde_json::to_vec(&client).expect("serialize").into(),
        });
        req
    }

    /// Mutation check: delete one openai-compat sampling row from the closed
    /// table -> its case reds (the refusal resolves no row, so no retry).
    #[tokio::test]
    async fn an_openai_compat_sampling_refusal_drops_the_key_commits_and_preflights_the_next_request()
     {
        use crate::wire_check_state::FieldRepairOutcome;

        for case in cases() {
            let key = case.key;
            let seat = Arc::new(CompatSeat {
                key,
                fixture: case.fixture,
                bodies: Mutex::new(Vec::new()),
            });
            let router = compat_router(&seat);

            let first = router
                .complete_with_options(sampling_req(&case), RouterOptions::new())
                .await;

            assert!(first.result.is_ok(), "{key}: {:?}", first.result.err());
            let bodies = seat.bodies();
            assert_eq!(bodies.len(), 2, "{key}: one refused attempt, one retry");
            assert_eq!(
                bodies[0].get(key),
                Some(&json!(0.5)),
                "{key}: premise: the egress forwards the key, so no profile stripped it: {}",
                bodies[0],
            );
            assert_eq!(bodies[1].get(key), None, "{key}: {}", bodies[1]);
            let mut without_key = bodies[0].clone();
            without_key.as_object_mut().expect("an object").remove(key);
            assert_eq!(
                bodies[1], without_key,
                "{key}: the retry drops the key and nothing else"
            );
            assert_eq!(first.meta.field_repairs.len(), 1, "{key}");
            let repair = &first.meta.field_repairs[0];
            assert!(repair.repair_succeeded, "{key}");
            assert_eq!(repair.field_path, key);
            assert_eq!(
                repair.action,
                super::super::FIELD_ACTION_DROP_REPAIR,
                "{key}"
            );
            let outcomes: Vec<FieldRepairOutcome> = router
                .wire_check_snapshot()
                .findings
                .iter()
                .filter_map(|finding| finding.field_repair_outcome)
                .collect();
            assert_eq!(outcomes, vec![FieldRepairOutcome::Repaired], "{key}");
            let [event] = first.meta.learned_capabilities.as_slice() else {
                panic!("{key}: one verdict: {:?}", first.meta.learned_capabilities);
            };
            assert_eq!(
                (
                    event.capability_key.as_str(),
                    event.provider_kind.as_str(),
                    event.value
                ),
                (
                    crate::field_capability::field_capability_key(key)
                        .expect("a grounded path mints a key")
                        .as_str(),
                    "openai-compat",
                    None
                ),
                "{key}",
            );
            assert!(
                router.acknowledge_durable_field_confirmation(
                    &event.state_key,
                    &event.capability_key,
                    &event.provider_kind,
                    event.persistence_generation,
                    event.incarnation,
                    event.observations,
                ),
                "{key}: premise: the durable write acknowledges the confirmation",
            );
            let verdict_key = crate::field_verdict::FieldVerdictKey::new(
                &crate::state_key::StateKey::parse(&event.state_key)
                    .expect("the event names a lane"),
                key,
                "openai-compat",
            )
            .expect("a grounded path mints a key");
            let cadence = router
                .field_verdicts()
                .canaries()
                .snapshot(&verdict_key)
                .expect("the confirmation seeded the canary state")
                .cadence;
            assert!(
                cadence > 1,
                "{key}: premise: the next request is not the canary"
            );

            let second = router
                .complete_with_options(sampling_req(&case), RouterOptions::new())
                .await;

            assert!(second.result.is_ok(), "{key}: {:?}", second.result.err());
            let bodies = seat.bodies();
            assert_eq!(bodies.len(), 3, "{key}: the second request is sent once");
            assert_eq!(bodies[2].get(key), None, "{key}: {}", bodies[2]);
            assert!(
                second.meta.field_repairs.is_empty(),
                "{key}: no reactive repair"
            );
            assert!(
                second
                    .meta
                    .field_preflight
                    .iter()
                    .any(|record| record.acted && record.field_path == Some(key)),
                "{key}: the pre-flight drop acted: {:?}",
                second.meta.field_preflight,
            );
        }
    }
}

#[cfg(feature = "openai-responses")]
mod responses_temperature {
    use super::*;

    use routectl_core::failure_class::classify;

    const TEMPERATURE: &str = "temperature";

    const RESPONSES_TOML: &str = "[wire_check]\nfirst_n = 8\nsample_one_in = 1\n\
         [retry]\nmax_attempts = 1\ninitial_backoff_ms = 0\njitter_ms = 0\n\n\
         [providers.p0]\nkind = \"openai-responses\"\nauth_kind = \"api-key\"\n\
         base_url = \"https://example.test/v1\"\napi_key_ref = \"literal:k\"\n";

    /// The mantle-responses refusal of `temperature` as captured: the OpenAI
    /// envelope naming the parameter in `error.param`.
    const CAPTURED_MESSAGE: &str =
        "Unsupported parameter: 'temperature' is not supported with this model.";

    /// The refusal as the openai-responses provider surfaces it: the body
    /// with its lifted `error.type` and `error.code`.
    fn refusal(code: &'static str) -> Error {
        let body = json!({"error": {
            "code": code,
            "message": CAPTURED_MESSAGE,
            "param": TEMPERATURE,
            "type": "invalid_request_error",
        }})
        .to_string();
        Error::upstream_with_body(
            "responses-seat",
            400,
            routectl_core::UpstreamBody::new(body),
            None,
            Some("invalid_request_error".into()),
            Some(code.into()),
        )
    }

    /// A Responses seat: deposits a body carrying `temperature` while the
    /// request does, refuses that body with `code`'s refusal, and otherwise
    /// serves.
    struct ResponsesSeat {
        code: &'static str,
        calls: AtomicUsize,
    }

    impl ResponsesSeat {
        fn answer(&self, req: &ChatRequest) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut body = json!({"model": "upstream", "input": "hi"});
            if let Some(temperature) = req.temperature {
                body[TEMPERATURE] = json!(temperature);
            }
            if let Some(tap) = req.routectl_internal.wire_tap.as_ref() {
                tap.deposit(serde_json::to_vec(&body).expect("serialize").into());
            }
            if req.temperature.is_some() {
                return Err(refusal(self.code));
            }
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl Provider for ResponsesSeat {
        fn id(&self) -> &'static str {
            "responses-seat"
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response("responses-seat", "unused"))
        }
        async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
            self.answer(&req).map(|()| success_response())
        }
        async fn stream(&self, req: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            self.answer(&req).map(|()| {
                stream::iter(vec![Ok(content_chunk())]).boxed()
                    as BoxStream<'static, Result<ChatChunk>>
            })
        }
    }

    fn responses_router(seat: &Arc<ResponsesSeat>) -> Router {
        let mut config: Config = toml::from_str(RESPONSES_TOML).expect("valid test toml");
        config
            .aliases
            .insert(ALIAS.to_string(), AliasValue::Chain(vec!["m0".to_string()]));
        let mut router = Router::new(Arc::new(config));
        router.install_resolved_models(BTreeMap::from([(
            "m0".to_string(),
            Arc::new(ResolvedModel::new(
                "m0",
                "p0",
                Arc::clone(seat) as Arc<dyn Provider>,
                "upstream",
            )),
        )]));
        router.with_capability_writes_assumed_durable_for_tests()
    }

    /// A Responses client's `temperature: 0.5`, client bytes included for
    /// the wire check.
    fn temperature_req() -> ChatRequest {
        let mut req = req_on(ALIAS);
        req.temperature = Some(0.5);
        req.reasoning = None;
        req.routectl_internal.anthropic_thinking_display = None;
        req.routectl_internal.anthropic_thinking = routectl_core::AnthropicThinking::SentNone;
        req.routectl_internal.ingress_wire = Some(routectl_core::wire_check::IngressWire {
            dialect: routectl_core::wire_check::IngressDialect::Responses,
            client_version: None,
            bytes: serde_json::to_vec(&json!({
                "model": ALIAS,
                "input": "hi",
                "temperature": 0.5,
            }))
            .expect("serialize")
            .into(),
        });
        req
    }

    /// The parser names the `temperature` row on an openai-responses lane,
    /// but the lane's kind is outside the field repair: no plan admits it, a
    /// refusal is sent once and reaches the client, and nothing is repaired
    /// or learned. Both codes: the captured `unsupported_parameter`, which
    /// the class gate also refuses on this kind, and `invalid_value`, which
    /// arrives as `BadRequest` so only the kind gate stands.
    ///
    /// Mutation check: add openai-responses to `FIELD_REPAIR_KINDS` -> red
    /// (a plan is admitted).
    #[tokio::test]
    async fn a_responses_temperature_refusal_admits_no_plan_and_repairs_nothing() {
        for code in ["unsupported_parameter", "invalid_value"] {
            let seat = Arc::new(ResponsesSeat {
                code,
                calls: AtomicUsize::new(0),
            });
            let router = responses_router(&seat);
            let req = temperature_req();
            let named = super::super::rejected_field(
                router.field_repair_table(),
                &classify(&refusal(code), Some("openai-responses")).class,
                &refusal(code),
                "openai-responses",
            );
            if code == "invalid_value" {
                assert_eq!(
                    named.map(|named| named.row.path),
                    Some(TEMPERATURE),
                    "{code}: premise: the refusal names the temperature row",
                );
            }
            let target = router
                .dispatch_chain_for_request(
                    &req,
                    crate::router::class_observe::DispatchSurface::Complete,
                )
                .expect("the chain resolves")
                .0
                .into_iter()
                .next()
                .expect("one target");

            let plan = router.plan_field_carry(
                &target,
                &req,
                super::super::FieldSettlementMode::Settling,
                std::time::Instant::now(),
            );
            let dispatched = router
                .complete_with_options(req, RouterOptions::new())
                .await;

            assert!(plan.is_none(), "{code}: no plan admits the lane");
            assert_eq!(seat.calls.load(Ordering::SeqCst), 1, "{code}: no retry");
            assert!(dispatched.result.is_err(), "{code}: the refusal stands");
            assert!(
                dispatched.meta.field_repairs.is_empty(),
                "{code}: no repair: {:?}",
                dispatched.meta.field_repairs,
            );
            assert!(
                dispatched.meta.learned_capabilities.is_empty(),
                "{code}: and no verdict",
            );
        }
    }
}
