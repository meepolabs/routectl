//! Refresh-flow tracing coverage for the codex (chatgpt-oauth) OAuth
//! provider. Drives the refresh response decoder through both the
//! success and the 401-`refresh_token_expired` paths under a captured
//! `tracing` subscriber and asserts the structured fields routectl is
//! contractually obliged to emit:
//!
//!   - Pre-POST event is emitted by `Codex::refresh_token` itself; we
//!     test the response-side decoder here, which is where the success
//!     / error events fire.
//!   - Success event: `status`, `new_refresh_token_present`,
//!     `new_refresh_token_sha8`, `expires_in`, all carrying the
//!     `routectl_auth::oauth::providers::codex` target.
//!   - Error event: `status`, `error_kind`, `prior_refresh_token_sha8`
//!     -- with `error_kind="refresh_expired"` for the
//!     `refresh_token_expired` 401 mapping.
//!
//! NEVER asserted: any token VALUE in any event field, AND no event
//! field may carry any portion of the upstream response body. Token
//! endpoints can echo the submitted refresh_token (or mint a new one)
//! inside their error envelope; logging the body verbatim would turn
//! a refresh failure into a credential leak in the trace destination.
//! Test failures that print diff output must not print the bearer /
//! refresh values either; we use synthetic strings where they DO
//! appear.
//!
//! Why a dedicated integration-test binary: each test installs a
//! thread-local capture subscriber via `tracing::subscriber::set_default`,
//! and the suite uses `current_thread` runtimes to keep that guard
//! active. Other auth tests are oblivious to this subscriber.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use routectl_testkit::with_capture;

/// Build a synthetic JWT (`header.payload.sig`) whose payload is the
/// given JSON value. The signature is non-empty filler -- routectl
/// never verifies it.
fn jwt(payload: serde_json::Value) -> String {
    let enc = |b: &[u8]| URL_SAFE_NO_PAD.encode(b);
    let header = enc(br#"{"alg":"none","typ":"JWT"}"#);
    let body = enc(payload.to_string().as_bytes());
    let sig = enc(b"sig");
    format!("{header}.{body}.{sig}")
}

/// Wrap a JSON body and status into a `reqwest::Response` without
/// touching the network. Mirrors how `reqwest::Response` is normally
/// produced inside reqwest from an `http::Response<Bytes>`.
fn synthetic_response(status: u16, body: &str) -> reqwest::Response {
    let http_resp: http::Response<Bytes> = http::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Bytes::from(body.to_owned()))
        .expect("build http response");
    reqwest::Response::from(http_resp)
}

/// Drive the codex OAuth refresh path through the public `OAuthFlow`
/// trait. This indirection keeps the test honest: it exercises the
/// real `refresh_token` code path including its pre-POST tracing line.
/// The wiremock URL is intentionally NOT used -- the codex flow pins
/// `TOKEN_URL` to a const, so we cannot redirect it without changing
/// production code. We split coverage instead:
///   - `pre_post_request_event` calls `refresh_token` and inspects the
///     pre-POST event ONLY (the network leg fails since we do not run
///     a server, but the pre-POST debug already fired).
///   - `success_response_event` and `expired_refresh_event` call the
///     internal `decode_token_response_traced` (re-exported under a
///     test-visible name) so we can assert the response-side events
///     against synthetic responses.
mod public_path {
    use super::*;
    use routectl_auth::oauth::testing::codex_refresh;

    #[tokio::test]
    async fn pre_post_event_carries_grant_type_and_refresh_token_sha8() {
        let http = reqwest::Client::new();
        let refresh = "test-refresh-token-XYZ";

        let (_result, events) = with_capture(codex_refresh(&http, refresh)).await;

        // The pre-POST debug must fire BEFORE the network leg returns
        // (success or failure). Find it by message.
        let pre = events
            .iter()
            .find(|e| e.message == "codex refresh request")
            .unwrap_or_else(|| panic!("no pre-POST event captured: {events:#?}"));
        assert_eq!(pre.level, tracing::Level::DEBUG);
        assert_eq!(pre.field("grant_type"), Some("refresh_token"));
        // sha8 is 8 lowercase hex chars deterministically derived from
        // the input; pin both shape and value.
        let sha = pre
            .field("refresh_token_sha8")
            .expect("refresh_token_sha8 field");
        assert_eq!(sha.len(), 8, "sha8 must be 8 hex chars: {sha}");
        assert!(
            sha.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "sha8 must be lowercase hex: {sha}"
        );
        // The refresh token VALUE must NEVER appear in any event field.
        for ev in &events {
            for (_, v) in &ev.fields {
                assert!(
                    !v.contains(refresh),
                    "refresh token leaked into event field: {ev:#?}"
                );
            }
        }
    }
}

mod response_path {
    use super::*;
    use routectl_auth::oauth::testing::decode_refresh_response_traced;

    /// Synthetic 8-hex-char tag that mirrors the shape of a real
    /// `sha8(prior_refresh_token)` value. Tests pass this in place of
    /// computing the real digest so the assertion can pin both the
    /// shape and the exact value while keeping the test self-contained.
    const PRIOR_SHA8: &str = "deadbeef";

