// The deliberate translation drops this egress makes while building a
// request, one three-assertion set per counted drop class:
//
//   1. the arm's WARN/DEBUG fired, captured structurally via
//      `routectl_testkit::capture_events` (assertions read `CapturedEvent`
//      fields, never a substring of rendered output);
//   2. the dropped content is ABSENT FROM THE EMITTED WIRE VALUE -- the
//      serialized request body, not merely "a warning fired". A warn-plus-
//      counter reporting a removal the wire never performed is the overclaim
//      this bar exists to catch;
//   3. a positive control: a similar-but-representable sibling shape SURVIVES
//      in that same emitted value, so assertion 2 cannot pass by the whole
//      turn having vanished.
//
// Plus a counter delta per class, read through the public snapshot.
//
// `include!`d into `request_tests.rs`; all top-level imports live there, so
// do not add `use` lines here.
//
// SERIAL GUARDS: the drop registry is process-global and the runner is
// threaded, so every test below that reads a counter delta carries the
// `openai_responses_<class>` guard for its class -- and so does every OTHER
// test in this crate that constructs a request reaching the same arm, even
// incidentally. A guard name no sibling shares excludes nothing.

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

/// The `(openai-responses, class)` counter's current value, read through the
/// public snapshot. Zero when the class has never fired in this process.
fn responses_drop_count(class: &str) -> u64 {
    crate::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == "openai-responses" && e.drop_class == class)
        .map_or(0, |e| e.drop_count)
}

/// Translate under log capture, returning the emitted wire value alongside
/// every event the translation produced.
fn translate_capturing(
    cfg: &OpenAiResponsesConfig,
    req: &ChatRequest,
) -> (Value, Vec<CapturedEvent>) {
    let mut wire = Value::Null;
    let events = routectl_testkit::capture_events(|| {
        wire = translate_to_json(cfg, req);
    });
    (wire, events)
}

/// Whether any captured event's `message` names the given drop record. The
/// message text is the operator-facing grep target for these records, so it
/// is what a log assertion pins; the structured fields are asserted
/// separately by the tests that care about a specific field's value.
fn any_event_message_contains(events: &[CapturedEvent], needle: &str) -> bool {
    events.iter().any(|e| e.message.contains(needle))
}

fn responses_detail(kind: ReasoningDetailKind, format: &str, payload: Value) -> ReasoningDetail {
    ReasoningDetail {
        kind,
        id: Some("rs_1".into()),
        format: Some(format.into()),
        index: None,
        payload,
    }
}

/// An assistant turn carrying `details` on `reasoning_details` and nothing
/// else, preceded by a user turn so the request is well-formed.
fn turn_with_reasoning_details(details: Vec<ReasoningDetail>) -> Vec<Message> {
    vec![
        user_text("hi"),
        Message {
            refusal: None,
            role: Role::Assistant,
            content: MessageContent::Text("answer".into()),
            reasoning: None,
            reasoning_details: details,
            name: None,
            tool_call_id: None,
            tool_calls: None,
        },
    ]
}

/// An `Encrypted` detail tagged for the codex lane -- the shape that DOES
/// replay onto `cfg()`'s ChatgptOauth lane. The positive control for every
/// reasoning-drop test below.
fn replayable_detail() -> ReasoningDetail {
    responses_detail(
        ReasoningDetailKind::Encrypted,
        CODEX_OAUTH,
        json!({"encrypted_content": "REPLAYABLE_SIG"}),
    )
}

