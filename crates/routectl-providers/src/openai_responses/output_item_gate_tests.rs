//! The non-stream provenance gate on upstream output items this lane does not
//! model: a Responses client keeps each one, every other client loses it, and
//! the loss counts once per client request on the census and on the request's
//! observation slot.
//!
//! The unit-test registry is per thread and `complete()` runs on the test's
//! own current-thread runtime, so each delta is exactly what that test caused.

use std::sync::Arc;

use routectl_core::{
    ChatRequest, ChatResponse, ContentPart, Message, MessageContent, Provider, RequestObservation,
    RequestProvenance, Role, StaticToken, TokenSource,
};
use serde_json::{Value, json};
use tracing_test::traced_test;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{AuthKind, LANE, OpenAiResponsesConfig, OpenAiResponsesProvider};
use crate::translation_drop_metrics::{translation_drop_snapshot, translation_lane_seen};

const CLASS: &str = "output_item_unrepresentable";
const UNMODELED: &str = "custom_tool_call";
const ITEM_BODY_SENTINEL: &str = "item-body-sentinel-7f3a";

fn provider(base_url: &str) -> OpenAiResponsesProvider {
    OpenAiResponsesProvider::new(OpenAiResponsesConfig {
        id: "responses-gate".into(),
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

fn unmodeled_item(call_id: &str) -> Value {
    json!({"type": UNMODELED, "id": format!("ctc_{call_id}"), "call_id": call_id,
           "name": "exec", "input": ITEM_BODY_SENTINEL, "status": "completed"})
}

fn message_item(content: Value) -> Value {
    json!({"type": "message", "id": "m1", "role": "assistant", "status": "completed",
           "content": content})
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

fn text_with_unmodeled_items(count: usize) -> Vec<Value> {
    let mut items = vec![message_item(
        json!([{"type": "output_text", "text": "ok", "annotations": []}]),
    )];
    items.extend((0..count).map(|n| unmodeled_item(&format!("call_{n}"))));
    items
}

async fn complete_against(template: ResponseTemplate, req: ChatRequest) -> ChatResponse {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(template)
        .mount(&server)
        .await;
    provider(&server.uri())
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

fn type_tags(resp: &ChatResponse) -> Vec<String> {
    match &resp.choices[0].message.content {
        MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|p| match p {
                ContentPart::Other { type_tag, .. } => Some(type_tag.clone()),
                ContentPart::Known(_) => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

#[tokio::test]
async fn a_non_responses_client_loses_the_unmodeled_item_and_counts_it_once() {
    // (row, provenance, unmodeled items upstream, item kept, drops counted)
    let rows = [
        (
            "anthropic client",
            RequestProvenance::AnthropicIngress,
            1,
            false,
            1,
        ),
        ("chat client", RequestProvenance::OpenaiIngress, 1, false, 1),
        ("library caller", RequestProvenance::Library, 1, false, 1),
        (
            "two items, one request",
            RequestProvenance::OpenaiIngress,
            2,
            false,
            1,
        ),
        (
            "responses client",
            RequestProvenance::ResponsesIngress,
            1,
            true,
            0,
        ),
    ];
    for (row, provenance, items, kept, drops) in rows {
        // Arrange
        let slot = RequestObservation::new();
        let (census_before, seen_before) = (census(), translation_lane_seen(LANE));

        // Act
        let resp = complete_against(
            upstream(text_with_unmodeled_items(items)),
            request(provenance, &slot),
        )
        .await;

        // Assert
        let tags = type_tags(&resp);
        if kept {
            assert_eq!(tags, vec![UNMODELED.to_string()], "{row}");
        } else {
            assert!(tags.is_empty(), "{row}: {tags:?}");
            assert!(
                matches!(&resp.choices[0].message.content, MessageContent::Text(t) if t == "ok"),
                "{row}: the surviving text keeps the text-only shape, got {:?}",
                resp.choices[0].message.content
            );
        }
        assert_eq!(census() - census_before, drops, "{row}: census");
        assert_eq!(
            slot.snapshot().translation_drops,
            drops,
            "{row}: observation"
        );
        assert_eq!(
            translation_lane_seen(LANE) - seen_before,
            1,
            "{row}: the request reached the lane"
        );
        assert_eq!(
            resp.choices[0].finish_reason.as_deref(),
            Some("stop"),
            "{row}: finish reason"
        );
    }
}

#[tokio::test]
async fn a_retried_request_counts_the_dropped_item_once() {
    // Arrange: one client request normalized twice (a retry on one target).
    let slot = RequestObservation::new();
    let before = census();

    // Act
    for _ in 0..2 {
        complete_against(
            upstream(text_with_unmodeled_items(1)),
            request(RequestProvenance::AnthropicIngress, &slot),
        )
        .await;
    }

    // Assert
    assert_eq!(census() - before, 1);
    assert_eq!(slot.snapshot().translation_drops, 1);
}

#[tokio::test]
async fn a_refusal_stays_for_a_non_responses_client_and_counts_nothing() {
    // Arrange: the positive control for the refusal exemption, with the
    // same provenance that drops an unmodeled item.
    let slot = RequestObservation::new();
    let before = census();
    let items = vec![message_item(json!([{"type": "refusal", "refusal": "no"}]))];

    // Act
    let resp = complete_against(
        upstream(items),
        request(RequestProvenance::OpenaiIngress, &slot),
    )
    .await;

    // Assert
    assert_eq!(type_tags(&resp), vec!["refusal".to_string()]);
    assert_eq!(census() - before, 0);
    assert_eq!(slot.snapshot().translation_drops, 0);
}

#[traced_test]
#[tokio::test]
async fn the_drop_log_names_the_item_type_and_never_its_body() {
    // Arrange
    let slot = RequestObservation::new();

    // Act
    complete_against(
        upstream(text_with_unmodeled_items(1)),
        request(RequestProvenance::AnthropicIngress, &slot),
    )
    .await;

    // Assert
    assert!(logs_contain("dropping unmodeled output items"));
    assert!(logs_contain(UNMODELED));
    assert!(!logs_contain(ITEM_BODY_SENTINEL));
}
