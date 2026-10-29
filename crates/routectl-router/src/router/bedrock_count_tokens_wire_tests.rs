//! A Bedrock token count is read against the request body its CountTokens
//! envelope carried: a 400 naming a path routectl declared adding to the
//! Invoke body is laid to that edit, even though the body crossed the wire
//! base64-wrapped inside the envelope.
//!
//! A count refused for an operator payload extra is re-sent with the extra
//! reverted, and the re-sent envelope carries the reverted body.
//!
//! Driven through a real Bedrock provider against a wiremock upstream, so
//! the deposit read is the one the egress made on the way out.

use std::collections::BTreeMap;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use routectl_core::wire_check::{IngressDialect, IngressWire};
use routectl_core::{ChatRequest, Message, MessageContent, Role};
use routectl_providers::bedrock::{
    BedrockApiShape, BedrockConfig, BedrockCreds, BedrockProvider, auth,
};
use routectl_testkit::CapturedEvent;
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use crate::config::Config;
use crate::resolved::ResolvedModel;
use crate::router::Router;

const ALIAS: &str = "count-alias";
const REGION: &str = "us-east-1";
/// A synthetic Anthropic-family wire id, so the seat is count-capable and no
/// baked catalog prior applies.
const WIRE: &str = "us.anthropic.zz-count-wire";
const EDIT_REJECTED: &str = "routectl_edit_rejected";
const ERROR_TYPE_HEADER: &str =
    "ValidationException:http://internal.amazon.com/coral/com.amazon.bedrock/";
/// A field the Invoke egress adds to every body and declares as added.
const DECLARED_FIELD: &str = "anthropic_version";
/// A non-canonical key the operator's payload extras add to every body.
const EXTRA_KEY: &str = "zz_operator_extra";
const COUNTED: u64 = 42;

const CONFIG: &str = r#"
[retry]
max_attempts = 1
initial_backoff_ms = 0
jitter_ms = 0

[providers.p1]
kind = "bedrock"
region = "us-east-1"
api_shape = "invoke"
creds = { kind = "default-chain" }

[aliases]
"count-alias" = ["m1"]
"#;

/// Assembled at runtime so no key-shaped literal sits in the source.
fn bearer_key() -> String {
    ["count", "wire", "key"].join("-")
}

/// An upstream refusing every count with a 400 naming the declared field.
async fn refusing_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(400)
                .insert_header("x-amzn-ErrorType", ERROR_TYPE_HEADER)
                .set_body_raw(
                    json!({"message": format!("{DECLARED_FIELD}: Extra inputs are not permitted")})
                        .to_string(),
                    "application/json",
                ),
        )
        .mount(&server)
        .await;
    server
}

/// [`CONFIG`] with the operator's payload extras on the provider.
fn config_with_extras() -> String {
    CONFIG.replace(
        "creds = { kind = \"default-chain\" }\n",
        &format!(
            "creds = {{ kind = \"default-chain\" }}\npayload_extras = {{ {EXTRA_KEY} = \"x\" }}\n"
        ),
    )
}

/// The InvokeModel body a CountTokens envelope carries, decoded.
fn carried_body(request: &Request) -> Value {
    let envelope: Value = serde_json::from_slice(&request.body).expect("envelope is JSON");
    let b64 = envelope["input"]["invokeModel"]["body"]
        .as_str()
        .expect("an invokeModel body");
    let raw = B64.decode(b64).expect("carried body is base64");
    serde_json::from_slice(&raw).expect("carried body is JSON")
}

/// An upstream refusing any count whose carried body holds the operator's
/// extra, and serving any count whose carried body does not.
async fn extra_refusing_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &Request| {
            if carried_body(request).get(EXTRA_KEY).is_some() {
                ResponseTemplate::new(400)
                    .insert_header("x-amzn-ErrorType", ERROR_TYPE_HEADER)
                    .set_body_raw(
                        json!({"message": format!("{EXTRA_KEY}: Extra inputs are not permitted")})
                            .to_string(),
                        "application/json",
                    )
            } else {
                ResponseTemplate::new(200).set_body_json(json!({"inputTokens": COUNTED}))
            }
        })
        .mount(&server)
        .await;
    server
}

