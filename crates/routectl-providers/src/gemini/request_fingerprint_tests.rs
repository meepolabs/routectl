// The Claude Code billing/attribution block on the Gemini egress: withheld
// from BOTH system surfaces (`req.system` and `Role::System` messages) on the
// serialized wire body, legitimate system content kept in its existing order,
// and the withhold counted once per request as a policy action.
//
// Every test here drives a strip site, so every one carries
// `#[serial_test::serial(gemini_client_fingerprint_stripped)]`: the counter is
// process-global and the runner is threaded, and a guard name no sibling
// shares excludes nothing.
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

/// Single-source pin for the `Role::System` message strip site: the fingerprint
/// rides ONLY a system-role message, beside a clean top-level system, so the
/// canonical-system site is live but withholds nothing.
#[test]
#[serial_test::serial(gemini_client_fingerprint_stripped)]
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

/// Single-source pin for the top-level `system` strip site: the fingerprint
/// rides ONLY `req.system`, beside a clean system-role message.
#[test]
#[serial_test::serial(gemini_client_fingerprint_stripped)]
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
#[serial_test::serial(gemini_client_fingerprint_stripped)]
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
#[serial_test::serial(gemini_client_fingerprint_stripped)]
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
#[serial_test::serial(gemini_client_fingerprint_stripped)]
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
#[serial_test::serial(gemini_client_fingerprint_stripped)]
fn clean_system_content_on_both_sources_is_kept_and_not_counted() {
    // Arrange
    let req = ChatRequest {
        model: "gemini-2.5-pro".into(),
        system: Some(SystemContent::Text("canonical prompt".into())),
        messages: vec![make_system("message prompt"), make_user("hi")].into(),
        ..Default::default()
    };

    // Act
    let (body, delta) = wire_and_strip_delta(&req);

    // Assert
    assert_eq!(
        system_instruction_texts(&body),
        vec!["message prompt".to_string(), "canonical prompt".to_string()],
    );
    assert_eq!(delta, 0, "nothing was withheld, so nothing is counted");
}

/// The predicate is leading-position only: a prompt that merely MENTIONS the
/// header name mid-text is legitimate content and ships.
#[test]
#[serial_test::serial(gemini_client_fingerprint_stripped)]
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
    let (body, delta) = wire_and_strip_delta(&req);

    // Assert
    assert_eq!(
        system_instruction_texts(&body),
        vec![mention.to_string(), mention.to_string()],
    );
    assert_eq!(delta, 0);
}

/// Each withhold site reports itself at WARN, without echoing the withheld text.
#[test]
#[serial_test::serial(gemini_client_fingerprint_stripped)]
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
    let warns: Vec<_> = events
        .iter()
        .filter(|e| e.level == tracing::Level::WARN && e.message.contains("billing/attribution"))
        .collect();
    assert_eq!(warns.len(), 2, "one WARN per stripping site: {events:?}");
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
#[serial_test::serial(gemini_client_fingerprint_stripped)]
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

/// Single-source pin for the metadata strip site: no system content at all,
/// so neither system site can set the tally.
#[test]
#[serial_test::serial(gemini_client_fingerprint_stripped)]
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
#[serial_test::serial(gemini_client_fingerprint_stripped)]
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
#[serial_test::serial(gemini_client_fingerprint_stripped)]
fn operator_only_metadata_reaches_the_wire_and_is_not_counted() {
    // Arrange
    let mut req = req_with_ingress_metadata();
    req.provider_extras = Some(json!({"metadata": {"trace": "operator-set"}}));
    req.routectl_internal.operator_payload_extras = Some(std::sync::Arc::new(
        json!({"metadata": {"trace": "operator-set"}}),
    ));

    // Act
    let (body, delta) = provider_body_and_strip_delta(&req);

    // Assert
    assert_eq!(body["metadata"], json!({"trace": "operator-set"}));
    assert_eq!(delta, 0, "nothing client-sourced was withheld");
}

#[test]
#[serial_test::serial(gemini_client_fingerprint_stripped)]
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
#[serial_test::serial(gemini_client_fingerprint_stripped)]
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
