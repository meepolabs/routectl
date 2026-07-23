//! Google "Antigravity" OAuth flow (Gemini via the Cloud Code surface).
//!
//! Constants extracted from the CLIProxyAPI reference implementation
//! (`internal/auth/antigravity/constants.go`). These are the production
//! values baked into the public Antigravity desktop client; the
//! `client_secret` below is a *public* desktop/installed-app secret
//! (distributed inside the client binary), not a routectl-managed
//! credential -- it is the antigravity analogue of the hardcoded
//! `client_id`s already carried by the Anthropic and codex flows. It is
//! safe to commit for the same reason: Google's installed-app client
//! type treats it as non-confidential.
//!
//! Surface map:
//! - Authorize URL: <https://accounts.google.com/o/oauth2/v2/auth>
//! - Token URL:     <https://oauth2.googleapis.com/token>
//!   (used for BOTH the authorization_code exchange and refresh_token)
//!
//! Differences versus the Anthropic / codex flows:
//! 1. Confidential-style client: the token-endpoint POSTs carry
//!    `client_secret` (form-urlencoded, Google's documented shape) and
//!    the authorize URL does NOT send a PKCE `code_challenge`. The login
//!    driver still mints a PKCE verifier; antigravity ignores it (sending
//!    a verifier Google never issued a challenge for would be rejected).
//! 2. `access_type=offline` + `prompt=consent` are load-bearing: without
//!    them Google's token endpoint will not mint a refresh_token, so
//!    routectl could never refresh.
//! 3. Standard `expires_in` (seconds) in the token response, so expiry is
//!    `now + expires_in` -- no JWT `exp` parsing (codex) needed.
//! 4. Google rotates refresh tokens lazily: the refresh response usually
//!    omits `refresh_token`, so the prior one is preserved (as codex does).
//!
//! The access_token authenticates against the Cloud Code private API
//! (`cloudcode-pa.googleapis.com/v1internal`), NOT the public
//! `generativelanguage.googleapis.com` Gemini surface. Wiring the egress
//! that consumes this token (request envelope, project-id onboarding) is
//! a separate slice; this file only owns token acquisition + refresh.

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use url::Url;

use crate::oauth::providers::{AuthParams, OAuthFlow, refresh_classify, truncate};
use crate::oauth::types::{AccountInfo, SecretToken, TokenRecord, unix_now};
use crate::oauth::{OAuthError, OAuthResult};

/// Public installed-app client credentials for the Antigravity surface.
/// See the module header: the secret is non-confidential by Google's
/// installed-app client model.
const CLIENT_ID: &str = "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
const CLIENT_SECRET: &str = "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";

const AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// The upstream registered this exact `localhost` redirect against the
/// OAuth client, so routectl must bind this port and path rather than a
/// kernel-assigned ephemeral one (mirrors codex's fixed-port behavior).
const CALLBACK_PORT: u16 = 51121;
const CALLBACK_PATH: &str = "/oauth-callback";

