//! Empty optional arguments on the complete path: the codex lane removes them
//! from a returned function call against the request's own tool schemas,
//! counts the removal once per client request, declares it on a response leg
//! that is compared, and logs tool names and counts only.
//!
//! The unit-test registry is per thread and `complete()` runs on the test's
//! own current-thread runtime, so each delta is exactly what that test caused.

use std::sync::Arc;

use routectl_core::wire_check::{
    DeclaredEdit, EditKind, EditSite, IngressDialect, Leg, WirePath, WireTap,
};
use routectl_core::{
    ChatRequest, ChatResponse, ContentPart, CustomTool, KnownContentPart, Message, MessageContent,
    Provider, RequestObservation, Role, StaticToken, TokenSource, ToolDef,
};
use serde_json::{Value, json};
use tracing_test::traced_test;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::response::EMPTY_OPTIONAL_ARGUMENTS_PATH;
use super::{AuthKind, LANE, OpenAiResponsesConfig, OpenAiResponsesProvider};
use crate::translation_drop_metrics::translation_drop_snapshot;

const CLASS: &str = "codex_empty_optional_dropped";
const TOOL: &str = "read_file";
const PATH_SENTINEL: &str = "/w/sentinel-path-4c1e";

/// The arguments a model sent for an optional page range it had no value for.
const EMPTY_PAGES: &str = r#"{"allow_large":false,"file_path":"/w/sentinel-path-4c1e","limit":2000,"offset":0,"pages":""}"#;

fn provider(base_url: &str, auth_kind: AuthKind) -> OpenAiResponsesProvider {
    OpenAiResponsesProvider::new(OpenAiResponsesConfig {
        id: "responses-empty-optional".into(),
        auth: Arc::new(StaticToken::new("test-jwt")) as Arc<dyn TokenSource>,
        account_id: Some("acct".into()),
        base_url: base_url.to_string(),
        auth_kind,
        header_extras: Vec::new(),
        user_agent: None,
        session_id: None,
        installation_id: None,
        #[cfg(feature = "bedrock")]
        mantle: None,
    })
}

fn reader_tool() -> ToolDef {
    ToolDef::Custom(CustomTool {
        name: TOOL.into(),
        description: None,
        input_schema: json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
                "pages": {"type": "string"},
                "offset": {"type": "integer"},
                "limit": {"type": "integer"},
                "allow_large": {"type": "boolean"}
            },
            "required": ["file_path"]
        }),
        cache_control: None,
        defer_loading: None,
        strict: None,
        type_tag: None,
    })
}

