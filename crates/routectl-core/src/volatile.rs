//! Structural volatile-content detector over a request's stable cacheable
//! prefix.
//!
//! PURPOSE: a pure, warn-only diagnostic that also serves as the
//! non-mutating veto for auto-cache emission. A top-level cache breakpoint
//! caches the stable prefix (system prompt + tool name/description strings).
//! If that prefix carries per-request-volatile tokens (fresh ids, fresh
//! timestamps, auth tokens), auto-caching it writes a brand-new cache entry
//! every request that is never re-read -- the cache-write tokens bill at a
//! premium and the read never happens. The detector flags this at HIGH
//! confidence so the dispatch path can decline to auto-cache.
//!
//! COST ASYMMETRY (drives the tiering): a false HIGH (vetoing a prefix that
//! was actually stable) only costs a missed cache win -- equivalent to the
//! status quo of not auto-caching. A false NONE (allowing auto-cache of a
//! truly volatile prefix) costs wasted cache-write spend on every request.
//! HIGH therefore uses only high-precision whole-token patterns so it is both
//! rare and never trips on prose. Residual missed volatility (the LOW tier)
//! is backstopped downstream by an empirical thrash signal plus a
//! per-provider kill-switch, not by this detector.
//!
//! SCOPE (always-stable prefix only):
//!
//! - SYSTEM prompt text: both `SystemContent::Text` and each block's `text`.
//! - TOOL strings: `ToolDef::Custom` name + description; for
//!   `ToolDef::Other`, the `"name"` / `"description"` string fields if
//!   present.
//!
//! EXCLUDED on purpose:
//!
//! - messages -- they legitimately vary turn to turn, so scanning them would
//!   produce false vetoes and is not what a system+tools prefix cache
//!   depends on.
//! - tool `input_schema` -- schemas are structurally stable but routinely
//!   embed example values (ids, dates) that would false-positive.

use crate::schema::ChatRequest;
use crate::system_content::SystemContent;
use crate::tool_def::ToolDef;

/// Confidence that the scanned stable prefix carries per-request-volatile
/// content. Only `High` vetoes auto-caching; `Low` is warn-only and `None`
/// is the prose default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VolatileConfidence {
    None,
    Low,
    High,
}

/// Which high-precision volatile pattern matched. Recorded only for HIGH
/// matches so callers and logs can see what tripped the veto.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VolatileKind {
    Uuid,
    Timestamp,
    Jwt,
    HexBlob,
}

/// Result of scanning a request's stable prefix. Constructor-only: build it
/// through `scan_volatile`, read it through the accessors.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct VolatileReport {
    confidence: VolatileConfidence,
    kinds: Vec<VolatileKind>,
}

impl VolatileReport {
    pub const fn confidence(&self) -> VolatileConfidence {
        self.confidence
    }

    /// Matched HIGH-confidence kinds in first-seen order, deduplicated (a
    /// kind appears at most once even if several tokens of that kind match).
    pub fn kinds(&self) -> &[VolatileKind] {
        &self.kinds
    }

    /// The only surface the dispatch path consumes: true iff the prefix
    /// scored HIGH and auto-caching it should be vetoed.
    pub fn is_high_confidence_veto(&self) -> bool {
        self.confidence == VolatileConfidence::High
    }
}

/// Scan a request's always-stable cacheable prefix for per-request-volatile
/// tokens. PURE: borrows `req` read-only, returns an owned report, mutates
/// nothing.
#[must_use]
pub fn scan_volatile(req: &ChatRequest) -> VolatileReport {
    let mut acc = Accumulator::new();

    if let Some(system) = &req.system {
        scan_system(system, &mut acc);
    }
    if let Some(tools) = &req.tools {
        for tool in tools {
            scan_tool(tool, &mut acc);
        }
    }

    acc.into_report()
}

/// Collects the worst tier seen and the set of HIGH kinds in first-seen
/// order. Stops escalating once HIGH; LOW upgrades None but never downgrades
/// HIGH.
struct Accumulator {
    confidence: VolatileConfidence,
    kinds: Vec<VolatileKind>,
}

impl Accumulator {
    const fn new() -> Self {
        Self {
            confidence: VolatileConfidence::None,
            kinds: Vec::new(),
        }
    }

