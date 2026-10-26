// The `thinking.type.enabled` rewrite row: which requests carry it, what the
// repaired retry sends on the wire, what a success teaches the next request,
// and what a rewrite that does not take, or a failed retry, leaves behind. The
// seats run the real Anthropic body assembly, so every assertion about the
// repair reads the body an upstream would receive.
// An `include!`d FRAGMENT of `field_repair_tests.rs`.

// ---------------------------------------------------------------------------
// Fixture: a seat that refuses enabled thinking
// ---------------------------------------------------------------------------

/// The committed Bedrock capture of an adaptive-only model refusing
/// `thinking.type: enabled`.
const THINKING_ENABLED_FIXTURE: &str =
    include_str!("../../tests/fixtures/rejections/bedrock/thinking-type-enabled-unsupported.json");

const THINKING_TYPE_PATH: &str = "thinking.type.enabled";

fn thinking_type_key() -> String {
    crate::field_capability::field_capability_key(THINKING_TYPE_PATH)
        .expect("the grounded path is a qualified path")
}

/// The captured Bedrock body, as committed.
fn enabled_fixture_body() -> String {
    let fixture: serde_json::Value =
        serde_json::from_str(THINKING_ENABLED_FIXTURE).expect("the fixture is JSON");
    fixture["body"]
        .as_str()
        .expect("the fixture carries its body")
        .to_string()
}

/// The captured rejection exactly as bedrock-runtime returned it.
#[cfg(feature = "bedrock")]
fn bedrock_enabled_rejection() -> Error {
    Error::upstream_with_body(
        "thinking-seat",
        400,
        routectl_core::UpstreamBody::new(enabled_fixture_body()),
        None,
        Some("ValidationException".to_string()),
        None,
    )
}

/// The same refusal in the Anthropic Messages envelope: the captured message
/// with the Converse model-errors wrapper removed.
fn anthropic_enabled_rejection() -> Error {
    let captured: serde_json::Value =
        serde_json::from_str(&enabled_fixture_body()).expect("the body is JSON");
    let message = routectl_core::strip_converse_errors_prefix(
        captured["message"].as_str().expect("a flat message"),
    );
    Error::upstream(
        "thinking-seat",
        400,
        json!({
            "type": "error",
            "error": {"type": "invalid_request_error", "message": message},
        })
        .to_string(),
    )
}

/// How a [`ThinkingTypeSeat`] answers a body that is not refused.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AfterRefusal {
    Serve,
    Unavailable,
}

/// A seat that assembles each attempt's body with the real Anthropic egress,
/// deposits it into the attempt's wire tap, refuses it while its
/// `thinking.type` is one of `refused`, and otherwise answers `after`.
struct ThinkingTypeSeat {
    refused: &'static [&'static str],
    rejection: fn() -> Error,
    after: AfterRefusal,
    seen: Mutex<Vec<(ChatRequest, serde_json::Value)>>,
}

impl ThinkingTypeSeat {
    /// A seat refusing `thinking.type: enabled` when `refuses`.
    fn new(refuses: bool, rejection: fn() -> Error, after: AfterRefusal) -> Self {
        let refused: &'static [&'static str] = if refuses { &["enabled"] } else { &[] };
        Self::refusing_types(refused, rejection, after)
    }

    fn refusing_types(
        refused: &'static [&'static str],
        rejection: fn() -> Error,
        after: AfterRefusal,
    ) -> Self {
        Self {
            refused,
            rejection,
            after,
            seen: Mutex::new(Vec::new()),
        }
    }

    fn bodies(&self) -> Vec<serde_json::Value> {
        self.seen
            .lock()
            .iter()
            .map(|(_, body)| body.clone())
            .collect()
    }

    fn requests(&self) -> Vec<ChatRequest> {
        self.seen
            .lock()
            .iter()
            .map(|(req, _)| req.clone())
            .collect()
    }

    fn answer(&self, req: &ChatRequest) -> Result<()> {
        let egress = routectl_providers::anthropic_api::AnthropicApiProvider::new(
            routectl_providers::anthropic_api::AnthropicApiConfig::new("thinking-seat", "k"),
        );
        let mut body = egress.normalize_request(req)?;
        if let Some(object) = body.as_object_mut() {
            object.remove("stream");
        }
        if let Some(tap) = req.routectl_internal.wire_tap.as_ref() {
            tap.deposit(serde_json::to_vec(&body).expect("serialize").into());
        }
        let refused = body["thinking"]["type"]
            .as_str()
            .is_some_and(|thinking_type| self.refused.contains(&thinking_type));
        self.seen.lock().push((req.clone(), body));
        if refused {
            return Err((self.rejection)());
        }
        match self.after {
            AfterRefusal::Serve => Ok(()),
            AfterRefusal::Unavailable => Err(Error::upstream(
                "thinking-seat",
                503,
                r#"{"error":{"type":"api_error","message":"unavailable"}}"#,
            )),
        }
    }
}

