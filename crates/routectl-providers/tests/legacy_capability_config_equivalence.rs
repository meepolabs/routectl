//! Feature acceptance -- legacy-config egress-byte equivalence.
//!
//! The routing half of this acceptance bar (the `FilterSource` labels a
//! legacy `unsupported_features` list produces) lives in the router crate.
//! This file proves the OTHER half: the two legacy egress allowlists
//! (`[bedrock] allowed_betas` and `[bedrock] allowed_body_fields`, plus the
//! anthropic-egress `allowed_betas`) emit byte-identical wire output to the
//! pre-f3 baseline. The egress filters were untouched by f3; these tests pin
//! their absolute output so any accidental f3-adjacent regression is caught.
//!
//! Each surface is exercised in BOTH modes the allowlists support:
//!
//! - EMPTY allowlist == pass-through: every requested beta and every
//!   forward-compat body field survives on the wire.
//! - NON-EMPTY allowlist: only the listed betas / fields survive; the rest
//!   drop before dispatch.
//!
//! The Bedrock surfaces pin the FULL assembled body via `insta` snapshots
//! (absolute expected bytes) plus targeted drop/keep assertions. The
//! anthropic surface carries its betas on the `anthropic-beta` HTTP header
//! (the body-side field is stripped before send on the api.anthropic.com
//! egress), so it is proven against the captured outbound header -- the
//! actual wire surface -- via a wiremock round-trip.

#![cfg(all(feature = "bedrock", feature = "anthropic-api"))]

mod common;

use routectl_core::{ChatRequest, Provider};
use routectl_providers::anthropic_api::{
    AnthropicApiConfig, AnthropicApiProvider, AuthKind, CloakConfig,
};
use routectl_providers::bedrock::{
    BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth::ResolvedCreds,
};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
/// three egress surfaces so the equivalence proof runs against a single
/// legacy-shaped input.
fn legacy_request() -> ChatRequest {
    ChatRequest {
        model: "anthropic.claude-haiku-4-5".into(),
        messages: vec![common::user_msg("hello")],
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
    BedrockProvider::new(cfg, resolved)
}

/// Anthropic provider pointed at a wiremock URI so the outbound
/// `anthropic-beta` header can be captured. `ApiKey` auth on a non-
/// api.anthropic.com host adds no minted beta floor, so the header carries
/// exactly the client betas the allowlist admits.
fn anthropic_provider(base_url: String, allowed_betas: Vec<String>) -> AnthropicApiProvider {
    AnthropicApiProvider::new(AnthropicApiConfig {
        id: "anthropic-equivalence-test".into(),
        auth: std::sync::Arc::new(routectl_core::StaticToken::new("test-key")),
        base_url,
        anthropic_version: "2023-06-01".into(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        allowed_betas,
        forward_client_headers: Vec::new(),
        context_management: false,
        max_thinking_entry_bytes: AnthropicApiConfig::MAX_THINKING_ENTRY_BYTES,
        session_id: None,
        cloak: CloakConfig::default(),
        use_forwarded_bearer: false,
    })
}

/// Minimal Anthropic-shape success body so `complete()` reaches the
/// happy path and wiremock captures the outbound request.
fn ok_response_body() -> serde_json::Value {
    json!({
        "id": "msg_equivalence",
        "type": "message",
        "role": "assistant",
        "model": "claude-3-opus",
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1, "output_tokens": 1},
        "content": [{"type": "text", "text": "ok"}]
    })
}

/// Drive one `complete()` through a wiremock upstream and return the
/// captured `anthropic-beta` header value (or `None` when absent).
async fn captured_beta_header(allowed_betas: Vec<String>) -> Option<String> {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_response_body()))
        .mount(&mock_server)
        .await;

    let provider = anthropic_provider(mock_server.uri(), allowed_betas);
    provider
        .complete(legacy_request())
        .await
        .expect("anthropic complete must succeed");

    let received = mock_server
        .received_requests()
        .await
        .expect("wiremock captured requests");
    assert_eq!(received.len(), 1, "expected exactly one outbound request");
    received[0]
        .headers
        .get("anthropic-beta")
        .map(|v| v.to_str().expect("header is utf-8").to_string())
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

// =====================================================================
// Anthropic egress
// =====================================================================
//
// The anthropic egress gates only `allowed_betas` (it performs no body-field
// allowlisting -- that surface is Bedrock-only) and carries betas on the
// `anthropic-beta` HTTP header, so the header captured off the wire is the
// authoritative surface.

#[tokio::test]
async fn anthropic_egress_empty_allowlist_passes_through_every_beta() {
    // Act: empty allowlist == pass-through.
    let header = captured_beta_header(Vec::new())
        .await
        .expect("anthropic-beta header must be present");

    // Assert: both requested betas reach the wire header. The merge dedupes
    // via a set and joins with `,`, so wire order is implementation-defined;
    // assert flag presence, not order.
    assert!(
        header.split(',').any(|f| f.trim() == ALLOWED_BETA),
        "listed beta must reach the header; got `{header}`"
    );
    assert!(
        header.split(',').any(|f| f.trim() == UNLISTED_BETA),
        "empty allowed_betas must pass every beta through; got `{header}`"
    );
}

#[tokio::test]
async fn anthropic_egress_non_empty_allowlist_drops_unlisted_beta() {
    // Act: an allowlist admitting only one of the two requested betas.
    let header = captured_beta_header(vec![ALLOWED_BETA.into()])
        .await
        .expect("anthropic-beta header must be present");

    // Assert: the listed beta reaches the header, the unlisted one is gone.
    assert!(
        header.split(',').any(|f| f.trim() == ALLOWED_BETA),
        "listed beta must reach the header; got `{header}`"
    );
    assert!(
        !header.split(',').any(|f| f.trim() == UNLISTED_BETA),
        "non-empty allowed_betas must drop the unlisted beta; got `{header}`"
    );
}
