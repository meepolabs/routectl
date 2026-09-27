//! A Responses stream that crossed HTTP 200 must still tell the client whether
//! the turn finished. Every turn closes with exactly one of
//! `response.completed`, `response.incomplete`, `response.failed`, or
//! `response.cancelled`; a body that ends before one of them is a cut stream
//! and must surface as an error. Complete streams -- including a trailing
//! `[DONE]` sentinel and benign `error: null` / `error: {}` fields -- must
//! stay error-free.

use super::*;
use futures::StreamExt;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PROVIDER_ID: &str = "openai-responses:terminal-test";

fn provider_for(base_url: &str) -> OpenAiResponsesProvider {
    OpenAiResponsesProvider::new(OpenAiResponsesConfig {
        id: PROVIDER_ID.into(),
        auth: Arc::new(StaticToken::new("test-jwt")) as Arc<dyn TokenSource>,
        account_id: Some("acct-uuid".into()),
        base_url: base_url.to_string(),
        auth_kind: AuthKind::ChatgptOauth,
        header_extras: Vec::new(),
        user_agent: None,
        session_id: None,
        installation_id: None,
        #[cfg(feature = "bedrock")]
        mantle: None,
    })
}

fn req() -> ChatRequest {
    ChatRequest {
        model: "gpt-5-codex".into(),
        messages: vec![routectl_core::Message {
            refusal: None,
            role: routectl_core::Role::User,
            content: routectl_core::MessageContent::Text("ping".into()),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        }]
        .into(),
        max_tokens: Some(64),
        ..Default::default()
    }
}

fn created() -> Value {
    json!({"type": "response.created", "response": {"id": "r", "model": "m"}})
}

fn item_added() -> Value {
    json!({
        "type": "response.output_item.added", "output_index": 0,
        "item": {"type": "message", "id": "m1", "role": "assistant", "content": []}
    })
}

fn text_delta() -> Value {
    json!({"type": "response.output_text.delta", "output_index": 0, "delta": "hi"})
}

fn terminal(kind: &str, status: &str) -> Value {
    json!({
        "type": kind,
        "response": {
            "id": "r", "status": status, "model": "m",
            "incomplete_details": if status == "incomplete" {
                json!({"reason": "max_output_tokens"})
            } else {
                Value::Null
            },
            "output": [{"type": "message", "id": "m1", "role": "assistant",
                        "content": [{"type": "output_text", "text": "hi"}]}],
            "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
        }
    })
}

fn sse(events: &[Value]) -> String {
    events.iter().map(|e| format!("data: {e}\n\n")).collect()
}

async fn serve(body: String) -> (MockServer, OpenAiResponsesProvider) {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(body)
                .insert_header("content-type", "text/event-stream"),
        )
        .mount(&server)
        .await;
    let provider = provider_for(&server.uri());
    (server, provider)
}

async fn drain(body: String) -> Vec<Result<ChatChunk>> {
    let (_server, provider) = serve(body).await;
    let mut stream = provider.stream(req()).await.expect("stream opens");
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        items.push(item);
    }
    items
}

fn errors(items: &[Result<ChatChunk>]) -> Vec<&Error> {
    items.iter().filter_map(|r| r.as_ref().err()).collect()
}

fn final_finish_reason(items: &[Result<ChatChunk>]) -> Option<String> {
    items
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .filter_map(|c| c.choices.first()?.finish_reason.clone())
        .next_back()
}

fn assert_missing_terminal(err: &Error) {
    match err {
        Error::Upstream {
            provider,
            status,
            body,
            ..
        } => {
            assert_eq!(provider, PROVIDER_ID);
            assert_eq!(*status, 0);
            assert!(
                body.contains("stream ended without a terminal event"),
                "unexpected body: {body}"
            );
        }
        other => panic!("expected a missing-terminal Upstream error, got {other:?}"),
    }
}

fn assert_ends_with_missing_terminal(items: &[Result<ChatChunk>]) {
    let errs = errors(items);
    assert_eq!(errs.len(), 1, "exactly one error expected, got {errs:?}");
    let last = items.last().expect("stream yielded items");
    assert_missing_terminal(last.as_ref().expect_err("the error is the last item"));
}

#[tokio::test]
async fn completed_stream_ends_without_error() {
    let items = drain(sse(&[
        created(),
        item_added(),
        text_delta(),
        terminal("response.completed", "completed"),
    ]))
    .await;

    assert!(
        errors(&items).is_empty(),
        "healthy stream errored: {items:?}"
    );
    assert_eq!(final_finish_reason(&items).as_deref(), Some("stop"));
}