#[async_trait::async_trait]
impl Provider for ThinkingTypeSeat {
    fn id(&self) -> &'static str {
        "thinking-seat"
    }
    fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
        Ok(json!({}))
    }
    fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
        Err(Error::normalize_response("thinking-seat", "unused"))
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

const ANTHROPIC_API_TOML: &str = "kind = \"anthropic-api\"\napi_key_ref = \"literal:k\"\n";

#[cfg(feature = "bedrock")]
const BEDROCK_TOML: &str =
    "kind = \"bedrock\"\nregion = \"us-west-2\"\ncreds = { kind = \"default-chain\" }\n";

/// A router over one seat per `(provider_toml, seat)`, in chain order, with
/// `extra_toml` appended to the config, and durable capability writes assumed
/// so a committed verdict can act on the next request.
fn thinking_chain(seats: &[(&str, Arc<ThinkingTypeSeat>)], extra_toml: &str) -> Router {
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
                (
                    format!("m{idx}"),
                    Arc::new(ResolvedModel::new(
                        format!("m{idx}"),
                        format!("p{idx}"),
                        Arc::clone(seat) as Arc<dyn Provider>,
                        "upstream",
                    )),
                )
            })
            .collect(),
    );
    router.with_capability_writes_assumed_durable_for_tests()
}

fn refusing_seat(rejection: fn() -> Error, after: AfterRefusal) -> Arc<ThinkingTypeSeat> {
    Arc::new(ThinkingTypeSeat::new(true, rejection, after))
}

/// The client's Anthropic `thinking` object, minus `display`.
fn native_thinking(thinking: serde_json::Value) -> routectl_core::AnthropicThinking {
    routectl_core::AnthropicThinking::Sent(
        routectl_core::ThinkingObject::from_client(thinking).expect("an object builds a carrier"),
    )
}

/// A request on [`ALIAS`] whose canonical reasoning is the legacy enabled
/// shape and whose native carrier is `carrier`.
fn thinking_req(carrier: routectl_core::AnthropicThinking) -> ChatRequest {
    let mut req = req_on(ALIAS);
    req.max_tokens = Some(4096);
    req.reasoning = Some(ReasoningConfig {
        effort: None,
        max_tokens: Some(2048),
        exclude: Some(false),
        enabled: Some(true),
    });
    req.routectl_internal.anthropic_thinking_display = None;
    req.routectl_internal.anthropic_thinking = carrier;
    req
}

/// The Anthropic client's `thinking: {type: enabled, budget_tokens: 2048,
/// display: summarized}`, as the ingress splits it.
fn anthropic_enabled_req() -> ChatRequest {
    let mut req = thinking_req(native_thinking(
        json!({"type": "enabled", "budget_tokens": 2048}),
    ));
    req.routectl_internal.anthropic_thinking_display = Some("summarized".into());
    req
}

