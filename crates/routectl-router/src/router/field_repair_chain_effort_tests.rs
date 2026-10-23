// Ranked effort repair through the dispatch walk on anthropic-api: a refused
// level is retried at the level the refusal's list picks, and learned only
// beside a thinking shape the refusal cannot have depended on; a refusal of
// the effort field itself, or one the parser cannot attribute, repairs
// nothing; an effort the egress injected is repaired like one the client
// sent; and a refusal of the replacement is repaired again at a level not yet
// sent, every refused level learning the level that went out.
// An `include!`d FRAGMENT of `field_repair_chain_tests.rs`, inside its
// `repair_chain` module, sharing its seats, rejections and readers.

const XHIGH_PATH: &str = "output_config.effort.xhigh";
const LOW_PATH: &str = "output_config.effort.low";
const MEDIUM_PATH: &str = "output_config.effort.medium";
const HIGH_PATH: &str = "output_config.effort.high";
const MAX_PATH: &str = "output_config.effort.max";

/// The captured opus-4-5 refusal of `xhigh`, listing `high, low, medium`.
fn xhigh_refused() -> Error {
    Error::upstream(
        "chain-seat",
        400,
        disabled_fixture_body(include_str!(
            "../../tests/fixtures/rejections/anthropic-api/effort-xhigh-unsupported-opus-4-5.json"
        )),
    )
}

/// The `Supported levels` template refusing `low` on a model that lists only
/// `high` and `medium`. Synthesized from the captured template: no capture
/// refuses `low`.
fn low_refused() -> Error {
    anthropic_rejection(
        "This model does not support effort level 'low'. Supported levels: high, medium.",
    )
}

/// The same template refusing `high`, listing `low` and `medium`.
/// Synthesized from the captured template.
fn high_refused() -> Error {
    anthropic_rejection(
        "This model does not support effort level 'high'. Supported levels: low, medium.",
    )
}

fn sent_effort(body: &serde_json::Value) -> Option<&str> {
    body["output_config"]["effort"].as_str()
}

/// Refuse an `xhigh` effort, else serve.
fn refuse_xhigh(body: &serde_json::Value, _: usize) -> Option<Error> {
    (sent_effort(body) == Some("xhigh")).then(xhigh_refused)
}

/// The Anthropic client's `output_config.effort` beside `thinking` (absent
/// for `None`), as the ingress parses it: the client's `output_config` kept
/// in the extras, its effort lifted onto the canonical one, the thinking
/// object on the native carrier, and the client bytes for the wire check.
fn effort_req(effort: &str, thinking: Option<serde_json::Value>) -> ChatRequest {
    let mut req = req_on(ALIAS);
    req.max_tokens = Some(4096);
    let mut client = json!({
        "model": ALIAS,
        "max_tokens": 4096,
        "messages": [{"role": "user", "content": "hello"}],
        "output_config": {"effort": effort},
    });
    let enabled = match thinking.as_ref().and_then(|t| t["type"].as_str()) {
        Some("adaptive" | "enabled") => Some(true),
        Some("disabled") => Some(false),
        _ => None,
    };
    req.reasoning = Some(ReasoningConfig {
        effort: Some(effort.to_string()),
        max_tokens: thinking
            .as_ref()
            .and_then(|t| t["budget_tokens"].as_u64())
            .map(|budget| u32::try_from(budget).expect("a small budget")),
        exclude: None,
        enabled,
    });
    req.provider_extras = Some(json!({"output_config": {"effort": effort}}));
    req.routectl_internal.anthropic_thinking_display = None;
    req.routectl_internal.anthropic_thinking = match thinking {
        Some(thinking) => {
            client["thinking"] = thinking.clone();
            native_thinking(thinking)
        }
        None => routectl_core::AnthropicThinking::SentNone,
    };
    req.routectl_internal.ingress_wire = Some(routectl_core::wire_check::IngressWire {
        dialect: routectl_core::wire_check::IngressDialect::Anthropic,
        client_version: None,
        bytes: serde_json::to_vec(&client).expect("serialize").into(),
    });
    req
}

/// Acknowledge every verdict `dispatched` learned as durably written, so it
/// may act on the next request.
fn acknowledge(router: &Router, dispatched: &crate::router::Dispatched) {
    for event in &dispatched.meta.learned_capabilities {
        assert!(
            router.acknowledge_durable_field_confirmation(
                &event.state_key,
                &event.capability_key,
                &event.provider_kind,
                event.persistence_generation,
                event.incarnation,
                event.observations,
            ),
            "premise: the durable write of {} acknowledges",
            event.capability_key,
        );
    }
}

