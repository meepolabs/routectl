// Members a client sent that this hub does not model, carried on canonical
// messages and parts, and the top-level extras outside the allowlist: what
// each auth surface emits, withholds, declares and counts.
//
// `include!`d into `request_tests.rs`; all top-level imports live there.

const CARRIER_CLASS: &str = "unknown_member_withheld";

fn members(pairs: &[(&str, Value)]) -> serde_json::Map<String, Value> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

fn carried_text_part(text: &str, extras: serde_json::Map<String, Value>) -> ContentPart {
    ContentPart::Known(KnownContentPart::Text {
        text: text.into(),
        citations: None,
        cache_control: None,
        extras,
    })
}

fn with_members(message: Message, extras: serde_json::Map<String, Value>) -> Message {
    Message { extras, ..message }
}

/// A user message item with an `id` and one `input_text` part carrying a
/// member, as the Responses ingress builds it.
fn carried_user_turn() -> Message {
    with_members(
        user_parts(vec![carried_text_part(
            "hi",
            members(&[("annotations", json!(["n\u{e9}"]))]),
        )]),
        members(&[("id", json!("msg_user_1"))]),
    )
}

fn responses_request(messages: Vec<Message>) -> ChatRequest {
    let mut req = req_with(messages);
    req.routectl_internal.provenance = routectl_core::RequestProvenance::ResponsesIngress;
    req
}

/// The api-key and ChatGPT-OAuth surfaces emit a message's members on its
/// item and a part's members on its block; Bedrock Mantle emits neither.
#[test]
fn message_and_part_members_ride_the_first_party_surfaces_only() {
    let rows = [
        ("api-key", cfg_api_key(), true),
        ("chatgpt-oauth", cfg(), true),
        ("bedrock-mantle", cfg_bedrock_mantle(), false),
    ];
    for (surface, cfg, emits) in rows {
        // Arrange
        let req = responses_request(vec![carried_user_turn()]);

        // Act
        let wire = translate_to_json(&cfg, &req);

        // Assert
        let item = &wire["input"][0];
        let block = &item["content"][0];
        assert_eq!(block["text"], json!("hi"), "{surface}: {wire}");
        if emits {
            assert_eq!(item["id"], json!("msg_user_1"), "{surface}: {wire}");
            assert_eq!(
                block["annotations"],
                json!(["n\u{e9}"]),
                "{surface}: {wire}"
            );
        } else {
            assert!(item.get("id").is_none(), "{surface}: {wire}");
            assert!(block.get("annotations").is_none(), "{surface}: {wire}");
        }
    }
}

/// A request from another ingress dialect carries members spelled for that
/// dialect's wire, so none of them reaches this one.
#[test]
fn members_from_another_dialect_never_reach_the_responses_wire() {
    // Arrange
    let mut req = responses_request(vec![carried_user_turn()]);
    req.routectl_internal.provenance = routectl_core::RequestProvenance::OpenaiIngress;

    // Act
    let wire = translate_to_json(&cfg_api_key(), &req);

    // Assert
    assert_eq!(
        wire["input"][0]["content"][0]["text"],
        json!("hi"),
        "{wire}"
    );
    assert!(wire["input"][0].get("id").is_none(), "{wire}");
    assert!(
        wire["input"][0]["content"][0].get("annotations").is_none(),
        "{wire}"
    );
}

/// A carried member never replaces or recreates a member the wire node
/// writes itself, present or not: on the message item, on an `input_text`,
/// `output_text` and `input_image` block, and on a `function_call_output`.
#[test]
fn a_carried_member_naming_a_typed_member_never_reaches_the_wire() {
    let image_part = ContentPart::Known(KnownContentPart::ImageUrl {
        image_url: json!({"url": "https://example.com/a.png"}),
        cache_control: None,
        extras: members(&[("detail", json!("high")), ("type", json!("input_file"))]),
    });
    let user = with_members(
        user_parts(vec![
            carried_text_part(
                "hi",
                members(&[
                    ("type", json!("input_file")),
                    ("prompt_cache_breakpoint", json!({"mode": "explicit"})),
                    ("keep", json!(1)),
                ]),
            ),
            image_part,
        ]),
        members(&[("type", json!("reasoning")), ("id", json!("m1"))]),
    );
    let assistant = assistant_parts(vec![carried_text_part(
        "ok",
        members(&[("type", json!("input_text")), ("keep", json!(2))]),
    )]);
    let tool = with_members(
        tool_message("call_1", "42"),
        members(&[
            ("output", json!("forged")),
            ("call_id", json!("x")),
            ("id", json!("fco_1")),
        ]),
    );
    let req = responses_request(vec![user, assistant, tool]);

    // Act
    let wire = translate_to_json(&cfg_api_key(), &req);

    // Assert
    let input = &wire["input"];
    assert_eq!(input[0]["type"], json!("message"), "{wire}");
    assert_eq!(input[0]["id"], json!("m1"), "{wire}");
    assert_eq!(
        input[0]["content"][0],
        json!({"type": "input_text", "text": "hi", "keep": 1}),
        "{wire}"
    );
    assert_eq!(
        input[0]["content"][1],
        json!({"type": "input_image", "image_url": "https://example.com/a.png"}),
        "a carried `detail` must not recreate the typed member the part left absent: {wire}"
    );
    assert_eq!(
        input[1]["content"][0],
        json!({"type": "output_text", "text": "ok", "keep": 2}),
        "{wire}"
    );
    assert_eq!(
        input[2],
        json!({"type": "function_call_output", "call_id": "call_1", "output": "42", "id": "fco_1"}),
        "{wire}"
    );
}

