// The aggregated unsigned-reasoning WARN emitted by
// `emit_reasoning_blocks`: exact count, capped index sample, truncation
// flag. Mirrored by the Converse translator's own sidecar fragment,
// `bedrock/converse/messages_reasoning_warn_tests.rs`. Imports live in
// the host `messages_tests.rs` -- do not add `use` lines here.

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

/// Split a `Debug`-rendered `Vec<Option<u32>>` field value into its
/// element strings so the sample's length and its `None` entries can be
/// asserted without pinning the whole rendering byte-for-byte.
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

/// More unsigned details than the log cap must still produce ONE WARN
/// whose `skipped_count` is exact while `skipped_indices` carries only a
/// capped sample flagged by `indices_truncated`. A sampled `None` index
/// (one the upstream did not supply) must survive as `None` rather than
/// being flattened to a plausible integer, so a missing index stays
/// distinguishable from index 0.
#[test]
fn unsigned_reasoning_warn_caps_logged_indices_and_flags_truncation() {
    // Arrange: 11 unsigned details, the first with no index at all.
    // `None` sorts as 0, so it lands inside the sampled prefix.
    let mut details = vec![unsigned_detail(None)];
    details.extend((1..=10u32).map(|i| unsigned_detail(Some(i))));
    assert!(details.len() > MAX_LOGGED_DIAGNOSTIC_ITEMS);

    // Act
    let mut blocks_out = None;
    let events = capture_events(|| {
        blocks_out = Some(
            emit_reasoning_blocks("prov-test", &details, &mut passthrough_tally())
                .expect("translation ok"),
        );
    });

    // Assert
    assert!(
        blocks_out.expect("translator ran").is_empty(),
        "every unsigned detail must be skipped"
    );
    let warn = find_unsigned_warn(&events);
    assert_eq!(
        warn.field("skipped_count"),
        Some("11"),
        "skipped_count must stay exact, uncapped"
    );
    assert_eq!(
        warn.field("indices_truncated"),
        Some("true"),
        "a capped sample must be flagged as truncated"
    );
    let entries = debug_list_entries(
        warn.field("skipped_indices")
            .expect("indices field present"),
    );
    assert_eq!(
        entries.len(),
        MAX_LOGGED_DIAGNOSTIC_ITEMS,
        "sample must be capped at {MAX_LOGGED_DIAGNOSTIC_ITEMS}; got: {entries:?}"
    );
    assert!(
        entries.iter().any(|e| e == "None"),
        "a sampled None index must not be flattened away; got: {entries:?}"
    );
}

/// At or below the cap, the sample is the complete list and
/// `indices_truncated` must read `false` -- so the flag distinguishes a
/// sample from a whole list in BOTH directions.
#[test]
fn unsigned_reasoning_warn_keeps_full_index_list_when_within_cap() {
    // Arrange: 3 unsigned details, well under the cap.
    let details: Vec<_> = (0..3u32).map(|i| unsigned_detail(Some(i))).collect();

    // Act
    let events = capture_events(|| {
        emit_reasoning_blocks("prov-test", &details, &mut passthrough_tally())
            .expect("translation ok");
    });

    // Assert
    let warn = find_unsigned_warn(&events);
    assert_eq!(warn.field("skipped_count"), Some("3"));
    assert_eq!(
        warn.field("indices_truncated"),
        Some("false"),
        "an uncapped sample must not be flagged as truncated"
    );
    let entries = debug_list_entries(
        warn.field("skipped_indices")
            .expect("indices field present"),
    );
    assert_eq!(
        entries,
        vec!["Some(0)", "Some(1)", "Some(2)"],
        "every index must be present when under the cap"
    );
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

fn format_warn_for(details: &[ReasoningDetail]) -> CapturedEvent {
    let events = capture_events(|| {
        emit_reasoning_blocks("prov-test", details, &mut passthrough_tally())
            .expect("translation ok");
    });
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
    let warn = format_warn_for(&details);

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
    let warn = format_warn_for(&details);

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
    let warn = format_warn_for(&details);

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
    let warn = format_warn_for(&details);

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
    let warn = format_warn_for(&details);

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
