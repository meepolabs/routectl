//! The single production `Config` parse funnel plus a did-you-mean
//! enhancer for its "unknown field" / "unknown variant" errors.
//!
//! `toml` 1.x's error `Display` already carries the source line, a caret,
//! and serde's `unknown field `X`, expected one of `a`, `b`` candidate
//! list. The enhancer reads the offending token and the candidate list
//! back out of that rendered message, scores each candidate against the
//! token, and appends a `did you mean `Y`?` line for the closest match.
//! There is deliberately NO second field-name registry: the candidate
//! list is exactly the one serde already emitted, so it cannot drift from
//! the real struct fields.
//!
//! WHY parse the Display string rather than a structured error: the
//! candidate list lives only in serde's message text (toml exposes no
//! typed accessor for it), and threading `Spanned`/custom visitors through
//! the whole `Config` tree was rejected as disproportionate for an
//! error-presentation feature. On any message that does not match the
//! expected shape, or when no candidate scores close enough, the original
//! `Display` string is returned UNCHANGED -- never a new error class.

use crate::config::Config;

/// Minimum `jaro_winkler` similarity for a candidate to be offered as a
/// suggestion. Below this, the token is treated as too far from every
/// known field/variant to guess at, and no suggestion is appended.
const SUGGESTION_THRESHOLD: f64 = 0.7;

/// Parse `text` into a [`Config`], mapping any deserialize error through
/// `enhance_unknown_field_error` so an "unknown field"/"unknown variant"
/// failure gains a `did you mean` hint when a close field name exists.
///
/// The single production config parse site funnels through here.
pub fn parse_config(text: &str) -> Result<Config, String> {
    toml::from_str::<Config>(text).map_err(|e| enhance_unknown_field_error(&e))
}

/// Render `err` and, when its message is serde's "unknown field" or
/// "unknown variant" shape with a candidate list, append a
/// `did you mean `Y`?` line naming the closest candidate. Returns the
/// original `Display` string unchanged when the message does not match or
/// no candidate scores at/above [`SUGGESTION_THRESHOLD`].
fn enhance_unknown_field_error(err: &toml::de::Error) -> String {
    let display = err.to_string();
    match closest_candidate(&display) {
        Some(best) => format!("{display}\n\ndid you mean `{best}`?"),
        None => display,
    }
}

/// Extract the offending token + candidate list from `display` and return
/// the closest candidate when it scores at/above the threshold.
fn closest_candidate(display: &str) -> Option<String> {
    let (token, candidates) = extract_token_and_candidates(display)?;
    let (best_score, best) = candidates
        .into_iter()
        .map(|candidate| (strsim::jaro_winkler(&token, &candidate), candidate))
        .max_by(|(a, _), (b, _)| a.total_cmp(b))?;
    (best_score >= SUGGESTION_THRESHOLD).then_some(best)
}

/// Pull the offending token (`unknown field `X`` / `unknown variant `X``)
/// and the backtick-quoted candidate list that follows `expected` out of a
/// rendered toml/serde error message. Returns `None` when either marker is
/// absent or the candidate list is empty.
fn extract_token_and_candidates(display: &str) -> Option<(String, Vec<String>)> {
    // Anchor from the marker: toml embeds a verbatim source-line preview
    // BEFORE the "unknown field/variant" clause, and that preview can itself
    // contain the text "expected " (e.g. a value like "as expected value").
    // Searching from the marker keeps the preview out of the candidate scan.
    let (marker_pos, token) = ["unknown field `", "unknown variant `"]
        .into_iter()
        .find_map(|marker| {
            let pos = display.find(marker)?;
            let token = first_backtick_token_after(&display[pos..], marker)?;
            Some((pos, token))
        })?;

    let anchored = &display[marker_pos..];
    let expected_at = anchored.find("expected ")?;
    let candidates: Vec<String> = backtick_tokens(&anchored[expected_at..])
        .into_iter()
        .filter(|candidate| *candidate != token)
        .collect();
    if candidates.is_empty() {
        return None;
    }
    Some((token, candidates))
}

/// The text between the first backtick that follows `marker` and the next
/// backtick, or `None` when `marker` is absent or unterminated.
fn first_backtick_token_after(haystack: &str, marker: &str) -> Option<String> {
    let start = haystack.find(marker)? + marker.len();
    let rest = &haystack[start..];
    let end = rest.find('`')?;
    Some(rest[..end].to_string())
}

