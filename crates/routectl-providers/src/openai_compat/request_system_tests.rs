// System content on the openai-compat wire body from BOTH canonical
// surfaces: the filtered top-level `system` lowered to a leading
// `role: "system"` message, and every `Role::System` message kept in place
// with the Claude Code billing/attribution block withheld. The withhold is
// counted once per request whichever surface carried it.
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

/// A system turn carrying an image rides the openai-compat wire in place
/// with every part, after the lowered top-level system.
#[test]
fn a_system_turn_with_an_image_rides_in_place_with_every_part() {
    let image = json!({"type": "image_url", "image_url": {"url": "https://example.com/a.png"}});
    let turn = Message {
        content: MessageContent::Parts(vec![
            ContentPart::Known(KnownContentPart::Text {
                text: "match this".into(),
                citations: None,
                cache_control: None,
            }),
            serde_json::from_value(image.clone()).expect("image part"),
        ]),
        ..system_text_message("unused")
    };
    let req = req_with(
        Some(routectl_core::SystemContent::Text("be brief".into())),
        vec![user_text_message("hi"), turn],
    );

    let (body, _, _) = wire_and_strip_delta(&req);

    assert_eq!(
        roles_and_contents(&body),
        vec![
            ("system".to_string(), json!("be brief")),
            ("user".to_string(), json!("hi")),
            (
                "system".to_string(),
                json!([{"type": "text", "text": "match this"}, image])
            ),
        ]
    );
}

// ---------------------------------------------------------------------------
// `developer` turns: the Anthropic ingress leaves them as `Role::Other`, and
// this wire forwards the tag, where it carries system authority.
// ---------------------------------------------------------------------------

fn other_role_message(tag: &str, texts: &[&str]) -> Message {
    Message {
        role: Role::Other(tag.into()),
        ..system_parts_message(texts)
    }
}

#[test]
fn a_fingerprint_only_in_a_developer_turn_is_withheld_from_openai_compat_and_counted() {
    // Arrange
    let req = req_with(
        None,
        vec![
            user_text_message("hi"),
            other_role_message("developer", &[COMPAT_FINGERPRINT, "dev-legit-sentinel"]),
            user_text_message("again"),
        ],
    );

    // Act
    let (body, rendered, delta) = wire_and_strip_delta(&req);

    // Assert
    assert!(
        !rendered.contains(COMPAT_FINGERPRINT_TELL),
        "the fingerprint in a developer turn reached the wire: {rendered}"
    );
    assert_eq!(
        roles_and_contents(&body),
        vec![
            ("user".to_string(), json!("hi")),
            (
                "developer".to_string(),
                json!([{"type": "text", "text": "dev-legit-sentinel"}])
            ),
            ("user".to_string(), json!("again")),
        ]
    );
    assert_eq!(delta, 1, "the withhold is one policy action");
}

/// A row of the `Other`-role table: name, the turn, the wire messages a
/// withholding row expects (`None`: the turn rides unchanged), and the
/// policy-action delta.
type OtherRoleRow = (
    &'static str,
    Message,
    Option<Vec<(String, serde_json::Value)>>,
    u64,
);

/// Each row puts one `Other`-role turn between two user turns. A row with no
/// withhold expects the turn on the wire exactly as the canonical message
/// serializes; a withholding row expects the listed wire messages.
#[test]
fn other_role_turns_are_screened_only_when_they_carry_system_authority() {
    let fingerprint_only = Message {
        content: MessageContent::Text(COMPAT_FINGERPRINT.into()),
        ..other_role_message("developer", &[])
    };
    let rows: Vec<OtherRoleRow> = vec![
        (
            "developer text that is wholly the fingerprint leaves the wire",
            fingerprint_only,
            Some(vec![
                ("user".to_string(), json!("hi")),
                ("user".to_string(), json!("again")),
            ]),
            1,
        ),
        (
            "developer without the fingerprint passes through unchanged",
            other_role_message("developer", &["dev one", "dev two"]),
            None,
            0,
        ),
        (
            "a mid-string billing prefix in a developer turn is kept",
            other_role_message(
                "developer",
                &["note x-anthropic-billing-header: not leading"],
            ),
            None,
            0,
        ),
        (
            "a non-authority tag is not screened",
            other_role_message("moderator", &[COMPAT_FINGERPRINT, "mod text"]),
            None,
            0,
        ),
    ];
    for (name, turn, expected, expected_delta) in rows {
        // Arrange
        let canonical = serde_json::to_value(&turn).expect("turn serializes");
        let req = req_with(
            None,
            vec![user_text_message("hi"), turn, user_text_message("again")],
        );

        // Act
        let (body, _, delta) = wire_and_strip_delta(&req);

        // Assert
        match expected {
            Some(messages) => assert_eq!(roles_and_contents(&body), messages, "{name}"),
            None => {
                assert_eq!(body["messages"].as_array().map(Vec::len), Some(3), "{name}");
                assert_eq!(body["messages"][1], canonical, "{name}");
            }
        }
        assert_eq!(delta, expected_delta, "{name}");
    }
}

