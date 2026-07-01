//! SSE event-sequence parser + comparator. Parses upstream-shape SSE
//! bytes into ordered [`SseEventCmp`] structs and asserts pairwise
//! equality between two streams.

use serde_json::Value;
use thiserror::Error;

use super::{DiffMessage, assert_json_equal_structural};

/// Error returned by [`parse_sse_events`]. Identifies the offending
/// event index so failures point at the bad frame.
#[derive(Debug, Error)]
pub enum ParseError {
    #[error("malformed sse stream: {0}")]
    Malformed(String),
    #[error("invalid sse data on event #{index}: {message}")]
    InvalidJson { index: usize, message: String },
}

/// One parsed SSE event: an optional `event:` name and the JSON-decoded
/// `data:` payload. `[DONE]` sentinels arrive with `event = None` and
/// `data_parsed = Value::String("[DONE]")`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEventCmp {
    pub event: Option<String>,
    pub data_parsed: Value,
}

/// Parse an SSE byte stream into ordered events. Handles single-line
/// data, multi-line `data:` continuations (joined with `\n` before
/// JSON-parse), `event:` named events, comment lines (`:` prefix),
/// the `[DONE]` sentinel, and trailing-newline tolerance.
pub fn parse_sse_events(raw: &[u8]) -> Result<Vec<SseEventCmp>, ParseError> {
    let s = std::str::from_utf8(raw)
        .map_err(|e| ParseError::Malformed(format!("non-utf8 byte at {}", e.valid_up_to())))?;

    // Normalize CRLF -> LF and trim trailing newlines (one or two).
    let s = s.replace("\r\n", "\n");
    let s = s.trim_end_matches('\n').to_string();

    let mut out = Vec::new();
    for raw_block in s.split("\n\n") {
        let block = raw_block.trim_matches('\n');
        if block.is_empty() {
            continue;
        }
        if let Some(ev) = parse_sse_block(block, out.len())? {
            out.push(ev);
        }
    }
    Ok(out)
}

/// Parse one SSE block (text between two `\n\n` separators) into a
/// single event. Returns `None` for blocks that carry only comments
/// or only an `event:` line with no data.
fn parse_sse_block(block: &str, event_index: usize) -> Result<Option<SseEventCmp>, ParseError> {
    let mut event_name: Option<String> = None;
    let mut data_lines: Vec<&str> = Vec::new();
    for line in block.split('\n') {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(':') {
            continue;
        }
        if let Some(rest) = line.strip_prefix("event:") {
            event_name = Some(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest));
        }
        // Other field types (id:, retry:) are ignored -- we only diff data.
    }
    if data_lines.is_empty() {
        return Ok(None);
    }
    let data_str = data_lines.join("\n");
    if data_str.trim() == "[DONE]" {
        return Ok(Some(SseEventCmp {
            event: None,
            data_parsed: Value::String("[DONE]".to_string()),
        }));
    }
    let parsed: Value = serde_json::from_str(&data_str).map_err(|e| ParseError::InvalidJson {
        index: event_index,
        message: e.to_string(),
    })?;
    Ok(Some(SseEventCmp {
        event: event_name,
        data_parsed: parsed,
    }))
}

