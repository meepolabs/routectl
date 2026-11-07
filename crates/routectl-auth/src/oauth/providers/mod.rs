//! Per-upstream OAuth flow definitions.
//!
//! The `OAuthFlow` trait abstracts the vendor-specific bits of an OAuth
//! 2.0 PKCE flow: client_id, scopes, endpoint URLs, callback path, and
//! the (sometimes idiosyncratic) token-endpoint POST body shape.
//!
//! Adding a new provider is one new file in this directory plus a
//! one-line registry entry below. The trait is `pub(crate)`, sealed by
//! visibility -- out-of-tree consumers cannot register providers
//! because the vendor surfaces are reverse-engineered, undocumented,
//! and need code-review discipline when they shift.

pub mod anthropic;
pub mod antigravity;
pub mod codex;
mod refresh_classify;
#[cfg(any(test, feature = "test-utils"))]
mod test_loopback;
pub mod xai;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::de::DeserializeOwned;
use sha2::{Digest, Sha256};
use url::Url;

use crate::oauth::types::TokenRecord;
use crate::oauth::{OAuthError, OAuthResult};

/// Parameters the auth-URL builder needs from the login driver.
/// Provider impls turn these into a vendor-specific URL.
pub struct AuthParams<'a> {
    pub challenge: &'a str,
    pub state: &'a str,
    pub redirect_uri: &'a str,
}

#[async_trait]
pub trait OAuthFlow: Send + Sync {
    /// Stable id used in `oauth://<provider>` URIs and `routectl
    /// login <provider>`.
    fn provider_id(&self) -> &'static str;

    /// Human-friendly name for `routectl whoami` headers.
    fn display_name(&self) -> &'static str;

    /// Build the browser-launch URL with PKCE challenge + CSRF state.
    fn auth_url(&self, params: &AuthParams<'_>) -> Url;

    /// Path the local callback server listens on (`/callback`).
    fn callback_path(&self) -> &'static str {
        "/callback"
    }

    /// Host the advertised redirect_uri is built against. Defaults to
    /// `localhost` (the Anthropic / codex registration), which the login
    /// driver pairs with a 127.0.0.1 bind because browsers resolve
    /// `localhost` back to the loopback. A provider whose public client
    /// registered its redirect against the literal `127.0.0.1` (xAI)
    /// overrides this so the authorize step does not reject the redirect.
    fn callback_host(&self) -> &'static str {
        "localhost"
    }

    /// Fixed local callback port this provider's redirect URIs are
    /// registered against, if any. `None` (the default) means the login
    /// driver binds a kernel-assigned ephemeral port -- the Anthropic
    /// behavior, where the public client allows any `localhost:<port>`
    /// redirect. `Some(port)` means the upstream registered a specific
    /// port (codex: 1455, with a fallback handled by the login driver),
    /// so an ephemeral port would be rejected at the authorize step.
    fn preferred_callback_port(&self) -> Option<u16> {
        None
    }

    /// Ordered list of candidate local ports the login driver tries when
    /// binding the callback listener (browser-launch path only). The
    /// first available port is bound; all candidates are exhausted before
    /// returning an error. An explicit `--callback-port` operator override
    /// bypasses this list entirely.
    ///
    /// The default covers two cases:
    /// - preferred port present (e.g. codex: 1455) -> `[preferred, 1457]`,
    ///   so a single busy port does not abort the login.
    /// - no preferred port (e.g. Anthropic) -> `[0]` for a kernel-assigned
    ///   ephemeral port.
    ///
    /// A provider whose redirect URIs are registered against exactly one
    /// fixed port and where 1457 is NOT in its allow-list MUST override
    /// to return only `[preferred_port]`, preventing the driver from falling
    /// back to a codex-specific port and advertising an unregistered
    /// redirect URI.
    fn callback_port_candidates(&self) -> Vec<u16> {
        match self.preferred_callback_port() {
            Some(p) => vec![p, 1457],
            None => vec![0],
        }
    }

    /// URL the upstream displays to operators using the headless
    /// `--print-url` flow. This is the value the operator's browser
    /// is redirected to after consent; routectl never serves it.
    /// Provider-specific because each upstream owns its own "manual
    /// paste" landing page.
    fn manual_redirect_url(&self) -> &'static str;

    /// POST to the token endpoint with `grant_type=authorization_code`.
    /// Provider-specific because some upstreams send JSON, others
    /// form-urlencoded; some require extra headers
    /// (e.g. `anthropic-beta: oauth-2025-04-20`); some surface
    /// `account` info inline, others via id_token claims. Some upstreams
    /// (Anthropic) also expect the CSRF `state` echoed in the body
    /// despite RFC 6749 not requiring it.
    async fn exchange_code(
        &self,
        http: &reqwest::Client,
        code: &str,
        verifier: &str,
        state: &str,
        redirect_uri: &str,
    ) -> OAuthResult<TokenRecord>;

    /// POST to the token endpoint with `grant_type=refresh_token`.
    /// Provider-specific for the same reasons as `exchange_code`.
    /// Called from `OAuthStore::refresh_under_lock` for both the
    /// near-expiry (`get`) and 401-recovery (`on_auth_failure`) paths.
    async fn refresh_token(
        &self,
        http: &reqwest::Client,
        refresh_token: &str,
    ) -> OAuthResult<TokenRecord>;
}

