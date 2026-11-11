//! Redaction of credential- and principal-shaped substrings from upstream
//! error text before it reaches a client error envelope.
//!
//! Callers pass DECODED string values (an `error.message` after JSON
//! decoding), never raw JSON text. One decode can still leave escapes: a
//! gateway nests another provider's JSON body inside its own message, and
//! a signing error echoes a canonical request whose path is percent-encoded
//! twice. So a JSON escape (`\n`, `\u0022`, `\"`, `\/`) or a `%XX` /
//! `%25XX` escape counts as a delimiter, and the encoded spellings of the
//! ARN markers match. Matching is hand-rolled over bytes: every marker is
//! ASCII, so a byte offset at a marker is always a char boundary.
//!
//! Log sinks render upstream text through [`upstream_text_for_log`] (or
//! [`scrub_upstream_value`] for a decoded body), so the scrub sees the
//! complete text before any control filter or length cap can cut a
//! credential short of its rule.

use std::borrow::Cow;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::ops::Range;

use crate::log_safe::{body_key_looks_credential, sanitize_for_log};

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
/// - an AWS ARN starting a token, plain (`arn:aws:`, `arn:aws-<partition>:`),
///   percent-encoded (`arn%3aaws`) or encoded twice (`arn%253aaws`), any
///   case, with `:`, `%3a` or `%253a` after `aws` / the partition in any
///   form, through the end of the token;
/// - a 12-digit AWS account id written after the word `account` (see
///   [`names_aws_principal`]), the digits only;
/// - a vendor credential keyed on its prefix: `sk-ant-api03-`,
///   `sk-ant-oat01-`, `sk-ant-ort01-`, `sk-ant-admin01-`, `sk-proj-`,
///   `sk-svcacct-`, `sk-or-v1-`, `ghp_`, `AKIA` (16+ body chars, anywhere);
///   and, each starting a token, `sk-admin-` (16+), `ASIA` (16 of
///   `A-Z0-9`), `AIza` (35), `ya29.` with an optional 1-3 digit legacy
///   segment (20+), a three-segment `eyJ` JWT (10+ per segment), `nvapi-`
///   (20+), `bedrock-api-key-` (20+), `ABSK` (100+ base64), `sk-None-`
///   (16+), and `sk-` plus an opaque run carrying `T3BlbkFJ` with 8+ run
///   chars before it and 16+ after;
/// - the value of an echoed `x-amz-security-token` (any case; then an
///   optional `"`, `'` or `\"`; `:` or `=`; optional spaces or tabs; an
///   optional `"`, `'` or `\"`; then 20+ of `[A-Za-z0-9+/=_%-]`), keeping
///   the name;
/// - the userinfo after any `://` (or JSON-escaped `:\/\/`), keeping what
///   precedes it and the `@`. No scheme is required: the text is displayed,
///   not parsed, so `://user:pass@` is redacted whatever stands before it.
///   The authority runs to `/` (so to an escaped `\/`), never to a bare `\`, so
///   a domain user such as `CORP\alice:pw@` is userinfo;
/// - the opaque token after the word `bearer` (any case, starting a token)
///   and spaces or tabs: a run of `[A-Za-z0-9._~+/-]` and trailing `=` of
///   6+ characters carrying a digit, or of 20+ characters, keeping the
///   word, so prose such as `bearer of bad news` stays;
/// - the value of a credential-named key (as judged for request bodies:
///   `api_key`, `secret`, `password`, `authorization`, a name ending in
///   `token` but not `tokens`, and so on; a name of 129+ characters counts
///   as one), keeping the key. The tokenizer-symbol keys `eos_token`,
///   `bos_token`, `pad_token`, `unk_token`, `stop_token`, `sep_token`,
///   `cls_token` and `mask_token` (whole, or as the last dotted segment)
///   are not credential-named. The key, of `[A-Za-z0-9_.-]`, is either
///   quoted with `"` or `'` (each optionally backslash-escaped, any depth)
///   then `:` or `=` and a quoted value, with optional spaces or tabs around
///   the separator; or bare (starting a token, so after `?`, `&`, a space
///   and so on) then `=`. A bare key directly followed by `=` takes a quoted
///   value or, unquoted, the run up to `&`, `,`, `;`, `)`, `}`, whitespace,
///   a control character or a quote (a query or form pair, or a `k=v` list
///   such as a Python repr or a Java `toString`), crossing whole any URL
///   authority after a `://` inside it; a bare key with spaces before `=`
///   takes a quoted value only, and a bare key of 129+ characters is not
///   judged (a base64 run before its `=` padding is no key). Prose such as `unexpected token: '}'` and an
///   unquoted `max_tokens: 5` stay. A value already `[redacted]` is left
///   alone. A quoted value runs to the matching closing quote (one carrying
///   no more backslashes than the opening one; after an unescaped opening
///   quote, one preceded by an even run of backslashes), or else to the
///   next control character or the end of the text.
///
/// Every rule sees every byte not already inside a redacted span, so a
/// credential-shaped key before its value is redacted too. A span that ends
/// at a delimiter in free text (an ARN, an echoed session-token value, the
/// token after `bearer`, an unquoted key value) is seen inside as well, so a
/// credential starting in it and running past that delimiter is redacted to
/// its own end.
#[must_use = "the returned text carries required redactions"]
pub fn scrub_upstream_text(s: &str) -> Cow<'_, str> {
    let spans = redaction_spans(s.as_bytes());
    if spans.is_empty() {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut copied = 0;
    for span in spans {
        out.push_str(&s[copied..span.start]);
        out.push_str(PLACEHOLDER);
        copied = span.end;
    }
    out.push_str(&s[copied..]);
    Cow::Owned(out)
}

