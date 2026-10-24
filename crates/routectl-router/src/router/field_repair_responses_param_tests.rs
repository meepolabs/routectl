// Parameter refusals on openai-responses, where only the effort parameter
// localizes: a refusal naming `temperature` names no row and repairs nothing,
// and a refusal naming a parameter an operator payload extra put on the wire
// does not revert that extra.
// An `include!`d FRAGMENT of `field_repair_tests.rs`.

#[cfg(feature = "openai-responses")]
mod responses_param_refusals {
    use super::*;

    use routectl_core::failure_class::classify;

    const TEMPERATURE: &str = "temperature";
    const SERVICE_TIER: &str = "service_tier";
    const EDIT_REJECTED: &str = "routectl_edit_rejected";

    const RESPONSES_TOML: &str = "[wire_check]\nfirst_n = 8\nsample_one_in = 1\n\
         [retry]\nmax_attempts = 1\ninitial_backoff_ms = 0\njitter_ms = 0\n\n\
         [providers.p0]\nkind = \"openai-responses\"\nauth_kind = \"api-key\"\n\
         base_url = \"https://example.test/v1\"\napi_key_ref = \"literal:k\"\n";
    /// An operator extra the egress merges into the body as a top-level key
    /// the client never sent.
    const SERVICE_TIER_EXTRA: &str = "payload_extras = { service_tier = \"priority\" }\n";

    /// The OpenAI envelope refusing `param` by name, with `code`, as the
    /// openai-responses provider surfaces it: the body with its lifted
    /// `error.type` and `error.code`.
    fn refusal(param: &str, code: &'static str) -> Error {
        let body = json!({"error": {
            "code": code,
            "message": format!("Unsupported parameter: '{param}' is not supported with this model."),
            "param": param,
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

    /// A Responses seat: deposits the body it would send (the request's
    /// `temperature` and its merged extras at the top level), refuses a body
    /// carrying `refused` with that parameter's refusal under `code`, and
    /// otherwise serves.
    struct ResponsesSeat {
        refused: &'static str,
        code: &'static str,
        calls: AtomicUsize,
    }

    impl ResponsesSeat {
        fn new(refused: &'static str, code: &'static str) -> Arc<Self> {
            Arc::new(Self {
                refused,
                code,
                calls: AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn answer(&self, req: &ChatRequest) -> Result<()> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut body = json!({"model": "upstream", "input": "hi"});
            if let Some(temperature) = req.temperature {
                body[TEMPERATURE] = json!(temperature);
            }
            if let Some(extras) = req.provider_extras.as_ref().and_then(|e| e.as_object()) {
                for (key, value) in extras {
                    body[key] = value.clone();
                }
            }
            if let Some(tap) = req.routectl_internal.wire_tap.as_ref() {
                tap.deposit(serde_json::to_vec(&body).expect("serialize").into());
            }
            if body.get(self.refused).is_some() {
                return Err(refusal(self.refused, self.code));
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

    fn responses_router(seat: &Arc<ResponsesSeat>, provider_extra: &str) -> Router {
        let toml_text = format!("{RESPONSES_TOML}{provider_extra}");
        let mut config: Config = toml::from_str(&toml_text).expect("valid test toml");
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

    /// A Responses client's request, with `temperature` when given, client
    /// bytes included for the wire check.
    fn responses_req(temperature: Option<f64>) -> ChatRequest {
        let mut req = req_on(ALIAS);
        req.temperature = temperature;
        req.reasoning = None;
        req.routectl_internal.anthropic_thinking_display = None;
        req.routectl_internal.anthropic_thinking = routectl_core::AnthropicThinking::SentNone;
        let mut client = json!({"model": ALIAS, "input": "hi"});
        if let Some(temperature) = temperature {
            client[TEMPERATURE] = json!(temperature);
        }
        req.routectl_internal.ingress_wire = Some(routectl_core::wire_check::IngressWire {
            dialect: routectl_core::wire_check::IngressDialect::Responses,
            client_version: None,
            bytes: serde_json::to_vec(&client).expect("serialize").into(),
        });
        req
    }

    /// One complete dispatch, with the events it logged.
    async fn dispatch(
        router: &Router,
        req: ChatRequest,
    ) -> (
        crate::router::Dispatched,
        Vec<routectl_testkit::CapturedEvent>,
    ) {
        routectl_testkit::with_capture(Box::pin(
            router.complete_with_options(req, RouterOptions::new()),
        ))
        .await
    }

    fn edit_rejected_warnings(events: &[routectl_testkit::CapturedEvent]) -> usize {
        events
            .iter()
            .filter(|event| event.message == EDIT_REJECTED)
            .count()
    }

    /// A Responses refusal naming `temperature` localizes nothing: only the
    /// effort parameter is read on this kind, so no row is named, no plan
    /// admits the lane, the refusal is sent once and reaches the client, and
    /// nothing is repaired or learned. Both codes: the `unsupported_parameter`
    /// refusal and an `invalid_value` one, which arrives as `BadRequest`.
    #[tokio::test]
    async fn a_responses_temperature_refusal_names_no_row_and_repairs_nothing() {
        for code in ["unsupported_parameter", "invalid_value"] {
            let seat = ResponsesSeat::new(TEMPERATURE, code);
            let router = responses_router(&seat, "");
            let req = responses_req(Some(0.5));
            let refused = refusal(TEMPERATURE, code);
            let named = super::super::rejected_field(
                router.field_repair_table(),
                &classify(&refused, Some("openai-responses")).class,
                &refused,
                "openai-responses",
                crate::router::learn_engine::SentEffort::Unknown,
            );
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
            let (dispatched, events) = Box::pin(dispatch(&router, req)).await;

            assert!(named.is_none(), "{code}: the refusal names no row");
            assert!(plan.is_none(), "{code}: no plan admits the lane");
            assert_eq!(seat.calls(), 1, "{code}: no retry");
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
            assert_eq!(edit_rejected_warnings(&events), 0, "{code}: no warning");
        }
    }

    /// A Responses refusal naming a parameter an operator payload extra put
    /// on the wire is not read as a refusal of that extra: the operator's
    /// edit is not undone, the request is not re-sent, and no edit-rejected
    /// warning is logged. Only the effort parameter localizes on this kind.
    ///
    /// Mutation check: localize every `error.param` on openai-responses
    /// again (drop the effort-param gate in `localize_responses`) -> red (the
    /// extra is reverted and the request re-sent, with the warning).
    #[tokio::test]
    async fn a_responses_refusal_of_an_operator_extra_does_not_revert_it() {
        for code in ["unsupported_parameter", "invalid_value"] {
            let seat = ResponsesSeat::new(SERVICE_TIER, code);
            let router = responses_router(&seat, SERVICE_TIER_EXTRA);

            let (dispatched, events) = Box::pin(dispatch(&router, responses_req(None))).await;

            assert_eq!(seat.calls(), 1, "{code}: sent once, no retry");
            assert!(dispatched.result.is_err(), "{code}: the refusal stands");
            assert_eq!(
                edit_rejected_warnings(&events),
                0,
                "{code}: no edit-rejected warning",
            );
            assert!(dispatched.meta.field_repairs.is_empty(), "{code}");
            assert!(dispatched.meta.learned_capabilities.is_empty(), "{code}");
        }
    }
}