/// Look up a provider impl by id. Returns `OAuthError::UnknownProvider`
/// (whose Display drives the operator-visible "known: ..." message
/// from `known_provider_ids`) if the id is not registered.
pub fn lookup(provider_id: &str) -> OAuthResult<&'static dyn OAuthFlow> {
    match provider_id {
        "anthropic" => Ok(&anthropic::Anthropic),
        "codex" => Ok(&codex::Codex),
        "xai" => Ok(&xai::Xai),
        "antigravity" => Ok(&antigravity::Antigravity),
        #[cfg(any(test, feature = "test-utils"))]
        test_loopback::PROVIDER_ID => Ok(&test_loopback::TestLoopback),
        other => Err(OAuthError::UnknownProvider(other.to_string())),
    }
}

/// All known provider ids. Used by the in-crate consistency test below
/// and by the login arg-validator. Kept as a single source of truth
/// alongside the `lookup` match above so the two stay in sync. Re-exported
/// at the `oauth` module root (`oauth::known_provider_ids`) so the CLI can
/// build its clap allowed-provider set from this list, keeping the accepted
/// set in lockstep with the registry. The operator-visible "unknown oauth
/// provider" text is built from `OAuthError::Display`.
pub const fn known_provider_ids() -> &'static [&'static str] {
    &["anthropic", "codex", "xai", "antigravity"]
}

/// Test-only re-exports for the integration tests under
/// `crates/routectl-auth/tests/`. Hidden from rustdoc and gated by
/// `#[doc(hidden)]` so this is not part of the supported public API.
#[doc(hidden)]
pub mod testing {
    use super::OAuthFlow;
    use super::codex::{Codex, decode_token_response_traced};
    use super::xai::{Xai, decode_token_response_traced as xai_decode_token_response_traced};
    use crate::oauth::OAuthResult;
    use crate::oauth::types::TokenRecord;

    /// Drive the codex provider's refresh-token leg. Wraps the
    /// `OAuthFlow::refresh_token` call so the integration test can
    /// invoke it without taking a dep on the crate-private trait.
    pub async fn codex_refresh(
        http: &reqwest::Client,
        refresh_token: &str,
    ) -> OAuthResult<TokenRecord> {
        Codex.refresh_token(http, refresh_token).await
    }