fn effort_rejection_outcomes(router: &Router) -> Vec<crate::wire_check_state::FieldRepairOutcome> {
    router
        .wire_check_snapshot()
        .findings
        .iter()
        .filter_map(|finding| finding.field_repair_outcome)
        .collect()
}

// -----------------------------------------------------------------------
// One refused level, repaired and learned
// -----------------------------------------------------------------------

/// An adaptive request's `xhigh` refused with `Supported levels: high, low,
/// medium.` is retried at `high` with the thinking object byte for byte as
/// sent; the success learns `xhigh -> high`, and the next request goes out
/// at `high` with no refusal first.
///
/// Mutation checks: make the ranked arm of `FieldEdit::for_rejection` answer
/// `None` -> red (the refusal reaches the client); compare row path to
/// rejection path in `field_repair_outcome` -> red on the outcome.
#[tokio::test]
async fn a_refused_level_is_retried_at_the_nearest_lower_listed_level_and_learned() {
    use crate::wire_check_state::FieldRepairOutcome;

    let seat = ChainSeat::new(Egress::Anthropic, refuse_xhigh);
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));
    let adaptive = || effort_req("xhigh", Some(json!({"type": "adaptive"})));

    let first = router
        .complete_with_options(adaptive(), RouterOptions::new())
        .await;

    assert!(first.result.is_ok(), "{:?}", first.result.as_ref().err());
    let bodies = seat.bodies();
    assert_eq!(bodies.len(), 2, "one refused attempt, one retry");
    assert_eq!(sent_effort(&bodies[0]), Some("xhigh"), "premise");
    assert_eq!(sent_effort(&bodies[1]), Some("high"));
    assert_eq!(
        serde_json::to_vec(&bodies[1]["thinking"]).expect("serialize"),
        serde_json::to_vec(&bodies[0]["thinking"]).expect("serialize"),
        "the thinking object is byte-identical after the effort repair",
    );
    assert_eq!(
        bodies[0]["thinking"],
        json!({"type": "adaptive"}),
        "premise"
    );
    assert_eq!(
        verdicts(&first.meta),
        vec![(key(XHIGH_PATH), Some(VerdictValue::Shape("high")))],
    );
    assert_eq!(succeeded(&first.meta), vec![(XHIGH_PATH, true, true)]);
    assert_eq!(
        effort_rejection_outcomes(&router),
        vec![FieldRepairOutcome::RepairedLossy],
        "the retry served a level the client did not send",
    );

    acknowledge(&router, &first);
    let second = router
        .complete_with_options(adaptive(), RouterOptions::new())
        .await;

    assert!(second.result.is_ok(), "{:?}", second.result.as_ref().err());
    let bodies = seat.bodies();
    assert_eq!(bodies.len(), 3, "the second request is sent once");
    assert_eq!(sent_effort(&bodies[2]), Some("high"));
    assert!(
        second.meta.field_repairs.is_empty(),
        "no refusal, no repair"
    );
    assert!(
        second
            .meta
            .field_preflight
            .iter()
            .any(|record| record.acted && record.field_path == Some(XHIGH_PATH)),
        "the learned level was applied before dispatch: {:?}",
        second.meta.field_preflight,
    );
}

/// The thinking shape the refused body sent decides whether the repaired
/// level is learned: adaptive and enabled learn; disabled, between_tools and
/// no thinking object repair the attempt and commit nothing, since the
/// refusal may hold only beside that shape. The thinking object ships
/// unchanged in every row.
///
/// Mutation check: make `Disabled` mintable in `ranked_step_mints` -> red on
/// `disabled`.
#[tokio::test]
async fn a_level_refused_beside_a_shape_it_may_depend_on_repairs_and_learns_nothing() {
    let rows: [(&str, Option<serde_json::Value>, bool); 5] = [
        ("adaptive", Some(json!({"type": "adaptive"})), true),
        (
            "enabled",
            Some(json!({"type": "enabled", "budget_tokens": 2048})),
            true,
        ),
        ("disabled", Some(json!({"type": "disabled"})), false),
        (
            "between_tools",
            Some(json!({"type": "between_tools"})),
            false,
        ),
        ("no thinking object", None, false),
    ];

    for (name, thinking, learns) in rows {
        let seat = ChainSeat::new(Egress::Anthropic, refuse_xhigh);
        let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

        let dispatched = router
            .complete_with_options(effort_req("xhigh", thinking), RouterOptions::new())
            .await;

        assert!(
            dispatched.result.is_ok(),
            "{name}: {:?}",
            dispatched.result.as_ref().err()
        );
        let bodies = seat.bodies();
        assert_eq!(bodies.len(), 2, "{name}: one refused attempt, one retry");
        assert_eq!(sent_effort(&bodies[1]), Some("high"), "{name}");
        assert_eq!(bodies[1]["thinking"], bodies[0]["thinking"], "{name}");
        let expected = if learns {
            vec![(key(XHIGH_PATH), Some(VerdictValue::Shape("high")))]
        } else {
            vec![]
        };
        assert_eq!(verdicts(&dispatched.meta), expected, "{name}");
        assert_eq!(
            router.field_verdicts().snapshot_len(),
            usize::from(learns),
            "{name}"
        );
        assert_eq!(
            succeeded(&dispatched.meta),
            vec![(XHIGH_PATH, true, learns)],
            "{name}",
        );
    }
}