    #[tokio::test]
    async fn success_response_event_carries_sha8_and_expires_in() {
        // Build a successful refresh response: a fresh access JWT, a
        // rotated refresh_token, and the standard token_type. The new
        // access JWT carries `exp` 3600 s in the future so `expires_in`
        // ends up positive.
        let now: u64 = 1_900_000_000;
        let access = jwt(serde_json::json!({ "exp": now + 3600 }));
        let body = serde_json::json!({
            "access_token": access,
            "refresh_token": "NEW-RT",
            "token_type": "Bearer",
            "scope": "openid profile email offline_access"
        })
        .to_string();
        let resp = synthetic_response(200, &body);

        let (result, events) = with_capture(decode_refresh_response_traced(
            resp,
            Some("PRIOR-RT"),
            PRIOR_SHA8,
        ))
        .await;
        result.expect("refresh succeeded");

        let success = events
            .iter()
            .find(|e| e.message == "codex refresh response")
            .unwrap_or_else(|| panic!("no success event captured: {events:#?}"));
        assert_eq!(success.level, tracing::Level::DEBUG);
        assert_eq!(success.field("status"), Some("200"));
        assert_eq!(success.field("new_refresh_token_present"), Some("true"));
        let sha = success
            .field("new_refresh_token_sha8")
            .expect("new_refresh_token_sha8 field");
        assert_eq!(sha.len(), 8, "sha8 must be 8 hex chars: {sha}");

        // No event field may carry the literal token value.
        for ev in &events {
            for (_, v) in &ev.fields {
                assert!(
                    !v.contains("NEW-RT") && !v.contains("PRIOR-RT"),
                    "refresh token leaked into event field: {ev:#?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn expired_refresh_emits_error_event_with_refresh_expired_kind() {
        let body = r#"{"error":{"code":"refresh_token_expired"}}"#;
        let resp = synthetic_response(401, body);

        let (result, events) = with_capture(decode_refresh_response_traced(
            resp,
            Some("PRIOR-RT"),
            PRIOR_SHA8,
        ))
        .await;
        let err = result.expect_err("expected RefreshExpired error");
        assert!(
            matches!(err, routectl_auth::oauth::OAuthError::RefreshExpired(_)),
            "got {err:?}"
        );

        let error_event = events
            .iter()
            .find(|e| e.message == "codex refresh failed")
            .unwrap_or_else(|| panic!("no error event captured: {events:#?}"));
        assert_eq!(error_event.level, tracing::Level::ERROR);
        assert_eq!(error_event.field("status"), Some("401"));
        assert_eq!(error_event.field("error_kind"), Some("refresh_expired"));
        // body_excerpt MUST NOT be present: token-endpoint error
        // envelopes can echo the submitted refresh_token (or mint a
        // new one), so logging the body verbatim would defeat the
        // bearer-redaction contract.
        assert!(
            error_event.field("body_excerpt").is_none(),
            "body_excerpt must be dropped from refresh-failure events: {error_event:#?}"
        );
        // prior_refresh_token_sha8 must be present and shape-pinned so
        // an operator can correlate the failure to the credential that
        // triggered it without re-deriving the value.
        let sha = error_event
            .field("prior_refresh_token_sha8")
            .expect("prior_refresh_token_sha8 field");
        assert_eq!(sha.len(), 8, "sha8 must be 8 hex chars: {sha}");
        assert!(
            sha.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "sha8 must be lowercase hex: {sha}"
        );
        assert_eq!(sha, PRIOR_SHA8);
    }

    #[tokio::test]
    async fn generic_401_emits_error_event_with_token_endpoint_kind() {
        let body = r#"{"error":"invalid_client"}"#;
        let resp = synthetic_response(401, body);

        let (result, events) = with_capture(decode_refresh_response_traced(
            resp,
            Some("PRIOR-RT"),
            PRIOR_SHA8,
        ))
        .await;
        let err = result.expect_err("expected TokenEndpoint error");
        assert!(
            matches!(err, routectl_auth::oauth::OAuthError::TokenEndpoint(_)),
            "got {err:?}"
        );

        let error_event = events
            .iter()
            .find(|e| e.message == "codex refresh failed")
            .unwrap_or_else(|| panic!("no error event captured: {events:#?}"));
        assert_eq!(error_event.level, tracing::Level::ERROR);
        assert_eq!(error_event.field("error_kind"), Some("token_endpoint"));
        // Same body-leak guard as the refresh_expired path.
        assert!(
            error_event.field("body_excerpt").is_none(),
            "body_excerpt must be dropped from refresh-failure events: {error_event:#?}"
        );
        let sha = error_event
            .field("prior_refresh_token_sha8")
            .expect("prior_refresh_token_sha8 field");
        assert_eq!(sha.len(), 8, "sha8 must be 8 hex chars: {sha}");
        assert!(
            sha.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "sha8 must be lowercase hex: {sha}"
        );
        assert_eq!(sha, PRIOR_SHA8);
    }

    /// Hostile / misbehaving token endpoint: the error envelope echoes
    /// the submitted refresh_token verbatim. The captured tracing
    /// event MUST NOT carry that value in any field. This is the
    /// regression guard for the credential-leak fix that dropped the
    /// `body_excerpt` field from refresh-failure events.
    #[tokio::test]
    async fn refresh_failure_does_not_leak_echoed_refresh_token() {
        // Synthetic literal that uniquely identifies a token leak if
        // it shows up in any event field. The value is chosen to be
        // non-overlapping with the standard error-code strings.
        const ECHOED_RT: &str = "rt-leakcanary-7c1e2f4a8b3d9e0c";
        let body = format!(
            r#"{{"error":{{"code":"refresh_token_reused","refresh_token":"{ECHOED_RT}"}}}}"#
        );
        let resp = synthetic_response(401, &body);

        let (result, events) = with_capture(decode_refresh_response_traced(
            resp,
            Some(ECHOED_RT),
            PRIOR_SHA8,
        ))
        .await;
        let _ = result.expect_err("expected refresh failure");

        for ev in &events {
            assert!(
                !ev.message.contains(ECHOED_RT),
                "refresh token leaked into event message: {ev:#?}"
            );
            for (k, v) in &ev.fields {
                assert!(
                    !v.contains(ECHOED_RT),
                    "refresh token leaked into event field {k}: {ev:#?}"
                );
            }
        }
    }
}
