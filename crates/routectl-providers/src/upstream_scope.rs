//! The closed table of upstream failure patterns whose reach is wider than
//! the target that answered ([`ScopeRow`]), matched on exact evidence from
//! one lane only.
//!
//! The response is untrusted and a recognized row widens the failure to
//! every target on the same endpoint, so the match fails closed: the body
//! must be in-cap and parse, the shared parse
//! ([`routectl_core::ParsedErrorBody`]) must
//! name the row's type and message, and a strict second parse of the same
//! text must find exactly the documented envelope keys, each once. The
//! shared parse keeps the last of a duplicated key; the strict parse rejects
//! the duplicate, so the two can never disagree about what was matched.

use reqwest::header::{HeaderMap, RETRY_AFTER};
use routectl_core::{ScopeRow, UpstreamBody};
use serde::{Deserialize, Deserializer};

/// The lane a response was answered on, for rows that are evidence only
/// there. A read with no lane matches no row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScopeLane {
    /// Anthropic Messages on the provider's own OAuth bearer credential:
    /// not the Bedrock Mantle endpoint, not a forwarded client bearer.
    AnthropicOAuth,
}

/// The non-standard millisecond reset header; a reset hint like
/// `Retry-After`.
const RETRY_AFTER_MS: &str = "retry-after-ms";

/// Prefix of the documented rate-limit header family. Header names in a
/// [`HeaderMap`] are always lowercase.
const RATELIMIT_HEADER_PREFIX: &str = "anthropic-ratelimit-";

/// Header facts the table reads, recorded by presence alone: a header whose
/// value does not parse still counts.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScopeHeaderFacts {
    has_retry_hint: bool,
    has_ratelimit_header: bool,
}

impl ScopeHeaderFacts {
    pub fn from_headers(headers: &HeaderMap) -> Self {
        Self {
            has_retry_hint: headers.contains_key(RETRY_AFTER)
                || headers.contains_key(RETRY_AFTER_MS),
            has_ratelimit_header: headers
                .keys()
                .any(|name| name.as_str().starts_with(RATELIMIT_HEADER_PREFIX)),
        }
    }
}

/// Everything a row can be matched on, taken from one upstream read.
pub struct ScopeEvidence<'a> {
    pub lane: Option<ScopeLane>,
    pub status: u16,
    pub hit_cap: bool,
    pub headers: ScopeHeaderFacts,
    pub body: &'a UpstreamBody,
}

/// One row: every field must hold, and the response must carry no reset
/// hint and no rate-limit header.
struct ScopeRule {
    row: ScopeRow,
    lane: ScopeLane,
    status: u16,
    error_type: &'static str,
    error_message: &'static str,
}

const SCOPE_TABLE: [ScopeRule; 1] = [ScopeRule {
    row: ScopeRow::AnthropicBareRateLimit,
    lane: ScopeLane::AnthropicOAuth,
    status: 429,
    error_type: "rate_limit_error",
    error_message: "Error",
}];

impl ScopeRule {
    fn matches(&self, evidence: &ScopeEvidence<'_>) -> bool {
        evidence.lane == Some(self.lane)
            && evidence.status == self.status
            && !evidence.hit_cap
            && !evidence.headers.has_retry_hint
            && !evidence.headers.has_ratelimit_header
            && is_exact_envelope(evidence.body, self.error_type, self.error_message)
    }
}

/// The row `evidence` is exact evidence of, or `None` (target scope).
pub fn recognize_scope_row(evidence: &ScopeEvidence<'_>) -> Option<ScopeRow> {
    SCOPE_TABLE
        .iter()
        .find(|rule| rule.matches(evidence))
        .map(|rule| rule.row)
}

/// True when the body is exactly `{"type":"error","error":{"type":..,
/// "message":..}}` with an optional string `request_id`, both parses agree
/// on the type and message, and no key repeats.
fn is_exact_envelope(body: &UpstreamBody, error_type: &str, error_message: &str) -> bool {
    let Some(parsed) = body.parsed() else {
        return false;
    };
    if parsed.error_type() != Some(error_type) || parsed.error_message() != Some(error_message) {
        return false;
    }
    serde_json::from_str::<StrictEnvelope>(body.text()).is_ok_and(|strict| {
        strict.kind == "error"
            && strict.error.kind == error_type
            && strict.error.message == error_message
    })
}

/// The documented error envelope, read strictly: an unknown key or a
/// repeated key fails the parse.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictEnvelope {
    #[serde(rename = "type")]
    kind: String,
    error: StrictError,
    #[serde(rename = "request_id", default, deserialize_with = "present_string")]
    _request_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictError {
    #[serde(rename = "type")]
    kind: String,
    message: String,
}

/// An optional key that, when present, must be a string (`null` fails).
fn present_string<'de, D: Deserializer<'de>>(de: D) -> Result<Option<String>, D::Error> {
    String::deserialize(de).map(Some)
}

#[cfg(test)]
#[path = "upstream_scope_tests.rs"]
pub mod tests;