/// A `prompt_cache_breakpoint` in a part's carrier never ships: the
/// breakpoint record alone places one, on the surface that takes it.
#[test]
fn only_the_breakpoint_record_places_a_prompt_cache_breakpoint() {
    // Arrange
    let carried = members(&[("prompt_cache_breakpoint", json!({"mode": "carried"}))]);
    let mut req = responses_request(vec![user_parts(vec![carried_text_part("hi", carried)])]);
    let recorded = recorded_breakpoint(Some(0), "hi", "/input/0/content/0");
    req.routectl_internal.responses_cache_breakpoints = vec![recorded];
    let no_record = responses_request(req.messages.to_vec());

    // Act
    let placed = translate_to_json(&cfg_api_key(), &req);
    let unplaced = translate_to_json(&cfg_api_key(), &no_record);

    // Assert
    assert_eq!(
        placed["input"][0]["content"][0]["prompt_cache_breakpoint"],
        breakpoint_value(),
        "{placed}"
    );
    assert!(
        unplaced["input"][0]["content"][0]
            .get("prompt_cache_breakpoint")
            .is_none(),
        "{unplaced}"
    );
}

/// A kept system turn rides `input[]` in place with its members, under the
/// role the client wrote: `developer` when the ingress recorded it as a
/// developer item, else `system`. Its text is not copied into
/// `instructions`.
#[test]
fn a_kept_system_turn_rides_in_place_with_its_members_and_its_inbound_role() {
    let kept = |id: &str, text: &str| {
        with_members(
            Message {
                role: Role::System,
                content: MessageContent::Text(text.into()),
                ..user_text("")
            },
            members(&[("id", json!(id))]),
        )
    };
    let rows = [
        ("developer item", vec![1], "developer"),
        ("system item", vec![], "system"),
    ];
    for (row, developer_turns, role) in rows {
        // Arrange
        let mut req = responses_request(vec![
            user_text("hi"),
            kept("msg_dev_1", "be brief"),
            user_text("again"),
        ]);
        req.routectl_internal.responses_developer_turns = developer_turns;

        // Act
        let wire = translate_to_json(&cfg(), &req);

        // Assert
        assert_eq!(
            wire["input"][1],
            json!({"type": "message", "role": role, "id": "msg_dev_1",
                   "content": [{"type": "input_text", "text": "be brief"}]}),
            "{row}: {wire}"
        );
        assert_eq!(wire["instructions"], json!(""), "{row}: {wire}");
    }
}

/// With many kept system turns, exactly the recorded ones go out as
/// `developer`, each at its own position.
#[test]
fn many_kept_system_turns_take_the_developer_role_exactly_where_recorded() {
    // Arrange
    const TURNS: usize = 64;
    let kept = |index: usize| {
        with_members(
            Message {
                role: Role::System,
                content: MessageContent::Text(format!("rule {index}")),
                ..user_text("")
            },
            members(&[("id", json!(format!("msg_{index}")))]),
        )
    };
    let mut req = responses_request((0..TURNS).map(kept).collect());
    let recorded: Vec<usize> = (0..TURNS).filter(|index| index % 3 != 1).collect();
    req.routectl_internal.responses_developer_turns = recorded.clone();

    // Act
    let wire = translate_to_json(&cfg(), &req);

    // Assert
    let roles: Vec<&str> = (0..TURNS)
        .map(|index| wire["input"][index]["role"].as_str().expect("role"))
        .collect();
    let expected: Vec<&str> = (0..TURNS)
        .map(|index| {
            if recorded.contains(&index) {
                "developer"
            } else {
                "system"
            }
        })
        .collect();
    assert_eq!(roles, expected, "{wire}");
}

/// The developer-turn record names a kept system turn only: a user turn at
/// that index keeps its own role.
#[test]
fn the_developer_turn_record_never_rewrites_a_non_system_turn() {
    // Arrange
    let mut req = responses_request(vec![user_text("hi")]);
    req.routectl_internal.responses_developer_turns = vec![0];

    // Act
    let wire = translate_to_json(&cfg(), &req);

    // Assert
    assert_eq!(wire["input"][0]["role"], json!("user"), "{wire}");
}

