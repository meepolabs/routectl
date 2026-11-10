//! Output items the Responses stream parser does not model (for example
//! `custom_tool_call`): a Responses client receives each one whole, once, on
//! an `OpaqueSseEvent::OutputItem`; every other client loses it, counted once
//! per request on the census and on the request's observation slot.
//!
//! The unit-test registry is per thread and every stream here is polled on
//! the test's own current-thread runtime, so each delta is exactly what that
//! test caused.

use std::sync::Arc;

use futures::StreamExt;
use routectl_core::{
    ChatChunk, ChatRequest, Message, MessageContent, OpaqueSseEvent, Provider, RequestObservation,
    RequestProvenance, Role, StaticToken, TokenSource,
};
use serde_json::{Value, json};
use tracing_test::traced_test;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{MAX_OUTPUT_BLOCKS, ResponsesStreamState};
use crate::openai_responses::response_types::ResponsesStreamEvent;
use crate::openai_responses::{AuthKind, LANE, OpenAiResponsesConfig, OpenAiResponsesProvider};
use crate::translation_drop_metrics::translation_drop_snapshot;

const CLASS: &str = "output_item_unrepresentable";
const ITEM_TYPE: &str = "custom_tool_call";
const FULL_INPUT: &str = "item-body-sentinel-4c1d";

fn added(output_index: u32) -> Value {
    json!({"type": "response.output_item.added", "output_index": output_index,
           "item": {"type": ITEM_TYPE, "id": format!("ctc_{output_index}"),
                    "call_id": format!("call_{output_index}"), "name": "exec",
                    "input": "", "status": "in_progress"}})
}

fn full_item(output_index: u32) -> Value {
    json!({"type": ITEM_TYPE, "id": format!("ctc_{output_index}"),
           "call_id": format!("call_{output_index}"), "name": "exec",
           "input": FULL_INPUT, "status": "completed"})
}

fn input_delta(output_index: u32) -> Value {
    json!({"type": "response.custom_tool_call_input.delta", "output_index": output_index,
           "item_id": format!("ctc_{output_index}"), "delta": "partial"})
}

fn done(output_index: u32) -> Value {
    json!({"type": "response.output_item.done", "output_index": output_index,
           "item": full_item(output_index)})
}

fn completed(output: Vec<Value>) -> Value {
    json!({"type": "response.completed",
           "response": {"id": "resp_1", "model": "gpt-test", "status": "completed",
                        "output": output,
                        "usage": {"input_tokens": 3, "output_tokens": 1, "total_tokens": 4}}})
}

fn message_events() -> Vec<Value> {
    vec![
        json!({"type": "response.output_item.added", "output_index": 0,
               "item": {"type": "message", "id": "m1", "role": "assistant", "content": []}}),
        json!({"type": "response.output_text.delta", "output_index": 0, "delta": "ok"}),
        json!({"type": "response.output_item.done", "output_index": 0,
               "item": {"type": "message", "id": "m1", "role": "assistant",
                        "content": [{"type": "output_text", "text": "ok"}]}}),
    ]
}

fn message_item() -> Value {
    json!({"type": "message", "id": "m1", "role": "assistant", "status": "completed",
           "content": [{"type": "output_text", "text": "ok", "annotations": []}]})
}

fn responses_state() -> ResponsesStreamState {
    ResponsesStreamState::new(
        AuthKind::ApiKey,
        RequestProvenance::ResponsesIngress,
        None,
        &[],
        None,
    )
}

fn drive_all(state: &mut ResponsesStreamState, events: Vec<Value>) -> Vec<ChatChunk> {
    events
        .into_iter()
        .flat_map(|ev| {
            let typed: ResponsesStreamEvent = serde_json::from_value(ev).expect("event parse");
            state.parse_event("test", typed).expect("event processing")
        })
        .collect()
}

fn carried_items(chunks: &[ChatChunk]) -> Vec<Value> {
    chunks
        .iter()
        .flat_map(|c| &c.opaque_events)
        .filter_map(|e| match e {
            OpaqueSseEvent::OutputItem { item } => Some(item.clone()),
            _ => None,
        })
        .collect()
}

