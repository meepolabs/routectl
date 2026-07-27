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

use crate::cache_control::{BreakpointPosition, compute_frozen_floor};
use crate::content_part::{ContentPart, KnownContentPart};
use crate::schema::{ChatRequest, Message, MessageContent};
use crate::system_content::SystemContent;
use crate::tool_def::ToolDef;

/// Confidence that the scanned stable prefix carries per-request-volatile
/// content. Only `High` vetoes auto-caching; `Low` is warn-only and `None`
/// is the prose default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum VolatileConfidence {
    /// No volatile content detected (the prose default).
    None,
    /// Low-confidence signal; warn-only, does not veto auto-caching.
    Low,
    /// High-confidence signal; vetoes auto-caching.
    High,
}

/// Which high-precision volatile pattern matched. Recorded only for HIGH
/// matches so callers and logs can see what tripped the veto.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum VolatileKind {
    /// A UUID token.
    Uuid,
    /// A timestamp token.
    Timestamp,
    /// A JSON Web Token.
    Jwt,
    /// A long hexadecimal blob (e.g. a hash or nonce).
    HexBlob,
}

impl VolatileKind {
    /// Stable, log-safe token naming this kind. Carries no raw value.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Uuid => "uuid",
            Self::Timestamp => "timestamp",
            Self::Jwt => "jwt",
            Self::HexBlob => "hex_blob",
        }
    }
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
    /// The scan's overall confidence verdict.
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

/// Which structural component of the caller-cached prefix a WARN-tier
/// advisory finding sits in. Ordered by cache-prefix position (tools first).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum PrefixComponent {
    /// Tool definitions (name + description).
    Tools,
    /// System prompt text.
    System,
    /// Leading message text inside the caller-cached region.
    Messages,
}

impl PrefixComponent {
    /// Stable, log-safe token naming this component.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tools => "tools",
            Self::System => "system",
            Self::Messages => "messages",
        }
    }
}

/// One WARN-tier advisory finding: a high-precision volatile token sits in a
/// component of the region the CALLER marked cacheable. Carries only the
/// structural facts (never the raw value): which component, which kind, and
/// the caller's final breakpoint position.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CallerPrefixFinding {
    component: PrefixComponent,
    kind: VolatileKind,
    breakpoint_position: BreakpointPosition,
}

impl CallerPrefixFinding {
    /// The structural component the volatile token was found in.
    pub const fn component(&self) -> PrefixComponent {
        self.component
    }

    /// The high-precision volatile kind that matched.
    pub const fn kind(&self) -> VolatileKind {
        self.kind
    }

    /// The caller's final (deepest) cache breakpoint position, which bounds
    /// the scanned region.
    pub const fn breakpoint_position(&self) -> BreakpointPosition {
        self.breakpoint_position
    }
}

/// Result of the advisory pass over the caller-cached prefix. Constructor-
/// only: build it through `scan_caller_prefix_advisory`, read it through
/// `findings`.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct CallerPrefixAdvisory {
    findings: Vec<CallerPrefixFinding>,
}

impl CallerPrefixAdvisory {
    /// The advisory findings, one per (component, high-precision kind) in
    /// first-seen order. Empty when the caller supplied no breakpoints or no
    /// high-precision volatile token sits in the cached region.
    pub fn findings(&self) -> &[CallerPrefixFinding] {
        &self.findings
    }
}

