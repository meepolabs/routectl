//! xAI (Grok) OAuth flow.
//!
//! Constants verified against the CLIProxyAPI reference implementation
//! (`internal/auth/xai`) and xAI's live OIDC discovery document. This is
//! a public OAuth 2.0 PKCE client (no client_secret) -- the `client_id`
//! below is a public client identifier, the xAI analogue of the hardcoded
//! `client_id`s already carried by the Anthropic and codex flows.
//!
//! Surface map:
//! - Authorize URL: <https://auth.x.ai/oauth2/authorize>
//! - Token URL:     <https://auth.x.ai/oauth2/token>
//!   (used for BOTH the authorization_code exchange and refresh_token)
//!
//! Shape relative to the existing flows:
//! 1. PKCE public client like codex (S256 `code_challenge`, no
//!    client_secret, form-urlencoded authorization_code exchange).
//! 2. Standard `expires_in` (seconds) in the token response, so expiry is
//!    `now + expires_in` like antigravity -- NOT codex's JWT `exp` style.
//! 3. The id_token (when present) is display-only: routectl pulls `email`
//!    and `sub` claims for `routectl whoami` but never verifies the JWT
//!    signature or its nonce claim (the upstream is the verifier).
//! 4. xAI rotates refresh tokens lazily: a refresh response may omit
//!    `refresh_token`, so the prior one is preserved (as codex/antigravity
//!    do).
//!
//! The access_token authenticates against the xAI API
//! (`https://api.x.ai/v1`); wiring the egress that consumes this token is
//! a separate slice. This file only owns token acquisition + refresh.

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::TryRng;
use rand::rngs::SysRng;
use url::Url;

use crate::oauth::providers::{AuthParams, OAuthFlow, truncate};
use crate::oauth::types::{AccountInfo, SecretToken, TokenRecord, unix_now};
use crate::oauth::{OAuthError, OAuthResult};

/// Public PKCE client id for the xAI (Grok) flow. No client_secret --
/// this is a public OAuth client identified solely by `client_id` + PKCE.
const CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";
const AUTHORIZE_URL: &str = "https://auth.x.ai/oauth2/authorize";
const TOKEN_URL: &str = "https://auth.x.ai/oauth2/token";

/// xAI registered its loopback redirect against this exact host, port,
/// and path, so routectl must bind them rather than a kernel-assigned
/// ephemeral port (mirrors codex's / antigravity's fixed-port behavior).
const CALLBACK_HOST: &str = "127.0.0.1";
const CALLBACK_PORT: u16 = 56121;
const CALLBACK_PATH: &str = "/callback";

/// Scopes the xAI client requests. `offline_access` is the load-bearing
/// one for routectl's refresh flow (it makes the token endpoint mint a
/// refresh_token); the openid/profile/email scopes feed the display-only
/// account identity; `grok-cli:access` + `api:access` mirror the client's
/// grant so the token endpoint issues the same capabilities.
const SCOPES: &[&str] = &[
    "openid",
    "profile",
    "email",
    "offline_access",
    "grok-cli:access",
    "api:access",
];

/// Number of random bytes behind the OIDC `nonce`. 32 bytes -> 43 chars
/// base64url, matching the entropy of the PKCE state token.
const NONCE_BYTES: usize = 32;

/// Maximum number of bytes routectl will read from the token endpoint
/// response. Real responses are well under 4 KiB even with a JWT
/// id_token; a 64 KiB cap leaves generous headroom without letting a
/// misbehaving or hostile upstream drive the loader toward OOM.
const MAX_TOKEN_BODY_BYTES: usize = 64 * 1024;

pub struct Xai;

/// Mint a fresh opaque `nonce` for the OIDC authorize request. Uses the
/// same CSPRNG primitive (`SysRng` -> base64url-no-pad) the PKCE module
/// uses for the verifier and state. xAI's authorize endpoint requires a
/// `nonce`, but routectl never verifies the id_token's nonce claim (the
/// id_token is display-only), so a fresh random nonce per call is correct
/// and need not be persisted across the flow.
fn generate_nonce() -> String {
    let mut bytes = [0u8; NONCE_BYTES];
    SysRng
        .try_fill_bytes(&mut bytes)
        .expect("SysRng failed to fill OIDC nonce");
    URL_SAFE_NO_PAD.encode(bytes)
}

