//! Auth header injection for the Gemini egress.
//!
//! Gemini uses an API key passed as the `x-goog-api-key` HTTP header.
//! The key is resolved by the caller and passed in as a plain string.

use reqwest::RequestBuilder;

use routectl_core::Result;

/// Apply the `x-goog-api-key` header to an in-flight `RequestBuilder`.
/// The key is resolved once per request by the caller (via
/// `cfg.auth.token().await`) and passed in here so a routectl-managed
/// token source can rotate without a daemon restart.
///
/// Returns the modified builder. Infallible for a static key; the
/// `Result` wrapper keeps the signature uniform with other provider
/// auth modules that may return validation errors.
pub(crate) fn apply(rb: RequestBuilder, key: &str) -> Result<RequestBuilder> {
    Ok(rb.header("x-goog-api-key", key))
}

/// Apply `Authorization: Bearer <token>` to an in-flight `RequestBuilder`.
/// Used by the Cloud Code egress, which authenticates with an OAuth bearer
/// token rather than the public surface's `x-goog-api-key`.
pub(crate) fn apply_bearer(rb: RequestBuilder, token: &str) -> Result<RequestBuilder> {
    Ok(rb.header("authorization", format!("Bearer {token}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::Client;

    fn header_value(req: &reqwest::Request, name: &str) -> Option<String> {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string())
    }

    #[test]
    fn apply_sets_x_goog_api_key_header() {
        let client = Client::new();
        let rb = client.post("https://generativelanguage.googleapis.com/v1beta/models/gemini-2.5-pro:generateContent");
        let rb = apply(rb, "my-api-key-value").expect("apply ok");
        let req = rb.build().expect("build ok");

        assert_eq!(
            header_value(&req, "x-goog-api-key").as_deref(),
            Some("my-api-key-value"),
            "x-goog-api-key header must carry the resolved key",
        );
        assert!(
            header_value(&req, "authorization").is_none(),
            "authorization header must NOT be set (Gemini uses x-goog-api-key)",
        );
    }

    #[test]
    fn apply_bearer_sets_authorization_header() {
        let client = Client::new();
        let rb = client.post("https://cloudcode-pa.googleapis.com/v1internal:generateContent");
        let rb = apply_bearer(rb, "ya29.token-value").expect("apply_bearer ok");
        let req = rb.build().expect("build ok");

        assert_eq!(
            header_value(&req, "authorization").as_deref(),
            Some("Bearer ya29.token-value"),
            "authorization header must carry the bearer token",
        );
        assert!(
            header_value(&req, "x-goog-api-key").is_none(),
            "x-goog-api-key header must NOT be set on the Cloud Code path",
        );
    }
}
