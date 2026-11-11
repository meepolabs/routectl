//! Wire tests for the Mantle forwarded-media URL check: refused rows never
//! reach the mock upstream, accepted rows do, and the first-party lane and
//! function-call arguments are untouched.

use std::sync::Arc;

use futures::StreamExt;
use routectl_core::wire_check::{DeclaredEdit, EditKind, EditSite, Leg, WireTap};
use routectl_core::{
    ChatRequest, ContentPart, CustomTool, Error, KnownContentPart, Message, MessageContent,
    Provider, RefusalReason, RequestProvenance, ResponsesPassthroughItem, Role, StaticToken,
    ToolDef,
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

fn planted_gs() -> String {
    format!("gs://{PLANTED_HOST}/{PLANTED_PATH}.png")
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

fn url_image_block(url: &str) -> Value {
    json!({"type": "image", "source": {"type": "url", "url": url}})
}

/// A passthrough message whose content part carries a url-type image source
/// under a member named `parameters`.
fn part_parameters_item(url: &str) -> Value {
    json!({
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": "t", "parameters": url_image_block(url)}]
    })
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
            "passthrough input_image object url s3",
            passthrough_request(json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_image", "image_url": {"url": s3}}]
            })),
        ),
        (
            "passthrough computer_call_output object output s3",
            passthrough_request(json!({
                "type": "computer_call_output",
                "call_id": "call_c",
                "output": {"type": "computer_screenshot", "image_url": s3}
            })),
        ),
        (
            "passthrough image_url object two levels deep",
            passthrough_request(json!({
                "type": "custom_tool_call_output",
                "call_id": "call_d",
                "output": {"parts": [{"image": {"image_url": {"url": s3}}}]}
            })),
        ),
        (
            "passthrough file bare url s3",
            passthrough_request(json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "file", "url": s3}]
            })),
        ),
        (
            "passthrough source url gs",
            passthrough_request(json!({
                "type": "agent_message",
                "content": [{
                    "type": "image",
                    "source": {"type": "url", "url": planted_gs()}
                }]
            })),
        ),
        (
            "passthrough url-shaped key above image_url",
            passthrough_request(json!({
                "type": "agent_message",
                "meta": {s3.clone(): {"image_url": s3}}
            })),
        ),
        (
            "passthrough part parameters carrying a url source",
            passthrough_request(part_parameters_item(&s3)),
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
        (
            "passthrough part parameters carrying an https source",
            passthrough_request(part_parameters_item("https://example.com/d.png")),
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

#[tokio::test]
async fn mantle_does_not_inspect_tool_schemas() {
    let server = mock_upstream().await;
    let provider = mantle_provider(&server.uri()).await;
    let schema = json!({
        "type": "object",
        "properties": {"image_url": {"type": "string", "default": planted_s3()}}
    });
    let mut req = request(vec![message(Role::User, MessageContent::Text("hi".into()))]);
    req.tools = Some(vec![ToolDef::Custom(CustomTool {
        name: "fetch".into(),
        description: None,
        input_schema: schema,
        cache_control: None,
        defer_loading: None,
        strict: None,
        type_tag: None,
    })]);

    provider
        .complete(req)
        .await
        .expect("a tool schema default naming s3 is not a media URL");

    let body = wire_body(&server).await;
    assert_eq!(
        body.pointer("/tools/0/parameters/properties/image_url/default"),
        Some(&Value::String(planted_s3())),
        "{body}"
    );
}

#[tokio::test]
async fn mantle_does_not_inspect_tool_definitions_in_tool_declaring_input_items() {
    let tool = json!({
        "type": "function",
        "name": "fetch",
        "parameters": {"type": "object", "properties": {"image": url_image_block(&planted_s3())}}
    });
    for kind in ["additional_tools", "tool_search_output"] {
        let server = mock_upstream().await;
        let provider = mantle_provider(&server.uri()).await;
        let item = json!({"type": kind, "tools": [tool.clone()]});

        provider
            .complete(passthrough_request(item.clone()))
            .await
            .unwrap_or_else(|e| panic!("{kind}: a tool definition is not media, got {e:?}"));

        let body = wire_body(&server).await;
        let input = body["input"].as_array().expect("input array");
        assert!(
            input.contains(&item),
            "{kind}: item forwarded as written: {body}"
        );
    }
}

fn planted_http() -> String {
    format!("http://{PLANTED_HOST}/{PLANTED_PATH}.png")
}

/// A turn from the Responses dialect carrying `parts`.
fn responses_request(role: Role, parts: Vec<ContentPart>) -> ChatRequest {
    let mut req = request(vec![message(role, MessageContent::Parts(parts))]);
    req.routectl_internal.provenance = RequestProvenance::ResponsesIngress;
    req
}

/// The unmodeled part the ingress builds from `block`.
fn other_part(block: &Value) -> ContentPart {
    let mut extras = block.as_object().cloned().expect("block is an object");
    let type_tag = match extras.remove("type") {
        Some(Value::String(tag)) => tag,
        other => panic!("block needs a string type, got {other:?}"),
    };
    ContentPart::Other {
        type_tag,
        cache_control: None,
        extras,
    }
}

/// A `role` turn from the Responses dialect: a text part, then one
/// unmodeled part built from `block`.
fn forwarded_part_request(role: Role, block: &Value) -> ChatRequest {
    responses_request(role, vec![text("look"), other_part(block)])
}

fn content_part_drops() -> u64 {
    crate::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == "openai-responses" && e.drop_class == "content_part_unrepresentable")
        .map_or(0, |e| e.drop_count)
}

/// How a forwarded-part row must end on the Mantle lane.
enum Outcome {
    /// Refused locally, before any upstream call, with an unrepresentable
    /// error naming `message`.
    Refused { message: &'static str },
    /// Sent with the part gone, counted once and its Remove declared.
    Dropped,
    /// Sent, with the part on the wire as `expected`.
    Sent { expected: Value },
}

fn sent(expected: Value) -> Outcome {
    Outcome::Sent { expected }
}

const URL_ONLY_FILE: Outcome = Outcome::Refused {
    message: "only file_url",
};

fn unmodeled_part_rows() -> Vec<(&'static str, Role, Value, Outcome)> {
    let http = planted_http();
    let s3 = planted_s3();
    vec![
        (
            "http image_url nested two levels deep",
            Role::User,
            json!({"type": "input_bundle_x", "payload": {"inner": {"image_url": {"url": http}}}}),
            Outcome::Dropped,
        ),
        (
            "http file_url",
            Role::User,
            json!({"type": "input_document_x", "file_url": http}),
            Outcome::Dropped,
        ),
        (
            "upper-case FILE_URL",
            Role::User,
            json!({"type": "input_document_x", "FILE_URL": http}),
            Outcome::Dropped,
        ),
        (
            "non-string url member",
            Role::User,
            json!({"type": "input_video_x", "video_url": 7}),
            Outcome::Dropped,
        ),
        (
            "s3 source with a uri member",
            Role::User,
            json!({"type": "input_document_x", "source": {"type": "s3", "uri": s3}}),
            Outcome::Dropped,
        ),
        (
            "url under a parameters member",
            Role::User,
            json!({"type": "input_bundle_x", "parameters": {"image_url": http}}),
            Outcome::Dropped,
        ),
        (
            "tool_use shaped part with a file_url input",
            Role::User,
            json!({"type": "tool_use", "input": {"file_url": http}}),
            Outcome::Dropped,
        ),
        (
            "upper-case INPUT_FILE type with an http file_url",
            Role::User,
            json!({"type": "INPUT_FILE", "file_url": http}),
            Outcome::Dropped,
        ),
        (
            "https and data urls nested",
            Role::User,
            json!({
                "type": "input_bundle_x",
                "payload": {
                    "inner": {"image_url": {"url": "https://example.com/a.png"}},
                    "thumb_url": "data:image/png;base64,AAAA"
                }
            }),
            Outcome::Dropped,
        ),
        (
            "assistant turn s3 source",
            Role::Assistant,
            json!({"type": "input_document_x", "source": {"type": "s3", "uri": s3}}),
            Outcome::Dropped,
        ),
    ]
}

fn input_file_rows() -> Vec<(&'static str, Role, Value, Outcome)> {
    let http = planted_http();
    let file_data = "data:application/pdf;base64,JVBER";
    vec![
        (
            "input_file carrying only an https file_url",
            Role::User,
            json!({"type": "input_file", "file_url": "https://example.com/a.pdf"}),
            URL_ONLY_FILE,
        ),
        (
            "assistant input_file carrying only an https file_url",
            Role::Assistant,
            json!({"type": "input_file", "file_url": "https://example.com/a.pdf"}),
            URL_ONLY_FILE,
        ),
        (
            "input_file carrying only a non-string file_url",
            Role::User,
            json!({"type": "input_file", "file_url": {"url": http}}),
            Outcome::Refused {
                message: "no string carrier",
            },
        ),
        (
            "input_file with file_data and file_url",
            Role::User,
            json!({
                "type": "input_file",
                "file_data": file_data,
                "file_url": "https://example.com/a.pdf",
                "filename": "a.pdf"
            }),
            sent(json!({"type": "input_file", "file_data": file_data, "filename": "a.pdf"})),
        ),
        (
            "input_file with file_data and an upper-case FILE_URL",
            Role::User,
            json!({"type": "input_file", "file_data": file_data, "FILE_URL": http}),
            sent(json!({"type": "input_file", "file_data": file_data})),
        ),
        (
            "input_file with file_data and a nested url source",
            Role::User,
            json!({
                "type": "input_file",
                "file_data": file_data,
                "source": {"type": "url", "url": http}
            }),
            sent(json!({"type": "input_file", "file_data": file_data})),
        ),
        (
            "input_file with file_data and an empty file_url",
            Role::User,
            json!({"type": "input_file", "file_data": file_data, "file_url": ""}),
            sent(json!({"type": "input_file", "file_data": file_data})),
        ),
        (
            "input_file with file_data and a non-string file_url",
            Role::User,
            json!({"type": "input_file", "file_data": file_data, "file_url": {"url": http}}),
            sent(json!({"type": "input_file", "file_data": file_data})),
        ),
        (
            "input_file with file_id and an http file_url",
            Role::User,
            json!({"type": "input_file", "file_id": "file-abc", "file_url": http}),
            sent(json!({"type": "input_file", "file_id": "file-abc"})),
        ),
    ]
}

fn assert_local_refusal(result: Result<(), Error>, row: &str, expected: &str) {
    match result {
        Err(Error::NormalizeRequest(_, msg, RefusalReason::Unrepresentable)) => {
            assert!(msg.contains(expected), "{row}: {msg}");
            assert!(
                !msg.contains("example.com") && !msg.contains(PLANTED_HOST),
                "{row}: echoes the url: {msg}"
            );
        }
        other => panic!("{row}: expected an unrepresentable refusal, got {other:?}"),
    }
}

fn declares_file_url_withhold(edits: &[DeclaredEdit]) -> bool {
    edits.iter().any(|e| {
        e.site() == EditSite::AuthSurfaceGate
            && e.path() == "/input/*/content/*/file_url"
            && e.kind() == EditKind::Remove
    })
}

fn declares_part_remove(edits: &[DeclaredEdit]) -> bool {
    edits.iter().any(|e| {
        e.site() == EditSite::DialectFieldStrip
            && e.path() == "/messages/*/content/*"
            && e.kind() == EditKind::Remove
    })
}

async fn send(
    provider: &OpenAiResponsesProvider,
    stream: bool,
    req: ChatRequest,
) -> Result<(), Error> {
    if stream {
        let mut chunks = provider.stream(req).await?;
        while chunks.next().await.is_some() {}
        Ok(())
    } else {
        provider.complete(req).await.map(|_| ())
    }
}

/// What one Mantle send of a forwarded-part request produced.
struct MantleRun {
    server: MockServer,
    result: Result<(), Error>,
    drop_delta: u64,
    edits: Vec<DeclaredEdit>,
}

async fn send_mantle(stream: bool, mut req: ChatRequest) -> MantleRun {
    let tap = WireTap::default();
    req.routectl_internal.wire_tap = Some(tap.clone());
    let server = mock_upstream().await;
    let provider = mantle_provider(&server.uri()).await;
    let before = content_part_drops();
    let result = send(&provider, stream, req).await;
    let drop_delta = content_part_drops() - before;
    let edits = tap.take_request().edits(Leg::Request).to_vec();
    MantleRun {
        server,
        result,
        drop_delta,
        edits,
    }
}

/// No planted url, upper-case url member or s3 uri survives into `body`.
fn assert_no_planted_url(body: &Value, row: &str) {
    let raw = body.to_string();
    for needle in [PLANTED_HOST, "FILE_URL", "s3://"] {
        assert!(
            !raw.contains(needle),
            "{row}: {needle} reached the wire: {raw}"
        );
    }
}

async fn assert_mantle_outcome(row: &str, block: &Value, outcome: &Outcome, run: MantleRun) {
    match outcome {
        Outcome::Refused { message } => {
            assert_local_refusal(run.result, row, message);
            assert_eq!(hit_count(&run.server).await, 0, "{row}: no upstream call");
        }
        Outcome::Dropped => {
            run.result
                .unwrap_or_else(|e| panic!("{row}: must reach the upstream, got {e:?}"));
            let body = wire_body(&run.server).await;
            let content = body.pointer("/input/0/content").expect("content");
            assert_eq!(content.as_array().map(Vec::len), Some(1), "{row}: {body}");
            assert_no_planted_url(&body, row);
            assert_eq!(run.drop_delta, 1, "{row}: one drop per request");
            assert!(declares_part_remove(&run.edits), "{row}: {:?}", run.edits);
        }
        Outcome::Sent { expected } => {
            run.result
                .unwrap_or_else(|e| panic!("{row}: must reach the upstream, got {e:?}"));
            let body = wire_body(&run.server).await;
            assert_eq!(body.pointer("/input/0/content/1"), Some(expected), "{row}");
            assert_no_planted_url(&body, row);
            assert_eq!(run.drop_delta, 0, "{row}: a rebuilt file is not a drop");
            assert!(!declares_part_remove(&run.edits), "{row}: {:?}", run.edits);
            let withheld = block
                .get("file_url")
                .and_then(Value::as_str)
                .is_some_and(|u| !u.is_empty());
            assert_eq!(
                declares_file_url_withhold(&run.edits),
                withheld,
                "{row}: file_url withhold declaration"
            );
        }
    }
}

#[tokio::test]
async fn mantle_refuses_unforwardable_url_inside_forwarded_part() {
    for (row, role, block, outcome) in unmodeled_part_rows() {
        for stream in [false, true] {
            let row = format!("{row} (stream={stream})");
            let run = send_mantle(stream, forwarded_part_request(role.clone(), &block)).await;
            assert_mantle_outcome(&row, &block, &outcome, run).await;
        }
    }
}

#[tokio::test]
async fn mantle_rebuilds_forwarded_input_file_from_its_carrier_fields() {
    for (row, role, block, outcome) in input_file_rows() {
        for stream in [false, true] {
            let row = format!("{row} (stream={stream})");
            let run = send_mantle(stream, forwarded_part_request(role.clone(), &block)).await;
            assert_mantle_outcome(&row, &block, &outcome, run).await;
        }
    }
}

#[tokio::test]
async fn mantle_ships_modeled_media_beside_a_dropped_unmodeled_part() {
    let file_data = "data:application/pdf;base64,JVBER";
    for stream in [false, true] {
        let req = responses_request(
            Role::User,
            vec![
                text("look"),
                image_url("https://example.com/a.png"),
                other_part(&json!({"type": "input_file", "file_data": file_data})),
                other_part(&json!({
                    "type": "input_document_x",
                    "source": {"type": "s3", "uri": planted_s3()}
                })),
            ],
        );

        let run = send_mantle(stream, req).await;

        run.result
            .unwrap_or_else(|e| panic!("stream={stream}: must reach the upstream, got {e:?}"));
        let body = wire_body(&run.server).await;
        assert_eq!(
            body.pointer("/input/0/content"),
            Some(&json!([
                {"type": "input_text", "text": "look"},
                {"type": "input_image", "image_url": "https://example.com/a.png"},
                {"type": "input_file", "file_data": file_data}
            ])),
            "stream={stream}"
        );
        assert_eq!(run.drop_delta, 1, "stream={stream}");
    }
}

#[tokio::test]
async fn first_party_lane_forwards_an_unmodeled_part_verbatim() {
    let block = json!({"type": "input_document_x", "source": {"type": "s3", "uri": planted_s3()}});
    for stream in [false, true] {
        let server = mock_upstream().await;
        let provider = first_party_provider(&server.uri());
        let before = content_part_drops();

        send(
            &provider,
            stream,
            forwarded_part_request(Role::User, &block),
        )
        .await
        .unwrap_or_else(|e| panic!("stream={stream}: must reach the upstream, got {e:?}"));

        let body = wire_body(&server).await;
        assert_eq!(
            body.pointer("/input/0/content/1"),
            Some(&block),
            "stream={stream}"
        );
        assert_eq!(content_part_drops() - before, 0, "stream={stream}");
    }
}