/// [`anthropic_enabled_req`] with the client's `output_config.effort` set to
/// `effort`, lifted into the canonical effort as the ingress does.
fn anthropic_enabled_req_with_effort(effort: &str) -> ChatRequest {
    let mut req = anthropic_enabled_req();
    req.provider_extras = Some(json!({"output_config": {"effort": effort}}));
    if let Some(reasoning) = req.reasoning.as_mut() {
        reasoning.effort = Some(effort.to_string());
    }
    req
}

/// The same thinking intent from a translating ingress: no native carrier.
fn cross_dialect_enabled_req() -> ChatRequest {
    thinking_req(routectl_core::AnthropicThinking::NotCaptured)
}

/// The repaired retry's wire shape: adaptive thinking keeping only
/// `display`, and the effort it pairs with.
fn assert_adaptive_retry(body: &serde_json::Value, effort: &str, label: &str) {
    assert_eq!(
        body["thinking"],
        json!({"type": "adaptive", "display": "summarized"}),
        "{label}: the retry sends adaptive thinking with display and nothing else",
    );
    assert_eq!(
        body["output_config"]["effort"],
        json!(effort),
        "{label}: the retry pairs adaptive thinking with a positive effort",
    );
}

// ---------------------------------------------------------------------------
// Presence
// ---------------------------------------------------------------------------

#[test]
fn the_thinking_type_surface_is_present_only_when_the_egress_emits_enabled() {
    let surface = super::FieldSurface::AnthropicThinkingType("enabled");
    let unthinking = || {
        let mut req = thinking_req(routectl_core::AnthropicThinking::NotCaptured);
        req.reasoning = None;
        req
    };
    let rows: [(&str, ChatRequest, bool); 6] = [
        ("native enabled", anthropic_enabled_req(), true),
        ("translated enabled", cross_dialect_enabled_req(), true),
        (
            "native disabled",
            thinking_req(native_thinking(json!({"type": "disabled"}))),
            false,
        ),
        (
            "native adaptive",
            thinking_req(native_thinking(json!({"type": "adaptive"}))),
            false,
        ),
        (
            "client sent no thinking",
            thinking_req(routectl_core::AnthropicThinking::SentNone),
            false,
        ),
        ("translated, no reasoning", unthinking(), false),
    ];

    for (name, req, present) in rows {
        assert_eq!(surface.present_in(&req), present, "{name}");
    }
}

#[test]
fn no_two_closed_table_rows_share_a_surface() {
    let rows = super::closed_table();
    for (idx, row) in rows.iter().enumerate() {
        for other in &rows[idx + 1..] {
            assert_ne!(
                row.surface, other.surface,
                "`{}` and `{}` would be one mutation under two identities",
                row.path, other.path,
            );
        }
    }
    assert!(
        rows.iter()
            .any(|row| row.surface == super::FieldSurface::AnthropicThinkingType("enabled")),
        "control: the thinking-type row is in the scanned table",
    );
}

#[test]
fn the_thinking_type_row_has_no_recheck_payload() {
    let req = anthropic_enabled_req();
    assert!(
        super::FieldSurface::AnthropicThinkingType("enabled").present_in(&req),
        "premise: the request carries the row",
    );

    let grounded: Vec<_> =
        super::recheck_payloads(crate::router::FieldRepairTable::grounded_only(), &req).collect();

    assert!(
        grounded.is_empty(),
        "a row no token count validates is never re-checked: {grounded:?}",
    );
    // Control: the same scan yields a payload for a count-validated row the
    // request carries, so the empty answer above is about the row.
    let mut control = req;
    control.chat_template_kwargs = Some(json!({ super::TEST_RECHECK_KWARG: "v" }));
    assert_eq!(
        super::recheck_payloads(crate::router::FieldRepairTable::default(), &control)
            .map(|(path, _)| path)
            .collect::<Vec<_>>(),
        vec![super::TEST_RECHECK_PATH],
    );
}

// ---------------------------------------------------------------------------
// The rewrite itself
// ---------------------------------------------------------------------------

