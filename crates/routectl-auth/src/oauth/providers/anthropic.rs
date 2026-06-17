//! Anthropic claude.ai OAuth flow.
//!
//! Constants extracted from the claude-code Node bundle (v2.1.150).
//! These are the production-tier values used by the official CLI; the
//! flow is a public OAuth 2.0 PKCE client (no client_secret), with
//! `anthropic-beta: oauth-2025-04-20` required on token-endpoint
//! requests.
//!
//! Surface map:
//! - Authorize URL: <https://claude.com/cai/oauth/authorize>
//! - Token URL:     <https://platform.claude.com/v1/oauth/token>
//! - Manual paste:  <https://platform.claude.com/oauth/code/callback>
//!   (used when the operator launches login on a headless machine and
//!   pastes the code back into routectl rather than running a local
//!   callback server).
//!
//! Two callback flavors are supported:
//! 1. Browser-launched: redirect_uri = `http://127.0.0.1:<port>/callback`,
//!    captured by routectl's local axum sub-app.
//! 2. Manual (--print-url): redirect_uri = MANUAL_REDIRECT_URL,
//!    operator pastes the resulting `code#state` back to routectl.

use async_trait::async_trait;
use url::Url;

use crate::oauth::providers::{truncate, AuthParams, OAuthFlow};
use crate::oauth::types::{unix_now, AccountInfo, SecretToken, TokenRecord};
use crate::oauth::{OAuthError, OAuthResult};

pub(crate) const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
pub(crate) const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";
pub(crate) const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub(crate) const MANUAL_REDIRECT_URL: &str = "https://platform.claude.com/oauth/code/callback";
pub(crate) const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";

/// Maximum number of bytes routectl will read from the token endpoint
/// response. Real responses are well under 4 KiB; a 64 KiB cap leaves
/// generous headroom for future fields (id_token, granted_scopes,
/// vendor extensions) without letting a misbehaving or hostile upstream
/// drive the loader toward OOM.
const MAX_TOKEN_BODY_BYTES: usize = 64 * 1024;

/// Scopes claude-code requests on the claude.ai (subscription) flow.
/// `user:inference` is the load-bearing one for routectl -- it is
/// what the resulting access_token can use against
/// `api.anthropic.com/v1/messages`. The others are claude-code-specific
/// but harmless to include (parity with claude-code's tokens, less
/// suspicious to Anthropic's heuristics).
pub(crate) const SCOPES: &[&str] = &[
    "user:profile",
    "user:inference",
    "user:sessions:claude_code",
    "user:mcp_servers",
    "user:file_upload",
];

pub(crate) struct Anthropic;

/// Stamp the claude.ai identity onto a single token-endpoint request.
/// The claude.ai OAuth flow needs ONLY the claude-cli User-Agent -- it
/// must NOT carry the codex fingerprint (originator + residency) nor the
/// Stainless SDK block (`x-app` + `x-stainless-*`), which belong to the
/// egress messages surface, not the token endpoint. Folding the single
/// header in here (consume-and-return, mirroring `codex_identity`) keeps
/// the two production POST sites byte-identical and gives the regression
/// tests the real production stamping to assert against.
fn anthropic_identity(rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    rb.header(
        reqwest::header::USER_AGENT,
        routectl_core::identity::anthropic::default_claude_code_user_agent(),
    )
}

