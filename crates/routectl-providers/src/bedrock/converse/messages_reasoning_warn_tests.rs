// The aggregated unsigned-reasoning WARN the Converse egress emits once
// per outbound request: exact skip count, exact affected-turn count,
// bounded `(message_index, detail_index)` sample, truncation flag. Mirrors
// `anthropic_api::messages_reasoning_warn_tests.rs`. Imports live in the
// host `messages_tests.rs` -- do not add `use` lines here.

/// A Text reasoning detail in the anthropic format whose signature is
/// empty -- exactly the shape the unsigned-skip branch aggregates.
fn unsigned_detail(index: Option<u32>) -> ReasoningDetail {
    ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: None,
        format: Some(ANTHROPIC_FORMAT.to_string()),
        index,
        payload: json!({"text": "thinking", "signature": ""}),
    }
}

/// An assistant turn carrying `reasoning_details` plus trailing text, so
/// the turn survives translation even when every detail is skipped.
fn assistant_turn(details: Vec<ReasoningDetail>) -> Message {
    Message {
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Text("ok".into()),
        reasoning: None,
        reasoning_details: details,
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// An assistant turn whose `content` carries a `Thinking` PART with no
/// signature. This is the reachable hard-error shape: the content-part
/// translator rejects it rather than skipping it, so `build_messages`
/// returns `Err` from a later turn.
fn assistant_turn_with_unsigned_thinking_part() -> Message {
    Message {
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Parts(vec![ContentPart::Known(KnownContentPart::Thinking {
            thinking: "unsigned".into(),
            signature: None,
        })]),
        reasoning: None,
        reasoning_details: vec![],
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

const UNSIGNED_WARN_NEEDLE: &str =
    "skipping Thinking blocks on Converse replay: signature missing or empty";

fn unsigned_warns(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|e| e.message.contains(UNSIGNED_WARN_NEEDLE))
        .collect()
}

/// The single aggregated unsigned WARN. Asserting the count here is the
/// point of aggregation: a per-message implementation emits one line per
/// affected turn instead.
fn find_unsigned_warn(events: &[CapturedEvent]) -> &CapturedEvent {
    let matches = unsigned_warns(events);
    assert_eq!(
        matches.len(),
        1,
        "exactly one aggregated unsigned WARN expected per request; got events: {events:?}"
    );
    let warn = matches[0];
    assert_eq!(warn.level, tracing::Level::WARN);
    warn
}

/// Skips pooled across MORE turns than the sample can hold must still
/// yield ONE WARN whose `skipped_count` and `turns_affected` are exact
/// while `skipped_locations` carries only a capped prefix flagged by
/// `skipped_locations_truncated`. Three turns of four unsigned details
/// overflow the cap after the second turn, so the sample is the first
/// eight locations -- the counts, not the sample, carry the magnitude.
#[test]
#[serial_test::serial(bedrock_converse_reasoning_signature_missing_drop)]
fn unsigned_reasoning_warn_aggregates_every_turn_and_caps_the_location_sample() {
    // Arrange: 3 assistant turns at message indices 0, 1, 2; 4 unsigned
    // details each, so 12 skips against a sample capped at 8.
    let messages: Vec<Message> = (0..3)
        .map(|_| assistant_turn((0..4u32).map(|i| unsigned_detail(Some(i))).collect()))
        .collect();

    // Act
    let mut result_ok = false;
    let events = capture_events(|| {
        result_ok = build_messages("prov-test", &messages).is_ok();
    });

    // Assert
    assert!(result_ok, "unsigned details are skipped, not an error");
    let warn = find_unsigned_warn(&events);
    assert_eq!(
        warn.field("skipped_count"),
        Some("12"),
        "skipped_count must stay exact, uncapped"
    );
    assert_eq!(
        warn.field("turns_affected"),
        Some("3"),
        "every affected turn must be counted; a per-message emitter would \
         instead log three lines each carrying turns_affected=1"
    );
    assert_eq!(
        warn.field("skipped_locations"),
        Some(
            "[(0, Some(0)), (0, Some(1)), (0, Some(2)), (0, Some(3)), (1, Some(0)), (1, Some(1)), (1, Some(2)), (1, Some(3))]"
        ),
        "the sample is the capped prefix of (message_index, detail_index) pairs"
    );
    assert_eq!(
        warn.field("skipped_locations_truncated"),
        Some("true"),
        "a capped sample must be flagged as truncated"
    );
}

/// Within the cap the sample is the COMPLETE list, so
/// `skipped_locations_truncated` reads `false` and every affected turn's
/// message index is named. Pairing each detail index with its message
/// index is what makes the sample readable: `reasoning_details` indices
/// restart per turn, so a bare detail index pooled across turns cannot
/// distinguish a contiguous tail from a scattered set. A detail index the
/// upstream never supplied stays `None` rather than being flattened to a
/// plausible 0.
#[test]
#[serial_test::serial(bedrock_converse_reasoning_signature_missing_drop)]
fn unsigned_reasoning_warn_names_every_turn_and_keeps_a_missing_detail_index() {
    // Arrange: 3 assistant turns at message indices 0, 1, 2 with 2
    // unsigned details each -- 6 skips, under the cap. The first detail
    // of the first turn carries no index at all.
    let mut messages = vec![assistant_turn(vec![
        unsigned_detail(None),
        unsigned_detail(Some(1)),
    ])];
    messages.extend(
        (0..2).map(|_| assistant_turn(vec![unsigned_detail(Some(0)), unsigned_detail(Some(1))])),
    );

    // Act
    let events = capture_events(|| {
        build_messages("prov-test", &messages).expect("translation ok");
    });

    // Assert
    let warn = find_unsigned_warn(&events);
    assert_eq!(warn.field("skipped_count"), Some("6"));
    assert_eq!(warn.field("turns_affected"), Some("3"));
    assert_eq!(
        warn.field("skipped_locations"),
        Some("[(0, None), (0, Some(1)), (1, Some(0)), (1, Some(1)), (2, Some(0)), (2, Some(1))]"),
        "an uncapped sample names all three message indices and keeps a \
         missing detail index as None"
    );
    assert_eq!(
        warn.field("skipped_locations_truncated"),
        Some("false"),
        "an uncapped sample must not be flagged as truncated"
    );
}

/// A request whose reasoning details are all signed skips nothing, so the
/// aggregate WARN must not fire at all -- the flush is conditional on a
/// recorded skip, not on the tally existing.
#[test]
fn signed_reasoning_details_emit_no_unsigned_warn() {
    // Arrange
    let signed = ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: None,
        format: Some(ANTHROPIC_FORMAT.to_string()),
        index: Some(0),
        payload: json!({"text": "thinking", "signature": "sig_abc"}),
    };
    let messages = vec![assistant_turn(vec![signed])];

    // Act
    let events = capture_events(|| {
        build_messages("prov-test", &messages).expect("translation ok");
    });

    // Assert
    assert!(
        unsigned_warns(&events).is_empty(),
        "nothing was skipped, so no aggregate WARN is owed; got events: {events:?}"
    );
}

/// The aggregate is flushed on BOTH arms. A first turn records an unsigned
/// skip, then a later turn carries an unsigned `Thinking` content part,
/// which the content-part translator rejects with a hard error. Without a
/// flush on the error arm the recorded skip is silently swallowed and the
/// operator sees only the translation failure.
#[test]
#[serial_test::serial(bedrock_converse_reasoning_signature_missing_drop)]
fn unsigned_reasoning_warn_survives_a_later_turns_translation_error() {
    // Arrange
    let messages = vec![
        assistant_turn(vec![unsigned_detail(Some(0))]),
        assistant_turn_with_unsigned_thinking_part(),
    ];

    // Act
    let mut result_is_err = false;
    let events = capture_events(|| {
        result_is_err = build_messages("prov-test", &messages).is_err();
    });

    // Assert
    assert!(
        result_is_err,
        "an unsigned Thinking content part must fail translation"
    );
    let warn = find_unsigned_warn(&events);
    assert_eq!(warn.field("skipped_count"), Some("1"));
    assert_eq!(warn.field("turns_affected"), Some("1"));
    assert_eq!(
        warn.field("skipped_locations"),
        Some("[(0, Some(0))]"),
        "the skip recorded before the error must still reach the operator"
    );
}

// ---------------------------------------------------------------------------
// No-wire-shape reasoning kinds (Summary, an unrecognized kind): the
// aggregated WARN plus the per-request `bedrock-converse` /
// `reasoning_summary_unsupported` translation-drop counter this category
// feeds. Serialized against each other and against the no-wire-shape test
// in the host `messages.rs` inline `mod tests` (which shares this
// drop_class): the counter is a process-global registry, so an unmarked
// concurrent test asserting an exact delta on the same key would be flaky
// against this crate's default multi-threaded test runner.
// ---------------------------------------------------------------------------

const SUMMARY_WARN_NEEDLE: &str = "skipping reasoning details on Converse egress: kind has no Converse reasoningContent wire shape";

fn summary_warns(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|e| e.message.contains(SUMMARY_WARN_NEEDLE))
        .collect()
}

/// A reasoning detail whose kind has no Converse wire shape -- exactly the
/// shape `emit_reasoning_blocks_converse`'s merged `Summary | Other` arm
/// drops.
fn no_wire_shape_detail(kind: ReasoningDetailKind, index: Option<u32>) -> ReasoningDetail {
    ReasoningDetail {
        kind,
        id: None,
        format: Some(ANTHROPIC_FORMAT.to_string()),
        index,
        payload: json!({"text": "summary text"}),
    }
}

/// The `(bedrock-converse, reasoning_summary_unsupported)` drop counter's
/// current count, read through the same snapshot surface the router's
/// doctor path reads.
fn reasoning_summary_drop_count() -> u64 {
    crate::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == "bedrock-converse" && e.drop_class == "reasoning_summary_unsupported")
        .map_or(0, |e| e.drop_count)
}

