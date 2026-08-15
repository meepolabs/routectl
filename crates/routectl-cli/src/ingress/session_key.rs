//! Inbound per-conversation session-key resolution, shared across ingress
//! dialects.
//!
//! `inbound_session_key` is routectl's conversation identity: it keys the
//! K estimator, the ledger `session_id` column, the shadow store and the
//! sticky pin. Every dialect resolves it from a HEADER candidate first and a
//! BODY candidate second, and the tail of that resolution -- trim,
//! empty-as-absent, precedence, the bound, and the never-log-raw conflict
//! WARN -- lives here so the dialects cannot drift apart on it.
//!
//! What stays dialect-local is the VOCABULARY: which header names and which
//! body path a given wire shape carries. Each ingress reads its own
//! candidates and hands them to [`resolve_session_key`].

use axum::http::HeaderMap;

/// Header names the OpenAI-shaped ingresses accept as inbound session
/// identity, in PRECEDENCE ORDER: the first entry with a non-empty,
/// bound-passing value wins. A header outside this set yields no key --
/// the closed set is the point, and a client routectl has no capture for
/// gets today's keyless behavior rather than a guess.
///
/// Order is load-bearing and each entry is grounded in a real client
/// capture:
///
/// | Order | Header | Emitters | Why it qualifies |
/// |---|---|---|---|
/// | 1 | `x-session-id` | crush (every LLM call), opencode (every non-hosted request), pi (openrouter format, when session-affinity headers are enabled) | Three independent emitters, per-conversation |
/// | 2 | `session_id` | pi (openai format), cline (openai-codex path) | Two emitters; the cline openai-compat path's only identity |
/// | 3 | `agent-session-id` | goose | Per-session, from goose's own explicit session-id header const |
/// | 4 | `x-task-id` | cline (billing path) | Carries cline's session id |
/// | 5 | `session-id` | codex | Per-conversation uuid on every codex Responses request |
///
/// Deliberately EXCLUDED, so the reasons are not re-litigated.
/// `x-session-affinity` is value-identical to a higher-precedence entry in
/// every emitter that sends it, so it adds recall and no lanes.
/// `x-opencode-session` rides only opencode's HOSTED-gateway branch, a
/// different code path from the non-hosted one that sends `x-session-id`,
/// so it never identifies traffic aimed at routectl. `thread-id` and
/// `x-client-request-id` are per-request in some emitters (including
/// routectl's own Responses egress) and per-conversation in others, so
/// admitting them would mint a fresh key per request and make coverage
/// look perfect while the estimator can never calibrate;
/// `x-codex-turn-state` is per-TURN; `x-parent-session-id` merges subagent
/// sessions into their parent, a different grouping than a conversation.
///
/// HAZARD on entry 5, which is why it is LAST. routectl itself stamps
/// `session-id` outbound on the ChatgptOauth Responses lane
/// (`routectl-providers/src/openai_responses/client.rs`), and that value is
/// a stable PER-CREDENTIAL id minted at login, not a per-conversation one.
/// In a routectl-fronting-routectl topology the inner instance would read
/// it and fuse every conversation on that credential into one session key.
/// It is included anyway because codex is the flagship Responses client and
/// `session-id` is its only usable identity, so excluding it leaves the
/// highest-value lane permanently keyless. Placing it last means any
/// genuine per-conversation header outranks it.
pub const OPENAI_SESSION_HEADERS: &[&str] = &[
    "x-session-id",
    "session_id",
    "agent-session-id",
    "x-task-id",
    "session-id",
];

/// Maximum accepted length, in UTF-8 bytes, of a trimmed inbound session
/// key. The value is client-controlled and is CLONED into the canonical
/// request, every ledger row, the K-estimator key, the shadow key and the
/// sticky pin key; the entry-count bounds downstream bound entries, not
/// owned bytes, and no ingress-captured string is length-capped today.
/// 256 bytes is far above every observed emitter (all send a uuid or a
/// hash of one) and far below a size worth propagating.
pub(crate) const MAX_INBOUND_SESSION_KEY_BYTES: usize = 256;