fn census() -> u64 {
    translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == LANE && e.drop_class == CLASS)
        .map_or(0, |e| e.drop_count)
}

#[test]
fn a_responses_client_receives_the_finalized_item_exactly_once() {
    // (row, events after the message, completed body output)
    let rows = [
        (
            "added, input delta, done, completed",
            vec![added(1), input_delta(1), done(1)],
            vec![message_item(), full_item(1)],
        ),
        (
            "done then completed with an empty body",
            vec![added(1), done(1)],
            vec![],
        ),
        (
            "completed with no done flushes the body's copy",
            vec![added(1), input_delta(1)],
            vec![message_item(), full_item(1)],
        ),
    ];
    for (row, item_events, body) in rows {
        // Arrange
        let mut state = responses_state();
        let mut events = message_events();
        events.extend(item_events);
        events.push(completed(body));

        // Act
        let chunks = drive_all(&mut state, events);

        // Assert
        assert_eq!(carried_items(&chunks), vec![full_item(1)], "{row}");
        let finish: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.choices.first()?.finish_reason.clone())
            .collect();
        assert_eq!(finish, vec!["stop".to_string()], "{row}: finish reason");
    }
}

#[test]
fn completed_with_no_done_and_no_body_copy_flushes_the_added_item_once() {
    // Arrange
    let mut state = responses_state();
    let mut events = message_events();
    events.push(added(1));
    events.push(completed(vec![]));

    // Act
    let chunks = drive_all(&mut state, events);

    // Assert
    let items = carried_items(&chunks);
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["call_id"], "call_1");
    assert_eq!(items[0]["input"], "");
}

#[test]
fn the_custom_tool_input_preview_is_not_forwarded() {
    // Arrange
    let mut state = responses_state();
    drive_all(&mut state, vec![added(1)]);

    // Act
    let chunks = drive_all(&mut state, vec![input_delta(1)]);

    // Assert
    assert!(chunks.is_empty(), "{chunks:?}");
}

fn provider(base_url: &str) -> OpenAiResponsesProvider {
    OpenAiResponsesProvider::new(OpenAiResponsesConfig {
        id: "responses-stream-gate".into(),
        auth: Arc::new(StaticToken::new("test-jwt")) as Arc<dyn TokenSource>,
        account_id: Some("acct".into()),
        base_url: base_url.to_string(),
        auth_kind: AuthKind::ApiKey,
        header_extras: Vec::new(),
        user_agent: None,
        session_id: None,
        installation_id: None,
        #[cfg(feature = "bedrock")]
        mantle: None,
    })
}

fn request(provenance: RequestProvenance, observation: &RequestObservation) -> ChatRequest {
    let mut req = ChatRequest {
        model: "gpt-test".into(),
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
        ..Default::default()
    };
    req.routectl_internal.provenance = provenance;
    req.routectl_internal.observation = Some(observation.clone());
    req
}

/// A streamed answer: the message, then `items` unmodeled items each with
/// its own added and done events, then completion.
fn upstream_stream(items: u32) -> ResponseTemplate {
    let mut events = vec![json!({"type": "response.created",
                                 "response": {"id": "resp_1", "model": "gpt-test"}})];
    events.extend(message_events());
    let mut output = vec![message_item()];
    for idx in 1..=items {
        events.extend([added(idx), input_delta(idx), done(idx)]);
        output.push(full_item(idx));
    }
    events.push(completed(output));
    let body: String = events.iter().map(|e| format!("data: {e}\n\n")).collect();
    ResponseTemplate::new(200)
        .insert_header("content-type", "text/event-stream")
        .set_body_string(body)
}

async fn stream_against(template: ResponseTemplate, req: ChatRequest) -> Vec<ChatChunk> {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(template)
        .mount(&server)
        .await;
    let mut stream = provider(&server.uri())
        .stream(req)
        .await
        .expect("stream opens");
    let mut chunks = Vec::new();
    while let Some(item) = stream.next().await {
        chunks.push(item.expect("stream item"));
    }
    chunks
}