/// Mutation check: drop the positive-effort half of `rewrite_is_complete` ->
/// red.
#[test]
fn an_adaptive_rewrite_without_a_positive_effort_is_incomplete() {
    let surface = super::FieldSurface::AnthropicThinkingType("enabled");
    let mut rewritten = thinking_req(native_thinking(json!({"type": "adaptive"})));
    let target = "thinking.type.adaptive";
    assert_eq!(
        surface.value_in(&rewritten).as_deref(),
        Some(target),
        "premise: the type reads back as the target",
    );

    assert!(!surface.rewrite_is_complete(&rewritten, target));
    rewritten.reasoning.as_mut().expect("reasoning").effort = Some("none".into());
    assert!(
        !surface.rewrite_is_complete(&rewritten, target),
        "none is off"
    );
    rewritten.reasoning.as_mut().expect("reasoning").effort = Some("low".into());
    assert!(surface.rewrite_is_complete(&rewritten, target));
    rewritten.provider_extras = Some(json!({"output_config": {"effort": "none"}}));
    assert!(
        !surface.rewrite_is_complete(&rewritten, target),
        "a client none the egress merges over the canonical effort turns the body off",
    );
    rewritten.reasoning.as_mut().expect("reasoning").effort = None;
    rewritten.provider_extras = Some(json!({"output_config": {"effort": "high"}}));
    assert!(
        surface.rewrite_is_complete(&rewritten, target),
        "a client effort the egress merges onto the wire counts on its own",
    );
}