/// WARN-tier, READ-ONLY advisory pass over the region the CALLER marked
/// cacheable: content at or before the caller's final `cache_control`
/// breakpoint (tools, system, and leading messages up to that marker).
///
/// This is DISTINCT from `scan_volatile`, which drives the auto-emit veto
/// over routectl's own always-stable prefix (system + tools only) and whose
/// scope is deliberately fixed. This pass is advisory-only: it never vetoes,
/// never mutates, and fires ONLY when the caller supplied at least one
/// breakpoint (auto-emitted breakpoints are routectl's own and are exempt --
/// callers compute this off the ORIGINAL request, before any auto-emit
/// injection).
///
/// Only the high-precision whole-token detectors (UUID, RFC3339 timestamp,
/// JWT, long hex blob) surface a finding; counter-shaped LOW signals are not
/// reported, so a WARN never trips on prose or example ids.
///
/// PURE: borrows `req` read-only, returns an owned advisory, mutates nothing.
#[must_use]
pub fn scan_caller_prefix_advisory(req: &ChatRequest) -> CallerPrefixAdvisory {
    let floor = compute_frozen_floor(req);
    // The deepest caller breakpoint bounds the cached region. `positions` is
    // in cache-prefix order (tools -> system -> messages -> top-level), so
    // its last entry is the deepest. Empty means no caller breakpoint.
    let Some(&final_pos) = floor.positions().last() else {
        return CallerPrefixAdvisory {
            findings: Vec::new(),
        };
    };

    let mut findings = Vec::new();

    // Tools are the frontmost prefix element. If the deepest marker IS a tool
    // marker, only tools up to and including it are cached; a deeper marker
    // (system/messages/top-level) caches the whole tools array.
    if let Some(tools) = &req.tools {
        let take = if final_pos == BreakpointPosition::Tools {
            last_marked_tool(tools).map_or(0, |i| i + 1)
        } else {
            tools.len()
        };
        let mut acc = Accumulator::new();
        for tool in tools.iter().take(take) {
            scan_tool(tool, &mut acc);
        }
        push_findings(PrefixComponent::Tools, &acc, final_pos, &mut findings);
    }

    // System sits after tools: cached only when the deepest marker reaches
    // System or deeper. A System-position marker caps the scan at the last
    // marked block (a per-block marker requires the Blocks form); a deeper
    // marker caches the whole system prefix.
    if final_pos >= BreakpointPosition::System {
        let mut acc = Accumulator::new();
        match req.system.as_ref() {
            Some(SystemContent::Blocks(blocks)) => {
                let take = if final_pos == BreakpointPosition::System {
                    last_marked_system_block(blocks).map_or(0, |i| i + 1)
                } else {
                    blocks.len()
                };
                for b in blocks.iter().take(take) {
                    scan_text(&b.text, &mut acc);
                }
            }
            // Flat system text carries no per-block marker, so it can only be
            // cached by a deeper (messages/top-level) marker, never a
            // System-position one.
            Some(SystemContent::Text(s)) if final_pos > BreakpointPosition::System => {
                scan_text(s, &mut acc);
            }
            _ => {}
        }
        push_findings(PrefixComponent::System, &acc, final_pos, &mut findings);
    }

    // Messages sit after system: cached only when the deepest marker reaches
    // Messages or a top-level marker. A top-level marker freezes the whole
    // message list; a Messages-position marker caches every message before the
    // last marked one in full, plus that message up to and including its last
    // marked part -- content after the marker is uncached and unscanned.
    if final_pos >= BreakpointPosition::Messages {
        let mut acc = Accumulator::new();
        if final_pos == BreakpointPosition::TopLevel {
            for m in &*req.messages {
                scan_message_all_text(m, &mut acc);
            }
        } else if let Some(last) = last_marked_message(req) {
            for m in req.messages.iter().take(last) {
                scan_message_all_text(m, &mut acc);
            }
            scan_message_up_to_last_marker(&req.messages[last], &mut acc);
        }
        push_findings(PrefixComponent::Messages, &acc, final_pos, &mut findings);
    }

    CallerPrefixAdvisory { findings }
}

/// Index of the last tool carrying a caller `cache_control` marker.
fn last_marked_tool(tools: &[ToolDef]) -> Option<usize> {
    tools
        .iter()
        .enumerate()
        .filter(|(_, t)| t.cache_control().is_some())
        .map(|(i, _)| i)
        .next_back()
}

/// Index of the last system block carrying a caller `cache_control` marker.
fn last_marked_system_block(blocks: &[crate::system_content::SystemBlock]) -> Option<usize> {
    blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| b.cache_control.is_some())
        .map(|(i, _)| i)
        .next_back()
}

/// Index of the last message carrying a caller `cache_control` marker.
fn last_marked_message(req: &ChatRequest) -> Option<usize> {
    req.messages
        .iter()
        .enumerate()
        .filter(|(_, m)| message_has_caller_marker(m))
        .map(|(i, _)| i)
        .next_back()
}

/// Whether any content part of `m` carries a caller `cache_control` marker.
fn message_has_caller_marker(m: &Message) -> bool {
    match &m.content {
        MessageContent::Parts(parts) => parts.iter().any(|p| p.cache_control().is_some()),
        MessageContent::Text(_) | MessageContent::Null => false,
    }
}

/// Scan every text surface of a fully-cached message (flat text and `text`
/// blocks). Structured payloads (tool inputs/results, images, documents) are
/// intentionally not scanned: like schemas, they routinely carry example
/// ids/dates that would false-positive.
fn scan_message_all_text(m: &Message, acc: &mut Accumulator) {
    match &m.content {
        MessageContent::Text(s) => scan_text(s, acc),
        MessageContent::Parts(parts) => {
            for p in parts {
                scan_part_text(p, acc);
            }
        }
        MessageContent::Null => {}
    }
}

/// Scan a partially-cached message: text of every part up to AND INCLUDING the
/// last part carrying a caller marker. Parts after that marker are outside the
/// cached region.
fn scan_message_up_to_last_marker(m: &Message, acc: &mut Accumulator) {
    if let MessageContent::Parts(parts) = &m.content {
        let last = parts
            .iter()
            .enumerate()
            .filter(|(_, p)| p.cache_control().is_some())
            .map(|(i, _)| i)
            .next_back();
        if let Some(last) = last {
            for p in parts.iter().take(last + 1) {
                scan_part_text(p, acc);
            }
        }
    }
}

