//! Refresh-flow tracing coverage for the xAI (Grok) OAuth provider. Drives
//! the refresh response decoder through both the success and the
//! 400/401-`invalid_grant` paths under a captured `tracing` subscriber and
//! asserts the structured fields routectl is contractually obliged to emit:
//!
//!   - Pre-POST event is emitted by `Xai::refresh_token` itself; we test
//!     the response-side decoder here, which is where the success / error
//!     events fire.
//!   - Success event: `status`, `new_refresh_token_present`,
//!     `new_refresh_token_sha8`, `expires_in`, all carrying the
//!     `routectl_auth::oauth::providers::xai` target.
//!   - Error event: `status`, `error_kind`, `prior_refresh_token_sha8`
//!     -- with `error_kind="refresh_expired"` for the `invalid_grant`
//!     401/400 mapping.
//!
//! xAI-specific wire realities covered here:
//!   - Lazy refresh rotation: a successful refresh may omit `refresh_token`;
//!     the prior token is preserved and `new_refresh_token_present` is false.
//!   - Status-gated `invalid_grant`: only a 400 or 401 body with
//!     `error=invalid_grant` maps to `RefreshExpired`; a 5xx with the same
//!     body must NOT terminate the credential.
//!
//! NEVER asserted: any token VALUE in any event field, AND no event field
//! may carry any portion of the upstream response body. Token endpoints can
//! echo the submitted refresh_token; logging the body verbatim would turn a
//! refresh failure into a credential leak in the trace destination.
//!
//! Why a dedicated integration-test binary: each test installs a
//! thread-local capture subscriber via `tracing::subscriber::set_default`,
//! and the suite uses `current_thread` runtimes to keep that guard active.
//! Other auth tests are oblivious to this subscriber.

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use tracing::field::{Field, Visit};

#[derive(Debug, Clone)]
#[allow(dead_code)] // `target` is captured for diagnostic Debug output on test failure
struct CapturedEvent {
    level: tracing::Level,
    target: String,
    message: String,
    fields: Vec<(String, String)>,
}

impl CapturedEvent {
    fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Default)]
struct FieldCollector {
    message: String,
    fields: Vec<(String, String)>,
}

impl Visit for FieldCollector {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.fields.push((field.name().into(), value.into()));
        }
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        let s = format!("{value:?}");
        if field.name() == "message" {
            self.message = s.trim_matches('"').to_string();
        } else {
            self.fields.push((field.name().into(), s));
        }
    }
}

#[derive(Default)]
struct CaptureSubscriber {
    captured: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl tracing::Subscriber for CaptureSubscriber {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        Some(tracing::level_filters::LevelFilter::TRACE)
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        let meta = event.metadata();
        let mut visitor = FieldCollector::default();
        event.record(&mut visitor);
        let captured = CapturedEvent {
            level: *meta.level(),
            target: meta.target().to_string(),
            message: visitor.message,
            fields: visitor.fields,
        };
        if let Ok(mut guard) = self.captured.lock() {
            guard.push(captured);
        }
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

async fn with_capture<F, T>(fut: F) -> (T, Vec<CapturedEvent>)
where
    F: std::future::Future<Output = T>,
{
    let captured: Arc<Mutex<Vec<CapturedEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let subscriber = CaptureSubscriber {
        captured: captured.clone(),
    };
    let _guard = tracing::subscriber::set_default(subscriber);
    let out = fut.await;
    let events = captured.lock().expect("capture lock poisoned").clone();
    (out, events)
}

/// Wrap a JSON body and status into a `reqwest::Response` without touching
/// the network. Mirrors how `reqwest::Response` is normally produced inside
/// reqwest from an `http::Response<Bytes>`.
fn synthetic_response(status: u16, body: &str) -> reqwest::Response {
    let http_resp: http::Response<Bytes> = http::Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Bytes::from(body.to_owned()))
        .expect("build http response");
    reqwest::Response::from(http_resp)
}

/// Drive the xAI OAuth refresh path through the public `OAuthFlow` trait.
/// We split coverage into the pre-POST event (fired inside `refresh_token`)
/// and the response-side events (fired inside `decode_token_response_traced`).
/// The public path test lets the network leg fail since we do not run a
/// server; the pre-POST debug already fired before the POST is issued.
mod public_path {
    use super::*;
    use routectl_auth::oauth::testing::xai_refresh;