/// OAuth scopes the Antigravity client requests. `cloud-platform` is the
/// load-bearing one for the Cloud Code surface; the userinfo scopes feed
/// the display-only account email; `cclog` + `experimentsandconfigs`
/// mirror the client's grant so the token endpoint issues the same
/// capabilities the real client gets.
const SCOPES: &[&str] = &[
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

/// Maximum number of bytes routectl will read from the token endpoint
/// response. Real responses are well under 4 KiB; a 64 KiB cap leaves
/// generous headroom without letting a misbehaving or hostile upstream
/// drive the loader toward OOM.
const MAX_TOKEN_BODY_BYTES: usize = 64 * 1024;

pub struct Antigravity;

#[async_trait]
impl OAuthFlow for Antigravity {
    fn provider_id(&self) -> &'static str {
        "antigravity"
    }

    fn display_name(&self) -> &'static str {
        "Google (Antigravity / Gemini)"
    }

    fn callback_path(&self) -> &'static str {
        CALLBACK_PATH
    }

    fn preferred_callback_port(&self) -> Option<u16> {
        Some(CALLBACK_PORT)
    }

    fn callback_port_candidates(&self) -> Vec<u16> {
        // Antigravity registered its redirect against exactly one fixed
        // port. Port 1457 (the codex fallback) is NOT in antigravity's
        // allow-list, so the candidate list is intentionally single-entry:
        // a port-busy failure is a clear signal for the operator rather
        // than a silent mismatch on an unregistered redirect URI.
        vec![CALLBACK_PORT]
    }

    /// Antigravity has no headless "paste the code" landing page; the
    /// flow always runs through the local callback server. Returning the
    /// authorize URL here keeps `--print-url` from pointing at a dead
    /// endpoint, but operators should use the default browser flow.
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
            // offline + consent are required for Google to mint a
            // refresh_token; without them routectl could never refresh.
            .append_pair("access_type", "offline")
            .append_pair("prompt", "consent")
            // NOTE: deliberately no `code_challenge` -- this is a
            // client_secret confidential flow, not PKCE.
            .append_pair("state", params.state);
        url
    }

    async fn exchange_code(
        &self,
        http: &reqwest::Client,
        code: &str,
        _verifier: &str,
        _state: &str,
        redirect_uri: &str,
    ) -> OAuthResult<TokenRecord> {
        // Google's token endpoint takes the authorization_code grant as
        // form-urlencoded with client_id + client_secret. The PKCE
        // verifier is intentionally unused (no challenge was sent); the
        // CSRF `state` is validated by the callback server, not echoed
        // here.
        let form = [
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
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
        let form = [
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
        ];

        // The prior refresh token's sha8 is the only correlation id an
        // operator has across the pre / post / error legs of a refresh
        // attempt -- emitted on every event so interleaved refreshes can
        // be told apart without ever logging the token VALUE.
        let prior_sha8 = super::sha8(refresh_token);
        tracing::debug!(
            grant_type = "refresh_token",
            refresh_token_sha8 = %prior_sha8,
            "antigravity refresh request"
        );

        let resp = http
            .post(TOKEN_URL)
            .form(&form)
            .send()
            .await
            .map_err(|e| OAuthError::Network(format!("refresh endpoint POST: {e}")))?;

        decode_token_response_traced(resp, Some(refresh_token), &prior_sha8).await
    }
}

/// Internal token-endpoint response shape. Every field is optional so a
/// refresh response (which omits `refresh_token`) and a thin error body
/// both deserialize without panicking; the mapping layer enforces what
/// each flow actually requires.
#[derive(serde::Deserialize)]
struct Resp {
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
}

/// Display-only claims read out of the id_token JWT when one is present.
/// Never trusted for auth -- purely to populate `routectl whoami`.
#[derive(serde::Deserialize, Default)]
struct IdClaims {
    #[serde(default)]
    email: Option<String>,
}

/// Shared decoder for both `exchange_code` and `refresh_token`.
/// `prior_refresh` is `Some` on the refresh path (the previous refresh
/// token, used as the fallback when Google omits a fresh one); `None` on
/// exchange, where a missing refresh_token is a hard error -- a login
/// that yields no refresh token can never be refreshed later.
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

