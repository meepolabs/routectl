//! Provenance of the terminal chunk's cache-inclusive input, through the
//! real parser and the real base-URL rule: the closing `message_delta`'s
//! own input, the first-party endpoint's first event carried over, or an
//! Anthropic-compatible proxy's first event carried over.

use routectl_core::{ChatChunk, UsageInputSource};

use super::stream_state_for;

const START: &str = r#"{"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"claude-opus-4-7","usage":{"input_tokens":12,"output_tokens":1,"cache_creation_input_tokens":300,"cache_read_input_tokens":40000}}}"#;
const OUTPUT_ONLY_DELTA: &str = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":9}}"#;
const FULL_DELTA: &str = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":20,"output_tokens":9,"cache_creation_input_tokens":300,"cache_read_input_tokens":40000}}"#;
const PARTIAL_DELTA: &str = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":20,"output_tokens":9}}"#;

const DIRECT: &str = "https://api.anthropic.com";
const PROXY: &str = "https://compat-proxy.example.invalid";

fn terminal(base_url: &str, delta: &str) -> ChatChunk {
    let mut state = stream_state_for("test", base_url);
    state.parse_event("test", START).expect("start parses");
    state
        .parse_event("test", delta)
        .expect("delta parses")
        .expect("terminal chunk")
}

fn source(chunk: &ChatChunk) -> Option<UsageInputSource> {
    chunk
        .upstream_meta
        .as_ref()
        .and_then(|m| m.usage_input_source)
}

#[test]
fn direct_output_only_delta_is_the_vendor_opening() {
    let chunk = terminal(DIRECT, OUTPUT_ONLY_DELTA);

    assert_eq!(
        chunk.usage.as_ref().and_then(|u| u.prompt_tokens),
        Some(40_312),
        "positive control: the opening was carried over"
    );
    assert_eq!(source(&chunk), Some(UsageInputSource::VendorOpening));
}

#[test]
fn proxy_output_only_delta_is_an_unverified_proxy_opening() {
    let chunk = terminal(PROXY, OUTPUT_ONLY_DELTA);

    assert_eq!(
        chunk.usage.as_ref().and_then(|u| u.prompt_tokens),
        Some(40_312)
    );
    assert_eq!(source(&chunk), Some(UsageInputSource::ProxyOpening));
}

#[test]
fn a_delta_reporting_every_input_component_is_explicit_on_either_endpoint() {
    for base in [DIRECT, PROXY] {
        let chunk = terminal(base, FULL_DELTA);

        assert_eq!(
            chunk.usage.as_ref().and_then(|u| u.prompt_tokens),
            Some(40_320)
        );
        assert_eq!(
            source(&chunk),
            Some(UsageInputSource::ExplicitFinal),
            "{base}"
        );
    }
}

#[test]
fn a_delta_missing_a_cache_component_is_not_explicit() {
    // The cache fields are copied from the first event, so the total is
    // only as good as that event.
    let chunk = terminal(PROXY, PARTIAL_DELTA);

    assert_eq!(
        chunk.usage.as_ref().and_then(|u| u.prompt_tokens),
        Some(40_320)
    );
    assert_eq!(source(&chunk), Some(UsageInputSource::ProxyOpening));
}

#[test]
fn only_the_exact_first_party_host_is_the_vendor() {
    let chunk = terminal("https://api.anthropic.com.example.net", OUTPUT_ONLY_DELTA);

    assert_eq!(source(&chunk), Some(UsageInputSource::ProxyOpening));
}

#[test]
fn a_stream_with_no_input_anywhere_carries_no_source() {
    let mut state = stream_state_for("test", DIRECT);
    state
        .parse_event(
            "test",
            r#"{"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"x"}}"#,
        )
        .expect("start parses");
    let chunk = state
        .parse_event("test", OUTPUT_ONLY_DELTA)
        .expect("delta parses")
        .expect("terminal chunk");

    assert!(chunk.usage.as_ref().and_then(|u| u.prompt_tokens).is_none());
    assert_eq!(source(&chunk), None);
}

