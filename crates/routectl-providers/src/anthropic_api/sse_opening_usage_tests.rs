//! Opening-usage carrier on the `message_start` role chunk. The upstream's
//! first-event input and cache fields ride the skip-serialized
//! `upstream_meta` of the content-free role chunk, field for field, and
//! never canonical `usage`.

use super::*;

const MESSAGE_START_WITH_CACHE: &str = r#"{
    "type":"message_start",
    "message": {
        "id":"msg_open","type":"message","role":"assistant",
        "content":[],"model":"claude-opus-4-7",
        "stop_reason":null,"stop_sequence":null,
        "usage": {
            "input_tokens": 1234,
            "output_tokens": 1,
            "cache_creation_input_tokens": 5678,
            "cache_read_input_tokens": 91011,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 5000,
                "ephemeral_1h_input_tokens": 678
            }
        }
    }
}"#;

const MESSAGE_START_WITHOUT_USAGE: &str = r#"{
    "type":"message_start",
    "message": {
        "id":"msg_bare","type":"message","role":"assistant",
        "content":[],"model":"claude-opus-4-7",
        "stop_reason":null,"stop_sequence":null
    }
}"#;

const TEXT_BLOCK_START: &str =
    r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#;
const TEXT_DELTA: &str =
    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#;

fn role_chunk_of(state: &mut SseState, message_start: &str) -> ChatChunk {
    state
        .parse_event("test", message_start)
        .expect("message_start parses")
        .expect("message_start emits the role chunk")
}

#[test]
fn role_chunk_carries_exact_first_event_input_and_cache_fields() {
    // Arrange
    let mut state = SseState::default();
    let before = std::time::Instant::now();

    // Act
    let chunk = role_chunk_of(&mut state, MESSAGE_START_WITH_CACHE);

    // Assert
    let opening = chunk
        .upstream_meta
        .as_ref()
        .and_then(|meta| meta.opening_usage.as_ref())
        .expect("the role chunk carries the opening usage");
    assert_eq!(opening.origin, OpeningUsageOrigin::AnthropicMessages);
    assert_eq!(opening.input_tokens, 1234);
    assert_eq!(opening.cache_creation_input_tokens, Some(5678));
    assert_eq!(opening.cache_read_input_tokens, Some(91011));
    assert_eq!(
        opening.cache_creation,
        Some(CacheCreation {
            ephemeral_5m_input_tokens: Some(5000),
            ephemeral_1h_input_tokens: Some(678),
        })
    );
    assert!(opening.observed_at >= before);
    assert!(
        chunk.usage.is_none(),
        "opening usage must never ride canonical chunk usage"
    );
    let meta = chunk.upstream_meta.as_ref().expect("carrier present");
    assert!(
        !meta.has_quota_family(),
        "the parser adds no quota family; that comes from the response head"
    );
}

#[test]
fn message_start_without_usage_produces_no_opening_carrier() {
    // Arrange
    let mut state = SseState::default();

    // Act
    let chunk = role_chunk_of(&mut state, MESSAGE_START_WITHOUT_USAGE);

    // Assert
    assert!(matches!(chunk.choices[0].delta.role, Some(Role::Assistant)));
    assert!(chunk.upstream_meta.is_none());
    assert!(chunk.usage.is_none());
}

#[test]
fn only_the_role_chunk_carries_the_opening_usage() {
    // Arrange
    let mut state = SseState::default();
    let _role = role_chunk_of(&mut state, MESSAGE_START_WITH_CACHE);

    // Act
    let later: Vec<ChatChunk> = [TEXT_BLOCK_START, TEXT_DELTA]
        .iter()
        .filter_map(|event| state.parse_event("test", event).expect("event parses"))
        .collect();

    // Assert
    assert!(!later.is_empty(), "the text delta emits a chunk");
    for chunk in &later {
        assert!(chunk.upstream_meta.is_none());
        assert!(chunk.usage.is_none());
    }
}

#[test]
fn a_repeated_message_start_emits_no_second_opening_carrier() {
    // Arrange
    let mut state = SseState::default();
    let _role = role_chunk_of(&mut state, MESSAGE_START_WITH_CACHE);

    // Act
    let repeated = state
        .parse_event("test", MESSAGE_START_WITH_CACHE)
        .expect("message_start parses");

    // Assert
    assert!(repeated.is_none());
}

#[test]
fn an_overridden_origin_labels_the_opening_carrier() {
    // Arrange
    let mut state = SseState::default().with_opening_origin(OpeningUsageOrigin::BedrockInvoke);

    // Act
    let chunk = role_chunk_of(&mut state, MESSAGE_START_WITH_CACHE);

    // Assert
    let opening = chunk
        .upstream_meta
        .and_then(|meta| meta.opening_usage)
        .expect("opening usage present");
    assert_eq!(opening.origin, OpeningUsageOrigin::BedrockInvoke);
    assert_eq!(opening.input_tokens, 1234);
}

#[test]
fn the_role_chunk_serializes_without_usage_or_carrier() {
    // Arrange
    let mut state = SseState::default();
    let chunk = role_chunk_of(&mut state, MESSAGE_START_WITH_CACHE);
    assert!(
        chunk.upstream_meta.is_some(),
        "positive control: carrier set"
    );

    // Act
    let wire = serde_json::to_value(&chunk).expect("chunk serializes");

    // Assert
    assert!(wire.get("usage").is_none(), "wire: {wire}");
    assert!(wire.get("upstream_meta").is_none(), "wire: {wire}");
    assert!(!wire.to_string().contains("1234"), "wire: {wire}");
}
