//! Shared token-endpoint error-body classification for OAuth providers.
//!
//! Several providers share the same terminal signal for a dead refresh
//! token: an OAuth 2.0 error envelope whose top-level `error` field is
//! `invalid_grant`. This module owns the single structured-parse
//! definition so provider flows do not each carry their own copy.
//!
//! The parse is deliberately structured, not a substring scan: a
//! transient failure body (a 5xx page, a rate-limit envelope, an
//! `error_description` that merely mentions the phrase) must not be
//! mistaken for a terminal `invalid_grant`. Callers additionally gate on
//! the HTTP status (400/401) before treating the result as terminal.

/// True iff the token-endpoint error body parses as JSON whose top-level
/// `error` field equals `invalid_grant`. A non-JSON body, a missing or
/// non-string `error` field, or the phrase appearing only inside another
/// field (e.g. `error_description`) all return false.
pub(super) fn is_invalid_grant(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
        .as_deref()
        == Some("invalid_grant")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_invalid_grant_is_true() {
        assert!(is_invalid_grant(r#"{"error":"invalid_grant"}"#));
    }

    #[test]
    fn structured_invalid_grant_with_extra_fields_is_true() {
        assert!(is_invalid_grant(
            r#"{"error":"invalid_grant","error_description":"Token expired or revoked."}"#
        ));
    }

    #[test]
    fn structured_invalid_client_is_false() {
        assert!(!is_invalid_grant(r#"{"error":"invalid_client"}"#));
    }

    #[test]
    fn invalid_grant_only_in_description_is_false() {
        assert!(!is_invalid_grant(
            r#"{"error":"server_error","error_description":"upstream saw invalid_grant earlier"}"#
        ));
    }

    #[test]
    fn non_json_body_is_false() {
        assert!(!is_invalid_grant("invalid_grant"));
        assert!(!is_invalid_grant("<html>invalid_grant</html>"));
    }

    #[test]
    fn error_field_absent_is_false() {
        assert!(!is_invalid_grant(r#"{"message":"invalid_grant"}"#));
    }

    #[test]
    fn error_field_non_string_is_false() {
        assert!(!is_invalid_grant(r#"{"error":42}"#));
    }
}
