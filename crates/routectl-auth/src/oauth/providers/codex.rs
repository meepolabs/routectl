//! OpenAI ChatGPT/Codex OAuth flow.
//!
//! Constants extracted from the open-source codex CLI (codex-rs/login).
//! These are the production-tier values used by the official CLI; the
//! flow is a public OAuth 2.0 PKCE client (no client_secret).
//!
//! Surface map:
//! - Authorize URL: <https://auth.openai.com/oauth/authorize>
//! - Token URL:     <https://auth.openai.com/oauth/token>
//!   (used for BOTH the authorization_code exchange and refresh_token)
//!
//! Two idiosyncrasies versus the Anthropic flow:
//! 1. The token response carries NO `expires_in`. The access_token is a
//!    JWT; routectl parses its `exp` claim to derive `expires_at_unix`.
//! 2. The refresh response makes every field optional. When the upstream
//!    omits `refresh_token`, routectl preserves the prior one (the
//!    previous refresh_token is passed into `refresh_token` already).
//!
//! JWT handling note for future security review: routectl decodes the
//! JWT payload for claims (`exp`, `chatgpt_account_id`, `email`) ONLY.
//! It does NOT verify the JWT signature -- the upstream is the verifier;
//! routectl never makes a trust decision on these claims. They feed
//! `expires_at_unix` (a refresh-timing hint, re-checked against the
//! upstream on every 401) and display-only identity fields.

use async_trait::async_trait;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use serde::de::DeserializeOwned;
use url::Url;

use crate::oauth::providers::{truncate, AuthParams, OAuthFlow};
use crate::oauth::types::{unix_now, AccountInfo, SecretToken, TokenRecord};
use crate::oauth::{OAuthError, OAuthResult};

/// Public PKCE client id for the codex CLI. No client_secret -- this is
/// a public OAuth client identified solely by `client_id` + PKCE.
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// Scopes the codex CLI requests. `offline_access` is the load-bearing
/// one for routectl's refresh flow (it is what makes the token endpoint
/// mint a refresh_token); the others mirror the official CLI's grant so
/// the resulting tokens are consistent from a real codex login.
const SCOPES: &[&str] = &[
    "openid",
    "profile",
    "email",
    "offline_access",
    "api.connectors.read",
    "api.connectors.invoke",
];

/// Maximum number of bytes routectl will read from the token endpoint
/// response. Real responses are well under 4 KiB even with a JWT
/// access_token; a 64 KiB cap leaves generous headroom without letting
/// a misbehaving or hostile upstream drive the loader toward OOM.
const MAX_TOKEN_BODY_BYTES: usize = 64 * 1024;

pub(crate) struct Codex;

#[async_trait]
impl OAuthFlow for Codex {
    fn provider_id(&self) -> &'static str {
        "codex"
    }

    fn display_name(&self) -> &'static str {
        "OpenAI (ChatGPT/Codex)"
    }

    fn callback_path(&self) -> &'static str {
        "/auth/callback"
    }

    /// Codex's public client registers its redirect URIs against fixed
    /// ports (1455, fallback 1457), so routectl cannot use an ephemeral
    /// port here the way the Anthropic flow does.
    fn preferred_callback_port(&self) -> Option<u16> {
        Some(1455)
    }

    /// The codex CLI has no headless "paste the code" landing page; the
    /// flow always runs through the local callback server. Returning the
    /// authorize URL here keeps `--print-url` from pointing at a dead
    /// endpoint -- but operators should use the default browser flow.
    fn manual_redirect_url(&self) -> &'static str {
        AUTHORIZE_URL
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
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("state", params.state)
            .append_pair("originator", "codex_cli_rs");
        url
    }

    async fn exchange_code(
        &self,
        http: &reqwest::Client,
        code: &str,
        verifier: &str,
        _state: &str,
        redirect_uri: &str,
    ) -> OAuthResult<TokenRecord> {
        // OpenAI's token endpoint takes the authorization_code grant as
        // form-urlencoded per RFC 6749 (unlike claude.ai, which wants
        // JSON). `state` is NOT echoed in the body -- the CSRF check
        // happens at the callback only.
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ];
        let resp = http
            .post(TOKEN_URL)
            .form(&form)
            .send()
            .await
            .map_err(|e| OAuthError::Network(format!("token endpoint POST: {e}")))?;
        decode_token_response(resp, None).await
    }

    async fn refresh_token(
        &self,
        http: &reqwest::Client,
        refresh_token: &str,
    ) -> OAuthResult<TokenRecord> {
        // The refresh grant POSTs JSON (matching the codex CLI's
        // `request_chatgpt_token_refresh`). The response may omit
        // `refresh_token`; when it does, the prior token is preserved
        // via the `prior_refresh` fallback below.
        let body = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
        });
        let resp = http
            .post(TOKEN_URL)
            .json(&body)
            .send()
            .await
            .map_err(|e| OAuthError::Network(format!("refresh endpoint POST: {e}")))?;
        decode_token_response(resp, Some(refresh_token)).await
    }
}

