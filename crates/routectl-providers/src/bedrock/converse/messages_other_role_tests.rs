// `Role::Other` on the Converse egress: Converse only models `user` and
// `assistant`, so an unrecognized role forwards as `user` with one DEBUG
// naming the dropped tag. Imports live in the host `messages_tests.rs` --
// do not add `use` lines here.

fn other_role_turn(tag: &str, text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::Other(tag.to_string()),
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn run_translate(messages: &[Message]) -> (Result<Vec<ConverseMessage>>, Vec<CapturedEvent>) {
    let mut tally = CitationsDropTally::new(TEST_ID);
    let mut reasoning = ReasoningSkipTally::new(TEST_ID);
    let mut content_drops = ContentDropTally::default();
    let mut out = None;
    let events = capture_events(|| {
        out = Some(translate_messages(
            TEST_ID,
            messages,
            &mut tally,
            &mut reasoning,
            &mut content_drops,
            &mut ClientFingerprintStripTally::default(),
        ));
    });
    (out.expect("closure always runs"), events)
}

/// An unrecognized role forwards its content as a `user` turn and emits
/// exactly one DEBUG naming the original tag.
#[test]
fn other_role_forwards_as_user_with_debug() {
    // Arrange
    let messages = vec![other_role_turn("narrator", "hello there")];

    // Act
    let (result, events) = run_translate(&messages);

    // Assert
    let translated = result.expect("translation must succeed");
    assert_eq!(translated.len(), 1, "the turn must survive translation");
    assert_eq!(
        translated[0].role, "user",
        "must forward as the closest legal role"
    );
    let debug_events: Vec<_> = events
        .iter()
        .filter(|e| e.level == tracing::Level::DEBUG && e.field("role") == Some("narrator"))
        .collect();
    assert_eq!(
        debug_events.len(),
        1,
        "exactly one DEBUG must name the dropped role tag, got: {events:?}"
    );
}

/// Sibling positive control: a recognized `Role::User` turn takes the
/// ordinary path and emits no such DEBUG, proving the assertion above
/// actually exercises the `Role::Other` arm rather than firing regardless
/// of role.
#[test]
fn known_user_role_emits_no_unrecognized_role_debug() {
    // Arrange
    let messages = vec![user_turn(vec![text_part("hello there")])];

    // Act
    let (result, events) = run_translate(&messages);

    // Assert
    let translated = result.expect("translation must succeed");
    assert_eq!(translated.len(), 1);
    assert_eq!(translated[0].role, "user");
    assert!(
        !events
            .iter()
            .any(|e| e.message.contains("unrecognized message role")),
        "a recognized role must not trip the unrecognized-role fallback, got: {events:?}"
    );
}

// A `developer` turn carries system authority, so the billing/attribution
// block is withheld from it before the coercion to `user` and counted on the
// lane's per-request policy action. Driven through the whole-request
// translation, where the tally is flushed.

const DEVELOPER_FINGERPRINT: &str = "x-anthropic-billing-header: cc_version=9.9.9; cch=cvfp4";
const DEVELOPER_FINGERPRINT_TELL: &str = "cvfp4";

fn converse_fingerprint_strip_count() -> u64 {
    crate::translation_drop_metrics::translation_policy_action_snapshot()
        .into_iter()
        .find(|e| e.lane == "bedrock-converse" && e.policy_class == "client_fingerprint_stripped")
        .map_or(0, |e| e.action_count)
}

fn developer_test_cfg() -> crate::bedrock::BedrockConfig {
    crate::bedrock::BedrockConfig {
        id: "bedrock:test-converse".into(),
        region: "us-west-2".into(),
        model_id: "anthropic.claude-sonnet-4-5".into(),
        api_shape: crate::bedrock::BedrockApiShape::Converse,
        creds: crate::bedrock::BedrockCreds::BearerKey { key: "test".into() },
        user_agent: None,
        header_extras: Vec::new(),
        anthropic_beta: Vec::new(),
        additional_model_request_fields: None,
    }
}

/// The serialized `messages` of the whole-request translation of `turn`
/// between an assistant reply and a closing user turn, plus the policy-action
/// delta it caused.
fn converse_wire_with_turn(turn: Message) -> (Value, u64) {
    let req = routectl_core::ChatRequest {
        model: "anthropic.claude-sonnet-4-5".into(),
        messages: vec![
            user_turn(vec![text_part("hello")]),
            Message {
                role: Role::Assistant,
                ..user_turn(vec![text_part("ok")])
            },
            turn,
            user_turn(vec![text_part("again")]),
        ]
        .into(),
        ..Default::default()
    };
    let before = converse_fingerprint_strip_count();
    let out = super::super::request::translate(&developer_test_cfg(), &req)
        .expect("the request must translate");
    let after = converse_fingerprint_strip_count();
    let wire = serde_json::to_value(&out).expect("the request serializes");
    (wire["messages"].clone(), after - before)
}

fn other_role_parts(tag: &str, texts: &[&str]) -> Message {
    Message {
        content: MessageContent::Parts(texts.iter().map(|t| text_part(t)).collect()),
        ..other_role_turn(tag, "")
    }
}

/// SINGLE-SOURCE pin for the developer-turn strip site: the fingerprint rides
/// only a `developer` turn, which ships as `user` with its legitimate part.
#[test]
fn a_fingerprint_only_in_a_developer_turn_is_withheld_from_converse_and_counted() {
    // Arrange
    let turn = other_role_parts("developer", &[DEVELOPER_FINGERPRINT, "dev-legit-sentinel"]);

    // Act
    let (messages, delta) = converse_wire_with_turn(turn);

    // Assert
    assert!(
        !messages.to_string().contains(DEVELOPER_FINGERPRINT_TELL),
        "the fingerprint in a developer turn reached the wire: {messages}"
    );
    assert_eq!(
        messages,
        json!([
            {"role": "user", "content": [{"text": "hello"}]},
            {"role": "assistant", "content": [{"text": "ok"}]},
            {"role": "user", "content": [{"text": "dev-legit-sentinel"}, {"text": "again"}]},
        ])
    );
    assert_eq!(delta, 1, "the withhold is one policy action");
}

/// The rest of the `Other`-role contract on this lane: a developer turn that
/// is wholly the fingerprint leaves the wire and still counts; a developer
/// turn without it, and any non-authority tag even when it carries the text,
/// ship as `user` unchanged and count nothing.
#[test]
fn other_role_turns_are_screened_on_converse_only_when_they_carry_system_authority() {
    let rows: [(&str, Message, Value, u64); 3] = [
        (
            "developer text that is wholly the fingerprint leaves the wire",
            other_role_turn("developer", DEVELOPER_FINGERPRINT),
            json!([{"text": "again"}]),
            1,
        ),
        (
            "developer without the fingerprint ships unchanged",
            other_role_turn("developer", "dev plain"),
            json!([{"text": "dev plain"}, {"text": "again"}]),
            0,
        ),
        (
            "a non-authority tag carrying the text ships as a user turn unchanged",
            other_role_turn("moderator", DEVELOPER_FINGERPRINT),
            json!([{"text": DEVELOPER_FINGERPRINT}, {"text": "again"}]),
            0,
        ),
    ];
    for (name, turn, expected_last_user, expected_delta) in rows {
        // Act
        let (messages, delta) = converse_wire_with_turn(turn);

        // Assert
        assert_eq!(
            messages.as_array().map(Vec::len),
            Some(3),
            "{name}: {messages}"
        );
        assert_eq!(messages[2]["role"], json!("user"), "{name}");
        assert_eq!(messages[2]["content"], expected_last_user, "{name}");
        assert_eq!(delta, expected_delta, "{name}");
    }
}