// -----------------------------------------------------------------------
// Refusals that repair nothing
// -----------------------------------------------------------------------

/// The captured OpenRouter refusal of an effort value: its accepted set sits
/// only inside a string under `metadata.raw`.
fn openrouter_effort_refused() -> Error {
    Error::upstream(
        "chain-seat",
        400,
        disabled_fixture_body(include_str!(
            "../../tests/fixtures/rejections/anthropic-api/effort-invalid-value-openrouter.json"
        )),
    )
}

/// The bare refusal of the effort field itself, which lists no level.
fn effort_field_refused() -> Error {
    anthropic_rejection("output_config.effort: Extra inputs are not permitted")
}

/// A refusal of the effort field, which names no level to move to, and the
/// OpenRouter refusal, which localizes nothing, each reach the client as
/// sent: no retry, no repair record, and no verdict on any effort key, the
/// bare field key included.
#[tokio::test]
async fn an_effort_refusal_naming_no_level_repairs_and_learns_nothing() {
    let rows: [(&str, Respond); 2] = [
        ("bare extra inputs", |body, _| {
            sent_effort(body).map(|_| effort_field_refused())
        }),
        ("openrouter", |body, _| {
            sent_effort(body).map(|_| openrouter_effort_refused())
        }),
    ];

    for (name, respond) in rows {
        let seat = ChainSeat::new(Egress::Anthropic, respond);
        let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

        let dispatched = router
            .complete_with_options(
                effort_req("xhigh", Some(json!({"type": "adaptive"}))),
                RouterOptions::new(),
            )
            .await;

        assert!(dispatched.result.is_err(), "{name}: the refusal stands");
        assert_eq!(seat.calls(), 1, "{name}: nothing to retry");
        assert!(dispatched.meta.field_repairs.is_empty(), "{name}");
        assert!(
            dispatched.meta.learned_capabilities.is_empty(),
            "{name}: {:?}",
            verdicts(&dispatched.meta),
        );
        assert_eq!(router.field_verdicts().snapshot_len(), 0, "{name}");
        let bare = key("output_config.effort");
        assert!(
            router
                .learned_capability_snapshot()
                .iter()
                .all(|entry| entry.feature_key != bare && !entry.feature_key.contains("effort")),
            "{name}: no effort key is resident",
        );
    }
}

/// Refuse disabled thinking, then an `xhigh` effort with the captured list,
/// then the `high` that repair wrote.
fn disabled_xhigh_then_high(body: &serde_json::Value, _: usize) -> Option<Error> {
    if thinking_type(body) == Some("disabled") {
        return Some(anthropic_disabled_rejection());
    }
    match sent_effort(body) {
        Some("xhigh") => Some(effort_level_refused()),
        Some("high") => Some(high_refused()),
        _ => None,
    }
}

