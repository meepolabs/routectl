//! Emission tests for the lane-faithful reasoning format tag.
//!
//! The three Responses lanes validate replay incompatibly, so each must
//! stamp its own tag, the streaming and non-streaming paths must agree
//! tag-for-tag on the same lane, and the legacy shared tag must never
//! leave an egress again.

use serde_json::{Value, json};

use routectl_core::{
    BEDROCK_MANTLE, CODEX_OAUTH, ChatRequest, Message, MessageContent, OPENAI_APIKEY,
    OPENAI_RESPONSES_V1, ReasoningDetail, Role,
};

use super::response;
use super::response_types::{ResponsesResponse, ResponsesStreamEvent};
use super::sse::ResponsesStreamState;
use super::{AuthKind, OpenAiResponsesConfig, lane_format_tag, request};

const LANES: [AuthKind; 3] = [
    AuthKind::ChatgptOauth,
    AuthKind::ApiKey,
    AuthKind::BedrockMantle,
];

fn reasoning_body() -> Value {
    json!({
        "id": "resp_01",
        "status": "completed",
        "model": "test-model",
        "output": [{
            "type": "reasoning",
            "id": "rs_1",
            "summary": [{"type": "summary_text", "text": "step"}],
            "content": [{"type": "reasoning_text", "text": "detail"}],
            "encrypted_content": "sig"
        }]
    })
}

fn non_streaming_details(auth_kind: AuthKind) -> Vec<ReasoningDetail> {
    let typed: ResponsesResponse = serde_json::from_value(reasoning_body()).unwrap();
    let resp = response::translate("test", auth_kind, typed).expect("translate");
    resp.choices[0].message.reasoning_details.clone()
}

fn streaming_details(auth_kind: AuthKind) -> Vec<ReasoningDetail> {
    let events = [
        json!({"type": "response.created", "response": {"id": "resp_01", "model": "test-model"}}),
        json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "reasoning", "id": "rs_1", "summary": []}
        }),
        json!({
            "type": "response.reasoning_summary_text.delta",
            "output_index": 0,
            "summary_index": 0,
            "delta": "step"
        }),
        json!({
            "type": "response.reasoning_text.delta",
            "output_index": 0,
            "content_index": 0,
            "delta": "detail"
        }),
        json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {"type": "reasoning", "id": "rs_1", "summary": [],
                     "encrypted_content": "sig"}
        }),
    ];

    let mut state = ResponsesStreamState::new(auth_kind);
    let mut details = Vec::new();
    for ev in events {
        let typed: ResponsesStreamEvent = serde_json::from_value(ev).unwrap();
        for chunk in state.parse_event("test", typed).expect("event processing") {
            details.extend(chunk.choices[0].delta.reasoning_details.clone());
        }
    }
    details
}

fn assert_all_tagged(details: &[ReasoningDetail], expected: &str) {
    assert!(!details.is_empty(), "no reasoning details emitted");
    for d in details {
        assert_eq!(d.format.as_deref(), Some(expected));
    }
}

#[test]
fn chatgpt_oauth_lane_stamps_codex_tag_non_streaming() {
    assert_all_tagged(&non_streaming_details(AuthKind::ChatgptOauth), CODEX_OAUTH);
}

#[test]
fn api_key_lane_stamps_api_key_tag_non_streaming() {
    assert_all_tagged(&non_streaming_details(AuthKind::ApiKey), OPENAI_APIKEY);
}

#[test]
fn bedrock_mantle_lane_stamps_mantle_tag_non_streaming() {
    assert_all_tagged(
        &non_streaming_details(AuthKind::BedrockMantle),
        BEDROCK_MANTLE,
    );
}

#[test]
fn streaming_and_non_streaming_paths_stamp_identical_tag_per_lane() {
    // Divergence between these two paths is the bug class this tag
    // vocabulary exists to remove: the same lane must mint artifacts a
    // later turn can replay regardless of which path produced them.
    for lane in LANES {
        let expected = lane_format_tag(lane);
        assert_all_tagged(&non_streaming_details(lane), expected);
        assert_all_tagged(&streaming_details(lane), expected);
    }
}

#[test]
fn no_lane_emits_the_legacy_shared_tag() {
    for lane in LANES {
        for d in non_streaming_details(lane)
            .iter()
            .chain(streaming_details(lane).iter())
        {
            assert_ne!(d.format.as_deref(), Some(OPENAI_RESPONSES_V1));
        }
    }
}

/// A freshly emitted lane tag must survive the trip back out to the
/// egress: the detail is lifted into a Reasoning input item and its
/// signature reaches the next request body. A reader still comparing the
/// tag by equality against the legacy value would drop it here instead of
/// failing loudly, which is the regression this pins.
#[test]
fn freshly_tagged_detail_replays_onto_the_wire_of_its_own_lane() {
    for lane in LANES {
        for details in [non_streaming_details(lane), streaming_details(lane)] {
            let assistant = Message {
                refusal: None,
                role: Role::Assistant,
                content: MessageContent::Text("answer".into()),
                reasoning: None,
                reasoning_details: details,
                name: None,
                tool_call_id: None,
                tool_calls: None,
            };
            let req = ChatRequest {
                model: "test-model".into(),
                messages: vec![assistant].into(),
                ..Default::default()
            };
            let mut cfg = OpenAiResponsesConfig::new("openai-responses:test", "literal:test");
            cfg.auth_kind = lane;

            let body = serde_json::to_value(request::translate(&cfg, &req).expect("translate"))
                .expect("serialize");

            let reasoning = &body["input"][0];
            assert_eq!(reasoning["type"], "reasoning", "lane {lane:?}");
            assert_eq!(
                reasoning["encrypted_content"], "sig",
                "lane {lane:?} dropped the signature of a detail it minted itself"
            );
        }
    }
}
