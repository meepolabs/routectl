// The max_tokens ceiling row against the limit routectl fills for a client
// that sent none: an anthropic-api seat assembles the real Messages body, so
// the refused value is routectl's own fill, the rejection matches the egress's
// own declaration of that default, and every assertion reads the body an
// upstream would receive.
// An `include!`d FRAGMENT of `field_repair_tests.rs`.

mod ceiling_fill {
    use super::*;

    use crate::wire_check_state::FieldRepairOutcome;
    use routectl_core::wire_check::{EditKind, EditSite, IngressDialect, IngressWire, Leg};

    /// The committed Messages capture of a model refusing a `max_tokens` above
    /// its own limit, served verbatim: the parser reads [`NAMED_LIMIT`] from
    /// it.
    const MESSAGES_CEILING_FIXTURE: &str = include_str!(
        "../../tests/fixtures/rejections/anthropic-api/max-tokens-above-model-limit-messages.json"
    );
    /// The limit the capture names.
    const NAMED_LIMIT: u32 = 128_000;
    /// The model's stamped `max_output_tokens`, the value the capture was
    /// refused for, so the seat refuses the very body the upstream refused.
    const FILLED: u32 = 1_000_000;
    const TOML: &str = "[wire_check]\nfirst_n = 8\nsample_one_in = 1\n\
         [retry]\nmax_attempts = 1\ninitial_backoff_ms = 0\njitter_ms = 0\n\n\
         [providers.p0]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n";

    fn messages_ceiling_rejection() -> Error {
        let fixture: serde_json::Value =
            serde_json::from_str(MESSAGES_CEILING_FIXTURE).expect("the fixture is JSON");
        Error::upstream_with_body(
            "fill-seat",
            400,
            routectl_core::UpstreamBody::new(
                fixture["body"]
                    .as_str()
                    .expect("the fixture carries its body"),
            ),
            None,
            Some("invalid_request_error".to_string()),
            None,
        )
    }

    /// One attempt as the seat saw it: the request handed to the egress, the
    /// body the egress assembled from it, and the request edits the egress
    /// declared while assembling it.
    struct Attempt {
        req: ChatRequest,
        body: serde_json::Value,
        egress_edits: Vec<routectl_core::wire_check::DeclaredEdit>,
    }

    /// A seat that refuses every body whose `max_tokens` is above
    /// [`NAMED_LIMIT`] and serves the rest.
    struct FillSeat {
        attempts: Mutex<Vec<Attempt>>,
    }

