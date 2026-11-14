//! Every lane that carries its own excerpt of an upstream error body renders
//! the error (`{err}` and `{err:?}`, the text the router's WARN lines print)
//! with each credential redacted whole, even one the excerpt cut splits.
//! Driven end to end: a real provider of each lane against a mock upstream
//! answering HTTP 500 with a body that has no string `error.message`.

use std::sync::Arc;

use routectl_core::{ChatRequest, Error, InMemoryProjectCache, Provider, StaticToken, TokenSource};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::anthropic_api::{AnthropicApiConfig, AnthropicApiProvider};
use crate::bedrock::{BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth};
use crate::gemini::{GeminiConfig, GeminiProvider};
use crate::openai_compat::{OpenAiCompatConfig, OpenAiCompatProvider};
use crate::openai_responses::{AuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider};
use crate::upstream_log::tests::{
    CLEAN_SENTINEL, planted_bodies_without_message, rendering_faults,
};

const PROVIDER_ID: &str = "carried-excerpt-test";
const STATUS: u16 = 500;

#[derive(Clone, Copy, Debug)]
enum Lane {
    Compat,
    Responses,
    Gemini,
    CloudCode,
    Anthropic,
    Bedrock,
}

const LANES: [Lane; 6] = [
    Lane::Compat,
    Lane::Responses,
    Lane::Gemini,
    Lane::CloudCode,
    Lane::Anthropic,
    Lane::Bedrock,
];

fn request() -> ChatRequest {
    ChatRequest {
        model: "carried-excerpt-model".into(),
        messages: vec![routectl_core::test_utils::user_msg("ping")].into(),
        max_tokens: Some(16),
        ..Default::default()
    }
}

async fn provider_for(lane: Lane, base: &str) -> Arc<dyn Provider> {
    match lane {
        Lane::Compat => Arc::new(OpenAiCompatProvider::new(OpenAiCompatConfig::new(
            PROVIDER_ID,
            base,
            "k",
        ))),
        Lane::Responses => {
            let mut cfg = OpenAiResponsesConfig::new(PROVIDER_ID, "k");
            cfg.auth_kind = AuthKind::ApiKey;
            cfg.base_url = base.to_string();
            Arc::new(OpenAiResponsesProvider::new(cfg))
        }
        Lane::Gemini => {
            let mut cfg = GeminiConfig::new(PROVIDER_ID, "k");
            cfg.base_url = base.to_string();
            Arc::new(GeminiProvider::new(cfg))
        }
        Lane::CloudCode => {
            let token: Arc<dyn TokenSource> = Arc::new(StaticToken::new("t"));
            let mut cfg = GeminiConfig::new_cloud_code(
                PROVIDER_ID,
                token,
                Arc::new(InMemoryProjectCache::new()),
            );
            cfg.base_url = base.to_string();
            Arc::new(GeminiProvider::new(cfg))
        }
        Lane::Anthropic => {
            let mut cfg = AnthropicApiConfig::new(PROVIDER_ID, "k");
            cfg.base_url = base.to_string();
            Arc::new(AnthropicApiProvider::new(cfg))
        }
        Lane::Bedrock => {
            let creds = BedrockCreds::BearerKey {
                key: ["carried", "excerpt", "test"].join("-"),
            };
            let cfg = BedrockConfig {
                id: PROVIDER_ID.into(),
                region: "us-east-1".into(),
                model_id: "us.anthropic.claude-opus-5-5".into(),
                api_shape: BedrockApiShape::Invoke,
                creds: creds.clone(),
                user_agent: None,
                header_extras: Vec::new(),
                anthropic_beta: Vec::new(),
                additional_model_request_fields: None,
            };
            let resolved = auth::resolve(&creds, "us-east-1").await.expect("resolve");
            Arc::new(
                BedrockProvider::new(cfg, resolved)
                    .expect("canonical region")
                    .with_runtime_origin_for_tests(base),
            )
        }
    }
}