#[tokio::test]
async fn incomplete_stream_ends_without_error_as_length() {
    let items = drain(sse(&[
        created(),
        item_added(),
        text_delta(),
        terminal("response.incomplete", "incomplete"),
    ]))
    .await;

    assert!(
        errors(&items).is_empty(),
        "incomplete is success-with-cutoff: {items:?}"
    );
    assert_eq!(final_finish_reason(&items).as_deref(), Some("length"));
}

#[tokio::test]
async fn done_sentinel_after_completed_ends_without_error() {
    let body = format!(
        "{}data: [DONE]\n\n",
        sse(&[
            created(),
            item_added(),
            text_delta(),
            terminal("response.completed", "completed"),
        ])
    );

    let items = drain(body).await;

    assert!(
        errors(&items).is_empty(),
        "[DONE] after the terminal errored: {items:?}"
    );
    assert_eq!(final_finish_reason(&items).as_deref(), Some("stop"));
}

#[tokio::test]
async fn benign_error_fields_and_keepalives_do_not_fail_a_completed_stream() {
    let mut delta = text_delta();
    delta["error"] = Value::Null;
    let mut in_progress = json!({"type": "response.in_progress", "response": {"id": "r"}});
    in_progress["error"] = json!({});
    let mut done = terminal("response.completed", "completed");
    done["response"]["error"] = Value::Null;
    let body = format!(
        ": keepalive comment\n\n{}",
        sse(&[created(), in_progress, item_added(), delta, done])
    );

    let items = drain(body).await;

    assert!(
        errors(&items).is_empty(),
        "benign fields errored: {items:?}"
    );
    assert_eq!(final_finish_reason(&items).as_deref(), Some("stop"));
}

#[tokio::test]
async fn stream_cut_mid_content_surfaces_missing_terminal_error() {
    let items = drain(sse(&[created(), item_added(), text_delta()])).await;

    assert!(
        items.iter().any(|r| r
            .as_ref()
            .is_ok_and(|c| c.choices[0].delta.content.as_deref() == Some("hi"))),
        "content before the cut still reaches the client"
    );
    assert_ends_with_missing_terminal(&items);
}

#[tokio::test]
async fn empty_success_body_surfaces_missing_terminal_error() {
    let items = drain(String::new()).await;

    assert_eq!(items.len(), 1, "only the error is yielded: {items:?}");
    assert_ends_with_missing_terminal(&items);
}

#[tokio::test]
async fn failed_terminal_surfaces_its_own_error_only() {
    let mut failed = terminal("response.failed", "failed");
    failed["response"]["error"] = json!({"code": "server_error", "message": "boom"});

    let items = drain(sse(&[created(), item_added(), text_delta(), failed])).await;

    let errs = errors(&items);
    assert_eq!(
        errs.len(),
        1,
        "one error, not a second missing-terminal: {errs:?}"
    );
    match errs[0] {
        Error::Upstream { body, .. } => assert!(body.contains("boom"), "got {body}"),
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn cancelled_terminal_surfaces_its_own_error_only() {
    let items = drain(sse(&[
        created(),
        terminal("response.cancelled", "cancelled"),
    ]))
    .await;

    let errs = errors(&items);
    assert_eq!(
        errs.len(),
        1,
        "one error, not a second missing-terminal: {errs:?}"
    );
    match errs[0] {
        Error::Upstream { body, .. } => assert!(
            !body.contains("stream ended without a terminal event"),
            "cancellation is a terminal, not a cut: {body}"
        ),
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn complete_on_cut_stream_returns_missing_terminal_error() {
    let (_server, provider) = serve(sse(&[created(), item_added(), text_delta()])).await;

    let err = provider.complete(req()).await.expect_err("cut stream");

    assert_missing_terminal(&err);
}

#[tokio::test]
async fn complete_with_done_sentinel_after_completed_returns_response() {
    let body = format!(
        "{}data: [DONE]\n\n",
        sse(&[
            created(),
            item_added(),
            text_delta(),
            terminal("response.completed", "completed"),
        ])
    );
    let (_server, provider) = serve(body).await;

    let resp = provider.complete(req()).await.expect("healthy stream");

    assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
}

#[tokio::test]
async fn complete_on_failed_or_cancelled_terminal_reports_the_terminal_not_a_cut() {
    for (kind, status) in [
        ("response.failed", "failed"),
        ("response.cancelled", "cancelled"),
    ] {
        let (_server, provider) = serve(sse(&[created(), terminal(kind, status)])).await;

        let err = provider.complete(req()).await.expect_err(kind);

        match err {
            Error::Upstream { body, .. } => assert!(
                !body.contains("stream ended without a terminal event"),
                "{kind} is a terminal, not a cut: {body}"
            ),
            other => panic!("{kind}: expected Upstream, got {other:?}"),
        }
    }
}
