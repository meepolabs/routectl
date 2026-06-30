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
}
