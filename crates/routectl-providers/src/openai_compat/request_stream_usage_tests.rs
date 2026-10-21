// `stream_options.include_usage` forced onto a finished streaming body: the
// client's value is overridden and counted, the operator's is kept.
//
// `include!`d into the `tests` module of `request.rs`; imports live there.

const OVERRIDDEN_CLASS: &str = "client_stream_include_usage_overridden";

/// The streaming body for `req` after the merge and the force, plus the
/// override counter's delta across both.
fn forced_body_and_delta(req: &ChatRequest, config_extras: Option<&Value>) -> (Value, u64) {
    let before = compat_policy_count(OVERRIDDEN_CLASS);
    let mut body = normalize(
        "test",
        req,
        ReasoningDialect::Passthrough,
        HistoryReasoning::Auto,
        config_extras,
        false,
    )
    .expect("lenient normalize must succeed");
    force_stream_include_usage("test", &mut body, req, config_extras);
    (body, compat_policy_count(OVERRIDDEN_CLASS) - before)
}

fn chat_ingress_req(extras: Value) -> ChatRequest {
    let mut req = simple_req("m");
    req.routectl_internal.provenance = RequestProvenance::OpenaiIngress;
    req.provider_extras = Some(extras);
    req
}

#[test]
fn a_client_include_usage_false_is_forced_true_declared_and_counted_once() {
    // Arrange
    let req = chat_ingress_req(json!({"stream_options": {"include_usage": false}}));

    // Act
    let (body, delta) = forced_body_and_delta(&req, None);

    // Assert
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    assert_eq!(delta, 1);
}

#[test]
fn a_client_stream_options_without_the_flag_gains_it_uncounted() {
    let req = chat_ingress_req(json!({"stream_options": {"other": 1}}));

    let (body, delta) = forced_body_and_delta(&req, None);

    assert_eq!(
        body["stream_options"],
        json!({"include_usage": true, "other": 1})
    );
    assert_eq!(delta, 0);
}

#[test]
fn a_body_without_stream_options_gains_the_object_uncounted() {
    let req = simple_req("m");

    let (body, delta) = forced_body_and_delta(&req, None);

    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    assert_eq!(delta, 0);
}

#[test]
fn a_client_stream_options_that_is_not_an_object_is_replaced_and_counted() {
    let req = chat_ingress_req(json!({"stream_options": "on"}));

    let (body, delta) = forced_body_and_delta(&req, None);

    assert_eq!(body["stream_options"], json!({"include_usage": true}));
    assert_eq!(delta, 1);
}

#[test]
fn the_operator_include_usage_false_wins_over_the_force() {
    // The router records the operator's `payload_extras` beside the merged
    // bag it lands on `provider_extras`.
    let operator = json!({"stream_options": {"include_usage": false}});
    let mut req = chat_ingress_req(operator.clone());
    req.routectl_internal.operator_payload_extras = Some(std::sync::Arc::new(operator));

    let (body, delta) = forced_body_and_delta(&req, None);

    assert_eq!(body["stream_options"], json!({"include_usage": false}));
    assert_eq!(delta, 0);
}

#[test]
fn the_config_payload_extras_include_usage_false_wins_over_the_force() {
    let config = json!({"stream_options": {"include_usage": false}});
    let req = simple_req("m");

    let (body, delta) = forced_body_and_delta(&req, Some(&config));

    assert_eq!(body["stream_options"], json!({"include_usage": false}));
    assert_eq!(delta, 0);
}

#[test]
fn an_operator_stream_options_without_the_flag_does_not_shield_a_client_false() {
    // Only an operator value for the flag itself is the operator's choice; a
    // sibling key under the same object leaves the client's value to the force.
    let mut req = chat_ingress_req(json!({"stream_options": {"include_usage": false}}));
    req.routectl_internal.operator_payload_extras =
        Some(std::sync::Arc::new(json!({"stream_options": {"other": 1}})));

    let (body, delta) = forced_body_and_delta(&req, None);

    assert_eq!(body["stream_options"]["include_usage"], json!(true));
    assert_eq!(delta, 1);
}
