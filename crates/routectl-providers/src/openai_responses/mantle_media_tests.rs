//! Wire tests for the Mantle forwarded-media URL check: refused rows never
//! reach the mock upstream, accepted rows do, and the first-party lane and
//! function-call arguments are untouched.

use std::sync::Arc;

use futures::StreamExt;
use routectl_core::{
    ChatRequest, ContentPart, Error, KnownContentPart, Message, MessageContent, Provider,
    RefusalReason, ResponsesPassthroughItem, Role, StaticToken,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::super::{AuthKind, OpenAiResponsesConfig, OpenAiResponsesProvider};
use crate::bedrock::BedrockCreds;
use crate::bedrock::auth::resolve;
use crate::mantle::MantleAuth;

const MODEL: &str = "openai.gpt-oss-120b";
const PLANTED_HOST: &str = "planted-bucket-zq7";
const PLANTED_PATH: &str = "secret-key-path-k3";

fn planted_s3() -> String {
    format!("s3://{PLANTED_HOST}/{PLANTED_PATH}.png")
}

fn config(
    base_url: &str,
    auth_kind: AuthKind,
    mantle: Option<MantleAuth>,
) -> OpenAiResponsesConfig {
    OpenAiResponsesConfig {
        id: "openai-responses:media".into(),
        auth: Arc::new(StaticToken::new("")),
        account_id: None,
        base_url: base_url.to_string(),
        auth_kind,
        header_extras: Vec::new(),
        user_agent: None,
        session_id: None,
        installation_id: None,
        mantle,
    }
}

async fn mantle_provider(base_url: &str) -> OpenAiResponsesProvider {
    let creds = BedrockCreds::BearerKey {
        key: "mantle-media-bearer".into(),
    };
    let resolved = resolve(&creds, "us-west-2").await.unwrap();
    let mantle = MantleAuth {
        region: "us-west-2".into(),
        creds: resolved,
    };
    OpenAiResponsesProvider::new(config(base_url, AuthKind::BedrockMantle, Some(mantle)))
}

fn first_party_provider(base_url: &str) -> OpenAiResponsesProvider {
    let mut cfg = config(base_url, AuthKind::ApiKey, None);
    cfg.auth = Arc::new(StaticToken::new("first-party-key"));
    OpenAiResponsesProvider::new(cfg)
}

fn completed_sse() -> String {
    let completed = json!({
        "id": "resp_media",
        "object": "response",
        "status": "completed",
        "model": MODEL,
        "output": [{
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{"type": "output_text", "text": "ok"}]
        }],
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
    });
    format!(
        "data: {{\"type\":\"response.completed\",\"response\":{}}}\n\n",
        serde_json::to_string(&completed).unwrap()
    )
}

async fn mock_upstream() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(completed_sse()),
        )
        .mount(&server)
        .await;
    server
}

fn message(role: Role, content: MessageContent) -> Message {
    Message {
        refusal: None,
        role,
        content,
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn text(t: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::Text {
        text: t.into(),
        citations: None,
        cache_control: None,
    })
}

fn image_url(url: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::ImageUrl {
        image_url: json!({"url": url}),
        cache_control: None,
    })
}

fn request(messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        model: MODEL.into(),
        messages: messages.into(),
        max_tokens: Some(64),
        ..Default::default()
    }
}

fn user_image_request(url: &str) -> ChatRequest {
    request(vec![message(
        Role::User,
        MessageContent::Parts(vec![text("look"), image_url(url)]),
    )])
}

fn tool_result_image_request(url: &str) -> ChatRequest {
    let call = json!({
        "id": "call_img",
        "type": "function",
        "function": {"name": "screenshot", "arguments": "{}"}
    });
    let mut assistant = message(Role::Assistant, MessageContent::Text(String::new()));
    assistant.tool_calls = Some(vec![call]);
    let mut tool = message(
        Role::Tool,
        MessageContent::Parts(vec![text("shot"), image_url(url)]),
    );
    tool.tool_call_id = Some("call_img".into());
    request(vec![
        message(Role::User, MessageContent::Text("take one".into())),
        assistant,
        tool,
    ])
}

fn passthrough_request(item: Value) -> ChatRequest {
    let mut req = request(vec![message(Role::User, MessageContent::Text("hi".into()))]);
    req.routectl_internal.responses_input_passthrough = vec![ResponsesPassthroughItem {
        modeled_prefix: 1,
        item,
    }];
    req
}

fn function_argument_request() -> ChatRequest {
    let arguments = json!({"source": planted_s3()}).to_string();
    let call = json!({
        "id": "call_arg",
        "type": "function",
        "function": {"name": "fetch", "arguments": arguments}
    });
    let mut assistant = message(Role::Assistant, MessageContent::Text(String::new()));
    assistant.tool_calls = Some(vec![call]);
    let mut tool = message(Role::Tool, MessageContent::Text("done".into()));
    tool.tool_call_id = Some("call_arg".into());
    request(vec![
        message(Role::User, MessageContent::Text("fetch it".into())),
        assistant,
        tool,
    ])
}