fn request(observation: &RequestObservation) -> ChatRequest {
    let mut req = ChatRequest {
        model: "gpt-test".into(),
        messages: vec![Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Text("read it".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        tools: Some(vec![reader_tool()]),
        ..Default::default()
    };
    req.routectl_internal.observation = Some(observation.clone());
    req
}

fn call(call_id: &str, arguments: &str) -> Value {
    json!({"type": "function_call", "id": format!("fc_{call_id}"), "call_id": call_id,
           "name": TOOL, "arguments": arguments, "status": "completed"})
}

/// An upstream answer whose output holds `items`, as the one SSE terminal
/// event `complete()` drains.
fn upstream(items: Vec<Value>) -> ResponseTemplate {
    let body = json!({"id": "resp_1", "object": "response", "created_at": 1_700_000_000_i64,
                      "status": "completed", "model": "gpt-test", "output": items,
                      "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}});
    let event = json!({"type": "response.completed", "response": body});
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(format!("data: {event}\n\n"))
}

async fn complete_on(auth_kind: AuthKind, items: Vec<Value>, req: ChatRequest) -> ChatResponse {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(upstream(items))
        .mount(&server)
        .await;
    provider(&server.uri(), auth_kind)
        .complete(req)
        .await
        .expect("complete")
}

fn census() -> u64 {
    translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == LANE && e.drop_class == CLASS)
        .map_or(0, |e| e.drop_count)
}

fn tool_call_arguments(resp: &ChatResponse) -> Vec<String> {
    resp.choices[0]
        .message
        .tool_calls
        .iter()
        .flatten()
        .map(|tc| {
            tc["function"]["arguments"]
                .as_str()
                .expect("string")
                .to_string()
        })
        .collect()
}

fn tool_use_inputs(resp: &ChatResponse) -> Vec<Value> {
    match &resp.choices[0].message.content {
        MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Known(KnownContentPart::ToolUse { input, .. }) => Some(input.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn without_pages() -> Value {
    json!({"allow_large": false, "file_path": PATH_SENTINEL, "limit": 2000, "offset": 0})
}

#[tokio::test]
async fn only_the_codex_lane_drops_an_empty_optional_argument() {
    // (row, lane, arguments expected on both carriers, drops counted)
    let rows = [
        (
            "api-key keeps it",
            AuthKind::ApiKey,
            serde_json::from_str::<Value>(EMPTY_PAGES).expect("fixture"),
            0,
        ),
        (
            "chatgpt-oauth drops it",
            AuthKind::ChatgptOauth,
            without_pages(),
            1,
        ),
    ];
    for (row, auth_kind, expected, drops) in rows {
        // Arrange
        let slot = RequestObservation::new();
        let before = census();

        // Act
        let resp = complete_on(auth_kind, vec![call("call_1", EMPTY_PAGES)], request(&slot)).await;

        // Assert
        let arguments = tool_call_arguments(&resp);
        assert_eq!(arguments.len(), 1, "{row}");
        let parsed: Value = serde_json::from_str(&arguments[0]).expect("arguments stay JSON");
        assert_eq!(parsed, expected, "{row}: tool_calls");
        assert_eq!(
            tool_use_inputs(&resp),
            vec![expected],
            "{row}: tool_use part"
        );
        if drops == 0 {
            assert_eq!(arguments[0], EMPTY_PAGES, "{row}: byte-equal passthrough");
        }
        assert_eq!(census() - before, drops, "{row}: census");
        assert_eq!(
            slot.snapshot().translation_drops,
            drops,
            "{row}: observation"
        );
    }
}

#[tokio::test]
async fn two_empty_optionals_in_one_response_count_once() {
    // Arrange: two calls in one response, each carrying an empty optional.
    let slot = RequestObservation::new();
    let before = census();
    let items = vec![call("call_1", EMPTY_PAGES), call("call_2", EMPTY_PAGES)];

    // Act
    let resp = complete_on(AuthKind::ChatgptOauth, items, request(&slot)).await;

    // Assert
    assert_eq!(
        tool_use_inputs(&resp),
        vec![without_pages(), without_pages()]
    );
    assert_eq!(census() - before, 1, "census");
    assert_eq!(slot.snapshot().translation_drops, 1, "observation");
}

#[tokio::test]
async fn a_codex_call_with_nothing_to_drop_is_byte_equal_and_uncounted() {
    // Arrange
    let slot = RequestObservation::new();
    let before = census();
    let arguments = r#"{"file_path": "/w/a.txt", "pages": "1-3"}"#;

    // Act
    let resp = complete_on(
        AuthKind::ChatgptOauth,
        vec![call("call_1", arguments)],
        request(&slot),
    )
    .await;

    // Assert
    assert_eq!(tool_call_arguments(&resp), vec![arguments.to_string()]);
    assert_eq!(census() - before, 0);
    assert_eq!(slot.snapshot().translation_drops, 0);
}

#[tokio::test]
async fn the_drop_is_declared_on_a_response_leg_that_is_compared() {
    // (row, tap, drop declared)
    let rows = [
        (
            "fidelity tap",
            WireTap::checking_response(IngressDialect::Responses),
            true,
        ),
        ("translation tap", WireTap::admitted(), false),
    ];
    for (row, tap, declared) in rows {
        // Arrange
        let slot = RequestObservation::new();
        let mut req = request(&slot);
        req.routectl_internal.wire_tap = Some(tap.clone());

        // Act
        complete_on(
            AuthKind::ChatgptOauth,
            vec![call("call_1", EMPTY_PAGES)],
            req,
        )
        .await;

        // Assert
        let edits: Vec<_> = tap
            .take_response()
            .edits(Leg::Response)
            .iter()
            .filter(|e| e.site() == EditSite::DialectFieldStrip)
            .map(|e| (e.path().to_string(), e.kind()))
            .collect();
        let want = if declared {
            vec![(EMPTY_OPTIONAL_ARGUMENTS_PATH.to_string(), EditKind::Rewrite)]
        } else {
            Vec::new()
        };
        assert_eq!(edits, want, "{row}");
    }
}

#[test]
fn the_declared_pattern_names_a_returned_call_s_arguments() {
    // Arrange: where a Responses client's body carries the second output
    // item's arguments, and a sibling field the declaration must not cover.
    let arguments = WirePath::root().child("output").index(1).child("arguments");
    let name = WirePath::root().child("output").index(1).child("name");
    let edit = DeclaredEdit::new(
        EditSite::DialectFieldStrip,
        EMPTY_OPTIONAL_ARGUMENTS_PATH,
        EditKind::Rewrite,
    )
    .expect("the register accepts the pattern");

    // Act / Assert
    assert!(edit.matches(&arguments));
    assert!(!edit.matches(&name));
}

#[traced_test]
#[tokio::test]
async fn the_drop_log_names_tool_and_count_and_never_a_value() {
    // Arrange
    let slot = RequestObservation::new();

    // Act
    complete_on(
        AuthKind::ChatgptOauth,
        vec![call("call_1", EMPTY_PAGES)],
        request(&slot),
    )
    .await;

    // Assert
    assert!(logs_contain(
        "dropping empty optional function call arguments"
    ));
    assert!(logs_contain("read_file:1"));
    assert!(!logs_contain(PATH_SENTINEL));
    assert!(!logs_contain("allow_large"));
}
