// The `thinking.type.disabled` rewrite row end to end: an adaptive-only
// anthropic-api model is repaired to adaptive (with the companion effort when
// the client carries no positive one), a Bedrock model whose off switch is
// between_tools is repaired to a bare between_tools on both Bedrock API
// shapes, and a lane that accepts disabled is never touched. Requests are
// built the way the Anthropic ingress builds them for `{type: disabled}`, so
// no display or budget the client never sent can leak into a retry.
// An `include!`d FRAGMENT of `field_repair_tests.rs`, sharing the seat
// harness of `field_repair_thinking_type_tests.rs`.

const THINKING_DISABLED_PATH: &str = "thinking.type.disabled";

/// The committed anthropic-api capture of an adaptive-only model refusing
/// `thinking.type: disabled`.
const ANTHROPIC_DISABLED_FIXTURE: &str = include_str!(
    "../../tests/fixtures/rejections/anthropic-api/thinking-type-disabled-unsupported.json"
);

/// The committed Bedrock Invoke capture of a model whose off switch is
/// `between_tools` refusing `thinking.type: disabled`.
#[cfg(feature = "bedrock")]
const BEDROCK_DISABLED_FIXTURE: &str =
    include_str!("../../tests/fixtures/rejections/bedrock/thinking-type-disabled-unsupported.json");

fn thinking_disabled_key() -> String {
    crate::field_capability::field_capability_key(THINKING_DISABLED_PATH)
        .expect("the grounded path is a qualified path")
}

/// The `body` string a committed rejection fixture carries.
fn disabled_fixture_body(fixture: &str) -> String {
    let fixture: serde_json::Value = serde_json::from_str(fixture).expect("the fixture is JSON");
    fixture["body"]
        .as_str()
        .expect("the fixture carries its body")
        .to_string()
}

/// The captured anthropic-api rejection exactly as the upstream returned it.
fn anthropic_disabled_rejection() -> Error {
    Error::upstream(
        "thinking-seat",
        400,
        disabled_fixture_body(ANTHROPIC_DISABLED_FIXTURE),
    )
}

/// The client body an Anthropic client sends with `thinking: {type:
/// disabled}` and, when given, `output_config.effort`.
fn disabled_client_body(effort: Option<&str>) -> serde_json::Value {
    let mut body = json!({
        "model": ALIAS,
        "max_tokens": 4096,
        "messages": [{"role": "user", "content": "hello"}],
        "thinking": {"type": "disabled"},
    });
    if let Some(effort) = effort {
        body["output_config"] = json!({"effort": effort});
    }
    body
}

/// [`disabled_client_body`] as the Anthropic ingress parses it: canonical
/// reasoning off, the client's effort lifted into the canonical effort and
/// kept in the forward-compat extras, the thinking object on the native
/// carrier, no display, and the client bytes for the wire check.
fn disabled_req(effort: Option<&str>) -> ChatRequest {
    let mut req = req_on(ALIAS);
    req.max_tokens = Some(4096);
    req.reasoning = Some(ReasoningConfig {
        effort: effort.map(str::to_string),
        max_tokens: None,
        exclude: None,
        enabled: Some(false),
    });
    req.provider_extras = effort.map(|effort| json!({"output_config": {"effort": effort}}));
    req.routectl_internal.anthropic_thinking_display = None;
    req.routectl_internal.anthropic_thinking = native_thinking(json!({"type": "disabled"}));
    req.routectl_internal.ingress_wire = Some(routectl_core::wire_check::IngressWire {
        dialect: routectl_core::wire_check::IngressDialect::Anthropic,
        client_version: None,
        bytes: serde_json::to_vec(&disabled_client_body(effort))
            .expect("serialize")
            .into(),
    });
    req
}