    fn record_high(&mut self, kind: VolatileKind) {
        self.confidence = VolatileConfidence::High;
        if !self.kinds.contains(&kind) {
            self.kinds.push(kind);
        }
    }

    fn record_low(&mut self) {
        if self.confidence == VolatileConfidence::None {
            self.confidence = VolatileConfidence::Low;
        }
    }

    fn into_report(self) -> VolatileReport {
        VolatileReport {
            confidence: self.confidence,
            kinds: self.kinds,
        }
    }
}

fn scan_system(system: &SystemContent, acc: &mut Accumulator) {
    match system {
        SystemContent::Text(s) => scan_text(s, acc),
        SystemContent::Blocks(blocks) => {
            for block in blocks {
                scan_text(&block.text, acc);
            }
        }
    }
}

fn scan_tool(tool: &ToolDef, acc: &mut Accumulator) {
    match tool {
        ToolDef::Custom(custom) => {
            scan_text(&custom.name, acc);
            if let Some(desc) = &custom.description {
                scan_text(desc, acc);
            }
        }
        ToolDef::Other(value) => {
            if let Some(obj) = value.as_object() {
                if let Some(name) = obj.get("name").and_then(|v| v.as_str()) {
                    scan_text(name, acc);
                }
                if let Some(desc) = obj.get("description").and_then(|v| v.as_str()) {
                    scan_text(desc, acc);
                }
            }
        }
    }
}

/// Token delimiters in addition to ASCII whitespace. Splitting on these keeps
/// matches whole-token: a uuid wrapped in quotes or parens still matches, but
/// a substring of a longer identifier never does. Deliberately EXCLUDES
/// characters that occur INSIDE the target patterns -- `:` `.` `-` `_` `+`
/// `/` `=` -- because RFC3339 timestamps carry `:` `.` `+` `-`, JWTs carry
/// `.` `-` `_` `=`, and uuids/hex carry `-`. Splitting on those would shatter
/// a real volatile token into fragments and miss the match. The design note's
/// delimiter sketch listed `:` among others; using it verbatim would break
/// timestamp detection, so the colon (and the other in-pattern characters)
/// are excluded here -- precision of the whole-token match wins.
const fn is_delimiter(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            ',' | ';' | '"' | '\'' | '(' | ')' | '{' | '}' | '[' | ']' | '<' | '>' | '|' | '`'
        )
}

/// Tokenize on whitespace + common delimiters and classify each whole token.
/// Whole-token matching (never substring) is what keeps prose at None.
fn scan_text(text: &str, acc: &mut Accumulator) {
    for raw in text.split(is_delimiter) {
        // Trim sentence-edge punctuation that is never a valid terminal char
        // of any target pattern, so a volatile token ending (or opening) a
        // sentence -- `...446655440000.`, `token:`, `id!` -- still matches.
        // Interior characters and the in-pattern chars (`-` `_` `+` `/` `=`)
        // are left untouched; only the edges are stripped.
        let token = raw.trim_matches(|c| matches!(c, '.' | ',' | ';' | ':' | '!' | '?'));
        if token.is_empty() {
            continue;
        }
        classify_token(token, acc);
    }
}

fn classify_token(token: &str, acc: &mut Accumulator) {
    // HIGH tiers first: any single match vetoes.
    if is_uuid_v4(token) {
        acc.record_high(VolatileKind::Uuid);
        return;
    }
    if is_rfc3339_timestamp(token) {
        acc.record_high(VolatileKind::Timestamp);
        return;
    }
    if is_jwt(token) {
        acc.record_high(VolatileKind::Jwt);
        return;
    }
    if is_hex_blob_high(token) {
        acc.record_high(VolatileKind::HexBlob);
        return;
    }

    // LOW tiers: warn-only, never veto.
    if is_iso_date_only(token) || is_hex_blob_low(token) || is_long_digit_run(token) {
        acc.record_low();
    }
}

