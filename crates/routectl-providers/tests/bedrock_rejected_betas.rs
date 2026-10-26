//! Bedrock egress withholds the client-lifted `anthropic_beta` flags the
//! request's `routectl_internal.withheld_betas` names, on both carriers, while
//! the operator's per-provider `anthropic_beta` floor still forwards them and
//! every other client flag reaches the wire verbatim.

#![cfg(feature = "bedrock")]

mod common;

use routectl_core::{ChatRequest, Provider, RoutectlInternal};
use routectl_providers::bedrock::{
    BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth::ResolvedCreds,
};
use routectl_providers::translation_drop_metrics::translation_drop_snapshot;
use serde_json::{Value, json};
use serial_test::serial;

const WITHHELD_BETAS: [&str; 3] = [
    "zz-withheld-a-2099-01-01",
    "zz-withheld-b-2099-01-01",
    "zz-withheld-c-2099-01-01",
];
const ACCEPTED_CLIENT_BETA: &str = "interleaved-thinking-2025-05-14";
const DROP_CLASS: &str = "anthropic_beta_rejected_by_bedrock";

/// The withheld-set counter on `lane`. Every test in this binary that reaches
/// a lane's arm is serialized on that lane's guard, because the registry is
/// process-global and the runner is threaded.
fn drop_count(lane: &str) -> u64 {
    translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == lane && e.drop_class == DROP_CLASS)
        .map_or(0, |e| e.drop_count)
}

fn converse_drop_count() -> u64 {
    drop_count("bedrock-converse")
}

fn invoke_drop_count() -> u64 {
    drop_count("bedrock-invoke")
}

fn provider(api_shape: BedrockApiShape, anthropic_beta: Vec<String>) -> BedrockProvider {
    provider_for("anthropic.claude-opus-4-6-v1", api_shape, anthropic_beta)
}

fn provider_for(
    model_id: &str,
    api_shape: BedrockApiShape,
    anthropic_beta: Vec<String>,
) -> BedrockProvider {
    let cfg = BedrockConfig {
        id: "bedrock-rejected-betas-test".into(),
        region: "us-east-1".into(),
        model_id: model_id.into(),
        api_shape,
        creds: BedrockCreds::BearerKey {
            key: "test-key".into(),
        },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta,
        additional_model_request_fields: None,
    };
    let resolved = ResolvedCreds::Bearer {
        key: "test-key".into(),
    };
    BedrockProvider::new(cfg, resolved).expect("canonical region")
}

fn withheld_set() -> std::sync::Arc<[String]> {
    WITHHELD_BETAS.iter().map(|b| (*b).to_string()).collect()
}

/// A request whose client-lifted betas are the three withheld flags plus one
/// that is not, in the order a client would send them, with the three named
/// in its withheld set the way the router fills it.
fn request_with_client_betas() -> ChatRequest {
    let mut betas: Vec<String> = vec![ACCEPTED_CLIENT_BETA.into()];
    betas.extend(WITHHELD_BETAS.iter().map(|b| (*b).to_string()));
    let mut internal = RoutectlInternal::default();
    internal.withheld_betas = withheld_set();
    ChatRequest {
        model: "anthropic.claude-opus-4-6-v1".into(),
        messages: vec![common::user_msg("hello")].into(),
        max_tokens: Some(64),
        anthropic_beta: betas,
        routectl_internal: internal,
        ..Default::default()
    }
}

/// The `anthropic_beta` array exactly as it rides on the wire for this carrier.
fn wire_betas(provider: &BedrockProvider, api_shape: BedrockApiShape) -> Value {
    let body = provider
        .normalize_request(&request_with_client_betas())
        .expect("bedrock normalize");
    match api_shape {
        BedrockApiShape::Invoke => body["anthropic_beta"].clone(),
        BedrockApiShape::Converse => body["additionalModelRequestFields"]["anthropic_beta"].clone(),
        other => panic!("no wire location known for the beta array on {other:?}"),
    }
}

fn assert_withheld_betas_are_dropped_and_the_rest_kept(api_shape: BedrockApiShape) {
    // Arrange
    let provider = provider(api_shape, Vec::new());

    // Act
    let betas = wire_betas(&provider, api_shape);

    // Assert
    assert_eq!(
        betas,
        json!([ACCEPTED_CLIENT_BETA]),
        "the withheld flags must be dropped and every other client flag kept"
    );
}