#[async_trait]
impl OAuthFlow for Anthropic {
    fn provider_id(&self) -> &'static str {
        "anthropic"
    }

    fn display_name(&self) -> &'static str {
        "Anthropic (claude.ai)"
    }

    fn manual_redirect_url(&self) -> &'static str {
        MANUAL_REDIRECT_URL
    }

    fn auth_url(&self, params: &AuthParams<'_>) -> Url {
        let mut url = Url::parse(AUTHORIZE_URL).expect("authorize URL is a constant");
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", CLIENT_ID)
            .append_pair("redirect_uri", params.redirect_uri)
            .append_pair("scope", &SCOPES.join(" "))
            .append_pair("code_challenge", params.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", params.state);
        url
    }

    async fn exchange_code(
        &self,
        http: &reqwest::Client,
        code: &str,
        verifier: &str,
        state: &str,
        redirect_uri: &str,
    ) -> OAuthResult<TokenRecord> {
        // claude.ai's `oauth-2025-04-20` token endpoint takes JSON
        // (despite RFC 6749 prescribing form-urlencoded for OAuth 2.0
        // token endpoints) and requires the CSRF `state` echoed in the
        // body. Without `state` the upstream returns 400
        // invalid_request_error: "Invalid request format". Confirmed
        // against three independent OSS implementations
        // (pacifio/cersei, achetronic/claude-oauth-proxy, querymt/
        // anthropic-auth).
        let body = serde_json::json!({
            "grant_type": "authorization_code",
            "code": code,
            "code_verifier": verifier,
            "state": state,
            "redirect_uri": redirect_uri,
            "client_id": CLIENT_ID,
        });
        let resp = anthropic_identity(
            http.post(TOKEN_URL)
                .header("anthropic-beta", OAUTH_BETA_HEADER)
                .header("content-type", "application/json"),
        )
        .json(&body)
        .send()
        .await
        .map_err(|e| OAuthError::Network(format!("token endpoint POST: {e}")))?;
        decode_token_response(resp, TokenFlow::Exchange).await
    }

    async fn refresh_token(
        &self,
        http: &reqwest::Client,
        refresh_token: &str,
    ) -> OAuthResult<TokenRecord> {
        // Same JSON content-type as exchange_code per RFC 6749 section 6
        // (and the upstream's actual behavior on `oauth-2025-04-20`).
        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
        });

        // Compute sha8 once; reuse on pre-POST debug and failure error
        // so operators can correlate the three legs of a refresh attempt
        // (pre-POST, success, failure) by refresh_token_sha8 without
        // seeing the raw token value.
        let prior_sha8 = super::sha8(refresh_token);
        tracing::debug!(
            grant_type = "refresh_token",
            refresh_token_sha8 = %prior_sha8,
            "anthropic refresh request"
        );

        let resp = anthropic_identity(
            http.post(TOKEN_URL)
                .header("anthropic-beta", OAUTH_BETA_HEADER)
                .header("content-type", "application/json"),
        )
        .json(&body)
        .send()
        .await
        .map_err(|e| OAuthError::Network(format!("refresh endpoint POST: {e}")))?;
        decode_token_response_traced(resp, &prior_sha8).await
    }
}

/// Internal token-endpoint response shape. The vendor returns the same
/// JSON for `authorization_code` and `refresh_token` grants. Public to
/// the `providers` module (via `pub(super)`) so the parsing helper
/// below can be unit-tested with hand-rolled fixture JSON without
/// faking a `reqwest::Response`.
#[derive(serde::Deserialize)]
pub(super) struct Resp {
    access_token: String,
    refresh_token: String,
    #[serde(default = "default_token_type")]
    token_type: String,
    expires_in: u64,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    account: Option<AccountField>,
}

fn default_token_type() -> String {
    "Bearer".into()
}

#[derive(serde::Deserialize)]
pub(super) struct AccountField {
    #[serde(default)]
    email: Option<String>,
    #[serde(default, alias = "uuid", alias = "id")]
    account_id: Option<String>,
}

/// Shared response decoder for both `exchange_code` and `refresh_token`.
/// Three discrete steps (status check + parse + map) so each is small
/// enough to keep in head with its failure modes. `flow` selects how
/// `invalid_grant` is bucketed: a fresh login that hits invalid_grant
/// means the auth code is bad (expired, already used, or PKCE/redirect
/// mismatch); a refresh that hits invalid_grant means the refresh token
/// is gone.
async fn decode_token_response(
    resp: reqwest::Response,
    flow: TokenFlow,
) -> OAuthResult<TokenRecord> {
    let status = resp.status();
    let url = resp.url().to_string();
    let body = read_capped_body(resp).await?;

    check_status_error(status, &url, &body, flow)?;
    let parsed = parse_token_response_json(&body, flow)?;
    Ok(map_to_record(parsed, flow))
}

