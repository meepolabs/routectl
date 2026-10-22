// Repair chaining through the dispatch walks: a repaired retry the upstream
// refuses for a DIFFERENT field commits the earlier steps and repairs the new
// field on the same target; the same field, an inferred or absent-path
// attribution, a declared-edit undo, a target change and an exhausted budget
// each commit only what was proven. Seats run the real egress body assembly,
// so every assertion reads the body an upstream would receive.
// An `include!`d FRAGMENT of `field_repair_tests.rs`, sharing the thinking
// seat helpers of `field_repair_thinking_type_tests.rs` and
// `field_repair_thinking_disabled_tests.rs`.

mod repair_chain {
    use super::*;

    use crate::VerdictValue;
    use routectl_providers::openai_compat::request::normalize;
    use routectl_providers::openai_compat::{HistoryReasoning, ReasoningDialect};

    const MAX_TOKENS_PATH: &str = "max_tokens";
    /// The committed anthropic-api capture of `max_tokens` above the model's
    /// output limit.
    const MAX_TOKENS_FIXTURE: &str = include_str!(
        "../../tests/fixtures/rejections/anthropic-api/max-tokens-above-model-limit-messages.json"
    );
    /// The value that capture says was sent, and the limit it names.
    const SENT_MAX_TOKENS: u32 = 1_000_000;
    const MODEL_LIMIT: u32 = 128_000;
    /// An upstream model name no openai-compat reasoning profile matches, so
    /// the egress forwards every sampling parameter the client sent.
    const UNPROFILED_MODEL: &str = "acme-chat-1";
    const OPENAI_COMPAT_TOML: &str = "kind = \"openai-compat\"\n\
        base_url = \"https://example.test/v1\"\napi_key_ref = \"literal:k\"\n";
    const ONE_ATTEMPT: &str = "[retry]\nmax_attempts = 1\ninitial_backoff_ms = 0\njitter_ms = 0\n";
    /// Room for one same-target retry after a repaired attempt.
    const THREE_ATTEMPTS: &str =
        "[retry]\nmax_attempts = 3\ninitial_backoff_ms = 0\njitter_ms = 0\n";
    /// The `stop` the requests that need a carried, row-less field send.
    const STOP: &str = "zz-stop";

    /// How a seat answers one attempt, given the body it assembled and the
    /// attempt's zero-based position; `None` serves.
    type Respond = fn(&serde_json::Value, usize) -> Option<Error>;

    #[derive(Clone, Copy)]
    enum Egress {
        Anthropic,
        OpenAiCompat,
    }

    /// A seat that assembles each attempt's body with the real egress,
    /// deposits it into the attempt's wire tap, and answers per `respond`.
    struct ChainSeat {
        egress: Egress,
        respond: Respond,
        bodies: Mutex<Vec<serde_json::Value>>,
    }

    impl ChainSeat {
        fn new(egress: Egress, respond: Respond) -> Arc<Self> {
            Arc::new(Self {
                egress,
                respond,
                bodies: Mutex::new(Vec::new()),
            })
        }

        fn bodies(&self) -> Vec<serde_json::Value> {
            self.bodies.lock().clone()
        }

        fn calls(&self) -> usize {
            self.bodies.lock().len()
        }

        fn body_of(&self, req: &ChatRequest) -> Result<serde_json::Value> {
            match self.egress {
                Egress::Anthropic => {
                    let egress = routectl_providers::anthropic_api::AnthropicApiProvider::new(
                        routectl_providers::anthropic_api::AnthropicApiConfig::new(
                            "chain-seat",
                            "k",
                        ),
                    );
                    let mut body = egress.normalize_request(req)?;
                    if let Some(object) = body.as_object_mut() {
                        object.remove("stream");
                    }
                    Ok(body)
                }
                Egress::OpenAiCompat => normalize(
                    "chain-seat",
                    req,
                    ReasoningDialect::OpenAi,
                    HistoryReasoning::default(),
                    None,
                    false,
                ),
            }
        }