/// Every span to redact, in order, overlapping spans merged. Every rule
/// probes every byte up to and including the start of each span found: a
/// span that starts past its probe position (a value after its key, digits
/// after `account`) waits in `ahead` while the bytes before it are probed,
/// so a credential-shaped key is still matched, and its first byte is
/// probed too, so a credential filling a value is matched to its own end.
/// The rest of a span is probed only when the span is [`Span::probed`].
fn redaction_spans(bytes: &[u8]) -> Vec<Range<usize>> {
    let mut found = Vec::new();
    let mut ahead: BinaryHeap<Reverse<Span>> = BinaryHeap::new();
    let mut runs = RunCaches::default();
    let mut at = 0;
    while at < bytes.len() {
        let mut next = at + 1;
        while let Some(&Reverse(span)) = ahead.peek()
            && span.start <= at
        {
            ahead.pop();
            next = next.max(span.resume_at());
            found.push(span.range());
        }
        for span in match_at(bytes, at, &mut runs).into_iter().flatten() {
            if span.start > at {
                ahead.push(Reverse(span));
            } else {
                next = next.max(span.resume_at());
                found.push(span.range());
            }
        }
        at = next;
    }
    found.extend(ahead.into_iter().map(|Reverse(span)| span.range()));
    merge_overlapping(found)
}

/// A span to redact and whether its inside is probed. A span is probed when
/// its end is a delimiter in free text that another credential may run
/// across. A span ending at a quote, at the `@` closing userinfo, at the end
/// of a vendor credential's own alphabet, or after a fixed digit count is
/// skipped: no other rule's alphabet runs across those ends, and skipping
/// keeps a long or repeated match linear.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Span {
    start: usize,
    end: usize,
    is_probed: bool,
}

impl Span {
    const fn skipped(range: Range<usize>) -> Self {
        Self {
            start: range.start,
            end: range.end,
            is_probed: false,
        }
    }

    const fn probed(range: Range<usize>) -> Self {
        Self {
            start: range.start,
            end: range.end,
            is_probed: true,
        }
    }

    const fn range(self) -> Range<usize> {
        self.start..self.end
    }

    /// The first byte probed after this span starts.
    const fn resume_at(self) -> usize {
        if self.is_probed { self.start } else { self.end }
    }
}

/// The end of the last run each probed-span rule measured. A rule matching
/// again inside its own run reuses that end instead of rescanning, which
/// keeps probing inside a long chain (`arn:aws:...arn:aws:...`,
/// `a_token=a_token=...`) linear.
#[derive(Default)]
struct RunCaches {
    arn: RunCache,
    session_token: RunCache,
    form_value: RunCache,
}