    /// Drive the response-side of a refresh-token call through the
    /// tracing-instrumented decoder. The integration test passes a
    /// synthetic `reqwest::Response`, asserts on the captured events,
    /// and uses the `Result` shape to confirm the error mapping is
    /// preserved alongside the trace emission.
    pub async fn decode_refresh_response_traced(
        resp: reqwest::Response,
        prior_refresh: Option<&str>,
        prior_refresh_sha8: &str,
    ) -> OAuthResult<TokenRecord> {
        decode_token_response_traced(resp, prior_refresh, prior_refresh_sha8).await
    }

    /// Drive the xai provider's refresh-token leg. Wraps the
    /// `OAuthFlow::refresh_token` call so the integration test can
    /// invoke it without taking a dep on the crate-private trait.
    pub async fn xai_refresh(
        http: &reqwest::Client,
        refresh_token: &str,
    ) -> OAuthResult<TokenRecord> {
        Xai.refresh_token(http, refresh_token).await
    }

    /// Drive the response-side of an xai refresh-token call through the
    /// tracing-instrumented decoder. The integration test passes a
    /// synthetic `reqwest::Response`, asserts on the captured events,
    /// and uses the `Result` shape to confirm the error mapping is
    /// preserved alongside the trace emission.
    pub async fn decode_xai_refresh_response_traced(
        resp: reqwest::Response,
        prior_refresh: Option<&str>,
        prior_refresh_sha8: &str,
    ) -> OAuthResult<TokenRecord> {
        xai_decode_token_response_traced(resp, prior_refresh, prior_refresh_sha8).await
    }
}

/// Build a body-free `TokenEndpoint` error from a serde_json parse
/// failure of a token-exchange response.
///
/// The raw response body carries freshly minted access/refresh tokens,
/// and serde's own `Display` echoes offending value fragments (`invalid
/// type: string "sk-...", ...`), so NEITHER the body nor the serde
/// message may reach an operator-visible error or log. Emit only the
/// error category plus its line/column position -- secret-free integers
/// that are enough to triage a shape mismatch. Shared by every provider's
/// `parse_token_response_json` so the four cannot drift.
pub(super) fn token_parse_error(e: &serde_json::Error) -> OAuthError {
    let kind = match e.classify() {
        serde_json::error::Category::Io => "io",
        serde_json::error::Category::Syntax => "syntax",
        serde_json::error::Category::Data => "data",
        serde_json::error::Category::Eof => "eof",
    };
    OAuthError::TokenEndpoint(format!(
        "parse token response: {kind} error at line {} column {}",
        e.line(),
        e.column()
    ))
}

/// Longest OAuth error code we will echo. RFC 6749 section 5.2 codes
/// (`invalid_grant`, `invalid_client`, `unsupported_grant_type`) and the
/// provider refresh codes (`refresh_token_expired`) are short snake_case
/// tokens; a value longer than this is not a code and is dropped.
const MAX_TOKEN_ERROR_CODE_LEN: usize = 48;

/// Extract a machine-usable OAuth error code from a non-2xx token-endpoint
/// body, fail-safe by SHAPE. Returns `Some(code)` only when a recognized
/// error field (`error` as a string, `error.code`, or a top-level `code`)
/// holds a value matching the tightly-bounded OAuth-error-code shape:
/// 1..=`MAX_TOKEN_ERROR_CODE_LEN` bytes drawn only from `[a-z0-9_]`. Any
/// other value fails the check and is dropped -- an echoed authorization
/// code, PKCE verifier, or partially minted token carries uppercase, `-`,
/// `.`, or over-length runs, so it can never survive as a "code".
pub(super) fn safe_token_error_code(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let raw = token_error_code_field(&v)?;
    is_token_error_code_shape(&raw).then_some(raw)
}