#[async_trait]
impl OAuthFlow for Xai {
    fn provider_id(&self) -> &'static str {
        "xai"
    }

    fn display_name(&self) -> &'static str {
        "xAI (Grok)"
    }

    fn callback_path(&self) -> &'static str {
        CALLBACK_PATH
    }

    /// xAI registered its loopback redirect against `127.0.0.1`, NOT
    /// `localhost`, so the advertised redirect_uri host must be the
    /// literal IP or the authorize step rejects the redirect.
    fn callback_host(&self) -> &'static str {
        CALLBACK_HOST
    }

    fn preferred_callback_port(&self) -> Option<u16> {
        Some(CALLBACK_PORT)
    }

    fn callback_port_candidates(&self) -> Vec<u16> {
        // xAI registered its redirect against exactly one fixed port.
        // Port 1457 (the codex fallback) is NOT in xAI's allow-list, so
        // the candidate list is intentionally single-entry: a port-busy
        // failure is a clear signal for the operator rather than a silent
        // mismatch on an unregistered redirect URI.
        vec![CALLBACK_PORT]
    }

    /// xAI has no headless "paste the code" landing page; the flow always
    /// runs through the local callback server. Returning the authorize URL
    /// here keeps `--print-url` from pointing at a dead endpoint, but
    /// operators should use the default browser flow.
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
            .append_pair("state", params.state)
            // OIDC-required by xAI's authorize endpoint. Freshly minted
            // per call; routectl never verifies the returned id_token's
            // nonce claim (see module header), so it need not persist.
            .append_pair("nonce", &generate_nonce())
            // `plan=generic` matches the reference client; `referrer` is
            // our product tag (the reference sent its own product's tag).
            .append_pair("plan", "generic")
            .append_pair("referrer", "routectl");
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
        // xAI's token endpoint takes the authorization_code grant as
        // form-urlencoded per RFC 6749. No client_secret (public PKCE
        // client). `state` is NOT echoed in the body -- the CSRF check
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
        let form = [
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh_token),
        ];

        // The prior refresh token's sha8 is the only correlation id an
        // operator has across the pre / post / error legs of a refresh
        // attempt -- emitted on every event so interleaved refreshes can
        // be told apart without ever logging the token VALUE.
        let prior_sha8 = super::sha8(refresh_token);
        tracing::debug!(
            grant_type = "refresh_token",
            refresh_token_sha8 = %prior_sha8,
            "xai refresh request"
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
/// refresh response (which may omit `refresh_token`) and a thin error body
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
    #[serde(default)]
    sub: Option<String>,
}

/// Shared decoder for both `exchange_code` and `refresh_token`.
/// `prior_refresh` is `Some` on the refresh path (the previous refresh
/// token, used as the fallback when xAI omits a fresh one); `None` on
/// exchange, where a missing refresh_token is a hard error -- a login that
/// yields no refresh token can never be refreshed later.
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

/// Refresh-only variant of [`decode_token_response`] that emits tracing
/// events keyed off the prior refresh token's sha8 so operators can
/// correlate a 401 across logs without ever seeing token VALUES. The
/// failure-side events deliberately do NOT echo the response body: a
/// token-endpoint error envelope can carry the long-lived refresh token,
/// and logging it verbatim would defeat the bearer-redaction contract.
/// `pub(super)` so the providers-module `testing` re-export can hand it
/// to integration tests.
pub(super) async fn decode_token_response_traced(
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
            "xai refresh failed"
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
                "xai refresh failed"
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
        "xai refresh response"
    );

    Ok(record)
}