/// `(row, client effort, effort the adaptive retry carries)`.
type DisabledRow = (&'static str, Option<&'static str>, &'static str);

/// The client effort beside disabled thinking, and what adaptive pairs with:
/// the companion where the client carries no positive effort, the client's
/// own otherwise.
const ADAPTIVE_EFFORT_ROWS: [DisabledRow; 3] = [
    ("effort absent", None, "low"),
    ("effort none", Some("none"), "low"),
    ("effort high", Some("high"), "high"),
];

/// Every wire-check rejection finding's repair outcome.
fn rejection_outcomes(router: &Router) -> Vec<crate::wire_check_state::FieldRepairOutcome> {
    router
        .wire_check_snapshot()
        .findings
        .iter()
        .filter_map(|finding| finding.field_repair_outcome)
        .collect()
}

// ---------------------------------------------------------------------------
// Presence
// ---------------------------------------------------------------------------

#[test]
fn the_thinking_disabled_surface_is_present_only_when_the_egress_emits_disabled() {
    let surface = super::FieldSurface::AnthropicThinkingType("disabled");
    let translated_off = || {
        let mut req = thinking_req(routectl_core::AnthropicThinking::NotCaptured);
        req.reasoning = Some(ReasoningConfig {
            effort: Some("none".into()),
            ..Default::default()
        });
        req
    };
    let rows: [(&str, ChatRequest, bool); 6] = [
        ("native disabled", disabled_req(None), true),
        (
            "native disabled beside effort high",
            disabled_req(Some("high")),
            true,
        ),
        ("translated effort none", translated_off(), true),
        ("native enabled", anthropic_enabled_req(), false),
        (
            "native between_tools",
            thinking_req(native_thinking(json!({"type": "between_tools"}))),
            false,
        ),
        (
            "client sent no thinking",
            thinking_req(routectl_core::AnthropicThinking::SentNone),
            false,
        ),
    ];

    for (name, req, present) in rows {
        assert_eq!(surface.present_in(&req), present, "{name}");
    }
}

// ---------------------------------------------------------------------------
// anthropic-api: disabled -> adaptive
// ---------------------------------------------------------------------------

/// Mutation check: remove `thinking.type.adaptive` from the disabled row's
/// accepted set -> red (no member fills, so no retry is sent).
#[tokio::test]
async fn a_disabled_rejection_on_anthropic_api_retries_adaptive_and_commits_the_shape() {
    use crate::wire_check_state::FieldRepairOutcome;

    for (name, effort, retry_effort) in ADAPTIVE_EFFORT_ROWS {
        let seat = Arc::new(ThinkingTypeSeat::refusing_types(
            &["disabled"],
            anthropic_disabled_rejection,
            AfterRefusal::Serve,
        ));
        let router = thinking_chain(&[(ANTHROPIC_API_TOML, Arc::clone(&seat))], WIRE_CHECK_EVERY);

        let dispatched = router
            .complete_with_options(disabled_req(effort), RouterOptions::new())
            .await;

        assert!(
            dispatched.result.is_ok(),
            "{name}: {:?}",
            dispatched.result.err()
        );
        let bodies = seat.bodies();
        assert_eq!(bodies.len(), 2, "{name}: one refused attempt, one retry");
        assert_eq!(bodies[0]["thinking"], json!({"type": "disabled"}), "{name}");
        assert_eq!(
            bodies[1]["thinking"],
            json!({"type": "adaptive"}),
            "{name}: the client sent no display, so adaptive goes out bare",
        );
        assert_eq!(
            bodies[1]["output_config"]["effort"],
            json!(retry_effort),
            "{name}",
        );
        assert_eq!(
            dispatched
                .meta
                .learned_capabilities
                .iter()
                .map(|event| (event.capability_key.clone(), event.value))
                .collect::<Vec<_>>(),
            vec![(
                thinking_disabled_key(),
                Some(crate::VerdictValue::Shape("thinking.type.adaptive")),
            )],
            "{name}: the success commits the named shape, never the effort",
        );
        assert_eq!(
            dispatched.meta.field_repairs[0].action,
            super::FIELD_ACTION_REWRITE_REPAIR,
            "{name}",
        );
        assert_eq!(
            rejection_outcomes(&router),
            vec![FieldRepairOutcome::RepairedLossy],
            "{name}: the rewrite served a value the client did not send",
        );
        let snapshot = router.wire_check_snapshot();
        assert!(
            snapshot.loss.is_empty(),
            "{name}: every edit the repair made is declared: {:?}",
            snapshot.findings,
        );
    }
}

/// Mutation check: make the pre-flight planner skip rewrite rows -> red (the
/// second request is refused before it is repaired).
#[tokio::test]
async fn the_committed_adaptive_rewrite_preflights_a_disabled_request_with_no_rejection() {
    for (name, effort, retry_effort) in ADAPTIVE_EFFORT_ROWS {
        let seat = Arc::new(ThinkingTypeSeat::refusing_types(
            &["disabled"],
            anthropic_disabled_rejection,
            AfterRefusal::Serve,
        ));
        let router = thinking_chain(&[(ANTHROPIC_API_TOML, Arc::clone(&seat))], "");
        let first = router
            .complete_with_options(disabled_req(effort), RouterOptions::new())
            .await;
        assert!(first.result.is_ok(), "{name}: {:?}", first.result.err());
        let event = first
            .meta
            .learned_capabilities
            .first()
            .unwrap_or_else(|| panic!("{name}: the repair committed a verdict"));
        assert!(
            router.acknowledge_durable_field_confirmation(
                &event.state_key,
                &event.capability_key,
                &event.provider_kind,
                event.persistence_generation,
                event.incarnation,
                event.observations,
            ),
            "{name}: premise: the durable write acknowledges the confirmation",
        );

        let second = router
            .complete_with_options(disabled_req(effort), RouterOptions::new())
            .await;

        assert!(second.result.is_ok(), "{name}: {:?}", second.result.err());
        let bodies = seat.bodies();
        assert_eq!(bodies.len(), 3, "{name}: the second request is sent once");
        assert_eq!(bodies[2]["thinking"], json!({"type": "adaptive"}), "{name}");
        assert_eq!(
            bodies[2]["output_config"]["effort"],
            json!(retry_effort),
            "{name}",
        );
        assert!(
            second.meta.field_repairs.is_empty(),
            "{name}: no rejection, so no reactive repair",
        );
        assert!(
            second
                .meta
                .field_preflight
                .iter()
                .any(|record| record.acted && record.field_path == Some(THINKING_DISABLED_PATH)),
            "{name}: the pre-flight rewrite acted: {:?}",
            second.meta.field_preflight,
        );
    }
}

/// Control for the rows above: the same seat refusing nothing serves the
/// client's disabled thinking as sent.
#[tokio::test]
async fn a_lane_that_accepts_disabled_thinking_is_never_repaired_and_learns_nothing() {
    for (name, effort, _) in ADAPTIVE_EFFORT_ROWS {
        let seat = Arc::new(ThinkingTypeSeat::refusing_types(
            &[],
            anthropic_disabled_rejection,
            AfterRefusal::Serve,
        ));
        let router = thinking_chain(&[(ANTHROPIC_API_TOML, Arc::clone(&seat))], WIRE_CHECK_EVERY);

        let dispatched = router
            .complete_with_options(disabled_req(effort), RouterOptions::new())
            .await;

        assert!(
            dispatched.result.is_ok(),
            "{name}: {:?}",
            dispatched.result.err()
        );
        let bodies = seat.bodies();
        assert_eq!(bodies.len(), 1, "{name}: sent once");
        assert_eq!(bodies[0]["thinking"], json!({"type": "disabled"}), "{name}");
        assert!(dispatched.meta.field_repairs.is_empty(), "{name}");
        assert!(dispatched.meta.learned_capabilities.is_empty(), "{name}");
        assert!(rejection_outcomes(&router).is_empty(), "{name}");
        assert_eq!(router.field_repair_counters().repair_attempted, 0, "{name}");
    }
}

// ---------------------------------------------------------------------------
// Bedrock Invoke and Converse: disabled -> between_tools
// ---------------------------------------------------------------------------

#[cfg(feature = "bedrock")]
mod bedrock_thinking_off {
    use super::*;

    use crate::wire_check_state::FieldRepairOutcome;
    use routectl_providers::bedrock::{
        BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth,
    };
    use wiremock::matchers::{body_partial_json, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const REGION: &str = "us-east-1";

    /// A synthetic wire id keeps every baked catalog prior out of play.
    const WIRE: &str = "us.anthropic.zz-thinking-off";

    const ERROR_TYPE_HEADER: &str =
        "ValidationException:http://internal.amazon.com/coral/com.amazon.bedrock/";

    /// What answers a request whose thinking is no longer `disabled`.
    #[derive(Clone, Copy)]
    enum Retry {
        Serve,
        RefuseAgain,
    }

    /// Assembled at runtime so no key-shaped literal sits in the source.
    fn bearer_key() -> String {
        ["thinking", "off", "test", "key"].join("-")
    }

    fn is_converse(shape: BedrockApiShape) -> bool {
        matches!(shape, BedrockApiShape::Converse)
    }

    fn config_toml(shape: BedrockApiShape) -> String {
        let api_shape = if is_converse(shape) {
            "converse"
        } else {
            "invoke"
        };
        format!(
            "{WIRE_CHECK_EVERY}\n[retry]\nmax_attempts = 1\ninitial_backoff_ms = 0\n\
             jitter_ms = 0\n\n[providers.p0]\nkind = \"bedrock\"\nregion = \"{REGION}\"\n\
             creds = {{ kind = \"default-chain\" }}\napi_shape = \"{api_shape}\"\n"
        )
    }

    /// The committed rejection as `shape` returns it: the captured flat body
    /// on Invoke, its message behind the model-errors wrapper on Converse.
    fn rejection(shape: BedrockApiShape) -> ResponseTemplate {
        let captured: serde_json::Value =
            serde_json::from_str(&disabled_fixture_body(BEDROCK_DISABLED_FIXTURE))
                .expect("the body is JSON");
        let body = if is_converse(shape) {
            json!({
                "message": format!(
                    "The model returned the following errors: {}",
                    captured["message"].as_str().expect("a flat message"),
                ),
            })
        } else {
            captured
        };
        ResponseTemplate::new(400)
            .insert_header("x-amzn-ErrorType", ERROR_TYPE_HEADER)
            .set_body_raw(body.to_string(), "application/json")
    }

    fn success(shape: BedrockApiShape) -> ResponseTemplate {
        let body = if is_converse(shape) {
            json!({
                "output": {"message": {"role": "assistant", "content": [{"text": "ok"}]}},
                "stopReason": "end_turn",
                "usage": {"inputTokens": 3, "outputTokens": 1, "totalTokens": 4},
            })
        } else {
            json!({
                "id": "msg_thinking_off",
                "type": "message",
                "role": "assistant",
                "model": WIRE,
                "content": [{"type": "text", "text": "ok"}],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 3, "output_tokens": 1},
            })
        };
        ResponseTemplate::new(200).set_body_json(body)
    }

    /// Where `shape` carries the Anthropic `thinking` and `output_config`.
    fn anthropic_fields(shape: BedrockApiShape, body: &serde_json::Value) -> &serde_json::Value {
        if is_converse(shape) {
            &body["additionalModelRequestFields"]
        } else {
            body
        }
    }

    /// `{"thinking": {"type": "disabled"}}` where `shape` carries it.
    fn disabled_thinking_on(shape: BedrockApiShape) -> serde_json::Value {
        let thinking = json!({"thinking": {"type": "disabled"}});
        if is_converse(shape) {
            json!({"additionalModelRequestFields": thinking})
        } else {
            thinking
        }
    }

    struct Lane {
        router: Router,
        server: MockServer,
        shape: BedrockApiShape,
    }

    impl Lane {
        /// A real Bedrock provider of `shape` in front of a mock upstream
        /// that refuses disabled thinking with the committed capture and
        /// answers every other body per `retry`.
        async fn start(shape: BedrockApiShape, retry: Retry) -> Self {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .and(body_partial_json(disabled_thinking_on(shape)))
                .respond_with(rejection(shape))
                .with_priority(1)
                .mount(&server)
                .await;
            let otherwise = match retry {
                Retry::Serve => success(shape),
                Retry::RefuseAgain => rejection(shape),
            };
            Mock::given(method("POST"))
                .respond_with(otherwise)
                .mount(&server)
                .await;

            let mut config: Config = toml::from_str(&config_toml(shape)).expect("valid test toml");
            config
                .aliases
                .insert(ALIAS.to_string(), AliasValue::Chain(vec!["m0".to_string()]));
            let mut router = Router::new(Arc::new(config));
            router.install_resolved_models(BTreeMap::from([(
                "m0".to_string(),
                Arc::new(ResolvedModel::new(
                    "m0",
                    "p0",
                    Arc::new(provider(shape, &server.uri()).await) as Arc<dyn Provider>,
                    WIRE,
                )),
            )]));
            Self {
                router: router.with_capability_writes_assumed_durable_for_tests(),
                server,
                shape,
            }
        }

        /// The Anthropic `thinking` and `output_config.effort` of each body
        /// the upstream received, in order.
        async fn sent(&self) -> Vec<(serde_json::Value, serde_json::Value)> {
            self.server
                .received_requests()
                .await
                .expect("request recording is on")
                .iter()
                .map(|request| {
                    let body: serde_json::Value =
                        serde_json::from_slice(&request.body).expect("request body is JSON");
                    let fields = anthropic_fields(self.shape, &body);
                    (
                        fields["thinking"].clone(),
                        fields["output_config"]["effort"].clone(),
                    )
                })
                .collect()
        }
    }

    async fn provider(shape: BedrockApiShape, origin: &str) -> BedrockProvider {
        let creds = BedrockCreds::BearerKey { key: bearer_key() };
        let cfg = BedrockConfig {
            id: "bedrock:p0".into(),
            region: REGION.into(),
            model_id: WIRE.into(),
            api_shape: shape,
            creds: creds.clone(),
            user_agent: None,
            header_extras: Vec::new(),
            anthropic_beta: Vec::new(),
            additional_model_request_fields: None,
        };
        let resolved = auth::resolve(&creds, REGION).await.expect("resolve");
        BedrockProvider::new(cfg, resolved)
            .expect("canonical region")
            .with_runtime_origin_for_tests(origin)
    }

    const SHAPES: [BedrockApiShape; 2] = [BedrockApiShape::Invoke, BedrockApiShape::Converse];

    /// Mutation check: remove `thinking.type.between_tools` from the disabled
    /// row's accepted set -> red (no member fills, so no retry is sent).
    #[tokio::test]
    async fn a_disabled_rejection_on_bedrock_retries_bare_between_tools_with_the_effort_as_sent() {
        for shape in SHAPES {
            for (name, effort, _) in ADAPTIVE_EFFORT_ROWS {
                let lane = Lane::start(shape, Retry::Serve).await;

                let dispatched = lane
                    .router
                    .complete_with_options(disabled_req(effort), RouterOptions::new())
                    .await;

                assert!(
                    dispatched.result.is_ok(),
                    "{shape:?} {name}: {:?}",
                    dispatched.result.err()
                );
                let sent = lane.sent().await;
                assert_eq!(
                    sent.len(),
                    2,
                    "{shape:?} {name}: one refused attempt, one retry"
                );
                assert_eq!(sent[0].0, json!({"type": "disabled"}), "{shape:?} {name}");
                assert_eq!(
                    sent[1].0,
                    json!({"type": "between_tools"}),
                    "{shape:?} {name}: between_tools takes no other member",
                );
                assert_eq!(
                    sent[1].1, sent[0].1,
                    "{shape:?} {name}: the effort goes out as the client sent it",
                );
                assert_eq!(
                    sent[1].1,
                    effort.map_or(serde_json::Value::Null, |effort| json!(effort)),
                    "{shape:?} {name}: premise: the first attempt carried the client's effort",
                );
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
                    vec![(
                        thinking_disabled_key(),
                        "bedrock".to_string(),
                        Some(crate::VerdictValue::Shape("thinking.type.between_tools")),
                    )],
                    "{shape:?} {name}",
                );
                assert_eq!(
                    rejection_outcomes(&lane.router),
                    vec![FieldRepairOutcome::RepairedLossy],
                    "{shape:?} {name}: the rewrite served a value the client did not send",
                );
            }
        }
    }

    /// The repair budget, not the row, bounds a lane that refuses the
    /// repaired shape too: the retry is the request's one repair, and its
    /// failure commits nothing.
    #[tokio::test]
    async fn a_refused_between_tools_retry_commits_nothing_and_repairs_once() {
        for shape in SHAPES {
            let lane = Lane::start(shape, Retry::RefuseAgain).await;

            let dispatched = lane
                .router
                .complete_with_options(disabled_req(Some("high")), RouterOptions::new())
                .await;

            assert!(
                dispatched.result.is_err(),
                "{shape:?}: the refusal reaches the client"
            );
            let sent = lane.sent().await;
            assert_eq!(
                sent.iter()
                    .map(|(thinking, _)| thinking.clone())
                    .collect::<Vec<_>>(),
                vec![
                    json!({"type": "disabled"}),
                    json!({"type": "between_tools"})
                ],
                "{shape:?}: one repaired retry and no further attempt",
            );
            assert_eq!(dispatched.meta.field_repairs.len(), 1, "{shape:?}");
            assert!(
                !dispatched.meta.field_repairs[0].repair_succeeded,
                "{shape:?}"
            );
            assert!(
                dispatched.meta.learned_capabilities.is_empty(),
                "{shape:?}: a failed repaired retry commits no verdict",
            );
            assert_eq!(
                lane.router.field_repair_counters().repair_attempted,
                1,
                "{shape:?}"
            );
            assert_eq!(
                lane.router.field_verdicts().snapshot_len(),
                0,
                "{shape:?}: nothing is resident",
            );
        }
    }
    /// The committed Converse refusal of `xhigh`, inside the model-errors
    /// wrapper as captured.
    fn converse_xhigh_refused() -> ResponseTemplate {
        ResponseTemplate::new(400)
            .insert_header("x-amzn-ErrorType", ERROR_TYPE_HEADER)
            .set_body_raw(
                disabled_fixture_body(include_str!(
                    "../../tests/fixtures/rejections/bedrock/effort-xhigh-unsupported-converse-sonnet-4-6.json"
                )),
                "application/json",
            )
    }

    /// On Converse the effort travels in the field bag. A pending
    /// between_tools rewrite is proven by the `Supported levels` refusal of
    /// the bag's `xhigh`, which names another field, so it commits; the
    /// refused level is then retried at `high`, and with between_tools
    /// thinking beside it that repair learns nothing.
    ///
    /// Mutation check: make `BetweenTools` mintable in `ranked_step_mints` ->
    /// red (an effort verdict is learned).
    #[tokio::test]
    async fn a_converse_level_refusal_after_a_thinking_rewrite_commits_the_rewrite_only() {
        let lane = Lane::start(BedrockApiShape::Converse, Retry::Serve).await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"additionalModelRequestFields": {
                "thinking": {"type": "between_tools"},
                "output_config": {"effort": "xhigh"},
            }})))
            .respond_with(converse_xhigh_refused())
            .with_priority(1)
            .mount(&lane.server)
            .await;

        let dispatched = lane
            .router
            .complete_with_options(disabled_req(Some("xhigh")), RouterOptions::new())
            .await;

        assert!(
            dispatched.result.is_ok(),
            "{:?}",
            dispatched.result.as_ref().err()
        );
        let sent = lane.sent().await;
        assert_eq!(
            sent,
            vec![
                (json!({"type": "disabled"}), json!("xhigh")),
                (json!({"type": "between_tools"}), json!("xhigh")),
                (json!({"type": "between_tools"}), json!("high")),
            ],
        );
        assert_eq!(
            dispatched
                .meta
                .learned_capabilities
                .iter()
                .map(|event| (event.capability_key.clone(), event.value))
                .collect::<Vec<_>>(),
            vec![(
                thinking_disabled_key(),
                Some(crate::VerdictValue::Shape("thinking.type.between_tools")),
            )],
        );
        assert_eq!(
            dispatched
                .meta
                .field_repairs
                .iter()
                .map(|record| (record.field_path, record.repair_succeeded, record.learned))
                .collect::<Vec<_>>(),
            vec![
                (THINKING_DISABLED_PATH, true, true),
                ("output_config.effort.xhigh", true, false),
            ],
        );
    }
}
