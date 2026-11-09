// The Claude Code billing/attribution block on the Gemini egress: withheld
// from BOTH system surfaces (`req.system` and `Role::System` messages) on the
// serialized wire body, legitimate system content kept in its existing order,
// and the withhold counted once per request as a policy action.
//
// `include!`d into the `tests` module of `request.rs`; imports live there.

const FINGERPRINT: &str = "x-anthropic-billing-header: cc_version=9.9.9; cch=f1ng3r";
const FINGERPRINT_TELL: &str = "f1ng3r";

/// The `(gemini, client_fingerprint_stripped)` policy-action counter.
fn gemini_fingerprint_strip_count() -> u64 {
    crate::translation_drop_metrics::translation_policy_action_snapshot()
        .into_iter()
        .find(|e| e.lane == "gemini" && e.policy_class == "client_fingerprint_stripped")
        .map_or(0, |e| e.action_count)
}

fn system_block(text: &str) -> SystemBlock {
    SystemBlock {
        kind: "text".into(),
        text: text.into(),
        cache_control: None,
        citations: None,
    }
}

fn system_with_parts(texts: &[&str]) -> Message {
    Message {
        role: Role::System,
        content: MessageContent::Parts(
            texts
                .iter()
                .map(|t| {
                    ContentPart::Known(KnownContentPart::Text {
                        text: (*t).into(),
                        citations: None,
                        cache_control: None,
                    })
                })
                .collect(),
        ),
        refusal: None,
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// The `systemInstruction.parts[].text` values of a serialized body, in order.
fn system_instruction_texts(body: &Value) -> Vec<String> {
    body.get("systemInstruction")
        .and_then(|si| si.get("parts"))
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Translate and count in one step, so every test reads the delta the same way.
fn wire_and_strip_delta(req: &ChatRequest) -> (Value, u64) {
    let before = gemini_fingerprint_strip_count();
    let body = wire_body(req);
    let after = gemini_fingerprint_strip_count();
    (body, after - before)
}

/// The wire body for `req`, asserting the lane ran and withheld nothing.
fn wire_without_strip(req: &ChatRequest) -> Value {
    crate::translation_drop_metrics::assert_lane_ran_uncounted(
        LANE,
        "client_fingerprint_stripped",
        || wire_body(req),
    )
}

/// Single-source pin for the `Role::System` message strip site: the fingerprint
/// rides ONLY a system-role message, beside a clean top-level system, so the
/// canonical-system site is live but withholds nothing.
#[test]
fn a_fingerprint_only_in_a_system_role_message_is_withheld_from_gemini_and_counted() {
    // Arrange
    let req = ChatRequest {
        model: "gemini-2.5-pro".into(),
        system: Some(SystemContent::Text("canonical prompt".into())),
        messages: vec![
            make_system(FINGERPRINT),
            make_system("message prompt"),
            make_user("hi"),
        ]
        .into(),
        ..Default::default()
    };

    // Act
    let (body, delta) = wire_and_strip_delta(&req);

    // Assert
    let rendered = rendered(&body);
    assert!(
        !rendered.contains(FINGERPRINT_TELL),
        "the client fingerprint must not reach Gemini: {rendered}"
    );
    assert_eq!(
        system_instruction_texts(&body),
        vec!["message prompt".to_string(), "canonical prompt".to_string()],
        "both legitimate system texts survive in their existing order"
    );
    assert_eq!(
        delta, 1,
        "the message-surface withhold is one policy action"
    );
}

/// The serialized `contents` of `turn` between an assistant reply and a
/// closing user turn, plus the policy-action delta it caused.
fn contents_with_turn(turn: Message) -> (Value, u64) {
    let req = ChatRequest {
        model: "gemini-2.5-pro".into(),
        messages: vec![
            make_user("hi"),
            make_assistant("ok"),
            turn,
            make_user("again"),
        ]
        .into(),
        ..Default::default()
    };
    let (body, delta) = wire_and_strip_delta(&req);
    (body["contents"].clone(), delta)
}

fn other_role_with_parts(tag: &str, texts: &[&str]) -> Message {
    Message {
        role: Role::Other(tag.into()),
        ..system_with_parts(texts)
    }
}

fn other_role_text(tag: &str, text: &str) -> Message {
    Message {
        role: Role::Other(tag.into()),
        ..make_system(text)
    }
}

/// Single-source pin for the developer-turn strip site: the fingerprint rides
/// ONLY a `developer` turn, which ships as `user` with its legitimate part.
#[test]
fn a_fingerprint_only_in_a_developer_turn_is_withheld_from_gemini_and_counted() {
    // Arrange
    let turn = other_role_with_parts("developer", &[FINGERPRINT, "dev-legit-sentinel"]);

    // Act
    let (contents, delta) = contents_with_turn(turn);

    // Assert
    assert!(
        !contents.to_string().contains(FINGERPRINT_TELL),
        "the fingerprint in a developer turn reached Gemini: {contents}"
    );
    assert_eq!(contents.as_array().map(Vec::len), Some(4), "{contents}");
    assert_eq!(contents[2]["role"], json!("user"));
    assert_eq!(
        contents[2]["parts"],
        json!([{"text": "dev-legit-sentinel"}])
    );
    assert_eq!(delta, 1, "the developer-turn withhold is one policy action");
}

/// The rest of the `Other`-role contract on this lane: a developer turn that
/// is wholly the fingerprint leaves the wire and still counts; a developer
/// turn without it, and any non-authority tag even when it carries the text,
/// ship as `user` unchanged and count nothing.
#[test]
fn other_role_turns_are_screened_on_gemini_only_when_they_carry_system_authority() {
    let rows: [(&str, Message, Option<Value>, u64); 3] = [
        (
            "developer text that is wholly the fingerprint leaves the wire",
            other_role_text("developer", FINGERPRINT),
            None,
            1,
        ),
        (
            "developer without the fingerprint ships unchanged",
            other_role_text("developer", "dev plain"),
            Some(json!([{"text": "dev plain"}])),
            0,
        ),
        (
            "a non-authority tag carrying the text ships as a user turn unchanged",
            other_role_text("moderator", FINGERPRINT),
            Some(json!([{"text": FINGERPRINT}])),
            0,
        ),
    ];
    for (name, turn, expected_parts, expected_delta) in rows {
        // Act
        let (contents, delta) = contents_with_turn(turn);

        // Assert
        let roles: Vec<&Value> = contents
            .as_array()
            .expect("contents array")
            .iter()
            .map(|c| &c["role"])
            .collect();
        match expected_parts {
            Some(parts) => {
                assert_eq!(roles.len(), 4, "{name}: {contents}");
                assert_eq!(contents[2]["role"], json!("user"), "{name}");
                assert_eq!(contents[2]["parts"], parts, "{name}");
            }
            None => assert_eq!(
                roles,
                vec![&json!("user"), &json!("model"), &json!("user")],
                "{name}: {contents}"
            ),
        }
        assert_eq!(delta, expected_delta, "{name}");
    }
}

/// Single-source pin for the top-level `system` strip site: the fingerprint
/// rides ONLY `req.system`, beside a clean system-role message.
#[test]
fn a_fingerprint_only_in_the_top_level_system_is_withheld_from_gemini_and_counted() {
    // Arrange
    let req = ChatRequest {
        model: "gemini-2.5-pro".into(),
        system: Some(SystemContent::Blocks(vec![
            system_block(FINGERPRINT),
            system_block("canonical prompt"),
        ])),
        messages: vec![make_system("message prompt"), make_user("hi")].into(),
        ..Default::default()
    };

    // Act
    let (body, delta) = wire_and_strip_delta(&req);

    // Assert
    let rendered = rendered(&body);
    assert!(
        !rendered.contains(FINGERPRINT_TELL),
        "the client fingerprint must not reach Gemini: {rendered}"
    );
    assert_eq!(
        system_instruction_texts(&body),
        vec!["message prompt".to_string(), "canonical prompt".to_string()],
        "both legitimate system texts survive in their existing order"
    );
    assert_eq!(delta, 1, "the top-level withhold is one policy action");
}

/// A fingerprint on both surfaces is still ONE action for the request, and the
/// surviving parts keep the order the egress has always emitted: system-role
/// messages first, then the top-level system.
#[test]
fn a_fingerprint_on_both_system_sources_is_withheld_and_counted_once() {
    // Arrange
    let req = ChatRequest {
        model: "gemini-2.5-pro".into(),
        system: Some(SystemContent::Blocks(vec![
            system_block("canonical one"),
            system_block(FINGERPRINT),
            system_block("canonical two"),
        ])),
        messages: vec![
            make_system("message one"),
            make_system(FINGERPRINT),
            make_user("hi"),
            make_system("message two"),
        ]
        .into(),
        ..Default::default()
    };

    // Act
    let (body, delta) = wire_and_strip_delta(&req);

    // Assert
    assert!(!rendered(&body).contains(FINGERPRINT_TELL));
    assert_eq!(
        system_instruction_texts(&body),
        vec![
            "message one".to_string(),
            "message two".to_string(),
            "canonical one".to_string(),
            "canonical two".to_string(),
        ],
    );
    assert_eq!(
        delta, 1,
        "two stripping surfaces on one request are one action"
    );
}

/// The part-level strip: a system-role message carrying the fingerprint as one
/// text part among others keeps its legitimate parts.
#[test]
fn a_fingerprint_part_inside_a_multi_part_system_message_is_withheld_alone() {
    // Arrange
    let req = ChatRequest {
        model: "gemini-2.5-pro".into(),
        messages: vec![
            system_with_parts(&["first part", FINGERPRINT, "last part"]),
            make_user("hi"),
        ]
        .into(),
        ..Default::default()
    };

    // Act
    let (body, delta) = wire_and_strip_delta(&req);

    // Assert
    assert!(!rendered(&body).contains(FINGERPRINT_TELL));
    assert_eq!(
        system_instruction_texts(&body),
        vec!["first part".to_string(), "last part".to_string()],
    );
    assert_eq!(delta, 1);
}

/// A request whose ONLY system content is the fingerprint ships no
/// `systemInstruction` at all, and is still counted: the flush sits outside the
/// assembly, so collapsing to nothing cannot skip it.
#[test]
fn an_all_fingerprint_system_omits_the_system_instruction_and_still_counts() {
    // Arrange
    let req = ChatRequest {
        model: "gemini-2.5-pro".into(),
        system: Some(SystemContent::Text(FINGERPRINT.into())),
        messages: vec![make_system(FINGERPRINT), make_user("hi")].into(),
        ..Default::default()
    };

    // Act
    let (body, delta) = wire_and_strip_delta(&req);

    // Assert
    assert!(
        body.get("systemInstruction").is_none(),
        "nothing legitimate survived, so no systemInstruction ships: {body}"
    );
    assert!(!rendered(&body).contains(FINGERPRINT_TELL));
    assert_eq!(delta, 1);
}

/// Positive control on the counter AND the filter: clean system content on
/// both surfaces is emitted whole and records nothing, so the deltas above
/// cannot pass on a counter that bumps for every request or a filter that
/// strips everything.
#[test]
fn clean_system_content_on_both_sources_is_kept_and_not_counted() {
    // Arrange
    let req = ChatRequest {
        model: "gemini-2.5-pro".into(),
        system: Some(SystemContent::Text("canonical prompt".into())),
        messages: vec![make_system("message prompt"), make_user("hi")].into(),
        ..Default::default()
    };

    // Act
    let body = wire_without_strip(&req);

    // Assert
    assert_eq!(
        system_instruction_texts(&body),
        vec!["message prompt".to_string(), "canonical prompt".to_string()],
    );
}

/// The predicate is leading-position only: a prompt that merely MENTIONS the
/// header name mid-text is legitimate content and ships.
#[test]
fn a_mid_text_mention_of_the_header_name_is_not_withheld() {
    // Arrange
    let mention = "explain what x-anthropic-billing-header: means";
    let req = ChatRequest {
        model: "gemini-2.5-pro".into(),
        system: Some(SystemContent::Text(mention.into())),
        messages: vec![make_system(mention), make_user("hi")].into(),
        ..Default::default()
    };

    // Act
    let body = wire_without_strip(&req);

    // Assert
    assert_eq!(
        system_instruction_texts(&body),
        vec![mention.to_string(), mention.to_string()],
    );
}

/// Each withhold site reports itself once at DEBUG, without echoing the withheld text.
#[test]
fn each_withhold_site_warns_without_echoing_the_fingerprint() {
    // Arrange
    let req = ChatRequest {
        model: "gemini-2.5-pro".into(),
        system: Some(SystemContent::Text(FINGERPRINT.into())),
        messages: vec![make_system(FINGERPRINT), make_user("hi")].into(),
        ..Default::default()
    };

    // Act
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| body = wire_body(&req));

    // Assert
    let levels: Vec<_> = events
        .iter()
        .filter(|e| e.message.contains("billing/attribution"))
        .map(|e| e.level)
        .collect();
    assert_eq!(
        levels,
        vec![tracing::Level::DEBUG; 2],
        "one DEBUG per stripping site: {events:?}"
    );
    assert!(
        events
            .iter()
            .all(|e| !format!("{e:?}").contains(FINGERPRINT_TELL)),
        "no event may echo the withheld fingerprint: {events:?}"
    );
    assert!(!rendered(&body).contains(FINGERPRINT_TELL));
}

/// The strip is attempt-local: translating does not mutate the caller's
/// request, so a fallback attempt onto another lane receives the original
/// system content and applies that lane's own policy.
#[test]
fn translating_leaves_the_callers_request_unmodified() {
    // Arrange
    let req = ChatRequest {
        model: "gemini-2.5-pro".into(),
        system: Some(SystemContent::Text(FINGERPRINT.into())),
        messages: vec![make_system(FINGERPRINT), make_user("hi")].into(),
        ..Default::default()
    };
    let before = serde_json::to_value(&req).expect("serialize");

    // Act
    let first = wire_body(&req);
    let second = wire_body(&req);

    // Assert
    assert_eq!(serde_json::to_value(&req).expect("serialize"), before);
    assert_eq!(first, second, "a retried attempt emits the same body");
}

// ---------------------------------------------------------------------------
// Top-level `metadata` from the ingress sweep
// ---------------------------------------------------------------------------

const METADATA_TELL: &str = "meta-fp-9k2";

/// A request whose ingress sweep carried the Anthropic `metadata` block.
fn req_with_ingress_metadata() -> ChatRequest {
    ChatRequest {
        model: "gemini-2.5-pro".into(),
        messages: vec![make_user("hi")].into(),
        provider_extras: Some(json!({
            "metadata": {"user_id": METADATA_TELL, "account_uuid": METADATA_TELL},
            "safetySettings": []
        })),
        ..Default::default()
    }
}

/// The full provider body pipeline, counted.
fn provider_body_and_strip_delta(req: &ChatRequest) -> (Value, u64) {
    let before = gemini_fingerprint_strip_count();
    let body = provider_body("gemini:test", req).expect("provider body builds");
    let after = gemini_fingerprint_strip_count();
    (body, after - before)
}

/// The full provider body for `req`, asserting the lane ran and withheld
/// nothing.
fn provider_body_without_strip(req: &ChatRequest) -> Value {
    crate::translation_drop_metrics::assert_lane_ran_uncounted(
        LANE,
        "client_fingerprint_stripped",
        || provider_body("gemini:test", req).expect("provider body builds"),
    )
}

/// Single-source pin for the metadata strip site: no system content at all,
/// so neither system site can set the tally.
#[test]
fn ingress_metadata_is_withheld_from_the_gemini_body_and_counted() {
    // Arrange
    let req = req_with_ingress_metadata();

    // Act
    let (body, delta) = provider_body_and_strip_delta(&req);

    // Assert
    let rendered = rendered(&body);
    assert!(body.get("metadata").is_none(), "{rendered}");
    assert!(!rendered.contains(METADATA_TELL), "{rendered}");
    assert_eq!(
        body["safetySettings"],
        json!([]),
        "other extras still merge"
    );
    assert_eq!(delta, 1, "the metadata withhold is one policy action");
}

#[test]
fn operator_metadata_survives_while_the_ingress_contribution_is_withheld() {
    // Arrange: the dispatch layer deep-merged the operator's metadata over
    // the ingress one, so `provider_extras` holds both sets of keys.
    let mut req = req_with_ingress_metadata();
    req.provider_extras = Some(json!({
        "metadata": {"user_id": METADATA_TELL, "trace": "operator-set"}
    }));
    req.routectl_internal.operator_payload_extras = Some(std::sync::Arc::new(
        json!({"metadata": {"trace": "operator-set"}}),
    ));

    // Act
    let (body, delta) = provider_body_and_strip_delta(&req);

    // Assert
    assert_eq!(body["metadata"], json!({"trace": "operator-set"}));
    assert!(!rendered(&body).contains(METADATA_TELL));
    assert_eq!(delta, 1, "the ingress contribution was withheld");
}

#[test]
fn operator_only_metadata_reaches_the_wire_and_is_not_counted() {
    // Arrange
    let mut req = req_with_ingress_metadata();
    req.provider_extras = Some(json!({"metadata": {"trace": "operator-set"}}));
    req.routectl_internal.operator_payload_extras = Some(std::sync::Arc::new(
        json!({"metadata": {"trace": "operator-set"}}),
    ));

    // Act
    let body = provider_body_without_strip(&req);

    // Assert
    assert_eq!(body["metadata"], json!({"trace": "operator-set"}));
}

#[test]
fn metadata_and_both_system_fingerprints_count_one_policy_action() {
    // Arrange
    let mut req = req_with_ingress_metadata();
    req.system = Some(SystemContent::Text(FINGERPRINT.into()));
    req.messages = vec![make_system(FINGERPRINT), make_user("hi")].into();

    // Act
    let (body, delta) = provider_body_and_strip_delta(&req);

    // Assert
    let rendered = rendered(&body);
    assert!(!rendered.contains(FINGERPRINT_TELL), "{rendered}");
    assert!(!rendered.contains(METADATA_TELL), "{rendered}");
    assert_eq!(delta, 1, "three withhold sites, one request, one action");
}

#[test]
fn the_metadata_withhold_never_logs_metadata_values() {
    // Arrange
    let req = req_with_ingress_metadata();

    // Act
    let mut body = Value::Null;
    let events = routectl_testkit::capture_events(|| {
        body = provider_body("gemini:test", &req).expect("provider body builds");
    });

    // Assert
    assert!(
        events.iter().any(|e| e.message.contains("metadata")),
        "the withhold is reported: {events:?}"
    );
    assert!(
        events
            .iter()
            .all(|e| !format!("{e:?}").contains(METADATA_TELL)),
        "no event may echo a metadata value: {events:?}"
    );
    assert!(body.get("metadata").is_none());
}
