//! Redaction of credential- and principal-shaped substrings from upstream
//! error text before it reaches a client envelope or a log line.
//!
//! Every matcher runs over DECODED string values (an `error.message` after
//! JSON decoding), never raw JSON text, where an escape can spell any
//! marker. Matching is hand-rolled over bytes: every marker is ASCII, so a
//! byte offset at a marker is always a char boundary.

use std::borrow::Cow;
use std::ops::Range;

/// The one replacement every match gets.
const PLACEHOLDER: &str = "[redacted]";

/// Length of an AWS account id.
const ACCOUNT_ID_DIGITS: usize = 12;

/// No-break space, skipped as an account-id separator.
const NBSP: &[u8] = "\u{a0}".as_bytes();

/// Replace every credential- or principal-shaped substring of `s` with
/// `[redacted]`. Deterministic; borrows `s` unchanged when nothing matches.
///
/// Redacted shapes:
/// - an AWS ARN starting a token, plain (`arn:aws:`, `arn:aws-<partition>:`)
///   or percent-encoded (`arn%3aaws`, any case), with `:` or `%3a` after
///   `aws` / the partition in either form, through the end of the token;
/// - a 12-digit AWS account id written after the word `account` (see
///   [`names_aws_principal`]), the digits only;
/// - a vendor credential keyed on its prefix: `sk-ant-api03-`,
///   `sk-ant-oat01-`, `sk-ant-ort01-`, `sk-ant-admin01-`, `sk-proj-`,
///   `sk-svcacct-`, `sk-or-v1-`, `ghp_`, `AKIA` (16+ body chars, anywhere);
///   and, each starting a token, `sk-admin-` (16+), `ASIA` (16 of
///   `A-Z0-9`), `AIza` (35), `ya29.` with an optional 1-3 digit legacy
///   segment (20+), a three-segment `eyJ` JWT (10+ per segment), `nvapi-`
///   (20+), `bedrock-api-key-` (20+) and `ABSK` (100+ base64);
/// - the userinfo of a URL (`scheme://user:pass@` or `scheme://token@`),
///   keeping the scheme and the `@`.
pub fn scrub_upstream_text(s: &str) -> Cow<'_, str> {
    let bytes = s.as_bytes();
    let mut out: Option<String> = None;
    let mut copied = 0;
    let mut at = 0;
    while at < bytes.len() {
        let Some(span) = match_at(bytes, at) else {
            at += 1;
            continue;
        };
        let buf = out.get_or_insert_with(|| String::with_capacity(s.len()));
        buf.push_str(&s[copied..span.start]);
        buf.push_str(PLACEHOLDER);
        copied = span.end;
        at = span.end.max(at + 1);
    }
    match out {
        None => Cow::Borrowed(s),
        Some(mut buf) => {
            buf.push_str(&s[copied..]);
            Cow::Owned(buf)
        }
    }
}

/// True when `text` names an AWS principal or account in an AWS shape: an
/// AWS ARN starting a token, plain (`arn:aws:` or a partition variant
/// `arn:aws-<x>:`) or percent-encoded (`arn%3aaws`, any case, then `:` or
/// `%3a`), or a
/// 12-digit account id after the word `account` (any case), optionally
/// followed by the word `id` or `number` (any case). Between `account`, the
/// optional word and the digits, only these separators are skipped: space,
/// tab, no-break space (U+00A0), `:`, `=`, `#`, `_`, `-`, `"`, `'`, `(` and
/// `)`. A loose marker (a word ending in `arn:`, a long number such as a
/// timestamp, an 11- or 13-digit run) is not one.
pub fn names_aws_principal(text: &str) -> bool {
    let bytes = text.as_bytes();
    (0..bytes.len()).any(|at| arn_at(bytes, at).is_some() || account_id_at(bytes, at).is_some())
}

/// The span to redact for a match beginning at `at`, if any rule matches.
fn match_at(bytes: &[u8], at: usize) -> Option<Range<usize>> {
    arn_at(bytes, at)
        .or_else(|| account_id_at(bytes, at))
        .or_else(|| token_at(bytes, at))
        .or_else(|| userinfo_at(bytes, at))
}

const fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// True when position `at` begins a token: nothing before it, a byte outside
/// `[A-Za-z0-9_-]`, or a complete `%XX` escape (an encoded delimiter).
fn starts_token(bytes: &[u8], at: usize) -> bool {
    if at == 0 || !is_token_char(bytes[at - 1]) {
        return true;
    }
    at >= 3
        && bytes[at - 3] == b'%'
        && bytes[at - 2].is_ascii_hexdigit()
        && bytes[at - 1].is_ascii_hexdigit()
}

