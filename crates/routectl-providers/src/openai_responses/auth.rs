//! Auth header dispatch for the OpenAI Responses egress.
//!
//! Three auth_kind variants:
//!
//! - `ChatgptOauth` (wired in the relevant stage): the ChatGPT subscription surface
//!   at `https://chatgpt.com/backend-api/codex`. Injects:
//!       Authorization: Bearer <jwt>
//!       ChatGPT-Account-Id: <uuid>
//!       originator: <cfg.originator or "codex_cli_rs">
//!       User-Agent:  <cfg.user_agent or "routectl/<ver> codex-cli">
//!   The originator and ChatGPT-Account-Id headers mirror codex's
//!   `default_client.rs::default_headers` and
//!   `backend-client/src/client.rs::headers`.
//!
//! - `ApiKey` (deferred to the relevant stage): the standard OpenAI surface at
//!   `https://api.openai.com/v1/responses`. Returns NotImplemented.
//!
//! - `BedrockMantle` (deferred to the relevant stage): the Mantle proxy at
//!   `https://bedrock-mantle.<region>.api.aws/openai/v1`. Returns
//!   NotImplemented.

use reqwest::RequestBuilder;

use routectl_core::{Error, Result};

use super::{AuthKind, OpenAiResponsesConfig};

/// Default `originator` header value for the ChatGPT-OAuth surface.
/// Mirrors codex's `DEFAULT_ORIGINATOR` in `default_client.rs:36`.
pub(crate) const DEFAULT_ORIGINATOR: &str = "codex_cli_rs";

/// Default `User-Agent` suffix for the ChatGPT-OAuth surface. The
/// operator-supplied `cfg.user_agent` wins when set; otherwise we
/// emit `routectl/<version> codex-cli` so audit logs on the upstream
/// can tell the request came from routectl while preserving the
/// codex-derived header values OpenAI's edge expects.
pub(crate) fn default_user_agent() -> String {
    format!("routectl/{} codex-cli", env!("CARGO_PKG_VERSION"))
}

/// Apply auth headers to an in-flight `RequestBuilder` per
/// `cfg.auth_kind`. Returns the modified builder on success or an
/// Error for the deferred variants.
pub(crate) fn apply(
    rb: RequestBuilder,
    cfg: &OpenAiResponsesConfig,
) -> Result<RequestBuilder> {
    match cfg.auth_kind {
        AuthKind::ChatgptOauth => Ok(apply_chatgpt_oauth(rb, cfg)),
        AuthKind::ApiKey => Err(Error::Auth(format!(
            "openai-responses provider `{}`: api-key auth_kind not yet supported (the relevant stage)",
            cfg.id
        ))),
        AuthKind::BedrockMantle => Err(Error::Auth(format!(
            "openai-responses provider `{}`: bedrock-mantle auth_kind not yet supported (the relevant stage)",
            cfg.id
        ))),
    }
}

