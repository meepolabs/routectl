// The sampling-parameter drop rows: which requests carry them, where a
// fidelity lane declares the drop, and the temperature row end to end on a
// Bedrock Converse seat that assembles the real Converse body, so every
// assertion about the repair reads the body an upstream would receive.
// An `include!`d FRAGMENT of `field_repair_tests.rs`.

#[test]
fn a_sampling_param_is_present_while_set_and_dropped_by_taking_it() {
    let surface = super::FieldSurface::SamplingParam(super::SamplingParam::Temperature);
    let mut req = req_on(ALIAS);
    assert!(!surface.present_in(&req), "unset is absent");
    assert!(!surface.drop_from(&mut req), "nothing to drop");

    req.temperature = Some(0.0);
    assert!(surface.present_in(&req), "a zero temperature is still sent");
    assert!(surface.drop_from(&mut req));

    assert_eq!(req.temperature, None);
    assert!(!surface.present_in(&req));
}

#[test]
fn a_sampling_param_drop_declares_its_root_pointer_on_a_root_spelling_wire() {
    use routectl_core::wire_check::IngressDialect;
    let surface = super::FieldSurface::SamplingParam(super::SamplingParam::Temperature);
    let rows = [
        (IngressDialect::Anthropic, Some("/temperature")),
        (IngressDialect::Chat, Some("/temperature")),
        (IngressDialect::Responses, None),
    ];

    for (wire, pointer) in rows {
        assert_eq!(
            super::super::edit_declare::field_wire_pointer(surface, wire),
            pointer,
            "{wire:?}",
        );
    }
}

#[test]
fn the_temperature_row_has_no_recheck_payload() {
    let mut req = req_on(ALIAS);
    req.temperature = Some(0.5);
    assert!(
        super::FieldSurface::SamplingParam(super::SamplingParam::Temperature).present_in(&req),
        "premise: the request carries the row",
    );

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