/// A refusal of the level a ranked repair wrote is repaired again, at a level
/// the request has not sent: `xhigh` -> `high` -> `medium`, served. The
/// thinking step the first effort refusal proved commits there; both effort
/// steps commit `medium` at the success, each under its own key.
///
/// Mutation check: make a refusal of the pending effort read as moved-on
/// (skip ranked steps in `moved_on_by`) -> red (the `xhigh` step commits
/// `high` at the `high` refusal, so the effort field is committed and that
/// refusal reaches the client).
#[tokio::test]
async fn a_refusal_of_the_replacement_level_is_repaired_again_and_both_levels_learn() {
    let seat = ChainSeat::new(Egress::Anthropic, disabled_xhigh_then_high);
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

    let dispatched = router
        .complete_with_options(disabled_req(Some("xhigh")), RouterOptions::new())
        .await;

    assert!(
        dispatched.result.is_ok(),
        "{:?}",
        dispatched.result.as_ref().err()
    );
    let sent: Vec<_> = seat
        .bodies()
        .iter()
        .map(|b| sent_effort(b).map(str::to_string))
        .collect();
    assert_eq!(
        sent,
        ["xhigh", "xhigh", "high", "medium"].map(|level| Some(level.to_string())),
        "disabled, then xhigh and high refused, then medium served",
    );
    assert_eq!(router.repairs_per_request(), one_request_in(3));
    let medium = Some(VerdictValue::Shape("medium"));
    assert_eq!(
        verdicts(&dispatched.meta),
        vec![
            (
                thinking_disabled_key(),
                Some(VerdictValue::Shape("thinking.type.adaptive")),
            ),
            (key(XHIGH_PATH), medium),
            (key(HIGH_PATH), medium),
        ],
    );
    assert_eq!(
        succeeded(&dispatched.meta),
        vec![
            (THINKING_DISABLED_PATH, true, true),
            (XHIGH_PATH, true, true),
            (HIGH_PATH, true, true),
        ],
    );
}

/// The captured opus-4-5 shape refusing `max`, listing every lower level.
/// Synthesized from the captured template: no capture refuses `max`.
fn max_refused() -> Error {
    anthropic_rejection(
        "This model does not support effort level 'max'. Supported levels: high, low, medium, xhigh.",
    )
}

/// Refuse `max` listing `xhigh`, then `xhigh` with the captured list, else
/// serve.
fn refuse_max_then_xhigh(body: &serde_json::Value, _: usize) -> Option<Error> {
    match sent_effort(body) {
        Some("max") => Some(max_refused()),
        Some("xhigh") => Some(xhigh_refused()),
        _ => None,
    }
}

/// One refused level after another: `max` -> `xhigh` -> `high`, served. Each
/// refused level has its own step and draws once; the success commits `high`
/// under both keys, and both steps end committed.
///
/// Mutation check: drop the on-demand admission of a ranked row the plan did
/// not carry (`candidate_for` answers only admitted candidates) -> red (the
/// `xhigh` refusal finds no candidate and reaches the client).
#[tokio::test]
async fn each_refused_level_in_a_chain_learns_the_level_that_succeeded() {
    let seat = ChainSeat::new(Egress::Anthropic, refuse_max_then_xhigh);
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

    let dispatched = router
        .complete_with_options(
            effort_req("max", Some(json!({"type": "adaptive"}))),
            RouterOptions::new(),
        )
        .await;

    assert!(
        dispatched.result.is_ok(),
        "{:?}",
        dispatched.result.as_ref().err()
    );
    let sent: Vec<_> = seat
        .bodies()
        .iter()
        .map(|b| sent_effort(b).map(str::to_string))
        .collect();
    assert_eq!(
        sent,
        ["max", "xhigh", "high"].map(|level| Some(level.to_string()))
    );
    assert_eq!(router.repairs_per_request(), one_request_in(2), "two draws");
    let high = Some(VerdictValue::Shape("high"));
    assert_eq!(
        verdicts(&dispatched.meta),
        vec![(key(MAX_PATH), high), (key(XHIGH_PATH), high)],
    );
    assert_eq!(
        succeeded(&dispatched.meta),
        vec![(MAX_PATH, true, true), (XHIGH_PATH, true, true)],
    );
    assert_eq!(step_exits(&router), (2, 0));
}

/// The `Supported levels` template refusing `medium`, listing `high` and
/// `low`. Synthesized from the captured template.
fn medium_refused() -> Error {
    anthropic_rejection(
        "This model does not support effort level 'medium'. Supported levels: high, low.",
    )
}

/// Refuse `low` listing `high, medium`, then `medium` listing `high, low`,
/// else serve.
fn refuse_low_then_medium(body: &serde_json::Value, _: usize) -> Option<Error> {
    match sent_effort(body) {
        Some("low") => Some(low_refused()),
        Some("medium") => Some(medium_refused()),
        _ => None,
    }
}