/// An unmodeled part whose `type` tag routectl does not recognize but which
/// still carries a `text` field; this wire forwards such a part verbatim.
fn unmodeled_text_part(text: &str) -> ContentPart {
    let mut extras = serde_json::Map::new();
    extras.insert("text".into(), serde_json::Value::String(text.into()));
    ContentPart::Other {
        type_tag: "Text".into(),
        cache_control: None,
        extras,
    }
}

fn known_text_part(text: &str) -> ContentPart {
    ContentPart::Known(KnownContentPart::Text {
        text: text.into(),
        citations: None,
        cache_control: None,
    })
}

fn authority_turn(role: Role, parts: Vec<ContentPart>) -> Message {
    Message {
        role,
        content: MessageContent::Parts(parts),
        ..system_parts_message(&[])
    }
}

/// The block screen is per carrier, not per modeled type: the block riding an
/// unmodeled text-bearing part of a system or developer turn is withheld and
/// counted once, and the same part without the block rides unchanged.
#[test]
fn an_unmodeled_part_carrying_the_fingerprint_is_withheld_from_openai_compat() {
    let developer = || Role::Other("developer".into());
    let rows: Vec<(&str, Message, Message, u64)> = vec![
        (
            "system turn",
            authority_turn(
                Role::System,
                vec![
                    unmodeled_text_part(COMPAT_FINGERPRINT),
                    known_text_part("legit-sentinel"),
                ],
            ),
            authority_turn(Role::System, vec![known_text_part("legit-sentinel")]),
            1,
        ),
        (
            "developer turn",
            authority_turn(
                developer(),
                vec![
                    unmodeled_text_part(COMPAT_FINGERPRINT),
                    known_text_part("legit-sentinel"),
                ],
            ),
            authority_turn(developer(), vec![known_text_part("legit-sentinel")]),
            1,
        ),
        (
            "system turn without the fingerprint is unchanged",
            authority_turn(
                Role::System,
                vec![
                    unmodeled_text_part("plain"),
                    known_text_part("legit-sentinel"),
                ],
            ),
            authority_turn(
                Role::System,
                vec![
                    unmodeled_text_part("plain"),
                    known_text_part("legit-sentinel"),
                ],
            ),
            0,
        ),
        (
            "developer turn without the fingerprint is unchanged",
            authority_turn(
                developer(),
                vec![
                    unmodeled_text_part("plain"),
                    known_text_part("legit-sentinel"),
                ],
            ),
            authority_turn(
                developer(),
                vec![
                    unmodeled_text_part("plain"),
                    known_text_part("legit-sentinel"),
                ],
            ),
            0,
        ),
    ];
    for (name, turn, expected_turn, expected_delta) in rows {
        // Arrange
        let expected = serde_json::to_value(&expected_turn).expect("turn serializes");
        let req = req_with(
            None,
            vec![user_text_message("hi"), turn, user_text_message("again")],
        );

        // Act
        let (body, rendered, delta) = wire_and_strip_delta(&req);

        // Assert
        assert!(
            !rendered.contains(COMPAT_FINGERPRINT_TELL),
            "{name}: {rendered}"
        );
        assert_eq!(body["messages"].as_array().map(Vec::len), Some(3), "{name}");
        assert_eq!(body["messages"][1], expected, "{name}");
        assert_eq!(delta, expected_delta, "{name}");
    }
}