/// The api-key surface forwards a top-level extras key outside the
/// allowlist; one naming a field routectl writes is withheld, declared and
/// counted instead of overriding it.
#[test]
fn api_key_forwards_unlisted_top_level_members_but_never_a_managed_one() {
    use routectl_core::wire_check::{EditKind, EditSite};
    // Arrange
    let mut req = responses_request(vec![user_text("hi")]);
    req.provider_extras = Some(json!({
        "safety_identifier": "user-7",
        "model": "forged",
        "instructions": "forged",
        "max_tokens": 9,
    }));
    let before = responses_drop_count(CARRIER_CLASS);

    // Act
    let (wire, edits) = translate_declaring(&cfg_api_key(), &req);

    // Assert
    assert_eq!(responses_drop_count(CARRIER_CLASS) - before, 1, "{wire}");
    assert_eq!(wire["safety_identifier"], json!("user-7"), "{wire}");
    assert_eq!(wire["model"], json!("gpt-5"), "{wire}");
    assert_eq!(wire["instructions"], json!(""), "{wire}");
    assert!(wire.get("max_tokens").is_none(), "{wire}");
    for key in ["/model", "/instructions", "/max_tokens"] {
        let strip = (
            EditSite::ManagedExtrasStrip,
            key.to_owned(),
            EditKind::Remove,
        );
        assert!(edits.contains(&strip), "{key}: {edits:?}");
    }
    assert!(
        !edits
            .iter()
            .any(|(_, path, _)| path == "/safety_identifier"),
        "a forwarded member is no edit: {edits:?}"
    );
}

/// The ChatGPT-OAuth and Bedrock Mantle surfaces keep the allowlist: an
/// unlisted top-level key is absent, declared as an auth-surface Remove, and
/// the request counts once however many it withheld, alongside one
/// `lane_seen`.
#[test]
fn top_level_members_outside_the_forwarded_set_are_withheld_declared_and_counted_once() {
    use routectl_core::wire_check::{EditKind, EditSite};
    let rows = [
        (
            "chatgpt-oauth one key",
            cfg(),
            json!({"safety_identifier": "u"}),
        ),
        (
            "chatgpt-oauth two keys",
            cfg(),
            json!({"safety_identifier": "u", "truncation": "auto"}),
        ),
        (
            "bedrock-mantle",
            cfg_bedrock_mantle(),
            json!({"safety_identifier": "u"}),
        ),
    ];
    for (row, cfg, extras) in rows {
        // Arrange
        let mut req = responses_request(vec![carried_user_turn()]);
        req.provider_extras = Some(extras.clone());
        let drops_before = responses_drop_count(CARRIER_CLASS);
        let seen_before = responses_lane_seen_count();

        // Act
        let (wire, edits) = translate_declaring(&cfg, &req);

        // Assert
        assert_eq!(
            responses_drop_count(CARRIER_CLASS) - drops_before,
            1,
            "{row}"
        );
        assert_eq!(responses_lane_seen_count() - seen_before, 1, "{row}");
        for key in extras.as_object().expect("object").keys() {
            assert!(wire.get(key).is_none(), "{row}: {wire}");
            let gate = (
                EditSite::AuthSurfaceGate,
                format!("/{key}"),
                EditKind::Remove,
            );
            assert!(edits.contains(&gate), "{row} {key}: {edits:?}");
        }
    }
}

/// Positive control for the row above: a request whose extras are all on
/// the allowlist, or that carries members only on its messages, counts
/// nothing on ChatGPT-OAuth.
#[test]
fn allowlisted_extras_and_message_members_count_no_withhold() {
    // Arrange
    let mut req = responses_request(vec![carried_user_turn()]);
    req.provider_extras = Some(json!({"service_tier": "flex", "prompt_cache_key": "k"}));

    // Act
    let wire = crate::translation_drop_metrics::assert_lane_ran_uncounted(
        "openai-responses",
        CARRIER_CLASS,
        || translate_to_json(&cfg(), &req),
    );

    // Assert
    assert_eq!(wire["service_tier"], json!("flex"), "{wire}");
    assert_eq!(wire["input"][0]["id"], json!("msg_user_1"), "{wire}");
}

/// A top-level key from another ingress dialect stays off every surface
/// and is not this class's to count.
#[test]
fn a_top_level_key_from_another_dialect_is_neither_forwarded_nor_counted_here() {
    // Arrange
    let mut req = req_with(vec![user_text("hi")]);
    req.routectl_internal.provenance = routectl_core::RequestProvenance::AnthropicIngress;
    req.provider_extras = Some(json!({"context_management": {"edits": []}}));

    // Act
    let wire = crate::translation_drop_metrics::assert_lane_ran_uncounted(
        "openai-responses",
        CARRIER_CLASS,
        || translate_to_json(&cfg_api_key(), &req),
    );

    // Assert
    assert!(wire.get("context_management").is_none(), "{wire}");
}
