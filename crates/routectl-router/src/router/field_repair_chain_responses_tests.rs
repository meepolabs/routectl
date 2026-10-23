// Ranked effort repair through the dispatch walks on openai-responses: the
// captured codex refusal of `minimal` is retried at `low` and learned on both
// auth lanes (the body has no thinking object, so the refusal cannot have
// depended on one), on the complete walk and as a pre-content 400 on the
// stream walk; the same text at status 0, as a failed stream event surfaces
// it, repairs nothing.
// An `include!`d FRAGMENT of `field_repair_chain_tests.rs`, inside its
// `repair_chain::responses` module, sharing the chain seat and readers.

use routectl_providers::openai_responses::AuthKind;

const MINIMAL_PATH: &str = "output_config.effort.minimal";

/// The two auth lanes field repair attributes, each on its default base URL.
const RESPONSES_LANES: [(&str, &str, AuthKind); 2] = [
    (
        "chatgpt-oauth",
        "kind = \"openai-responses\"\napi_key_ref = \"literal:k\"\n\
         account_id_ref = \"literal:a\"\n",
        AuthKind::ChatgptOauth,
    ),
    (
        "api-key",
        "kind = \"openai-responses\"\napi_key_ref = \"literal:k\"\nauth_kind = \"api-key\"\n",
        AuthKind::ApiKey,
    ),
];

const MINIMAL_FIXTURE: &str = include_str!(
    "../../tests/fixtures/rejections/openai-responses/reasoning-effort-minimal-unsupported-codex.json"
);

/// The captured codex refusal of `minimal` at `status`, carrying the
/// `error.type` and `error.code` tokens the Responses egress lifts from its
/// body, as the egress builds it: the classifier reads those tokens, and
/// `unsupported_value` lifts the refusal out of `BadRequest`.
fn minimal_refused_at(status: u16) -> Error {
    let body = disabled_fixture_body(MINIMAL_FIXTURE);
    let envelope: serde_json::Value = serde_json::from_str(&body).expect("the body is JSON");
    let token = |field: &str| envelope["error"][field].as_str().map(str::to_owned);
    assert_eq!(
        token("code").as_deref(),
        Some("unsupported_value"),
        "premise: the capture's code"
    );
    Error::upstream_full(
        "chain-seat",
        status,
        body,
        None,
        token("type"),
        token("code"),
    )
}

/// The codex refusal of `minimal` at the HTTP status it was captured on.
fn minimal_refused() -> Error {
    minimal_refused_at(400)
}

/// The same refusal at status 0, the status a failed stream event surfaces on.
fn minimal_refused_at_status_zero() -> Error {
    minimal_refused_at(0)
}

fn responses_effort(body: &serde_json::Value) -> Option<&str> {
    body["reasoning"]["effort"].as_str()
}

fn refuse_minimal(body: &serde_json::Value, _: usize) -> Option<Error> {
    (responses_effort(body) == Some("minimal")).then(minimal_refused)
}

fn refuse_minimal_at_status_zero(body: &serde_json::Value, _: usize) -> Option<Error> {
    (responses_effort(body) == Some("minimal")).then(minimal_refused_at_status_zero)
}

/// A Responses client's `reasoning.effort` as the ingress parses it: the
/// canonical effort alone, and the client bytes for the wire check.
fn responses_effort_req(effort: &str) -> ChatRequest {
    let mut req = req_on(ALIAS);
    req.reasoning = Some(ReasoningConfig {
        effort: Some(effort.to_string()),
        ..Default::default()
    });
    req.provider_extras = None;
    req.routectl_internal.anthropic_thinking = routectl_core::AnthropicThinking::NotCaptured;
    req.routectl_internal.anthropic_thinking_display = None;
    let client = json!({
        "model": ALIAS,
        "input": "hello",
        "reasoning": {"effort": effort},
    });
    req.routectl_internal.ingress_wire = Some(routectl_core::wire_check::IngressWire {
        dialect: routectl_core::wire_check::IngressDialect::Responses,
        client_version: None,
        bytes: serde_json::to_vec(&client).expect("serialize").into(),
    });
    req
}

fn responses_router(provider_toml: &str, seat: &Arc<ChainSeat>) -> Router {
    chain_router(&[(provider_toml, seat)], &wire_checked(ONE_ATTEMPT))
}

/// Drive one request on the stream walk and drain it, so a committed stream
/// settles.
async fn stream_once(router: &Router, req: ChatRequest) -> (crate::router::DispatchMeta, bool) {
    let opened = router.stream_with_options(req, RouterOptions::new()).await;
    let served = match opened.result {
        Ok(body) => {
            let frames: Vec<Result<ChatChunk>> = body.collect().await;
            !frames.is_empty() && frames.iter().all(std::result::Result::is_ok)
        }
        Err(_) => false,
    };
    (opened.meta, served)
}