    impl FillSeat {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                attempts: Mutex::new(Vec::new()),
            })
        }

        fn sent_limits(&self) -> Vec<serde_json::Value> {
            self.attempts
                .lock()
                .iter()
                .map(|attempt| attempt.body["max_tokens"].clone())
                .collect()
        }

        fn client_limits(&self) -> Vec<Option<u32>> {
            self.attempts
                .lock()
                .iter()
                .map(|attempt| attempt.req.max_tokens)
                .collect()
        }

        fn bodies(&self) -> Vec<serde_json::Value> {
            self.attempts
                .lock()
                .iter()
                .map(|attempt| attempt.body.clone())
                .collect()
        }

        fn answer(&self, req: &ChatRequest) -> Result<()> {
            let egress = routectl_providers::anthropic_api::AnthropicApiProvider::new(
                routectl_providers::anthropic_api::AnthropicApiConfig::new("fill-seat", "k"),
            );
            let mut observed = req.clone();
            let probe = routectl_core::wire_check::WireTap::admitted();
            observed.routectl_internal.wire_tap = Some(probe.clone());
            let egress_edits = {
                egress.normalize_request(&observed)?;
                probe.take_request().edits(Leg::Request).to_vec()
            };
            let mut body = egress.normalize_request(req)?;
            if let Some(object) = body.as_object_mut() {
                object.remove("stream");
            }
            if let Some(tap) = req.routectl_internal.wire_tap.as_ref() {
                tap.deposit(serde_json::to_vec(&body).expect("serialize").into());
            }
            let above = body["max_tokens"]
                .as_u64()
                .is_some_and(|sent| sent > u64::from(NAMED_LIMIT));
            self.attempts.lock().push(Attempt {
                req: req.clone(),
                body,
                egress_edits,
            });
            if above {
                return Err(messages_ceiling_rejection());
            }
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl Provider for FillSeat {
        fn id(&self) -> &'static str {
            "fill-seat"
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response("fill-seat", "unused"))
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

    fn fill_router(seat: &Arc<FillSeat>) -> Router {
        let mut config: Config = toml::from_str(TOML).expect("valid test toml");
        config
            .aliases
            .insert(ALIAS.to_string(), AliasValue::Chain(vec!["m0".to_string()]));
        let mut router = Router::new(Arc::new(config));
        router.install_resolved_models(BTreeMap::from([(
            "m0".to_string(),
            Arc::new(
                ResolvedModel::new(
                    "m0",
                    "p0",
                    Arc::clone(seat) as Arc<dyn Provider>,
                    "upstream",
                )
                .with_max_output_tokens(FILLED),
            ),
        )]));
        router.with_capability_writes_assumed_durable_for_tests()
    }

    /// An Anthropic client's request with no `max_tokens`, client bytes
    /// included for the wire check. `reasoning` rides through canonically.
    fn unlimited_req(reasoning: Option<ReasoningConfig>) -> ChatRequest {
        let mut req = req_on(ALIAS);
        req.max_tokens = None;
        req.reasoning = reasoning;
        req.routectl_internal.anthropic_thinking_display = None;
        req.routectl_internal.anthropic_thinking = routectl_core::AnthropicThinking::NotCaptured;
        req.routectl_internal.ingress_wire = Some(IngressWire {
            dialect: IngressDialect::Anthropic,
            client_version: None,
            bytes: serde_json::to_vec(&json!({
                "model": ALIAS,
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

    /// Mutation check: restore `Proceed` for an unreverted required-field
    /// default in `answer_declared` -> red (no retry, nothing learned); record
    /// every ceiling repair lossy again -> red on the action and the outcome.
    #[tokio::test]
    async fn a_refused_fill_retries_at_the_named_limit_and_learns_it() {
        for stream in [false, true] {
            let seat = FillSeat::new();
            let router = fill_router(&seat);

            let (meta, served) = if stream {
                let opened = router
                    .stream_with_options(unlimited_req(None), RouterOptions::new())
                    .await;
                let served = match opened.result {
                    Ok(body) => body
                        .collect::<Vec<_>>()
                        .await
                        .iter()
                        .all(std::result::Result::is_ok),
                    Err(_) => false,
                };
                (opened.meta, served)
            } else {
                let done = router
                    .complete_with_options(unlimited_req(None), RouterOptions::new())
                    .await;
                (done.meta, done.result.is_ok())
            };

            assert!(served, "stream={stream}");
            assert_eq!(
                seat.sent_limits(),
                vec![json!(FILLED), json!(NAMED_LIMIT)],
                "stream={stream}: the fill is refused, the retry goes out at the named limit",
            );
            assert_eq!(
                seat.client_limits(),
                vec![None, None],
                "stream={stream}: the clamp writes the fill, never a client value",
            );
            let repair = &meta.field_repairs[..];
            assert_eq!(repair.len(), 1, "stream={stream}");
            assert!(repair[0].repair_succeeded, "stream={stream}");
            assert_eq!(repair[0].field_path, "max_tokens");
            assert_eq!(
                repair[0].action,
                super::super::FIELD_ACTION_CEILING_FILL_REPAIR,
                "stream={stream}",
            );
            assert!(repair[0].learned, "stream={stream}");
            assert_eq!(
                router.field_repair_counters().repair_lossy,
                0,
                "stream={stream}: lowering routectl's own fill is lossless",
            );
            let event = meta
                .learned_capabilities
                .first()
                .expect("the retry's success commits the ceiling");
            assert_eq!(
                event
                    .value
                    .as_ref()
                    .map(crate::VerdictValue::encode)
                    .as_deref(),
                Some("ceiling:128000"),
                "stream={stream}",
            );
            assert_eq!(
                rejection_outcomes(&router),
                vec![FieldRepairOutcome::Repaired],
                "stream={stream}: the client sent no limit, so the served request is what it asked",
            );
        }
    }

    /// The egress keeps declaring its default on the retry, and the repair
    /// adds no second declaration at that pointer, so a further rejection of
    /// it still matches one edit.
    #[tokio::test]
    async fn the_clamped_fill_stays_the_egress_declared_default() {
        let seat = FillSeat::new();
        let router = fill_router(&seat);

        let done = router
            .complete_with_options(unlimited_req(None), RouterOptions::new())
            .await;

        assert!(done.result.is_ok(), "premise: the clamped retry serves");
        let attempts = seat.attempts.lock();
        let at_max_tokens = |edits: &[routectl_core::wire_check::DeclaredEdit]| {
            edits
                .iter()
                .filter(|edit| edit.path() == "/max_tokens")
                .map(|edit| (edit.site(), edit.kind()))
                .collect::<Vec<_>>()
        };
        let retry = &attempts[1];
        assert_eq!(
            at_max_tokens(&retry.egress_edits),
            vec![(EditSite::RequiredFieldDefault, EditKind::Add)],
            "the egress still declares its default on the retry",
        );
        assert_eq!(
            at_max_tokens(&retry.req.routectl_internal.declared_edits),
            vec![],
            "the repair adds no declaration at the egress's own pointer",
        );
    }

    /// The legacy thinking budget is fitted to the clamped fill: the retry's
    /// budget stays below its `max_tokens`.
    #[tokio::test]
    async fn the_thinking_budget_fits_the_clamped_fill() {
        let seat = FillSeat::new();
        let router = fill_router(&seat);
        let reasoning = ReasoningConfig {
            effort: None,
            max_tokens: Some(200_000),
            exclude: None,
            enabled: Some(true),
        };

        let done = router
            .complete_with_options(unlimited_req(Some(reasoning)), RouterOptions::new())
            .await;

        assert!(done.result.is_ok(), "premise: the clamped retry serves");
        let bodies = seat.bodies();
        assert_eq!(
            bodies[0]["thinking"]["budget_tokens"],
            json!(200_000),
            "premise: under the fill the budget fits as asked",
        );
        assert_eq!(
            bodies[1]["thinking"]["budget_tokens"],
            json!(NAMED_LIMIT - 1),
            "the retry's budget is fitted under the clamped limit",
        );
    }

    /// Once the clamped fill commits, the next request's fill is lowered
    /// before it is sent: no rejection, no repair, and still no client value.
    ///
    /// Mutation check: make `clamp_lowers` read only `req.max_tokens` again
    /// -> red (the fill is never pre-flighted, so the second request draws a
    /// refusal).
    #[tokio::test]
    async fn the_request_after_a_committed_fill_ceiling_goes_out_at_the_ceiling() {
        let seat = FillSeat::new();
        let router = fill_router(&seat);
        let first = router
            .complete_with_options(unlimited_req(None), RouterOptions::new())
            .await;
        assert!(first.result.is_ok(), "premise: the clamped retry serves");
        let event = first
            .meta
            .learned_capabilities
            .first()
            .expect("premise: the repair committed a ceiling")
            .clone();
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

        let second = router
            .complete_with_options(unlimited_req(None), RouterOptions::new())
            .await;

        assert!(second.result.is_ok());
        assert_eq!(
            seat.sent_limits(),
            vec![json!(FILLED), json!(NAMED_LIMIT), json!(NAMED_LIMIT)],
            "the second request is sent once, at the ceiling",
        );
        assert_eq!(seat.client_limits(), vec![None, None, None]);
        assert!(second.meta.field_repairs.is_empty(), "no reactive repair");
        let record = second
            .meta
            .field_preflight
            .iter()
            .find(|record| record.field_path == Some("max_tokens"))
            .expect("the filled limit was planned");
        assert!(record.acted);
        assert_eq!(
            record.reason,
            crate::router::field_preflight::FIELD_PREFLIGHT_ACTION_CEILING,
        );
    }
}

/// What reaches each egress for a client that sent no `max_tokens`, and the
/// limit the egress then sends, through the real chain stamp and catalog fill.
mod fill_presence {
    use super::*;

    /// A seat that records the request each attempt hands it and serves.
    struct RecordingSeat {
        seen: Mutex<Vec<ChatRequest>>,
    }

    #[async_trait::async_trait]
    impl Provider for RecordingSeat {
        fn id(&self) -> &'static str {
            "recording-seat"
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response("recording-seat", "unused"))
        }
        async fn complete(&self, req: ChatRequest) -> Result<ChatResponse> {
            self.seen.lock().push(req);
            Ok(success_response())
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!("only the complete surface is driven")
        }
    }

    /// The request one dispatch hands a seat on `provider_toml`, its model
    /// `upstream` and an optional ceiling already stamped on the resolved
    /// model (which the catalog fill then leaves alone), after the catalog
    /// fill stamps the resolved model.
    async fn dispatched(
        provider_toml: &str,
        upstream: &str,
        stamped: Option<u32>,
        client: Option<u32>,
    ) -> ChatRequest {
        dispatched_with(provider_toml, upstream, stamped, client, None).await
    }

    /// [`dispatched`] for a request carrying `reasoning` canonically.
    async fn dispatched_with(
        provider_toml: &str,
        upstream: &str,
        stamped: Option<u32>,
        client: Option<u32>,
        reasoning: Option<ReasoningConfig>,
    ) -> ChatRequest {
        let toml_text = format!(
            "[providers.p0]\n{provider_toml}\n[models.m0]\nprovider = \"p0\"\nupstream = \"{upstream}\"\n"
        );
        let mut config: Config = toml::from_str(&toml_text).expect("valid test toml");
        config
            .aliases
            .insert(ALIAS.to_string(), AliasValue::Chain(vec!["m0".to_string()]));
        let seat = Arc::new(RecordingSeat {
            seen: Mutex::new(Vec::new()),
        });
        let mut model =
            ResolvedModel::new("m0", "p0", Arc::clone(&seat) as Arc<dyn Provider>, upstream);
        if let Some(tokens) = stamped {
            model = model.with_max_output_tokens(tokens);
        }
        let models = crate::factory::apply_catalog_overlay(
            BTreeMap::from([("m0".to_string(), Arc::new(model))]),
            &config,
            &overlay_ceiling(),
        );
        let mut router = Router::new(Arc::new(config));
        router.install_resolved_models(models);
        let mut req = req_on(ALIAS);
        req.max_tokens = client;
        req.reasoning = reasoning;
        req.routectl_internal.anthropic_thinking_display = None;
        req.routectl_internal.anthropic_thinking = routectl_core::AnthropicThinking::NotCaptured;
        router
            .complete_with_options(req, RouterOptions::new())
            .await
            .result
            .expect("the seat serves");
        let seen = seat.seen.lock();
        assert_eq!(seen.len(), 1, "one attempt");
        seen[0].clone()
    }

    /// A Bedrock upstream whose output ceiling only the overlay below names,
    /// so a body that sends it can only have read the catalog.
    const OVERLAID_BEDROCK: &str = "anthropic.claude-overlaid-v1:0";
    const OVERLAID_CEILING: u32 = 32_000;

    /// The catalog overlay naming [`OVERLAID_CEILING`] for
    /// [`OVERLAID_BEDROCK`] and nothing else.
    fn overlay_ceiling() -> crate::catalog_overlay::CatalogOverlay {
        let mut overlay = crate::catalog_overlay::CatalogOverlay::default();
        overlay.cells.insert(
            format!("bedrock:{OVERLAID_BEDROCK}"),
            Some(crate::catalog_overlay::OverlayCell {
                source: crate::catalog_overlay::OverlaySource::User,
                verified_at: "2026-01-01".to_string(),
                wm: None,
                rm: None,
                ttl_seconds: None,
                min_prefix_tokens: None,
                max_context_tokens: None,
                max_output_tokens: Some(OVERLAID_CEILING),
                input_cost_per_token: None,
                output_cost_per_token: None,
                capabilities: None,
            }),
        );
        overlay
    }

    const ANTHROPIC: &str = "kind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n";
    const OPENAI: &str = "kind = \"openai-compat\"\nbase_url = \"https://example.test/v1\"\napi_key_ref = \"literal:k\"\n";
    /// An Anthropic upstream the baked catalog gives a 128000 ceiling.
    const CATALOGED: &str = "claude-opus-4-6";

    /// Mutation check: stamp the slot on every egress again -> red on the
    /// openai-compat rows; present only on a client value -> red on the
    /// anthropic-api rows.
    #[tokio::test]
    async fn with_no_client_value_max_tokens_is_present_only_where_the_egress_fills_it() {
        let rows: Vec<(&str, &str, &str, Option<u32>, bool)> = vec![
            (
                "anthropic-api, baseline",
                ANTHROPIC,
                "claude-unlisted",
                None,
                true,
            ),
            ("anthropic-api, catalog", ANTHROPIC, CATALOGED, None, true),
            (
                "anthropic-api, stamped",
                ANTHROPIC,
                CATALOGED,
                Some(8000),
                true,
            ),
            ("openai-compat, no override", OPENAI, "gpt-x", None, false),
            ("openai-compat, stamped", OPENAI, "gpt-x", Some(8000), false),
        ];

        for (name, provider, upstream, stamped, present) in rows {
            let req = dispatched(provider, upstream, stamped, None).await;

            assert_eq!(
                super::super::FieldSurface::MaxTokens.present_in(&req),
                present,
                "{name}: slot {}",
                req.routectl_internal.max_output_tokens,
            );
        }
    }

    #[cfg(feature = "bedrock")]
    #[tokio::test]
    async fn with_no_client_value_max_tokens_is_present_on_invoke_and_absent_on_converse() {
        let bedrock = |shape: &str| {
            format!(
                "kind = \"bedrock\"\nregion = \"us-east-1\"\ncreds = {{ kind = \"default-chain\" }}\napi_shape = \"{shape}\"\n"
            )
        };
        let rows = [
            ("invoke, catalog", bedrock("invoke"), None, true),
            ("invoke, stamped", bedrock("invoke"), Some(8000), true),
            ("converse, catalog", bedrock("converse"), None, false),
            ("converse, stamped", bedrock("converse"), Some(8000), false),
        ];

        for (name, provider, stamped, present) in rows {
            let req = dispatched(&provider, OVERLAID_BEDROCK, stamped, None).await;

            assert_eq!(
                super::super::FieldSurface::MaxTokens.present_in(&req),
                present,
                "{name}: slot {}",
                req.routectl_internal.max_output_tokens,
            );
        }
    }

    /// The body `max_tokens` the anthropic-api egress assembles from `req`.
    fn anthropic_body_limit(req: &ChatRequest) -> serde_json::Value {
        let egress = routectl_providers::anthropic_api::AnthropicApiProvider::new(
            routectl_providers::anthropic_api::AnthropicApiConfig::new("recording-seat", "k"),
        );
        egress.normalize_request(req).expect("the body assembles")["max_tokens"].clone()
    }

    /// The fill reproduces the egress's own ladder: a client value, else the
    /// model's stamped ceiling, else the catalog's, else the baseline, byte
    /// for byte.
    #[tokio::test]
    async fn the_anthropic_body_limit_is_the_stamped_then_catalog_then_baseline_value() {
        let rows = [
            ("client", CATALOGED, Some(8000), Some(1234), 1234),
            ("stamped", CATALOGED, Some(8000), None, 8000),
            ("catalog", CATALOGED, None, None, 128_000),
            ("baseline", "claude-unlisted", None, None, 64_000),
        ];

        for (name, upstream, stamped, client, sent) in rows {
            let req = dispatched(ANTHROPIC, upstream, stamped, client).await;

            assert_eq!(anthropic_body_limit(&req), json!(sent), "{name}");
        }
    }

    #[cfg(feature = "bedrock")]
    #[tokio::test]
    async fn the_invoke_body_limit_is_the_stamped_then_catalog_then_baseline_value() {
        use routectl_providers::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};
        let provider = "kind = \"bedrock\"\nregion = \"us-east-1\"\ncreds = { kind = \"default-chain\" }\napi_shape = \"invoke\"\n";
        let rows = [
            ("stamped", OVERLAID_BEDROCK, Some(8000), 8000),
            ("catalog", OVERLAID_BEDROCK, None, OVERLAID_CEILING),
            ("baseline", "anthropic.claude-unlisted-v1:0", None, 64_000),
        ];

        for (name, upstream, stamped, sent) in rows {
            let req = dispatched(provider, upstream, stamped, None).await;
            let cfg = BedrockConfig {
                id: "bedrock:p0".into(),
                region: "us-east-1".into(),
                model_id: upstream.into(),
                api_shape: BedrockApiShape::Invoke,
                creds: BedrockCreds::BearerKey {
                    key: ["invoke", "seat", "test"].join("-"),
                },
                user_agent: None,
                header_extras: Vec::new(),
                anthropic_beta: Vec::new(),
                additional_model_request_fields: None,
            };

            let body = routectl_providers::bedrock::invoke::normalize_request(&cfg, &req)
                .expect("the body assembles");

            assert_eq!(body["max_tokens"], json!(sent), "{name}");
        }
    }

    /// Converse sends no limit the client did not, yet its legacy thinking
    /// window still fits the model's ceiling: the carrier reaches the budget
    /// with no fill marker on the request.
    ///
    /// Mutation check: zero the slot on an egress that does not fill
    /// `max_tokens` -> red (the window widens to the egress baseline).
    #[cfg(feature = "bedrock")]
    #[tokio::test]
    async fn the_converse_thinking_budget_fits_the_stamped_ceiling_with_no_client_limit() {
        use routectl_providers::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};
        let provider = "kind = \"bedrock\"\nregion = \"us-east-1\"\ncreds = { kind = \"default-chain\" }\napi_shape = \"converse\"\n";
        let rows = [
            ("enabled, no effort", None, 4000),
            ("explicit budget above the ceiling", Some(48_000), 7999),
        ];

        for (name, budget, expected) in rows {
            let reasoning = ReasoningConfig {
                effort: None,
                max_tokens: budget,
                exclude: None,
                enabled: Some(true),
            };
            let req = dispatched_with(
                provider,
                OVERLAID_BEDROCK,
                Some(8000),
                None,
                Some(reasoning),
            )
            .await;
            let cfg = BedrockConfig {
                id: "bedrock:p0".into(),
                region: "us-east-1".into(),
                model_id: OVERLAID_BEDROCK.into(),
                api_shape: BedrockApiShape::Converse,
                creds: BedrockCreds::BearerKey {
                    key: ["converse", "seat", "test"].join("-"),
                },
                user_agent: None,
                header_extras: Vec::new(),
                anthropic_beta: Vec::new(),
                additional_model_request_fields: None,
            };

            let body = routectl_providers::bedrock::converse::normalize_request(&cfg, &req)
                .expect("the body assembles");

            assert_eq!(
                body["additionalModelRequestFields"]["thinking"]["budget_tokens"],
                json!(expected),
                "{name}",
            );
            assert!(
                body["inferenceConfig"].get("maxTokens").is_none(),
                "{name}: Converse sends no limit the client did not",
            );
        }
    }
}