async fn bedrock_router(origin: &str) -> Router {
    bedrock_router_from(CONFIG, origin).await
}

async fn bedrock_router_from(toml_src: &str, origin: &str) -> Router {
    let config: Config = toml::from_str(toml_src).expect("valid test toml");
    let mut router = Router::new(Arc::new(config));
    let creds = BedrockCreds::BearerKey { key: bearer_key() };
    let cfg = BedrockConfig {
        id: "bedrock:p1".into(),
        region: REGION.into(),
        model_id: WIRE.into(),
        api_shape: BedrockApiShape::Invoke,
        creds: creds.clone(),
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
    };
    let resolved = auth::resolve(&creds, REGION).await.expect("resolve");
    let provider = BedrockProvider::new(cfg, resolved)
        .expect("canonical region")
        .with_runtime_origin_for_tests(origin);
    let model = ResolvedModel::new("m1", "p1", Arc::new(provider), WIRE);
    router.install_resolved_models(BTreeMap::from([("m1".to_owned(), Arc::new(model))]));
    router
}

/// A request whose client body, captured as the Anthropic ingress does,
/// carries no `anthropic_version`.
fn client_request() -> ChatRequest {
    let client = json!({"model": ALIAS, "messages": [{"role": "user", "content": "hi"}]});
    let mut req = ChatRequest {
        model: ALIAS.into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("hi".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(16),
        ..Default::default()
    };
    req.routectl_internal.ingress_wire = Some(IngressWire {
        dialect: IngressDialect::Anthropic,
        client_version: None,
        bytes: serde_json::to_vec(&client).expect("serialize").into(),
    });
    req
}

/// `(lane, path, edit_site)` of every `routectl_edit_rejected` warning.
fn edit_rejected_warnings(events: &[CapturedEvent]) -> Vec<(String, String, String)> {
    events
        .iter()
        .filter(|event| event.level == tracing::Level::WARN && event.message == EDIT_REJECTED)
        .map(|event| {
            let field = |name| event.field(name).unwrap_or_default().to_owned();
            (field("lane"), field("path"), field("edit_site"))
        })
        .collect()
}

#[tokio::test]
async fn a_bedrock_count_rejection_of_a_declared_field_is_laid_to_its_edit() {
    // Arrange
    let server = refusing_upstream().await;
    let router = bedrock_router(&server.uri()).await;

    // Act
    let (counted, events) = Box::pin(routectl_testkit::with_capture(
        router.count_tokens_with_meta(client_request()),
    ))
    .await;

    // Assert
    let requests = server.received_requests().await.expect("recorded requests");
    assert!(
        requests
            .iter()
            .all(|request| request.url.path().ends_with("/count-tokens")),
        "every call went to the CountTokens endpoint"
    );
    assert!(!requests.is_empty(), "the count reached the upstream");
    assert!(counted.result.is_err(), "the refused count is not served");
    assert_eq!(
        edit_rejected_warnings(&events),
        [(
            "anthropic->bedrock-invoke".to_owned(),
            format!("/{DECLARED_FIELD}"),
            "required_field_default".to_owned(),
        )],
        "one warning lays the rejected path to the declared edit"
    );
}

#[tokio::test]
async fn a_bedrock_count_refused_for_an_operator_extra_re_sends_the_reverted_body() {
    // Arrange
    let server = extra_refusing_upstream().await;
    let router = bedrock_router_from(&config_with_extras(), &server.uri()).await;

    // Act
    let counted = router.count_tokens(client_request()).await;

    // Assert
    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 2, "the refused count is re-sent once");
    assert!(
        requests
            .iter()
            .all(|request| request.url.path().ends_with("/count-tokens")),
        "every call went to the CountTokens endpoint"
    );
    assert_eq!(
        carried_body(&requests[0]).get(EXTRA_KEY),
        Some(&json!("x")),
        "the first count carries the operator's extra"
    );
    assert!(
        carried_body(&requests[1]).get(EXTRA_KEY).is_none(),
        "the re-sent count carries the reverted body"
    );
    let count = counted.expect("the re-sent count is served");
    assert_eq!(u64::from(count.input_tokens), COUNTED);
}
