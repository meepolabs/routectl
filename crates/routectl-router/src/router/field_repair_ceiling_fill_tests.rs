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
    /// The model's configured `max_output_tokens`, the value the capture was
    /// refused for, so the seat refuses the very body the upstream refused.
    const FILLED: u32 = 1_000_000;
    const TOML: &str = "[wire_check]\nfirst_n = 8\nsample_one_in = 1\n\
         [retry]\nmax_attempts = 1\ninitial_backoff_ms = 0\njitter_ms = 0\n\n\
         [providers.p0]\nkind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n";

    /// The captured refusal, naming `limit` in place of [`NAMED_LIMIT`].
    fn messages_ceiling_rejection(limit: u32) -> Error {
        let fixture: serde_json::Value =
            serde_json::from_str(MESSAGES_CEILING_FIXTURE).expect("the fixture is JSON");
        let body = fixture["body"]
            .as_str()
            .expect("the fixture carries its body");
        let named = NAMED_LIMIT.to_string();
        assert_eq!(body.matches(&named).count(), 1, "premise: named once");
        Error::upstream_with_body(
            "fill-seat",
            400,
            routectl_core::UpstreamBody::new(body.replace(&named, &limit.to_string())),
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

    /// A seat that refuses every body whose `max_tokens` is above its
    /// limit, naming that limit, and serves the rest.
    struct FillSeat {
        limit: u32,
        attempts: Mutex<Vec<Attempt>>,
    }

    impl FillSeat {
        fn new() -> Arc<Self> {
            Self::with_limit(NAMED_LIMIT)
        }

        fn with_limit(limit: u32) -> Arc<Self> {
            Arc::new(Self {
                limit,
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
                .is_some_and(|sent| sent > u64::from(self.limit));
            self.attempts.lock().push(Attempt {
                req: req.clone(),
                body,
                egress_edits,
            });
            if above {
                return Err(messages_ceiling_rejection(self.limit));
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
        fill_router_configured(seat, FILLED)
    }

    /// A one-seat router whose model sets `[models.m0] max_output_tokens =
    /// configured`, stamped through the factory's own per-model stamp.
    fn fill_router_configured(seat: &Arc<FillSeat>, configured: u32) -> Router {
        let mut config: Config = toml::from_str(&format!(
            "{TOML}\n[models.m0]\nprovider = \"p0\"\nupstream = \"upstream\"\n\
             max_output_tokens = {configured}\n"
        ))
        .expect("valid test toml");
        config
            .aliases
            .insert(ALIAS.to_string(), AliasValue::Chain(vec!["m0".to_string()]));
        let model = crate::factory::apply_model_knobs(
            ResolvedModel::new(
                "m0",
                "p0",
                Arc::clone(seat) as Arc<dyn Provider>,
                "upstream",
            ),
            "m0",
            &config.models["m0"],
        );
        let mut router = Router::new(Arc::new(config));
        router.install_resolved_models(BTreeMap::from([("m0".to_string(), Arc::new(model))]));
        router.with_capability_writes_assumed_durable_for_tests()
    }

    /// Serve one request with no client limit and, when it committed a
    /// learned ceiling, acknowledge the durable write as the daemon does.
    async fn serve_and_acknowledge(router: &Router) {
        let done = router
            .complete_with_options(unlimited_req(None), RouterOptions::new())
            .await;
        if let Some(event) = done.meta.learned_capabilities.first() {
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
        }
    }

    /// With no client limit, two requests go out at the configured fill, a
    /// learned ceiling lowers it from the second request on, and a limit
    /// below the floor never does.
    ///
    /// Mutation check: compare the ceiling with `0 <` in place of the floor
    /// in `FieldSurface::clamp_lowers` -> red on the "below the floor" row.
    #[tokio::test]
    async fn the_fill_is_the_configured_value_lowered_by_a_learned_ceiling_to_the_floor() {
        use super::super::MIN_LEARNED_OUTPUT_CEILING as FLOOR;
        let rows: [(&str, u32, u32, Vec<u32>); 4] = [
            (
                "config value within the limit",
                8_000,
                NAMED_LIMIT,
                vec![8_000, 8_000],
            ),
            (
                "learned ceiling lowers the config value",
                FILLED,
                NAMED_LIMIT,
                vec![FILLED, NAMED_LIMIT, NAMED_LIMIT],
            ),
            (
                "learned ceiling at the floor lowers it",
                64_000,
                FLOOR,
                vec![64_000, FLOOR, FLOOR],
            ),
            (
                "a limit below the floor never lowers it",
                64_000,
                FLOOR - 1,
                vec![64_000, 64_000],
            ),
        ];

        for (name, configured, limit, sent) in rows {
            let seat = FillSeat::with_limit(limit);
            let router = fill_router_configured(&seat, configured);

            serve_and_acknowledge(&router).await;
            serve_and_acknowledge(&router).await;

            let expected: Vec<serde_json::Value> = sent.into_iter().map(|v| json!(v)).collect();
            assert_eq!(seat.sent_limits(), expected, "{name}");
            assert_eq!(seat.client_limits().iter().flatten().count(), 0, "{name}");
        }
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
/// limit the egress then sends, through the real per-model stamp.
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
    /// `upstream` and an optional `[models.m0] max_output_tokens`, stamped
    /// through the factory's own per-model stamp.
    async fn dispatched(
        provider_toml: &str,
        upstream: &str,
        configured: Option<u32>,
        client: Option<u32>,
    ) -> ChatRequest {
        dispatched_with(provider_toml, upstream, configured, client, None).await
    }

    /// [`dispatched`] for a request carrying `reasoning` canonically.
    async fn dispatched_with(
        provider_toml: &str,
        upstream: &str,
        configured: Option<u32>,
        client: Option<u32>,
        reasoning: Option<ReasoningConfig>,
    ) -> ChatRequest {
        let ceiling_line = configured.map_or_else(String::new, |tokens| {
            format!("max_output_tokens = {tokens}\n")
        });
        let toml_text = format!(
            "[providers.p0]\n{provider_toml}\n[models.m0]\nprovider = \"p0\"\nupstream = \"{upstream}\"\n{ceiling_line}"
        );
        let mut config: Config = toml::from_str(&toml_text).expect("valid test toml");
        config
            .aliases
            .insert(ALIAS.to_string(), AliasValue::Chain(vec!["m0".to_string()]));
        let seat = Arc::new(RecordingSeat {
            seen: Mutex::new(Vec::new()),
        });
        let model = crate::factory::apply_model_knobs(
            ResolvedModel::new("m0", "p0", Arc::clone(&seat) as Arc<dyn Provider>, upstream),
            "m0",
            &config.models["m0"],
        );
        let mut router = Router::new(Arc::new(config));
        router.install_resolved_models(BTreeMap::from([("m0".to_string(), Arc::new(model))]));
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

    /// A Bedrock Anthropic upstream id.
    #[cfg(feature = "bedrock")]
    const BEDROCK_UPSTREAM: &str = "anthropic.claude-opus-4-6-v1:0";

    const ANTHROPIC: &str = "kind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n";
    const OPENAI: &str = "kind = \"openai-compat\"\nbase_url = \"https://example.test/v1\"\napi_key_ref = \"literal:k\"\n";
    /// An Anthropic upstream id.
    const CLAUDE: &str = "claude-opus-4-6";

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
            ("anthropic-api, unset", ANTHROPIC, CLAUDE, None, true),
            (
                "anthropic-api, configured",
                ANTHROPIC,
                CLAUDE,
                Some(8000),
                true,
            ),
            ("openai-compat, no override", OPENAI, "gpt-x", None, false),
            (
                "openai-compat, configured",
                OPENAI,
                "gpt-x",
                Some(8000),
                false,
            ),
        ];

        for (name, provider, upstream, configured, present) in rows {
            let req = dispatched(provider, upstream, configured, None).await;

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
            ("invoke, unset", bedrock("invoke"), None, true),
            ("invoke, configured", bedrock("invoke"), Some(8000), true),
            ("converse, unset", bedrock("converse"), None, false),
            (
                "converse, configured",
                bedrock("converse"),
                Some(8000),
                false,
            ),
        ];

        for (name, provider, configured, present) in rows {
            let req = dispatched(&provider, BEDROCK_UPSTREAM, configured, None).await;

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
    /// model's configured ceiling, else the baseline, byte for byte.
    ///
    /// Mutation check: drop the `max_output_tokens` stamp from
    /// `apply_model_knobs` -> red on the "configured" row.
    #[tokio::test]
    async fn the_anthropic_body_limit_is_the_client_then_configured_then_baseline_value() {
        let rows = [
            ("client", CLAUDE, Some(8000), Some(1234), 1234),
            ("configured", CLAUDE, Some(8000), None, 8000),
            ("neither", CLAUDE, None, None, 64_000),
        ];

        for (name, upstream, configured, client, sent) in rows {
            let req = dispatched(ANTHROPIC, upstream, configured, client).await;

            assert_eq!(anthropic_body_limit(&req), json!(sent), "{name}");
        }
    }

    #[cfg(feature = "bedrock")]
    #[tokio::test]
    async fn the_invoke_body_limit_is_the_configured_then_baseline_value() {
        use routectl_providers::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds};
        let provider = "kind = \"bedrock\"\nregion = \"us-east-1\"\ncreds = { kind = \"default-chain\" }\napi_shape = \"invoke\"\n";
        let rows = [
            ("configured", BEDROCK_UPSTREAM, Some(8000), 8000),
            ("baseline", BEDROCK_UPSTREAM, None, 64_000),
        ];

        for (name, upstream, configured, sent) in rows {
            let req = dispatched(provider, upstream, configured, None).await;
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

    /// With no client limit, Converse sizes its legacy thinking window from
    /// the model's configured ceiling and sends that same ceiling as
    /// `maxTokens`, so the budget sits below the limit the upstream sees.
    ///
    /// Mutation checks: zero the slot on an egress that does not fill
    /// `max_tokens` -> red (the window widens to the egress baseline); make
    /// Converse forward the omitted client limit -> red (no `maxTokens`).
    #[cfg(feature = "bedrock")]
    #[tokio::test]
    async fn the_converse_thinking_budget_fits_the_configured_ceiling_with_no_client_limit() {
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
                BEDROCK_UPSTREAM,
                Some(8000),
                None,
                Some(reasoning),
            )
            .await;
            let cfg = BedrockConfig {
                id: "bedrock:p0".into(),
                region: "us-east-1".into(),
                model_id: BEDROCK_UPSTREAM.into(),
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
            let budget = body["additionalModelRequestFields"]["thinking"]["budget_tokens"]
                .as_u64()
                .expect("a thinking budget ships");
            let sent = body["inferenceConfig"]["maxTokens"]
                .as_u64()
                .unwrap_or_else(|| panic!("{name}: no maxTokens: {}", body["inferenceConfig"]));
            assert_eq!(sent, 8000, "{name}: the configured ceiling is sent");
            assert!(budget < sent, "{name}: budget {budget} fits {sent}");
        }
    }
}