fn assert_operator_floor_forwards_a_rejected_beta(api_shape: BedrockApiShape) {
    // Arrange
    let floor = WITHHELD_BETAS[1];
    let provider = provider(api_shape, vec![floor.into()]);

    // Act
    let betas = wire_betas(&provider, api_shape);

    // Assert: the floor flag rides; the other two withheld client flags are
    // still withheld. Membership rather than exact order: the floor merge
    // places a floor flag the client also sent in its floor position.
    let shipped: Vec<&str> = betas
        .as_array()
        .expect("anthropic_beta array on the wire")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(shipped.contains(&floor), "floor flag missing: {shipped:?}");
    assert!(shipped.contains(&ACCEPTED_CLIENT_BETA), "{shipped:?}");
    for withheld in [WITHHELD_BETAS[0], WITHHELD_BETAS[2]] {
        assert!(
            !shipped.contains(&withheld),
            "{withheld} shipped: {shipped:?}"
        );
    }
}

#[test]
#[serial(bedrock_invoke_anthropic_beta_rejected_by_bedrock)]
fn invoke_withholds_bedrock_rejected_client_betas() {
    assert_withheld_betas_are_dropped_and_the_rest_kept(BedrockApiShape::Invoke);
}

#[test]
#[serial(bedrock_converse_anthropic_beta_rejected_by_bedrock)]
fn converse_withholds_bedrock_rejected_client_betas() {
    assert_withheld_betas_are_dropped_and_the_rest_kept(BedrockApiShape::Converse);
}

#[test]
#[serial(bedrock_invoke_anthropic_beta_rejected_by_bedrock)]
fn invoke_operator_floor_forwards_a_bedrock_rejected_beta() {
    assert_operator_floor_forwards_a_rejected_beta(BedrockApiShape::Invoke);
}

#[test]
#[serial(bedrock_converse_anthropic_beta_rejected_by_bedrock)]
fn converse_operator_floor_forwards_a_bedrock_rejected_beta() {
    assert_operator_floor_forwards_a_rejected_beta(BedrockApiShape::Converse);
}

#[test]
#[serial(bedrock_invoke_anthropic_beta_rejected_by_bedrock)]
fn a_request_carrying_only_rejected_client_betas_ships_no_beta_field() {
    // Arrange
    let provider = provider(BedrockApiShape::Invoke, Vec::new());
    let req = ChatRequest {
        anthropic_beta: WITHHELD_BETAS.iter().map(|b| (*b).to_string()).collect(),
        ..request_with_client_betas()
    };

    // Act
    let body = provider.normalize_request(&req).expect("bedrock normalize");

    // Assert
    assert!(
        body.get("anthropic_beta").is_none(),
        "an all-withheld beta list must not ship as an empty array; got {body}"
    );
}

#[test]
#[serial(bedrock_converse_anthropic_beta_rejected_by_bedrock)]
fn bedrock_rejected_beta_withhold_bumps_the_drop_counter_once() {
    // Arrange: three withheld flags in one request are one drop event.
    let provider = provider(BedrockApiShape::Converse, Vec::new());
    let before = converse_drop_count();

    // Act
    let _ = wire_betas(&provider, BedrockApiShape::Converse);

    // Assert
    assert_eq!(converse_drop_count() - before, 1);
}

#[test]
#[serial(
    bedrock_invoke_anthropic_beta_rejected_by_bedrock,
    bedrock_converse_anthropic_beta_rejected_by_bedrock
)]
fn invoke_rejected_beta_withhold_bumps_the_drop_counter_once() {
    // Arrange: three withheld flags in one request are one drop event, on the
    // Invoke lane and not on Converse.
    let provider = provider(BedrockApiShape::Invoke, Vec::new());
    let (invoke_before, converse_before) = (invoke_drop_count(), converse_drop_count());

    // Act
    let betas = wire_betas(&provider, BedrockApiShape::Invoke);

    // Assert
    assert_eq!(betas, json!([ACCEPTED_CLIENT_BETA]));
    assert_eq!(invoke_drop_count() - invoke_before, 1);
    assert_eq!(converse_drop_count(), converse_before);
}

#[test]
#[serial(bedrock_invoke_anthropic_beta_rejected_by_bedrock)]
fn invoke_request_without_rejected_betas_counts_no_withhold() {
    // Arrange: the floor asserts all three, so nothing is withheld.
    let floor: Vec<String> = WITHHELD_BETAS.iter().map(|b| (*b).to_string()).collect();
    let provider = provider(BedrockApiShape::Invoke, floor);
    let before = invoke_drop_count();

    // Act
    let _ = wire_betas(&provider, BedrockApiShape::Invoke);

    // Assert
    assert_eq!(invoke_drop_count(), before);
}