/// Every `encrypted_content` string the emitted body carries on a
/// `reasoning` input item.
fn wire_reasoning_signatures(wire: &Value) -> Vec<String> {
    wire["input"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|i| i["type"] == "reasoning")
                .filter_map(|i| i["encrypted_content"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// reasoning_format_foreign
//
// A detail whose format tag is outside the Responses family was minted by an
// upstream in another dialect. Cross-dialect only: the Responses ingress
// stamps `openai-responses-v1` on every detail it builds.
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(openai_responses_reasoning_format_foreign)]
fn foreign_format_detail_drops_from_the_wire_and_counts() {
    // Arrange: a foreign-tagged detail carrying a signature, beside a
    // codex-tagged sibling that must replay.
    const FOREIGN_SIG: &str = "FOREIGN_ONLY_SIGNATURE";
    let req = req_with(turn_with_reasoning_details(vec![
        responses_detail(
            ReasoningDetailKind::Encrypted,
            "anthropic-claude-v1",
            json!({"encrypted_content": FOREIGN_SIG}),
        ),
        replayable_detail(),
    ]));

    // Act
    let before = responses_drop_count("reasoning_format_foreign");
    let (wire, events) = translate_capturing(&cfg(), &req);
    let after = responses_drop_count("reasoning_format_foreign");

    // Assert 1: the drop record fired, naming the offending format.
    let record = events
        .iter()
        .find(|e| {
            e.message.contains(
                "openai-responses: skipped reasoning_details entries with a non-Responses-family format",
            )
        })
        .unwrap_or_else(|| panic!("the foreign-format drop must be observable, got: {events:?}"));
    assert_eq!(
        record.field("skipped"),
        Some("1"),
        "the record must count the one skipped detail, got: {record:?}"
    );
    assert!(
        record
            .field("formats")
            .is_some_and(|f| f.contains("anthropic-claude-v1")),
        "the record must name the foreign format, got: {record:?}"
    );

    // Assert 2: the foreign signature is absent from the EMITTED WIRE VALUE.
    assert!(
        !wire.to_string().contains(FOREIGN_SIG),
        "a foreign-format signature must not reach the wire: {wire}"
    );

    // Assert 3: positive control -- the codex-tagged sibling survived, so
    // assertion 2 did not pass by the whole turn vanishing.
    assert_eq!(
        wire_reasoning_signatures(&wire),
        vec!["REPLAYABLE_SIG".to_string()],
        "the same-lane sibling must still replay: {wire}"
    );

    assert_eq!(
        after - before,
        1,
        "the drop must be counted exactly once for the request"
    );
}

#[test]
#[serial_test::serial(openai_responses_reasoning_format_foreign)]
fn a_request_of_only_responses_family_details_records_no_foreign_format_drop() {
    // Arrange: the positive control as a whole request -- nothing foreign.
    let req = req_with(turn_with_reasoning_details(vec![replayable_detail()]));

    // Act
    let before = responses_drop_count("reasoning_format_foreign");
    let (wire, events) = translate_capturing(&cfg(), &req);
    let after = responses_drop_count("reasoning_format_foreign");

    // Assert: the artifact rode, no record fired, no count moved.
    assert_eq!(wire_reasoning_signatures(&wire), vec!["REPLAYABLE_SIG"]);
    assert!(
        !any_event_message_contains(&events, "non-Responses-family format"),
        "a clean request must emit no foreign-format record, got: {events:?}"
    );
    assert_eq!(after, before, "a clean request must not move the counter");
}

// ---------------------------------------------------------------------------
// reasoning_scheme_incompatible
//
// A Responses-family artifact whose replay scheme the TARGET lane is proven
// to reject. Only a proven-incompatible pair strips; an unestablished pair is
// carried optimistically by the same gate.
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(openai_responses_reasoning_scheme_incompatible)]
fn scheme_incompatible_detail_drops_from_the_wire_and_counts() {
    // Arrange: a mantle-scheme artifact aimed at the id-validating codex
    // lane (a PROVEN-incompatible pair), beside a codex-scheme sibling.
    const MANTLE_SIG: &str = "MANTLE_SCHEME_SIGNATURE";
    let req = req_with(turn_with_reasoning_details(vec![
        responses_detail(
            ReasoningDetailKind::Encrypted,
            BEDROCK_MANTLE,
            json!({"encrypted_content": MANTLE_SIG}),
        ),
        replayable_detail(),
    ]));

    // Act
    let before = responses_drop_count("reasoning_scheme_incompatible");
    let (wire, events) = translate_capturing(&cfg(), &req);
    let after = responses_drop_count("reasoning_scheme_incompatible");

    // Assert 1: the strip record fired, counting the one stripped detail.
    let record = events
        .iter()
        .find(|e| {
            e.message.contains(
                "openai-responses: stripped reasoning_details entries whose replay scheme the target lane rejects",
            )
        })
        .unwrap_or_else(|| panic!("the scheme strip must be observable, got: {events:?}"));
    assert_eq!(
        record.field("stripped"),
        Some("1"),
        "the record must count the one stripped detail, got: {record:?}"
    );

    // Assert 2: the stripped signature is absent from the emitted wire value.
    assert!(
        !wire.to_string().contains(MANTLE_SIG),
        "a scheme-rejected signature must not reach the wire: {wire}"
    );

    // Assert 3: positive control -- the same-scheme sibling rode.
    assert_eq!(
        wire_reasoning_signatures(&wire),
        vec!["REPLAYABLE_SIG".to_string()],
        "the same-scheme sibling must still replay: {wire}"
    );

    assert_eq!(
        after - before,
        1,
        "the strip must be counted exactly once for the request"
    );
}

#[test]
#[serial_test::serial(openai_responses_reasoning_scheme_incompatible)]
fn a_same_scheme_request_records_no_scheme_incompatible_drop() {
    // Arrange: a codex-scheme artifact on the codex lane -- a proven-
    // COMPATIBLE pair, which must not strip.
    let req = req_with(turn_with_reasoning_details(vec![replayable_detail()]));

    // Act
    let before = responses_drop_count("reasoning_scheme_incompatible");
    let (wire, events) = translate_capturing(&cfg(), &req);
    let after = responses_drop_count("reasoning_scheme_incompatible");

    // Assert
    assert_eq!(wire_reasoning_signatures(&wire), vec!["REPLAYABLE_SIG"]);
    assert!(
        !any_event_message_contains(&events, "replay scheme the target lane rejects"),
        "a compatible pair must emit no strip record, got: {events:?}"
    );
    assert_eq!(after, before, "a compatible pair must not move the counter");
}

// ---------------------------------------------------------------------------
// reasoning_detail_kind_unsupported
//
// An unrecognized `ReasoningDetailKind` has no slot in this egress's
// `reasoning` item -- summary / content / encrypted_content are the complete
// set. Cross-dialect only: the Responses ingress constructs only Summary,
// Encrypted, and Text, so an `Other` kind arises from `Deserialize` mapping
// an unrecognized discriminator on the canonical schema, which a Chat
// Completions client reaches.
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(openai_responses_reasoning_detail_kind_unsupported)]
fn unrecognized_detail_kind_drops_from_the_wire_and_counts() {
    // Arrange: an unrecognized-kind detail whose payload carries a marker
    // string, beside a recognized Encrypted sibling on the same item id.
    const UNSUPPORTED_PAYLOAD: &str = "PAYLOAD_OF_AN_UNSUPPORTED_KIND";
    let req = req_with(turn_with_reasoning_details(vec![
        responses_detail(
            ReasoningDetailKind::Other("reasoning.future_kind".into()),
            CODEX_OAUTH,
            json!({"text": UNSUPPORTED_PAYLOAD}),
        ),
        replayable_detail(),
    ]));

    // Act
    let before = responses_drop_count("reasoning_detail_kind_unsupported");
    let (wire, events) = translate_capturing(&cfg(), &req);
    let after = responses_drop_count("reasoning_detail_kind_unsupported");

    // Assert 1: the drop record fired, naming the unrecognized tag.
    let record = events
        .iter()
        .find(|e| {
            e.message.contains(
                "openai-responses: dropped reasoning_details entries whose kind has no Responses reasoning-item slot",
            )
        })
        .unwrap_or_else(|| {
            panic!("the unsupported-kind drop must be observable, got: {events:?}")
        });
    assert_eq!(
        record.field("dropped"),
        Some("1"),
        "the record must count the one dropped detail, got: {record:?}"
    );
    assert!(
        record
            .field("kinds")
            .is_some_and(|k| k.contains("reasoning.future_kind")),
        "the record must name the unrecognized kind tag, got: {record:?}"
    );

    // Assert 2: the unsupported payload is absent from the emitted wire value.
    assert!(
        !wire.to_string().contains(UNSUPPORTED_PAYLOAD),
        "an unsupported kind's payload must not reach the wire: {wire}"
    );

    // Assert 3: positive control -- the recognized sibling sharing that item
    // id still produced its reasoning item with the signature intact.
    assert_eq!(
        wire_reasoning_signatures(&wire),
        vec!["REPLAYABLE_SIG".to_string()],
        "the recognized sibling on the same id must still replay: {wire}"
    );

    assert_eq!(
        after - before,
        1,
        "the drop must be counted exactly once for the request"
    );
}

