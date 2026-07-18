//! Shared Anthropic `error.type` -> synthetic HTTP status mapping.
//!
//! Both the native Anthropic SSE path and the Bedrock-Converse
//! eventstream can carry an in-band `{"type":"error","error":{"type":
//! ...}}` event mid-200-stream. Each maps the Anthropic error vocabulary
//! to the SAME synthetic HTTP status the sync (non-stream) error path
//! would carry, so `failure_class::classify` and the terminal-error
//! classifier see identical structured facts whether a failure arrives
//! streaming or non-streaming. The mapping lives here (not inline in
//! either egress) so the two paths cannot drift.

/// Map an Anthropic `error.type` token to the synthetic HTTP status the
/// sync error path would carry for the same failure. Unknown tokens fall
/// back to `502` (bad gateway).
pub fn anthropic_error_type_to_status(err_type: &str) -> u16 {
    match err_type {
        "overloaded_error" => 529,
        "rate_limit_error" => 429,
        "invalid_request_error" => 400,
        "authentication_error" => 401,
        "permission_error" => 403,
        "not_found_error" => 404,
        _ => 502,
    }
}

#[cfg(test)]
mod tests {
    use super::anthropic_error_type_to_status;

    #[test]
    fn known_vocabulary_maps_to_sync_path_statuses() {
        let cases: &[(&str, u16)] = &[
            ("overloaded_error", 529),
            ("rate_limit_error", 429),
            ("invalid_request_error", 400),
            ("authentication_error", 401),
            ("permission_error", 403),
            ("not_found_error", 404),
        ];
        for (err_type, expected) in cases {
            assert_eq!(
                anthropic_error_type_to_status(err_type),
                *expected,
                "{err_type} should map to {expected}"
            );
        }
    }

    #[test]
    fn unknown_type_falls_back_to_bad_gateway() {
        assert_eq!(anthropic_error_type_to_status("api_error"), 502);
        assert_eq!(anthropic_error_type_to_status(""), 502);
        assert_eq!(anthropic_error_type_to_status("some_future_error"), 502);
    }
}