/// Negative control: a turn carrying one `Summary` detail drops it, the
/// aggregated WARN names the category, and the per-request drop counter
/// advances by exactly one.
#[test]
#[serial_test::serial(bedrock_converse_reasoning_summary_drop)]
fn summary_reasoning_detail_warns_and_bumps_the_drop_counter_once() {
    // Arrange
    let before = reasoning_summary_drop_count();
    let messages = vec![assistant_turn(vec![no_wire_shape_detail(
        ReasoningDetailKind::Summary,
        Some(0),
    )])];

    // Act
    let events = capture_events(|| {
        build_messages("prov-test", &messages).expect("translation ok");
    });

    // Assert
    let matches = summary_warns(&events);
    assert_eq!(
        matches.len(),
        1,
        "exactly one aggregated no-wire-shape WARN expected per request; got events: {events:?}"
    );
    assert_eq!(matches[0].level, tracing::Level::WARN);
    assert_eq!(matches[0].field("skipped_count"), Some("1"));
    assert_eq!(
        reasoning_summary_drop_count(),
        before + 1,
        "the drop counter must advance by exactly one for this request"
    );
}

/// Once-per-request property, proven rather than assumed: a request
/// carrying THREE no-wire-shape details (a mix of `Summary` and an
/// unrecognized `Other` kind) across two turns must still bump the drop
/// counter by exactly one, not once per dropped detail. The counter is
/// wired from the tally's `flush()`, which runs once per `build_messages`
/// call -- this is the assertion that pins that placement.
#[test]
#[serial_test::serial(bedrock_converse_reasoning_summary_drop)]
fn multiple_no_wire_shape_details_in_one_request_bump_the_drop_counter_once() {
    // Arrange
    let before = reasoning_summary_drop_count();
    let messages = vec![
        assistant_turn(vec![
            no_wire_shape_detail(ReasoningDetailKind::Summary, Some(0)),
            no_wire_shape_detail(ReasoningDetailKind::Other("future.kind".into()), Some(1)),
        ]),
        assistant_turn(vec![no_wire_shape_detail(
            ReasoningDetailKind::Summary,
            Some(0),
        )]),
    ];

    // Act
    let events = capture_events(|| {
        build_messages("prov-test", &messages).expect("translation ok");
    });

    // Assert
    let matches = summary_warns(&events);
    assert_eq!(
        matches.len(),
        1,
        "three dropped details across two turns must still fold into ONE \
         aggregated WARN; got events: {events:?}"
    );
    assert_eq!(
        matches[0].field("skipped_count"),
        Some("3"),
        "the WARN's own count stays exact even though the drop counter \
         below only advances once for the whole request"
    );
    assert_eq!(
        reasoning_summary_drop_count(),
        before + 1,
        "three no-wire-shape details in one request is one drop EVENT, not \
         three -- the counter must advance by exactly one"
    );
}