/// Trim a raw candidate and accept it only if it can safely become a
/// session key: non-empty, at most [`MAX_INBOUND_SESSION_KEY_BYTES`], and
/// composed entirely of GRAPHIC ASCII (`!` through `~`).
///
/// The accepted value persists to the ledger `session_id` column, where an
/// operator later renders or greps it. A client-chosen bidi override or
/// isolate (U+202E and U+2066-2069) embedded there visually reorders the
/// surrounding text of any such rendering, and a zero-width character
/// (U+200B, U+FEFF) makes two rows that display identically compare
/// unequal -- both are display spoofs against the operator, not against a
/// log line. Restricting to graphic ASCII excludes those, the Cc controls,
/// and U+2028/U+2029 by construction, and costs no recall on any observed
/// emitter: each sends a uuid or a hash of one. Interior whitespace is
/// excluded with them DELIBERATELY, and not because nothing can carry it --
/// a legacy user-named session id can, since the name is validated only for
/// path traversal and length before becoming the header value. The trade is
/// accepted knowingly: such a lane resolves keyless rather than admitting a
/// value that displays with invisible padding in a column an operator greps.
/// Widening to permit a space would reintroduce exactly the hazard this
/// predicate exists to close. Ordinary punctuation is NOT restricted; ledger
/// writes bind the value as a parameter.
///
/// A rejected candidate is indistinguishable from an absent one, so
/// resolution falls through to the next source and the request itself is
/// never rejected. The forwarded `provider_extras` copy is untouched, so
/// the wire contract does not change.
fn accept_candidate(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > MAX_INBOUND_SESSION_KEY_BYTES {
        return None;
    }
    if !trimmed.chars().all(|c| c.is_ascii_graphic()) {
        return None;
    }
    Some(trimmed)
}

/// First accepted [`OPENAI_SESSION_HEADERS`] value present on the request,
/// in the const's precedence order, trimmed. A value that fails the
/// length/character bound is skipped like an absent one.
///
/// No case normalization here or anywhere else in this module: `HeaderName`
/// is lowercase-normalized when the request is parsed, so the lowercase
/// const entries already match a client that wrote `X-Session-Id`.
pub fn first_session_header(headers: &HeaderMap) -> Option<&str> {
    OPENAI_SESSION_HEADERS.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|v| v.to_str().ok())
            .and_then(accept_candidate)
    })
}