fn has_prefix_ci(bytes: &[u8], at: usize, prefix: &[u8]) -> bool {
    bytes
        .get(at..at + prefix.len())
        .is_some_and(|got| got.eq_ignore_ascii_case(prefix))
}

/// Length of the run at `at` whose bytes satisfy `class`.
fn run_len(bytes: &[u8], at: usize, class: impl Fn(u8) -> bool) -> usize {
    bytes
        .get(at..)
        .map_or(0, |rest| rest.iter().take_while(|&&b| class(b)).count())
}

/// An AWS ARN, plain or percent-encoded, starting a token at `at`; the span
/// runs to the end of the token.
fn arn_at(bytes: &[u8], at: usize) -> Option<Range<usize>> {
    if !starts_token(bytes, at) {
        return None;
    }
    let after_head = plain_arn_head(bytes, at).or_else(|| encoded_arn_head(bytes, at))?;
    Some(at..after_head + run_len(bytes, after_head, |b| !ends_arn(b)))
}

/// End of `arn:aws` / `arn:aws-<partition>` and its separator at `at`.
fn plain_arn_head(bytes: &[u8], at: usize) -> Option<usize> {
    let rest = at + "arn:aws".len();
    if bytes.get(at..rest)? != b"arn:aws" {
        return None;
    }
    after_arn_separator(bytes, after_partition(bytes, rest)?)
}

/// End of `arn%3aaws` / `arn%3aaws-<partition>` (any case) and its
/// separator at `at`.
fn encoded_arn_head(bytes: &[u8], at: usize) -> Option<usize> {
    let rest = at + "arn%3aaws".len();
    if !has_prefix_ci(bytes, at, b"arn%3aaws") {
        return None;
    }
    after_arn_separator(bytes, after_partition(bytes, rest)?)
}

/// Past a `:` or `%3a` (any case) at `at`: either spelling may follow
/// either form of the head, since a partly encoded ARN is still one.
fn after_arn_separator(bytes: &[u8], at: usize) -> Option<usize> {
    if bytes.get(at) == Some(&b':') {
        Some(at + 1)
    } else {
        has_prefix_ci(bytes, at, b"%3a").then_some(at + 3)
    }
}

/// Skips an optional `-<partition>` at `at`; `None` for a bare `-`.
fn after_partition(bytes: &[u8], at: usize) -> Option<usize> {
    if bytes.get(at) != Some(&b'-') {
        return Some(at);
    }
    let len = run_len(bytes, at + 1, |b| b.is_ascii_alphanumeric() || b == b'-');
    (len > 0).then_some(at + 1 + len)
}

const fn ends_arn(b: u8) -> bool {
    b.is_ascii_whitespace()
        || b.is_ascii_control()
        || matches!(
            b,
            b'"' | b'\'' | b'`' | b'<' | b'>' | b',' | b')' | b']' | b'}'
        )
}

/// A 12-digit account id after `account` at `at`; the span is the digits.
fn account_id_at(bytes: &[u8], at: usize) -> Option<Range<usize>> {
    if !has_prefix_ci(bytes, at, b"account") {
        return None;
    }
    let mut pos = skip_account_separators(bytes, at + "account".len());
    if let Some(word) = [b"id".as_slice(), b"number"]
        .into_iter()
        .find(|word| has_prefix_ci(bytes, pos, word))
    {
        pos = skip_account_separators(bytes, pos + word.len());
    }
    let digits = run_len(bytes, pos, |b| b.is_ascii_digit());
    (digits == ACCOUNT_ID_DIGITS).then_some(pos..pos + digits)
}

/// Skips the separators [`names_aws_principal`] documents.
fn skip_account_separators(bytes: &[u8], mut at: usize) -> usize {
    loop {
        match bytes.get(at) {
            Some(b' ' | b'\t' | b':' | b'=' | b'#' | b'_' | b'-' | b'"' | b'\'' | b'(' | b')') => {
                at += 1;
            }
            _ if bytes.get(at..at + NBSP.len()) == Some(NBSP) => at += NBSP.len(),
            _ => return at,
        }
    }
}

/// A prefix-keyed credential and the body class and floor its prefix takes.
struct TokenRule {
    prefix: &'static [u8],
    anchored: bool,
    body: fn(u8) -> bool,
    min_body: usize,
}

const fn opaque(b: u8) -> bool {
    is_token_char(b)
}

const fn upper_alnum(b: u8) -> bool {
    b.is_ascii_uppercase() || b.is_ascii_digit()
}

const fn bedrock_key_char(b: u8) -> bool {
    is_token_char(b) || b == b'=' || b == b'&'
}

const fn base64_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'+' || b == b'/'
}

const fn rule(
    prefix: &'static [u8],
    anchored: bool,
    body: fn(u8) -> bool,
    min: usize,
) -> TokenRule {
    TokenRule {
        prefix,
        anchored,
        body,
        min_body: min,
    }
}

