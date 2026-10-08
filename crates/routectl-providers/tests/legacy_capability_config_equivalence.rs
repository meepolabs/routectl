//! Feature acceptance -- legacy-config egress-byte equivalence.
//!
//! The routing half of this acceptance bar (the `FilterSource` labels a
//! legacy `unsupported_features` list produces) lives in the router crate.
//! This file proves the OTHER half: the two legacy Bedrock egress allowlists
//! (`[bedrock] allowed_betas` and `[bedrock] allowed_body_fields`) emit
//! byte-identical wire output to the legacy baseline. The egress filters
//! were untouched by the per-provider capability migration; these tests pin
//! their absolute output so any accidental regression is caught.
//!
//! Each surface is exercised in BOTH modes the allowlists support:
//!
//! - EMPTY allowlist == pass-through: every requested beta and every
//!   forward-compat body field survives on the wire.
//! - NON-EMPTY allowlist: only the listed betas / fields survive; the rest
//!   drop before dispatch.
//!
//! Each surface pins the FULL assembled body via `insta` snapshots
//! (absolute expected bytes) plus targeted drop/keep assertions.

#![cfg(all(feature = "bedrock", feature = "anthropic-api"))]

mod common;

use routectl_core::{ChatRequest, Provider};
use routectl_providers::bedrock::{
    BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth::ResolvedCreds,
};
use serde_json::json;

// A beta the non-empty allowlist accepts, and one it rejects.
const ALLOWED_BETA: &str = "context-1m-2025-08-07";
const UNLISTED_BETA: &str = "unlisted-beta-2099-01-01";
// A forward-compat body field the non-empty allowlist accepts, and one it
// rejects. Both are long-tail (non-canonical) knobs the ingress
// forward-compat sweep lands in `provider_extras`.
const LISTED_FIELD: &str = "top_k";
const UNLISTED_FIELD: &str = "diagnostics";

/// Structural keys the assembled Invoke body carries, plus the one
/// forward-compat field the operator chose to keep. Shared with Converse:
/// Converse's `additionalModelRequestFields` bag never holds the structural
/// keys (they ride at the AWS top level), so the extra entries are inert
/// there -- an allowlist that lists a key absent from the bag is a no-op.
fn non_empty_body_fields() -> Vec<String> {
    vec![
        "anthropic_version".into(),
        "anthropic_beta".into(),
        "messages".into(),
        "max_tokens".into(),
        LISTED_FIELD.into(),
    ]
}

/// One request carrying both betas (one to keep, one to drop) and both
/// forward-compat body fields (one to keep, one to drop). Reused across all
/// two egress surfaces so the equivalence proof runs against a single
/// legacy-shaped input.
fn legacy_request() -> ChatRequest {
    ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![common::user_msg("hello")].into(),
        max_tokens: Some(64),
        anthropic_beta: vec![ALLOWED_BETA.into(), UNLISTED_BETA.into()],
        provider_extras: Some(json!({
            LISTED_FIELD: 40,
            UNLISTED_FIELD: { "trace_id": "abc" },
        })),
        ..Default::default()
    }
}

fn bedrock_provider(
    api_shape: BedrockApiShape,
    allowed_betas: Vec<String>,
    allowed_body_fields: Vec<String>,
) -> BedrockProvider {
    let cfg = BedrockConfig {
        id: "bedrock-equivalence-test".into(),
        region: "us-east-1".into(),
        model_id: "anthropic.claude-3-opus-20240229-v1:0".into(),
        api_shape,
        creds: BedrockCreds::BearerKey {
            key: "test-key".into(),
        },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        allowed_betas,
        allowed_body_fields,
        additional_model_request_fields: None,
        adaptive_thinking: None,
    };
    let resolved = ResolvedCreds::Bearer {
        key: "test-key".into(),
    };
    BedrockProvider::new(cfg, resolved).expect("canonical region")
}

// =====================================================================
// Bedrock Invoke
// =====================================================================

