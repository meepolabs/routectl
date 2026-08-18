// The aggregated reasoning-skip WARNs emitted once per outbound provider
// attempt by `translate_messages`: exact counts, capped samples,
// truncation flags, and the per-attempt (not per-turn) aggregation across
// every assistant turn. Mirrored by the Converse translator's own sidecar
// fragment, `bedrock/converse/messages_reasoning_warn_tests.rs`. Imports
// live in the host `messages_tests.rs` -- do not add `use` lines here.

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

/// An assistant turn carrying `details` plus non-empty text content, so
/// its wire block list is never empty and the empty-content backstop
/// stays out of the WARN set under test.
fn assistant_turn(details: Vec<ReasoningDetail>) -> Message {
    Message {
        refusal: None,
        role: Role::Assistant,
        content: MessageContent::Text("ok".to_string()),
        reasoning: None,
        reasoning_details: details,
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// An assistant turn whose content is `Null`, so a non-emittable-only
/// detail set leaves the block list empty and reaches the backstop.
fn null_assistant_turn(details: Vec<ReasoningDetail>) -> Message {
    Message {
        content: MessageContent::Null,
        ..assistant_turn(details)
    }
}

/// A System turn. It is dropped from the wire output, so a transcript
/// that opens with one makes every assistant turn's CANONICAL index
/// differ from its output-array position -- the sampled locations must
/// name the canonical one.
fn system_turn() -> Message {
    Message {
        refusal: None,
        role: Role::System,
        content: MessageContent::Text("be brief".to_string()),
        reasoning: None,
        reasoning_details: Vec::new(),
        name: None,
        tool_call_id: None,
        tool_calls: None,
    }
}

/// Run the full per-role walk over `messages` and capture what it logged.
/// Driving the WARNs through `translate_messages` (rather than a single
/// turn's helper) is the point: the aggregation is per outbound attempt.
fn warns_for(messages: &[Message]) -> Vec<CapturedEvent> {
    let events = capture_events(|| {
        translate_messages("prov-test", messages, &mut passthrough_tally())
            .expect("translation ok");
    });
    events
        .into_iter()
        .filter(|e| e.level == tracing::Level::WARN)
        .collect()
}

/// Split a `Debug`-rendered `Vec<String>` field value into its element
/// strings so the sample's length and contents can be asserted without
/// pinning the whole rendering byte-for-byte.
fn debug_list_entries(rendered: &str) -> Vec<String> {
    let inner = rendered
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    if inner.is_empty() {
        return Vec::new();
    }
    inner.split(", ").map(|e| e.trim().to_string()).collect()
}

/// Split a `Debug`-rendered `Vec<(usize, Option<u32>)>` into its tuple
/// entries. Splitting at top-level parens rather than on ", " keeps the
/// nested `Some(n)` intact.
fn location_entries(rendered: &str) -> Vec<String> {
    let inner = rendered
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let mut entries: Vec<String> = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (i, c) in inner.char_indices() {
        match c {
            '(' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    entries.push(inner[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    entries
}

/// The distinct message indices a rendered location sample names, in
/// first-seen order.
fn sampled_message_indices(rendered: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for entry in location_entries(rendered) {
        let index = entry
            .trim_start_matches('(')
            .split(',')
            .next()
            .expect("tuple has a first element")
            .trim()
            .to_string();
        if !seen.contains(&index) {
            seen.push(index);
        }
    }
    seen
}

fn find_unsigned_warn(events: &[CapturedEvent]) -> &CapturedEvent {
    let matches: Vec<_> = events
        .iter()
        .filter(|e| {
            e.message
                .contains("skipping Thinking blocks on replay: signature missing or empty")
        })
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "exactly one aggregated unsigned WARN expected; got events: {events:?}"
    );
    let warn = matches[0];
    assert_eq!(warn.level, tracing::Level::WARN);
    warn
}

fn unsigned_warn_for(messages: &[Message]) -> CapturedEvent {
    let events = warns_for(messages);
    find_unsigned_warn(&events).clone()
}

/// More unsigned details than the log cap must still produce ONE WARN
/// whose `skipped_count` is exact while `skipped_locations` carries only a
/// capped sample flagged by `skipped_locations_truncated`. A sampled
/// `None` detail index (one the upstream did not supply) must survive as
/// `None` rather than being flattened to a plausible integer, so a
/// missing index stays distinguishable from index 0.
#[test]
fn unsigned_reasoning_warn_caps_logged_indices_and_flags_truncation() {
    // Arrange: 11 unsigned details on one turn, the first with no index
    // at all. `None` sorts as 0, so it lands inside the sampled prefix.
    let mut details = vec![unsigned_detail(None)];
    details.extend((1..=10u32).map(|i| unsigned_detail(Some(i))));
    assert!(details.len() > MAX_LOGGED_DIAGNOSTIC_ITEMS);

    // Act
    let warn = unsigned_warn_for(&[assistant_turn(details)]);

    // Assert
    assert_eq!(
        warn.field("skipped_count"),
        Some("11"),
        "skipped_count must stay exact, uncapped"
    );
    assert_eq!(
        warn.field("turns_affected"),
        Some("1"),
        "one turn carried every skip"
    );
    assert_eq!(
        warn.field("skipped_locations_truncated"),
        Some("true"),
        "a capped sample must be flagged as truncated"
    );
    let rendered = warn
        .field("skipped_locations")
        .expect("locations field present");
    let entries = location_entries(rendered);
    assert_eq!(
        entries.len(),
        MAX_LOGGED_DIAGNOSTIC_ITEMS,
        "sample must be capped at {MAX_LOGGED_DIAGNOSTIC_ITEMS}; got: {entries:?}"
    );
    assert!(
        entries.contains(&"(0, None)".to_string()),
        "a sampled None detail index must not be flattened away; got: {entries:?}"
    );
}

/// At or below the cap, the sample is the complete list and
/// `skipped_locations_truncated` must read `false` -- so the flag
/// distinguishes a sample from a whole list in BOTH directions.
#[test]
fn unsigned_reasoning_warn_keeps_full_index_list_when_within_cap() {
    // Arrange: 3 unsigned details on one turn, well under the cap.
    let details: Vec<_> = (0..3u32).map(|i| unsigned_detail(Some(i))).collect();

    // Act
    let warn = unsigned_warn_for(&[assistant_turn(details)]);

    // Assert
    assert_eq!(warn.field("skipped_count"), Some("3"));
    assert_eq!(warn.field("turns_affected"), Some("1"));
    assert_eq!(
        warn.field("skipped_locations_truncated"),
        Some("false"),
        "an uncapped sample must not be flagged as truncated"
    );
    let entries = location_entries(
        warn.field("skipped_locations")
            .expect("locations field present"),
    );
    assert_eq!(
        entries,
        vec!["(0, Some(0))", "(0, Some(1))", "(0, Some(2))"],
        "every location must be present when under the cap"
    );
}

/// The aggregation unit is the outbound provider ATTEMPT, not the
/// assistant turn: three skipping turns must produce ONE WARN whose
/// `skipped_count` is the pooled total and whose `turns_affected` is
/// exact. A per-turn implementation emits three lines each reading
/// `turns_affected=1`, so this cannot pass by accident.
#[test]
fn unsigned_reasoning_warn_pools_every_turn_into_one_line() {
    // Arrange: 3 assistant turns x 4 unsigned details = 12 skips.
    let messages: Vec<Message> = std::iter::once(system_turn())
        .chain(
            (0..3).map(|_| assistant_turn((0..4u32).map(|i| unsigned_detail(Some(i))).collect())),
        )
        .collect();

    // Act
    let events = warns_for(&messages);

    // Assert -- one line, exact magnitudes.
    let warn = find_unsigned_warn(&events);
    assert_eq!(
        warn.field("skipped_count"),
        Some("12"),
        "skipped_count pools every turn's skips"
    );
    assert_eq!(
        warn.field("turns_affected"),
        Some("3"),
        "turns_affected counts turns, not details"
    );
    assert_eq!(
        warn.field("skipped_locations_truncated"),
        Some("true"),
        "12 locations exceed the cap"
    );
    let rendered = warn
        .field("skipped_locations")
        .expect("locations field present");
    let entries = location_entries(rendered);
    assert_eq!(
        entries.len(),
        MAX_LOGGED_DIAGNOSTIC_ITEMS,
        "sample stays capped however many turns skip; got: {entries:?}"
    );
    // The sample fills in walk order, so the cap is exhausted by the
    // first two turns' 8 details. `turns_affected` above -- not the
    // sample -- is what reports that a third turn was affected.
    assert_eq!(
        sampled_message_indices(rendered),
        vec!["1", "2"],
        "the capped sample names the turns it had room for; got: {entries:?}"
    );
}

/// A pooled sample must name the TURN each skip came from, not just a
/// detail index: every message's `reasoning_details` has its own index
/// space, so `Some(0)` from two turns is two distinct locations. The
/// message index is the CANONICAL request index -- the leading System
/// turn is dropped from the wire output, so an implementation reading the
/// output array's position would report 0/1/2 here.
#[test]
fn unsigned_reasoning_warn_locations_name_the_canonical_turn_index() {
    // Arrange: System + 3 assistant turns x 2 unsigned details = 6
    // locations, under the cap so every turn reaches the sample.
    let messages: Vec<Message> = std::iter::once(system_turn())
        .chain(
            (0..3).map(|_| assistant_turn((0..2u32).map(|i| unsigned_detail(Some(i))).collect())),
        )
        .collect();

    // Act
    let warn = unsigned_warn_for(&messages);

    // Assert
    assert_eq!(warn.field("skipped_count"), Some("6"));
    assert_eq!(warn.field("turns_affected"), Some("3"));
    assert_eq!(warn.field("skipped_locations_truncated"), Some("false"));
    let entries = location_entries(
        warn.field("skipped_locations")
            .expect("locations field present"),
    );
    assert_eq!(
        entries,
        vec![
            "(1, Some(0))",
            "(1, Some(1))",
            "(2, Some(0))",
            "(2, Some(1))",
            "(3, Some(0))",
            "(3, Some(1))",
        ],
        "each location pairs the canonical message index with the \
         per-message detail index; got: {entries:?}"
    );
}

/// The empty-content backstop is folded into the same per-attempt tally,
/// so a transcript of Null-content unsigned-only turns emits a WARN count
/// that does not grow with the turn count. Left per-message, the backstop
/// alone would make the reasoning path O(turns) again.
#[test]
fn null_content_turns_keep_the_warn_count_independent_of_turn_count() {
    // Arrange: the same shape at two lengths.
    let three: Vec<Message> = (0..3)
        .map(|_| null_assistant_turn(vec![unsigned_detail(Some(0))]))
        .collect();
    let seven: Vec<Message> = (0..7)
        .map(|_| null_assistant_turn(vec![unsigned_detail(Some(0))]))
        .collect();

    // Act
    let three_warns = warns_for(&three);
    let seven_warns = warns_for(&seven);

    // Assert -- one unsigned line plus one backstop line, either length.
    assert_eq!(
        three_warns.len(),
        seven_warns.len(),
        "WARN count must not grow with turn count; 3 turns: {three_warns:?}, \
         7 turns: {seven_warns:?}"
    );
    assert_eq!(
        three_warns.len(),
        2,
        "one unsigned line plus one backstop line; got: {three_warns:?}"
    );
    let backstop = three_warns
        .iter()
        .find(|e| e.field("event") == Some("empty_content_backstop"))
        .expect("the aggregated backstop WARN must fire");
    assert_eq!(
        backstop.field("backstop_count"),
        Some("3"),
        "the backstop count stays exact even though the line is one"
    );
    assert_eq!(
        find_unsigned_warn(&seven_warns).field("skipped_count"),
        Some("7")
    );
}

/// The flush must be conditional per category: a transcript where nothing
/// is skipped emits nothing at all. A flush that always fires would turn
/// every healthy request into WARN noise.
#[test]
fn a_transcript_with_nothing_skipped_emits_no_warns() {
    // Arrange: a signed anthropic detail is emittable, so no category
    // records anything.
    let signed = ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: None,
        format: Some(ANTHROPIC_FORMAT.to_string()),
        index: Some(0),
        payload: json!({"text": "thinking", "signature": "sig"}),
    };

    // Act
    let warns = warns_for(&[assistant_turn(vec![signed])]);

    // Assert
    assert!(
        warns.is_empty(),
        "nothing was skipped, so no WARN may fire; got: {warns:?}"
    );
}

/// The two anthropic skip causes have different remediations (an upstream
/// signature defect versus a cross-provider replay), so they stay
/// SEPARATE lines: an attempt carrying both emits exactly two, never one
/// per message.
#[test]
fn both_skip_categories_emit_exactly_one_line_each() {
    // Arrange: 3 turns, each carrying one detail of each category.
    let messages: Vec<Message> = (0..3)
        .map(|_| {
            assistant_turn(vec![
                unsigned_detail(Some(0)),
                foreign_format_detail(Some("openai-o-format")),
            ])
        })
        .collect();

    // Act
    let warns = warns_for(&messages);

    // Assert
    assert_eq!(
        warns.len(),
        2,
        "one line per category, never one per message; got: {warns:?}"
    );
    assert_eq!(find_unsigned_warn(&warns).field("skipped_count"), Some("3"));
    assert_eq!(find_format_warn(&warns).field("skipped_count"), Some("3"));
}

/// A reasoning detail whose `format` is foreign to the Anthropic
/// translator -- the shape the format-skip branch aggregates. `Text` kind
/// with a present signature, so the only reason it cannot be echoed is
/// the format tag.
fn foreign_format_detail(format: Option<&str>) -> ReasoningDetail {
    ReasoningDetail {
        kind: ReasoningDetailKind::Text,
        id: None,
        format: format.map(str::to_string),
        index: None,
        payload: json!({"text": "thinking", "signature": "sig"}),
    }
}

fn find_format_warn(events: &[CapturedEvent]) -> &CapturedEvent {
    let matches: Vec<_> = events
        .iter()
        .filter(|e| {
            e.message
                .contains("skipping reasoning blocks on replay: format is not anthropic-claude-v1")
        })
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "exactly one aggregated format WARN expected; got events: {events:?}"
    );
    let warn = matches[0];
    assert_eq!(warn.level, tracing::Level::WARN);
    warn
}

fn format_warn_for(details: Vec<ReasoningDetail>) -> CapturedEvent {
    let events = warns_for(&[assistant_turn(details)]);
    find_format_warn(&events).clone()
}

/// More DISTINCT foreign formats than the log cap must still produce ONE
/// WARN whose `skipped_count` is the exact total (not the distinct count)
/// while `skipped_formats` carries only a capped sample flagged by
/// `formats_truncated`.
#[test]
fn format_skip_warn_caps_distinct_formats_and_flags_truncation() {
    // Arrange: 12 details spread over 9 distinct foreign formats.
    let mut details: Vec<_> = (0..9)
        .map(|i| foreign_format_detail(Some(&format!("foreign-format-{i}"))))
        .collect();
    details.extend((0..3).map(|i| foreign_format_detail(Some(&format!("foreign-format-{i}")))));
    assert_eq!(details.len(), 12);

    // Act
    let warn = format_warn_for(details);

    // Assert
    assert_eq!(
        warn.field("skipped_count"),
        Some("12"),
        "skipped_count must be the exact total, not the distinct count"
    );
    assert_eq!(
        warn.field("formats_truncated"),
        Some("true"),
        "a rejected distinct format must flag the sample as truncated"
    );
    let entries = debug_list_entries(
        warn.field("skipped_formats")
            .expect("formats field present"),
    );
    assert_eq!(
        entries.len(),
        MAX_LOGGED_DIAGNOSTIC_ITEMS,
        "sample must be capped at {MAX_LOGGED_DIAGNOSTIC_ITEMS}; got: {entries:?}"
    );
}

/// The defect this guards: deriving the truncation flag from
/// offered-vs-stored counts. Many details sharing ONE format store a
/// single entry that fully represents them, so nothing was dropped and
/// the flag must read `false` even though the exact count is far larger.
#[test]
fn format_skip_warn_reports_no_truncation_when_one_format_repeats() {
    // Arrange: 10 details, all carrying the same foreign format.
    let details: Vec<_> = (0..10)
        .map(|_| foreign_format_detail(Some("openai-o-format")))
        .collect();

    // Act
    let warn = format_warn_for(details);

    // Assert
    assert_eq!(warn.field("skipped_count"), Some("10"));
    assert_eq!(
        warn.field("formats_truncated"),
        Some("false"),
        "repeats of a stored format drop nothing, so the sample is whole"
    );
    let entries = debug_list_entries(
        warn.field("skipped_formats")
            .expect("formats field present"),
    );
    assert_eq!(
        entries,
        vec!["\"openai-o-format\""],
        "a repeated format must occupy exactly one slot; got: {entries:?}"
    );
}

/// One distinct format beyond a sample already at the cap IS a drop, so
/// the flag flips -- the boundary the duplicate case must not trip.
#[test]
fn format_skip_warn_flags_truncation_when_a_new_format_exceeds_the_cap() {
    // Arrange: exactly the cap in distinct formats, plus one more.
    let mut details: Vec<_> = (0..MAX_LOGGED_DIAGNOSTIC_ITEMS)
        .map(|i| foreign_format_detail(Some(&format!("known-format-{i}"))))
        .collect();
    details.push(foreign_format_detail(Some("brand-new-format")));

    // Act
    let warn = format_warn_for(details);

    // Assert
    assert_eq!(
        warn.field("formats_truncated"),
        Some("true"),
        "a distinct format rejected at capacity must flag truncation"
    );
    let rendered = warn
        .field("skipped_formats")
        .expect("formats field present");
    assert!(
        !rendered.contains("brand-new-format"),
        "the rejected format must not appear in the sample; got: {rendered}"
    );
}

/// A format tag is caller-supplied, so it reaches the log field
/// sanitized: no raw control character survives, and an oversized tag is
/// length-capped. Two tags differing only in control characters
/// therefore collapse into one slot rather than each claiming one.
#[test]
fn format_skip_warn_sanitizes_and_caps_caller_supplied_tags() {
    // Arrange: a control-char-bearing tag, the same tag with different
    // control characters, and a tag far longer than the sanitizer cap.
    let long_tag = "z".repeat(1000);
    let details = vec![
        foreign_format_detail(Some("evil\nformat\r\0tag")),
        foreign_format_detail(Some("evil\rformat\n\0tag")),
        foreign_format_detail(Some(&long_tag)),
    ];

    // Act
    let warn = format_warn_for(details);

    // Assert
    let rendered = warn
        .field("skipped_formats")
        .expect("formats field present");
    for raw in ['\n', '\r', '\0'] {
        assert!(
            !rendered.contains(raw),
            "a raw control character reached the log field: {rendered:?}"
        );
    }
    assert!(
        !rendered.contains(&long_tag),
        "an oversized tag must be length-capped; got: {rendered}"
    );
    let entries = debug_list_entries(rendered);
    assert_eq!(
        entries.len(),
        2,
        "tags differing only in control chars must share one slot; got: {entries:?}"
    );
    assert!(
        entries.iter().all(|e| e.len() <= 258),
        "every entry must stay within the sanitizer cap plus quoting; got: {entries:?}"
    );
}

/// A detail with no format tag at all still needs an operator-visible
/// slot, so the absent tag renders as an explicit placeholder rather
/// than an empty string.
#[test]
fn format_skip_warn_renders_an_absent_format_as_a_placeholder() {
    // Arrange
    let details = vec![foreign_format_detail(None)];

    // Act
    let warn = format_warn_for(details);

    // Assert
    assert_eq!(warn.field("skipped_count"), Some("1"));
    assert_eq!(
        warn.field("formats_truncated"),
        Some("false"),
        "a single stored format drops nothing"
    );
    let entries = debug_list_entries(
        warn.field("skipped_formats")
            .expect("formats field present"),
    );
    assert_eq!(entries, vec!["\"<none>\""]);
}

// --- format-tag rendering under the redaction knob ----------------------
//
// `record_format` reads the process-frozen knob, so the three cells of the
// (known/unknown tag) x (knob on/off) matrix are pinned against
// `render_skipped_format`, which takes the flag as an argument. The
// end-to-end knob-ON path through the WARN lives in the isolated
// `tests/anthropic_reasoning_format_redaction.rs` binary.

/// The knob's promise is about CALLER content. A vocabulary tag is protocol
/// vocabulary routectl itself defines, so redaction must not cost the
/// operator the one field that says which dialect arrived.
#[test]
fn a_known_format_tag_renders_literally_in_both_knob_states() {
    for tag in [
        ANTHROPIC_FORMAT,
        routectl_core::CODEX_OAUTH,
        routectl_core::OPENAI_APIKEY,
        routectl_core::BEDROCK_MANTLE,
        routectl_core::OPENAI_RESPONSES_V1,
    ] {
        assert_eq!(
            render_skipped_format(Some(tag), true),
            tag,
            "a known tag must survive redaction"
        );
        assert_eq!(
            render_skipped_format(Some(tag), false),
            tag,
            "a known tag is unchanged with redaction off"
        );
    }
}

/// An unrecognized tag is a caller-chosen free-text string, and the knob is
/// the operator's request to keep that class of content out of the logs.
#[test]
fn an_unknown_format_tag_becomes_the_placeholder_under_redaction() {
    assert_eq!(
        render_skipped_format(Some("openai-o-format"), true),
        "<unrecognized>"
    );
}

/// With redaction off, the literal echo is what makes a tag routectl does
/// not know yet discoverable at all.
#[test]
fn an_unknown_format_tag_echoes_literally_without_redaction() {
    assert_eq!(
        render_skipped_format(Some("openai-o-format"), false),
        "openai-o-format"
    );
}

/// Every unrecognized tag collapsing to ONE placeholder is the point: the
/// distinctness sample must not be fillable with caller-chosen strings.
#[test]
fn distinct_unknown_tags_share_the_single_placeholder_slot_under_redaction() {
    let details: Vec<_> = (0..MAX_LOGGED_DIAGNOSTIC_ITEMS + 3)
        .map(|i| foreign_format_detail(Some(&format!("foreign-format-{i}"))))
        .collect();
    let rendered: std::collections::BTreeSet<String> = details
        .iter()
        .map(|d| render_skipped_format(d.format.as_deref(), true))
        .collect();

    assert_eq!(
        rendered.into_iter().collect::<Vec<_>>(),
        vec!["<unrecognized>".to_string()],
        "unknown tags must not each claim a sample slot under redaction"
    );
}

/// The absent-tag placeholder is routectl's own literal, not caller
/// content, so the knob does not touch it.
#[test]
fn an_absent_format_renders_the_same_placeholder_in_both_knob_states() {
    assert_eq!(render_skipped_format(None, true), "<none>");
    assert_eq!(render_skipped_format(None, false), "<none>");
}
