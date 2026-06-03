//! Auth header dispatch for the OpenAI Responses egress.
//!
//! Three auth_kind variants:
//!
//! - `ChatgptOauth` (wired in CG.A): the ChatGPT subscription surface
//!   at `https://chatgpt.com/backend-api/codex`. Injects:
//!   ```text
//!   Authorization: Bearer <jwt>
//!   ChatGPT-Account-Id: <uuid>
//!   originator: <cfg.originator or "codex_cli_rs">
//!   x-openai-internal-codex-residency: us
//!   version: <PINNED_CODEX_VERSION>
//!   session-id: <stable per-credential uuid>
//!   x-codex-installation-id: <stable per-install uuid>
//!   x-codex-window-id: <per-process uuid>
//!   thread-id: <per-call uuid>
//!   x-client-request-id: <same uuid as thread-id>
//!   User-Agent: <client-level>
//!   ```
//!   The codex CLI's risk system inspects every routectl-emitted
//!   request claiming `originator: codex_cli_rs` and invalidates
//!   sessions whose HTTP fingerprint drifts from a real codex
//!   install. Mirrors:
//!   - `default_client.rs::default_headers` -- originator, residency, UA
//!   - `client.rs::build_responses_identity_headers` -- window_id,
//!     installation_id (the parent_thread_id / subagent flags are
//!     codex-internal and intentionally absent until v1.1)
//!   - `endpoint/responses.rs::stream_request` -- session-id,
//!     thread-id, x-client-request-id
//!
//! - `ApiKey` (wired in CG.E): the standard OpenAI surface at
//!   `https://api.openai.com/v1/responses`. Injects only
//!   `Authorization: Bearer <api_key>` -- ChatGPT-Account-Id and
//!   originator are ChatGPT-OAuth-specific and absent here. Operators
//!   needing `OpenAI-Organization` / `OpenAI-Project` set them via
//!   `extra_headers`.
//!
//! - `BedrockMantle`: the Mantle proxy at
//!   `https://bedrock-mantle.<region>.api.aws/openai/v1`. Authorization:
//!   Bearer <bearer> using the long-term Bedrock API key (resolved via
//!   api_key_ref, typically env://AWS_BEARER_TOKEN_BEDROCK).

use reqwest::RequestBuilder;

use routectl_core::codex_fingerprint::{
    codex_user_agent, CODEX_ORIGINATOR, ORIGINATOR_HEADER_NAME, PINNED_CODEX_VERSION,
    RESIDENCY_HEADER_NAME, RESIDENCY_HEADER_VALUE,
};
use routectl_core::Result;

use super::{AuthKind, OpenAiResponsesConfig};

/// Default `originator` header value for the ChatGPT-OAuth surface.
/// Mirrors codex's `DEFAULT_ORIGINATOR` in `default_client.rs:36`.
pub(crate) const DEFAULT_ORIGINATOR: &str = CODEX_ORIGINATOR;

/// Default `User-Agent` for the ChatGPT-OAuth surface. Operator-supplied
/// `cfg.user_agent` wins when set; otherwise we emit codex CLI's UA shape
/// (`codex_cli_rs/<X.Y.Z> (<os_type> <os_version>; <arch>) <terminal>`).
/// The risk system on chatgpt.com fingerprints both the literal
/// `originator` value AND the UA shape; emitting `routectl/<ver>
/// codex-cli` here is enough drift to trigger step-up auth on the next
/// codex login.
pub(crate) fn default_user_agent() -> String {
    codex_user_agent().to_string()
}

/// Apply auth headers to an in-flight `RequestBuilder` per
/// `cfg.auth_kind`. The bearer is resolved by the caller (once per
/// upstream request via `cfg.auth.token().await`) and passed in here
/// so a routectl-managed OAuth source can rotate without a daemon
/// restart. `window_id` is the process-global codex window-id from
/// `routectl_core::codex_fingerprint::codex_window_id()` (one per
/// `routectl serve` process; threaded in by the provider).
/// Returns the modified builder.
pub(crate) fn apply(
    rb: RequestBuilder,
    cfg: &OpenAiResponsesConfig,
    bearer: &str,
    window_id: &str,
) -> Result<RequestBuilder> {
    match cfg.auth_kind {
        AuthKind::ChatgptOauth => Ok(apply_chatgpt_oauth(rb, cfg, bearer, window_id)),
        AuthKind::ApiKey | AuthKind::BedrockMantle => Ok(apply_api_key(rb, bearer)),
    }
}