/// Decode a base64url-no-pad JWT payload into `T`. The signature is NOT
/// verified -- see the module header. Returns `TokenEndpoint` on a
/// malformed JWT so a bad upstream response surfaces as a token-endpoint
/// failure rather than a panic.
fn decode_jwt_payload<T: DeserializeOwned>(jwt: &str) -> Result<T, OAuthError> {
    // JWT format: header.payload.signature -- all three segments must be
    // present and non-empty.
    let mut parts = jwt.split('.');
    let payload_b64 = match (parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s)) if !h.is_empty() && !p.is_empty() && !s.is_empty() => p,
        _ => {
            return Err(OAuthError::TokenEndpoint(
                "access_token is not a JWT".into(),
            ))
        }
    };
    let bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| OAuthError::TokenEndpoint(format!("JWT payload base64 decode: {e}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| OAuthError::TokenEndpoint(format!("JWT payload JSON parse: {e}")))
}

/// Internal token-endpoint response shape for the codex flow. Distinct
/// from Anthropic's `Resp`: no `expires_in` (the access_token JWT's
/// `exp` claim drives expiry instead), and every field is optional on
/// the refresh path. `pub(super)` so the parsing/mapping helpers can be
/// unit-tested with fixture JSON without faking a `reqwest::Response`.
#[derive(serde::Deserialize)]
pub(super) struct Resp {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    scope: Option<String>,
}

/// Claims routectl reads out of the access_token JWT payload. `exp` is
/// the standard top-level expiry claim (Unix seconds); the account id is
/// nested under the OpenAI auth-claims object. Both are best-effort:
/// `exp` defaults to "already expired" and the account id to `None` if
/// absent, so a thin JWT never panics the loader.
#[derive(serde::Deserialize, Default)]
struct JwtClaims {
    #[serde(default)]
    exp: Option<u64>,
    #[serde(default, rename = "https://api.openai.com/auth")]
    auth: Option<AuthClaims>,
}

#[derive(serde::Deserialize, Default)]
struct AuthClaims {
    #[serde(default)]
    chatgpt_account_id: Option<String>,
}

/// Shared response decoder for both `exchange_code` and `refresh_token`.
/// `prior_refresh` is `Some` on the refresh path: it is the previous
/// refresh token, used as the fallback when the upstream omits a fresh
/// one (OpenAI rotates lazily). `None` on the exchange path, where a
/// missing refresh_token is a hard error -- a login that yields no
/// refresh token cannot be refreshed later.
async fn decode_token_response(
    resp: reqwest::Response,
    prior_refresh: Option<&str>,
) -> OAuthResult<TokenRecord> {
    let status = resp.status();
    let url = resp.url().to_string();
    let body = read_capped_body(resp).await?;

    check_status_error(status, &url, &body, prior_refresh.is_some())?;
    let parsed = parse_token_response_json(&body, prior_refresh.is_some())?;
    map_to_record(parsed, prior_refresh)
}

/// Pull the response body as UTF-8, capped at `MAX_TOKEN_BODY_BYTES`.
/// The token endpoint speaks JSON; non-UTF-8 is a hard error rather than
/// something to silently lose via `unwrap_or_default`.
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

/// Map status + body to an `OAuthError`. On the refresh path, a 401 with
/// any of the three `refresh_token_*` error codes maps to
/// `RefreshExpired` (operator guidance: re-run login); any other refresh
/// failure -- including a generic 401 -- maps to `TokenEndpoint`. The
/// refresh body is never echoed because it carries the long-lived
/// refresh token in some IdP error envelopes; the exchange body is kept
/// (truncated) since its auth code is single-use and short-lived.
fn check_status_error(
    status: reqwest::StatusCode,
    url: &str,
    body: &str,
    is_refresh: bool,
) -> OAuthResult<()> {
    if status.is_success() {
        return Ok(());
    }
    if is_refresh && status == reqwest::StatusCode::UNAUTHORIZED && is_refresh_token_dead(body) {
        return Err(OAuthError::RefreshExpired("codex".into()));
    }
    Err(if is_refresh {
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

/// True if the token-endpoint error body names one of OpenAI's three
/// terminal refresh-token error codes. Mirrors the codex CLI's
/// `classify_refresh_token_failure`: the code may sit at `error` (a bare
/// string), `error.code` (a nested object), or top-level `code`.
fn is_refresh_token_dead(body: &str) -> bool {
    matches!(
        extract_error_code(body).as_deref(),
        Some("refresh_token_expired")
            | Some("refresh_token_reused")
            | Some("refresh_token_invalidated")
    )
}

/// Extract an OAuth error code from a token-endpoint error body. Checks
/// `error` (string), `error.code` (object), then top-level `code`.
fn extract_error_code(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    if let Some(err) = v.get("error") {
        if let Some(code) = err.as_str() {
            return Some(code.to_string());
        }
        if let Some(code) = err.get("code").and_then(|c| c.as_str()) {
            return Some(code.to_string());
        }
    }
    v.get("code").and_then(|c| c.as_str()).map(String::from)
}

/// Parse the JSON body into the internal `Resp` shape. Flow-aware so a
/// malformed refresh response does not echo the body (which may carry
/// the long-lived refresh token in error envelopes some IdPs return);
/// exchange responses keep the truncated body for operator triage since
/// the auth code in that flow is single-use and short-lived.
pub(super) fn parse_token_response_json(body: &str, is_refresh: bool) -> OAuthResult<Resp> {
    serde_json::from_str::<Resp>(body).map_err(|e| {
        if is_refresh {
            OAuthError::TokenEndpoint(format!("parse token response: {e}"))
        } else {
            OAuthError::TokenEndpoint(format!(
                "parse token response: {e}; body={}",
                truncate(body, 200)
            ))
        }
    })
}

/// Project the parsed `Resp` onto the on-disk `TokenRecord`.
///
/// - `expires_at_unix` comes from the access_token JWT's `exp` claim --
///   an absolute Unix timestamp, NOT `now + expires_in`. A JWT without
///   `exp` saturates to 0 (looks expired everywhere; the safe direction).
/// - `refresh_token`: on refresh, fall back to `prior_refresh` when the
///   upstream omits a fresh one. On exchange (`prior_refresh == None`),
///   a missing refresh_token is a hard error.
/// - `account_id` comes from the access_token JWT's nested auth claim;
///   `email` falls back to the id_token JWT when present.
fn map_to_record(parsed: Resp, prior_refresh: Option<&str>) -> OAuthResult<TokenRecord> {
    let access_token = parsed
        .access_token
        .ok_or_else(|| OAuthError::TokenEndpoint("token response missing access_token".into()))?;

    let refresh_token = match (parsed.refresh_token, prior_refresh) {
        (Some(rt), _) => rt,
        (None, Some(prior)) => prior.to_string(),
        (None, None) => {
            return Err(OAuthError::TokenEndpoint(
                "token response missing refresh_token".into(),
            ))
        }
    };

    let claims: JwtClaims = decode_jwt_payload(&access_token)?;
    let expires_at_unix = claims.exp.unwrap_or(0);
    let account_id = claims.auth.and_then(|a| a.chatgpt_account_id);

    // Email is display-only; pull it from the id_token JWT when one is
    // present, ignoring a malformed id_token rather than failing login.
    let email = parsed
        .id_token
        .as_deref()
        .and_then(|jwt| decode_jwt_payload::<IdClaims>(jwt).ok())
        .and_then(|c| c.email);

    let scopes = parsed
        .scope
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default();

    Ok(TokenRecord {
        access_token: SecretToken::new(access_token),
        refresh_token: SecretToken::new(refresh_token),
        token_type: parsed.token_type.unwrap_or_else(|| "Bearer".into()),
        expires_at_unix,
        scopes,
        account: AccountInfo { email, account_id },
        obtained_at_unix: unix_now(),
    })
}

/// Display-only claims read out of the id_token JWT.
#[derive(serde::Deserialize, Default)]
struct IdClaims {
    #[serde(default)]
    email: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic unsigned JWT (`header.payload.sig`) from a
    /// payload JSON `Value`. Mirrors the codex CLI's test helper. The
    /// signature segment is non-empty filler -- routectl never verifies
    /// it.
    fn jwt(payload: serde_json::Value) -> String {
        let enc = |b: &[u8]| URL_SAFE_NO_PAD.encode(b);
        let header = enc(br#"{"alg":"none","typ":"JWT"}"#);
        let body = enc(payload.to_string().as_bytes());
        let sig = enc(b"sig");
        format!("{header}.{body}.{sig}")
    }

    #[test]
    fn auth_url_includes_pkce_scopes_and_codex_params() {
        let url = Codex.auth_url(&AuthParams {
            challenge: "CHAL",
            state: "STATE",
            redirect_uri: "http://localhost:1455/auth/callback",
        });
        let s = url.as_str();
        assert!(s.starts_with(AUTHORIZE_URL), "got: {s}");
        assert!(s.contains("response_type=code"));
        assert!(s.contains(&format!("client_id={CLIENT_ID}")));
        assert!(s.contains("code_challenge=CHAL"));
        assert!(s.contains("code_challenge_method=S256"));
        assert!(s.contains("state=STATE"));
        // The three codex-specific extra params.
        assert!(s.contains("id_token_add_organizations=true"));
        assert!(s.contains("codex_cli_simplified_flow=true"));
        assert!(s.contains("originator=codex_cli_rs"));
        // Space-joined scopes (url::Url renders space as '+' or '%20').
        assert!(
            s.contains("offline_access") && s.contains("api.connectors.invoke"),
            "scopes missing: {s}"
        );
    }

    #[test]
    fn preferred_callback_port_is_1455() {
        assert_eq!(Codex.preferred_callback_port(), Some(1455));
    }

    #[test]
    fn anthropic_preferred_callback_port_is_none() {
        use crate::oauth::providers::anthropic::Anthropic;
        assert_eq!(Anthropic.preferred_callback_port(), None);
    }

    #[test]
    fn exchange_jwt_exp_and_account_id_populate_record() {
        // exp = now + 3600 and the chatgpt_account_id claim must land in
        // expires_at_unix and account.account_id respectively.
        let n: u64 = 1_900_000_000;
        let access = jwt(serde_json::json!({
            "exp": n + 3600,
            "https://api.openai.com/auth": { "chatgpt_account_id": "acc-xyz" }
        }));
        let body = serde_json::json!({
            "access_token": access,
            "refresh_token": "RT",
            "token_type": "Bearer"
        })
        .to_string();
        let parsed = parse_token_response_json(&body, false).unwrap();
        let rec = map_to_record(parsed, None).unwrap();
        assert_eq!(rec.expires_at_unix, n + 3600);
        assert_eq!(rec.account.account_id.as_deref(), Some("acc-xyz"));
        assert_eq!(rec.refresh_token.expose(), "RT");
    }

    #[test]
    fn refresh_omitting_refresh_token_preserves_prior() {
        // A refresh response with no `refresh_token` must keep the prior
        // one (OpenAI rotates lazily).
        let access = jwt(serde_json::json!({ "exp": 1_900_000_000u64 }));
        let body = serde_json::json!({ "access_token": access }).to_string();
        let parsed = parse_token_response_json(&body, true).unwrap();
        let rec = map_to_record(parsed, Some("PRIOR-RT")).unwrap();
        assert_eq!(rec.refresh_token.expose(), "PRIOR-RT");
    }

    #[test]
    fn refresh_with_new_refresh_token_uses_new() {
        let access = jwt(serde_json::json!({ "exp": 1_900_000_000u64 }));
        let body = serde_json::json!({
            "access_token": access,
            "refresh_token": "NEW-RT"
        })
        .to_string();
        let parsed = parse_token_response_json(&body, true).unwrap();
        let rec = map_to_record(parsed, Some("PRIOR-RT")).unwrap();
        assert_eq!(rec.refresh_token.expose(), "NEW-RT");
    }

    #[test]
    fn exchange_missing_refresh_token_is_error() {
        // On the exchange path (no prior token), a missing refresh_token
        // cannot be papered over.
        let access = jwt(serde_json::json!({ "exp": 1_900_000_000u64 }));
        let body = serde_json::json!({ "access_token": access }).to_string();
        let parsed = parse_token_response_json(&body, false).unwrap();
        let err = map_to_record(parsed, None).unwrap_err();
        match err {
            OAuthError::TokenEndpoint(m) => assert!(m.contains("refresh_token"), "got: {m}"),
            other => panic!("expected TokenEndpoint, got {other:?}"),
        }
    }

    #[test]
    fn jwt_without_exp_saturates_to_zero() {
        let access = jwt(serde_json::json!({ "sub": "u" }));
        let body = serde_json::json!({
            "access_token": access,
            "refresh_token": "RT"
        })
        .to_string();
        let parsed = parse_token_response_json(&body, false).unwrap();
        let rec = map_to_record(parsed, None).unwrap();
        assert_eq!(rec.expires_at_unix, 0);
        assert!(rec.account.account_id.is_none());
    }

    #[test]
    fn email_pulled_from_id_token() {
        let access = jwt(serde_json::json!({ "exp": 1_900_000_000u64 }));
        let id = jwt(serde_json::json!({ "email": "u@example.com" }));
        let body = serde_json::json!({
            "access_token": access,
            "refresh_token": "RT",
            "id_token": id
        })
        .to_string();
        let parsed = parse_token_response_json(&body, false).unwrap();
        let rec = map_to_record(parsed, None).unwrap();
        assert_eq!(rec.account.email.as_deref(), Some("u@example.com"));
    }

    #[test]
    fn refresh_token_expired_code_maps_to_refresh_expired() {
        assert_refresh_expired(r#"{"error":{"code":"refresh_token_expired"}}"#);
    }

    #[test]
    fn refresh_token_reused_code_maps_to_refresh_expired() {
        assert_refresh_expired(r#"{"error":{"code":"refresh_token_reused"}}"#);
    }

    #[test]
    fn refresh_token_invalidated_code_maps_to_refresh_expired() {
        assert_refresh_expired(r#"{"error":{"code":"refresh_token_invalidated"}}"#);
    }

    #[test]
    fn refresh_token_dead_code_as_bare_error_string() {
        // The code may also arrive as a bare `error` string rather than
        // a nested object.
        assert_refresh_expired(r#"{"error":"refresh_token_reused"}"#);
    }

    fn assert_refresh_expired(body: &str) {
        let err = check_status_error(
            reqwest::StatusCode::UNAUTHORIZED,
            "https://auth.openai.com/oauth/token",
            body,
            true,
        )
        .unwrap_err();
        match err {
            OAuthError::RefreshExpired(p) => assert_eq!(p, "codex"),
            other => panic!("expected RefreshExpired, got {other:?}"),
        }
    }

    #[test]
    fn generic_401_on_refresh_is_token_endpoint() {
        // A 401 whose code is not one of the three terminal refresh
        // errors must NOT map to RefreshExpired -- it is a generic
        // token-endpoint failure (and must not echo the body).
        let err = check_status_error(
            reqwest::StatusCode::UNAUTHORIZED,
            "https://auth.openai.com/oauth/token",
            r#"{"error":"invalid_client"}"#,
            true,
        )
        .unwrap_err();
        match err {
            OAuthError::TokenEndpoint(m) => {
                assert!(m.contains("401"), "got: {m}");
                assert!(!m.contains("invalid_client"), "refresh body leaked: {m}");
            }
            other => panic!("expected TokenEndpoint, got {other:?}"),
        }
    }

    #[test]
    fn exchange_4xx_keeps_body_for_triage() {
        let err = check_status_error(
            reqwest::StatusCode::BAD_REQUEST,
            "https://auth.openai.com/oauth/token",
            r#"{"error":"invalid_grant","error_description":"code expired"}"#,
            false,
        )
        .unwrap_err();
        match err {
            OAuthError::TokenEndpoint(m) => {
                assert!(m.contains("400"), "got: {m}");
                assert!(m.contains("invalid_grant"), "got: {m}");
            }
            other => panic!("expected TokenEndpoint, got {other:?}"),
        }
    }

    #[test]
    fn malformed_jwt_surfaces_token_endpoint_error() {
        let body = serde_json::json!({
            "access_token": "not-a-jwt",
            "refresh_token": "RT"
        })
        .to_string();
        let parsed = parse_token_response_json(&body, false).unwrap();
        let err = map_to_record(parsed, None).unwrap_err();
        assert!(matches!(err, OAuthError::TokenEndpoint(_)));
    }

    #[test]
    fn extract_error_code_reads_top_level_code() {
        assert_eq!(
            extract_error_code(r#"{"code":"refresh_token_expired"}"#).as_deref(),
            Some("refresh_token_expired")
        );
    }
}