#[tokio::test]
async fn a_non_responses_client_loses_the_streamed_item_and_counts_it_once() {
    // (row, provenance, unmodeled items upstream, items delivered, drops counted)
    let rows = [
        (
            "anthropic client",
            RequestProvenance::AnthropicIngress,
            1,
            0,
            1,
        ),
        ("chat client", RequestProvenance::OpenaiIngress, 1, 0, 1),
        (
            "two items, one request",
            RequestProvenance::OpenaiIngress,
            2,
            0,
            1,
        ),
        (
            "responses client",
            RequestProvenance::ResponsesIngress,
            1,
            1,
            0,
        ),
    ];
    for (row, provenance, items, delivered, drops) in rows {
        // Arrange
        let slot = RequestObservation::new();
        let before = census();

        // Act
        let chunks = stream_against(upstream_stream(items), request(provenance, &slot)).await;

        // Assert
        assert_eq!(carried_items(&chunks).len(), delivered, "{row}: delivered");
        assert_eq!(census() - before, drops, "{row}: census");
        assert_eq!(
            slot.snapshot().translation_drops,
            drops,
            "{row}: observation"
        );
        let finish: Vec<_> = chunks
            .iter()
            .filter_map(|c| c.choices.first()?.finish_reason.clone())
            .collect();
        assert_eq!(finish, vec!["stop".to_string()], "{row}: finish reason");
    }
}

#[traced_test]
#[tokio::test]
async fn the_stream_drop_log_names_the_item_type_and_never_its_body() {
    // Arrange
    let slot = RequestObservation::new();

    // Act
    stream_against(
        upstream_stream(1),
        request(RequestProvenance::AnthropicIngress, &slot),
    )
    .await;

    // Assert
    assert!(logs_contain(
        "dropping unmodeled output item the client dialect cannot carry"
    ));
    assert!(logs_contain(ITEM_TYPE));
    assert!(!logs_contain(FULL_INPUT));
}

/// A state whose block map already holds `MAX_OUTPUT_BLOCKS` open items.
fn state_at_cap(slot: &RequestObservation) -> ResponsesStreamState {
    let mut state = ResponsesStreamState::new(
        AuthKind::ApiKey,
        RequestProvenance::ResponsesIngress,
        Some(slot.clone()),
        &[],
        None,
    );
    let fill = (0..MAX_OUTPUT_BLOCKS as u32).map(|idx| {
        json!({"type": "response.output_item.added", "output_index": idx,
               "item": {"type": "message", "id": format!("m{idx}"), "role": "assistant",
                        "content": []}})
    });
    drive_all(&mut state, fill.collect());
    state
}

#[traced_test]
#[test]
fn an_item_past_the_block_cap_is_counted_and_warned() {
    // (row, items added past the cap, drops counted)
    let rows = [
        ("at the cap", 0, 0),
        ("one past", 1, 1),
        ("three past", 3, 1),
    ];
    for (row, past, drops) in rows {
        // Arrange
        let slot = RequestObservation::new();
        let mut state = state_at_cap(&slot);
        let before = census();
        let cap = MAX_OUTPUT_BLOCKS as u32;
        let mut events: Vec<Value> = (cap..cap + past).map(added).collect();
        events.extend((cap..cap + past).map(done));
        events.push(completed(vec![]));

        // Act
        let chunks = drive_all(&mut state, events);

        // Assert
        assert!(carried_items(&chunks).is_empty(), "{row}: delivered");
        assert_eq!(census() - before, drops, "{row}: census");
        assert_eq!(
            slot.snapshot().translation_drops,
            drops,
            "{row}: observation"
        );
    }
    logs_assert(|lines: &[&str]| {
        let warned = lines
            .iter()
            .filter(|l| l.contains("WARN") && l.contains("output_item.added beyond cap"))
            .count();
        let typed = lines
            .iter()
            .filter(|l| l.contains("output_item.added beyond cap") && l.contains(ITEM_TYPE))
            .count();
        // One WARN per stream that crossed the cap (two rows), every skip
        // naming the item type and never the item body.
        match (warned, typed) {
            (2, 4) => Ok(()),
            other => Err(format!("(warned, typed) = {other:?}; expected (2, 4)")),
        }
    });
    assert!(!logs_contain(FULL_INPUT));
}