/// A positive effort only the client's `output_config` carries (a translating
/// ingress that swept it into the extras) stays as sent: the companion is for
/// a request with no positive effort at all.
///
/// Mutation check: read only the canonical effort in `carries_positive_effort`
/// -> red (the client's high is overwritten with the companion).
#[tokio::test]
async fn a_positive_client_effort_outside_the_canonical_one_stays_as_sent() {
    let seat = refusing_seat(anthropic_enabled_rejection, AfterRefusal::Serve);
    let router = thinking_chain(&[(ANTHROPIC_API_TOML, Arc::clone(&seat))], "");
    let mut req = cross_dialect_enabled_req();
    req.provider_extras = Some(json!({"output_config": {"effort": "high"}}));
    assert_eq!(
        req.reasoning.as_ref().and_then(|r| r.effort.as_deref()),
        None,
        "premise: no canonical effort",
    );

    let dispatched = router
        .complete_with_options(req, RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    let bodies = seat.bodies();
    assert_eq!(bodies.len(), 2);
    assert_adaptive_retry(&bodies[1], "high", "client extras effort");
}

#[test]
fn a_rewrite_to_a_type_that_is_not_adaptive_is_written_bare() {
    let surface = super::FieldSurface::AnthropicThinkingType("disabled");
    let mut req = thinking_req(native_thinking(json!({"type": "disabled", "extra": 1})));
    req.routectl_internal.anthropic_thinking_display = Some("summarized".into());

    assert!(surface.rewrite_in(&mut req, "thinking.type.between_tools"));

    let routectl_core::AnthropicThinking::Sent(object) = &req.routectl_internal.anthropic_thinking
    else {
        panic!("the rewrite writes a native carrier");
    };
    assert_eq!(
        serde_json::Value::Object(object.as_object().clone()),
        json!({"type": "between_tools"}),
    );
    assert_eq!(
        req.routectl_internal.anthropic_thinking_display, None,
        "between_tools takes no display",
    );
    assert_eq!(
        req.reasoning.as_ref().and_then(|r| r.effort.as_deref()),
        None,
        "only an adaptive target pairs with the companion effort",
    );
}

// ---------------------------------------------------------------------------
// Dispatch: repair, commit, and the next request
// ---------------------------------------------------------------------------

/// Name, request, and the effort the repaired retry carries.
type RewriteRow = (&'static str, fn() -> ChatRequest, &'static str);

/// Mutation checks: write the adaptive target through `set_thinking_type` ->
/// red (budget_tokens survives on the retry); stop writing the companion over
/// the client's `output_config.effort` -> red on the "none" row (the egress
/// turns the body off, so no retry is sent); stop moving a derived display
/// onto the carrier -> red on the cross-dialect row (display is lost).
#[tokio::test]
async fn an_enabled_rejection_retries_once_with_adaptive_thinking_and_commits() {
    let rows: [RewriteRow; 4] = [
        ("native carrier, no effort", anthropic_enabled_req, "low"),
        ("cross-dialect request", cross_dialect_enabled_req, "low"),
        (
            "client effort none",
            || anthropic_enabled_req_with_effort("none"),
            "low",
        ),
        (
            "client effort high",
            || anthropic_enabled_req_with_effort("high"),
            "high",
        ),
    ];

    for (name, req, effort) in rows {
        let seat = refusing_seat(anthropic_enabled_rejection, AfterRefusal::Serve);
        let router = thinking_chain(&[(ANTHROPIC_API_TOML, Arc::clone(&seat))], "");

        let dispatched = router
            .complete_with_options(req(), RouterOptions::new())
            .await;

        assert!(
            dispatched.result.is_ok(),
            "{name}: {:?}",
            dispatched.result.err()
        );
        let bodies = seat.bodies();
        assert_eq!(bodies.len(), 2, "{name}: one refused attempt, one retry");
        assert_eq!(bodies[0]["thinking"]["type"], "enabled", "{name}");
        assert_adaptive_retry(&bodies[1], effort, name);
        assert_eq!(
            dispatched
                .meta
                .learned_capabilities
                .iter()
                .map(|event| (event.capability_key.clone(), event.value))
                .collect::<Vec<_>>(),
            vec![(
                thinking_type_key(),
                Some(crate::VerdictValue::Shape("thinking.type.adaptive")),
            )],
            "{name}: the successful retry commits the named shape, not the effort",
        );
        assert_eq!(
            dispatched.meta.field_repairs[0].action,
            super::FIELD_ACTION_REWRITE_REPAIR,
            "{name}",
        );
    }
}

/// The committed Bedrock envelope drives the same repair on a Bedrock lane
/// whose body is the Anthropic-shape one.
#[cfg(feature = "bedrock")]
#[tokio::test]
async fn the_captured_bedrock_rejection_repairs_a_bedrock_lane() {
    let seat = refusing_seat(bedrock_enabled_rejection, AfterRefusal::Serve);
    let router = thinking_chain(&[(BEDROCK_TOML, Arc::clone(&seat))], "");

    let dispatched = router
        .complete_with_options(anthropic_enabled_req(), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    let bodies = seat.bodies();
    assert_eq!(bodies.len(), 2);
    assert_adaptive_retry(&bodies[1], "low", "bedrock");
    assert_eq!(dispatched.meta.learned_capabilities.len(), 1);
    assert_eq!(
        dispatched.meta.learned_capabilities[0].provider_kind,
        "bedrock"
    );
}

/// Mutation check: make the pre-flight planner skip rewrite rows -> red (the
/// second request is refused before it is repaired).
#[tokio::test]
async fn the_committed_rewrite_preflights_the_next_request_with_no_rejection() {
    let seat = refusing_seat(anthropic_enabled_rejection, AfterRefusal::Serve);
    let router = thinking_chain(&[(ANTHROPIC_API_TOML, Arc::clone(&seat))], "");
    let first = router
        .complete_with_options(anthropic_enabled_req(), RouterOptions::new())
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
        THINKING_TYPE_PATH,
        ANTHROPIC_API_KIND,
    )
    .expect("a qualified path mints a key");
    let cadence = router
        .field_verdicts()
        .canaries()
        .snapshot(&key)
        .expect("the confirmation seeded the canary state")
        .cadence;
    assert!(cadence > 1, "premise: the next request is not the canary");

    let second = router
        .complete_with_options(anthropic_enabled_req(), RouterOptions::new())
        .await;

    assert!(second.result.is_ok(), "{:?}", second.result.err());
    let bodies = seat.bodies();
    assert_eq!(bodies.len(), 3, "the second request is sent once");
    assert_adaptive_retry(&bodies[2], "low", "pre-flight");
    assert!(
        second.meta.field_repairs.is_empty(),
        "no rejection, so no reactive repair",
    );
    assert!(
        second
            .meta
            .field_preflight
            .iter()
            .any(|record| record.acted && record.field_path == Some(THINKING_TYPE_PATH)),
        "the pre-flight rewrite acted: {:?}",
        second.meta.field_preflight,
    );
}

// ---------------------------------------------------------------------------
// A rewrite that does not take
// ---------------------------------------------------------------------------

/// A refused-value rejection on the non-taking row naming its one member.
fn non_taking_rejection() -> Error {
    let body = json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "message": format!(
                "{}: Input should be '{}'",
                super::TEST_NON_TAKING_PATH,
                super::TEST_NON_TAKING_ACCEPTED[0],
            ),
        },
    });
    Error::upstream("field-mock", 400, body.to_string())
}