/// Refresh-only variant of [`decode_token_response`] that emits
/// tracing events keyed off the prior refresh token's sha8 so operators
/// can correlate a 401 across logs without ever seeing token VALUES. The
/// failure-side events deliberately do NOT echo the response body: a
/// token-endpoint error envelope can carry the long-lived refresh token,
/// and logging it verbatim would defeat the bearer-redaction contract.
async fn decode_token_response_traced(
    resp: reqwest::Response,
    prior_refresh: Option<&str>,
    prior_refresh_sha8: &str,
) -> OAuthResult<TokenRecord> {
    let status = resp.status();
    let url = resp.url().to_string();
    let body = read_capped_body(resp).await?;

    if let Err(e) = check_status_error(status, &url, &body, prior_refresh.is_some()) {
        tracing::error!(
            status = %status.as_u16(),
            error_kind = %error_kind_label(&e),
            prior_refresh_token_sha8 = %prior_refresh_sha8,
            "antigravity refresh failed"
        );
        return Err(e);
    }

    let parsed = match parse_token_response_json(&body, prior_refresh.is_some()) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!(
                status = %status.as_u16(),
                error_kind = %error_kind_label(&e),
                prior_refresh_token_sha8 = %prior_refresh_sha8,
                "antigravity refresh failed"
            );
            return Err(e);
        }
    };

    let new_refresh_present = parsed.refresh_token.is_some();
    let new_refresh_sha8 = parsed.refresh_token.as_deref().map(super::sha8);
    let expires_in = parsed.expires_in.unwrap_or(0);

    let record = map_to_record(parsed, prior_refresh)?;

    tracing::debug!(
        status = %status.as_u16(),
        new_refresh_token_present = %new_refresh_present,
        new_refresh_token_sha8 = %new_refresh_sha8.as_deref().unwrap_or("-"),
        expires_in = %expires_in,
        "antigravity refresh response"
    );

    Ok(record)
}

/// Short label for the error variant, used in the structured
/// `error_kind` field on refresh-failure trace events.
const fn error_kind_label(e: &OAuthError) -> &'static str {
    match e {
        OAuthError::Network(_) => "network",
        OAuthError::TokenEndpoint(_) => "token_endpoint",
        OAuthError::RefreshExpired(_) => "refresh_expired",
        _ => "other",
    }
}

/// Pull the response body as UTF-8, capped at `MAX_TOKEN_BODY_BYTES`.
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

