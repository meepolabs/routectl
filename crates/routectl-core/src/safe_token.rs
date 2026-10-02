//! Shape gate for a single token lifted out of an upstream error message.
//!
//! Upstream rejection messages name the thing they rejected (a request
//! parameter, a tool type, a beta flag). Before such a name is logged
//! verbatim, used as a metric label, or acted on, it must look like a token
//! rather than arbitrary upstream text.

/// Upper bound, in bytes, on a token accepted by [`is_safe_token`]. Every
/// legitimate name this gate sees (a capability param, a tool type, a dated
/// beta flag) is far shorter; the cap only bounds an adversarial or buggy
/// upstream.
pub const MAX_SAFE_TOKEN_LEN: usize = 64;

/// True if `token` is a non-empty, bounded, single-token ASCII string with no
/// whitespace or control bytes (`is_ascii_graphic` is the printable ASCII
/// range excluding space). Admits every token-shaped name while refusing
/// log-forging content (newlines, control bytes, ANSI escapes) and oversized
/// blobs.
pub fn is_safe_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_SAFE_TOKEN_LEN
        && token.bytes().all(|b| b.is_ascii_graphic())
}

#[cfg(test)]
mod tests {
    use super::{MAX_SAFE_TOKEN_LEN, is_safe_token};

    #[test]
    fn accepts_token_shaped_names() {
        for token in ["web_search", "advanced-tool-use-2025-11-20", "a"] {
            assert!(is_safe_token(token), "{token} must be accepted");
        }
    }

    #[test]
    fn accepts_a_token_exactly_at_the_length_cap() {
        let at_cap = "a".repeat(MAX_SAFE_TOKEN_LEN);

        assert!(is_safe_token(&at_cap));
    }

    #[test]
    fn rejects_a_token_one_byte_over_the_length_cap() {
        let over_cap = "a".repeat(MAX_SAFE_TOKEN_LEN + 1);

        assert!(!is_safe_token(&over_cap));
    }

    #[test]
    fn rejects_empty_whitespace_control_and_non_ascii_tokens() {
        for token in [
            "",
            "two words",
            "line\nbreak",
            "tab\there",
            "\x1b[31m",
            "caf\u{e9}",
        ] {
            assert!(!is_safe_token(token), "{token:?} must be refused");
        }
    }
}