const OPENER_123: &str = r#"{"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"x","usage":{"input_tokens":123,"output_tokens":1,"cache_creation_input_tokens":55,"cache_read_input_tokens":700}}}"#;

fn close_after_opener(base_url: &str, delta_usage: &str) -> ChatChunk {
    let mut state = stream_state_for("test", base_url);
    state.parse_event("test", OPENER_123).expect("start parses");
    let delta = format!(
        r#"{{"type":"message_delta","delta":{{"stop_reason":"end_turn","stop_sequence":null}},"usage":{delta_usage}}}"#
    );
    state
        .parse_event("test", &delta)
        .expect("delta parses")
        .expect("terminal chunk")
}

#[test]
fn an_explicit_zero_raw_input_beside_a_cache_read_is_a_fully_cached_prompt() {
    let chunk = close_after_opener(
        PROXY,
        r#"{"input_tokens":0,"output_tokens":9,"cache_creation_input_tokens":0,"cache_read_input_tokens":900}"#,
    );

    let usage = chunk.usage.as_ref().expect("usage");
    assert_eq!(usage.prompt_tokens, Some(900));
    assert_eq!(usage.cache_read_input_tokens, Some(900));
    assert_eq!(usage.cache_creation_input_tokens, Some(0));
    assert_eq!(source(&chunk), Some(UsageInputSource::ExplicitFinal));
}

#[test]
fn an_explicit_zero_cache_component_overrides_the_opener_when_raw_is_positive() {
    let chunk = close_after_opener(
        PROXY,
        r#"{"input_tokens":40,"output_tokens":9,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}"#,
    );

    let usage = chunk.usage.as_ref().expect("usage");
    assert_eq!(usage.prompt_tokens, Some(40));
    assert_eq!(usage.cache_read_input_tokens, Some(0));
    assert_eq!(source(&chunk), Some(UsageInputSource::ExplicitFinal));
}

#[test]
fn an_all_zero_placeholder_delta_still_backfills_with_the_endpoint_marker() {
    for (base, expected) in [
        (DIRECT, UsageInputSource::VendorOpening),
        (PROXY, UsageInputSource::ProxyOpening),
    ] {
        let chunk = close_after_opener(
            base,
            r#"{"input_tokens":0,"output_tokens":9,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}"#,
        );

        assert_eq!(
            chunk.usage.as_ref().and_then(|u| u.prompt_tokens),
            Some(123 + 55 + 700),
            "{base}"
        );
        assert_eq!(source(&chunk), Some(expected), "{base}");
    }
}

#[test]
fn the_opening_carrier_records_whether_the_endpoint_is_the_vendor() {
    for (base, vendor) in [(DIRECT, true), (PROXY, false)] {
        let mut state = stream_state_for("test", base);

        let opener = state
            .parse_event("test", START)
            .expect("parses")
            .expect("opener");

        let opening = opener
            .upstream_meta
            .and_then(|m| m.opening_usage)
            .expect("opening carried");
        assert_eq!(opening.from_vendor_endpoint, vendor, "{base}");
    }
}

const RAW_ZERO_OPENER: &str = r#"{"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"x","usage":{"input_tokens":0,"output_tokens":1}}}"#;

fn close_after(opener: &str, base_url: &str, delta_usage: &str) -> ChatChunk {
    let mut state = stream_state_for("test", base_url);
    state.parse_event("test", opener).expect("start parses");
    let delta = format!(
        r#"{{"type":"message_delta","delta":{{"stop_reason":"end_turn","stop_sequence":null}},"usage":{delta_usage}}}"#
    );
    state
        .parse_event("test", &delta)
        .expect("delta parses")
        .expect("terminal chunk")
}