/// Scan the text of a single content part (only `text` blocks carry scannable
/// prose).
fn scan_part_text(p: &ContentPart, acc: &mut Accumulator) {
    if let ContentPart::Known(KnownContentPart::Text { text, .. }) = p {
        scan_text(text, acc);
    }
}

/// Record one advisory finding per HIGH-precision kind the accumulator saw
/// (its `kinds` set holds only HIGH matches, first-seen deduplicated). LOW
/// signals leave `kinds` empty and produce no finding.
fn push_findings(
    component: PrefixComponent,
    acc: &Accumulator,
    breakpoint_position: BreakpointPosition,
    findings: &mut Vec<CallerPrefixFinding>,
) {
    for &kind in &acc.kinds {
        findings.push(CallerPrefixFinding {
            component,
            kind,
            breakpoint_position,
        });
    }
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
            }]
            .into(),
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

    // -- caller-prefix advisory pass ---------------------------------------

    use crate::cache_control::CacheControl;
    use crate::content_part::{ContentPart, KnownContentPart};

    fn part_text(text: &str, cc: Option<CacheControl>) -> ContentPart {
        ContentPart::Known(KnownContentPart::Text {
            text: text.into(),
            citations: None,
            cache_control: cc,
        })
    }

    fn user_msg(parts: Vec<ContentPart>) -> Message {
        Message {
            role: Role::User,
            content: MessageContent::Parts(parts),
            reasoning: None,
            reasoning_details: Vec::new(),
            name: None,
            tool_call_id: None,
            tool_calls: None,
            refusal: None,
        }
    }

    const UUID: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[test]
    fn advisory_no_caller_breakpoints_is_empty() {
        // An auto-emit-only request (zero caller breakpoints) never warns,
        // even with a volatile token sitting in the system prefix.
        let req = req_with_system(&format!("Session {UUID} active."));
        let advisory = scan_caller_prefix_advisory(&req);
        assert!(advisory.findings().is_empty());
    }

    #[test]
    fn advisory_warns_on_volatile_before_message_breakpoint() {
        // Two messages: the first carries a volatile uuid AND the caller's
        // cache_control marker, so it sits inside the cached region.
        let req = ChatRequest {
            model: "test-model".into(),
            messages: vec![
                user_msg(vec![part_text(
                    &format!("context {UUID}"),
                    Some(CacheControl::ephemeral_5m()),
                )]),
                user_msg(vec![part_text("later turn", None)]),
            ]
            .into(),
            ..Default::default()
        };

        let advisory = scan_caller_prefix_advisory(&req);
        let findings = advisory.findings();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].component(), PrefixComponent::Messages);
        assert_eq!(findings[0].kind(), VolatileKind::Uuid);
        assert_eq!(
            findings[0].breakpoint_position(),
            crate::cache_control::BreakpointPosition::Messages
        );
    }

    #[test]
    fn advisory_does_not_warn_on_volatile_after_the_breakpoint() {
        // The marker sits on message 0; the volatile uuid is in message 1,
        // AFTER the cached region, so it must not warn.
        let req = ChatRequest {
            model: "test-model".into(),
            messages: vec![
                user_msg(vec![part_text(
                    "stable header",
                    Some(CacheControl::ephemeral_5m()),
                )]),
                user_msg(vec![part_text(&format!("fresh {UUID}"), None)]),
            ]
            .into(),
            ..Default::default()
        };

        let advisory = scan_caller_prefix_advisory(&req);
        assert!(advisory.findings().is_empty());
    }

    #[test]
    fn advisory_top_level_marker_scans_all_messages() {
        // A top-level caller marker freezes the whole prefix, so a volatile
        // token in any message warns.
        let req = ChatRequest {
            model: "test-model".into(),
            messages: vec![
                user_msg(vec![part_text("stable header", None)]),
                user_msg(vec![part_text(&format!("fresh {UUID}"), None)]),
            ]
            .into(),
            cache_control: Some(CacheControl::ephemeral_5m()),
            ..Default::default()
        };

        let advisory = scan_caller_prefix_advisory(&req);
        let findings = advisory.findings();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].component(), PrefixComponent::Messages);
        assert_eq!(findings[0].kind(), VolatileKind::Uuid);
    }

    #[test]
    fn advisory_warns_on_system_when_system_breakpoint_present() {
        // A system-block caller marker caches tools + system; a volatile
        // token in the system prefix warns.
        let req = ChatRequest {
            model: "test-model".into(),
            system: Some(SystemContent::Blocks(vec![SystemBlock {
                kind: "text".into(),
                text: format!("run at 2026-06-18T14:30:00Z ({UUID})"),
                cache_control: Some(CacheControl::ephemeral_5m()),
                citations: None,
            }])),
            messages: vec![user_msg(vec![part_text("hi", None)])].into(),
            ..Default::default()
        };

        let advisory = scan_caller_prefix_advisory(&req);
        let kinds: Vec<_> = advisory.findings().iter().map(|f| f.kind()).collect();
        assert!(
            advisory
                .findings()
                .iter()
                .all(|f| f.component() == PrefixComponent::System)
        );
        assert!(kinds.contains(&VolatileKind::Uuid));
        assert!(kinds.contains(&VolatileKind::Timestamp));
    }

    #[test]
    fn advisory_tools_only_breakpoint_does_not_scan_system() {
        // A tools-only caller marker caches tools but NOT system; a volatile
        // token that lives only in system must not warn.
        let req = ChatRequest {
            model: "test-model".into(),
            system: Some(SystemContent::Text(format!("session {UUID}"))),
            tools: Some(vec![ToolDef::Custom(CustomTool {
                name: "calc".into(),
                description: Some("adds numbers".into()),
                input_schema: json!({"type": "object"}),
                cache_control: Some(CacheControl::ephemeral_5m()),
                defer_loading: None,
                strict: None,
                type_tag: None,
            })]),
            messages: vec![user_msg(vec![part_text("hi", None)])].into(),
            ..Default::default()
        };

        let advisory = scan_caller_prefix_advisory(&req);
        assert!(advisory.findings().is_empty());
    }

    #[test]
    fn advisory_does_not_warn_on_part_after_marker_in_marked_message() {
        // Within the marked message, the marker sits on part 0; a volatile
        // token in part 1 (after the marker) is outside the cached region.
        let req = ChatRequest {
            model: "test-model".into(),
            messages: vec![user_msg(vec![
                part_text("stable header", Some(CacheControl::ephemeral_5m())),
                part_text(&format!("fresh {UUID}"), None),
            ])]
            .into(),
            ..Default::default()
        };

        let advisory = scan_caller_prefix_advisory(&req);
        assert!(advisory.findings().is_empty());
    }

    #[test]
    fn advisory_warns_on_part_at_or_before_marker_in_marked_message() {
        // The marker sits on part 1; the volatile token in part 0 is at-or-
        // before it, inside the cached region.
        let req = ChatRequest {
            model: "test-model".into(),
            messages: vec![user_msg(vec![
                part_text(&format!("context {UUID}"), None),
                part_text("stable tail", Some(CacheControl::ephemeral_5m())),
            ])]
            .into(),
            ..Default::default()
        };

        let advisory = scan_caller_prefix_advisory(&req);
        let findings = advisory.findings();
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].component(), PrefixComponent::Messages);
        assert_eq!(findings[0].kind(), VolatileKind::Uuid);
    }

    #[test]
    fn advisory_does_not_warn_on_tool_after_the_marked_tool() {
        // The deepest (and only) marker is on tool 0; a volatile token in
        // tool 1 is after it and outside the cached region.
        let marked = ToolDef::Custom(CustomTool {
            name: "calc".into(),
            description: Some("adds numbers".into()),
            input_schema: json!({"type": "object"}),
            cache_control: Some(CacheControl::ephemeral_5m()),
            defer_loading: None,
            strict: None,
            type_tag: None,
        });
        let later = ToolDef::Custom(CustomTool {
            name: "lookup".into(),
            description: Some(format!("fetch {UUID}")),
            input_schema: json!({"type": "object"}),
            cache_control: None,
            defer_loading: None,
            strict: None,
            type_tag: None,
        });
        let req = ChatRequest {
            model: "test-model".into(),
            tools: Some(vec![marked, later]),
            messages: vec![user_msg(vec![part_text("hi", None)])].into(),
            ..Default::default()
        };

        let advisory = scan_caller_prefix_advisory(&req);
        assert!(advisory.findings().is_empty());
    }

    #[test]
    fn advisory_does_not_mutate_request() -> Result<(), Box<dyn std::error::Error>> {
        let req = ChatRequest {
            model: "test-model".into(),
            system: Some(SystemContent::Blocks(vec![SystemBlock {
                kind: "text".into(),
                text: format!("run {UUID}"),
                cache_control: Some(CacheControl::ephemeral_5m()),
                citations: None,
            }])),
            messages: vec![user_msg(vec![part_text(
                &format!("more {UUID}"),
                Some(CacheControl::ephemeral_5m()),
            )])]
            .into(),
            ..Default::default()
        };
        let before = serde_json::to_value(&req)?;

        let _ = scan_caller_prefix_advisory(&req);

        let after = serde_json::to_value(&req)?;
        assert_eq!(before, after);
        Ok(())
    }
}