/// UUIDv4: canonical 8-4-4-4-12 lowercase/uppercase hex with the version
/// nibble `4` and the variant nibble in {8,9,a,b}. The token must equal the
/// UUID exactly (36 chars including the four hyphens).
fn is_uuid_v4(token: &str) -> bool {
    if token.len() != 36 {
        return false;
    }
    let bytes = token.as_bytes();
    // Hyphen positions in 8-4-4-4-12.
    const HYPHENS: [usize; 4] = [8, 13, 18, 23];
    for (i, &b) in bytes.iter().enumerate() {
        if HYPHENS.contains(&i) {
            if b != b'-' {
                return false;
            }
        } else if !b.is_ascii_hexdigit() {
            return false;
        }
    }
    // Version nibble at index 14 must be '4'.
    if bytes[14] != b'4' {
        return false;
    }
    // Variant nibble at index 19 must be in {8,9,a,b}.
    matches!(bytes[19].to_ascii_lowercase(), b'8' | b'9' | b'a' | b'b')
}

/// RFC3339 timestamp: full `YYYY-MM-DDThh:mm:ss` with optional fractional
/// seconds (`.` followed by 1+ digits) and a `Z` or `+-hh:mm` offset. A bare
/// date with no time is NOT high (see `is_iso_date_only`).
fn is_rfc3339_timestamp(token: &str) -> bool {
    let bytes = token.as_bytes();
    // Minimum: "YYYY-MM-DDThh:mm:ss" == 19 chars.
    if bytes.len() < 19 {
        return false;
    }
    // Date portion: YYYY-MM-DD
    if !all_digits(&bytes[0..4])
        || bytes[4] != b'-'
        || !all_digits(&bytes[5..7])
        || bytes[7] != b'-'
        || !all_digits(&bytes[8..10])
    {
        return false;
    }
    // 'T' or 't' separator.
    if !matches!(bytes[10], b'T' | b't') {
        return false;
    }
    // Time portion: hh:mm:ss
    if !all_digits(&bytes[11..13])
        || bytes[13] != b':'
        || !all_digits(&bytes[14..16])
        || bytes[16] != b':'
        || !all_digits(&bytes[17..19])
    {
        return false;
    }

    let mut idx = 19;
    // Optional fractional seconds: '.' then 1+ digits.
    if idx < bytes.len() && bytes[idx] == b'.' {
        idx += 1;
        let frac_start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        if idx == frac_start {
            return false;
        }
    }

    // Offset: 'Z'/'z' OR +-hh:mm. Required (a naive local time is not RFC3339).
    if idx >= bytes.len() {
        return false;
    }
    match bytes[idx] {
        b'Z' | b'z' => idx + 1 == bytes.len(),
        b'+' | b'-' => {
            // +-hh:mm
            idx + 6 == bytes.len()
                && all_digits(&bytes[idx + 1..idx + 3])
                && bytes[idx + 3] == b':'
                && all_digits(&bytes[idx + 4..idx + 6])
        }
        _ => false,
    }
}