#[test]
#[serial_test::serial(openai_responses_reasoning_detail_kind_unsupported)]
fn a_recognized_kind_detail_records_no_unsupported_kind_drop() {
    // Arrange: only recognized kinds -- Summary and Text alongside the
    // signature, all of which have a slot.
    let req = req_with(turn_with_reasoning_details(vec![
        responses_detail(
            ReasoningDetailKind::Summary,
            CODEX_OAUTH,
            json!({"text": "a summary"}),
        ),
        responses_detail(
            ReasoningDetailKind::Text,
            CODEX_OAUTH,
            json!({"text": "the chain"}),
        ),
        replayable_detail(),
    ]));

    // Act
    let before = responses_drop_count("reasoning_detail_kind_unsupported");
    let (wire, events) = translate_capturing(&cfg(), &req);
    let after = responses_drop_count("reasoning_detail_kind_unsupported");

    // Assert: every surface rode, no record, no count.
    let item = wire["input"]
        .as_array()
        .and_then(|items| items.iter().find(|i| i["type"] == "reasoning"))
        .unwrap_or_else(|| panic!("a reasoning item must be emitted: {wire}"));
    assert_eq!(item["summary"][0]["text"], "a summary", "got: {wire}");
    assert_eq!(item["content"][0]["text"], "the chain", "got: {wire}");
    assert!(
        !any_event_message_contains(&events, "kind has no Responses reasoning-item slot"),
        "recognized kinds must emit no unsupported-kind record, got: {events:?}"
    );
    assert_eq!(after, before, "recognized kinds must not move the counter");
}

