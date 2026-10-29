// Every closed-table row, refused by its captured rejection on a lane that
// produced it, reads as CARRIED on the body that lane's real egress assembles:
// a repaired retry refused for one of these fields proves the fields repaired
// before it only when the settlement confirms the body carried the field.
// An `include!`d FRAGMENT of `learn_engine_location_tests.rs`.

mod closed_table_presence {
    use super::*;

    use routectl_core::{AnthropicThinking, ReasoningConfig, ThinkingObject};
    use routectl_providers::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};
    use routectl_providers::openai_compat::request::normalize;
    use routectl_providers::openai_compat::{HistoryReasoning, ReasoningDialect};

    const ANTHROPIC_DISABLED_FIXTURE: &str = include_str!(
        "../../tests/fixtures/rejections/anthropic-api/thinking-type-disabled-unsupported.json"
    );
    const MESSAGES_CEILING_FIXTURE: &str = include_str!(
        "../../tests/fixtures/rejections/anthropic-api/max-tokens-above-model-limit-messages.json"
    );
    const SENT_MAX_TOKENS: u32 = 1_000_000;

    fn compat_fixture(param: &str) -> &'static str {
        match param {
            "temperature" => TEMPERATURE_FIXTURE,
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

    // -----------------------------------------------------------------------
    // Client requests, as the ingress builds them
    // -----------------------------------------------------------------------

    fn base() -> ChatRequest {
        ChatRequest {
            model: ALIAS.into(),
            messages: vec![routectl_core::Message {
                role: routectl_core::Role::User,
                content: routectl_core::MessageContent::Text("hello".into()),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
                refusal: None,
            }]
            .into(),
            max_tokens: Some(4096),
            ..Default::default()
        }
    }

    fn thinking(carrier: Value, enabled: bool, budget: Option<u32>) -> ChatRequest {
        let mut req = base();
        req.reasoning = Some(ReasoningConfig {
            effort: None,
            max_tokens: budget,
            exclude: None,
            enabled: Some(enabled),
        });
        req.routectl_internal.anthropic_thinking = AnthropicThinking::Sent(
            ThinkingObject::from_client(carrier).expect("an object builds a carrier"),
        );
        req
    }

    fn disabled_thinking() -> ChatRequest {
        thinking(json!({"type": "disabled"}), false, None)
    }

    fn enabled_thinking() -> ChatRequest {
        thinking(
            json!({"type": "enabled", "budget_tokens": 2048}),
            true,
            Some(2048),
        )
    }

    #[cfg(feature = "bedrock")]
    fn adaptive_thinking() -> ChatRequest {
        thinking(json!({"type": "adaptive"}), true, None)
    }

    fn enabled_thinking_with_display() -> ChatRequest {
        let mut req = enabled_thinking();
        req.routectl_internal.anthropic_thinking_display = Some("bogus".into());
        req
    }

    fn above_limit() -> ChatRequest {
        let mut req = base();
        req.max_tokens = Some(SENT_MAX_TOKENS);
        req
    }

    fn with_temperature() -> ChatRequest {
        let mut req = base();
        req.temperature = Some(0.5);
        req
    }

    fn with_sampling() -> ChatRequest {
        let mut req = with_temperature();
        req.top_p = Some(0.5);
        req.presence_penalty = Some(0.5);
        req.frequency_penalty = Some(0.5);
        req
    }

    // -----------------------------------------------------------------------
    // Bodies the real egresses assemble
    // -----------------------------------------------------------------------

    fn messages_body(req: &ChatRequest) -> Value {
        AnthropicApiProvider::new(AnthropicApiConfig::new("presence-seat", "k"))
            .normalize_request(req)
            .expect("the messages body assembles")
    }

    fn compat_body(req: &ChatRequest) -> Value {
        normalize(
            "presence-seat",
            req,
            ReasoningDialect::OpenAi,
            HistoryReasoning::default(),
            None,
            false,
        )
        .expect("the chat body assembles")
    }

    #[cfg(feature = "bedrock")]
    fn bedrock_cfg(
        api_shape: routectl_providers::bedrock::BedrockApiShape,
    ) -> routectl_providers::bedrock::BedrockConfig {
        routectl_providers::bedrock::BedrockConfig {
            id: "bedrock:p0".into(),
            region: "us-east-1".into(),
            model_id: "us.anthropic.claude-sonnet-5".into(),
            api_shape,
            creds: routectl_providers::bedrock::BedrockCreds::BearerKey {
                key: ["presence", "seat", "test"].join("-"),
            },
            user_agent: None,
            header_extras: Vec::new(),
            anthropic_beta: Vec::new(),
            additional_model_request_fields: None,
        }
    }

    #[cfg(feature = "bedrock")]
    fn converse_body(req: &ChatRequest) -> Value {
        let cfg = bedrock_cfg(routectl_providers::bedrock::BedrockApiShape::Converse);
        routectl_providers::bedrock::converse::normalize_request(&cfg, req)
            .expect("the converse body assembles")
    }

    #[cfg(feature = "bedrock")]
    fn invoke_body(req: &ChatRequest) -> Value {
        let cfg = bedrock_cfg(routectl_providers::bedrock::BedrockApiShape::Invoke);
        routectl_providers::bedrock::invoke::normalize_request(&cfg, req)
            .expect("the invoke body assembles")
    }

    fn messages_display(_: &ChatRequest, _: Value) -> Value {
        messages_body(&enabled_thinking_with_display())
    }
    fn messages_disabled(_: &ChatRequest, _: Value) -> Value {
        messages_body(&disabled_thinking())
    }
    fn messages_enabled(_: &ChatRequest, _: Value) -> Value {
        messages_body(&enabled_thinking())
    }
    fn messages_above_limit(_: &ChatRequest, _: Value) -> Value {
        messages_body(&above_limit())
    }
    fn compat_sampling(_: &ChatRequest, _: Value) -> Value {
        compat_body(&with_sampling())
    }
    #[cfg(feature = "bedrock")]
    fn invoke_disabled(_: &ChatRequest, _: Value) -> Value {
        invoke_body(&disabled_thinking())
    }
    #[cfg(feature = "bedrock")]
    fn invoke_above_limit(_: &ChatRequest, _: Value) -> Value {
        invoke_body(&above_limit())
    }
    #[cfg(feature = "bedrock")]
    fn converse_disabled(_: &ChatRequest, _: Value) -> Value {
        converse_body(&disabled_thinking())
    }
    #[cfg(feature = "bedrock")]
    fn converse_enabled(_: &ChatRequest, _: Value) -> Value {
        converse_body(&enabled_thinking())
    }
    #[cfg(feature = "bedrock")]
    fn converse_adaptive(_: &ChatRequest, _: Value) -> Value {
        converse_body(&adaptive_thinking())
    }
    #[cfg(feature = "bedrock")]
    fn converse_temperature(_: &ChatRequest, _: Value) -> Value {
        converse_body(&with_temperature())
    }
    #[cfg(feature = "bedrock")]
    fn converse_above_limit(_: &ChatRequest, _: Value) -> Value {
        converse_body(&above_limit())
    }

    // -----------------------------------------------------------------------
    // Rejections
    // -----------------------------------------------------------------------

    /// A captured Bedrock message as Converse returns it, behind the
    /// model-errors wrapper.
    #[cfg(feature = "bedrock")]
    fn converse_wrapped(text: &str) -> Error {
        let (status, body) = fixture(text);
        let captured: Value = serde_json::from_str(&body).expect("the body is JSON");
        let message = captured["message"].as_str().expect("a flat message");
        let wrapped = if message.starts_with("The model returned the following errors: ") {
            message.to_string()
        } else {
            format!("The model returned the following errors: {message}")
        };
        Error::upstream("scripted", status, json!({"message": wrapped}).to_string())
    }

    /// The captured enabled refusal in the Messages envelope.
    fn messages_enabled_rejection() -> Error {
        let (_, body) = fixture(CONVERSE_FIXTURE);
        let captured: Value = serde_json::from_str(&body).expect("the body is JSON");
        let message = routectl_core::strip_converse_errors_prefix(
            captured["message"].as_str().expect("a flat message"),
        );
        anthropic_message(message)
    }

    // -----------------------------------------------------------------------
    // Effort rows, one per level of the rank
    // -----------------------------------------------------------------------

    const EFFORT_LEVELS: [&str; 6] = routectl_providers::effort::VALID_EFFORT_TOKENS;

    /// An Anthropic client's effort-only request at rank member `L`, as the
    /// ingress parses it.
    fn effort_req<const L: usize>() -> ChatRequest {
        let mut req = base();
        req.provider_extras = Some(json!({"output_config": {"effort": EFFORT_LEVELS[L]}}));
        req.reasoning = Some(ReasoningConfig {
            effort: Some(EFFORT_LEVELS[L].into()),
            ..Default::default()
        });
        req.routectl_internal.anthropic_thinking = AnthropicThinking::SentNone;
        req
    }

    fn messages_effort<const L: usize>(_: &ChatRequest, _: Value) -> Value {
        messages_body(&effort_req::<L>())
    }
    #[cfg(feature = "bedrock")]
    fn invoke_effort<const L: usize>(_: &ChatRequest, _: Value) -> Value {
        invoke_body(&effort_req::<L>())
    }
    #[cfg(feature = "bedrock")]
    fn converse_effort<const L: usize>(_: &ChatRequest, _: Value) -> Value {
        converse_body(&effort_req::<L>())
    }

    /// The `Supported levels` refusal of rank member `L`, listing one other
    /// level.
    fn effort_level_message<const L: usize>() -> String {
        let other = if L == 0 {
            EFFORT_LEVELS[1]
        } else {
            EFFORT_LEVELS[0]
        };
        format!(
            "This model does not support effort level '{}'. Supported levels: {other}.",
            EFFORT_LEVELS[L]
        )
    }

    fn messages_effort_refusal<const L: usize>() -> Error {
        anthropic_message(&effort_level_message::<L>())
    }
    #[cfg(feature = "bedrock")]
    fn invoke_effort_refusal<const L: usize>() -> Error {
        Error::upstream(
            "scripted",
            400,
            json!({"message": effort_level_message::<L>()}).to_string(),
        )
    }
    #[cfg(feature = "bedrock")]
    fn converse_effort_refusal<const L: usize>() -> Error {
        let wrapped = format!(
            "The model returned the following errors: {}",
            effort_level_message::<L>()
        );
        Error::upstream("scripted", 400, json!({"message": wrapped}).to_string())
    }

    /// The row name for rank member `level` on `lane`; leaked so it lives as
    /// long as the `&'static` name a [`SiteRow`] carries.
    fn effort_name(level: usize, lane: &str) -> &'static str {
        Box::leak(format!("output_config.effort.{}, {lane}", EFFORT_LEVELS[level]).into_boxed_str())
    }

    /// One effort row per rank member on `lane`, from the per-member body and
    /// refusal builders.
    fn effort_lane_rows(
        lane: &str,
        provider_block: &'static str,
        rows: [(Shape, fn() -> Error); 6],
    ) -> Vec<SiteRow> {
        rows.into_iter()
            .enumerate()
            .map(|(level, (sent, err))| {
                carried(effort_name(level, lane), provider_block, sent, err)
            })
            .collect()
    }

    fn messages_effort_rows() -> Vec<SiteRow> {
        effort_lane_rows(
            "anthropic-api",
            ANTHROPIC,
            [
                (messages_effort::<0>, messages_effort_refusal::<0>),
                (messages_effort::<1>, messages_effort_refusal::<1>),
                (messages_effort::<2>, messages_effort_refusal::<2>),
                (messages_effort::<3>, messages_effort_refusal::<3>),
                (messages_effort::<4>, messages_effort_refusal::<4>),
                (messages_effort::<5>, messages_effort_refusal::<5>),
            ],
        )
    }

    #[cfg(feature = "bedrock")]
    fn bedrock_effort_rows() -> Vec<SiteRow> {
        let mut rows = effort_lane_rows(
            "invoke",
            INVOKE,
            [
                (invoke_effort::<0>, invoke_effort_refusal::<0>),
                (invoke_effort::<1>, invoke_effort_refusal::<1>),
                (invoke_effort::<2>, invoke_effort_refusal::<2>),
                (invoke_effort::<3>, invoke_effort_refusal::<3>),
                (invoke_effort::<4>, invoke_effort_refusal::<4>),
                (invoke_effort::<5>, invoke_effort_refusal::<5>),
            ],
        );
        rows.extend(effort_lane_rows(
            "converse",
            CONVERSE,
            [
                (converse_effort::<0>, converse_effort_refusal::<0>),
                (converse_effort::<1>, converse_effort_refusal::<1>),
                (converse_effort::<2>, converse_effort_refusal::<2>),
                (converse_effort::<3>, converse_effort_refusal::<3>),
                (converse_effort::<4>, converse_effort_refusal::<4>),
                (converse_effort::<5>, converse_effort_refusal::<5>),
            ],
        ));
        rows
    }

    fn compat_row(param: &'static str) -> SiteRow {
        SiteRow {
            name: param,
            provider_block: OPENAI_COMPAT,
            dialect: IngressDialect::Chat,
            sent: Some(compat_sampling),
            err: match param {
                "temperature" => || fixture_rejection(compat_fixture("temperature")),
                "top_p" => || fixture_rejection(compat_fixture("top_p")),
                "presence_penalty" => || fixture_rejection(compat_fixture("presence_penalty")),
                _ => || fixture_rejection(compat_fixture("frequency_penalty")),
            },
            in_egress: Some(true),
        }
    }

    fn carried(
        name: &'static str,
        provider_block: &'static str,
        sent: Shape,
        err: fn() -> Error,
    ) -> SiteRow {
        SiteRow {
            name,
            provider_block,
            dialect: IngressDialect::Anthropic,
            sent: Some(sent),
            err,
            in_egress: Some(true),
        }
    }

    fn presence_rows() -> Vec<SiteRow> {
        let rows = vec![
            carried(
                "thinking.enabled.display, anthropic-api",
                ANTHROPIC,
                messages_display,
                || fixture_rejection(DISPLAY_FIXTURE),
            ),
            carried(
                "thinking.type.disabled, anthropic-api",
                ANTHROPIC,
                messages_disabled,
                || fixture_rejection(ANTHROPIC_DISABLED_FIXTURE),
            ),
            carried(
                "thinking.type.enabled, anthropic-api",
                ANTHROPIC,
                messages_enabled,
                messages_enabled_rejection,
            ),
            carried(
                "max_tokens, anthropic-api",
                ANTHROPIC,
                messages_above_limit,
                || fixture_rejection(MESSAGES_CEILING_FIXTURE),
            ),
            compat_row("temperature"),
            compat_row("top_p"),
            compat_row("presence_penalty"),
            compat_row("frequency_penalty"),
        ];
        let mut rows = rows;
        rows.extend(messages_effort_rows());
        #[cfg(feature = "bedrock")]
        {
            rows.extend(bedrock_presence_rows());
            rows.extend(bedrock_effort_rows());
        }
        rows
    }

    #[cfg(feature = "bedrock")]
    const BEDROCK_DISABLED_FIXTURE: &str = include_str!(
        "../../tests/fixtures/rejections/bedrock/thinking-type-disabled-unsupported.json"
    );
    #[cfg(feature = "bedrock")]
    const CONVERSE_ADAPTIVE_FIXTURE: &str = include_str!(
        "../../tests/fixtures/rejections/bedrock/thinking-type-adaptive-unknown-variant-converse.json"
    );
    #[cfg(feature = "bedrock")]
    const INVOKE_CEILING_FIXTURE: &str = include_str!(
        "../../tests/fixtures/rejections/bedrock/max-tokens-above-model-limit-invoke.json"
    );
    #[cfg(feature = "bedrock")]
    const CONVERSE_CEILING_FIXTURE: &str =
        include_str!("../../tests/fixtures/rejections/bedrock/max-tokens-above-model-limit.json");

    #[cfg(feature = "bedrock")]
    fn bedrock_presence_rows() -> [SiteRow; 7] {
        [
            carried(
                "thinking.type.disabled, invoke",
                INVOKE,
                invoke_disabled,
                || fixture_rejection(BEDROCK_DISABLED_FIXTURE),
            ),
            carried("max_tokens, invoke", INVOKE, invoke_above_limit, || {
                fixture_rejection(INVOKE_CEILING_FIXTURE)
            }),
            carried(
                "thinking.type.disabled, converse",
                CONVERSE,
                converse_disabled,
                || converse_wrapped(BEDROCK_DISABLED_FIXTURE),
            ),
            carried(
                "thinking.type.enabled, converse",
                CONVERSE,
                converse_enabled,
                || fixture_rejection(CONVERSE_FIXTURE),
            ),
            carried(
                "thinking.type.adaptive, converse",
                CONVERSE,
                converse_adaptive,
                || fixture_rejection(CONVERSE_ADAPTIVE_FIXTURE),
            ),
            carried(
                "temperature, converse",
                CONVERSE,
                converse_temperature,
                || fixture_rejection(BEDROCK_TEMPERATURE_FIXTURE),
            ),
            carried(
                "max_tokens, converse",
                CONVERSE,
                converse_above_limit,
                || fixture_rejection(CONVERSE_CEILING_FIXTURE),
            ),
        ]
    }

    /// The tag-value and `inferenceConfig` reads confirm presence only: a
    /// tag holding another value, or a Converse body without the field where
    /// Converse spells it, stays unknown rather than carried or absent.
    #[test]
    fn a_body_holding_another_value_or_no_spelling_stays_unknown() {
        let rows = vec![SiteRow {
            name: "thinking.type.disabled refused, the body's tag is enabled",
            provider_block: ANTHROPIC,
            dialect: IngressDialect::Anthropic,
            sent: Some(messages_enabled),
            err: || fixture_rejection(ANTHROPIC_DISABLED_FIXTURE),
            in_egress: None,
        }];
        #[cfg(feature = "bedrock")]
        let rows = {
            let mut rows = rows;
            rows.extend([
                SiteRow {
                    name: "thinking.type.enabled refused, the bag's tag is disabled",
                    provider_block: CONVERSE,
                    dialect: IngressDialect::Anthropic,
                    sent: Some(converse_disabled),
                    err: || fixture_rejection(CONVERSE_FIXTURE),
                    in_egress: None,
                },
                SiteRow {
                    name: "Converse temperature refused, inferenceConfig lacks it",
                    provider_block: CONVERSE,
                    dialect: IngressDialect::Anthropic,
                    sent: Some(converse_above_limit),
                    err: || fixture_rejection(BEDROCK_TEMPERATURE_FIXTURE),
                    in_egress: None,
                },
            ]);
            rows
        };
        for row in rows {
            assert_eq!(settle(&row), row.in_egress, "{}", row.name);
        }
    }

    /// The presence rows cover every effort row on every lane that spells the
    /// effort field on a Messages-shape body, so a level missing from them
    /// is a level whose presence nothing pins.
    #[test]
    fn the_presence_rows_cover_every_effort_level_on_each_messages_lane() {
        let names: Vec<&str> = presence_rows().iter().map(|row| row.name).collect();
        let lanes: &[&str] = if cfg!(feature = "bedrock") {
            &["anthropic-api", "invoke", "converse"]
        } else {
            &["anthropic-api"]
        };
        let effort_rows = super::super::super::field_repair::grounded_table()
            .iter()
            .filter(|row| {
                matches!(
                    row.transform,
                    super::super::super::field_repair::FieldTransform::RankedRewrite { .. }
                )
            });
        let mut covered = 0;
        for row in effort_rows {
            for lane in lanes {
                let name = format!("{}, {lane}", row.path);
                assert!(names.contains(&name.as_str()), "no presence row {name}");
                covered += 1;
            }
        }
        assert_eq!(covered, EFFORT_LEVELS.len() * lanes.len());
    }

    #[test]
    fn every_closed_table_row_refused_on_its_lane_reads_as_carried() {
        let failures: Vec<String> = presence_rows()
            .iter()
            .filter_map(|row| {
                let read = settle(row);
                (read != row.in_egress).then(|| format!("{}: {read:?}", row.name))
            })
            .collect();
        assert!(failures.is_empty(), "{failures:#?}");
    }
}