/// A request carrying only the non-taking row's field.
fn non_taking_req() -> ChatRequest {
    let mut req = req_on(ALIAS);
    req.reasoning = None;
    req.routectl_internal.anthropic_thinking_display = None;
    req.chat_template_kwargs = Some(json!({ super::TEST_NON_TAKING_KWARG: "client-value" }));
    req
}

/// Mutation check: delete the read-back check in `rewrite_in` -> red (the
/// rewrite is adopted, the budget is drawn, and a retry is sent).
#[test]
fn a_rewrite_that_does_not_read_back_draws_no_budget_and_reports_no_repair() {
    let (router, _seen) = single_seat(ALIAS, Answer::ServeImmediately);
    let mut req = non_taking_req();
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
            super::FieldSettlementMode::Settling,
            std::time::Instant::now(),
        )
        .expect("the non-taking field admits a plan");
    let mut budget = crate::router::repair_budget::RepairBudget::per_request();
    let mut meta = crate::router::DispatchMeta::for_alias(ALIAS);
    let before = serde_json::to_string(&req).expect("serializable");

    let applied = plan.apply(
        &mut req,
        &mut meta,
        &mut budget,
        &FailureClass::BadRequest,
        &non_taking_rejection(),
        ANTHROPIC_API_KIND,
    );

    assert!(
        applied.is_none(),
        "a rewrite that did not take is no repair"
    );
    assert!(plan.repaired_path().is_none());
    assert_eq!(serde_json::to_string(&req).expect("serializable"), before);
    let spent = (0..=usize::from(REPAIRS_PER_REQUEST))
        .filter(|_| budget.draw())
        .count();
    assert_eq!(
        spent,
        usize::from(REPAIRS_PER_REQUEST),
        "nothing was charged, so the whole ceiling remains",
    );
}

/// The same refusal through a dispatch: the rejection reaches the client and
/// no retry is sent.
#[tokio::test]
async fn a_rewrite_that_does_not_read_back_sends_no_retry() {
    struct NonTakingSeat {
        calls: AtomicUsize,
    }
    #[async_trait::async_trait]
    impl Provider for NonTakingSeat {
        fn id(&self) -> &'static str {
            "field-mock"
        }
        fn normalize_request(&self, _: &ChatRequest) -> Result<serde_json::Value> {
            Ok(json!({}))
        }
        fn normalize_response(&self, _: serde_json::Value) -> Result<ChatResponse> {
            Err(Error::normalize_response("field-mock", "unused"))
        }
        async fn complete(&self, _: ChatRequest) -> Result<ChatResponse> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err(non_taking_rejection())
        }
        async fn stream(&self, _: ChatRequest) -> Result<BoxStream<'static, Result<ChatChunk>>> {
            unreachable!("the test completes")
        }
    }
    let seat = Arc::new(NonTakingSeat {
        calls: AtomicUsize::new(0),
    });
    let mut router = Router::new(Arc::new(chain_config(ALIAS, 1)));
    router.install_resolved_models(BTreeMap::from([(
        "m0".to_string(),
        Arc::new(ResolvedModel::new(
            "m0",
            "p0",
            Arc::clone(&seat) as Arc<dyn Provider>,
            "wire-0",
        )),
    )]));

    let dispatched = router
        .complete_with_options(non_taking_req(), RouterOptions::new())
        .await;

    assert!(
        dispatched.result.is_err(),
        "the rejection reaches the client"
    );
    assert_eq!(seat.calls.load(Ordering::SeqCst), 1, "no retry was sent");
    assert!(dispatched.meta.field_repairs.is_empty());
    assert_eq!(router.field_repair_counters().repair_attempted, 0);
}