/// ChatGPT-OAuth header injection. Mirrors codex's
/// `backend-client/src/client.rs::headers` (Authorization + ChatGPT-
/// Account-Id) and `default_client.rs::default_headers` (originator).
///
/// User-Agent is set at the reqwest::Client level (client-level
/// default header) via `http_client::build` in
/// `OpenAiResponsesProvider::new()`, matching the anthropic_api
/// pattern. Per-request UA injection is intentionally absent here so
/// the single source of truth for the UA is the client level.
fn apply_chatgpt_oauth(rb: RequestBuilder, cfg: &OpenAiResponsesConfig) -> RequestBuilder {
    let mut rb = rb
        .header("authorization", format!("Bearer {}", cfg.api_key))
        .header(
            "originator",
            cfg.originator.as_deref().unwrap_or(DEFAULT_ORIGINATOR),
        );

    if let Some(account_id) = cfg.account_id.as_deref() {
        rb = rb.header("ChatGPT-Account-Id", account_id);
    }

    rb
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::Client;

    fn base_cfg(auth_kind: AuthKind) -> OpenAiResponsesConfig {
        OpenAiResponsesConfig {
            id: "openai-responses:test".into(),
            api_key: "test-jwt".into(),
            account_id: Some("acct-uuid".into()),
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            auth_kind,
            extra_headers: Vec::new(),
            user_agent: None,
            originator: None,
        }
    }

    /// Pull a single header value out of a built `reqwest::Request`,
    /// case-insensitively (reqwest lowercases header names internally).
    fn header(req: &reqwest::Request, name: &str) -> Option<String> {
        req.headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    }

    #[test]
    fn chatgpt_oauth_auth_headers_inject_bearer_account_id_originator() {
        // Arrange
        let cfg = base_cfg(AuthKind::ChatgptOauth);
        let client = Client::new();
        let rb = client.post("https://chatgpt.com/backend-api/codex/responses");

        // Act
        let rb = apply(rb, &cfg).expect("apply");
        let req = rb.build().expect("build");

        // Assert: per-request headers only. UA is set at client level
        // by OpenAiResponsesProvider::new() -- not visible here.
        assert_eq!(
            header(&req, "authorization").as_deref(),
            Some("Bearer test-jwt")
        );
        assert_eq!(
            header(&req, "chatgpt-account-id").as_deref(),
            Some("acct-uuid")
        );
        assert_eq!(header(&req, "originator").as_deref(), Some("codex_cli_rs"));
        // Per-request UA is intentionally absent (client-level only).
        assert!(
            header(&req, "user-agent").is_none(),
            "UA should not be set as a per-request header"
        );
    }

    #[test]
    fn default_user_agent_contains_version_and_codex_cli() {
        // Arrange + Act
        let ua = default_user_agent();

        // Assert: format is "routectl/<semver> codex-cli"
        assert!(
            ua.starts_with("routectl/") && ua.contains("codex-cli"),
            "unexpected UA string: {ua}"
        );
    }

    #[test]
    fn chatgpt_oauth_default_originator_is_codex_cli_rs() {
        // Arrange
        let cfg = base_cfg(AuthKind::ChatgptOauth);
        let client = Client::new();
        let rb = client.post("https://example.test");

        // Act
        let rb = apply(rb, &cfg).expect("apply");
        let req = rb.build().expect("build");

        // Assert
        assert_eq!(header(&req, "originator").as_deref(), Some("codex_cli_rs"));
    }

    #[test]
    fn chatgpt_oauth_originator_override_from_config() {
        // Arrange
        let mut cfg = base_cfg(AuthKind::ChatgptOauth);
        cfg.originator = Some("custom-agent".into());
        let client = Client::new();
        let rb = client.post("https://example.test");

        // Act
        let rb = apply(rb, &cfg).expect("apply");
        let req = rb.build().expect("build");

        // Assert
        assert_eq!(header(&req, "originator").as_deref(), Some("custom-agent"));
    }

    #[test]
    fn api_key_auth_returns_not_implemented() {
        // Arrange
        let mut cfg = base_cfg(AuthKind::ApiKey);
        cfg.account_id = None;
        let client = Client::new();
        let rb = client.post("https://api.openai.com/v1/responses");

        // Act
        let err = apply(rb, &cfg).expect_err("expected Err");

        // Assert
        match err {
            Error::Auth(msg) => {
                assert!(msg.contains("api-key"), "msg: {msg}");
                assert!(msg.contains("the relevant stage"), "msg: {msg}");
            }
            other => panic!("expected Error::Auth, got {other:?}"),
        }
    }

    #[test]
    fn bedrock_mantle_auth_returns_not_implemented() {
        // Arrange
        let mut cfg = base_cfg(AuthKind::BedrockMantle);
        cfg.account_id = None;
        let client = Client::new();
        let rb = client.post("https://bedrock-mantle.us-east-1.api.aws/openai/v1/responses");

        // Act
        let err = apply(rb, &cfg).expect_err("expected Err");

        // Assert
        match err {
            Error::Auth(msg) => {
                assert!(msg.contains("bedrock-mantle"), "msg: {msg}");
                assert!(msg.contains("the relevant stage"), "msg: {msg}");
            }
            other => panic!("expected Error::Auth, got {other:?}"),
        }
    }
}