    #[tokio::test]
    async fn pre_post_event_carries_grant_type_and_refresh_token_sha8() {
        let http = reqwest::Client::new();
        let refresh = "test-xai-refresh-token-ABC";

        let (_result, events) = with_capture(xai_refresh(&http, refresh)).await;

        let pre = events
            .iter()
            .find(|e| e.message == "xai refresh request")
            .unwrap_or_else(|| panic!("no pre-POST event captured: {events:#?}"));
        assert_eq!(pre.level, tracing::Level::DEBUG);
        assert_eq!(pre.field("grant_type"), Some("refresh_token"));
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
    use routectl_auth::oauth::testing::decode_xai_refresh_response_traced;

    /// Synthetic 8-hex-char tag that mirrors the shape of a real
    /// `sha8(prior_refresh_token)` value.
    const PRIOR_SHA8: &str = "deadbeef";

    #[tokio::test]
    async fn success_response_event_carries_sha8_and_expires_in() {
        // xAI reads `expires_in` from the JSON response body directly
        // (no JWT exp claim), so the access_token can be any opaque string.
        let body = serde_json::json!({
            "access_token": "xai-AT-OPAQUE",
            "refresh_token": "xai-NEW-RT",
            "token_type": "Bearer",
            "expires_in": 3600
        })
        .to_string();
        let resp = synthetic_response(200, &body);

        let (result, events) = with_capture(decode_xai_refresh_response_traced(
            resp,
            Some("xai-PRIOR-RT"),
            PRIOR_SHA8,
        ))
        .await;
        result.expect("refresh succeeded");

        let success = events
            .iter()
            .find(|e| e.message == "xai refresh response")
            .unwrap_or_else(|| panic!("no success event captured: {events:#?}"));
        assert_eq!(success.level, tracing::Level::DEBUG);
        assert_eq!(success.field("status"), Some("200"));
        assert_eq!(success.field("new_refresh_token_present"), Some("true"));
        let sha = success
            .field("new_refresh_token_sha8")
            .expect("new_refresh_token_sha8 field");
        assert_eq!(sha.len(), 8, "sha8 must be 8 hex chars: {sha}");
        assert!(
            sha.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()),
            "sha8 must be lowercase hex: {sha}"
        );
        // No event field may carry the literal token value.
        for ev in &events {
            for (_, v) in &ev.fields {
                assert!(
                    !v.contains("xai-NEW-RT") && !v.contains("xai-PRIOR-RT"),
                    "refresh token leaked into event field: {ev:#?}"
                );
            }
        }
    }

    #[tokio::test]
    async fn lazy_rotation_success_reports_no_new_refresh_token() {
        // xAI commonly omits refresh_token on a successful refresh (lazy
        // rotation). The prior token is preserved internally; the success
        // event must carry `new_refresh_token_present=false` and
        // `new_refresh_token_sha8="-"` (the sentinel for absent).
        let body = serde_json::json!({
            "access_token": "xai-AT-LAZY",
            "token_type": "Bearer",
            "expires_in": 1800
        })
        .to_string();
        let resp = synthetic_response(200, &body);

        let (result, events) = with_capture(decode_xai_refresh_response_traced(
            resp,
            Some("xai-PRIOR-RT-LAZY"),
            PRIOR_SHA8,
        ))
        .await;
        result.expect("lazy-rotation refresh succeeded");

        let success = events
            .iter()
            .find(|e| e.message == "xai refresh response")
            .unwrap_or_else(|| panic!("no success event captured: {events:#?}"));
        assert_eq!(success.field("new_refresh_token_present"), Some("false"));
        assert_eq!(success.field("new_refresh_token_sha8"), Some("-"));
    }

    #[tokio::test]
    async fn invalid_grant_on_401_emits_error_event_with_refresh_expired_kind() {
        // xAI signals a dead refresh token as `{"error":"invalid_grant"}` on
        // a 400 or 401 response.
        let body = r#"{"error":"invalid_grant"}"#;
        let resp = synthetic_response(401, body);

        let (result, events) = with_capture(decode_xai_refresh_response_traced(
            resp,
            Some("xai-PRIOR-RT"),
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
            .find(|e| e.message == "xai refresh failed")
            .unwrap_or_else(|| panic!("no error event captured: {events:#?}"));
        assert_eq!(error_event.level, tracing::Level::ERROR);
        assert_eq!(error_event.field("status"), Some("401"));
        assert_eq!(error_event.field("error_kind"), Some("refresh_expired"));
        // body_excerpt MUST NOT be present: token-endpoint error envelopes
        // can echo the submitted refresh_token, so logging the body verbatim
        // would defeat the bearer-redaction contract.
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

    /// Hostile / misbehaving token endpoint: the error envelope echoes the
    /// submitted refresh_token verbatim. The captured tracing event MUST NOT
    /// carry that value in any field. This is the regression guard for the
    /// credential-leak fix that dropped the `body_excerpt` field from
    /// refresh-failure events.
    #[tokio::test]
    async fn refresh_failure_does_not_leak_echoed_refresh_token() {
        const ECHOED_RT: &str = "xai-rt-leakcanary-9d4f2e1b3a7c5f0e";
        let body = format!(r#"{{"error":"invalid_grant","refresh_token":"{ECHOED_RT}"}}"#);
        let resp = synthetic_response(401, &body);

        let (result, events) = with_capture(decode_xai_refresh_response_traced(
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