#[test]
#[serial(bedrock_converse_anthropic_beta_rejected_by_bedrock)]
fn converse_request_without_rejected_betas_counts_no_withhold() {
    // Arrange: the floor asserts all three, so nothing is withheld.
    let floor: Vec<String> = WITHHELD_BETAS.iter().map(|b| (*b).to_string()).collect();
    let provider = provider(BedrockApiShape::Converse, floor);
    let before = converse_drop_count();

    // Act
    let betas = wire_betas(&provider, BedrockApiShape::Converse);

    // Assert
    assert_eq!(
        betas,
        json!([
            WITHHELD_BETAS[0],
            WITHHELD_BETAS[1],
            WITHHELD_BETAS[2],
            ACCEPTED_CLIENT_BETA
        ])
    );
    assert_eq!(converse_drop_count(), before);
}

/// The router unions a `header_extras["anthropic-beta"]`-pinned flag into
/// `anthropic_beta` and records it in `routectl_internal.operator_betas`;
/// that pin is operator-asserted, so the withhold must spare it.
fn assert_header_extras_pinned_rejected_beta_is_forwarded(api_shape: BedrockApiShape) {
    // Arrange
    let pinned = WITHHELD_BETAS[1];
    let provider = provider(api_shape, Vec::new());
    let mut req = request_with_client_betas();
    req.routectl_internal.operator_betas = vec![pinned.into()];

    // Act
    let body = provider.normalize_request(&req).expect("bedrock normalize");

    // Assert
    let betas = match api_shape {
        BedrockApiShape::Invoke => &body["anthropic_beta"],
        _ => &body["additionalModelRequestFields"]["anthropic_beta"],
    };
    assert_eq!(betas, &json!([ACCEPTED_CLIENT_BETA, pinned]));
}

#[test]
#[serial(bedrock_invoke_anthropic_beta_rejected_by_bedrock)]
fn invoke_header_extras_pinned_rejected_beta_is_forwarded() {
    assert_header_extras_pinned_rejected_beta_is_forwarded(BedrockApiShape::Invoke);
}

#[test]
#[serial(bedrock_converse_anthropic_beta_rejected_by_bedrock)]
fn converse_header_extras_pinned_rejected_beta_is_forwarded() {
    assert_header_extras_pinned_rejected_beta_is_forwarded(BedrockApiShape::Converse);
}

/// A caller that bypasses the router fills no withheld set, so every client
/// flag, including ones no list names, reaches the wire verbatim and in order.
fn assert_request_without_a_withheld_set_ships_every_client_beta(api_shape: BedrockApiShape) {
    // Arrange
    let provider = provider(api_shape, Vec::new());
    let mut req = request_with_client_betas();
    req.routectl_internal.withheld_betas = std::sync::Arc::default();
    let before = drop_count(api_shape.provider_kind_str());

    // Act
    let body = provider.normalize_request(&req).expect("bedrock normalize");

    // Assert
    let betas = match api_shape {
        BedrockApiShape::Invoke => &body["anthropic_beta"],
        _ => &body["additionalModelRequestFields"]["anthropic_beta"],
    };
    assert_eq!(
        betas,
        &json!([
            ACCEPTED_CLIENT_BETA,
            WITHHELD_BETAS[0],
            WITHHELD_BETAS[1],
            WITHHELD_BETAS[2]
        ])
    );
    assert_eq!(drop_count(api_shape.provider_kind_str()), before);
}

#[test]
#[serial(bedrock_invoke_anthropic_beta_rejected_by_bedrock)]
fn invoke_request_without_a_withheld_set_ships_every_client_beta() {
    assert_request_without_a_withheld_set_ships_every_client_beta(BedrockApiShape::Invoke);
}

#[test]
#[serial(bedrock_converse_anthropic_beta_rejected_by_bedrock)]
fn converse_request_without_a_withheld_set_ships_every_client_beta() {
    assert_request_without_a_withheld_set_ships_every_client_beta(BedrockApiShape::Converse);
}

#[test]
#[serial(bedrock_converse_anthropic_beta_rejected_by_bedrock)]
fn a_non_claude_converse_bag_with_every_client_beta_withheld_carries_none_and_counts_once() {
    // Arrange: a non-Claude model refuses the bag key itself, so the router
    // withholds every client flag the request carries.
    let model_id = "us.amazon.nova-2-lite-v1:0";
    let provider = provider_for(model_id, BedrockApiShape::Converse, Vec::new());
    let mut req = request_with_client_betas();
    req.model = model_id.into();
    req.routectl_internal.withheld_betas = req.anthropic_beta.iter().cloned().collect();
    let before = converse_drop_count();

    // Act
    let body = provider.normalize_request(&req).expect("bedrock normalize");

    // Assert
    assert!(
        body["additionalModelRequestFields"]
            .get("anthropic_beta")
            .is_none(),
        "the bag carries the refused key: {body}"
    );
    assert_eq!(converse_drop_count() - before, 1);
}