// ---------------------------------------------------------------------------
// Fallback isolation
// ---------------------------------------------------------------------------

/// Mutation check: repair the shared request instead of the attempt's clone
/// -> red (the fallback receives adaptive).
#[tokio::test]
async fn a_failed_repaired_retry_leaves_the_fallback_the_original_type() {
    let primary = refusing_seat(anthropic_enabled_rejection, AfterRefusal::Unavailable);
    let fallback = Arc::new(ThinkingTypeSeat::new(
        false,
        anthropic_enabled_rejection,
        AfterRefusal::Serve,
    ));
    let router = thinking_chain(
        &[
            (ANTHROPIC_API_TOML, Arc::clone(&primary)),
            (ANTHROPIC_API_TOML, Arc::clone(&fallback)),
        ],
        "",
    );

    let dispatched = router
        .complete_with_options(anthropic_enabled_req(), RouterOptions::new())
        .await;

    assert!(dispatched.result.is_ok(), "{:?}", dispatched.result.err());
    let primary_bodies = primary.bodies();
    assert_eq!(primary_bodies.len(), 2, "the primary repaired once");
    assert_adaptive_retry(&primary_bodies[1], "low", "primary retry");
    assert!(
        dispatched.meta.learned_capabilities.is_empty(),
        "a failed repaired retry commits nothing",
    );
    let fallback_requests = fallback.requests();
    assert_eq!(fallback_requests.len(), 1);
    assert_eq!(
        fallback_requests[0].routectl_internal.anthropic_thinking,
        native_thinking(json!({"type": "enabled", "budget_tokens": 2048})),
        "the fallback target receives the client's own carrier",
    );
    assert_eq!(
        fallback.bodies()[0]["thinking"],
        json!({"type": "enabled", "budget_tokens": 2048, "display": "summarized"}),
    );
}

// ---------------------------------------------------------------------------
// Wire check
// ---------------------------------------------------------------------------

/// The client body [`anthropic_enabled_req`] stands for.
fn anthropic_enabled_client_body() -> serde_json::Value {
    json!({
        "model": ALIAS,
        "max_tokens": 4096,
        "messages": [{"role": "user", "content": "hello"}],
        "thinking": {"type": "enabled", "budget_tokens": 2048, "display": "summarized"},
    })
}

fn wire_checked_enabled_req() -> ChatRequest {
    let mut req = anthropic_enabled_req();
    req.routectl_internal.ingress_wire = Some(routectl_core::wire_check::IngressWire {
        dialect: routectl_core::wire_check::IngressDialect::Anthropic,
        client_version: None,
        bytes: serde_json::to_vec(&anthropic_enabled_client_body())
            .expect("serialize")
            .into(),
    });
    req
}

const WIRE_CHECK_EVERY: &str = "[wire_check]\nfirst_n = 8\nsample_one_in = 1\n";