/// ChatGPT-OAuth header injection. Mirrors codex's
/// `backend-client/src/client.rs::headers` (Authorization + ChatGPT-
/// Account-Id) and `default_client.rs::default_headers` (originator,
/// residency) plus the per-request identity headers from
/// `client.rs::build_responses_identity_headers` (window_id,
/// installation_id) and `endpoint/responses.rs::stream_request`
/// (session-id, thread-id, x-client-request-id).
///
/// User-Agent is set at the reqwest::Client level (client-level
/// default header) via `http_client::build` in
/// `OpenAiResponsesProvider::new()`, matching the anthropic_api
/// pattern. Per-request UA injection is intentionally absent here so
/// the single source of truth for the UA is the client level.
fn apply_chatgpt_oauth(
    rb: RequestBuilder,
    cfg: &OpenAiResponsesConfig,
    bearer: &str,
    window_id: &str,
) -> RequestBuilder {
    let mut rb = rb
        .header("authorization", format!("Bearer {bearer}"))
        .header(
            ORIGINATOR_HEADER_NAME,
            cfg.originator.as_deref().unwrap_or(DEFAULT_ORIGINATOR),
        )
        .header(RESIDENCY_HEADER_NAME, RESIDENCY_HEADER_VALUE)
        .header("version", PINNED_CODEX_VERSION)
        .header("x-codex-window-id", window_id);

    // thread-id and x-client-request-id share one fresh UUIDv4 per
    // upstream call. Codex's `endpoint/responses.rs:90-93` mirrors
    // thread-id into x-client-request-id; the risk system expects the
    // pair to match.
    let thread_id = uuid::Uuid::new_v4().to_string();
    rb = rb
        .header("thread-id", &thread_id)
        .header("x-client-request-id", &thread_id);

    if let Some(account_id) = cfg.account_id.as_deref() {
        rb = rb.header("ChatGPT-Account-Id", account_id);
    }
    if let Some(session_id) = cfg.session_id.as_deref() {
        rb = rb.header("session-id", session_id);
    }
    if let Some(installation_id) = cfg.installation_id.as_deref() {
        rb = rb.header("x-codex-installation-id", installation_id);
    }

    rb
}