// ---------------------------------------------------------------------------
// image_source_kind_unrepresentable
//
// A canonical `Image` part whose `source.type` is neither `base64` nor `url`
// is well-formed but names a carrier this egress cannot emit. Cross-dialect
// only: the Responses ingress maps an `input_image` block to the OpenAI-shape
// `ImageUrl` carrier and mints no `KnownContentPart::Image` at all.
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(openai_responses_image_source_kind_unrepresentable)]
fn unknown_user_image_source_kind_drops_from_the_wire_and_counts() {
    // Arrange: an unrepresentable source kind naming a bucket + key, beside
    // a representable url-source sibling on the same user turn.
    let req = req_with(vec![user_parts(vec![
        image_part(json!({"type": "s3", "bucket": "private-bucket", "key": "img.png"})),
        image_part(json!({"type": "url", "url": "https://example.com/cat.jpg"})),
    ])]);

    // Act
    let before = responses_drop_count("image_source_kind_unrepresentable");
    let (wire, events) = translate_capturing(&cfg(), &req);
    let after = responses_drop_count("image_source_kind_unrepresentable");

    // Assert 1: the WARN fired at WARN level, naming the source kind and
    // the role it was dropped from.
    let record = events
        .iter()
        .find(|e| {
            e.message
                .contains("dropping image part with unknown source kind on Responses egress")
        })
        .unwrap_or_else(|| panic!("the image-source drop must be observable, got: {events:?}"));
    assert_eq!(record.level, tracing::Level::WARN, "got: {record:?}");
    assert_eq!(record.field("source_kind"), Some("s3"), "got: {record:?}");
    assert_eq!(record.field("role"), Some("user"), "got: {record:?}");

    // Assert 2: neither the unrepresentable kind tag nor the locator it
    // carried reaches the emitted wire value.
    let bytes = wire.to_string();
    assert!(
        !bytes.contains("private-bucket"),
        "the dropped source's locator must not reach the wire: {wire}"
    );
    assert!(
        !bytes.contains("\"s3\""),
        "the dropped source's kind tag must not reach the wire: {wire}"
    );

    // Assert 3: positive control -- the representable sibling shipped.
    assert_eq!(
        wire["input"][0]["content"],
        json!([{"type": "input_image", "image_url": "https://example.com/cat.jpg"}]),
        "the representable sibling must still ship: {wire}"
    );

    assert_eq!(
        after - before,
        1,
        "the drop must be counted exactly once for the request"
    );
}

