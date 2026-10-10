//! One capped JSON parse of an upstream error body, carried on
//! [`crate::Error::Upstream`] so consumers read the envelope without parsing
//! the body again.
//!
//! [`ParsedErrorBody::parse`] is the only producer. It applies the same rules
//! as the log-redaction reader: a body opens like JSON after leading
//! whitespace and an optional UTF-8 BOM, and a body over
//! [`crate::MAX_ERROR_BODY_BYTES`] is never parsed. HTML, plain-text, over-cap
//! and unparseable bodies yield no parse.
//!
//! The parsed value is deliberately opaque: it has no `Display`, its `Debug`
//! prints no content, and [`crate::Error`]'s `Debug` omits it, so an upstream
//! envelope (which can echo request fields) never reaches a log sink through
//! it. Callers read it only through the accessors below.

use std::fmt;
use std::sync::Arc;

use serde_json::Value;

use crate::log_safe::{ErrorJsonBody, read_error_json_body};

/// A successful capped parse of an upstream error body. Pointer-sized and
/// cheap to clone.
#[derive(Clone)]
pub struct ParsedErrorBody(Arc<Parsed>);

struct Parsed {
    value: Value,
    source_len: usize,
    source_hash: u64,
}

fn body_fingerprint(body: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::hash::DefaultHasher::new();
    body.hash(&mut hasher);
    hasher.finish()
}

impl fmt::Debug for ParsedErrorBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ParsedErrorBody").finish_non_exhaustive()
    }
}

impl ParsedErrorBody {
    /// Parse `body` once, or return `None` when it does not open like JSON,
    /// is over [`crate::MAX_ERROR_BODY_BYTES`], or does not parse.
    #[must_use]
    pub fn parse(body: &str) -> Option<Self> {
        match read_error_json_body(body) {
            ErrorJsonBody::Parsed(value) => Some(Self(Arc::new(Parsed {
                value,
                source_len: body.len(),
                source_hash: body_fingerprint(body),
            }))),
            ErrorJsonBody::NotJson | ErrorJsonBody::Unreadable(_) => None,
        }
    }

    /// True when this parse was produced from `body` (same length and
    /// content fingerprint), checked without parsing `body` again.
    pub(crate) fn describes(&self, body: &str) -> bool {
        self.0.source_len == body.len() && self.0.source_hash == body_fingerprint(body)
    }

    fn error_object(&self) -> Option<&Value> {
        self.0.value.get("error")
    }

    /// True when the body carries a top-level `error` key -- the envelope
    /// OpenAI Chat Completions, OpenAI Responses, and Anthropic Messages use.
    pub fn is_error_envelope(&self) -> bool {
        self.error_object().is_some()
    }

    /// The string at `error.message`, if any.
    pub fn error_message(&self) -> Option<&str> {
        self.error_object()?.get("message")?.as_str()
    }

    /// The string at `error.type`, if any.
    pub fn error_type(&self) -> Option<&str> {
        self.error_object()?.get("type")?.as_str()
    }

    /// The `error.code` value, stringified when the upstream sent a number.
    pub fn error_code(&self) -> Option<String> {
        match self.error_object()?.get("code")? {
            Value::String(s) => Some(s.clone()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }

    /// The string at `error.param`, if any.
    pub fn error_param(&self) -> Option<&str> {
        self.error_object()?.get("param")?.as_str()
    }

    /// The whole parsed body, for dialect readers whose envelope differs
    /// from the `{"error":{...}}` shape. Never log it unredacted.
    pub fn value(&self) -> &Value {
        &self.0.value
    }
}

/// The upstream's `error.message` from `parsed`, or else a redacted,
/// sanitized excerpt of `body`. `parsed` must be [`ParsedErrorBody::parse`]
/// of `body`; with that parse it answers as
/// [`crate::extract_upstream_message`] does for an in-cap body without a BOM,
/// without parsing `body` again.
pub fn upstream_message_or_excerpt(body: &str, parsed: Option<&ParsedErrorBody>) -> String {
    if let Some(message) = parsed.and_then(ParsedErrorBody::error_message) {
        return message.to_string();
    }
    let redacted = match parsed {
        Some(p) => crate::log_safe::redact_parsed_error_body(body, p.value()),
        None => crate::redact_error_body_text(body),
    };
    crate::sanitize_upstream_body(&redacted)
}

#[cfg(test)]
mod tests {
    use super::ParsedErrorBody;
    use crate::log_safe::capped_parse_count;

    #[test]
    fn over_cap_body_yields_no_parse_and_never_runs_the_parser() {
        let filler = "x".repeat(crate::MAX_ERROR_BODY_BYTES);
        let body = format!(r#"{{"error":{{"message":"{filler}"}}}}"#);
        let before = capped_parse_count();

        let parsed = ParsedErrorBody::parse(&body);

        assert!(parsed.is_none());
        assert_eq!(capped_parse_count() - before, 0);
    }

    #[test]
    fn body_exactly_at_cap_is_parsed() {
        let shell = r#"{"error":{"message":""}}"#;
        let filler = "x".repeat(crate::MAX_ERROR_BODY_BYTES - shell.len());
        let body = format!(r#"{{"error":{{"message":"{filler}"}}}}"#);
        assert_eq!(body.len(), crate::MAX_ERROR_BODY_BYTES);

        let parsed = ParsedErrorBody::parse(&body).expect("in-cap body parses");

        assert!(parsed.is_error_envelope());
    }

    #[test]
    fn non_json_and_unparseable_bodies_yield_no_parse() {
        for body in ["<html>bad gateway</html>", "plain text", "{\"error\":", ""] {
            assert!(ParsedErrorBody::parse(body).is_none(), "{body:?}");
        }
    }

    #[test]
    fn bom_prefixed_body_is_parsed() {
        let parsed = ParsedErrorBody::parse("\u{feff}{\"error\":{\"type\":\"t\"}}")
            .expect("BOM-prefixed body parses");

        assert_eq!(parsed.error_type(), Some("t"));
    }

    #[test]
    fn numeric_error_code_is_stringified() {
        let parsed = ParsedErrorBody::parse(r#"{"error":{"code":429}}"#).expect("parses");

        assert_eq!(parsed.error_code().as_deref(), Some("429"));
    }

    #[test]
    fn debug_renders_no_content() {
        let parsed =
            ParsedErrorBody::parse(r#"{"error":{"message":"SENTINEL-DEBUG"}}"#).expect("parses");

        let rendered = format!("{parsed:?}");

        assert_eq!(rendered, "ParsedErrorBody { .. }");
    }
}