/// Map status + body to an `OAuthError`. On the refresh path a 400/401
/// whose `error` field is `invalid_grant` means Google has revoked or
/// expired the refresh token -> `RefreshExpired` (operator guidance:
/// re-run login). The status gate matters: a transient 5xx whose body
/// incidentally carries `invalid_grant` must NOT terminate the refresh --
/// it falls through to the generic `TokenEndpoint` path so the credential
/// survives a retry. Any other refresh failure maps to `TokenEndpoint`
/// without echoing the body (it may carry the long-lived refresh token);
/// exchange failures keep the truncated body since the auth code is
/// single-use and short-lived.
fn check_status_error(
    status: reqwest::StatusCode,
    url: &str,
    body: &str,
    is_refresh: bool,
) -> OAuthResult<()> {
    if status.is_success() {
        return Ok(());
    }
    let invalid_grant_status =
        status == reqwest::StatusCode::BAD_REQUEST || status == reqwest::StatusCode::UNAUTHORIZED;
    if is_refresh && invalid_grant_status && refresh_classify::is_invalid_grant(body) {
        return Err(OAuthError::RefreshExpired("antigravity".into()));
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

/// Parse the JSON body into the internal `Resp` shape. Flow-aware so a
/// malformed refresh response does not echo the body (it may carry the
/// long-lived refresh token); exchange responses keep the truncated body
/// for operator triage since the auth code is single-use.
fn parse_token_response_json(body: &str, is_refresh: bool) -> OAuthResult<Resp> {
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
/// - `expires_at_unix` is `now + expires_in` (a missing/zero `expires_in`
///   saturates to "already expired", the safe direction).
/// - `refresh_token`: a present-but-empty value is treated the same as an
///   absent one -- it can never be used to refresh. On refresh, fall back
///   to `prior_refresh` when Google omits (or empties) it. On exchange
///   (`prior_refresh == None`), a missing refresh_token is a hard error.
/// - `email` is best-effort from the id_token JWT when present (display
///   only); absent for the standard antigravity grant, which is fine.
fn map_to_record(parsed: Resp, prior_refresh: Option<&str>) -> OAuthResult<TokenRecord> {
    let access_token = parsed
        .access_token
        .ok_or_else(|| OAuthError::TokenEndpoint("token response missing access_token".into()))?;

    let refresh_token = match (
        parsed.refresh_token.filter(|rt| !rt.is_empty()),
        prior_refresh,
    ) {
        (Some(rt), _) => rt,
        (None, Some(prior)) => prior.to_string(),
        (None, None) => {
            return Err(OAuthError::TokenEndpoint(
                "token response missing refresh_token".into(),
            ));
        }
    };

    let expires_at_unix = unix_now().saturating_add(parsed.expires_in.unwrap_or(0));

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
        account: AccountInfo {
            email,
            account_id: None,
        },
        obtained_at_unix: unix_now(),
        session_id: None,
        cloud_project_id: None,
    })
}

/// Decode a base64url-no-pad JWT payload into `T`. The signature is NOT
/// verified (the upstream is the verifier; routectl never makes a trust
/// decision on these claims -- they are display-only). Returns
/// `TokenEndpoint` on a malformed JWT rather than panicking.
fn decode_jwt_payload<T: serde::de::DeserializeOwned>(jwt: &str) -> OAuthResult<T> {
    let mut parts = jwt.split('.');
    let payload_b64 = match (parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s)) if !h.is_empty() && !p.is_empty() && !s.is_empty() => p,
        _ => return Err(OAuthError::TokenEndpoint("id_token is not a JWT".into())),
    };
    let bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| OAuthError::TokenEndpoint(format!("JWT payload base64 decode: {e}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| OAuthError::TokenEndpoint(format!("JWT payload JSON parse: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use routectl_testkit::with_capture;

    /// Build a synthetic unsigned JWT (`header.payload.sig`) from a
    /// payload JSON `Value`. The signature segment is non-empty filler --
    /// routectl never verifies it.
    fn jwt(payload: serde_json::Value) -> String {
        let enc = |b: &[u8]| URL_SAFE_NO_PAD.encode(b);
        let header = enc(br#"{"alg":"none","typ":"JWT"}"#);
        let body = enc(payload.to_string().as_bytes());
        let sig = enc(b"sig");
        format!("{header}.{body}.{sig}")
    }

    #[test]
    fn auth_url_has_offline_consent_and_no_pkce() {
        let url = Antigravity.auth_url(&AuthParams {
            challenge: "CHAL",
            state: "STATE",
            redirect_uri: "http://localhost:51121/oauth-callback",
        });
        let s = url.as_str();
        assert!(s.starts_with(AUTHORIZE_URL), "got: {s}");
        assert!(s.contains("response_type=code"));
        assert!(s.contains(&format!("client_id={CLIENT_ID}")));
        assert!(s.contains("access_type=offline"), "missing offline: {s}");
        assert!(s.contains("prompt=consent"), "missing consent: {s}");
        assert!(s.contains("state=STATE"));
        // Confidential flow: a PKCE challenge must NOT be present (Google
        // would then demand a verifier we never send).
        assert!(
            !s.contains("code_challenge"),
            "antigravity must not send a PKCE challenge: {s}"
        );
        assert!(s.contains("cloud-platform"), "scopes missing: {s}");
    }

    #[test]
    fn callback_port_and_path_match_registered_redirect() {
        assert_eq!(Antigravity.preferred_callback_port(), Some(51121));
        assert_eq!(Antigravity.callback_path(), "/oauth-callback");
        assert_eq!(Antigravity.callback_port_candidates(), vec![51121]);
    }

    #[test]
    fn exchange_maps_expires_in_to_absolute_expiry() {
        let body = serde_json::json!({
            "access_token": "AT",
            "refresh_token": "RT",
            "token_type": "Bearer",
            "expires_in": 3600,
            "scope": "https://www.googleapis.com/auth/cloud-platform"
        })
        .to_string();
        let before = unix_now();
        let parsed = parse_token_response_json(&body, false).unwrap();
        let rec = map_to_record(parsed, None).unwrap();
        assert_eq!(rec.access_token.expose(), "AT");
        assert_eq!(rec.refresh_token.expose(), "RT");
        assert!(
            rec.expires_at_unix >= before + 3600 && rec.expires_at_unix <= unix_now() + 3600,
            "expires_at_unix must be now+expires_in, got {}",
            rec.expires_at_unix
        );
        assert_eq!(rec.scopes.len(), 1);
    }

    #[test]
    fn refresh_omitting_refresh_token_preserves_prior() {
        // Google rotates lazily: a refresh response with no
        // `refresh_token` must keep the prior one.
        let body = serde_json::json!({
            "access_token": "AT2",
            "expires_in": 3600,
            "token_type": "Bearer"
        })
        .to_string();
        let parsed = parse_token_response_json(&body, true).unwrap();
        let rec = map_to_record(parsed, Some("PRIOR-RT")).unwrap();
        assert_eq!(rec.refresh_token.expose(), "PRIOR-RT");
    }

    #[test]
    fn refresh_with_empty_refresh_token_preserves_prior() {
        // A present-but-empty refresh_token on the refresh path is treated
        // as absent -- fall back to the prior validated token rather than
        // storing the unusable empty value.
        let body = serde_json::json!({
            "access_token": "AT2",
            "refresh_token": "",
            "expires_in": 3600
        })
        .to_string();
        let parsed = parse_token_response_json(&body, true).unwrap();
        let rec = map_to_record(parsed, Some("PRIOR-RT")).unwrap();
        assert_eq!(rec.refresh_token.expose(), "PRIOR-RT");
    }

    #[test]
    fn refresh_with_new_refresh_token_uses_new() {
        let body = serde_json::json!({
            "access_token": "AT2",
            "refresh_token": "NEW-RT",
            "expires_in": 3600
        })
        .to_string();
        let parsed = parse_token_response_json(&body, true).unwrap();
        let rec = map_to_record(parsed, Some("PRIOR-RT")).unwrap();
        assert_eq!(rec.refresh_token.expose(), "NEW-RT");
    }

    #[test]
    fn exchange_missing_refresh_token_is_error() {
        let body = serde_json::json!({ "access_token": "AT", "expires_in": 3600 }).to_string();
        let parsed = parse_token_response_json(&body, false).unwrap();
        let err = map_to_record(parsed, None).unwrap_err();
        match err {
            OAuthError::TokenEndpoint(m) => assert!(m.contains("refresh_token"), "got: {m}"),
            other => panic!("expected TokenEndpoint, got {other:?}"),
        }
    }

    #[test]
    fn email_pulled_from_id_token_when_present() {
        let id = jwt(serde_json::json!({ "email": "u@example.com" }));
        let body = serde_json::json!({
            "access_token": "AT",
            "refresh_token": "RT",
            "expires_in": 3600,
            "id_token": id
        })
        .to_string();
        let parsed = parse_token_response_json(&body, false).unwrap();
        let rec = map_to_record(parsed, None).unwrap();
        assert_eq!(rec.account.email.as_deref(), Some("u@example.com"));
    }

    #[test]
    fn missing_id_token_leaves_email_none() {
        let body = serde_json::json!({
            "access_token": "AT",
            "refresh_token": "RT",
            "expires_in": 3600
        })
        .to_string();
        let parsed = parse_token_response_json(&body, false).unwrap();
        let rec = map_to_record(parsed, None).unwrap();
        assert!(rec.account.email.is_none());
    }

    #[test]
    fn invalid_grant_on_refresh_maps_to_refresh_expired() {
        let err = check_status_error(
            reqwest::StatusCode::BAD_REQUEST,
            TOKEN_URL,
            r#"{"error":"invalid_grant","error_description":"Token has been expired or revoked."}"#,
            true,
        )
        .unwrap_err();
        match err {
            OAuthError::RefreshExpired(p) => assert_eq!(p, "antigravity"),
            other => panic!("expected RefreshExpired, got {other:?}"),
        }
    }

    #[test]
    fn invalid_grant_on_401_maps_to_refresh_expired() {
        let err = check_status_error(
            reqwest::StatusCode::UNAUTHORIZED,
            TOKEN_URL,
            r#"{"error":"invalid_grant"}"#,
            true,
        )
        .unwrap_err();
        match err {
            OAuthError::RefreshExpired(p) => assert_eq!(p, "antigravity"),
            other => panic!("expected RefreshExpired, got {other:?}"),
        }
    }

    #[test]
    fn invalid_grant_on_5xx_falls_through_to_token_endpoint() {
        // A transient 5xx whose body incidentally carries invalid_grant
        // must NOT terminate the credential as RefreshExpired -- the
        // refresh should be retryable. It falls through to the generic
        // TokenEndpoint path, which does not echo the body.
        let err = check_status_error(
            reqwest::StatusCode::SERVICE_UNAVAILABLE,
            TOKEN_URL,
            r#"{"error":"invalid_grant","secret_echo":"do-not-log"}"#,
            true,
        )
        .unwrap_err();
        match err {
            OAuthError::TokenEndpoint(m) => {
                assert!(m.contains("503"), "got: {m}");
                assert!(!m.contains("do-not-log"), "refresh body leaked: {m}");
            }
            other => panic!("expected TokenEndpoint, got {other:?}"),
        }
    }

    #[test]
    fn generic_refresh_failure_does_not_leak_body() {
        let err = check_status_error(
            reqwest::StatusCode::UNAUTHORIZED,
            TOKEN_URL,
            r#"{"error":"invalid_client","secret_echo":"do-not-log"}"#,
            true,
        )
        .unwrap_err();
        match err {
            OAuthError::TokenEndpoint(m) => {
                assert!(m.contains("401"), "got: {m}");
                assert!(!m.contains("do-not-log"), "refresh body leaked: {m}");
            }
            other => panic!("expected TokenEndpoint, got {other:?}"),
        }
    }

    #[test]
    fn exchange_4xx_keeps_body_for_triage() {
        let err = check_status_error(
            reqwest::StatusCode::BAD_REQUEST,
            TOKEN_URL,
            r#"{"error":"redirect_uri_mismatch"}"#,
            false,
        )
        .unwrap_err();
        match err {
            OAuthError::TokenEndpoint(m) => {
                assert!(m.contains("400"), "got: {m}");
                assert!(m.contains("redirect_uri_mismatch"), "got: {m}");
            }
            other => panic!("expected TokenEndpoint, got {other:?}"),
        }
    }

    #[test]
    fn malformed_id_token_is_ignored_not_fatal() {
        // A junk id_token must not fail the whole login -- email is
        // best-effort and falls back to None.
        let body = serde_json::json!({
            "access_token": "AT",
            "refresh_token": "RT",
            "expires_in": 3600,
            "id_token": "not-a-jwt"
        })
        .to_string();
        let parsed = parse_token_response_json(&body, false).unwrap();
        let rec = map_to_record(parsed, None).unwrap();
        assert!(rec.account.email.is_none());
    }

    /// Wrap raw bytes + status into a `reqwest::Response` without touching
    /// the network. Takes `Vec<u8>` (not `&str`) so the non-UTF-8 arm can
    /// feed byte sequences that are not valid UTF-8.
    fn synthetic_response(status: u16, body: Vec<u8>) -> reqwest::Response {
        let http_resp: http::Response<bytes::Bytes> = http::Response::builder()
            .status(status)
            .body(bytes::Bytes::from(body))
            .expect("build http response");
        reqwest::Response::from(http_resp)
    }

    // NOTE(T2-1): `read_capped_body` is byte-identical across all four
    // OAuth providers (anthropic/codex/xai/antigravity). These two arms
    // are pinned here on the antigravity copy as the representative; the
    // other three copies are the same source.
    #[tokio::test]
    async fn read_capped_body_rejects_over_cap_response() {
        // A token-endpoint response larger than the 64 KiB cap must be
        // refused with a TokenEndpoint error rather than buffered whole.
        let oversized = vec![b'x'; MAX_TOKEN_BODY_BYTES + 1];
        let resp = synthetic_response(200, oversized);

        let err = read_capped_body(resp).await.unwrap_err();

        match err {
            OAuthError::TokenEndpoint(m) => {
                assert!(m.contains("refusing to load"), "got: {m}");
                assert!(m.contains(&MAX_TOKEN_BODY_BYTES.to_string()), "got: {m}");
            }
            other => panic!("expected TokenEndpoint over-cap error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_capped_body_rejects_non_utf8_response() {
        // Invalid UTF-8 bytes must hard-error, never silently lossy-decode.
        // The error text must NOT echo the raw body content.
        let invalid_utf8 = vec![0xff, 0xfe, 0x80, 0x00];
        let resp = synthetic_response(200, invalid_utf8);

        let err = read_capped_body(resp).await.unwrap_err();

        match err {
            OAuthError::TokenEndpoint(m) => {
                assert!(m.contains("not valid UTF-8"), "got: {m}");
                assert!(!m.contains('\u{fffd}'), "must not echo lossy body: {m}");
            }
            other => panic!("expected TokenEndpoint UTF-8 error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn refresh_success_event_carries_sha8_and_status_no_token_value() {
        // A successful refresh emits the debug event with status,
        // new_refresh_token_present, an 8-hex sha8, and expires_in --
        // and NO token VALUE in any field.
        const NEW_RT: &str = "antigravity-new-refresh-token-CANARY";
        let body = serde_json::json!({
            "access_token": "AT",
            "refresh_token": NEW_RT,
            "token_type": "Bearer",
            "expires_in": 3600,
            "scope": "https://www.googleapis.com/auth/cloud-platform"
        })
        .to_string();
        let resp = synthetic_response(200, body.into_bytes());

        let (result, events) = with_capture(decode_token_response_traced(
            resp,
            Some("PRIOR-RT"),
            "deadbeef",
        ))
        .await;
        result.expect("refresh should succeed");

        let ev = events
            .iter()
            .find(|e| e.message == "antigravity refresh response")
            .unwrap_or_else(|| panic!("no success event captured: {events:#?}"));
        assert_eq!(ev.level, tracing::Level::DEBUG);
        assert_eq!(ev.field("status"), Some("200"));
        assert_eq!(ev.field("new_refresh_token_present"), Some("true"));
        let sha = ev
            .field("new_refresh_token_sha8")
            .expect("new_refresh_token_sha8 field");
        assert_eq!(sha.len(), 8, "sha8 must be 8 hex chars: {sha}");
        for e in &events {
            for (k, v) in &e.fields {
                assert!(
                    !v.contains(NEW_RT),
                    "token value leaked into field {k}: {e:#?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn refresh_expired_emits_error_event_with_refresh_expired_kind() {
        // A 400 invalid_grant on the refresh path maps to RefreshExpired
        // and emits the error event with error_kind + prior sha8, and no
        // body excerpt (the envelope could echo the refresh token).
        const ECHOED_RT: &str = "rt-leakcanary-antigravity-9f1c";
        let body = format!(r#"{{"error":"invalid_grant","refresh_token":"{ECHOED_RT}"}}"#);
        let resp = synthetic_response(400, body.into_bytes());

        let (result, events) = with_capture(decode_token_response_traced(
            resp,
            Some(ECHOED_RT),
            "deadbeef",
        ))
        .await;
        let err = result.expect_err("expected RefreshExpired");
        assert!(matches!(err, OAuthError::RefreshExpired(_)), "got {err:?}");

        let ev = events
            .iter()
            .find(|e| e.message == "antigravity refresh failed")
            .unwrap_or_else(|| panic!("no error event captured: {events:#?}"));
        assert_eq!(ev.level, tracing::Level::ERROR);
        assert_eq!(ev.field("status"), Some("400"));
        assert_eq!(ev.field("error_kind"), Some("refresh_expired"));
        assert_eq!(ev.field("prior_refresh_token_sha8"), Some("deadbeef"));
        for e in &events {
            assert!(
                !e.message.contains(ECHOED_RT),
                "token leaked into message: {e:#?}"
            );
            for (k, v) in &e.fields {
                assert!(
                    !v.contains(ECHOED_RT),
                    "token leaked into field {k}: {e:#?}"
                );
            }
        }
    }
}