#[test]
#[serial_test::serial(openai_responses_image_source_kind_unrepresentable)]
fn unknown_tool_result_image_source_kind_drops_from_the_wire_and_counts() {
    // Arrange: the same unrepresentable kind nested in a tool result, where
    // a separate translate path handles it, beside a representable sibling.
    let req = req_with(vec![
        user_text("run"),
        tool_message_parts(
            "call_1",
            vec![
                image_part(json!({"type": "gs", "bucket": "private-bucket", "key": "shot.png"})),
                image_part(json!({"type": "url", "url": "https://example.com/shot.png"})),
            ],
        ),
    ]);

    // Act
    let before = responses_drop_count("image_source_kind_unrepresentable");
    let (wire, events) = translate_capturing(&cfg(), &req);
    let after = responses_drop_count("image_source_kind_unrepresentable");

    // Assert 1: the tool-result variant of the WARN fired, tagged with the
    // tool role so the two sites stay distinguishable in a log.
    let record = events
        .iter()
        .find(|e| {
            e.message.contains(
                "dropping image part with unknown source kind in tool result on Responses egress",
            )
        })
        .unwrap_or_else(|| {
            panic!("the tool-result image-source drop must be observable, got: {events:?}")
        });
    assert_eq!(record.level, tracing::Level::WARN, "got: {record:?}");
    assert_eq!(record.field("source_kind"), Some("gs"), "got: {record:?}");
    assert_eq!(record.field("role"), Some("tool"), "got: {record:?}");

    // Assert 2: absent from the emitted wire value.
    assert!(
        !wire.to_string().contains("private-bucket"),
        "the dropped source's locator must not reach the wire: {wire}"
    );

    // Assert 3: positive control -- the representable sibling shipped inside
    // the same function_call_output body.
    assert_eq!(
        wire["input"][1]["output"],
        json!([{"type": "input_image", "image_url": "https://example.com/shot.png"}]),
        "the representable sibling must still ship: {wire}"
    );

    assert_eq!(
        after - before,
        1,
        "the drop must be counted exactly once for the request"
    );
}

#[test]
#[serial_test::serial(openai_responses_image_source_kind_unrepresentable)]
fn two_unrepresentable_sources_in_one_request_count_once() {
    // Arrange: one request, two dropped sources across BOTH translate paths.
    // The counter is per-REQUEST, so this must still register a single event.
    let req = req_with(vec![
        user_parts(vec![image_part(json!({"type": "s3", "key": "a.png"}))]),
        tool_message_parts(
            "call_1",
            vec![
                image_part(json!({"type": "gs", "key": "b.png"})),
                image_part(json!({"type": "url", "url": "https://example.com/c.png"})),
            ],
        ),
    ]);

    // Act
    let before = responses_drop_count("image_source_kind_unrepresentable");
    let _ = translate_to_json(&cfg(), &req);
    let after = responses_drop_count("image_source_kind_unrepresentable");

    // Assert
    assert_eq!(
        after - before,
        1,
        "a request with three dropped blocks of one class is ONE drop event"
    );
}