/// Positive control: a sibling request carrying only `Text`/`Encrypted`
/// details (both of which DO have a Converse wire shape) survives with no
/// no-wire-shape WARN and no drop-counter increment.
#[test]
#[serial_test::serial(bedrock_converse_reasoning_summary_drop)]
fn wire_representable_reasoning_details_emit_no_summary_warn_or_drop() {
    // Arrange
    let before = reasoning_summary_drop_count();
    let text = ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: None,
        format: Some(ANTHROPIC_FORMAT.to_string()),
        index: Some(0),
        payload: json!({"text": "thinking", "signature": "sig_abc"}),
    };
    let encrypted = ReasoningDetail {
        kind: ReasoningDetailKind::Encrypted,
        id: None,
        format: Some(ANTHROPIC_FORMAT.to_string()),
        index: Some(1),
        payload: json!({"data": "opaque"}),
    };
    let messages = vec![assistant_turn(vec![text, encrypted])];

    // Act
    let events = capture_events(|| {
        build_messages("prov-test", &messages).expect("translation ok");
    });

    // Assert
    assert!(
        summary_warns(&events).is_empty(),
        "Text and Encrypted details both have a Converse wire shape, so no \
         no-wire-shape WARN is owed; got events: {events:?}"
    );
    assert_eq!(
        reasoning_summary_drop_count(),
        before,
        "a request with nothing dropped must not advance the drop counter"
    );
}