/// Pull the candidate error-code string from the recognized JSON fields,
/// without shape-validating it (the caller does that).
fn token_error_code_field(v: &serde_json::Value) -> Option<String> {
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

/// Whether `s` matches the bounded OAuth-error-code shape (nonempty, at
/// most `MAX_TOKEN_ERROR_CODE_LEN` bytes, only `[a-z0-9_]`).
fn is_token_error_code_shape(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_TOKEN_ERROR_CODE_LEN
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Build the `TokenEndpoint` error for a non-2xx token-endpoint response
/// WITHOUT embedding the raw body. The exchange body carries the single-use
/// authorization code plus the PKCE verifier and the refresh body carries
/// the long-lived refresh token; an intercepting proxy can echo either back
/// in an error envelope, so the body is never surfaced. Only `status` +
/// `url` are kept, plus a machine-usable error code IF the body holds one in
/// the tightly-bounded shape (see [`safe_token_error_code`]). Shared by every
/// provider's `check_status_error` so the four cannot drift.
pub(super) fn token_status_error(status: reqwest::StatusCode, url: &str, body: &str) -> OAuthError {
    match safe_token_error_code(body) {
        Some(code) => OAuthError::TokenEndpoint(format!("{} {} ({code})", status.as_u16(), url)),
        None => OAuthError::TokenEndpoint(format!("{} {}", status.as_u16(), url)),
    }
}

/// First 4 bytes of `SHA-256(token)` rendered as 8 lowercase hex
/// chars. Stable, deterministic correlation id for refresh-flow trace
/// events without leaking token VALUES. 4 bytes (32 bits) gives a
/// collision probability that is fine for log correlation across a
/// single operator's refresh history -- this is NOT a cryptographic
/// commitment, just a logging tag.
///
/// Shared by the Anthropic and codex provider impls so both refresh
/// paths emit structurally identical trace events keyed off the same
/// sha8 computation.
pub(super) fn sha8(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut out = String::with_capacity(8);
    for b in &digest[..4] {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Maximum number of bytes routectl will read from a token endpoint
/// response. Real responses are well under 4 KiB even with a JWT
/// access_token or id_token; a 64 KiB cap leaves generous headroom for
/// future fields without letting a misbehaving or hostile upstream drive
/// the loader toward OOM.
pub(super) const MAX_TOKEN_BODY_BYTES: usize = 64 * 1024;

/// Where a provider's token-endpoint POSTs go. `url` and `allowed_host`
/// are repo constants: [`send_token_request`] refuses to send unless the
/// built request's URL passes [`check_token_url`] against `allowed_host`
/// and is unchanged from the parsed `url`.
pub(super) struct TokenEndpoint {
    pub(super) url: &'static str,
    pub(super) client_id: &'static str,
    pub(super) allowed_host: &'static str,
}

/// The one URL rule for token-endpoint traffic: https, no userinfo, a
/// host string exactly equal to `allowed_host` (no suffix or subdomain
/// match), and no explicit port (an explicit `:443` normalizes away on
/// parse and passes). Refusals name the failed rule only; the URL itself
/// is never echoed.
pub(super) fn check_token_url(url: &Url, allowed_host: &str) -> OAuthResult<()> {
    if url.scheme() != "https" {
        return Err(OAuthError::TokenEndpoint(
            "token endpoint URL is not https".into(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(OAuthError::TokenEndpoint(
            "token endpoint URL carries userinfo".into(),
        ));
    }
    if url.host_str() != Some(allowed_host) {
        return Err(OAuthError::TokenEndpoint(
            "token endpoint host is not the expected host".into(),
        ));
    }
    if url.port().is_some() {
        return Err(OAuthError::TokenEndpoint(
            "token endpoint URL carries an explicit port".into(),
        ));
    }
    Ok(())
}

/// The check on the request about to be executed. reqwest moves URL userinfo
/// into an Authorization header when the builder is created, so the URL
/// rule alone cannot see it on the built request; requiring the built URL to
/// equal the parsed endpoint URL refuses that and any other rewrite.
fn check_built_request_url(built: &Url, endpoint_url: &Url, allowed_host: &str) -> OAuthResult<()> {
    check_token_url(built, allowed_host)?;
    if built != endpoint_url {
        return Err(OAuthError::TokenEndpoint(
            "built request URL differs from the token endpoint URL".into(),
        ));
    }
    Ok(())
}

/// POST once to `endpoint`. The vendor closure adds the body and headers to
/// a builder bound to the parsed URL; the finished request's URL is then
/// checked, so the URL that is executed is the URL that was checked, and
/// nothing is sent when the check fails. A transport failure maps to
/// `OAuthError::Network` prefixed with `network_label`. No retries.
pub(super) async fn send_token_request(
    http: &reqwest::Client,
    endpoint: &TokenEndpoint,
    network_label: &'static str,
    build: impl FnOnce(reqwest::RequestBuilder) -> reqwest::RequestBuilder,
) -> OAuthResult<reqwest::Response> {
    let url = Url::parse(endpoint.url).map_err(|e| {
        OAuthError::TokenEndpoint(format!("token endpoint URL does not parse: {e}"))
    })?;
    let req = build(http.post(url.clone()))
        .build()
        .map_err(|e| OAuthError::TokenEndpoint(format!("token request does not build: {e}")))?;
    check_built_request_url(req.url(), &url, endpoint.allowed_host)?;
    http.execute(req)
        .await
        .map_err(|e| OAuthError::Network(format!("{network_label}: {e}")))
}

/// Short label for the error variant returned by the token endpoint.
/// Used in the structured `error_kind` field on refresh-failure trace
/// events so operators can grep for the failure mode without scraping
/// the human-readable message.
pub(super) const fn error_kind_label(e: &OAuthError) -> &'static str {
    match e {
        OAuthError::Network(_) => "network",
        OAuthError::TokenEndpoint(_) => "token_endpoint",
        OAuthError::RefreshExpired(_) => "refresh_expired",
        _ => "other",
    }
}

/// Pull the response body as UTF-8, capped at [`MAX_TOKEN_BODY_BYTES`].
/// A declared `Content-Length` over the cap is refused before any body
/// byte is read; otherwise chunks are accumulated and the read is refused
/// as soon as the running total would cross the cap, so a chunked or
/// understated body never grows the buffer past it. Token endpoints speak
/// JSON; non-UTF-8 is a hard error rather than something to silently lose
/// via `unwrap_or_default`.
pub(super) async fn read_capped_body(mut resp: reqwest::Response) -> OAuthResult<String> {
    if let Some(len) = resp.content_length()
        && len > MAX_TOKEN_BODY_BYTES as u64
    {
        return Err(over_cap_error(&format!("{len} bytes")));
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| OAuthError::Network(format!("read token response body: {e}")))?
    {
        if chunk.len() > MAX_TOKEN_BODY_BYTES - body.len() {
            return Err(over_cap_error("over the cap"));
        }
        body.extend_from_slice(&chunk);
    }
    let s = std::str::from_utf8(&body).map_err(|e| {
        OAuthError::TokenEndpoint(format!("token response is not valid UTF-8: {e}"))
    })?;
    Ok(s.to_string())
}

fn over_cap_error(size: &str) -> OAuthError {
    OAuthError::TokenEndpoint(format!(
        "token response is {size}; refusing to load (cap is {MAX_TOKEN_BODY_BYTES} bytes)"
    ))
}

/// A provider's status-to-error mapping: `(status, url, body, is_refresh)`.
pub(super) type CheckStatusFn = fn(reqwest::StatusCode, &str, &str, bool) -> OAuthResult<()>;

/// Untraced response decoder for both grants: read the capped body, then
/// run the vendor's status mapping, parser and record projection in that
/// order. `prior_refresh` is `Some`
/// on the refresh path (the previous refresh token) and `None` on the
/// exchange path; it selects the refresh arm of `check_status` and is
/// threaded to `map_to_record`.
pub(super) async fn decode_token_response<R>(
    resp: reqwest::Response,
    prior_refresh: Option<&str>,
    check_status: CheckStatusFn,
    parse: fn(&str) -> OAuthResult<R>,
    map_to_record: fn(R, Option<&str>) -> OAuthResult<TokenRecord>,
) -> OAuthResult<TokenRecord> {
    let status = resp.status();
    let url = resp.url().to_string();
    let body = read_capped_body(resp).await?;

    check_status(status, &url, &body, prior_refresh.is_some())?;
    let parsed = parse(&body)?;
    map_to_record(parsed, prior_refresh)
}

/// Decode the payload segment of a JWT (`header.payload.signature`, all
/// three non-empty, payload base64url without padding) into `T`.
///
/// Decode only: the signature is NOT verified and no claim is validated.
/// Callers must treat the result as untrusted hints (refresh timing,
/// display-only identity), never as an authorization decision.
/// `token_label` names the token in the not-a-JWT error.
pub(super) fn decode_jwt_payload<T: DeserializeOwned>(
    jwt: &str,
    token_label: &str,
) -> OAuthResult<T> {
    let mut parts = jwt.split('.');
    let payload_b64 = match (parts.next(), parts.next(), parts.next()) {
        (Some(h), Some(p), Some(s)) if !h.is_empty() && !p.is_empty() && !s.is_empty() => p,
        _ => {
            return Err(OAuthError::TokenEndpoint(format!(
                "{token_label} is not a JWT"
            )));
        }
    };
    let bytes = URL_SAFE_NO_PAD
        .decode(payload_b64)
        .map_err(|e| OAuthError::TokenEndpoint(format!("JWT payload base64 decode: {e}")))?;
    serde_json::from_slice(&bytes)
        .map_err(|e| OAuthError::TokenEndpoint(format!("JWT payload JSON parse: {e}")))
}

#[cfg(test)]
#[path = "token_transport_tests.rs"]
mod token_transport_tests;

#[cfg(test)]
mod tests {
    use super::*;

    const PRODUCTION_ENDPOINTS: [(&str, &TokenEndpoint); 4] = [
        ("anthropic", &anthropic::ENDPOINT),
        ("codex", &codex::ENDPOINT),
        ("xai", &xai::ENDPOINT),
        ("antigravity", &antigravity::ENDPOINT),
    ];

    fn parsed(url: &str) -> Url {
        Url::parse(url).expect("test URL parses")
    }

    #[test]
    fn check_token_url_accepts_every_production_endpoint() {
        for (name, endpoint) in PRODUCTION_ENDPOINTS {
            let result = check_token_url(&parsed(endpoint.url), endpoint.allowed_host);
            assert!(result.is_ok(), "{name}: {result:?}");
        }
    }

    #[test]
    fn check_token_url_accepts_the_default_port_spelled_out() {
        for (name, endpoint) in PRODUCTION_ENDPOINTS {
            let host = endpoint.allowed_host;
            let result = check_token_url(&parsed(&format!("https://{host}:443/x")), host);
            assert!(result.is_ok(), "{name}: {result:?}");
        }
    }

    #[test]
    fn check_token_url_refuses_off_rule_urls() {
        for (name, endpoint) in PRODUCTION_ENDPOINTS {
            let host = endpoint.allowed_host;
            let rows = [
                ("http", format!("http://{host}/token"), "not https"),
                (
                    "userinfo",
                    format!("https://user:pw@{host}/token"),
                    "userinfo",
                ),
                (
                    "username only",
                    format!("https://user@{host}/token"),
                    "userinfo",
                ),
                (
                    "other host",
                    "https://token.example.invalid/token".to_string(),
                    "expected host",
                ),
                (
                    "suffix lookalike",
                    format!("https://{host}.example.invalid/token"),
                    "expected host",
                ),
                (
                    "subdomain lookalike",
                    format!("https://lookalike.{host}/token"),
                    "expected host",
                ),
                (
                    "explicit port",
                    format!("https://{host}:8443/x"),
                    "explicit port",
                ),
            ];
            for (row, url, reason) in rows {
                match check_token_url(&parsed(&url), host) {
                    Err(OAuthError::TokenEndpoint(m)) => {
                        assert!(m.contains(reason), "{name}/{row}: {m}");
                        assert!(!m.contains(host), "{name}/{row}: URL echoed: {m}");
                    }
                    other => panic!("{name}/{row}: expected refusal, got {other:?}"),
                }
            }
        }
    }

    const SINKHOLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

    /// A client that sends every connection for the production token hosts
    /// to a test-owned loopback listener that closes each connection on
    /// accept, so a POST passes the URL check and then fails in transport
    /// without leaving the machine. The listener lives until the returned
    /// task is aborted, so no other test can take its port mid-run.
    async fn sinkhole_client() -> (reqwest::Client, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind loopback");
        let addr = listener.local_addr().expect("listener address");
        let sinkhole = tokio::spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                drop(conn);
            }
        });
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(SINKHOLE_TIMEOUT)
            .timeout(SINKHOLE_TIMEOUT);
        for (_, endpoint) in PRODUCTION_ENDPOINTS {
            builder = builder.resolve(endpoint.allowed_host, addr);
        }
        (builder.build().expect("build client"), sinkhole)
    }

    fn network_message(result: OAuthResult<TokenRecord>) -> String {
        match result {
            Err(OAuthError::Network(m)) => m,
            other => panic!("expected Network error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn transport_failures_carry_the_grant_network_label() {
        let (http, sinkhole) = sinkhole_client().await;
        for id in known_provider_ids() {
            let flow = lookup(id).expect("registered provider");

            let exchange = network_message(
                flow.exchange_code(
                    &http,
                    "code-a",
                    "verifier-a",
                    "state-a",
                    "http://localhost/cb",
                )
                .await,
            );
            let refresh = network_message(flow.refresh_token(&http, "refresh-a").await);

            assert!(
                exchange.starts_with("token endpoint POST: "),
                "{id}: {exchange}"
            );
            assert!(
                refresh.starts_with("refresh endpoint POST: "),
                "{id}: {refresh}"
            );
        }
        sinkhole.abort();
    }

    #[test]
    fn jwt_payload_decode_names_the_token_label_when_not_a_jwt() {
        let rows = [
            ("access_token", "not-a-jwt", "access_token is not a JWT"),
            ("id_token", "not-a-jwt", "id_token is not a JWT"),
            ("access_token", "a..c", "access_token is not a JWT"),
            ("id_token", "a.b", "id_token is not a JWT"),
        ];
        for (label, jwt, expected) in rows {
            match decode_jwt_payload::<serde_json::Value>(jwt, label) {
                Err(OAuthError::TokenEndpoint(m)) => assert_eq!(m, expected, "{label}/{jwt}"),
                other => panic!("{label}/{jwt}: expected TokenEndpoint, got {other:?}"),
            }
        }
    }

    #[test]
    fn jwt_payload_decode_keeps_base64_and_json_messages() {
        let not_json = URL_SAFE_NO_PAD.encode(b"not json");
        let rows = [
            (
                "base64",
                "h.!!!.s".to_string(),
                "JWT payload base64 decode: ",
            ),
            (
                "json",
                format!("h.{not_json}.s"),
                "JWT payload JSON parse: ",
            ),
        ];
        for (row, jwt, prefix) in rows {
            match decode_jwt_payload::<serde_json::Value>(&jwt, "id_token") {
                Err(OAuthError::TokenEndpoint(m)) => assert!(m.starts_with(prefix), "{row}: {m}"),
                other => panic!("{row}: expected TokenEndpoint, got {other:?}"),
            }
        }
    }

    #[test]
    fn jwt_payload_decode_returns_the_payload_claims() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"seat-a"}"#);
        let claims: serde_json::Value =
            decode_jwt_payload(&format!("h.{payload}.s"), "id_token").expect("decodes");
        assert_eq!(claims["sub"], "seat-a");
    }

    #[test]
    fn safe_token_error_code_accepts_bounded_oauth_codes() {
        assert_eq!(
            safe_token_error_code(r#"{"error":"invalid_grant"}"#).as_deref(),
            Some("invalid_grant")
        );
        assert_eq!(
            safe_token_error_code(r#"{"error":{"code":"invalid_client"}}"#).as_deref(),
            Some("invalid_client")
        );
        assert_eq!(
            safe_token_error_code(r#"{"code":"unsupported_grant_type"}"#).as_deref(),
            Some("unsupported_grant_type")
        );
    }

    #[test]
    fn safe_token_error_code_rejects_credential_shaped_values() {
        // An echoed authorization code / minted token carries uppercase,
        // hyphens, dots, or runs longer than the bound -- none may survive.
        assert_eq!(
            safe_token_error_code(r#"{"error":"ac_MixedCase-AUTHCODE.deadbeef"}"#),
            None
        );
        assert_eq!(
            safe_token_error_code(r#"{"error":"sk-live-TOKEN99"}"#),
            None
        );
        let over_long = "a".repeat(MAX_TOKEN_ERROR_CODE_LEN + 1);
        assert_eq!(
            safe_token_error_code(&format!(r#"{{"error":"{over_long}"}}"#)),
            None
        );
        // Non-string / absent / non-JSON bodies yield no code.
        assert_eq!(safe_token_error_code(r#"{"error":123}"#), None);
        assert_eq!(safe_token_error_code("not json"), None);
        assert_eq!(safe_token_error_code(r#"{"error":""}"#), None);
    }

    #[test]
    fn token_status_error_never_embeds_body_but_keeps_status_and_url() {
        let err = token_status_error(
            reqwest::StatusCode::BAD_REQUEST,
            "https://example.invalid/token",
            r#"{"error":"invalid_grant","secret_echo":"AUTHCODE-LEAK"}"#,
        );
        let msg = err.to_string();
        assert!(msg.contains("400"), "{msg}");
        assert!(msg.contains("https://example.invalid/token"), "{msg}");
        assert!(msg.contains("invalid_grant"), "{msg}");
        assert!(!msg.contains("AUTHCODE-LEAK"), "body leaked: {msg}");
        assert!(!msg.contains("secret_echo"), "body leaked: {msg}");
    }

    #[test]
    fn lookup_and_known_ids_stay_in_sync() {
        // Every id in `known_provider_ids()` must resolve via `lookup`.
        for id in known_provider_ids() {
            assert!(lookup(id).is_ok(), "lookup missing entry for {id}");
        }
    }

    #[test]
    fn callback_host_defaults_to_localhost_and_xai_overrides() {
        // The default host (anthropic / codex) is `localhost`; xAI
        // registered its loopback redirect against the literal IP, so it
        // must override to `127.0.0.1`.
        assert_eq!(lookup("anthropic").unwrap().callback_host(), "localhost");
        assert_eq!(lookup("codex").unwrap().callback_host(), "localhost");
        assert_eq!(lookup("xai").unwrap().callback_host(), "127.0.0.1");
    }

    #[test]
    fn xai_callback_port_candidates_is_single_registered_port_only() {
        // xAI registered its redirect against exactly one fixed port (56121).
        // Port 1457 (codex fallback) is NOT in xAI's allow-list, so the
        // candidate list must never include it.
        let candidates = lookup("xai").unwrap().callback_port_candidates();
        assert_eq!(candidates, vec![56121u16]);
        assert!(
            !candidates.contains(&1457),
            "xai must not fall back to codex port 1457; got {candidates:?}"
        );
    }

    #[test]
    fn codex_callback_port_candidates_includes_fallback() {
        // Codex registers both 1455 (preferred) and 1457 (fallback), so
        // the candidate list must be exactly [1455, 1457] in that order.
        assert_eq!(
            lookup("codex").unwrap().callback_port_candidates(),
            vec![1455u16, 1457u16]
        );
    }
}