/// Refresh-only variant of [`decode_token_response`] with structured
/// tracing. Emits `tracing::debug!` on success (expires_in,
/// new_refresh_token_sha8) and `tracing::error!` on failure (status,
/// error_kind, prior_refresh_token_sha8) so operators can correlate a
/// 401 or token-endpoint failure back to the specific credential
/// without ever seeing raw token values.
///
/// Failure-side events deliberately do NOT echo any portion of the
/// response body: token-endpoint error envelopes from some IdPs echo
/// the submitted refresh_token; logging the body verbatim would defeat
/// the bearer-redaction contract that governs the rest of the auth
/// layer. The structured fields carry every operator-actionable signal
/// (status, error_kind, prior sha8); the human-readable error string
/// returned to the caller still conveys the non-sensitive context.
pub(super) async fn decode_token_response_traced(
    resp: reqwest::Response,
    prior_refresh_sha8: &str,
) -> OAuthResult<TokenRecord> {
    let status = resp.status();
    let url = resp.url().to_string();
    let body = read_capped_body(resp).await?;

    if let Err(e) = check_status_error(status, &url, &body, TokenFlow::Refresh) {
        let kind = error_kind_label(&e);
        tracing::error!(
            status = %status.as_u16(),
            error_kind = %kind,
            prior_refresh_token_sha8 = %prior_refresh_sha8,
            "anthropic refresh failed"
        );
        return Err(e);
    }

    let parsed = match parse_token_response_json(&body, TokenFlow::Refresh) {
        Ok(p) => p,
        Err(e) => {
            let kind = error_kind_label(&e);
            tracing::error!(
                status = %status.as_u16(),
                error_kind = %kind,
                prior_refresh_token_sha8 = %prior_refresh_sha8,
                "anthropic refresh failed"
            );
            return Err(e);
        }
    };

    // Pull tracing fields before consuming `parsed` via `map_to_record`.
    let new_refresh_sha8 = super::sha8(&parsed.refresh_token);
    let expires_in = parsed.expires_in;
    let record = map_to_record(parsed, TokenFlow::Refresh);

    tracing::debug!(
        status = %status.as_u16(),
        expires_in = %expires_in,
        new_refresh_token_sha8 = %new_refresh_sha8,
        "anthropic refresh response"
    );

    Ok(record)
}

/// Short label for the error variant returned by the token endpoint.
/// Used in the structured `error_kind` field on refresh-failure trace
/// events so operators can grep for the failure mode without scraping
/// the human-readable message.
fn error_kind_label(e: &OAuthError) -> &'static str {
    match e {
        OAuthError::Network(_) => "network",
        OAuthError::TokenEndpoint(_) => "token_endpoint",
        OAuthError::RefreshExpired(_) => "refresh_expired",
        _ => "other",
    }
}

/// Which token-endpoint call produced the response. Used by
/// `check_status_error` to bucket `invalid_grant` correctly.
#[derive(Clone, Copy)]
pub(super) enum TokenFlow {
    Exchange,
    Refresh,
}

/// Pull the response body as UTF-8, capped at `MAX_TOKEN_BODY_BYTES`.
/// Anthropic's token endpoint speaks JSON; non-UTF-8 is a hard error
/// rather than something to silently lose via `unwrap_or_default`.
async fn read_capped_body(resp: reqwest::Response) -> OAuthResult<String> {
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| OAuthError::Network(format!("read token response body: {e}")))?;
    if bytes.len() > MAX_TOKEN_BODY_BYTES {
        return Err(OAuthError::TokenEndpoint(format!(
            "token response is {} bytes; refusing to load (cap is {} bytes)",
            bytes.len(),
            MAX_TOKEN_BODY_BYTES
        )));
    }
    let s = std::str::from_utf8(&bytes).map_err(|e| {
        OAuthError::TokenEndpoint(format!("token response is not valid UTF-8: {e}"))
    })?;
    Ok(s.to_string())
}