/// Every backtick-delimited token in `s`, in order. Used to read serde's
/// `expected one of `a`, `b`, `c`` candidate list.
fn backtick_tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = s;
    while let Some(open) = rest.find('`') {
        let after_open = &rest[open + 1..];
        let Some(close) = after_open.find('`') else {
            break;
        };
        out.push(after_open[..close].to_string());
        rest = &after_open[close + 1..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::parse_config;

    /// An unknown field within a known table gains a suggestion pointing at
    /// the closest real field. Pins serde's `unknown field` phrasing: a
    /// toml/serde bump that reshapes the message fails this loudly.
    #[test]
    fn unknown_field_with_close_typo_suggests_the_real_field() {
        let err =
            parse_config("[server]\nprt = 8080\n").expect_err("an unknown field must be rejected");

        assert!(
            err.contains("unknown field `prt`"),
            "expected serde's unknown-field phrasing, got: {err}"
        );
        assert!(
            err.contains("did you mean `port`?"),
            "expected a `port` suggestion, got: {err}"
        );
    }

    /// A bad provider `kind` is an unknown enum variant (the `ProviderEntry`
    /// enum is internally tagged on `kind`); the enhancer handles the
    /// `unknown variant` shape too.
    #[test]
    fn unknown_provider_kind_variant_suggests_the_real_kind() {
        let raw = "\
[providers.p]
kind = \"openai-compatt\"
base_url = \"http://127.0.0.1:1\"
api_key_ref = \"literal:x\"
";
        let err = parse_config(raw).expect_err("a bad provider kind must be rejected");

        assert!(
            err.contains("unknown variant `openai-compatt`"),
            "expected serde's unknown-variant phrasing, got: {err}"
        );
        assert!(
            err.contains("did you mean `openai-compat`?"),
            "expected an `openai-compat` suggestion, got: {err}"
        );
    }

    /// A typo'd `[retry.classes.*]` key is a bad `ConfigFailureClass` enum
    /// map key -- also surfaced by serde as an unknown variant.
    #[test]
    fn unknown_retry_class_key_variant_suggests_the_real_class() {
        let err = parse_config("[retry.classes.rate-limitedd]\nretry = 1\n")
            .expect_err("a bad retry.classes key must be rejected");

        assert!(
            err.contains("unknown variant `rate-limitedd`"),
            "expected serde's unknown-variant phrasing, got: {err}"
        );
        assert!(
            err.contains("did you mean `rate-limited`?"),
            "expected a `rate-limited` suggestion, got: {err}"
        );
    }

    /// A token far from every known field yields NO suggestion -- the
    /// enhancer must not invent a guess for garbage. The original message
    /// is returned unchanged.
    #[test]
    fn low_confidence_token_gets_no_suggestion() {
        let err = parse_config("[server]\nqqqqqqqq = 1\n")
            .expect_err("an unknown field must still be rejected");

        assert!(
            err.contains("unknown field `qqqqqqqq`"),
            "expected serde's unknown-field phrasing, got: {err}"
        );
        assert!(
            !err.contains("did you mean"),
            "a low-confidence token must not get a suggestion, got: {err}"
        );
    }

    /// A parse error with no unknown-field/variant shape (plain syntax
    /// error) passes through untouched -- never a new error class, never a
    /// spurious suggestion.
    #[test]
    fn syntax_error_passes_through_without_suggestion() {
        let err = parse_config("this = = broken\n").expect_err("broken TOML must be rejected");

        assert!(
            !err.contains("did you mean"),
            "a plain syntax error must not gain a suggestion, got: {err}"
        );
    }

    /// A well-formed config still parses through the funnel.
    #[test]
    fn valid_config_parses() {
        let cfg = parse_config("[server]\nport = 8080\n").expect("a valid config must parse");
        assert_eq!(cfg.server.port, 8080);
    }

    /// The offending value text itself contains "expected ", which toml
    /// echoes into the source-line preview BEFORE serde's candidate list.
    /// The enhancer must anchor its scan to the "unknown field" marker so the
    /// preview never becomes candidate material -- otherwise it would parse
    /// the offending token out of the preview and suggest it back verbatim.
    #[test]
    fn value_text_containing_expected_never_echoes_the_token_back() {
        let err = parse_config("[server]\nprt = \"as expected value\"\n")
            .expect_err("an unknown field must be rejected");

        assert!(
            err.contains("unknown field `prt`"),
            "expected serde's unknown-field phrasing, got: {err}"
        );
        assert!(
            !err.contains("did you mean `prt`?"),
            "the offending token must never be echoed back as a suggestion, got: {err}"
        );
    }
}