/// Mutation checks: stop declaring the dropped thinking members -> red
/// (`/thinking/budget_tokens` is an undeclared loss); declare the type
/// rewrite twice -> red (the declaration count).
#[tokio::test]
async fn the_repaired_attempt_declares_each_edit_once_and_checks_clean() {
    use crate::wire_check_state::CheckOutcome;
    use routectl_core::wire_check::{EditKind, EditSite};

    // Control: the same client body served without a repair checks clean, so
    // the request stands for that body and the comparison can see a loss.
    let control_seat = Arc::new(ThinkingTypeSeat::new(
        false,
        anthropic_enabled_rejection,
        AfterRefusal::Serve,
    ));
    let control = thinking_chain(&[(ANTHROPIC_API_TOML, control_seat)], WIRE_CHECK_EVERY);
    Box::pin(control.complete(wire_checked_enabled_req()))
        .await
        .expect("the control serves");
    let control_snapshot = control.wire_check_snapshot();
    assert!(
        control_snapshot.loss.is_empty(),
        "control: {:?}",
        control_snapshot.findings
    );
    assert!(
        control_snapshot
            .checks
            .iter()
            .any(|check| check.outcome == CheckOutcome::Clean),
        "control: the unrepaired body checks clean",
    );

    let seat = refusing_seat(anthropic_enabled_rejection, AfterRefusal::Serve);
    let router = thinking_chain(&[(ANTHROPIC_API_TOML, Arc::clone(&seat))], WIRE_CHECK_EVERY);

    Box::pin(router.complete(wire_checked_enabled_req()))
        .await
        .expect("the repaired retry serves");

    let requests = seat.requests();
    assert_eq!(requests.len(), 2);
    let mut repairs: Vec<(EditSite, String, EditKind)> = requests[1]
        .routectl_internal
        .declared_edits
        .iter()
        .filter(|edit| edit.site() == EditSite::FieldRepair)
        .map(|edit| (edit.site(), edit.path().to_owned(), edit.kind()))
        .collect();
    repairs.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(
        repairs,
        vec![
            (
                EditSite::FieldRepair,
                "/thinking/budget_tokens".to_owned(),
                EditKind::Remove
            ),
            (
                EditSite::FieldRepair,
                "/thinking/type".to_owned(),
                EditKind::Rewrite
            ),
        ],
    );
    let snapshot = router.wire_check_snapshot();
    assert!(snapshot.loss.is_empty(), "{:?}", snapshot.findings);
    assert!(
        snapshot
            .checks
            .iter()
            .any(|check| check.outcome == CheckOutcome::Clean),
        "the repaired attempt checks clean: {:?}",
        snapshot.checks,
    );
}

/// A client `output_config.effort` the companion replaced is declared as a
/// rewrite by the repair; one the client never sent is the egress's to
/// declare.
#[tokio::test]
async fn a_replaced_client_effort_is_declared_by_the_repair_and_checks_clean() {
    use routectl_core::wire_check::{EditKind, EditSite};

    let mut client_body = anthropic_enabled_client_body();
    client_body["output_config"] = json!({"effort": "none"});
    let mut req = anthropic_enabled_req_with_effort("none");
    req.routectl_internal.ingress_wire = Some(routectl_core::wire_check::IngressWire {
        dialect: routectl_core::wire_check::IngressDialect::Anthropic,
        client_version: None,
        bytes: serde_json::to_vec(&client_body).expect("serialize").into(),
    });
    let seat = refusing_seat(anthropic_enabled_rejection, AfterRefusal::Serve);
    let router = thinking_chain(&[(ANTHROPIC_API_TOML, Arc::clone(&seat))], WIRE_CHECK_EVERY);

    Box::pin(router.complete(req))
        .await
        .expect("the repaired retry serves");

    let requests = seat.requests();
    assert_eq!(requests.len(), 2);
    let effort_edits: Vec<(EditSite, EditKind)> = requests[1]
        .routectl_internal
        .declared_edits
        .iter()
        .filter(|edit| edit.path() == "/output_config/effort")
        .map(|edit| (edit.site(), edit.kind()))
        .collect();
    assert_eq!(
        effort_edits,
        vec![(EditSite::FieldRepair, EditKind::Rewrite)],
        "declared once, by the repair",
    );
    let snapshot = router.wire_check_snapshot();
    assert!(snapshot.loss.is_empty(), "{:?}", snapshot.findings);
}