#[test]
#[serial_test::serial(openai_responses_image_source_kind_unrepresentable)]
fn representable_image_sources_record_no_source_kind_drop() {
    // Arrange: both representable source shapes and nothing else.
    let req = req_with(vec![user_parts(vec![
        image_part(json!({"type": "url", "url": "https://example.com/cat.jpg"})),
        image_part(json!({"type": "base64", "media_type": "image/png", "data": "AAAA"})),
    ])]);

    // Act
    let before = responses_drop_count("image_source_kind_unrepresentable");
    let (wire, events) = translate_capturing(&cfg(), &req);
    let after = responses_drop_count("image_source_kind_unrepresentable");

    // Assert: both shipped, no WARN, no count.
    assert_eq!(
        wire["input"][0]["content"],
        json!([
            {"type": "input_image", "image_url": "https://example.com/cat.jpg"},
            {"type": "input_image", "image_url": "data:image/png;base64,AAAA"}
        ]),
        "got: {wire}"
    );
    assert!(
        !any_event_message_contains(&events, "unknown source kind"),
        "representable sources must emit no drop WARN, got: {events:?}"
    );
    assert_eq!(
        after, before,
        "representable sources must not move the counter"
    );
}

// ---------------------------------------------------------------------------
// cache_control_unsupported
//
// The Responses API models no prompt-cache breakpoint surface, so every
// caller `cache_control` marker drops. Cross-dialect only: the Responses
// ingress builds every content part and system block with `cache_control:
// None`, having no wire field to read one from.
// ---------------------------------------------------------------------------

#[test]
#[serial_test::serial(openai_responses_cache_control_unsupported)]
fn cache_control_marker_drops_from_the_wire_and_counts() {
    // Arrange: a marked text part beside an unmarked one, so the marker's
    // removal is separable from the part's translation.
    let mut req = req_with(vec![user_text_part_with_cc("cache me")]);
    req.cache_control = Some(CacheControl::ephemeral_5m());

    // Act
    let before = responses_drop_count("cache_control_unsupported");
    let (wire, events) = translate_capturing(&cfg(), &req);
    let after = responses_drop_count("cache_control_unsupported");

    // Assert 1: the WARN fired at WARN level, naming both marked surfaces.
    let record = events
        .iter()
        .find(|e| {
            e.message
                .contains("openai-responses egress: cache_control dropped")
        })
        .unwrap_or_else(|| panic!("the cache_control drop must be observable, got: {events:?}"));
    assert_eq!(record.level, tracing::Level::WARN, "got: {record:?}");
    let surfaces = record
        .field("dropped_surfaces")
        .unwrap_or_else(|| panic!("the record must name the surfaces, got: {record:?}"));
    assert!(surfaces.contains("messages"), "got: {record:?}");
    assert!(surfaces.contains("top-level"), "got: {record:?}");

    // Assert 2: the marker appears NOWHERE in the emitted wire value --
    // not as a top-level field, and not riding along inside a content part.
    // A serialized-substring check is what catches the marker surviving
    // inside an opaque payload rather than as a named field.
    let bytes = wire.to_string();
    assert!(
        !bytes.contains("cache_control"),
        "the cache_control marker must not reach the wire: {wire}"
    );
    assert!(
        !bytes.contains("ephemeral"),
        "the marker's value must not reach the wire either: {wire}"
    );

    // Assert 3: positive control -- the text the marker was attached to
    // still translated, so assertion 2 is not passing on a vanished part.
    assert_eq!(
        wire["input"][0]["content"],
        json!([{"type": "input_text", "text": "cache me"}]),
        "the marked part's text must still ship: {wire}"
    );

    assert_eq!(
        after - before,
        1,
        "the drop must be counted exactly once for the request"
    );
}