#[test]
fn bedrock_invoke_empty_allowlists_pass_through_every_beta_and_field() {
    // Arrange: empty allowlists == discovery-mode pass-through.
    let provider = bedrock_provider(BedrockApiShape::Invoke, Vec::new(), Vec::new());

    // Act
    let body = provider
        .normalize_request(&legacy_request())
        .expect("bedrock invoke normalize");

    // Assert: every requested beta and forward-compat field survives.
    assert_eq!(
        body["anthropic_beta"],
        json!([ALLOWED_BETA, UNLISTED_BETA]),
        "empty allowed_betas must pass every requested beta through"
    );
    assert_eq!(body[LISTED_FIELD], json!(40));
    assert_eq!(body[UNLISTED_FIELD], json!({ "trace_id": "abc" }));

    // Pin the full assembled body bytes.
    insta::with_settings!({snapshot_path => "snapshots/legacy_equivalence"}, {
        insta::assert_json_snapshot!("bedrock_invoke_pass_through", body);
    });
}

#[test]
fn bedrock_invoke_non_empty_allowlists_drop_unlisted_beta_and_field() {
    // Arrange: a beta allowlist admitting one flag, a body-field allowlist
    // admitting the structural keys plus one forward-compat field.
    let provider = bedrock_provider(
        BedrockApiShape::Invoke,
        vec![ALLOWED_BETA.into()],
        non_empty_body_fields(),
    );

    // Act
    let body = provider
        .normalize_request(&legacy_request())
        .expect("bedrock invoke normalize");

    // Assert: the unlisted beta and unlisted field drop; the listed ones
    // and the structural keys survive.
    assert_eq!(
        body["anthropic_beta"],
        json!([ALLOWED_BETA]),
        "non-empty allowed_betas must drop the unlisted beta"
    );
    assert_eq!(body[LISTED_FIELD], json!(40));
    assert!(
        body.get(UNLISTED_FIELD).is_none(),
        "non-empty allowed_body_fields must drop the unlisted field; got {body}"
    );

    // Pin the full assembled body bytes.
    insta::with_settings!({snapshot_path => "snapshots/legacy_equivalence"}, {
        insta::assert_json_snapshot!("bedrock_invoke_filtered", body);
    });
}

// =====================================================================
// Bedrock Converse
// =====================================================================

#[test]
fn bedrock_converse_empty_allowlists_pass_through_every_beta_and_field() {
    // Arrange
    let provider = bedrock_provider(BedrockApiShape::Converse, Vec::new(), Vec::new());

    // Act
    let body = provider
        .normalize_request(&legacy_request())
        .expect("bedrock converse normalize");
    let amrf = &body["additionalModelRequestFields"];

    // Assert: the additionalModelRequestFields bag carries every beta and
    // forward-compat field.
    assert_eq!(
        amrf["anthropic_beta"],
        json!([ALLOWED_BETA, UNLISTED_BETA]),
        "empty allowed_betas must pass every requested beta through on Converse"
    );
    assert_eq!(amrf[LISTED_FIELD], json!(40));
    assert_eq!(amrf[UNLISTED_FIELD], json!({ "trace_id": "abc" }));

    // Pin the full assembled body bytes.
    insta::with_settings!({snapshot_path => "snapshots/legacy_equivalence"}, {
        insta::assert_json_snapshot!("bedrock_converse_pass_through", body);
    });
}

#[test]
fn bedrock_converse_non_empty_allowlists_drop_unlisted_beta_and_field() {
    // Arrange
    let provider = bedrock_provider(
        BedrockApiShape::Converse,
        vec![ALLOWED_BETA.into()],
        non_empty_body_fields(),
    );

    // Act
    let body = provider
        .normalize_request(&legacy_request())
        .expect("bedrock converse normalize");
    let amrf = &body["additionalModelRequestFields"];

    // Assert
    assert_eq!(
        amrf["anthropic_beta"],
        json!([ALLOWED_BETA]),
        "non-empty allowed_betas must drop the unlisted beta on Converse"
    );
    assert_eq!(amrf[LISTED_FIELD], json!(40));
    assert!(
        amrf.get(UNLISTED_FIELD).is_none(),
        "non-empty allowed_body_fields must drop the unlisted field; got {amrf}"
    );

    // Pin the full assembled body bytes.
    insta::with_settings!({snapshot_path => "snapshots/legacy_equivalence"}, {
        insta::assert_json_snapshot!("bedrock_converse_filtered", body);
    });
}