// ---------------------------------------------------------------------------
// The two remaining reasoning drop classes and their per-request counters:
// a detail whose `format` tag is not the one this egress replays, and a
// Text detail whose signature is missing or empty. Both were already
// skipped; neither was counted. Serialized on their own drop_class names,
// which must also cover every test elsewhere in the crate that reaches the
// same arm -- the unsigned tests above included.
// ---------------------------------------------------------------------------

const FOREIGN_FORMAT_WARN_NEEDLE: &str = "detail format is not the one this egress replays";

fn foreign_format_warns(events: &[CapturedEvent]) -> Vec<&CapturedEvent> {
    events
        .iter()
        .filter(|e| e.message.contains(FOREIGN_FORMAT_WARN_NEEDLE))
        .collect()
}

fn reasoning_drop_count(class: &str) -> u64 {
    crate::translation_drop_metrics::translation_drop_snapshot()
        .into_iter()
        .find(|e| e.lane == "bedrock-converse" && e.drop_class == class)
        .map_or(0, |e| e.drop_count)
}

/// A signed Text detail carrying a format tag this egress does not replay.
/// Signed on purpose: the signature check sits BELOW the format guard, so a
/// signed fixture proves the format guard is what dropped it.
fn foreign_format_detail(kind: ReasoningDetailKind, payload: Value) -> ReasoningDetail {
    ReasoningDetail {
        kind,
        id: None,
        format: Some("some-other-upstream-v1".to_string()),
        index: Some(0),
        payload,
    }
}