/// Short label for the error variant, used in the structured `error_kind`
/// field on refresh-failure trace events.
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
/// whose `error` field is `invalid_grant` means xAI has revoked or expired
/// the refresh token -> `RefreshExpired` (operator guidance: re-run
/// login). The status gate matters: a transient 5xx whose body incidentally
/// carries `invalid_grant` must NOT terminate the refresh -- it falls
/// through to the generic `TokenEndpoint` path so the credential survives a
/// retry. Any other refresh failure maps to `TokenEndpoint` without echoing
/// the body (it may carry the long-lived refresh token); exchange failures
/// keep the truncated body since the auth code is single-use and
/// short-lived.
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
    if is_refresh && invalid_grant_status && is_invalid_grant(body) {
        return Err(OAuthError::RefreshExpired("xai".into()));
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

/// True if the token-endpoint error body's `error` field is
/// `invalid_grant` -- xAI's terminal signal for a dead refresh token.
fn is_invalid_grant(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
        .as_deref()
        == Some("invalid_grant")
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
/// - `refresh_token`: on refresh, fall back to `prior_refresh` when xAI
///   omits a fresh one. On exchange (`prior_refresh == None`), a missing
///   refresh_token is a hard error.
/// - `email` + `account_id` are best-effort from the id_token JWT when
///   present (display only); a malformed id_token leaves both `None`
///   rather than failing login.
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
            ));
        }
    };

    let expires_at_unix = unix_now().saturating_add(parsed.expires_in.unwrap_or(0));

    let claims = parsed
        .id_token
        .as_deref()
        .and_then(|jwt| decode_jwt_payload::<IdClaims>(jwt).ok())
        .unwrap_or_default();

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
            email: claims.email,
            account_id: claims.sub,
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
    fn auth_url_has_pkce_nonce_and_xai_params() {
        let url = Xai.auth_url(&AuthParams {
            challenge: "CHAL",
            state: "STATE",
            redirect_uri: "http://127.0.0.1:56121/callback",
        });
        let s = url.as_str();
        assert!(s.starts_with(AUTHORIZE_URL), "got: {s}");
        assert!(s.contains("response_type=code"));
        assert!(s.contains(&format!("client_id={CLIENT_ID}")));
        assert!(s.contains("code_challenge=CHAL"));
        assert!(s.contains("code_challenge_method=S256"));
        assert!(s.contains("state=STATE"));
        // The nonce param must be present and non-empty (it is freshly
        // minted inside auth_url).
        let nonce = url
            .query_pairs()
            .find(|(k, _)| k == "nonce")
            .map(|(_, v)| v.into_owned())
            .expect("nonce param present");
        assert!(!nonce.is_empty(), "nonce must be non-empty");
        assert!(s.contains("plan=generic"), "missing plan=generic: {s}");
        assert!(s.contains("referrer=routectl"), "missing referrer: {s}");
        // Space-joined scopes (url::Url renders space as '+' or '%20').
        assert!(
            s.contains("grok-cli") && s.contains("offline_access"),
            "scopes missing: {s}"
        );
    }

    #[test]
    fn two_auth_urls_mint_distinct_nonces() {
        // The nonce is freshly minted per call; two invocations must not
        // reuse a value.
        let params = AuthParams {
            challenge: "CHAL",
            state: "STATE",
            redirect_uri: "http://127.0.0.1:56121/callback",
        };
        let nonce_of = |u: Url| {
            u.query_pairs()
                .find(|(k, _)| k == "nonce")
                .map(|(_, v)| v.into_owned())
                .unwrap()
        };
        let a = nonce_of(Xai.auth_url(&params));
        let b = nonce_of(Xai.auth_url(&params));
        assert_ne!(a, b, "nonce must be freshly minted per call");
    }

    #[test]
    fn callback_host_port_and_path_match_registered_redirect() {
        assert_eq!(Xai.preferred_callback_port(), Some(56121));
        assert_eq!(Xai.callback_path(), "/callback");
        assert_eq!(Xai.callback_host(), "127.0.0.1");
    }

    #[test]
    fn exchange_maps_expires_in_to_absolute_expiry() {
        let body = serde_json::json!({
            "access_token": "AT",
            "refresh_token": "RT",
            "token_type": "Bearer",
            "expires_in": 3600,
            "scope": "openid grok-cli:access"
        })
        .to_string();
        let before = unix_now();
        let parsed = parse_token_response_json(&body, false).unwrap();
        let rec = map_to_record(parsed, None).unwrap();
        assert_eq!(rec.access_token.expose(), "AT");
        assert_eq!(rec.refresh_token.expose(), "RT");
        assert!(
            rec.expires_at_unix >= before + 3600 && rec.expires_at_unix <= before + 3600 + 5,
            "expires_at_unix must be now+expires_in, got {}",
            rec.expires_at_unix
        );
        assert_eq!(rec.scopes.len(), 2);
    }

    #[test]
    fn refresh_omitting_refresh_token_preserves_prior() {
        // xAI rotates lazily: a refresh response with no `refresh_token`
        // must keep the prior one.
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
    fn email_and_account_id_pulled_from_id_token() {
        let id = jwt(serde_json::json!({ "email": "u@example.com", "sub": "user-123" }));
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
        assert_eq!(rec.account.account_id.as_deref(), Some("user-123"));
    }

    #[test]
    fn missing_id_token_leaves_email_and_account_id_none() {
        let body = serde_json::json!({
            "access_token": "AT",
            "refresh_token": "RT",
            "expires_in": 3600
        })
        .to_string();
        let parsed = parse_token_response_json(&body, false).unwrap();
        let rec = map_to_record(parsed, None).unwrap();
        assert!(rec.account.email.is_none());
        assert!(rec.account.account_id.is_none());
    }

    #[test]
    fn malformed_id_token_is_ignored_not_fatal() {
        // A junk id_token must not fail the whole login -- email and
        // account_id are best-effort and fall back to None.
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
        assert!(rec.account.account_id.is_none());
    }

    #[test]
    fn invalid_grant_on_refresh_maps_to_refresh_expired() {
        let err = check_status_error(
            reqwest::StatusCode::BAD_REQUEST,
            TOKEN_URL,
            r#"{"error":"invalid_grant","error_description":"Token expired or revoked."}"#,
            true,
        )
        .unwrap_err();
        match err {
            OAuthError::RefreshExpired(p) => assert_eq!(p, "xai"),
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
            OAuthError::RefreshExpired(p) => assert_eq!(p, "xai"),
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
}
