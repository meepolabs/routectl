//! Bedrock egress withholds the client-lifted `anthropic_beta` flags AWS
//! rejects outright, on both carriers and in both allowlist modes, while the
//! operator's per-provider `anthropic_beta` floor still forwards them.

#![cfg(feature = "bedrock")]

mod common;

use routectl_core::{ChatRequest, Provider};
use routectl_providers::bedrock::{
    BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth::ResolvedCreds,
};
use routectl_providers::translation_drop_metrics::translation_drop_snapshot;
use serde_json::{Value, json};
use serial_test::serial;

const REJECTED_BETAS: [&str; 3] = [
    "advanced-tool-use-2025-11-20",
    "advisor-tool-2026-03-01",
    "prompt-caching-scope-2026-01-05",
];
const ACCEPTED_CLIENT_BETA: &str = "interleaved-thinking-2025-05-14";
const DROP_CLASS: &str = "anthropic_beta_rejected_by_bedrock";

/// The Converse lane's counter for the withheld set. Every test in this
/// binary that reaches the Converse arm is serialized on the same guard,
/// because the registry is process-global and the runner is threaded.
fn converse_drop_count() -> u64 {
    translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == "bedrock-converse" && e.drop_class == DROP_CLASS)
        .map_or(0, |e| e.drop_count)
}

fn provider(
    api_shape: BedrockApiShape,
    anthropic_beta: Vec<String>,
    allowed_betas: Vec<String>,
) -> BedrockProvider {
    let cfg = BedrockConfig {
        id: "bedrock-rejected-betas-test".into(),
        region: "us-east-1".into(),
        model_id: "anthropic.claude-opus-4-6-v1".into(),
        api_shape,
        creds: BedrockCreds::BearerKey {
            key: "test-key".into(),
        },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta,
        allowed_betas,
        allowed_body_fields: Vec::new(),
        additional_model_request_fields: None,
        adaptive_thinking: None,
    };
    let resolved = ResolvedCreds::Bearer {
        key: "test-key".into(),
    };
    BedrockProvider::new(cfg, resolved).expect("canonical region")
}