const TOKEN_RULES: &[TokenRule] = &[
    rule(b"sk-ant-api03-", false, opaque, 16),
    rule(b"sk-ant-oat01-", false, opaque, 16),
    rule(b"sk-ant-ort01-", false, opaque, 16),
    rule(b"sk-ant-admin01-", false, opaque, 16),
    rule(b"sk-proj-", false, opaque, 16),
    rule(b"sk-svcacct-", false, opaque, 16),
    rule(b"sk-or-v1-", false, opaque, 16),
    rule(b"ghp_", false, opaque, 16),
    rule(b"AKIA", false, opaque, 16),
    rule(b"sk-admin-", true, opaque, 16),
    rule(b"ASIA", true, upper_alnum, 16),
    rule(b"AIza", true, opaque, 35),
    rule(b"nvapi-", true, opaque, 20),
    rule(b"bedrock-api-key-", true, bedrock_key_char, 20),
];

/// A prefix-keyed credential at `at`; the span is prefix plus body run.
fn token_at(bytes: &[u8], at: usize) -> Option<Range<usize>> {
    TOKEN_RULES
        .iter()
        .find_map(|rule| prefixed_token_at(bytes, at, rule))
        .or_else(|| google_oauth_at(bytes, at))
        .or_else(|| jwt_at(bytes, at))
        .or_else(|| bedrock_long_term_key_at(bytes, at))
}

fn prefixed_token_at(bytes: &[u8], at: usize, rule: &TokenRule) -> Option<Range<usize>> {
    if !bytes.get(at..)?.starts_with(rule.prefix) || (rule.anchored && !starts_token(bytes, at)) {
        return None;
    }
    let body_at = at + rule.prefix.len();
    let body = run_len(bytes, body_at, rule.body);
    (body >= rule.min_body).then_some(at..body_at + body)
}

/// `ya29.` plus an optional `<1-3 digits>.` legacy segment, then 20+ opaque.
fn google_oauth_at(bytes: &[u8], at: usize) -> Option<Range<usize>> {
    if !bytes.get(at..)?.starts_with(b"ya29.") || !starts_token(bytes, at) {
        return None;
    }
    let mut body_at = at + "ya29.".len();
    let digits = run_len(bytes, body_at, |b| b.is_ascii_digit());
    if (1..=3).contains(&digits) && bytes.get(body_at + digits) == Some(&b'.') {
        body_at += digits + 1;
    }
    let body = run_len(bytes, body_at, opaque);
    (body >= 20).then_some(at..body_at + body)
}

/// `eyJ` then three `.`-separated opaque segments of 10+ each, the first
/// counted after `eyJ`.
fn jwt_at(bytes: &[u8], at: usize) -> Option<Range<usize>> {
    if !bytes.get(at..)?.starts_with(b"eyJ") || !starts_token(bytes, at) {
        return None;
    }
    let mut end = at + "eyJ".len();
    for segment in 0..3 {
        if segment > 0 {
            if bytes.get(end) != Some(&b'.') {
                return None;
            }
            end += 1;
        }
        let len = run_len(bytes, end, opaque);
        if len < 10 {
            return None;
        }
        end += len;
    }
    Some(at..end)
}

/// `ABSK` then 100+ base64 characters and up to two `=`.
fn bedrock_long_term_key_at(bytes: &[u8], at: usize) -> Option<Range<usize>> {
    if !bytes.get(at..)?.starts_with(b"ABSK") || !starts_token(bytes, at) {
        return None;
    }
    let body_at = at + "ABSK".len();
    let body = run_len(bytes, body_at, base64_char);
    if body < 100 {
        return None;
    }
    let pad = run_len(bytes, body_at + body, |b| b == b'=').min(2);
    Some(at..body_at + body + pad)
}

/// The userinfo of a URL whose `://` sits at `at`: everything after `://`
/// up to the LAST `@` before the authority ends (the WHATWG URL parser's
/// split). The span is the userinfo alone.
fn userinfo_at(bytes: &[u8], at: usize) -> Option<Range<usize>> {
    if !bytes.get(at..)?.starts_with(b"://") {
        return None;
    }
    let start = at + "://".len();
    let authority = run_len(bytes, start, |b| !ends_authority(b));
    let last_at = bytes[start..start + authority]
        .iter()
        .rposition(|&b| b == b'@')?;
    (last_at > 0).then_some(start..start + last_at)
}

const fn ends_authority(b: u8) -> bool {
    b.is_ascii_whitespace()
        || b.is_ascii_control()
        || matches!(b, b'/' | b'?' | b'#' | b'"' | b'<' | b'>')
}

#[cfg(test)]
#[path = "upstream_scrub_tests.rs"]
mod tests;
