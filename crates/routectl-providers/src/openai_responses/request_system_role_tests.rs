// System content reaching the Responses `instructions` string from BOTH
// canonical surfaces: the filtered top-level `system` first, then surviving
// `Role::System` message text in message order. The Claude Code
// billing/attribution block is withheld from either surface and counted once
// per request.
//
// `include!`d into `request_tests.rs`; all top-level imports live there.

const RESPONSES_FINGERPRINT: &str = "x-anthropic-billing-header: cc_version=9.9.9; cch=f1ng3r";
const RESPONSES_FINGERPRINT_TELL: &str = "f1ng3r";

fn system_text(text: &str) -> Message {
    Message {
        refusal: None,
        role: Role::System,
        content: MessageContent::Text(text.into()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn system_parts(texts: &[&str]) -> Message {
    Message {
        refusal: None,
        role: Role::System,
        content: MessageContent::Parts(texts.iter().map(|t| text_part(t)).collect()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

fn plain_system_block(text: &str) -> SystemBlock {
    SystemBlock {
        kind: "text".into(),
        text: text.into(),
        cache_control: None,
        citations: None,
    }
}

fn responses_fingerprint_strip_count() -> u64 {
    crate::translation_drop_metrics::translation_policy_action_snapshot()
        .into_iter()
        .find(|e| e.lane == "openai-responses" && e.policy_class == "client_fingerprint_stripped")
        .map_or(0, |e| e.action_count)
}

fn wire_with_strip_delta(req: &ChatRequest) -> (Value, u64) {
    let before = responses_fingerprint_strip_count();
    let v = translate_to_json(&cfg(), req);
    let after = responses_fingerprint_strip_count();
    (v, after - before)
}

/// A direct library caller putting its system prompt in the messages array
/// must not lose it: with no top-level `system`, the Role::System text is the
/// whole `instructions` string.
#[test]
fn a_system_role_message_reaches_instructions_when_no_top_level_system_exists() {
    // Arrange
    let req = req_with(vec![system_text("be helpful"), user_text("hi")]);

    // Act
    let (v, delta) = wire_with_strip_delta(&req);

    // Assert
    assert_eq!(v["instructions"], json!("be helpful"));
    assert_eq!(delta, 0, "nothing was withheld");
    let rendered = v["input"].to_string();
    assert!(
        !rendered.contains("be helpful"),
        "system text rides instructions, never a duplicate input item: {rendered}"
    );
}

/// Both surfaces present: the top-level system comes first, then every
/// surviving system-role message in message order -- including one that sits
/// after a user turn -- joined by the lane's block separator.
#[test]
fn top_level_system_precedes_system_role_messages_in_message_order() {
    // Arrange
    let mut req = req_with(vec![
        system_text("message one"),
        user_text("hi"),
        system_parts(&["message two a", "message two b"]),
    ]);
    req.system = Some(SystemContent::Blocks(vec![
        plain_system_block("canonical one"),
        plain_system_block("canonical two"),
    ]));

    // Act
    let (v, delta) = wire_with_strip_delta(&req);

    // Assert
    assert_eq!(
        v["instructions"],
        json!("canonical one\n\ncanonical two\n\nmessage one\n\nmessage two a\nmessage two b"),
    );
    assert_eq!(delta, 0);
}

/// Positive control on `req.system`: with no Role::System messages, the
/// top-level handling is byte-identical to before.
#[test]
fn a_top_level_system_alone_is_unchanged() {
    // Arrange
    let mut req = req_with(vec![user_text("hi")]);
    req.system = Some(SystemContent::Text("only canonical".into()));

    // Act
    let (v, delta) = wire_with_strip_delta(&req);

    // Assert
    assert_eq!(v["instructions"], json!("only canonical"));
    assert_eq!(delta, 0);
}

/// A blank top-level system never suppresses legitimate message-system text.
#[test]
fn a_blank_top_level_system_does_not_suppress_system_role_messages() {
    // Arrange
    let mut req = req_with(vec![system_text("from messages"), user_text("hi")]);
    req.system = Some(SystemContent::Text("   ".into()));

    // Act
    let (v, _) = wire_with_strip_delta(&req);

    // Assert
    assert_eq!(v["instructions"], json!("from messages"));
}

/// Single-source pin for the Role::System strip site: the fingerprint rides
/// ONLY a system-role message, beside a clean top-level system.
#[test]
fn a_fingerprint_only_in_a_system_role_message_is_withheld_and_counted() {
    // Arrange
    let mut req = req_with(vec![
        system_text(RESPONSES_FINGERPRINT),
        system_text("message prompt"),
        user_text("hi"),
    ]);
    req.system = Some(SystemContent::Text("canonical prompt".into()));

    // Act
    let (v, delta) = wire_with_strip_delta(&req);

    // Assert
    let rendered = v.to_string();
    assert!(
        !rendered.contains(RESPONSES_FINGERPRINT_TELL),
        "the client fingerprint must not reach the wire: {rendered}"
    );
    assert_eq!(
        v["instructions"],
        json!("canonical prompt\n\nmessage prompt")
    );
    assert_eq!(
        delta, 1,
        "the message-surface withhold is one policy action"
    );
}

/// A fingerprint on both surfaces is one action for the request; both
/// legitimate sources survive in order.
#[test]
fn a_fingerprint_on_both_system_sources_is_withheld_and_counted_once() {
    // Arrange
    let mut req = req_with(vec![
        system_parts(&[RESPONSES_FINGERPRINT, "message prompt"]),
        user_text("hi"),
    ]);
    req.system = Some(SystemContent::Blocks(vec![
        plain_system_block(RESPONSES_FINGERPRINT),
        plain_system_block("canonical prompt"),
    ]));

    // Act
    let (v, delta) = wire_with_strip_delta(&req);

    // Assert
    assert!(!v.to_string().contains(RESPONSES_FINGERPRINT_TELL));
    assert_eq!(
        v["instructions"],
        json!("canonical prompt\n\nmessage prompt")
    );
    assert_eq!(
        delta, 1,
        "two stripping surfaces on one request are one action"
    );
}

/// A request whose top-level system IS the fingerprint still delivers its
/// legitimate message-system content: an empty top-level result never
/// suppresses the other source.
#[test]
fn an_all_fingerprint_top_level_system_still_delivers_message_system_text() {
    // Arrange
    let mut req = req_with(vec![system_text("message prompt"), user_text("hi")]);
    req.system = Some(SystemContent::Text(RESPONSES_FINGERPRINT.into()));

    // Act
    let (v, delta) = wire_with_strip_delta(&req);

    // Assert
    assert_eq!(v["instructions"], json!("message prompt"));
    assert_eq!(delta, 1);
}

/// Everything withheld: `instructions` is the empty string the lane always
/// serializes, and the request still counts.
#[test]
fn an_all_fingerprint_system_on_both_sources_empties_instructions_and_counts() {
    // Arrange
    let mut req = req_with(vec![system_text(RESPONSES_FINGERPRINT), user_text("hi")]);
    req.system = Some(SystemContent::Text(RESPONSES_FINGERPRINT.into()));

    // Act
    let (v, delta) = wire_with_strip_delta(&req);

    // Assert
    assert_eq!(v["instructions"], json!(""));
    assert!(!v.to_string().contains(RESPONSES_FINGERPRINT_TELL));
    assert_eq!(delta, 1);
}

/// A request that withholds a fingerprint and THEN fails translation still
/// counts: the flush sits outside the fallible input build.
#[test]
fn a_request_that_fails_after_the_message_strip_still_counts() {
    // Arrange -- an image_url that is not an object is a hard error in the
    // input build, which runs after the system surfaces are assembled.
    let req = req_with(vec![
        system_text(RESPONSES_FINGERPRINT),
        user_parts(vec![ContentPart::Known(KnownContentPart::ImageUrl {
            image_url: json!("not-an-object"),
            cache_control: None,
        })]),
    ]);

    // Act
    let before = responses_fingerprint_strip_count();
    let err = translate_err(&cfg(), &req);
    let after = responses_fingerprint_strip_count();

    // Assert
    assert!(err.contains("image_url"), "unexpected error: {err}");
    assert_eq!(
        after - before,
        1,
        "the withhold happened before the failure"
    );
}

fn system_with_image(text: &str) -> Message {
    Message {
        content: MessageContent::Parts(vec![
            text_part(text),
            ContentPart::Known(KnownContentPart::ImageUrl {
                image_url: json!({"url": "https://example.com/a.png"}),
                cache_control: None,
            }),
        ]),
        ..system_text(text)
    }
}

/// A system turn carrying a non-text part has no place in `instructions`, so
/// it rides in place as a `system` message item with its image, and its text
/// is not copied into `instructions` as well.
#[test]
fn a_system_turn_with_an_image_rides_in_place_as_a_system_item() {
    let mut req = req_with(vec![
        user_text("hi"),
        system_with_image("match this"),
        user_text("again"),
    ]);
    req.system = Some(SystemContent::Text("be brief".into()));

    let wire = translate_to_json(&cfg(), &req);

    assert_eq!(wire["instructions"], json!("be brief"));
    let input = wire["input"].as_array().expect("input array");
    assert_eq!(input.len(), 3, "{wire}");
    assert_eq!(input[1]["role"], json!("system"));
    assert_eq!(
        input[1]["content"],
        json!([
            {"type": "input_text", "text": "match this"},
            {"type": "input_image", "image_url": "https://example.com/a.png"}
        ])
    );
}

/// The fingerprint part of a system turn kept in place is withheld from the
/// item it rides in and counted once.
#[test]
fn a_fingerprint_part_in_a_kept_system_turn_is_withheld_and_counted_once() {
    let mut turn = system_with_image("match this");
    if let MessageContent::Parts(parts) = &mut turn.content {
        parts.insert(0, text_part(RESPONSES_FINGERPRINT));
    }
    let req = req_with(vec![user_text("hi"), turn]);

    let (wire, delta) = wire_with_strip_delta(&req);

    assert!(
        !wire.to_string().contains(RESPONSES_FINGERPRINT_TELL),
        "{wire}"
    );
    assert_eq!(wire["input"][1]["content"][0]["text"], json!("match this"));
    assert_eq!(delta, 1);
}