#[test]
#[serial_test::serial(openai_responses_cache_control_unsupported)]
fn a_system_only_cache_marker_still_counts_though_the_warn_defers_to_system_rs() {
    // Arrange: the ONLY marker sits on a system block. `system.rs` owns that
    // surface's DEBUG record, so the request-level WARN deliberately excludes
    // it -- but a marker WAS dropped, so the counter must still move. A
    // counter that mirrored the WARN's surface set would understate the lane.
    let mut req = req_with(vec![user_text("hi")]);
    req.system = Some(SystemContent::Blocks(vec![SystemBlock {
        kind: "text".into(),
        text: "be helpful".into(),
        cache_control: Some(CacheControl::ephemeral_5m()),
        citations: None,
    }]));

    // Act
    let before = responses_drop_count("cache_control_unsupported");
    let (wire, events) = translate_capturing(&cfg(), &req);
    let after = responses_drop_count("cache_control_unsupported");

    // Assert 1: system.rs's own record fired, counting the marked block.
    let record = events
        .iter()
        .find(|e| {
            e.message
                .contains("openai-responses: dropping cache_control on system block(s)")
        })
        .unwrap_or_else(|| {
            panic!("system.rs must record its own cache_control drop, got: {events:?}")
        });
    assert_eq!(record.field("dropped_count"), Some("1"), "got: {record:?}");
    assert!(
        !any_event_message_contains(&events, "openai-responses egress: cache_control dropped"),
        "the request-level WARN must not double-report the system surface, got: {events:?}"
    );

    // Assert 2: the marker is off the wire.
    assert!(
        !wire.to_string().contains("cache_control"),
        "the system marker must not reach the wire: {wire}"
    );

    // Assert 3: positive control -- the marked block's prompt text still
    // reached `instructions`.
    assert_eq!(wire["instructions"], json!("be helpful"), "got: {wire}");

    assert_eq!(
        after - before,
        1,
        "a system-only marker is still a counted drop"
    );
}

#[test]
#[serial_test::serial(openai_responses_cache_control_unsupported)]
fn an_unmarked_request_records_no_cache_control_drop() {
    // Arrange: the positive control as a whole request -- no marker anywhere.
    let req = req_with(vec![user_text("hi")]);

    // Act
    let before = responses_drop_count("cache_control_unsupported");
    let (wire, events) = translate_capturing(&cfg(), &req);
    let after = responses_drop_count("cache_control_unsupported");

    // Assert
    assert_eq!(
        wire["input"][0]["content"],
        json!([{"type": "input_text", "text": "hi"}])
    );
    assert!(
        !any_event_message_contains(&events, "cache_control"),
        "an unmarked request must emit no cache_control record, got: {events:?}"
    );
    assert_eq!(
        after, before,
        "an unmarked request must not move the counter"
    );
}

// ---------------------------------------------------------------------------
// the per-lane denominator
// ---------------------------------------------------------------------------

/// `record_translation_lane_seen` runs at exactly one site on this lane --
/// the top of `request::translate` -- so it counts every request the lane's
/// translate path processed, drop or not, Ok or Err. Without that, every
/// `drop_rate()` on this lane reads 0.0 no matter how many drops fired.
///
/// The assertion is a LOWER BOUND, not an equality, and deliberately so: the
/// lane denominator is one shared key that every `translate` call in this
/// crate's suite bumps, so no serial guard short of one shared by ~100 tests
/// could make an exact delta stable. A lower bound is race-immune in the only
/// direction the registry moves (up) while still failing loudly on the defect
/// it exists to catch -- an unwired or mis-placed site leaves the delta at 0.
/// That `translate` is the SOLE site is a grep property, welded by the census
/// rather than assertable from inside one test.
#[test]
fn every_translated_request_counts_toward_the_lane_denominator() {
    // Arrange: one clean request and one that FAILS translation. Both are
    // requests the lane processed, so both belong in the denominator.
    let clean = req_with(vec![user_text("hi")]);
    let rejected = req_with(vec![user_parts(vec![image_url_part(json!({"url": ""}))])]);

    // Act
    let before = responses_lane_seen_count();
    let _ = translate(&cfg(), &clean).expect("the clean request translates");
    let _ = translate(&cfg(), &rejected).expect_err("the malformed request is rejected");
    let after = responses_lane_seen_count();

    // Assert
    assert!(
        after - before >= 2,
        "both requests must count toward the denominator (a rejected request is \
         still a request the lane processed); delta was {}",
        after - before
    );
}

/// This lane's request-volume denominator, read through the registry's own
/// accessor. Reads only: an earlier version seeded a throwaway drop class so
/// the snapshot would materialize a row to read the denominator off, which
/// made a function named `..._count` mutate the registry on every call.
fn responses_lane_seen_count() -> u64 {
    crate::translation_drop_metrics::translation_lane_seen("openai-responses")
}