/// A level the request already sent is never picked again: `low` refused
/// moves up to `medium`; `medium` refused lists `low` below it, which was
/// sent, so the repair moves up to `high` and is served.
///
/// Mutation check: exclude only the refused level in `ranked_edit_value`
/// (drop the sent levels) -> red (the second repair goes back to `low`).
#[tokio::test]
async fn a_level_already_sent_is_never_picked_again_in_a_chain() {
    let seat = ChainSeat::new(Egress::Anthropic, refuse_low_then_medium);
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

    let dispatched = router
        .complete_with_options(
            effort_req("low", Some(json!({"type": "adaptive"}))),
            RouterOptions::new(),
        )
        .await;

    assert!(
        dispatched.result.is_ok(),
        "{:?}",
        dispatched.result.as_ref().err()
    );
    let sent: Vec<_> = seat
        .bodies()
        .iter()
        .map(|b| sent_effort(b).map(str::to_string))
        .collect();
    assert_eq!(
        sent,
        ["low", "medium", "high"].map(|level| Some(level.to_string()))
    );
    let high = Some(VerdictValue::Shape("high"));
    assert_eq!(
        verdicts(&dispatched.meta),
        vec![(key(LOW_PATH), high), (key(MEDIUM_PATH), high)],
    );
}

/// The template refusing whatever level was sent, listing every other rank
/// member: an upstream that refuses each replacement in turn.
fn refuse_every_level(body: &serde_json::Value, _: usize) -> Option<Error> {
    let sent = sent_effort(body)?;
    let others: Vec<_> = routectl_providers::effort::VALID_EFFORT_TOKENS
        .iter()
        .filter(|level| **level != sent)
        .copied()
        .collect();
    let mut listed = others;
    listed.sort_unstable();
    Some(anthropic_rejection(&format!(
        "This model does not support effort level '{sent}'. Supported levels: {}.",
        listed.join(", ")
    )))
}

/// An upstream that refuses every level is answered with distinct levels
/// until the request's budget is spent, and never loops: every body that
/// went out carries a level no earlier body carried, the walk stops at the
/// budget with the refusal, and nothing is learned.
#[tokio::test]
async fn refusing_every_level_ends_at_the_budget_with_every_sent_level_distinct() {
    let seat = ChainSeat::new(Egress::Anthropic, refuse_every_level);
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

    let dispatched = router
        .complete_with_options(
            effort_req("max", Some(json!({"type": "adaptive"}))),
            RouterOptions::new(),
        )
        .await;

    assert!(dispatched.result.is_err(), "every level is refused");
    let sent: Vec<_> = seat
        .bodies()
        .iter()
        .filter_map(|b| sent_effort(b).map(str::to_string))
        .collect();
    assert_eq!(
        sent,
        ["max", "xhigh", "high", "medium", "low", "minimal"].map(str::to_string),
        "each level once, nearest lower first",
    );
    assert_eq!(
        router.repairs_per_request(),
        one_request_in(usize::from(REPAIRS_PER_REQUEST)),
    );
    assert!(dispatched.meta.learned_capabilities.is_empty());
    assert_eq!(step_exits(&router), (0, u64::from(REPAIRS_PER_REQUEST)));
}

// -----------------------------------------------------------------------
// An effort the egress injected
// -----------------------------------------------------------------------

/// Refuse disabled thinking, then a `max_tokens` above the limit, then a
/// `low` effort, else serve.
fn disabled_ceiling_then_low(body: &serde_json::Value, _: usize) -> Option<Error> {
    if thinking_type(body) == Some("disabled") {
        return Some(anthropic_disabled_rejection());
    }
    if sent_max_tokens(body).is_some_and(|sent| sent > u64::from(MODEL_LIMIT)) {
        return Some(max_tokens_rejection());
    }
    (sent_effort(body) == Some("low")).then(low_refused)
}

/// A client output_config holding a structured-output format and no effort.
fn format_config() -> serde_json::Value {
    json!({"format": {
        "type": "json_schema",
        "schema": {"type": "object", "properties": {}, "additionalProperties": false},
    }})
}

/// An Anthropic client's disabled thinking with no effort and `max_tokens`
/// above the model limit, plus `output_config` when given.
fn injected_effort_req(output_config: Option<serde_json::Value>) -> ChatRequest {
    let mut req = anthropic_req(SENT_MAX_TOKENS, false);
    let mut client = disabled_client_body(None);
    client["max_tokens"] = json!(SENT_MAX_TOKENS);
    if let Some(config) = output_config {
        req.provider_extras = Some(json!({"output_config": config}));
        client["output_config"] = config;
    }
    if let Some(wire) = req.routectl_internal.ingress_wire.as_mut() {
        wire.bytes = serde_json::to_vec(&client).expect("serialize").into();
    }
    req
}