/// Translate a non-success HTTP status into an `OAuthError`. The
/// invalid_grant body fragment maps to `RefreshExpired` so the operator
/// gets "re-run routectl login" guidance instead of a generic
/// "endpoint error".
/// Map status + body to an `OAuthError`. On 4xx/5xx, branch on `flow`:
/// a refresh that hits `invalid_grant` is `RefreshExpired` (operator
/// guidance: re-run login). An exchange that hits the same is
/// `TokenEndpoint` -- the auth code is the thing that died, and the
/// upstream body explains how (expired / already used / PKCE
/// mismatch / redirect_uri mismatch).
fn check_status_error(
    status: reqwest::StatusCode,
    url: &str,
    body: &str,
    flow: TokenFlow,
) -> OAuthResult<()> {
    if status.is_success() {
        return Ok(());
    }
    if matches!(flow, TokenFlow::Refresh) && body.contains("invalid_grant") {
        return Err(OAuthError::RefreshExpired("anthropic".into()));
    }
    // Refresh-flow request bodies carry the long-lived refresh token,
    // and some IdPs echo request fields in error envelopes. Omit the
    // upstream body excerpt entirely on the refresh path to prevent
    // secret leakage into operator-visible errors and logs. The
    // exchange path stays as-is; its body is the authorization code
    // (single-use, short-lived) plus the PKCE verifier (already used).
    Err(if matches!(flow, TokenFlow::Refresh) {
        OAuthError::TokenEndpoint(format!("{} {}", status.as_u16(), url))
    } else {
        OAuthError::TokenEndpoint(format!(
            "{} {}: {}",
            status.as_u16(),
            url,
            truncate(body, 500)
        ))
    })
}

/// Parse the JSON body into the internal `Resp` shape. Flow-aware so a
/// malformed refresh response does not echo the body (which may carry
/// the long-lived refresh token in error envelopes some IdPs return);
/// exchange responses keep the truncated body for operator triage
/// since the auth code in that flow is single-use and short-lived.
/// Public-in-crate so tests can drive the deserializer directly with a
/// fixture string -- the rest of `decode_token_response` is HTTP plumbing
/// that the fixture would have to fake otherwise.
pub(super) fn parse_token_response_json(body: &str, flow: TokenFlow) -> OAuthResult<Resp> {
    serde_json::from_str::<Resp>(body).map_err(|e| {
        if matches!(flow, TokenFlow::Refresh) {
            OAuthError::TokenEndpoint(format!("parse token response: {e}"))
        } else {
            OAuthError::TokenEndpoint(format!(
                "parse token response: {e}; body={}",
                truncate(body, 200)
            ))
        }
    })
}

