// System content on the openai-compat wire body from BOTH canonical
// surfaces: the filtered top-level `system` lowered to a leading
// `role: "system"` message, and every `Role::System` message kept in place
// with the Claude Code billing/attribution block withheld. The withhold is
// counted once per request whichever surface carried it.
//
// Every test here drives a strip site, so every one carries
// `#[serial_test::serial(openai_compat_client_fingerprint_stripped)]`, the
// guard the counter tests in the enclosing module hold: the registry is
// process-global and the runner is threaded.
//
// `include!`d into the `tests` module of `request.rs`; imports live there.

const COMPAT_FINGERPRINT: &str = "x-anthropic-billing-header: cc_version=9.9.9; cch=c0mpatfp";
const COMPAT_FINGERPRINT_TELL: &str = "c0mpatfp";

fn system_text_message(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::System,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn system_parts_message(texts: &[&str]) -> Message {
    Message {
        refusal: None,
        role: Role::System,
        content: MessageContent::Parts(
            texts
                .iter()
                .map(|text| {
                    ContentPart::Known(KnownContentPart::Text {
                        text: (*text).into(),
                        citations: None,
                        cache_control: None,
                    })
                })
                .collect(),
        ),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn user_text_message(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::User,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn plain_block(text: &str) -> routectl_core::SystemBlock {
    routectl_core::SystemBlock {
        kind: "text".into(),
        text: text.into(),
        cache_control: None,
        citations: None,
    }
}

fn req_with(system: Option<routectl_core::SystemContent>, messages: Vec<Message>) -> ChatRequest {
    ChatRequest {
        model: "gpt-4o".into(),
        system,
        messages: messages.into(),
        ..Default::default()
    }
}

/// The lenient wire body, rendered, plus the policy-action delta it caused.
fn wire_and_strip_delta(req: &ChatRequest) -> (serde_json::Value, String, u64) {
    let before = fingerprint_strip_count();
    let body = lenient_normalize(req);
    let after = fingerprint_strip_count();
    let rendered = serde_json::to_string(&body).expect("body renders");
    (body, rendered, after - before)
}

/// `(role, content)` of every wire message, in wire order.
fn roles_and_contents(body: &serde_json::Value) -> Vec<(String, serde_json::Value)> {
    body["messages"]
        .as_array()
        .expect("messages array")
        .iter()
        .map(|m| {
            (
                m["role"].as_str().expect("role").to_string(),
                m["content"].clone(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Single-source cases: each isolates ONE strip site, so deleting that site's
// strip or record reds exactly the case named on its marker.
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(openai_compat_client_fingerprint_stripped)]
fn a_fingerprint_only_in_a_system_role_message_is_withheld_from_openai_compat_and_counted() {
    // Arrange -- no top-level system at all: the Anthropic-ingress shape that
    // leaves `role: "system"` turns in `messages`.
    let req = req_with(
        None,
        vec![
            system_text_message(COMPAT_FINGERPRINT),
            system_text_message("message-legit-sentinel"),
            user_text_message("hi"),
        ],
    );

    // Act
    let (body, rendered, delta) = wire_and_strip_delta(&req);

    // Assert
    assert!(
        !rendered.contains(COMPAT_FINGERPRINT_TELL),
        "the fingerprint in a system-role message reached the wire: {rendered}"
    );
    assert_eq!(
        roles_and_contents(&body),
        vec![
            ("system".to_string(), json!("message-legit-sentinel")),
            ("user".to_string(), json!("hi")),
        ]
    );
    assert_eq!(delta, 1, "the withhold is one policy action");
}

#[test]
#[serial_test::serial(openai_compat_client_fingerprint_stripped)]
fn a_fingerprint_only_in_the_top_level_system_is_withheld_from_openai_compat_and_counted() {
    // Arrange
    let req = req_with(
        Some(routectl_core::SystemContent::Blocks(vec![
            plain_block(COMPAT_FINGERPRINT),
            plain_block("top-legit-sentinel"),
        ])),
        vec![user_text_message("hi")],
    );

    // Act
    let (body, rendered, delta) = wire_and_strip_delta(&req);

    // Assert
    assert!(
        !rendered.contains(COMPAT_FINGERPRINT_TELL),
        "the fingerprint in the top-level system reached the wire: {rendered}"
    );
    assert_eq!(
        roles_and_contents(&body),
        vec![
            ("system".to_string(), json!("top-legit-sentinel")),
            ("user".to_string(), json!("hi")),
        ]
    );
    assert_eq!(delta, 1, "the withhold is one policy action");
}

// ---------------------------------------------------------------------------
// The parts surface.
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(openai_compat_client_fingerprint_stripped)]
fn a_fingerprint_part_is_withheld_while_its_sibling_parts_keep_their_order() {
    // Arrange -- the fingerprint FOLLOWS legitimate text in the same message,
    // so a predicate run over the joined text would miss it.
    let req = req_with(
        None,
        vec![
            system_parts_message(&["part-one", COMPAT_FINGERPRINT, "part-two"]),
            user_text_message("hi"),
        ],
    );

    // Act
    let (body, rendered, delta) = wire_and_strip_delta(&req);

    // Assert
    assert!(
        !rendered.contains(COMPAT_FINGERPRINT_TELL),
        "a fingerprint part reached the wire: {rendered}"
    );
    assert_eq!(
        body["messages"][0],
        json!({
            "role": "system",
            "content": [
                {"type": "text", "text": "part-one"},
                {"type": "text", "text": "part-two"}
            ]
        })
    );
    assert_eq!(delta, 1);
}

#[test]
#[serial_test::serial(openai_compat_client_fingerprint_stripped)]
fn a_system_message_whose_only_part_is_the_fingerprint_leaves_the_wire_and_still_counts() {
    // Arrange
    let req = req_with(
        None,
        vec![
            system_parts_message(&[COMPAT_FINGERPRINT]),
            user_text_message("hi"),
        ],
    );

    // Act
    let (body, rendered, delta) = wire_and_strip_delta(&req);

    // Assert -- no empty-content system message is left behind.
    assert!(!rendered.contains(COMPAT_FINGERPRINT_TELL), "{rendered}");
    assert_eq!(
        roles_and_contents(&body),
        vec![("user".to_string(), json!("hi"))]
    );
    assert_eq!(delta, 1);
}

#[test]
#[serial_test::serial(openai_compat_client_fingerprint_stripped)]
fn a_blank_top_level_system_still_strips_the_fingerprint_from_system_role_messages() {
    // Arrange -- blank reads as absent, so it lowers nothing and must not
    // suppress the message-site strip either.
    let req = req_with(
        Some(routectl_core::SystemContent::Text("   ".into())),
        vec![
            system_text_message(COMPAT_FINGERPRINT),
            system_text_message("message-legit-sentinel"),
            user_text_message("hi"),
        ],
    );

    // Act
    let (body, rendered, delta) = wire_and_strip_delta(&req);

    // Assert
    assert!(!rendered.contains(COMPAT_FINGERPRINT_TELL), "{rendered}");
    assert_eq!(
        roles_and_contents(&body),
        vec![
            ("system".to_string(), json!("message-legit-sentinel")),
            ("user".to_string(), json!("hi")),
        ]
    );
    assert_eq!(delta, 1);
}

// ---------------------------------------------------------------------------
// Both sources: count once, deterministic order.
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(openai_compat_client_fingerprint_stripped)]
fn a_fingerprint_on_both_surfaces_counts_one_policy_action() {
    // Arrange
    let req = req_with(
        Some(routectl_core::SystemContent::Blocks(vec![
            plain_block(COMPAT_FINGERPRINT),
            plain_block("top-legit-sentinel"),
        ])),
        vec![
            system_text_message(COMPAT_FINGERPRINT),
            system_text_message("message-legit-sentinel"),
            user_text_message("hi"),
        ],
    );

    // Act
    let (body, rendered, delta) = wire_and_strip_delta(&req);

    // Assert -- the lowered top-level system first, then the surviving
    // message-system turn where the caller put it.
    assert!(!rendered.contains(COMPAT_FINGERPRINT_TELL), "{rendered}");
    assert_eq!(
        roles_and_contents(&body),
        vec![
            ("system".to_string(), json!("top-legit-sentinel")),
            ("system".to_string(), json!("message-legit-sentinel")),
            ("user".to_string(), json!("hi")),
        ]
    );
    assert_eq!(
        delta, 1,
        "one request withholding on two surfaces is one policy action"
    );
}

// ---------------------------------------------------------------------------
// Positive controls: legitimate system content survives, in order.
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(openai_compat_client_fingerprint_stripped)]
fn distinct_system_role_messages_survive_beside_a_top_level_system_in_caller_order() {
    // Arrange -- a direct caller supplying both surfaces, one system turn
    // leading and one mid-conversation.
    let req = req_with(
        Some(routectl_core::SystemContent::Text(
            "top-level prompt".into(),
        )),
        vec![
            system_text_message("leading system turn"),
            user_text_message("hi"),
            system_text_message("mid-conversation system turn"),
            user_text_message("again"),
        ],
    );

    // Act
    let (body, _, delta) = wire_and_strip_delta(&req);

    // Assert
    assert_eq!(
        roles_and_contents(&body),
        vec![
            ("system".to_string(), json!("top-level prompt")),
            ("system".to_string(), json!("leading system turn")),
            ("user".to_string(), json!("hi")),
            ("system".to_string(), json!("mid-conversation system turn")),
            ("user".to_string(), json!("again")),
        ]
    );
    assert_eq!(delta, 0, "nothing was withheld, so nothing is counted");
}

#[test]
#[serial_test::serial(openai_compat_client_fingerprint_stripped)]
fn a_system_role_message_repeating_the_top_level_text_is_kept_not_deduplicated() {
    // Arrange -- equal content is not provenance; the caller sent both.
    let req = req_with(
        Some(routectl_core::SystemContent::Text("same prompt".into())),
        vec![system_text_message("same prompt"), user_text_message("hi")],
    );

    // Act
    let (body, _, _) = wire_and_strip_delta(&req);

    // Assert
    assert_eq!(
        roles_and_contents(&body),
        vec![
            ("system".to_string(), json!("same prompt")),
            ("system".to_string(), json!("same prompt")),
            ("user".to_string(), json!("hi")),
        ]
    );
}

#[test]
#[serial_test::serial(openai_compat_client_fingerprint_stripped)]
fn a_mid_string_billing_prefix_in_a_system_role_message_is_kept_and_not_counted() {
    // Arrange
    let text = "intro x-anthropic-billing-header: not at the start";
    let req = req_with(
        None,
        vec![system_text_message(text), user_text_message("hi")],
    );

    // Act
    let (body, _, delta) = wire_and_strip_delta(&req);

    // Assert
    assert_eq!(body["messages"][0]["content"], json!(text));
    assert_eq!(delta, 0);
}

// ---------------------------------------------------------------------------
// The flush sits outside the fallible assembly.
// ---------------------------------------------------------------------------

/// The serial key list carries the document-drop key too: the document block
/// that makes strict mode fail bumps that counter as a side effect.
#[test]
#[serial_test::serial(
    openai_compat_client_fingerprint_stripped,
    openai_compat_document_block_unrepresentable
)]
fn a_request_that_withheld_the_fingerprint_and_then_failed_strict_translation_still_counts() {
    // Arrange
    let mut req = req_dropped_document_with_system_fingerprint();
    req.system = None;

    // Act
    let before = fingerprint_strip_count();
    let result = normalize(
        "test",
        &req,
        ReasoningDialect::Passthrough,
        HistoryReasoning::Auto,
        None,
        true,
    );
    let after = fingerprint_strip_count();

    // Assert
    assert!(
        result.is_err(),
        "strict mode must reject the document block"
    );
    assert_eq!(
        after - before,
        1,
        "the withhold happened before the failure, so it counts"
    );
}

/// A strict-mode failure that happens during the wire lift, AFTER the system
/// projection: the fingerprint rides a system-role message and a user turn
/// carries a document block the content lift rejects.
fn req_dropped_document_with_system_fingerprint() -> ChatRequest {
    let mut req = req_with(None, vec![system_text_message(COMPAT_FINGERPRINT)]);
    push_msg(
        &mut req,
        Message {
            refusal: None,
            role: Role::User,
            content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Document {
                source: json!({
                    "type": "base64",
                    "media_type": "application/pdf",
                    "data": "AA=="
                }),
                title: None,
                citations: None,
                cache_control: None,
            })]),
            reasoning: None,
            reasoning_details: vec![],
            name: None,
            tool_call_id: None,
            tool_calls: None,
        },
    );
    req
}