/// Standard OpenAI API-key header injection for the public
/// `api.openai.com/v1/responses` endpoint. Only `Authorization: Bearer
/// <api_key>` is required; `ChatGPT-Account-Id` and `originator` are
/// ChatGPT-OAuth-specific and intentionally absent. The factory's
/// `validate_openai_responses_account_id` already rejects an
/// `account_id_ref` paired with `auth_kind = "api-key"`, so we never
/// see `cfg.account_id` set on this path. User-Agent is set at the
/// client level (mirrors the chatgpt-oauth path). The bearer is
/// resolved by the caller and passed in (see `apply`).
fn apply_api_key(rb: RequestBuilder, bearer: &str) -> RequestBuilder {
    rb.header("authorization", format!("Bearer {bearer}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::Client;
    use routectl_core::{StaticToken, TokenSource};
    use std::sync::Arc;

    fn base_cfg(auth_kind: AuthKind) -> OpenAiResponsesConfig {
        OpenAiResponsesConfig {
            id: "openai-responses:test".into(),
            auth: Arc::new(StaticToken::new("test-jwt")) as Arc<dyn TokenSource>,
            account_id: Some("acct-uuid".into()),
            base_url: "https://chatgpt.com/backend-api/codex".into(),
            auth_kind,
            header_extras: Vec::new(),
            user_agent: None,
            originator: None,
            session_id: Some("00000000-0000-0000-0000-aaaaaaaaaaaa".into()),
            installation_id: Some("00000000-0000-0000-0000-bbbbbbbbbbbb".into()),
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
        let rb = apply(rb, &cfg, "test-jwt", "window-uuid").expect("apply");
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
    fn user_agent_starts_with_codex_cli_rs() {
        // Arrange + Act
        let ua = default_user_agent();

        // Assert: codex-style UA prefix and pinned version.
        assert!(
            ua.starts_with(&format!("{CODEX_ORIGINATOR}/{PINNED_CODEX_VERSION}")),
            "unexpected UA string: {ua}"
        );
        assert!(
            ua.contains(std::env::consts::ARCH),
            "UA should contain arch {} but was: {ua}",
            std::env::consts::ARCH,
        );
    }

    #[test]
    fn identity_headers_present_on_completion_request() {
        // Arrange
        let cfg = base_cfg(AuthKind::ChatgptOauth);
        let client = Client::new();
        let rb = client.post("https://chatgpt.com/backend-api/codex/responses");

        // Act
        let rb = apply(rb, &cfg, "test-jwt", "window-uuid-xyz").expect("apply");
        let req = rb.build().expect("build");

        // Assert: every codex identity header is present on a single
        // chatgpt-oauth request. Mirrors codex's
        // build_responses_identity_headers + build_session_headers.
        for name in [
            "version",
            "session-id",
            "x-codex-installation-id",
            "x-codex-window-id",
            "thread-id",
            "x-client-request-id",
            "x-openai-internal-codex-residency",
        ] {
            assert!(
                header(&req, name).is_some(),
                "missing required identity header {name:?}",
            );
        }
        assert_eq!(
            header(&req, "version").as_deref(),
            Some(PINNED_CODEX_VERSION)
        );
        assert_eq!(
            header(&req, "x-codex-window-id").as_deref(),
            Some("window-uuid-xyz")
        );
        assert_eq!(
            header(&req, "session-id").as_deref(),
            Some("00000000-0000-0000-0000-aaaaaaaaaaaa")
        );
        assert_eq!(
            header(&req, "x-codex-installation-id").as_deref(),
            Some("00000000-0000-0000-0000-bbbbbbbbbbbb")
        );
        assert_eq!(
            header(&req, "x-openai-internal-codex-residency").as_deref(),
            Some(RESIDENCY_HEADER_VALUE),
        );
    }

    #[test]
    fn thread_id_differs_per_turn_and_matches_x_client_request_id() {
        // Arrange
        let cfg = base_cfg(AuthKind::ChatgptOauth);
        let client = Client::new();

        // Act: two consecutive calls
        let req_a = apply(
            client.post("https://example.test"),
            &cfg,
            "test-jwt",
            "window-uuid",
        )
        .expect("apply")
        .build()
        .expect("build");
        let req_b = apply(
            client.post("https://example.test"),
            &cfg,
            "test-jwt",
            "window-uuid",
        )
        .expect("apply")
        .build()
        .expect("build");

        // Assert: thread-id != thread-id across turns
        let tid_a = header(&req_a, "thread-id").expect("thread-id A");
        let tid_b = header(&req_b, "thread-id").expect("thread-id B");
        assert_ne!(tid_a, tid_b, "thread-id must rotate per turn");
        // Assert: thread-id == x-client-request-id within a single
        // call. Codex pairs them.
        assert_eq!(
            header(&req_a, "x-client-request-id").as_deref(),
            Some(tid_a.as_str()),
        );
        assert_eq!(
            header(&req_b, "x-client-request-id").as_deref(),
            Some(tid_b.as_str()),
        );
    }

    #[test]
    fn window_id_stable_per_provider() {
        // Arrange: caller (the provider) threads the SAME window_id
        // through both calls, mirroring how
        // OpenAiResponsesProvider::build_headers reads the
        // process-global window-id and reuses it on every call.
        let cfg = base_cfg(AuthKind::ChatgptOauth);
        let client = Client::new();
        let window = "window-uuid-stable";

        // Act
        let req_a = apply(
            client.post("https://example.test"),
            &cfg,
            "test-jwt",
            window,
        )
        .expect("apply")
        .build()
        .expect("build");
        let req_b = apply(
            client.post("https://example.test"),
            &cfg,
            "test-jwt",
            window,
        )
        .expect("apply")
        .build()
        .expect("build");

        // Assert
        assert_eq!(
            header(&req_a, "x-codex-window-id"),
            header(&req_b, "x-codex-window-id"),
            "window id must be stable across two calls on the same provider",
        );
    }

    #[test]
    fn window_id_is_process_global_across_distinct_providers() {
        // Arrange: two distinct OpenAiResponsesProvider instances in
        // the same process must share the same window-id, because the
        // value lives in a process-global OnceLock in
        // routectl_core::codex_fingerprint. A per-instance window-id
        // would rotate on every router rebuild (hot-reload), cracking
        // the chatgpt.com impersonation contract.
        let cfg_a = base_cfg(AuthKind::ChatgptOauth);
        let cfg_b = base_cfg(AuthKind::ChatgptOauth);
        let client = Client::new();
        let window = routectl_core::codex_fingerprint::codex_window_id();

        // Act: simulate two providers reading the global window-id
        // independently and applying it.
        let req_a = apply(
            client.post("https://example.test"),
            &cfg_a,
            "test-jwt",
            window,
        )
        .expect("apply")
        .build()
        .expect("build");
        let req_b = apply(
            client.post("https://example.test"),
            &cfg_b,
            "test-jwt",
            window,
        )
        .expect("apply")
        .build()
        .expect("build");

        // Assert: identical window-id across distinct providers AND
        // across repeated codex_window_id() calls.
        assert_eq!(
            header(&req_a, "x-codex-window-id"),
            header(&req_b, "x-codex-window-id"),
            "window id must be process-global, not per-provider",
        );
        assert_eq!(
            header(&req_a, "x-codex-window-id").as_deref(),
            Some(routectl_core::codex_fingerprint::codex_window_id()),
        );
    }

    #[test]
    fn chatgpt_oauth_default_originator_is_codex_cli_rs() {
        // Arrange
        let cfg = base_cfg(AuthKind::ChatgptOauth);
        let client = Client::new();
        let rb = client.post("https://example.test");

        // Act
        let rb = apply(rb, &cfg, "test-jwt", "window-uuid").expect("apply");
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
        let rb = apply(rb, &cfg, "test-jwt", "window-uuid").expect("apply");
        let req = rb.build().expect("build");

        // Assert
        assert_eq!(header(&req, "originator").as_deref(), Some("custom-agent"));
    }

    #[test]
    fn api_key_auth_headers_inject_bearer_only() {
        // Arrange: ApiKey carries no account_id_ref by config-time
        // invariant (factory rejects the combination).
        let mut cfg = base_cfg(AuthKind::ApiKey);
        cfg.account_id = None;
        cfg.session_id = None;
        cfg.installation_id = None;
        let client = Client::new();
        let rb = client.post("https://api.openai.com/v1/responses");

        // Act: the caller resolves the bearer from `cfg.auth` and
        // passes it in; `apply_api_key` no longer reads the config.
        let rb = apply(rb, &cfg, "sk-test-123", "window-uuid").expect("apply");
        let req = rb.build().expect("build");

        // Assert: Bearer auth present; ChatGPT-OAuth-specific headers
        // (ChatGPT-Account-Id, originator, identity headers) are
        // absent.
        assert_eq!(
            header(&req, "authorization").as_deref(),
            Some("Bearer sk-test-123")
        );
        for absent in [
            "chatgpt-account-id",
            "originator",
            "version",
            "session-id",
            "x-codex-installation-id",
            "x-codex-window-id",
            "thread-id",
            "x-client-request-id",
            "x-openai-internal-codex-residency",
        ] {
            assert!(
                header(&req, absent).is_none(),
                "{absent:?} must not be set for api-key",
            );
        }
        // Per-request UA absent (client-level only).
        assert!(
            header(&req, "user-agent").is_none(),
            "UA should not be set as a per-request header"
        );
    }

    #[test]
    fn bedrock_mantle_auth_injects_bearer() {
        // Arrange: BedrockMantle carries no account_id_ref by config-time
        // invariant (factory rejects the combination). The bearer is the
        // long-term AWS Bedrock API key (typically resolved from
        // env://AWS_BEARER_TOKEN_BEDROCK by the caller).
        let mut cfg = base_cfg(AuthKind::BedrockMantle);
        cfg.account_id = None;
        cfg.session_id = None;
        cfg.installation_id = None;
        let client = Client::new();
        let rb = client.post("https://bedrock-mantle.us-east-2.api.aws/openai/v1/responses");

        // Act: byte-identical Bearer auth to the api-key path.
        let rb = apply(rb, &cfg, "bedrock-bearer-xyz", "window-uuid").expect("apply");
        let req = rb.build().expect("build");

        // Assert: Bearer auth present; ChatGPT-OAuth-specific headers
        // (ChatGPT-Account-Id, originator, identity headers) are
        // absent.
        assert_eq!(
            header(&req, "authorization").as_deref(),
            Some("Bearer bedrock-bearer-xyz")
        );
        for absent in [
            "chatgpt-account-id",
            "originator",
            "version",
            "session-id",
            "x-codex-installation-id",
            "x-codex-window-id",
            "thread-id",
            "x-client-request-id",
            "x-openai-internal-codex-residency",
        ] {
            assert!(
                header(&req, absent).is_none(),
                "{absent:?} must not be set for bedrock-mantle",
            );
        }
        // Per-request UA absent (client-level only).
        assert!(
            header(&req, "user-agent").is_none(),
            "UA should not be set as a per-request header"
        );
    }
}