async fn hit_count(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

async fn wire_body(server: &MockServer) -> Value {
    let received = server.received_requests().await.unwrap();
    assert_eq!(received.len(), 1, "exactly one upstream call expected");
    serde_json::from_slice(&received[0].body).unwrap()
}

fn assert_media_refusal(result: Result<(), Error>, row: &str) {
    match result {
        Err(Error::NormalizeRequest(_, msg, RefusalReason::Unrepresentable)) => {
            assert!(
                msg.contains("https or data"),
                "{row}: refusal names the scheme class: {msg}"
            );
            assert!(
                !msg.contains(PLANTED_HOST) && !msg.contains(PLANTED_PATH),
                "{row}: refusal must not echo the URL: {msg}"
            );
        }
        other => panic!("{row}: expected a local unrepresentable refusal, got {other:?}"),
    }
}

#[tokio::test]
async fn mantle_refuses_non_forwardable_media_urls_without_an_upstream_call() {
    let s3 = planted_s3();
    let rows: Vec<(&str, ChatRequest)> = vec![
        ("user input_image s3", user_image_request(&s3)),
        ("tool-result input_image s3", tool_result_image_request(&s3)),
        ("user input_image empty", user_image_request("")),
        (
            "passthrough input_image s3",
            passthrough_request(json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_image", "image_url": s3}]
            })),
        ),
        (
            "passthrough input_file file_url s3",
            passthrough_request(json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_file", "file_url": s3}]
            })),
        ),
        (
            "passthrough function_call_output image s3",
            passthrough_request(json!({
                "type": "function_call_output",
                "call_id": "call_p",
                "output": [{"type": "input_image", "image_url": s3}]
            })),
        ),
        (
            "passthrough input_image non-string",
            passthrough_request(json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_image", "image_url": {"url": s3}}]
            })),
        ),
    ];

    for (row, req) in rows {
        let server = mock_upstream().await;
        let provider = mantle_provider(&server.uri()).await;

        let result = provider.complete(req).await.map(|_| ());

        assert_media_refusal(result, row);
        assert_eq!(hit_count(&server).await, 0, "{row}: no upstream call");
    }
}

#[tokio::test]
async fn mantle_stream_path_refuses_before_the_upstream_call() {
    let server = mock_upstream().await;
    let provider = mantle_provider(&server.uri()).await;

    let result = provider
        .stream(user_image_request(&planted_s3()))
        .await
        .map(|_| ());

    assert_media_refusal(result, "stream user input_image s3");
    assert_eq!(hit_count(&server).await, 0);
}

#[tokio::test]
async fn mantle_forwards_https_and_data_media_urls() {
    let rows = [
        (
            "user https",
            user_image_request("https://example.com/a.png"),
        ),
        (
            "user data",
            user_image_request("data:image/png;base64,AAAA"),
        ),
        (
            "tool-result https",
            tool_result_image_request("https://example.com/b.png?sig=1"),
        ),
        (
            "passthrough file https",
            passthrough_request(json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_file", "file_url": "https://example.com/c.pdf"}]
            })),
        ),
    ];

    for (row, req) in rows {
        let server = mock_upstream().await;
        let provider = mantle_provider(&server.uri()).await;

        provider
            .complete(req)
            .await
            .unwrap_or_else(|e| panic!("{row}: must reach the upstream, got {e:?}"));

        assert_eq!(hit_count(&server).await, 1, "{row}");
    }
}

#[tokio::test]
async fn mantle_stream_path_forwards_an_https_media_url() {
    let server = mock_upstream().await;
    let provider = mantle_provider(&server.uri()).await;

    let mut stream = provider
        .stream(user_image_request("https://example.com/a.png"))
        .await
        .expect("an https image must reach the upstream");
    while stream.next().await.is_some() {}

    assert_eq!(hit_count(&server).await, 1);
}

#[tokio::test]
async fn first_party_lane_forwards_media_urls_unchanged() {
    let s3 = planted_s3();
    let rows = [
        (
            "user s3",
            user_image_request(&s3),
            "/input/0/content/1/image_url",
            s3.clone(),
        ),
        (
            "user empty",
            user_image_request(""),
            "/input/0/content/1/image_url",
            String::new(),
        ),
        (
            "tool-result s3",
            tool_result_image_request(&s3),
            "/input/2/output/1/image_url",
            s3.clone(),
        ),
    ];

    for (row, req, pointer, expected) in rows {
        let server = mock_upstream().await;
        let provider = first_party_provider(&server.uri());

        provider
            .complete(req)
            .await
            .unwrap_or_else(|e| panic!("{row}: first-party lane must forward, got {e:?}"));

        let body = wire_body(&server).await;
        assert_eq!(
            body.pointer(pointer),
            Some(&Value::String(expected)),
            "{row}: {body}"
        );
    }
}

#[tokio::test]
async fn mantle_does_not_inspect_function_call_arguments() {
    let server = mock_upstream().await;
    let provider = mantle_provider(&server.uri()).await;

    provider
        .complete(function_argument_request())
        .await
        .expect("a function_call argument naming s3 is not a media URL");

    let body = wire_body(&server).await;
    let arguments = body
        .pointer("/input/1/arguments")
        .and_then(Value::as_str)
        .expect("function_call arguments on the wire");
    assert!(arguments.contains(&planted_s3()), "{body}");
}
