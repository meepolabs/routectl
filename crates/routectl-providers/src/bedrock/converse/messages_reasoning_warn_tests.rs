// The aggregated unsigned-reasoning WARN emitted by
// `emit_reasoning_blocks_converse`: exact count, capped index sample,
// truncation flag. Mirrors
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
                .contains("skipping Thinking blocks on Converse replay: signature missing or empty")
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
    assert!(details.len() > MAX_LOGGED_SKIPPED_INDICES);

    // Act
    let mut blocks_out = None;
    let events = capture_events(|| {
        blocks_out =
            Some(emit_reasoning_blocks_converse("prov-test", &details).expect("translation ok"));
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
        MAX_LOGGED_SKIPPED_INDICES,
        "sample must be capped at {MAX_LOGGED_SKIPPED_INDICES}; got: {entries:?}"
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
        emit_reasoning_blocks_converse("prov-test", &details).expect("translation ok");
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