/// NEGATIVE CONTROL, `Text` arm. The format guard had zero tally and zero
/// log before this: a foreign-format detail vanished with no trace at all,
/// unlike the signature-empty case a few lines below it in the same arm.
/// All three assertions run here -- the WARN, the absence from the EMITTED
/// WIRE VALUE, and the surviving sibling.
#[test]
#[serial_test::serial(bedrock_converse_reasoning_foreign_format_drop)]
fn foreign_format_reasoning_detail_warns_and_bumps_the_drop_counter_once() {
    // Arrange
    let before = reasoning_drop_count("reasoning_foreign_format_unsupported");
    let messages = vec![assistant_turn(vec![foreign_format_detail(
        ReasoningDetailKind::Text,
        json!({"text": "SENTINELFOREIGNTHOUGHT", "signature": "sig_present"}),
    )])];

    // Act
    let mut wire = Value::Null;
    let events = capture_events(|| {
        let translated = build_messages(TEST_ID, &messages).expect("translation ok");
        wire = serde_json::to_value(&translated).expect("the message vec must serialize");
    });
    let after = reasoning_drop_count("reasoning_foreign_format_unsupported");

    // Assert 1 -- the WARN fired, with an exact skipped_count field.
    let matches = foreign_format_warns(&events);
    assert_eq!(
        matches.len(),
        1,
        "exactly one aggregated foreign-format WARN expected per request; got events: {events:?}"
    );
    assert_eq!(matches[0].level, tracing::Level::WARN);
    assert_eq!(matches[0].field("skipped_count"), Some("1"));

    // Assert 2 -- neither the thought text nor its signature reached the
    // upstream in ANY form. Asserting on the serialized body rather than the
    // typed block vec is what catches a payload riding inside an opaque
    // member.
    let body = wire.to_string();
    assert!(
        !body.contains("SENTINELFOREIGNTHOUGHT"),
        "a foreign-format thought must not reach the upstream; emitted body: {body}"
    );
    assert!(
        !body.contains("sig_present"),
        "the foreign-format signature must not reach the upstream; emitted body: {body}"
    );

    // Assert 3 -- positive control: the turn's own text still shipped, so the
    // fixture would have surfaced the thought had it ridden along.
    assert!(
        body.contains("ok"),
        "the assistant turn's text must survive the dropped detail; emitted body: {body}"
    );

    assert_eq!(
        after - before,
        1,
        "the foreign-format counter must advance by exactly one for this request"
    );
}

/// The `Encrypted` arm carries the identical guard and feeds the same
/// category, so a request mixing both arms is still ONE drop event. Pins
/// that the two guards were not instrumented as two separate classes.
#[test]
#[serial_test::serial(bedrock_converse_reasoning_foreign_format_drop)]
fn foreign_format_details_across_both_arms_bump_the_drop_counter_once() {
    // Arrange
    let before = reasoning_drop_count("reasoning_foreign_format_unsupported");
    let messages = vec![assistant_turn(vec![
        foreign_format_detail(
            ReasoningDetailKind::Text,
            json!({"text": "foreign thought", "signature": "sig_present"}),
        ),
        foreign_format_detail(
            ReasoningDetailKind::Encrypted,
            json!({"data": "SENTINELFOREIGNOPAQUE"}),
        ),
    ])];

    // Act
    let mut wire = Value::Null;
    let events = capture_events(|| {
        let translated = build_messages(TEST_ID, &messages).expect("translation ok");
        wire = serde_json::to_value(&translated).expect("the message vec must serialize");
    });
    let after = reasoning_drop_count("reasoning_foreign_format_unsupported");

    // Assert
    let matches = foreign_format_warns(&events);
    assert_eq!(
        matches.len(),
        1,
        "both arms fold into ONE aggregated WARN; got events: {events:?}"
    );
    assert_eq!(
        matches[0].field("skipped_count"),
        Some("2"),
        "the WARN's own count stays exact across both arms"
    );
    assert!(
        !wire.to_string().contains("SENTINELFOREIGNOPAQUE"),
        "the Encrypted arm's foreign payload must not reach the upstream; got: {wire}"
    );
    assert_eq!(
        after - before,
        1,
        "two foreign-format details in one request is one drop event, not two"
    );
}