        fn answer(&self, req: &ChatRequest) -> Result<()> {
            let body = self.body_of(req)?;
            if let Some(tap) = req.routectl_internal.wire_tap.as_ref() {
                tap.deposit(serde_json::to_vec(&body).expect("serialize").into());
            }
            let position = {
                let mut bodies = self.bodies.lock();
                bodies.push(body.clone());
                bodies.len() - 1
            };
            (self.respond)(&body, position).map_or(Ok(()), Err)
        }
    }

    #[async_trait::async_trait]
    impl Provider for ChainSeat {
        fn id(&self) -> &'static str {
            "chain-seat"
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response("chain-seat", "unused"))
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
        async fn count_tokens(&self, req: ChatRequest) -> Result<TokenCount> {
            self.answer(&req).map(|()| TokenCount {
                input_tokens: 7,
                ..Default::default()
            })
        }
    }

    /// A router over one seat per `(provider_toml, seat)`, in chain order,
    /// with `extra_toml` prepended and durable capability writes assumed.
    fn chain_router(seats: &[(&str, &Arc<ChainSeat>)], extra_toml: &str) -> Router {
        let mut toml_text = String::from(extra_toml);
        for (idx, (provider_toml, _)) in seats.iter().enumerate() {
            toml_text.push_str(&format!("\n[providers.p{idx}]\n{provider_toml}"));
        }
        let mut config: Config = toml::from_str(&toml_text).expect("valid test toml");
        config.aliases.insert(
            ALIAS.to_string(),
            AliasValue::Chain((0..seats.len()).map(|idx| format!("m{idx}")).collect()),
        );
        let mut router = Router::new(Arc::new(config));
        router.install_resolved_models(
            seats
                .iter()
                .enumerate()
                .map(|(idx, (_, seat))| {
                    let upstream = match seat.egress {
                        Egress::Anthropic => "upstream",
                        Egress::OpenAiCompat => UNPROFILED_MODEL,
                    };
                    (
                        format!("m{idx}"),
                        Arc::new(ResolvedModel::new(
                            format!("m{idx}"),
                            format!("p{idx}"),
                            Arc::clone(*seat) as Arc<dyn Provider>,
                            upstream,
                        )),
                    )
                })
                .collect(),
        );
        router.with_capability_writes_assumed_durable_for_tests()
    }

    fn anthropic_router(seat: &Arc<ChainSeat>, extra_toml: &str) -> Router {
        chain_router(&[(ANTHROPIC_API_TOML, seat)], extra_toml)
    }

    fn wire_checked(retry: &str) -> String {
        format!("{WIRE_CHECK_EVERY}{retry}")
    }

    // -----------------------------------------------------------------------
    // Rejections
    // -----------------------------------------------------------------------

    /// The captured max_tokens rejection exactly as the upstream returned it.
    fn max_tokens_rejection() -> Error {
        Error::upstream("chain-seat", 400, disabled_fixture_body(MAX_TOKENS_FIXTURE))
    }

    /// An anthropic-api `invalid_request_error` 400 carrying `message`.
    fn anthropic_rejection(message: &str) -> Error {
        Error::upstream(
            "chain-seat",
            400,
            json!({"type": "error", "error": {"type": "invalid_request_error", "message": message}})
                .to_string(),
        )
    }

    /// A 400 naming nothing.
    fn unlocalized_rejection() -> Error {
        anthropic_rejection("the request is malformed")
    }

    fn unavailable() -> Error {
        Error::upstream(
            "chain-seat",
            503,
            r#"{"error":{"type":"api_error","message":"unavailable"}}"#,
        )
    }

    /// The captured disabled refusal with its two thinking types swapped: a
    /// refusal of the adaptive value the rewrite sent. Synthesized from the
    /// committed template, so it parses exactly as a real refusal of that
    /// value would; no capture of it exists.
    fn adaptive_refused() -> Error {
        anthropic_rejection(
            "\"thinking.type.adaptive\" is not supported for this model. Use \
             \"thinking.type.disabled\" and \"output_config.effort\" to control thinking behavior.",
        )
    }

    /// The grounded request-validation refusal of a field the body carried
    /// and the closed table has no row for.
    fn stop_refused() -> Error {
        anthropic_rejection("stop_sequences: Extra inputs are not permitted")
    }

    /// The same field named by the ungrounded quoted template, which earns
    /// only an inferred attribution.
    fn stop_refused_inferred() -> Error {
        anthropic_rejection("\"stop_sequences\" is not supported on this model.")
    }

    /// A grounded refusal of a path the body did not carry.
    fn absent_top_k_refused() -> Error {
        anthropic_rejection("top_k: Extra inputs are not permitted")
    }

    fn thinking_type(body: &serde_json::Value) -> Option<&str> {
        body["thinking"]["type"].as_str()
    }

    fn sent_max_tokens(body: &serde_json::Value) -> Option<u64> {
        body["max_tokens"].as_u64()
    }

    /// Refuse disabled thinking, then a max_tokens above the limit, else
    /// serve.
    fn two_key(body: &serde_json::Value, _: usize) -> Option<Error> {
        if thinking_type(body) == Some("disabled") {
            return Some(anthropic_disabled_rejection());
        }
        sent_max_tokens(body)
            .is_some_and(|sent| sent > u64::from(MODEL_LIMIT))
            .then(max_tokens_rejection)
    }

    /// Refuse disabled thinking, then answer the rewrite with `then`.
    fn disabled_then(body: &serde_json::Value, then: fn() -> Error) -> Option<Error> {
        Some(if thinking_type(body) == Some("disabled") {
            anthropic_disabled_rejection()
        } else {
            then()
        })
    }

    // -----------------------------------------------------------------------
    // Requests
    // -----------------------------------------------------------------------

    /// An Anthropic client's disabled thinking with an explicit max_tokens of
    /// `max_tokens` and, when `stop`, a stop sequence.
    fn anthropic_req(max_tokens: u32, stop: bool) -> ChatRequest {
        let mut req = disabled_req(None);
        req.max_tokens = Some(max_tokens);
        let mut client = disabled_client_body(None);
        client["max_tokens"] = json!(max_tokens);
        if stop {
            req.stop = Some(vec![STOP.to_string()]);
            client["stop_sequences"] = json!([STOP]);
        }
        if let Some(wire) = req.routectl_internal.ingress_wire.as_mut() {
            wire.bytes = serde_json::to_vec(&client).expect("serialize").into();
        }
        req
    }

    fn two_key_req() -> ChatRequest {
        anthropic_req(SENT_MAX_TOKENS, false)
    }

    fn verdicts(meta: &crate::router::DispatchMeta) -> Vec<(String, Option<VerdictValue>)> {
        meta.learned_capabilities
            .iter()
            .map(|event| (event.capability_key.clone(), event.value))
            .collect()
    }

    fn key(path: &str) -> String {
        crate::field_capability::field_capability_key(path).expect("a grounded path mints a key")
    }

    fn two_key_verdicts() -> Vec<(String, Option<VerdictValue>)> {
        vec![
            (
                thinking_disabled_key(),
                Some(VerdictValue::Shape("thinking.type.adaptive")),
            ),
            (
                key(MAX_TOKENS_PATH),
                Some(VerdictValue::Ceiling(MODEL_LIMIT)),
            ),
        ]
    }

    fn succeeded(meta: &crate::router::DispatchMeta) -> Vec<(&'static str, bool, bool)> {
        meta.field_repairs
            .iter()
            .map(|record| (record.field_path, record.repair_succeeded, record.learned))
            .collect()
    }

    fn assert_two_key_bodies(seat: &ChainSeat, label: &str) {
        let bodies = seat.bodies();
        assert_eq!(
            bodies.len(),
            3,
            "{label}: two refusals and one served retry"
        );
        assert_eq!(thinking_type(&bodies[0]), Some("disabled"), "{label}");
        assert_eq!(
            sent_max_tokens(&bodies[0]),
            Some(u64::from(SENT_MAX_TOKENS)),
            "{label}"
        );
        assert_eq!(thinking_type(&bodies[1]), Some("adaptive"), "{label}");
        assert_eq!(
            sent_max_tokens(&bodies[1]),
            Some(u64::from(SENT_MAX_TOKENS)),
            "{label}: the second refusal names the field the first left alone",
        );
        assert_eq!(thinking_type(&bodies[2]), Some("adaptive"), "{label}");
        assert_eq!(
            sent_max_tokens(&bodies[2]),
            Some(u64::from(MODEL_LIMIT)),
            "{label}: both edits accumulate on the retried body",
        );
    }

    fn assert_two_key_settled(meta: &crate::router::DispatchMeta, label: &str) {
        assert_eq!(verdicts(meta), two_key_verdicts(), "{label}");
        assert_eq!(
            succeeded(meta),
            vec![
                (THINKING_DISABLED_PATH, true, true),
                (MAX_TOKENS_PATH, true, true),
            ],
            "{label}",
        );
    }

    // -----------------------------------------------------------------------
    // Two keys, one request, every walk
    // -----------------------------------------------------------------------

    /// Mutation check: put back the one-step-per-target gate in
    /// `try_field_repair` -> red (the max_tokens refusal reaches the client).
    #[tokio::test]
    async fn two_refused_fields_on_anthropic_api_complete_converge_in_one_request() {
        let seat = ChainSeat::new(Egress::Anthropic, two_key);
        let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

        let dispatched = router
            .complete_with_options(two_key_req(), RouterOptions::new())
            .await;

        assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
        assert_two_key_bodies(&seat, "complete");
        assert_two_key_settled(&dispatched.meta, "complete");
        assert_eq!(router.field_verdicts().snapshot_len(), 2);
        assert_eq!(
            rejection_outcomes(&router),
            vec![crate::wire_check_state::FieldRepairOutcome::RepairedLossy; 2],
            "each refusal joins its own repair record",
        );
    }

    #[tokio::test]
    async fn two_refused_fields_converge_on_a_stream_before_content() {
        let seat = ChainSeat::new(Egress::Anthropic, two_key);
        let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

        let dispatched = router
            .stream_with_options(two_key_req(), RouterOptions::new())
            .await;

        assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
        assert_two_key_bodies(&seat, "stream");
        assert_two_key_settled(&dispatched.meta, "stream");
    }

    /// The count leg assembles the messages body, which carries both fields.
    #[tokio::test]
    async fn two_refused_fields_converge_on_a_settling_token_count() {
        let seat = ChainSeat::new(Egress::Anthropic, two_key);
        let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

        let counted = router.count_tokens_with_meta(two_key_req()).await;

        assert!(counted.result.is_ok(), "{:?}", counted.result.err());
        assert_two_key_bodies(&seat, "count_tokens");
        assert_two_key_settled(&counted.meta, "count_tokens");
    }

    #[tokio::test]
    async fn a_non_settling_token_count_repairs_both_fields_and_commits_nothing() {
        let seat = ChainSeat::new(Egress::Anthropic, two_key);
        let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

        let counted = router.count_tokens(two_key_req()).await;

        assert!(counted.is_ok(), "{:?}", counted.err());
        assert_two_key_bodies(&seat, "non-settling count");
        assert_eq!(
            router.field_verdicts().snapshot_len(),
            0,
            "nothing resident"
        );
        assert!(router.learned_capability_snapshot().is_empty());
    }

    // -----------------------------------------------------------------------
    // Three keys on openai-compat
    // -----------------------------------------------------------------------

    fn compat_fixture(key: &str) -> &'static str {
        match key {
            "temperature" => include_str!(
                "../../tests/fixtures/rejections/openai-compat/unsupported-parameter-temperature.json"
            ),
            "top_p" => include_str!(
                "../../tests/fixtures/rejections/openai-compat/unsupported-parameter-top-p.json"
            ),
            "presence_penalty" => include_str!(
                "../../tests/fixtures/rejections/openai-compat/unsupported-parameter-presence-penalty.json"
            ),
            "frequency_penalty" => include_str!(
                "../../tests/fixtures/rejections/openai-compat/unsupported-parameter-frequency-penalty.json"
            ),
            other => panic!("no committed refusal of {other}"),
        }
    }

    /// The committed refusal of `key` as the openai-compat provider surfaces
    /// it: the body with its lifted `error.type` and `error.code`.
    fn compat_rejection(key: &str) -> Error {
        let fixture: serde_json::Value =
            serde_json::from_str(compat_fixture(key)).expect("the fixture is JSON");
        let status = u16::try_from(fixture["status"].as_u64().expect("a status")).expect("u16");
        let body = fixture["body"].as_str().expect("a body");
        let parsed: serde_json::Value = serde_json::from_str(body).expect("the body is JSON");
        let token = |pointer: &str| {
            parsed
                .pointer(pointer)
                .and_then(|v| v.as_str())
                .map(str::to_string)
        };
        Error::upstream_with_body(
            "chain-seat",
            status,
            routectl_core::UpstreamBody::new(body),
            None,
            token("/error/type"),
            token("/error/code"),
        )
    }

    const THREE_KEYS: [&str; 3] = ["temperature", "top_p", "presence_penalty"];
    const FOUR_KEYS: [&str; 4] = [
        "temperature",
        "top_p",
        "presence_penalty",
        "frequency_penalty",
    ];

    /// Refuse the first of `keys` the body carries, in order.
    fn refuse_first_of(keys: &[&str], body: &serde_json::Value) -> Option<Error> {
        keys.iter()
            .find(|key| body.get(**key).is_some())
            .map(|key| compat_rejection(key))
    }

    /// A Chat Completions client's request setting every one of `keys`.
    fn compat_req(keys: &[&str]) -> ChatRequest {
        let mut req = req_on(ALIAS);
        req.max_tokens = Some(1024);
        req.reasoning = None;
        req.routectl_internal.anthropic_thinking_display = None;
        req.routectl_internal.anthropic_thinking = routectl_core::AnthropicThinking::SentNone;
        let mut client = json!({
            "model": ALIAS,
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": "hello"}],
        });
        for key in keys {
            match *key {
                "temperature" => req.temperature = Some(0.5),
                "top_p" => req.top_p = Some(0.5),
                "presence_penalty" => req.presence_penalty = Some(0.5),
                "frequency_penalty" => req.frequency_penalty = Some(0.5),
                other => panic!("no sampling row {other}"),
            }
            client[*key] = json!(0.5);
        }
        req.routectl_internal.ingress_wire = Some(routectl_core::wire_check::IngressWire {
            dialect: routectl_core::wire_check::IngressDialect::Chat,
            client_version: None,
            bytes: serde_json::to_vec(&client).expect("serialize").into(),
        });
        req
    }

    #[tokio::test]
    async fn three_refused_parameters_on_openai_compat_converge_in_one_request() {
        let seat = ChainSeat::new(Egress::OpenAiCompat, |body, _| {
            refuse_first_of(&THREE_KEYS, body)
        });
        let router = chain_router(&[(OPENAI_COMPAT_TOML, &seat)], &wire_checked(ONE_ATTEMPT));

        let dispatched = router
            .complete_with_options(compat_req(&THREE_KEYS), RouterOptions::new())
            .await;

        assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
        let bodies = seat.bodies();
        assert_eq!(bodies.len(), 4, "three refusals and one served retry");
        for (idx, body) in bodies.iter().enumerate() {
            let carried: Vec<&str> = THREE_KEYS
                .into_iter()
                .filter(|key| body.get(*key).is_some())
                .collect();
            assert_eq!(carried, THREE_KEYS[idx..].to_vec(), "attempt {idx}: {body}");
        }
        assert_eq!(
            verdicts(&dispatched.meta),
            THREE_KEYS.map(|k| (key(k), None)).to_vec(),
        );
        assert_eq!(
            succeeded(&dispatched.meta),
            THREE_KEYS.map(|k| (k, true, true)).to_vec(),
        );
    }

    // -----------------------------------------------------------------------
    // What commits nothing
    // -----------------------------------------------------------------------

    /// The client got `expected`'s message, nothing was learned, and the one
    /// repair record did not succeed.
    fn assert_nothing_committed(
        router: &Router,
        dispatched: &crate::router::Dispatched,
        expected: &Error,
        label: &str,
    ) {
        let err = dispatched
            .result
            .as_ref()
            .err()
            .unwrap_or_else(|| panic!("{label}: the refusal reaches the client"));
        assert_eq!(
            err.parsed_body().and_then(|p| p.error_message()),
            expected.parsed_body().and_then(|p| p.error_message()),
            "{label}: the client gets the latest refusal",
        );
        assert!(
            dispatched.meta.learned_capabilities.is_empty(),
            "{label}: {:?}",
            verdicts(&dispatched.meta),
        );
        assert_eq!(router.field_verdicts().snapshot_len(), 0, "{label}");
        assert_eq!(
            succeeded(&dispatched.meta),
            vec![(THINKING_DISABLED_PATH, false, false)],
            "{label}",
        );
    }

    /// Mutation check: drop the identity-difference condition in
    /// `moved_on_by` -> red (the refused rewrite commits the value it refused).
    #[tokio::test]
    async fn a_refusal_of_the_rewritten_value_stops_and_commits_nothing() {
        let seat = ChainSeat::new(Egress::Anthropic, |body, _| {
            disabled_then(body, adaptive_refused)
        });
        let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

        let dispatched = router
            .complete_with_options(anthropic_req(4096, false), RouterOptions::new())
            .await;

        assert_eq!(seat.calls(), 2, "no second repair of the same field");
        assert_eq!(dispatched.meta.field_repairs.len(), 1);
        assert_nothing_committed(&router, &dispatched, &adaptive_refused(), "same field");
    }

    /// Mutation check: drop the self-identifying condition in `moved_on_by`
    /// -> red.
    #[tokio::test]
    async fn an_inferred_refusal_of_a_different_field_commits_nothing() {
        let seat = ChainSeat::new(Egress::Anthropic, |body, _| {
            disabled_then(body, stop_refused_inferred)
        });
        let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

        let dispatched = router
            .complete_with_options(anthropic_req(4096, true), RouterOptions::new())
            .await;

        assert_eq!(
            seat.bodies()[1]["stop_sequences"],
            json!([STOP]),
            "premise: the named field went out, so only the tier refuses the commit",
        );
        assert_nothing_committed(&router, &dispatched, &stop_refused_inferred(), "inferred");
    }

    /// Mutation check: drop the `names_absent_path` guard in
    /// `settle_field_moved_on` -> red.
    #[tokio::test]
    async fn a_refusal_naming_a_path_the_body_lacked_commits_nothing() {
        let seat = ChainSeat::new(Egress::Anthropic, |body, _| {
            disabled_then(body, absent_top_k_refused)
        });
        let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

        let dispatched = router
            .complete_with_options(anthropic_req(4096, false), RouterOptions::new())
            .await;

        assert!(
            seat.bodies()[1].get("top_k").is_none(),
            "premise: the body lacked the named path"
        );
        assert_nothing_committed(&router, &dispatched, &absent_top_k_refused(), "absent path");
    }

    const OPERATOR_EXTRA: &str = "payload_extras = { metadata = { op_key = \"v\" } }\n";

    /// Refuse disabled thinking, then the operator's extra, then answer the
    /// undone body with a refusal naming nothing.
    fn disabled_then_extra(body: &serde_json::Value, _: usize) -> Option<Error> {
        if thinking_type(body) == Some("disabled") {
            return Some(anthropic_disabled_rejection());
        }
        if body["metadata"].get("op_key").is_some() {
            return Some(anthropic_rejection(
                "metadata.op_key: Extra inputs are not permitted",
            ));
        }
        Some(unlocalized_rejection())
    }

    /// The refusal of the operator's extra is answered by undoing that edit,
    /// so it says nothing about the pending field.
    ///
    /// Mutation check: move the moved-on check above `answer_declared` in
    /// `on_rejection` -> red.
    #[tokio::test]
    async fn a_refusal_a_declared_edit_answers_commits_nothing() {
        let seat = ChainSeat::new(Egress::Anthropic, disabled_then_extra);
        let router = chain_router(
            &[(&format!("{ANTHROPIC_API_TOML}{OPERATOR_EXTRA}"), &seat)],
            &wire_checked(ONE_ATTEMPT),
        );
        let mut req = anthropic_req(4096, false);
        let mut client = disabled_client_body(None);
        client["metadata"] = json!({});
        req.provider_extras = Some(json!({"metadata": {}}));
        if let Some(wire) = req.routectl_internal.ingress_wire.as_mut() {
            wire.bytes = serde_json::to_vec(&client).expect("serialize").into();
        }

        let dispatched = router
            .complete_with_options(req, RouterOptions::new())
            .await;

        let bodies = seat.bodies();
        assert_eq!(
            bodies.len(),
            3,
            "refused, extra refused, undone and refused"
        );
        assert_eq!(bodies[1]["metadata"]["op_key"], json!("v"), "premise");
        assert!(
            bodies[2]["metadata"].get("op_key").is_none(),
            "premise: the undo answered the refusal: {}",
            bodies[2],
        );
        assert_nothing_committed(&router, &dispatched, &unlocalized_rejection(), "declared");
    }

    // -----------------------------------------------------------------------
    // A moved-on commit stands when the next field cannot be repaired
    // -----------------------------------------------------------------------

    /// Mutation check: read the remapped class instead of the native one in
    /// `settle_field_moved_on` -> red on `remapped` (an operator routing
    /// preference would decide what the upstream proved).
    #[tokio::test]
    async fn a_moved_on_refusal_of_a_field_with_no_row_commits_the_earlier_step() {
        let remap = "[providers.p0.class_overrides]\n400 = \"server-error\"\n";
        let rows = [("native", String::new()), ("remapped", remap.to_string())];
        for (name, provider_extra) in rows {
            let seat = ChainSeat::new(Egress::Anthropic, |body, _| {
                disabled_then(body, stop_refused)
            });
            let provider = format!("{ANTHROPIC_API_TOML}{provider_extra}");
            let router = chain_router(&[(&provider, &seat)], &wire_checked(ONE_ATTEMPT));

            let dispatched = router
                .complete_with_options(anthropic_req(4096, true), RouterOptions::new())
                .await;

            assert_eq!(seat.calls(), 2, "{name}");
            let err = dispatched.result.as_ref().expect_err("the refusal");
            assert_eq!(
                err.parsed_body().and_then(|p| p.error_message()),
                Some("stop_sequences: Extra inputs are not permitted"),
                "{name}: the latest refusal reaches the client",
            );
            assert_eq!(
                verdicts(&dispatched.meta),
                vec![(
                    thinking_disabled_key(),
                    Some(VerdictValue::Shape("thinking.type.adaptive")),
                )],
                "{name}",
            );
            assert_eq!(
                succeeded(&dispatched.meta),
                vec![(THINKING_DISABLED_PATH, true, true)],
                "{name}",
            );
        }
    }

    /// Four refused parameters on seat 0, a 503 there, and seat 1 refusing
    /// the first two: five draws in all, so seat 1's second field finds the
    /// budget empty.
    #[tokio::test]
    async fn exhaustion_across_two_seats_keeps_every_proven_step_committed() {
        let seat0 = ChainSeat::new(Egress::OpenAiCompat, |body, _| {
            refuse_first_of(&FOUR_KEYS, body).or_else(|| Some(unavailable()))
        });
        let seat1 = ChainSeat::new(Egress::OpenAiCompat, |body, _| {
            refuse_first_of(&FOUR_KEYS, body)
        });
        let router = chain_router(
            &[(OPENAI_COMPAT_TOML, &seat0), (OPENAI_COMPAT_TOML, &seat1)],
            &wire_checked(ONE_ATTEMPT),
        );

        let dispatched = router
            .complete_with_options(compat_req(&FOUR_KEYS), RouterOptions::new())
            .await;

        assert_eq!(seat0.calls(), 5, "four refusals and the 503");
        assert_eq!(seat1.calls(), 2, "one repair, then no budget");
        let err = dispatched.result.as_ref().expect_err("the refusal");
        assert_eq!(
            err.parsed_body().and_then(|p| p.error_param()),
            Some("top_p"),
            "the latest refusal reaches the client",
        );
        let lanes: Vec<(String, String)> = dispatched
            .meta
            .learned_capabilities
            .iter()
            .map(|event| (event.state_key.clone(), event.capability_key.clone()))
            .collect();
        let lane = |idx: usize| format!("p{idx}#{UNPROFILED_MODEL}");
        assert_eq!(
            lanes,
            vec![
                (lane(0), key("temperature")),
                (lane(0), key("top_p")),
                (lane(0), key("presence_penalty")),
                (lane(1), key("temperature")),
            ],
            "the step the 503 left pending on seat 0 is dropped",
        );
        assert_eq!(
            succeeded(&dispatched.meta),
            vec![
                ("temperature", true, true),
                ("top_p", true, true),
                ("presence_penalty", true, true),
                ("frequency_penalty", false, false),
                ("temperature", true, true),
            ],
        );
    }

    // -----------------------------------------------------------------------
    // Target change and same-target transient
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn a_pending_step_on_a_seat_left_for_a_fallback_commits_nothing() {
        let seat0 = ChainSeat::new(Egress::Anthropic, |body, _| {
            disabled_then(body, unavailable)
        });
        let seat1 = ChainSeat::new(Egress::Anthropic, |_, _| None);
        let router = chain_router(
            &[(ANTHROPIC_API_TOML, &seat0), (ANTHROPIC_API_TOML, &seat1)],
            &wire_checked(ONE_ATTEMPT),
        );

        let dispatched = router
            .complete_with_options(anthropic_req(4096, false), RouterOptions::new())
            .await;

        assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
        assert_eq!((seat0.calls(), seat1.calls()), (2, 1));
        assert_eq!(
            thinking_type(&seat1.bodies()[0]),
            Some("disabled"),
            "the fallback gets the client's own thinking",
        );
        assert!(verdicts(&dispatched.meta).is_empty());
        assert_eq!(router.field_verdicts().snapshot_len(), 0);
        assert_eq!(
            succeeded(&dispatched.meta),
            vec![(THINKING_DISABLED_PATH, false, false)],
        );
    }

    /// The retried body is the repaired body, so its success proves the step.
    #[tokio::test]
    async fn a_pending_step_survives_a_same_target_transient_and_commits_on_success() {
        let seat = ChainSeat::new(Egress::Anthropic, |body, position| {
            if thinking_type(body) == Some("disabled") {
                return Some(anthropic_disabled_rejection());
            }
            (position == 1).then(unavailable)
        });
        let router = anthropic_router(&seat, &wire_checked(THREE_ATTEMPTS));

        let dispatched = router
            .complete_with_options(anthropic_req(4096, false), RouterOptions::new())
            .await;

        assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
        assert_eq!(seat.calls(), 3, "refused, unavailable, served");
        assert_eq!(
            verdicts(&dispatched.meta),
            vec![(
                thinking_disabled_key(),
                Some(VerdictValue::Shape("thinking.type.adaptive")),
            )],
        );
        assert_eq!(
            succeeded(&dispatched.meta),
            vec![(THINKING_DISABLED_PATH, true, true)],
        );
    }
}