/// Compare two SSE byte streams pairwise. Event names must match in
/// order; `data:` payloads are compared via [`assert_json_equal_structural`]
/// with no ignore paths. The diff message names the first mismatched
/// event index.
pub fn assert_sse_equal(actual: &[u8], expected: &[u8]) -> Result<(), DiffMessage> {
    let a = parse_sse_events(actual)
        .map_err(|e| DiffMessage(format!("actual stream parse error: {e}")))?;
    let e = parse_sse_events(expected)
        .map_err(|err| DiffMessage(format!("expected stream parse error: {err}")))?;
    if a.len() != e.len() {
        return Err(DiffMessage(format!(
            "sse event count mismatch: actual={}, expected={}",
            a.len(),
            e.len()
        )));
    }
    for (i, (av, ev)) in a.iter().zip(e.iter()).enumerate() {
        if av.event != ev.event {
            return Err(DiffMessage(format!(
                "sse event name mismatch at index {}: actual={:?}, expected={:?}",
                i, av.event, ev.event
            )));
        }
        if let Err(diff) = assert_json_equal_structural(&av.data_parsed, &ev.data_parsed, &[]) {
            return Err(DiffMessage(format!(
                "sse data mismatch at index {i}: {diff}"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sse(s: &str) -> Vec<u8> {
        s.as_bytes().to_vec()
    }

    #[test]
    fn sse_equal_passes_on_identical_trivial() {
        let raw = sse(
            "event: message_start\ndata: {\"a\":1}\n\nevent: message_stop\ndata: {\"b\":2}\n\n",
        );
        assert!(assert_sse_equal(&raw, &raw).is_ok());
    }

    #[test]
    fn sse_equal_fails_on_event_name_mismatch() {
        let a = sse("event: foo\ndata: {}\n\nevent: bar\ndata: {}\n\n");
        let e = sse("event: foo\ndata: {}\n\nevent: BAZ\ndata: {}\n\n");
        let err = assert_sse_equal(&a, &e).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("name mismatch at index 1"), "got: {msg}");
    }

    #[test]
    fn sse_equal_fails_on_data_mismatch() {
        let a = sse("data: {\"x\":1}\n\ndata: {\"x\":2}\n\n");
        let e = sse("data: {\"x\":1}\n\ndata: {\"x\":3}\n\n");
        let err = assert_sse_equal(&a, &e).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("data mismatch at index 1"), "got: {msg}");
    }

    #[test]
    fn sse_equal_handles_done_sentinel() {
        let a = sse("data: {\"x\":1}\n\ndata: [DONE]\n\n");
        let e = sse("data: {\"x\":1}\n\ndata: [DONE]\n\n");
        assert!(assert_sse_equal(&a, &e).is_ok());
        let parsed = parse_sse_events(&a).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[1].event, None);
        assert_eq!(parsed[1].data_parsed, Value::String("[DONE]".to_string()));
    }

    #[test]
    fn sse_parser_skips_comment_lines() {
        let raw = sse(":heartbeat\n\ndata: {\"x\":1}\n\n: another comment\ndata: {\"x\":2}\n\n");
        let parsed = parse_sse_events(&raw).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].data_parsed, json!({"x": 1}));
        assert_eq!(parsed[1].data_parsed, json!({"x": 2}));
    }

    #[test]
    fn sse_parser_joins_multi_line_data() {
        let raw = sse("data: {\ndata:   \"foo\": \"bar\"\ndata: }\n\n");
        let parsed = parse_sse_events(&raw).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].data_parsed, json!({"foo": "bar"}));
    }

    #[test]
    fn sse_parser_tolerates_trailing_newlines() {
        let one = sse("data: {\"x\":1}\n");
        let two = sse("data: {\"x\":1}\n\n");
        let none = sse("data: {\"x\":1}");
        let p1 = parse_sse_events(&one).unwrap();
        let p2 = parse_sse_events(&two).unwrap();
        let p3 = parse_sse_events(&none).unwrap();
        assert_eq!(p1, p2);
        assert_eq!(p2, p3);
        assert_eq!(p1.len(), 1);
    }

    #[test]
    fn sse_parser_normalizes_crlf_to_lf() {
        let crlf = sse(
            "event: message_start\r\ndata: {\"x\":1}\r\n\r\nevent: message_stop\r\ndata: {\"y\":2}\r\n\r\n",
        );
        let lf = sse(
            "event: message_start\ndata: {\"x\":1}\n\nevent: message_stop\ndata: {\"y\":2}\n\n",
        );
        let p_crlf = parse_sse_events(&crlf).unwrap();
        let p_lf = parse_sse_events(&lf).unwrap();
        assert_eq!(p_crlf, p_lf);
        assert_eq!(p_crlf.len(), 2);
    }

    #[test]
    fn sse_parser_handles_no_space_after_data_colon() {
        let raw = sse("data:{\"x\":1}\n\n");
        let parsed = parse_sse_events(&raw).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].data_parsed, json!({"x": 1}));
    }
}