/// Project the parsed `Resp` onto the on-disk `TokenRecord`. Computes
/// `expires_at_unix` against `unix_now()` once at exchange time so a
/// later clock jump on disk does not corrupt validity.
///
/// `flow` decides whether to mint a `session_id`. A fresh exchange
/// (login) mints a new one; a refresh leaves it `None` so the OAuth
/// store preserves the prior value, stable across the credential's
/// lifetime. Mirrors the codex flow's per-credential session id.
fn map_to_record(parsed: Resp, flow: TokenFlow) -> TokenRecord {
    let now = unix_now();
    let scopes = parsed
        .scope
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default();
    let account = parsed
        .account
        .map(|a| AccountInfo {
            email: a.email,
            account_id: a.account_id,
        })
        .unwrap_or_default();

    let session_id = match flow {
        TokenFlow::Exchange => Some(uuid::Uuid::new_v4().to_string()),
        TokenFlow::Refresh => None,
    };

    TokenRecord {
        access_token: SecretToken::new(parsed.access_token),
        refresh_token: SecretToken::new(parsed.refresh_token),
        token_type: parsed.token_type,
        expires_at_unix: now.saturating_add(parsed.expires_in),
        scopes,
        account,
        obtained_at_unix: now,
        session_id,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_url_includes_pkce_and_scopes() {
        let url = Anthropic.auth_url(&AuthParams {
            challenge: "CHAL",
            state: "STATE",
            redirect_uri: "http://127.0.0.1:12345/callback",
        });
        let s = url.as_str();
        assert!(s.starts_with(AUTHORIZE_URL));
        assert!(s.contains("response_type=code"));
        assert!(s.contains(&format!("client_id={CLIENT_ID}")));
        assert!(s.contains("code_challenge=CHAL"));
        assert!(s.contains("code_challenge_method=S256"));
        assert!(s.contains("state=STATE"));
        // url::Url percent-encodes the redirect; check a fragment.
        assert!(s.contains("127.0.0.1") || s.contains("127.0.0.1%3A"));
        // Spaces in scope become '+' or '%20' under url::Url.
        assert!(s.contains("user%3Ainference") || s.contains("user:inference"));
    }

    #[test]
    fn manual_redirect_url_returns_constant() {
        assert_eq!(Anthropic.manual_redirect_url(), MANUAL_REDIRECT_URL);
    }

    #[test]
    fn parse_token_response_with_account_id_field() {
        let body = r#"{
            "access_token": "AT",
            "refresh_token": "RT",
            "token_type": "Bearer",
            "expires_in": 3600,
            "scope": "user:inference user:profile",
            "account": { "email": "u@example.com", "account_id": "acc-123" }
        }"#;
        let parsed = parse_token_response_json(body, TokenFlow::Exchange).unwrap();
        assert_eq!(
            parsed.account.as_ref().unwrap().account_id.as_deref(),
            Some("acc-123")
        );
        let rec = map_to_record(parsed, TokenFlow::Exchange);
        assert_eq!(rec.access_token.expose(), "AT");
        assert_eq!(rec.refresh_token.expose(), "RT");
        assert_eq!(rec.scopes.len(), 2);
        assert_eq!(rec.account.email.as_deref(), Some("u@example.com"));
    }

    #[test]
    fn parse_token_response_with_uuid_alias() {
        // Some Anthropic responses surface the account id as `uuid`.
        let body = r#"{
            "access_token": "AT",
            "refresh_token": "RT",
            "expires_in": 100,
            "account": { "uuid": "uuid-form-123" }
        }"#;
        let parsed = parse_token_response_json(body, TokenFlow::Exchange).unwrap();
        assert_eq!(
            parsed.account.as_ref().unwrap().account_id.as_deref(),
            Some("uuid-form-123")
        );
    }

    #[test]
    fn parse_token_response_with_id_alias() {
        // Some Anthropic responses surface the account id as `id`.
        let body = r#"{
            "access_token": "AT",
            "refresh_token": "RT",
            "expires_in": 100,
            "account": { "id": "id-form-456" }
        }"#;
        let parsed = parse_token_response_json(body, TokenFlow::Exchange).unwrap();
        assert_eq!(
            parsed.account.as_ref().unwrap().account_id.as_deref(),
            Some("id-form-456")
        );
    }

    #[test]
    fn check_status_error_invalid_grant_on_refresh_buckets_to_refresh_expired() {
        let err = check_status_error(
            reqwest::StatusCode::BAD_REQUEST,
            "https://example.invalid/v1/oauth/token",
            r#"{"error":"invalid_grant"}"#,
            TokenFlow::Refresh,
        )
        .unwrap_err();
        match err {
            OAuthError::RefreshExpired(p) => assert_eq!(p, "anthropic"),
            other => panic!("expected RefreshExpired, got {other:?}"),
        }
    }

    #[test]
    fn check_status_error_invalid_grant_on_exchange_is_token_endpoint() {
        // During login (exchange), invalid_grant means the auth code
        // is the thing that died, not the refresh token. Operator
        // needs the upstream's actual error body so they can tell
        // expired-code from PKCE-mismatch from redirect_uri-mismatch.
        let err = check_status_error(
            reqwest::StatusCode::BAD_REQUEST,
            "https://example.invalid/v1/oauth/token",
            r#"{"error":"invalid_grant","error_description":"code expired"}"#,
            TokenFlow::Exchange,
        )
        .unwrap_err();
        match err {
            OAuthError::TokenEndpoint(msg) => {
                assert!(msg.contains("400"), "got: {msg}");
                assert!(msg.contains("invalid_grant"), "got: {msg}");
            }
            other => panic!("expected TokenEndpoint, got {other:?}"),
        }
    }

    #[test]
    fn check_status_error_other_failure_is_token_endpoint() {
        let err = check_status_error(
            reqwest::StatusCode::INTERNAL_SERVER_ERROR,
            "https://example.invalid/v1/oauth/token",
            "boom",
            TokenFlow::Exchange,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("500"), "got: {msg}");
    }

    #[test]
    fn exchange_mints_a_valid_uuid_session_id() {
        // A fresh login (Exchange) mints a per-credential session_id; a
        // refresh leaves it None.
        let body = r#"{
            "access_token": "AT",
            "refresh_token": "RT",
            "expires_in": 3600
        }"#;
        let parsed = parse_token_response_json(body, TokenFlow::Exchange).unwrap();
        let rec = map_to_record(parsed, TokenFlow::Exchange);
        let sid = rec.session_id.expect("exchange must mint a session_id");
        // Must parse as a valid UUID v4.
        let parsed_uuid = uuid::Uuid::parse_str(&sid).expect("session_id must be a valid uuid");
        assert_eq!(
            parsed_uuid.get_version(),
            Some(uuid::Version::Random),
            "session_id must be a v4 uuid; got {sid}"
        );
    }

    #[test]
    fn refresh_leaves_session_id_none() {
        // A refresh must NOT mint a session_id -- the store preserves the
        // prior value, keeping the id stable across rotations. Re-minting
        // here would break session-id stability across rotations.
        let body = r#"{
            "access_token": "AT",
            "refresh_token": "RT",
            "expires_in": 3600
        }"#;
        let parsed = parse_token_response_json(body, TokenFlow::Refresh).unwrap();
        let rec = map_to_record(parsed, TokenFlow::Refresh);
        assert!(
            rec.session_id.is_none(),
            "refresh must leave session_id None for the store to preserve the prior value"
        );
    }

    /// Both production token-endpoint POSTs (`exchange_code` and
    /// `refresh_token`) route their builder through the shared
    /// `anthropic_identity` helper, so the identity stamping is identical
    /// for both flows. Exercising the helper directly -- the SAME function
    /// the production path calls -- covers both POST sites at once; a
    /// per-flow duplicate would only re-test the helper twice.
    ///
    /// Guards three regressions at once:
    /// 1. the claude-cli User-Agent is present (dropping it would make the
    ///    token endpoint see an unrecognized client);
    /// 2. the codex fingerprint (originator + residency) is absent;
    /// 3. the Stainless SDK block (`x-app` + every `x-stainless-*` key) is
    ///    absent -- that block belongs to the egress messages surface, not
    ///    the token endpoint. The excluded names are read from
    ///    `default_claude_code_identity_headers()` so the assertion tracks
    ///    the real header set instead of a hand-copied guess.
    #[test]
    fn anthropic_identity_stamps_claude_cli_ua_and_nothing_else() {
        use routectl_core::identity::anthropic::{
            default_claude_code_identity_headers, default_claude_code_user_agent,
        };

        // Arrange + Act: stamp identity onto a bare token-endpoint POST
        // through the production helper, then build it for inspection.
        let req = anthropic_identity(reqwest::Client::new().post(TOKEN_URL))
            .build()
            .expect("identity-stamped request must build");
        let headers = req.headers();
        let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());

        // Assert: claude-cli UA present.
        assert_eq!(
            header("user-agent"),
            Some(default_claude_code_user_agent()),
            "token-endpoint POST must carry the claude-cli User-Agent",
        );

        // Assert: codex fingerprint absent.
        assert!(
            header("originator").is_none(),
            "token-endpoint POST must NOT carry the codex originator header",
        );
        assert!(
            header("x-openai-internal-codex-residency").is_none(),
            "token-endpoint POST must NOT carry the codex residency header",
        );

        // Assert: the entire Stainless SDK block is absent. Iterating the
        // real default set means adding ANY of those keys (x-app or any
        // x-stainless-*) to anthropic_identity fails this assertion.
        for (name, _value) in default_claude_code_identity_headers() {
            assert!(
                headers.get(name).is_none(),
                "token-endpoint POST must NOT carry the Stainless header {name}",
            );
        }
    }
}