/// POSITIVE CONTROL: a detail carrying the format tag this egress DOES
/// replay survives, emits no foreign-format WARN, and advances no
/// foreign-format counter -- proving the guard keys on the format tag and
/// not on something incidental to the fixture above.
#[test]
#[serial_test::serial(bedrock_converse_reasoning_foreign_format_drop)]
fn replayed_format_reasoning_detail_emits_no_foreign_format_drop() {
    // Arrange
    let before = reasoning_drop_count("reasoning_foreign_format_unsupported");
    let native = ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: None,
        format: Some(ANTHROPIC_FORMAT.to_string()),
        index: Some(0),
        payload: json!({"text": "SURVIVINGTHOUGHT", "signature": "sig_present"}),
    };
    let messages = vec![assistant_turn(vec![native])];

    // Act
    let mut wire = Value::Null;
    let events = capture_events(|| {
        let translated = build_messages(TEST_ID, &messages).expect("translation ok");
        wire = serde_json::to_value(&translated).expect("the message vec must serialize");
    });
    let after = reasoning_drop_count("reasoning_foreign_format_unsupported");

    // Assert
    assert!(
        foreign_format_warns(&events).is_empty(),
        "a detail in the replayed format is representable, so no WARN is owed; got: {events:?}"
    );
    assert!(
        wire.to_string().contains("SURVIVINGTHOUGHT"),
        "a representable detail must reach the upstream; emitted body: {wire}"
    );
    assert_eq!(
        after, before,
        "a request with nothing dropped must not advance the foreign-format counter"
    );
}

/// The unsigned-signature skip was already tallied and warned but never
/// counted. This pins its counter alongside the WARN the tests above
/// already cover, and asserts the unsigned thought is absent from the
/// EMITTED WIRE VALUE rather than merely from the typed block vec.
#[test]
#[serial_test::serial(bedrock_converse_reasoning_signature_missing_drop)]
fn unsigned_reasoning_skip_bumps_the_drop_counter_once() {
    // Arrange -- two unsigned details across two turns: still one event.
    let before = reasoning_drop_count("reasoning_signature_missing");
    let messages = vec![
        assistant_turn(vec![unsigned_detail(Some(0))]),
        assistant_turn(vec![unsigned_detail(Some(0))]),
    ];

    // Act
    let mut wire = Value::Null;
    let events = capture_events(|| {
        let translated = build_messages(TEST_ID, &messages).expect("translation ok");
        wire = serde_json::to_value(&translated).expect("the message vec must serialize");
    });
    let after = reasoning_drop_count("reasoning_signature_missing");

    // Assert 1 -- the aggregated WARN fired with an exact count.
    let warn = find_unsigned_warn(&events);
    assert_eq!(warn.field("skipped_count"), Some("2"));

    // Assert 2 -- the unsigned thought never reached the upstream. The
    // fixture's payload text is `thinking`; a reasoningContent block would
    // carry it verbatim.
    let body = wire.to_string();
    assert!(
        !body.contains("reasoningContent"),
        "an unsigned detail must emit no reasoningContent block; emitted body: {body}"
    );

    // Assert 3 -- positive control: the turns' own text still shipped.
    assert!(
        body.contains("ok"),
        "the assistant turns' text must survive the dropped details; emitted body: {body}"
    );

    assert_eq!(
        after - before,
        1,
        "two unsigned details across two turns is one drop event, not two"
    );
}

/// POSITIVE CONTROL for the signature class: a signed detail in the
/// replayed format produces its block and advances no counter.
#[test]
#[serial_test::serial(bedrock_converse_reasoning_signature_missing_drop)]
fn signed_reasoning_detail_emits_no_signature_missing_drop() {
    // Arrange
    let before = reasoning_drop_count("reasoning_signature_missing");
    let signed = ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: None,
        format: Some(ANTHROPIC_FORMAT.to_string()),
        index: Some(0),
        payload: json!({"text": "SIGNEDTHOUGHT", "signature": "sig_abc"}),
    };
    let messages = vec![assistant_turn(vec![signed])];

    // Act
    let mut wire = Value::Null;
    capture_events(|| {
        let translated = build_messages(TEST_ID, &messages).expect("translation ok");
        wire = serde_json::to_value(&translated).expect("the message vec must serialize");
    });
    let after = reasoning_drop_count("reasoning_signature_missing");

    // Assert
    assert!(
        wire.to_string().contains("SIGNEDTHOUGHT"),
        "a signed detail must reach the upstream; emitted body: {wire}"
    );
    assert_eq!(
        after, before,
        "a request with nothing dropped must not advance the signature counter"
    );
}