/// A learned `disabled -> adaptive` verdict rewrites a later request's
/// disabled thinking before dispatch and pairs adaptive with the companion
/// `low` effort, which the egress writes into `output_config`: declared at
/// `/output_config` when the client sent none, and at `/output_config/effort`
/// when the client's own `output_config` held another key. A refusal of that
/// injected `low` is repaired like a client's effort in both shapes, and in
/// both the pending ceiling step it proved commits first.
///
/// Mutation check: drop `ranks_declared_effort` from `answer_declared` -> red
/// on `client output_config` (the declared injection answers the refusal
/// with `Proceed`, so neither the ceiling nor the effort commits); `no client
/// output_config` stays green, its declaration matching nothing.
#[tokio::test]
async fn an_injected_effort_refused_with_a_level_list_repairs_and_commits_the_pending_step() {
    let rows = [
        ("no client output_config", None),
        ("client output_config", Some(format_config())),
    ];

    for (name, output_config) in rows {
        let seat = ChainSeat::new(Egress::Anthropic, disabled_ceiling_then_low);
        let router = anthropic_router(&seat, &wire_checked(THREE_ATTEMPTS));
        let taught = router
            .complete_with_options(disabled_req(Some("high")), RouterOptions::new())
            .await;
        assert!(
            taught.result.is_ok(),
            "{name}: premise: the shape is learned"
        );
        acknowledge(&router, &taught);
        let taught_calls = seat.calls();

        let dispatched = router
            .complete_with_options(injected_effort_req(output_config), RouterOptions::new())
            .await;

        assert!(
            dispatched.result.is_ok(),
            "{name}: {:?}",
            dispatched.result.as_ref().err()
        );
        let bodies = &seat.bodies()[taught_calls..];
        assert_eq!(bodies.len(), 3, "{name}: ceiling, then low, then served");
        assert_eq!(
            thinking_type(&bodies[0]),
            Some("adaptive"),
            "{name}: premise"
        );
        assert_eq!(
            sent_effort(&bodies[1]),
            Some("low"),
            "{name}: premise: the injected effort went out"
        );
        assert_eq!(
            sent_max_tokens(&bodies[1]),
            Some(u64::from(MODEL_LIMIT)),
            "{name}"
        );
        assert_eq!(
            sent_effort(&bodies[2]),
            Some("medium"),
            "{name}: only higher levels are listed, so the nearest higher"
        );
        assert_eq!(
            verdicts(&dispatched.meta),
            vec![
                (
                    key(MAX_TOKENS_PATH),
                    Some(VerdictValue::Ceiling(MODEL_LIMIT))
                ),
                (key(LOW_PATH), Some(VerdictValue::Shape("medium"))),
            ],
            "{name}",
        );
    }
}

/// An effort written after admission is repaired too: a reactive thinking
/// rewrite writes the companion `low`, which the walk's admission never saw,
/// and its refusal is admitted on demand, commits the pending rewrite, and
/// is repaired to `medium`, which the success learns.
///
/// Mutation check: drop the on-demand admission in `candidate_for` -> red
/// (the refusal of `low` reaches the client).
#[tokio::test]
async fn a_refused_companion_effort_written_after_admission_is_repaired_and_learned() {
    let seat = ChainSeat::new(Egress::Anthropic, |body, _| {
        if thinking_type(body) == Some("disabled") {
            return Some(anthropic_disabled_rejection());
        }
        (sent_effort(body) == Some("low")).then(low_refused)
    });
    let router = anthropic_router(&seat, &wire_checked(ONE_ATTEMPT));

    let dispatched = router
        .complete_with_options(disabled_req(None), RouterOptions::new())
        .await;

    assert!(
        dispatched.result.is_ok(),
        "{:?}",
        dispatched.result.as_ref().err()
    );
    let bodies = seat.bodies();
    assert_eq!(
        bodies.len(),
        3,
        "disabled refused, then the companion, then served"
    );
    assert_eq!(sent_effort(&bodies[1]), Some("low"), "premise");
    assert_eq!(sent_effort(&bodies[2]), Some("medium"));
    assert_eq!(
        verdicts(&dispatched.meta),
        vec![
            (
                thinking_disabled_key(),
                Some(VerdictValue::Shape("thinking.type.adaptive")),
            ),
            (key(LOW_PATH), Some(VerdictValue::Shape("medium"))),
        ],
    );
    assert_eq!(
        succeeded(&dispatched.meta),
        vec![(THINKING_DISABLED_PATH, true, true), (LOW_PATH, true, true)],
    );
}