#[derive(Default)]
struct RunCache {
    run: Range<usize>,
}

impl RunCache {
    /// End of the run starting at `from`: the cached end when `from` lies
    /// inside the cached run, else `measure(from)`, cached.
    fn end_from(&mut self, from: usize, measure: impl FnOnce(usize) -> usize) -> usize {
        if !self.run.contains(&from) {
            self.run = from..measure(from);
        }
        self.run.end
    }
}

fn merge_overlapping(mut spans: Vec<Range<usize>>) -> Vec<Range<usize>> {
    spans.sort_unstable_by_key(|span| span.start);
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.start < last.end => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    merged
}

/// Log-line rendering of upstream-authored text: the complete text is
/// scrubbed with [`scrub_upstream_text`] first, then control-filtered and
/// capped by [`sanitize_for_log`]. Pass the raw text, never an excerpt
/// already capped or filtered, or a credential cut at the cap escapes its
/// rule.
#[must_use = "the returned text is the log-safe rendering"]
pub fn upstream_text_for_log(s: &str) -> String {
    sanitize_for_log(&scrub_upstream_text(s))
}

/// Applies [`scrub_upstream_text`] to a decoded JSON value in place,
/// redacting what the text scrub redacts in the same value serialized:
/// every string leaf and every object key is scrubbed (a key the scrub
/// rewrites is replaced; a rewritten key that would collide with another
/// key takes the first free `#2`, `#3`, ... suffix, so no entry is lost),
/// and a leaf is judged with its key as context. A non-empty string directly
/// under a credential-named key (the key rule of [`scrub_upstream_text`])
/// becomes `[redacted]`, and a 12-digit account id leading a string or
/// number directly under a key containing `account` (any case) is redacted,
/// a number becoming a string. Booleans, nulls and leaves the scrub leaves
/// unchanged are not rewritten.
pub fn scrub_upstream_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => {
            if let Cow::Owned(scrubbed) = scrub_upstream_text(text) {
                *text = scrubbed;
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(scrub_upstream_value),
        serde_json::Value::Object(map) => scrub_object(map),
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn scrub_object(map: &mut serde_json::Map<String, serde_json::Value>) {
    for (key, child) in map.iter_mut() {
        scrub_upstream_value(child);
        scrub_keyed_leaf(key, child);
    }
    if !map
        .keys()
        .any(|key| matches!(scrub_upstream_text(key), Cow::Owned(_)))
    {
        return;
    }
    let mut rewritten = Vec::new();
    for (key, child) in std::mem::take(map) {
        match scrub_upstream_text(&key) {
            Cow::Owned(scrubbed) => rewritten.push((scrubbed, child)),
            Cow::Borrowed(_) => {
                map.insert(key, child);
            }
        }
    }
    let mut next_suffix = HashMap::new();
    for (scrubbed, child) in rewritten {
        let key = unused_key(map, &mut next_suffix, scrubbed);
        map.insert(key, child);
    }
}

/// `key`, else the first of `key#2`, `key#3`, ... that `map` does not hold.
/// `next_suffix` remembers where each key's search stopped, so a run of
/// colliding keys stays linear.
fn unused_key(
    map: &serde_json::Map<String, serde_json::Value>,
    next_suffix: &mut HashMap<String, usize>,
    key: String,
) -> String {
    if !map.contains_key(&key) {
        return key;
    }
    let suffix = next_suffix.entry(key.clone()).or_insert(2);
    loop {
        let candidate = format!("{key}#{suffix}");
        *suffix += 1;
        if !map.contains_key(&candidate) {
            return candidate;
        }
    }
}

/// Redacts what the text scrub finds in a leaf only with its key before it.
fn scrub_keyed_leaf(key: &str, leaf: &mut serde_json::Value) {
    if quoted_key_names_credential(key.as_bytes()) {
        if let serde_json::Value::String(text) = leaf
            && !text.is_empty()
            && text != PLACEHOLDER
        {
            PLACEHOLDER.clone_into(text);
        }
        return;
    }
    if !contains_ci(key.as_bytes(), b"account") {
        return;
    }
    match leaf {
        serde_json::Value::String(text) => {
            redact_leading_account_id(text);
        }
        serde_json::Value::Number(number) => {
            let mut text = number.to_string();
            if redact_leading_account_id(&mut text) {
                *leaf = serde_json::Value::String(text);
            }
        }
        _ => {}
    }
}

/// True when `key` would be judged credential-named between quotes: every
/// byte a key byte, and the name credential-shaped.
fn quoted_key_names_credential(key: &[u8]) -> bool {
    !key.is_empty() && key.iter().all(|&b| is_key_char(b)) && key_names_credential(key)
}

fn contains_ci(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

/// Redacts an account id leading `text` as if `account` stood before it;
/// true when it did.
fn redact_leading_account_id(text: &mut String) -> bool {
    let Some(digits) = account_digits_from(text.as_bytes(), 0) else {
        return false;
    };
    text.replace_range(digits, PLACEHOLDER);
    true
}

/// True when `text` names an AWS principal or account in an AWS shape: an
/// AWS ARN starting a token, plain (`arn:aws:` or a partition variant
/// `arn:aws-<x>:`), percent-encoded (`arn%3aaws`) or encoded twice
/// (`arn%253aaws`), any case, then `:`, `%3a` or `%253a`; or a 12-digit
/// account id after the word `account` (any case), optionally followed by
/// the word `id` or `number` (any case). Between `account`, the optional
/// word and the digits, only these separators are skipped: space, tab,
/// no-break space (U+00A0), `:`, `=`, `#`, `_`, `-`, `"`, `'`, `(`, `)`,
/// `\`, the JSON escape `\t`, and the JSON escapes `\u0020`, `\u0009`,
/// `\u00a0`, `\u0022`, `\u0027`, `\u0028` and `\u0029` (any hex case).
/// A loose marker (a word ending in `arn:`, a long number such as a
/// timestamp, an 11- or 13-digit run) is not one.
#[must_use]
pub fn names_aws_principal(text: &str) -> bool {
    let bytes = text.as_bytes();
    (0..bytes.len()).any(|at| arn_at(bytes, at).is_some() || account_id_at(bytes, at).is_some())
}

/// The span each rule redacts for a match probed at `at`. Every rule is
/// evaluated, so one rule matching never hides another's match at the same
/// position. A span may start past `at` (a value after its key).
fn match_at(bytes: &[u8], at: usize, runs: &mut RunCaches) -> [Option<Span>; 7] {
    [
        arn_span_at(bytes, at, &mut runs.arn).map(Span::probed),
        account_id_at(bytes, at).map(Span::skipped),
        token_at(bytes, at).map(Span::skipped),
        session_token_span_at(bytes, at, &mut runs.session_token).map(Span::probed),
        userinfo_at(bytes, at).map(Span::skipped),
        bearer_token_at(bytes, at).map(Span::probed),
        credential_key_value_at(bytes, at, &mut runs.form_value),
    ]
}

const fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// True when position `at` begins a token: nothing before it, a byte outside
/// `[A-Za-z0-9_-]`, or an escaped delimiter ending at `at`.
fn starts_token(bytes: &[u8], at: usize) -> bool {
    if at == 0 || !is_token_char(bytes[at - 1]) {
        return true;
    }
    let before = &bytes[..at];
    ends_percent_escape(before) || ends_json_escape(before)
}

/// True when `before` ends in `%XX` or `%25XX` (a delimiter encoded once
/// or twice).
fn ends_percent_escape(before: &[u8]) -> bool {
    match before {
        [.., b'%', b'2', b'5', x, y] | [.., b'%', x, y] => {
            x.is_ascii_hexdigit() && y.is_ascii_hexdigit()
        }
        _ => false,
    }
}

/// True when `before` ends in a JSON escape that leaves a letter or digit
/// last: `\b`, `\f`, `\n`, `\r`, `\t` or `\uXXXX`.
fn ends_json_escape(before: &[u8]) -> bool {
    match before {
        [.., b'\\', b'u', a, b, c, d] if [a, b, c, d].iter().all(|h| h.is_ascii_hexdigit()) => true,
        [.., b'\\', last] => matches!(last, b'b' | b'f' | b'n' | b'r' | b't'),
        _ => false,
    }
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
    arn_span_at(bytes, at, &mut RunCache::default())
}

fn arn_span_at(bytes: &[u8], at: usize, run: &mut RunCache) -> Option<Range<usize>> {
    if !starts_token(bytes, at) {
        return None;
    }
    let after_head = plain_arn_head(bytes, at).or_else(|| encoded_arn_head(bytes, at))?;
    let end = run.end_from(after_head, |from| {
        from + run_len(bytes, from, |b| !ends_arn(b))
    });
    Some(at..end)
}

/// End of `arn:aws` / `arn:aws-<partition>` and its separator at `at`.
fn plain_arn_head(bytes: &[u8], at: usize) -> Option<usize> {
    if !has_prefix_ci(bytes, at, b"arn:aws") {
        return None;
    }
    after_arn_separator(bytes, after_partition(bytes, at + "arn:aws".len())?)
}

/// End of `arn%3aaws` / `arn%253aaws` (any case), an optional
/// `-<partition>`, and its separator at `at`.
fn encoded_arn_head(bytes: &[u8], at: usize) -> Option<usize> {
    let head = [b"arn%3aaws".as_slice(), b"arn%253aaws"]
        .into_iter()
        .find(|head| has_prefix_ci(bytes, at, head))?;
    after_arn_separator(bytes, after_partition(bytes, at + head.len())?)
}

/// Past a `:`, `%3a` or `%253a` (any case) at `at`: any spelling may follow
/// any form of the head, since a partly encoded ARN is still one.
fn after_arn_separator(bytes: &[u8], at: usize) -> Option<usize> {
    [b":".as_slice(), b"%3a", b"%253a"]
        .into_iter()
        .find(|sep| has_prefix_ci(bytes, at, sep))
        .map(|sep| at + sep.len())
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
    account_digits_from(bytes, at + "account".len())
}

/// The account-id digits after the word `account` ends at `from`: the
/// separators, an optional `id` / `number`, the separators, then exactly
/// 12 digits.
fn account_digits_from(bytes: &[u8], from: usize) -> Option<Range<usize>> {
    let mut pos = skip_account_separators(bytes, from);
    if let Some(word) = [b"id".as_slice(), b"number"]
        .into_iter()
        .find(|word| has_prefix_ci(bytes, pos, word))
    {
        pos = skip_account_separators(bytes, pos + word.len());
    }
    let digits = run_len(bytes, pos, |b| b.is_ascii_digit());
    (digits == ACCOUNT_ID_DIGITS).then_some(pos..pos + digits)
}

/// JSON `\u` escapes of the account-id separators, lowercase hex.
const ESCAPED_ACCOUNT_SEPARATORS: [&[u8]; 7] = [
    b"\\u0020", b"\\u0009", b"\\u00a0", b"\\u0022", b"\\u0027", b"\\u0028", b"\\u0029",
];

/// Skips the separators [`names_aws_principal`] documents.
fn skip_account_separators(bytes: &[u8], mut at: usize) -> usize {
    loop {
        if bytes.get(at..at + 2) == Some(b"\\t") {
            at += 2;
            continue;
        }
        if let Some(escape) = ESCAPED_ACCOUNT_SEPARATORS
            .iter()
            .find(|escape| has_prefix_ci(bytes, at, escape))
        {
            at += escape.len();
            continue;
        }
        match bytes.get(at) {
            Some(
                b' ' | b'\t' | b':' | b'=' | b'#' | b'_' | b'-' | b'"' | b'\'' | b'(' | b')'
                | b'\\',
            ) => at += 1,
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
    rule(b"sk-None-", true, opaque, 16),
];

/// The base64 of `OpenAI`, carried inside older OpenAI user keys.
const OPENAI_KEY_INFIX: &[u8] = b"T3BlbkFJ";
const OPENAI_INFIX_MIN_BEFORE: usize = 8;
const OPENAI_INFIX_MIN_AFTER: usize = 16;

/// A prefix-keyed credential at `at`; the span is prefix plus body run.
fn token_at(bytes: &[u8], at: usize) -> Option<Range<usize>> {
    TOKEN_RULES
        .iter()
        .find_map(|rule| prefixed_token_at(bytes, at, rule))
        .or_else(|| google_oauth_at(bytes, at))
        .or_else(|| jwt_at(bytes, at))
        .or_else(|| bedrock_long_term_key_at(bytes, at))
        .or_else(|| openai_infix_key_at(bytes, at))
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

/// `sk-` starting a token, then an opaque run holding [`OPENAI_KEY_INFIX`]
/// at some position with 8+ run chars before it and 16+ after; the span is
/// the whole run.
fn openai_infix_key_at(bytes: &[u8], at: usize) -> Option<Range<usize>> {
    if !bytes.get(at..)?.starts_with(b"sk-") || !starts_token(bytes, at) {
        return None;
    }
    let run_at = at + "sk-".len();
    let run = &bytes[run_at..run_at + run_len(bytes, run_at, opaque)];
    let carries_infix = run
        .windows(OPENAI_KEY_INFIX.len())
        .enumerate()
        .any(|(i, window)| {
            window == OPENAI_KEY_INFIX
                && i >= OPENAI_INFIX_MIN_BEFORE
                && run.len() - i - OPENAI_KEY_INFIX.len() >= OPENAI_INFIX_MIN_AFTER
        });
    carries_infix.then_some(at..run_at + run.len())
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

/// Minimum length of an echoed session-token value.
const SESSION_TOKEN_MIN: usize = 20;

/// The value of an echoed `x-amz-security-token` header or query parameter
/// whose name starts at `at`; the span is the value alone.
fn session_token_span_at(bytes: &[u8], at: usize, run: &mut RunCache) -> Option<Range<usize>> {
    const NAME: &[u8] = b"x-amz-security-token";
    if !has_prefix_ci(bytes, at, NAME) || !starts_token(bytes, at) {
        return None;
    }
    let sep = after_optional_quote(bytes, at + NAME.len());
    if !matches!(bytes.get(sep), Some(b':' | b'=')) {
        return None;
    }
    let value_at = sep + 1 + run_len(bytes, sep + 1, |b| b == b' ' || b == b'\t');
    let start = after_optional_quote(bytes, value_at);
    let end = run.end_from(start, |from| {
        from + run_len(bytes, from, |b| {
            b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'_' | b'%' | b'-')
        })
    });
    (end - start >= SESSION_TOKEN_MIN).then_some(start..end)
}

/// Past an optional `"`, `'` or `\"` at `at`.
fn after_optional_quote(bytes: &[u8], at: usize) -> usize {
    match bytes.get(at..) {
        Some([b'\\', b'"', ..]) => at + 2,
        Some([b'"' | b'\'', ..]) => at + 1,
        _ => at,
    }
}

/// The userinfo after a `://` (or JSON-escaped `:\/\/`) at `at`:
/// everything after the slashes up to the LAST `@` before the authority
/// ends. The span is the userinfo alone.
fn userinfo_at(bytes: &[u8], at: usize) -> Option<Range<usize>> {
    let start = after_url_slashes(bytes, at)?;
    let authority = run_len(bytes, start, |b| !ends_authority(b));
    let last_at = bytes[start..start + authority]
        .iter()
        .rposition(|&b| b == b'@')?;
    (last_at > 0).then_some(start..start + last_at)
}

/// Past a `://` (or JSON-escaped `:\/\/`) at `at`.
fn after_url_slashes(bytes: &[u8], at: usize) -> Option<usize> {
    [b"://".as_slice(), b":\\/\\/"]
        .into_iter()
        .find(|slashes| {
            bytes
                .get(at..)
                .is_some_and(|rest| rest.starts_with(slashes))
        })
        .map(|slashes| at + slashes.len())
}

/// A bare backslash is not here: it stays inside the authority, and an
/// escaped `\/` still ends at its `/`.
const fn ends_authority(b: u8) -> bool {
    b.is_ascii_whitespace()
        || b.is_ascii_control()
        || matches!(b, b'/' | b'?' | b'#' | b'"' | b'<' | b'>')
}

/// Longest key judged by name. A longer quoted key is treated as
/// credential-named, which keeps the judgment off the heap; a longer bare
/// key is no key at all (no query or form key runs that long, while a
/// base64 run before its `=` padding does).
const MAX_JUDGED_KEY_LEN: usize = 128;

/// Model-vocabulary keys whose names end in `token` but whose values are
/// tokenizer symbols, not credentials.
const VOCABULARY_TOKEN_KEYS: [&[u8]; 8] = [
    b"eos_token",
    b"bos_token",
    b"pad_token",
    b"unk_token",
    b"stop_token",
    b"sep_token",
    b"cls_token",
    b"mask_token",
];

/// The value of a credential-named key starting at `at` (at its opening
/// quote, or at its first character when bare); the span is the value
/// alone. A start just after a backslash, or a bare key just after a `.`,
/// is skipped: the run's own start already judged the same key, and
/// skipping keeps a long run of backslashes or dotted segments linear.
fn credential_key_value_at(bytes: &[u8], at: usize, form_run: &mut RunCache) -> Option<Span> {
    let prev = at.checked_sub(1).map(|i| bytes[i]);
    if prev == Some(b'\\') {
        return None;
    }
    let opening = quote_at(bytes, at);
    let key_at = opening.map_or(at, |quote| quote.end);
    if opening.is_none() && (prev == Some(b'.') || !starts_token(bytes, at)) {
        return None;
    }
    let key_len = run_len(bytes, key_at, is_key_char);
    if key_len == 0 || (opening.is_none() && key_len > MAX_JUDGED_KEY_LEN) {
        return None;
    }
    let key = &bytes[key_at..key_at + key_len];
    let mut pos = key_at + key_len;
    if let Some(open) = opening {
        let close = quote_at(bytes, pos).filter(|close| close.mark == open.mark)?;
        pos = close.end;
    }
    if !key_names_credential(key) {
        return None;
    }
    let value = if opening.is_none() && bytes.get(pos) == Some(&b'=') {
        form_value(bytes, pos + 1, form_run)
    } else {
        Span::skipped(quoted_value_after_separator(bytes, pos, opening.is_some())?)
    };
    let text = &bytes[value.range()];
    (!text.is_empty() && text != PLACEHOLDER.as_bytes()).then_some(value)
}

/// The quoted value after blanks, a `:` (quoted keys only) or `=`, and
/// blanks, starting at `pos`.
fn quoted_value_after_separator(
    bytes: &[u8],
    pos: usize,
    quoted_key: bool,
) -> Option<Range<usize>> {
    let sep = pos + run_len(bytes, pos, is_blank);
    match bytes.get(sep) {
        Some(b'=') => {}
        Some(b':') if quoted_key => {}
        _ => return None,
    }
    let value_quote = quote_at(bytes, sep + 1 + run_len(bytes, sep + 1, is_blank))?;
    Some(quoted_value(bytes, &value_quote))
}

/// A query or form value at `at`: a quoted value, else the unquoted run
/// [`form_run_end`] measures, which ends at a delimiter another credential
/// may run across, so its inside is probed.
fn form_value(bytes: &[u8], at: usize, run: &mut RunCache) -> Span {
    if let Some(open) = quote_at(bytes, at) {
        return Span::skipped(quoted_value(bytes, &open));
    }
    Span::probed(at..run.end_from(at, |from| form_run_end(bytes, from)))
}

/// End of the unquoted form value at `at`: the run up to `&`, `,`, `;`,
/// `)`, `}`, whitespace, a control character or a quote (bare or
/// backslash-escaped). A URL authority after a `://` (or `:\/\/`) in the
/// run is crossed whole, so userinfo holding one of those delimiters stays
/// inside the value.
fn form_run_end(bytes: &[u8], at: usize) -> usize {
    let mut pos = at;
    while let Some(&b) = bytes.get(pos) {
        if ends_form_value(b) {
            break;
        }
        if b == b'\\' {
            let run = run_len(bytes, pos, |b| b == b'\\');
            if matches!(bytes.get(pos + run), Some(b'"' | b'\'')) {
                break;
            }
            pos += run;
            continue;
        }
        if let Some(authority_at) = after_url_slashes(bytes, pos) {
            pos = authority_at + run_len(bytes, authority_at, |b| !ends_authority(b));
            continue;
        }
        pos += 1;
    }
    pos
}

const fn ends_form_value(b: u8) -> bool {
    b.is_ascii_whitespace()
        || b.is_ascii_control()
        || matches!(b, b'&' | b',' | b';' | b')' | b'}' | b'"' | b'\'')
}

/// The opaque token after `Bearer` (any case, starting a token) and one or
/// more spaces or tabs; the span is the token alone. The token is the run
/// of `[A-Za-z0-9._~+/-]` plus trailing `=`, and counts only when it runs
/// 6+ characters carrying a digit, or 20+ characters, so prose such as
/// `bearer of bad news` stays.
fn bearer_token_at(bytes: &[u8], at: usize) -> Option<Range<usize>> {
    const NAME: &[u8] = b"bearer";
    if !has_prefix_ci(bytes, at, NAME) || !starts_token(bytes, at) {
        return None;
    }
    let gap = run_len(bytes, at + NAME.len(), is_blank);
    if gap == 0 {
        return None;
    }
    let start = at + NAME.len() + gap;
    let body = run_len(bytes, start, |b| {
        b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'+' | b'/' | b'-')
    });
    let token = &bytes[start..start + body];
    let carries_digit = token.iter().any(u8::is_ascii_digit);
    if !((carries_digit && body >= BEARER_TOKEN_MIN) || body >= BEARER_TOKEN_MIN_PLAIN) {
        return None;
    }
    let pad = run_len(bytes, start + body, |b| b == b'=');
    Some(start..start + body + pad)
}

/// Shortest bearer token that counts when it carries a digit.
const BEARER_TOKEN_MIN: usize = 6;

/// Length at which a bearer token with no digit still counts as one.
const BEARER_TOKEN_MIN_PLAIN: usize = 20;

const fn is_key_char(b: u8) -> bool {
    is_token_char(b) || b == b'.'
}

const fn is_blank(b: u8) -> bool {
    b == b' ' || b == b'\t'
}

/// A quote mark preceded by `backslashes` backslashes, ending at `end`.
#[derive(Clone, Copy)]
struct Quote {
    mark: u8,
    backslashes: usize,
    end: usize,
}

/// Zero or more backslashes then `"` or `'` at `at`.
fn quote_at(bytes: &[u8], at: usize) -> Option<Quote> {
    let backslashes = run_len(bytes, at, |b| b == b'\\');
    let mark = *bytes.get(at + backslashes)?;
    matches!(mark, b'"' | b'\'').then_some(Quote {
        mark,
        backslashes,
        end: at + backslashes + 1,
    })
}

/// The value after `open`: up to the first `open.mark` preceded by no more
/// backslashes than `open` carries (a quote escaped one level deeper stays
/// inside) or, for an unescaped `open`, by an even run of backslashes
/// (escaped backslashes, kept in the value); else to the next control
/// character or the end.
fn quoted_value(bytes: &[u8], open: &Quote) -> Range<usize> {
    let start = open.end;
    let mut backslashes = 0;
    for (pos, &b) in bytes.iter().enumerate().skip(start) {
        if b == open.mark {
            if open.backslashes == 0 && backslashes % 2 == 0 {
                return start..pos;
            }
            if backslashes <= open.backslashes {
                return start..pos - backslashes;
            }
        }
        if b.is_ascii_control() {
            return start..pos;
        }
        backslashes = if b == b'\\' { backslashes + 1 } else { 0 };
    }
    start..bytes.len()
}

/// Judges an ASCII key by the request-body credential-key rule, which takes
/// it lowercased; a model-vocabulary key, whole or as the last dotted
/// segment, is not credential-named.
fn key_names_credential(key: &[u8]) -> bool {
    let mut lowered = [0_u8; MAX_JUDGED_KEY_LEN];
    let Some(buf) = lowered.get_mut(..key.len()) else {
        return true;
    };
    buf.copy_from_slice(key);
    buf.make_ascii_lowercase();
    let last_segment = buf.rsplit(|&b| b == b'.').next().unwrap_or_default();
    if VOCABULARY_TOKEN_KEYS.contains(&last_segment) {
        return false;
    }
    std::str::from_utf8(buf).is_ok_and(body_key_looks_credential)
}

#[cfg(test)]
#[path = "upstream_scrub_tests.rs"]
mod tests;
