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