#[test]
fn a_delta_with_a_cache_read_but_no_raw_input_field_is_partial() {
    let chunk = close_after(
        RAW_ZERO_OPENER,
        PROXY,
        r#"{"output_tokens":9,"cache_read_input_tokens":900}"#,
    );

    assert_eq!(
        chunk.usage.as_ref().and_then(|u| u.prompt_tokens),
        Some(900),
        "the wire and ledger total is unchanged"
    );
    assert_eq!(source(&chunk), Some(UsageInputSource::PartialFinal));
}

#[test]
fn a_stated_zero_raw_input_beside_a_cache_read_is_explicit_after_a_raw_zero_opener() {
    let chunk = close_after(
        RAW_ZERO_OPENER,
        PROXY,
        r#"{"input_tokens":0,"output_tokens":9,"cache_read_input_tokens":900}"#,
    );

    assert_eq!(
        chunk.usage.as_ref().and_then(|u| u.prompt_tokens),
        Some(900)
    );
    assert_eq!(source(&chunk), Some(UsageInputSource::ExplicitFinal));
}

#[test]
fn a_positive_raw_input_without_cache_fields_is_explicit_when_the_opener_adds_nothing() {
    let chunk = close_after(
        RAW_ZERO_OPENER,
        PROXY,
        r#"{"input_tokens":40,"output_tokens":9}"#,
    );

    assert_eq!(chunk.usage.as_ref().and_then(|u| u.prompt_tokens), Some(40));
    assert_eq!(source(&chunk), Some(UsageInputSource::ExplicitFinal));
}

#[test]
fn a_positive_cache_component_copied_from_the_opener_is_a_backfill() {
    // The opener's cache read is positive and the delta omits it.
    let chunk = close_after(
        OPENER_123,
        PROXY,
        r#"{"input_tokens":40,"output_tokens":9}"#,
    );

    assert_eq!(
        chunk.usage.as_ref().and_then(|u| u.prompt_tokens),
        Some(40 + 55 + 700)
    );
    assert_eq!(source(&chunk), Some(UsageInputSource::ProxyOpening));
}

/// A `message_start` carrying no usage at all, so the stream has no opening
/// carrier to say which endpoint it came from.
const START_WITHOUT_USAGE: &str = r#"{"type":"message_start","message":{"id":"m","type":"message","role":"assistant","content":[],"model":"x"}}"#;

fn explicit_close_without_opener(base_url: &str) -> ChatChunk {
    let mut state = stream_state_for("test", base_url);
    let opener = state
        .parse_event("test", START_WITHOUT_USAGE)
        .expect("start parses")
        .expect("role chunk");
    assert!(
        opener
            .upstream_meta
            .as_ref()
            .and_then(|m| m.opening_usage.as_ref())
            .is_none(),
        "premise: no opening carrier"
    );
    state
        .parse_event("test", FULL_DELTA)
        .expect("delta parses")
        .expect("terminal chunk")
}

fn usage_from_vendor(chunk: &ChatChunk) -> Option<bool> {
    chunk
        .upstream_meta
        .as_ref()
        .and_then(|m| m.usage_from_vendor_endpoint)
}

#[test]
fn an_explicit_close_records_its_endpoint_without_any_opening_carrier() {
    let direct = explicit_close_without_opener(DIRECT);
    let proxy = explicit_close_without_opener(PROXY);

    // The two closes are byte-identical in usage and source; only the
    // endpoint the parser read them from differs.
    let total = |c: &ChatChunk| c.usage.as_ref().and_then(|u| u.prompt_tokens);
    assert_eq!(total(&direct), Some(40_320));
    assert_eq!(total(&direct), total(&proxy));
    assert_eq!(source(&direct), Some(UsageInputSource::ExplicitFinal));
    assert_eq!(source(&proxy), Some(UsageInputSource::ExplicitFinal));
    assert_eq!(usage_from_vendor(&direct), Some(true));
    assert_eq!(usage_from_vendor(&proxy), Some(false));
}
