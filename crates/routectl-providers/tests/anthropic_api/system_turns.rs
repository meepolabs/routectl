//! Mid-conversation `role: "system"` turns on the assembled wire body: the
//! forward-vs-lift split at the provider boundary, the breakpoint cap, and
//! the count_tokens allowlist.

use super::*;
use pretty_assertions::assert_eq;

const fn system_parts_msg(parts: Vec<ContentPart>) -> Message {
    Message {
        refusal: None,
        role: Role::System,
        content: MessageContent::Parts(parts),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn marked_text_part(text: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::Text {
        text: text.into(),
        citations: None,
        cache_control: Some(CacheControl::ephemeral_5m()),
    })
}

/// Both present: the canonical system owns the wire `system` field and the
/// mid-conversation system turn rides `messages[]` at its original index
/// with byte-identical content.
#[test]
fn canonical_system_and_system_turn_both_reach_the_wire() {
    // Arrange
    let provider = make_provider("https://api.anthropic.com");
    let mut req = base_req(
        "claude-opus-4-8",
        vec![
            user_msg("first"),
            system_msg("The user sent a new message while you were working"),
            assistant_msg("noted"),
            user_msg("second"),
        ],
    );
    req.system = Some(SystemContent::Text("you are a helpful assistant".into()));

    // Act
    let body = provider.normalize_request(&req).unwrap();

    // Assert
    assert_eq!(body["system"], "you are a helpful assistant");
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 4, "got: {body}");
    assert_eq!(msgs[1]["role"], "system");
    assert_eq!(
        msgs[1]["content"],
        "The user sent a new message while you were working"
    );
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[2]["role"], "assistant");
    assert_eq!(msgs[3]["role"], "user");
}

/// Positive control for the test above: with the canonical system removed
/// the legacy lift fires, so the system text lands in the `system` field
/// and NO `role: "system"` message reaches `messages[]`.
#[test]
fn without_canonical_system_the_lift_fires_and_emits_no_system_turn() {
    // Arrange
    let provider = make_provider("https://api.anthropic.com");
    let req = base_req(
        "claude-opus-4-8",
        vec![
            user_msg("first"),
            system_msg("The user sent a new message while you were working"),
            assistant_msg("noted"),
            user_msg("second"),
        ],
    );

    // Act
    let body = provider.normalize_request(&req).unwrap();

    // Assert
    assert_eq!(
        body["system"],
        "The user sent a new message while you were working"
    );
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3, "got: {body}");
    assert!(
        msgs.iter().all(|m| m["role"] != "system"),
        "the lift consumed the turn; nothing re-emits it: {body}"
    );
}

/// A cache breakpoint on a forwarded system turn counts against the
/// 4-marker cap locally, so the fifth marker is a routectl validation
/// error rather than an opaque upstream 400.
#[test]
fn marker_on_a_forwarded_system_turn_counts_against_the_breakpoint_cap() {
    // Arrange
    let provider = make_provider("https://api.anthropic.com");
    let mut req = base_req(
        "claude-opus-4-8",
        vec![
            Message {
                refusal: None,
                role: Role::User,
                content: MessageContent::Parts(vec![
                    marked_text_part("a"),
                    marked_text_part("b"),
                    marked_text_part("c"),
                    marked_text_part("d"),
                ]),
                reasoning: None,
                reasoning_details: vec![],
                name: None,
                tool_call_id: None,
                tool_calls: None,
            },
            system_parts_msg(vec![marked_text_part("fifth marker")]),
            assistant_msg("ok"),
        ],
    );
    req.system = Some(SystemContent::Text("you are a helpful assistant".into()));

    // Act
    let err = provider
        .normalize_request(&req)
        .expect_err("expected breakpoint cap violation");

    // Assert
    let msg = format!("{err}");
    assert!(
        msg.contains("breakpoints") && msg.contains("maximum"),
        "expected breakpoint-cap error message, got: {msg}"
    );
}

/// The count_tokens body is assembled from an allowlist that includes
/// `messages`, so a forwarded system turn rides to
/// `/v1/messages/count_tokens` too -- both endpoints accept or reject the
/// shape together.
#[tokio::test]
async fn forwarded_system_turns_survive_the_count_tokens_allowlist() {
    // Arrange
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"input_tokens": 7})))
        .mount(&mock_server)
        .await;
    let provider = make_provider(&mock_server.uri());
    let mut req = base_req(
        "claude-opus-4-8",
        vec![user_msg("first"), system_msg("note"), assistant_msg("ok")],
    );
    req.system = Some(SystemContent::Text("you are a helpful assistant".into()));

    // Act
    let count = provider.count_tokens(req).await.unwrap();

    // Assert
    assert_eq!(count.input_tokens, 7);
    let received = mock_server.received_requests().await.unwrap();
    let captured: Value = serde_json::from_slice(&received[0].body).unwrap();
    let msgs = captured["messages"].as_array().unwrap();
    assert_eq!(msgs[1]["role"], "system", "got: {captured}");
    assert_eq!(msgs[1]["content"], "note", "got: {captured}");
}