/// The error `lane` returns when every upstream call answers `status` with
/// `body`.
async fn lane_error(lane: Lane, status: u16, body: &str) -> Error {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
        .mount(&server)
        .await;
    let provider = provider_for(lane, &server.uri()).await;
    let err = provider
        .complete(request())
        .await
        .expect_err("the upstream answers an error");
    assert!(
        matches!(&err, Error::Upstream { status: s, .. } if *s == status),
        "{lane:?}: expected the upstream error, got {err:?}"
    );
    err
}

#[tokio::test]
async fn every_lane_renders_a_body_without_a_message_scrubbed_before_the_cut() {
    let mut faults = Vec::new();
    for lane in LANES {
        for (row, body, planted) in planted_bodies_without_message() {
            let err = lane_error(lane, STATUS, &body).await;

            faults.extend(rendering_faults(&format!("{lane:?}/{row}"), &err, planted));
        }
    }

    assert!(faults.is_empty(), "{}", faults.join("\n"));
}

/// `(name, body, planted)` Cloud Code onboarding bodies: one whose key the
/// 256-char cut splits, and short ones whose credential the scrub finds only
/// through a tab, which the control filter would turn into `?`.
fn onboarding_bodies() -> Vec<(&'static str, String, &'static str)> {
    const KEY: &str = "sk-ant-api03-SECRETBODY0123456789abcdefghij";
    let key_at = 256 - 23;
    let head = format!("{CLEAN_SENTINEL} ");
    let straddling = format!("{head}{} {KEY} tail", "x".repeat(key_at - head.len() - 1));
    assert_eq!(straddling.find(KEY), Some(key_at), "key offset");
    assert!(
        (257..=512).contains(&straddling.len()),
        "body length {}",
        straddling.len()
    );
    vec![
        ("key straddling char 256", straddling, "sk-ant-api03-"),
        (
            "bearer token after a tab",
            format!("{CLEAN_SENTINEL} rejected Bearer\tplantedTok3n9xyz"),
            "plantedTok3n9xyz",
        ),
        (
            "quoted credential value after a tab",
            format!("{CLEAN_SENTINEL} echoed \"api_key\":\t\"plantedvalue42\""),
            "plantedvalue42",
        ),
    ]
}

#[tokio::test]
async fn cloud_code_onboarding_error_scrubs_before_its_shorter_cut() {
    let mut faults = Vec::new();
    for (row, body, planted) in onboarding_bodies() {
        let err = lane_error(Lane::CloudCode, STATUS, &body).await;

        faults.extend(rendering_faults(
            &format!("cloud code/{row}"),
            &err,
            planted,
        ));
    }

    assert!(faults.is_empty(), "{}", faults.join("\n"));
}

/// A body over [`routectl_core::MAX_ERROR_BODY_BYTES`]: the sentinel, an ARN
/// long enough that its `[redacted]` pulls the text after it to the front of
/// the excerpt, then a JWT whose third segment the error-body bound leaves 5
/// characters of, short of the scrub's 10-character segment floor.
fn credential_split_by_the_error_body_bound() -> String {
    let segment = "abcDEF1234".repeat(3);
    let jwt = format!("eyJ{segment}.{segment}.{segment}");
    let jwt_at = routectl_core::MAX_ERROR_BODY_BYTES - ("eyJ".len() + 2 * (segment.len() + 1) + 5);
    let head = format!("{CLEAN_SENTINEL} arn:aws:iam::123456789012:role/");
    let body = format!("{head}{} {jwt} tail", "r".repeat(jwt_at - head.len() - 1));
    assert_eq!(body.find(&jwt), Some(jwt_at), "jwt offset");
    body
}

#[tokio::test]
async fn every_lane_drops_a_credential_the_error_body_bound_splits() {
    let body = credential_split_by_the_error_body_bound();
    let mut faults = Vec::new();
    for lane in LANES {
        let err = lane_error(lane, STATUS, &body).await;

        faults.extend(rendering_faults(
            &format!("{lane:?}/credential split by the error-body bound"),
            &err,
            "eyJ",
        ));
    }

    assert!(faults.is_empty(), "{}", faults.join("\n"));
}