/// A request whose client-lifted betas are the three rejected flags plus one
/// AWS accepts, in the order a client would send them.
fn request_with_client_betas() -> ChatRequest {
    let mut betas: Vec<String> = vec![ACCEPTED_CLIENT_BETA.into()];
    betas.extend(REJECTED_BETAS.iter().map(|b| (*b).to_string()));
    ChatRequest {
        model: "anthropic.claude-opus-4-6-v1".into(),
        messages: vec![common::user_msg("hello")].into(),
        max_tokens: Some(64),
        anthropic_beta: betas,
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

fn assert_empty_allowlist_withholds_rejected_betas(api_shape: BedrockApiShape) {
    // Arrange
    let provider = provider(api_shape, Vec::new(), Vec::new());

    // Act
    let betas = wire_betas(&provider, api_shape);

    // Assert
    assert_eq!(
        betas,
        json!([ACCEPTED_CLIENT_BETA]),
        "pass-through mode must still withhold the AWS-rejected flags and keep the rest"
    );
}

fn assert_non_empty_allowlist_naming_a_rejected_beta_still_withholds_it(
    api_shape: BedrockApiShape,
) {
    // Arrange: the operator allowlist names a rejected flag, which is not the
    // escape hatch -- only the per-provider floor is.
    let provider = provider(
        api_shape,
        Vec::new(),
        vec![ACCEPTED_CLIENT_BETA.into(), REJECTED_BETAS[0].into()],
    );

    // Act
    let betas = wire_betas(&provider, api_shape);

    // Assert
    assert_eq!(betas, json!([ACCEPTED_CLIENT_BETA]));
}

fn assert_operator_floor_forwards_a_rejected_beta(api_shape: BedrockApiShape) {
    // Arrange
    let floor = REJECTED_BETAS[1];
    let provider = provider(api_shape, vec![floor.into()], Vec::new());

    // Act
    let betas = wire_betas(&provider, api_shape);

    // Assert: the floor flag rides; the other two rejected client flags are
    // still withheld. Membership rather than exact order: in pass-through
    // mode the Invoke floor merge does not dedup a floor flag the client
    // also sent.
    let shipped: Vec<&str> = betas
        .as_array()
        .expect("anthropic_beta array on the wire")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(shipped.contains(&floor), "floor flag missing: {shipped:?}");
    assert!(shipped.contains(&ACCEPTED_CLIENT_BETA), "{shipped:?}");
    for withheld in [REJECTED_BETAS[0], REJECTED_BETAS[2]] {
        assert!(
            !shipped.contains(&withheld),
            "{withheld} shipped: {shipped:?}"
        );
    }
}

#[test]
fn invoke_empty_allowlist_withholds_bedrock_rejected_client_betas() {
    assert_empty_allowlist_withholds_rejected_betas(BedrockApiShape::Invoke);
}

#[test]
#[serial(bedrock_converse_anthropic_beta_rejected_by_bedrock)]
fn converse_empty_allowlist_withholds_bedrock_rejected_client_betas() {
    assert_empty_allowlist_withholds_rejected_betas(BedrockApiShape::Converse);
}

#[test]
fn invoke_allowlist_naming_a_bedrock_rejected_beta_still_withholds_it() {
    assert_non_empty_allowlist_naming_a_rejected_beta_still_withholds_it(BedrockApiShape::Invoke);
}

#[test]
#[serial(bedrock_converse_anthropic_beta_rejected_by_bedrock)]
fn converse_allowlist_naming_a_bedrock_rejected_beta_still_withholds_it() {
    assert_non_empty_allowlist_naming_a_rejected_beta_still_withholds_it(BedrockApiShape::Converse);
}

#[test]
fn invoke_operator_floor_forwards_a_bedrock_rejected_beta() {
    assert_operator_floor_forwards_a_rejected_beta(BedrockApiShape::Invoke);
}

#[test]
#[serial(bedrock_converse_anthropic_beta_rejected_by_bedrock)]
fn converse_operator_floor_forwards_a_bedrock_rejected_beta() {
    assert_operator_floor_forwards_a_rejected_beta(BedrockApiShape::Converse);
}

#[test]
fn a_request_carrying_only_rejected_client_betas_ships_no_beta_field() {
    // Arrange
    let provider = provider(BedrockApiShape::Invoke, Vec::new(), Vec::new());
    let req = ChatRequest {
        anthropic_beta: REJECTED_BETAS.iter().map(|b| (*b).to_string()).collect(),
        ..request_with_client_betas()
    };

    // Act
    let body = provider.normalize_request(&req).expect("bedrock normalize");

    // Assert
    assert!(
        body.get("anthropic_beta").is_none(),
        "an all-rejected beta list must not ship as an empty array; got {body}"
    );
}

#[test]
#[serial(bedrock_converse_anthropic_beta_rejected_by_bedrock)]
fn bedrock_rejected_beta_withhold_bumps_the_drop_counter_once() {
    // Arrange: three rejected flags in one request are one drop event.
    let provider = provider(BedrockApiShape::Converse, Vec::new(), Vec::new());
    let before = converse_drop_count();

    // Act
    let _ = wire_betas(&provider, BedrockApiShape::Converse);

    // Assert
    assert_eq!(converse_drop_count() - before, 1);
}

#[test]
#[serial(bedrock_converse_anthropic_beta_rejected_by_bedrock)]
fn converse_request_without_rejected_betas_counts_no_withhold() {
    // Arrange: the floor asserts all three, so nothing is withheld.
    let floor: Vec<String> = REJECTED_BETAS.iter().map(|b| (*b).to_string()).collect();
    let provider = provider(BedrockApiShape::Converse, floor, Vec::new());
    let before = converse_drop_count();

    // Act
    let betas = wire_betas(&provider, BedrockApiShape::Converse);

    // Assert
    assert_eq!(
        betas,
        json!([
            REJECTED_BETAS[0],
            REJECTED_BETAS[1],
            REJECTED_BETAS[2],
            ACCEPTED_CLIENT_BETA
        ])
    );
    assert_eq!(converse_drop_count(), before);
}