/// Assert the bodies of a repaired `minimal` request: the refused one, then
/// the retry at `low`.
fn assert_minimal_retried_at_low(seat: &ChainSeat, label: &str) {
    let bodies = seat.bodies();
    assert_eq!(bodies.len(), 2, "{label}: one refused attempt, one retry");
    assert_eq!(responses_effort(&bodies[0]), Some("minimal"), "{label}");
    assert_eq!(responses_effort(&bodies[1]), Some("low"), "{label}");
    assert!(
        bodies[1].get("thinking").is_none(),
        "{label}: no thinking object is created"
    );
}

/// The captured codex `unsupported_value` refusal of `minimal`, listing
/// `none, low, medium, high, xhigh, max`, is retried at `low` on both auth
/// lanes and learned as `.minimal = shape:low`; the next request goes out at
/// `low` with no refusal first.
///
/// Mutation check: drop the openai-responses `unsupported_value` arm of
/// `class_can_name_a_field` -> red (the refusal reaches the client).
#[tokio::test]
async fn a_refused_minimal_is_retried_at_low_and_learned_on_both_auth_lanes() {
    for (lane, provider_toml, auth_kind) in RESPONSES_LANES {
        let seat = ChainSeat::new(Egress::OpenAiResponses(auth_kind), refuse_minimal);
        let router = responses_router(provider_toml, &seat);

        let first = router
            .complete_with_options(responses_effort_req("minimal"), RouterOptions::new())
            .await;

        assert!(
            first.result.is_ok(),
            "{lane}: {:?}",
            first.result.as_ref().err()
        );
        assert_minimal_retried_at_low(&seat, lane);
        assert_eq!(
            verdicts(&first.meta),
            vec![(key(MINIMAL_PATH), Some(VerdictValue::Shape("low")))],
            "{lane}",
        );
        assert_eq!(
            succeeded(&first.meta),
            vec![(MINIMAL_PATH, true, true)],
            "{lane}"
        );

        acknowledge(&router, &first);
        let second = router
            .complete_with_options(responses_effort_req("minimal"), RouterOptions::new())
            .await;

        assert!(second.result.is_ok(), "{lane}");
        let bodies = seat.bodies();
        assert_eq!(bodies.len(), 3, "{lane}: the second request is sent once");
        assert_eq!(responses_effort(&bodies[2]), Some("low"), "{lane}");
        assert!(
            second.meta.field_repairs.is_empty(),
            "{lane}: no refusal, no repair"
        );
    }
}

/// Codex streams: a 400 before any content reaches the rejection arm with
/// its HTTP status and repairs like the complete walk.
#[tokio::test]
async fn a_pre_content_stream_refusal_of_minimal_repairs_and_learns() {
    for (lane, provider_toml, auth_kind) in RESPONSES_LANES {
        let seat = ChainSeat::new(Egress::OpenAiResponses(auth_kind), refuse_minimal);
        let router = responses_router(provider_toml, &seat);

        let (meta, served) = stream_once(&router, responses_effort_req("minimal")).await;

        assert!(served, "{lane}: the retried stream serves");
        assert_minimal_retried_at_low(&seat, lane);
        assert_eq!(
            verdicts(&meta),
            vec![(key(MINIMAL_PATH), Some(VerdictValue::Shape("low")))],
            "{lane}",
        );
    }
}

/// The same refusal text at status 0, as a failed stream event surfaces it,
/// is outside the rejection statuses: no retry, no repair, no verdict, on
/// either walk.
#[tokio::test]
async fn the_refusal_text_at_status_zero_repairs_nothing() {
    for (lane, provider_toml, auth_kind) in RESPONSES_LANES {
        for walk in ["complete", "stream"] {
            let label = format!("{lane} {walk}");
            let seat = ChainSeat::new(
                Egress::OpenAiResponses(auth_kind),
                refuse_minimal_at_status_zero,
            );
            let router = responses_router(provider_toml, &seat);

            let (meta, served) = if walk == "stream" {
                stream_once(&router, responses_effort_req("minimal")).await
            } else {
                let done = router
                    .complete_with_options(responses_effort_req("minimal"), RouterOptions::new())
                    .await;
                let served = done.result.is_ok();
                (done.meta, served)
            };

            assert!(!served, "{label}: the refusal stands");
            assert_eq!(seat.calls(), 1, "{label}: nothing to retry");
            assert!(meta.field_repairs.is_empty(), "{label}");
            assert!(meta.learned_capabilities.is_empty(), "{label}");
            assert_eq!(router.field_verdicts().snapshot_len(), 0, "{label}");
        }
    }
}