/// JWT: three `.`-separated base64url segments where segment 0 base64url-
/// decodes to UTF-8 JSON containing the substring `"alg"`. The header sniff
/// avoids matching arbitrary `a.b.c` prose with two dots.
fn is_jwt(token: &str) -> bool {
    let mut parts = token.split('.');
    let (Some(header), Some(payload), Some(signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    if header.is_empty() || payload.is_empty() || signature.is_empty() {
        return false;
    }
    if !is_base64url(header) || !is_base64url(payload) || !is_base64url(signature) {
        return false;
    }
    let Some(decoded) = base64url_decode(header) else {
        return false;
    };
    let Ok(text) = std::str::from_utf8(&decoded) else {
        return false;
    };
    text.contains("\"alg\"")
}

/// Hex blob (HIGH): a token that is entirely hex and at least 32 chars.
fn is_hex_blob_high(token: &str) -> bool {
    token.len() >= 32 && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Hex blob (LOW): a token that is entirely hex and 16-31 chars. A short hex
/// word like `deadbeef` (8 chars) stays None.
fn is_hex_blob_low(token: &str) -> bool {
    let len = token.len();
    (16..32).contains(&len) && token.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Bare ISO date `YYYY-MM-DD` with no time component (LOW).
fn is_iso_date_only(token: &str) -> bool {
    let bytes = token.as_bytes();
    bytes.len() == 10
        && all_digits(&bytes[0..4])
        && bytes[4] == b'-'
        && all_digits(&bytes[5..7])
        && bytes[7] == b'-'
        && all_digits(&bytes[8..10])
}

/// Pure-digit run of 16 or more digits (LOW) -- e.g. a long numeric id.
fn is_long_digit_run(token: &str) -> bool {
    token.len() >= 16 && token.bytes().all(|b| b.is_ascii_digit())
}

fn all_digits(bytes: &[u8]) -> bool {
    !bytes.is_empty() && bytes.iter().all(u8::is_ascii_digit)
}

/// True iff every byte is a valid base64url character (`A-Za-z0-9-_`), with
/// optional trailing `=` padding. Empty is rejected by the caller.
fn is_base64url(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut seen_pad = false;
    for &b in bytes {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' => {
                if seen_pad {
                    return false;
                }
            }
            b'=' => seen_pad = true,
            _ => return false,
        }
    }
    true
}

/// Decode a base64url segment (no external dependency; the header is small).
/// Returns `None` on malformed input. Padding `=` is tolerated and ignored.
fn base64url_decode(s: &str) -> Option<Vec<u8>> {
    let mut bits: u32 = 0;
    let mut nbits: u32 = 0;
    let mut out = Vec::new();
    for &b in s.as_bytes() {
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        bits = (bits << 6) | val;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{ChatRequest, Message, MessageContent, Role};
    use crate::system_content::{SystemBlock, SystemContent};
    use crate::tool_def::{CustomTool, ToolDef};
    use serde_json::json;

    fn req_with_system(text: &str) -> ChatRequest {
        ChatRequest {
            model: "test-model".into(),
            system: Some(SystemContent::Text(text.into())),
            ..Default::default()
        }
    }

    #[test]
    fn high_confidence_veto_on_uuid_v4_in_system_prompt() {
        // Arrange
        let req = req_with_system("Session id: 550e8400-e29b-41d4-a716-446655440000 active.");

        // Act
        let report = scan_volatile(&req);

        // Assert
        assert!(report.is_high_confidence_veto());
        assert_eq!(report.confidence(), VolatileConfidence::High);
        assert_eq!(report.kinds(), &[VolatileKind::Uuid]);
    }

    #[test]
    fn high_confidence_veto_on_rfc3339_timestamp_in_system_prompt() {
        // Arrange
        let req = req_with_system("Generated at 2026-06-18T14:30:00Z for this run.");

        // Act
        let report = scan_volatile(&req);

        // Assert
        assert!(report.is_high_confidence_veto());
        assert_eq!(report.kinds(), &[VolatileKind::Timestamp]);
    }

    #[test]
    fn high_confidence_veto_on_rfc3339_with_offset_and_fraction() {
        // Arrange
        let req = req_with_system("ts 2026-06-18T14:30:00.123+05:30 here");

        // Act
        let report = scan_volatile(&req);

        // Assert
        assert!(report.is_high_confidence_veto());
        assert_eq!(report.kinds(), &[VolatileKind::Timestamp]);
    }

    #[test]
    fn high_confidence_veto_on_three_segment_jwt_in_system_prompt() {
        // Arrange
        // Fabricated, non-functional JWT: a real {"alg":"HS256","typ":"JWT"}
        // base64url header (which is all is_jwt sniffs) followed by a
        // deliberately non-eyJ payload/signature, so it exercises the detector
        // without being a scannable token. Do not "restore" a realistic
        // payload -- secret scanners flag eyJ.eyJ. JWT shapes.
        let jwt = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.cGF5bG9hZA.c2ln";
        let req = req_with_system(&format!("Bearer {jwt} must be present"));

        // Act
        let report = scan_volatile(&req);

        // Assert
        assert!(report.is_high_confidence_veto());
        assert_eq!(report.kinds(), &[VolatileKind::Jwt]);
    }

    #[test]
    fn high_confidence_veto_on_hex_blob_32_plus_in_system_prompt() {
        // Arrange
        let req = req_with_system("checksum a1b2c3d4e5f60718293a4b5c6d7e8f90 set");

        // Act
        let report = scan_volatile(&req);

        // Assert
        assert!(report.is_high_confidence_veto());
        assert_eq!(report.kinds(), &[VolatileKind::HexBlob]);
    }

    #[test]
    fn high_confidence_veto_from_token_in_tool_description() {
        // Arrange: a custom tool whose description carries a fresh uuid.
        let tool = ToolDef::Custom(CustomTool {
            name: "fetch".into(),
            description: Some(
                "Fetch record 550e8400-e29b-41d4-a716-446655440000 from store.".into(),
            ),
            input_schema: json!({"type": "object"}),
            cache_control: None,
            defer_loading: None,
            strict: None,
            type_tag: None,
        });
        let req = ChatRequest {
            model: "test-model".into(),
            tools: Some(vec![tool]),
            ..Default::default()
        };

        // Act
        let report = scan_volatile(&req);

        // Assert
        assert!(report.is_high_confidence_veto());
        assert_eq!(report.kinds(), &[VolatileKind::Uuid]);
    }

    #[test]
    fn high_confidence_veto_from_token_in_other_tool_name_field() {
        // Arrange: an Other-variant tool with a volatile token in its name.
        let tool = ToolDef::Other(json!({
            "type": "web_search_20250901",
            "name": "search a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "description": "ordinary builtin"
        }));
        let req = ChatRequest {
            model: "test-model".into(),
            tools: Some(vec![tool]),
            ..Default::default()
        };

        // Act
        let report = scan_volatile(&req);

        // Assert
        assert!(report.is_high_confidence_veto());
        assert_eq!(report.kinds(), &[VolatileKind::HexBlob]);
    }

    #[test]
    fn realistic_prose_system_prompt_scores_none() {
        // Arrange: multi-line authored boilerplate with a date-in-a-sentence
        // and a short hex word that must NOT trip HIGH.
        let prompt = "You are a careful assistant.\n\
            As of June 2026 you should cite sources where possible.\n\
            Avoid speculation. The debug marker deadbeef is a known sentinel.\n\
            Always answer in plain prose and keep responses concise.\n\
            When unsure, say so rather than guessing.";
        let req = req_with_system(prompt);

        // Act
        let report = scan_volatile(&req);

        // Assert
        assert_eq!(report.confidence(), VolatileConfidence::None);
        assert!(!report.is_high_confidence_veto());
        assert!(report.kinds().is_empty());
    }

    #[test]
    fn bare_iso_date_scores_low_not_veto() {
        // Arrange
        let req = req_with_system("Effective date 2026-06-18 for the policy.");

        // Act
        let report = scan_volatile(&req);

        // Assert
        assert_eq!(report.confidence(), VolatileConfidence::Low);
        assert!(!report.is_high_confidence_veto());
        assert!(report.kinds().is_empty());
    }

    #[test]
    fn twenty_char_hex_scores_low_not_veto() {
        // Arrange: 20 hex chars -> LOW tier (16-31 range).
        let req = req_with_system("ref a1b2c3d4e5f6a7b8c9d0 here");

        // Act
        let report = scan_volatile(&req);

        // Assert
        assert_eq!(report.confidence(), VolatileConfidence::Low);
        assert!(!report.is_high_confidence_veto());
    }

    #[test]
    fn volatile_token_in_last_user_message_only_scores_none() {
        // Arrange: a uuid in the final user message, nothing in the prefix.
        let req = ChatRequest {
            model: "test-model".into(),
            system: Some(SystemContent::Text("You are helpful.".into())),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text(
                    "Look up 550e8400-e29b-41d4-a716-446655440000 please.".into(),
                ),
                reasoning: None,
                reasoning_details: Vec::new(),
                name: None,
                tool_call_id: None,
                tool_calls: None,
                refusal: None,
            }],
            ..Default::default()
        };

        // Act
        let report = scan_volatile(&req);

        // Assert
        assert_eq!(report.confidence(), VolatileConfidence::None);
        assert!(!report.is_high_confidence_veto());
    }

    #[test]
    fn empty_system_and_no_tools_scores_none() {
        // Arrange
        let req = ChatRequest {
            model: "test-model".into(),
            ..Default::default()
        };

        // Act
        let report = scan_volatile(&req);

        // Assert
        assert_eq!(report.confidence(), VolatileConfidence::None);
        assert!(!report.is_high_confidence_veto());
    }

    #[test]
    fn high_confidence_veto_when_token_appears_in_system_block() {
        // Arrange: a Blocks-form system with a uuid in one block's text.
        let req = ChatRequest {
            model: "test-model".into(),
            system: Some(SystemContent::Blocks(vec![
                SystemBlock {
                    kind: "text".into(),
                    text: "Stable boilerplate header.".into(),
                    cache_control: None,
                    citations: None,
                },
                SystemBlock {
                    kind: "text".into(),
                    text: "run 550e8400-e29b-41d4-a716-446655440000".into(),
                    cache_control: None,
                    citations: None,
                },
            ])),
            ..Default::default()
        };

        // Act
        let report = scan_volatile(&req);

        // Assert
        assert!(report.is_high_confidence_veto());
        assert_eq!(report.kinds(), &[VolatileKind::Uuid]);
    }

    #[test]
    fn kinds_are_deduplicated_in_first_seen_order() {
        // Arrange: two uuids and a hex blob; uuid recorded once, then hex.
        let req = req_with_system(
            "a 550e8400-e29b-41d4-a716-446655440000 b 6ba7b810-9dad-41d1-80b4-00c04fd430c8 \
             c a1b2c3d4e5f60718293a4b5c6d7e8f90",
        );

        // Act
        let report = scan_volatile(&req);

        // Assert
        assert!(report.is_high_confidence_veto());
        assert_eq!(report.kinds(), &[VolatileKind::Uuid, VolatileKind::HexBlob]);
    }

    #[test]
    fn arbitrary_three_dot_token_is_not_jwt() {
        // Arrange: prose with two dots must not be mistaken for a JWT.
        let req = req_with_system("Use the a.b.c notation for nested keys.");

        // Act
        let report = scan_volatile(&req);

        // Assert
        assert_eq!(report.confidence(), VolatileConfidence::None);
    }

    #[test]
    fn scan_does_not_mutate_request() -> Result<(), Box<dyn std::error::Error>> {
        // Arrange: build a request, clone it, scan the original.
        let original = ChatRequest {
            model: "test-model".into(),
            system: Some(SystemContent::Text(
                "Session 550e8400-e29b-41d4-a716-446655440000 at 2026-06-18T14:30:00Z".into(),
            )),
            tools: Some(vec![ToolDef::Custom(CustomTool {
                name: "fetch".into(),
                description: Some("Fetch a record by id.".into()),
                input_schema: json!({"type": "object"}),
                cache_control: None,
                defer_loading: None,
                strict: None,
                type_tag: None,
            })]),
            ..Default::default()
        };
        let before = serde_json::to_value(&original)?;

        // Act
        let _ = scan_volatile(&original);

        // Assert: serialized form is byte-identical after the scan.
        let after = serde_json::to_value(&original)?;
        assert_eq!(before, after);
        Ok(())
    }

    #[test]
    fn trailing_sentence_punctuation_does_not_hide_high_tokens() {
        // A volatile token that ends a sentence must still veto after
        // edge-punctuation trimming (regression for the missed-on-period case).
        for prompt in [
            "Session 550e8400-e29b-41d4-a716-446655440000.",
            "checksum a1b2c3d4e5f60718293a4b5c6d7e8f90!",
            "generated at 2026-06-18T14:30:00Z,",
        ] {
            let report = scan_volatile(&req_with_system(prompt));
            assert!(
                report.is_high_confidence_veto(),
                "edge-punctuation token must still veto: {prompt:?}"
            );
        }
    }

    #[test]
    fn hex_thirty_one_is_low_thirty_two_is_high() {
        let hex31: String = "a".repeat(31);
        let r31 = scan_volatile(&req_with_system(&format!("ref {hex31} end")));
        assert_eq!(r31.confidence(), VolatileConfidence::Low);
        assert!(!r31.is_high_confidence_veto());

        let hex32: String = "a".repeat(32);
        let r32 = scan_volatile(&req_with_system(&format!("ref {hex32} end")));
        assert!(r32.is_high_confidence_veto());
        assert_eq!(r32.kinds(), &[VolatileKind::HexBlob]);
    }

    #[test]
    fn sixteen_digit_run_scores_low() {
        let report = scan_volatile(&req_with_system("acct 1234567890123456 on file"));
        assert_eq!(report.confidence(), VolatileConfidence::Low);
        assert!(!report.is_high_confidence_veto());
    }

    #[test]
    fn timestamp_without_offset_is_not_high() {
        // RFC3339 requires an offset; a naive local datetime must not veto.
        let report = scan_volatile(&req_with_system("ts 2026-06-18T14:30:00 here"));
        assert!(!report.is_high_confidence_veto());
        assert_eq!(report.confidence(), VolatileConfidence::None);
    }
}