/// Resolve the inbound session key from a header candidate and a body
/// candidate. Each is trimmed and must pass the length and
/// graphic-ASCII bound; the header wins when both survive, and a
/// candidate that fails is indistinguishable from an absent one, so
/// resolution falls through to the next source. The request itself is
/// never rejected and the forwarded body is never mutated.
///
/// When both survive and DISAGREE, emits one `warn`-level
/// `session_key_source_conflict` event carrying the boolean fact of the
/// mismatch and nothing else. The raw values are never logged: they are
/// client-controlled and may identify a user. The conflict is worth a WARN
/// because it means the same conversation would key into a different
/// K-estimator and ledger session depending on which source a given
/// request happened to carry.
pub fn resolve_session_key(header: Option<&str>, body: Option<&str>) -> Option<String> {
    let header_key = header.and_then(accept_candidate);
    let body_key = body.and_then(accept_candidate);

    if let (Some(h), Some(b)) = (header_key, body_key)
        && h != b
    {
        tracing::warn!(
            session_key_source_conflict = true,
            "inbound session key mismatch between header and body"
        );
    }

    header_key.or(body_key).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderName, HeaderValue};

    fn headers_from(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for (name, value) in pairs {
            headers.insert(
                HeaderName::from_bytes(name.as_bytes()).unwrap(),
                HeaderValue::from_str(value).unwrap(),
            );
        }
        headers
    }

    #[test]
    fn resolve_prefers_header_over_body() {
        assert_eq!(
            resolve_session_key(Some("from-header"), Some("from-body")).as_deref(),
            Some("from-header"),
        );
    }

    #[test]
    fn resolve_trims_both_candidates() {
        assert_eq!(
            resolve_session_key(Some("  from-header  "), None).as_deref(),
            Some("from-header"),
        );
        assert_eq!(
            resolve_session_key(None, Some("\tfrom-body\n")).as_deref(),
            Some("from-body"),
        );
    }

    #[test]
    fn resolve_treats_empty_header_as_absent_and_falls_through_to_body() {
        assert_eq!(
            resolve_session_key(Some("   "), Some("from-body")).as_deref(),
            Some("from-body"),
        );
    }

    #[test]
    fn resolve_yields_none_when_neither_source_has_a_value() {
        assert_eq!(resolve_session_key(None, None), None);
        assert_eq!(resolve_session_key(Some(""), Some("  ")), None);
    }

    #[test]
    fn resolve_warns_once_on_disagreement_without_logging_raw_values() {
        let events = routectl_testkit::capture_events(|| {
            assert_eq!(
                resolve_session_key(Some("sid-header"), Some("sid-body")).as_deref(),
                Some("sid-header"),
            );
        });

        let conflicts: Vec<_> = events
            .iter()
            .filter(|e| e.field("session_key_source_conflict").is_some())
            .collect();
        assert_eq!(
            conflicts.len(),
            1,
            "expected exactly one conflict WARN, got events: {events:?}",
        );
        assert_eq!(conflicts[0].level, tracing::Level::WARN);
        assert_eq!(
            conflicts[0].field("session_key_source_conflict"),
            Some("true"),
        );
        for event in &events {
            assert!(
                !event.message.contains("sid-header") && !event.message.contains("sid-body"),
                "raw session key must never be logged: {event:?}",
            );
            for (_, v) in &event.fields {
                assert!(
                    v != "sid-header" && v != "sid-body",
                    "raw session key must never appear in a structured field: {event:?}",
                );
            }
        }
    }

    #[test]
    fn resolve_does_not_warn_when_sources_agree_after_trim() {
        let events = routectl_testkit::capture_events(|| {
            let _ = resolve_session_key(Some(" sid-same "), Some("sid-same"));
        });
        assert!(
            !events
                .iter()
                .any(|e| e.field("session_key_source_conflict").is_some()),
            "agreeing sources must not fire the conflict guardrail: {events:?}",
        );
    }

    #[test]
    fn resolve_falls_through_when_header_exceeds_the_byte_bound() {
        let oversized = "a".repeat(MAX_INBOUND_SESSION_KEY_BYTES + 1);
        assert_eq!(
            resolve_session_key(Some(&oversized), Some("from-body")).as_deref(),
            Some("from-body"),
        );
        assert_eq!(resolve_session_key(Some(&oversized), None), None);
    }

    #[test]
    fn resolve_accepts_a_candidate_exactly_at_the_byte_bound() {
        let at_bound = "a".repeat(MAX_INBOUND_SESSION_KEY_BYTES);
        assert_eq!(
            resolve_session_key(Some(&at_bound), None).as_deref(),
            Some(at_bound.as_str()),
        );
    }

    /// The bound is enforced on BOTH candidates, not just the header. Pins
    /// the symmetry so an asymmetric change to one side cannot pass while
    /// the header-side bound tests stay green.
    #[test]
    fn resolve_rejects_an_oversized_or_control_bearing_body_candidate() {
        let oversized = "a".repeat(MAX_INBOUND_SESSION_KEY_BYTES + 1);
        assert_eq!(resolve_session_key(None, Some(&oversized)), None);
        assert_eq!(resolve_session_key(None, Some("sid\nfrom-body")), None);
    }

    #[test]
    fn resolve_prefers_a_valid_header_over_an_invalid_body() {
        let oversized = "a".repeat(MAX_INBOUND_SESSION_KEY_BYTES + 1);
        assert_eq!(
            resolve_session_key(Some("sid-from-header"), Some(&oversized)).as_deref(),
            Some("sid-from-header"),
        );
    }

    /// A bound-failing allowlist header must not shadow a later valid one:
    /// it is skipped exactly like an absent header rather than ending the
    /// search at the higher-precedence position.
    #[test]
    fn first_session_header_falls_through_a_control_bearing_higher_precedence_header() {
        let mut headers = HeaderMap::new();
        headers.insert("x-session-id", "sid\u{2028}with-separator".parse().unwrap());
        headers.insert("agent-session-id", "sid-goose".parse().unwrap());
        assert_eq!(first_session_header(&headers), Some("sid-goose"));
    }

    #[test]
    fn byte_bound_counts_utf8_bytes_not_chars() {
        // Two bytes per char, so the char count is well under the bound
        // while the byte length is one over it.
        let oversized = "\u{e9}".repeat(MAX_INBOUND_SESSION_KEY_BYTES / 2 + 1);
        assert!(oversized.chars().count() <= MAX_INBOUND_SESSION_KEY_BYTES);
        assert!(oversized.len() > MAX_INBOUND_SESSION_KEY_BYTES);
        assert_eq!(resolve_session_key(Some(&oversized), None), None);
    }

    #[test]
    fn resolve_falls_through_on_interior_control_or_separator_characters() {
        for bad in [
            "sid\nwith-newline",
            "sid\rwith-cr",
            "sid\u{7f}with-delete",
            "sid\u{0}with-nul",
            "sid\u{2028}with-line-separator",
            "sid\u{2029}with-paragraph-separator",
        ] {
            assert_eq!(
                resolve_session_key(Some(bad), Some("from-body")).as_deref(),
                Some("from-body"),
                "control-bearing candidate must be treated as absent: {bad:?}",
            );
            assert_eq!(resolve_session_key(Some(bad), None), None, "{bad:?}");
        }
    }

    /// The Cf FORMAT characters are the operator-facing display hazard:
    /// they carry no glyph, so a value bearing one renders as an ordinary
    /// id in the ledger column while reordering or hiding the text around
    /// it. `char::is_control` covers only the Cc category, so none of these
    /// were caught before the predicate narrowed to graphic ASCII.
    #[test]
    fn resolve_falls_through_on_bidi_and_zero_width_format_characters() {
        for bad in [
            "sid\u{202e}with-rtl-override",
            "sid\u{202d}with-ltr-override",
            "sid\u{2066}with-lrt-isolate",
            "sid\u{2069}with-pop-isolate",
            "sid\u{200b}with-zero-width-space",
            "sid\u{feff}with-byte-order-mark",
        ] {
            assert_eq!(
                resolve_session_key(Some(bad), Some("from-body")).as_deref(),
                Some("from-body"),
                "format-bearing candidate must be treated as absent: {bad:?}",
            );
            assert_eq!(resolve_session_key(Some(bad), None), None, "{bad:?}");
        }
    }

    /// Non-ASCII letters are rejected with them. No emitter sends one, and
    /// admitting them would reopen the confusable-glyph half of the same
    /// ledger-display hazard.
    #[test]
    fn resolve_falls_through_on_non_ascii_letters_and_interior_whitespace() {
        for bad in ["sid-caf\u{e9}", "sid-\u{0441}yrillic-es", "sid with-space"] {
            assert_eq!(resolve_session_key(Some(bad), None), None, "{bad:?}");
        }
    }

    #[test]
    fn resolve_accepts_ordinary_punctuation() {
        assert_eq!(
            resolve_session_key(Some("sid/with:punct-and_'quotes'"), None).as_deref(),
            Some("sid/with:punct-and_'quotes'"),
        );
    }

    #[test]
    fn first_session_header_honors_allowlist_precedence_order() {
        let all = headers_from(&[
            ("session-id", "fifth"),
            ("x-task-id", "fourth"),
            ("agent-session-id", "third"),
            ("session_id", "second"),
            ("x-session-id", "first"),
        ]);
        assert_eq!(first_session_header(&all), Some("first"));

        let without_top = headers_from(&[("session-id", "fifth"), ("session_id", "second")]);
        assert_eq!(first_session_header(&without_top), Some("second"));

        let only_last = headers_from(&[("session-id", "fifth")]);
        assert_eq!(first_session_header(&only_last), Some("fifth"));
    }

    #[test]
    fn first_session_header_matches_a_mixed_case_wire_name() {
        // Nothing in this module lowercases: `HeaderName` normalizes on
        // parse, so opencode's `X-Session-Id` hits the lowercase entry.
        let headers = headers_from(&[("X-Session-Id", "sid-opencode")]);
        assert_eq!(first_session_header(&headers), Some("sid-opencode"));
    }

    #[test]
    fn first_session_header_skips_empty_and_bound_failing_values() {
        let oversized = "a".repeat(MAX_INBOUND_SESSION_KEY_BYTES + 1);
        let headers = headers_from(&[
            ("x-session-id", "   "),
            ("session_id", &oversized),
            ("agent-session-id", "sid-goose"),
        ]);
        assert_eq!(first_session_header(&headers), Some("sid-goose"));
    }

    #[test]
    fn first_session_header_ignores_unlisted_headers() {
        let headers = headers_from(&[
            ("x-session-affinity", "affinity"),
            ("thread-id", "thread"),
            ("x-client-request-id", "request"),
            ("x-parent-session-id", "parent"),
            ("x-codex-turn-state", "turn"),
        ]);
        assert_eq!(first_session_header(&headers), None);
    }
}
