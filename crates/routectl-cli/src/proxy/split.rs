//! Request classification and split-leg dispatch for the MITM
//! front-proxy: decides, per decrypted HTTP/1.1 request, whether it is
//! an Anthropic-dialect inference call (re-inject over loopback to
//! routectl's own listener) or anything else (forward verbatim to the
//! real Anthropic origin).
//!
//! This module does not terminate TLS or serve HTTP/1.1 connections --
//! see `proxy::mitm` for that. [`handle_request`] is generic over the
//! request body type (`B: http_body::Body`, not `hyper::body::Incoming`
//! specifically) so it can be exercised directly in tests with a
//! synthetic body (e.g. `http_body_util::Full`) without spinning up a
//! real TLS/HTTP/1.1 connection; `proxy::mitm` instantiates it with
//! `hyper::body::Incoming` in production.

use bytes::Bytes;
use http::{HeaderName, HeaderValue, Request, Response, StatusCode};
use http_body_util::BodyExt;

use super::forward::{ForwardBody, ForwardRequest, empty_response, forward};
use super::metrics::{Leg, PathClass};
use super::mitm::MitmCtx;

/// The single source of truth for which request paths this MITM proxy
/// classifies as Anthropic-dialect inference traffic. Every path here
/// gets re-injected over loopback into routectl's own listener (which
/// carries the credential-swap seam for a later feature) instead of
/// forwarded to the real Anthropic origin.
///
/// Deliberately excludes `/v1/chat/completions` and `/v1/responses`:
/// those are direct-client ingress dialects, never reached through this
/// MITM channel (Claude Code never sends them to `api.anthropic.com`).
///
/// This is a `const` + a CI test ([`anthropic_inference_paths`] used
/// from `tests/server.rs`), not a table-driven route registration --
/// the three inference routes have distinct handlers/methods in
/// `build_axum_router`, so forcing them through one iterate-and-register
/// mechanism would be a contorted router refactor for no behavioral
/// gain. See the pointer comment at `build_axum_router` in
/// `server/mod.rs`.
pub(crate) const ANTHROPIC_INFERENCE_PATHS: &[&str] =
    &["/v1/messages", "/v1/messages/count_tokens", "/v1/models"];

/// Read-only accessor for [`ANTHROPIC_INFERENCE_PATHS`], for the
/// cross-crate anti-drift integration test in `tests/server.rs` (a
/// separate compilation unit that cannot see a `pub(crate)` item
/// directly). The const itself stays `pub(crate)` -- this accessor
/// widens nothing about who can influence classification, only who can
/// read the frozen list back.
pub const fn anthropic_inference_paths() -> &'static [&'static str] {
    ANTHROPIC_INFERENCE_PATHS
}

/// Known non-inference Anthropic-host path prefixes that are expected,
/// ordinary control-plane traffic -- routed to the real Anthropic
/// origin like any other non-inference path, but never worth a
/// first-occurrence warning. Anything NOT matching the inference const
/// or one of these prefixes still forwards to Anthropic (never dropped,
/// never blocked) but is logged once via `warn_once` and counted via
/// `incr_unknown_forwarded_paths`, so an operator notices new upstream
/// surface without any request ever failing because of it.
///
/// Claude Code's telemetry/error-reporting traffic to
/// `api.anthropic.com` goes out on `/api/event_logging`, which the
/// `/api/` prefix above already covers -- confirmed against a captured
/// request corpus from the operator research this list originated from.
/// Telemetry therefore classifies as [`PathClass::ControlPlane`], not
/// `Unknown`; only a path matching neither the inference const nor one
/// of these prefixes ever reaches the warn-once/`Unknown` path below.
///
/// Every entry carries a trailing slash so `starts_with` cannot match a
/// sibling path that merely shares the prefix string (e.g. a bare
/// `/v1/mcp_servers` here would also match `/v1/mcp_servers_evil`). A
/// request for the bare route with no trailing slash and no subpath
/// (`/v1/mcp_servers` exactly) is handled separately by
/// [`KNOWN_CONTROL_PLANE_EXACT_PATHS`].
const KNOWN_CONTROL_PLANE_PREFIXES: &[&str] = &["/v1/code/", "/api/", "/v1/mcp_servers/"];

/// Exact-match control-plane paths that carry no trailing slash and no
/// subpath of their own -- kept out of
/// [`KNOWN_CONTROL_PLANE_PREFIXES`] specifically so a `starts_with`
/// prefix check on a bare `/v1/mcp_servers` (no trailing slash) could
/// never also match an unrelated `/v1/mcp_servers_evil`.
const KNOWN_CONTROL_PLANE_EXACT_PATHS: &[&str] = &["/v1/mcp_servers"];

/// Set on the re-injected request only (never on the catch-all forward
/// to Anthropic): the seam marking "this request arrived via the MITM
/// re-inject path." The forwarded-mode ingress capture gate reads it as a
/// hint. Sourced from the single shared string in `crate::ingress` so the
/// set site here and the read site in `handlers::ingress_handle` cannot
/// drift on the literal.
const MITM_PROXIED_HEADER_NAME: HeaderName =
    HeaderName::from_static(crate::ingress::MITM_PROXIED_HEADER);

fn is_anthropic_inference_path(path: &str) -> bool {
    ANTHROPIC_INFERENCE_PATHS.contains(&path)
}

fn is_known_control_plane_path(path: &str) -> bool {
    KNOWN_CONTROL_PLANE_EXACT_PATHS.contains(&path)
        || KNOWN_CONTROL_PLANE_PREFIXES
            .iter()
            .any(|prefix| path.starts_with(prefix))
}

/// Converts a decrypted request body into the streaming `reqwest::Body`
/// [`forward`] expects, without buffering it -- works for any
/// `http_body::Body` impl (production: `hyper::body::Incoming`; tests:
/// e.g. `http_body_util::Full`).
fn into_reqwest_body<B>(body: B) -> reqwest::Body
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    reqwest::Body::wrap_stream(BodyExt::into_data_stream(body))
}

/// Classifies and dispatches one decrypted MITM request: re-inject over
/// loopback for a const inference path, forward verbatim to the fixed
/// Anthropic origin for everything else. Never strips or substitutes
/// the client's `Authorization` header on either leg -- both legs
/// re-inject/forward the original request headers byte-for-byte aside
/// from the one `x-routectl-mitm-proxied` addition on the inference
/// leg.
pub(crate) async fn handle_request<B>(ctx: &MitmCtx, req: Request<B>) -> Response<ForwardBody>
where
    B: http_body::Body<Data = Bytes> + Send + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    let (parts, body) = req.into_parts();
    let method = parts.method;
    let raw_path_and_query = parts
        .uri
        .path_and_query()
        .map_or_else(|| "/".to_string(), |pq| pq.as_str().to_string());
    let path = parts.uri.path().to_string();
    let mut headers = parts.headers;
    let reqwest_body = into_reqwest_body(body);

    // Runs on every request regardless of leg -- Claude Code's
    // `User-Agent` appears on both its inference and control-plane
    // calls alike, so the tested-vs-observed check does not depend on
    // the classification below. Never gates the request either way;
    // see `proxy::cc_version` for the warn-and-proceed contract.
    let observed_cc_version = super::cc_version::observed_cc_version(&headers);
    ctx.cc_version_warn_guard.check(
        ctx.tested_cc_version.as_deref(),
        observed_cc_version.as_deref(),
    );

    if is_anthropic_inference_path(&path) {
        headers.insert(MITM_PROXIED_HEADER_NAME, HeaderValue::from_static("1"));
        let forward_req = ForwardRequest {
            method: method.clone(),
            raw_path_and_query,
            headers,
            body: reqwest_body,
        };
        let response = forward(
            &ctx.forward_state,
            &ctx.metrics,
            &ctx.reinject_base,
            forward_req,
            Leg::Inference,
            PathClass::Inference,
        )
        .await;

        // A const inference path is a promise that routectl's own
        // listener serves that route (see the anti-drift CI test). A
        // 404 here means that promise broke -- config/route drift, not
        // a legitimate "not found" -- so this must never silently fall
        // through to a bare Anthropic forward, which would bypass
        // whatever credential-swap seam the re-inject leg exists for.
        if response.status() == StatusCode::NOT_FOUND {
            tracing::error!(
                target: "routectl_cli::proxy::split",
                %method,
                path = %path,
                "MITM const inference path returned 404 from the loopback re-inject \
                 target; ANTHROPIC_INFERENCE_PATHS has drifted from the routes routectl \
                 actually serves -- refusing to forward this request to Anthropic"
            );
            return empty_response(StatusCode::INTERNAL_SERVER_ERROR);
        }
        return response;
    }

    let path_class = if is_known_control_plane_path(&path) {
        PathClass::ControlPlane
    } else {
        ctx.warn_once.warn_once(method.as_str(), &path);
        ctx.metrics.incr_unknown_forwarded_paths();
        PathClass::Unknown
    };

    let forward_req = ForwardRequest {
        method,
        raw_path_and_query,
        headers,
        body: reqwest_body,
    };
    forward(
        &ctx.forward_state,
        &ctx.metrics,
        &ctx.upstream_origin,
        forward_req,
        Leg::ControlPlane,
        path_class,
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use http_body_util::Full;
    use reqwest::Url;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::proxy::forward::{ForwardState, build_client};
    use crate::proxy::metrics::{ProxyMetrics, WarnOnce};

    #[test]
    fn anthropic_inference_paths_excludes_direct_client_dialects() {
        assert!(!ANTHROPIC_INFERENCE_PATHS.contains(&"/v1/chat/completions"));
        assert!(!ANTHROPIC_INFERENCE_PATHS.contains(&"/v1/responses"));
    }

    #[test]
    fn is_known_control_plane_path_matches_documented_prefixes() {
        assert!(is_known_control_plane_path("/v1/code/foo"));
        assert!(is_known_control_plane_path("/api/hello"));
        assert!(is_known_control_plane_path("/v1/mcp_servers"));
        assert!(is_known_control_plane_path("/v1/mcp_servers/foo"));
        assert!(!is_known_control_plane_path("/v1/something_else"));
    }

    /// The trailing-slash convention on [`KNOWN_CONTROL_PLANE_PREFIXES`]
    /// must not let a `starts_with` prefix match swallow an unrelated
    /// sibling path that merely shares the `/v1/mcp_servers` string.
    #[test]
    fn is_known_control_plane_path_rejects_a_sibling_path_sharing_the_prefix_string() {
        assert!(!is_known_control_plane_path("/v1/mcp_servers_evil"));
    }

    fn body(bytes: &'static [u8]) -> Full<Bytes> {
        Full::new(Bytes::from_static(bytes))
    }

    fn test_ctx(reinject_base: Url, upstream_origin: Url) -> MitmCtx {
        MitmCtx {
            forward_state: ForwardState::new(build_client().unwrap(), 8, Duration::from_secs(30)),
            metrics: Arc::new(ProxyMetrics::new()),
            warn_once: Arc::new(WarnOnce::new()),
            upstream_origin,
            reinject_base,
            tested_cc_version: None,
            cc_version_warn_guard: crate::proxy::cc_version::CcVersionWarnGuard::new(),
        }
    }

    #[tokio::test]
    async fn const_inference_path_reinjects_over_loopback_and_sets_marker_header() {
        let reinject_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(b"reinjected".to_vec(), "text/plain"),
            )
            .mount(&reinject_server)
            .await;
        // The real Anthropic origin must never see this request.
        let upstream_server = MockServer::start().await;

        let ctx = test_ctx(
            Url::parse(&reinject_server.uri()).unwrap(),
            Url::parse(&upstream_server.uri()).unwrap(),
        );

        let req = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("authorization", "Bearer client-token-abc")
            .body(body(b"{}"))
            .unwrap();

        let response = handle_request(&ctx, req).await;
        assert_eq!(response.status(), StatusCode::OK);

        let last_request = reinject_server
            .received_requests()
            .await
            .expect("mock records requests")
            .into_iter()
            .next()
            .expect("one request recorded");
        assert_eq!(
            last_request.headers.get("x-routectl-mitm-proxied").unwrap(),
            "1"
        );
        assert_eq!(
            last_request.headers.get("authorization").unwrap(),
            "Bearer client-token-abc",
            "the original client Authorization must reach the loopback re-inject leg untouched"
        );

        assert!(
            upstream_server
                .received_requests()
                .await
                .unwrap()
                .is_empty(),
            "a const inference path must never reach the real Anthropic origin"
        );
    }

    #[tokio::test]
    async fn const_inference_path_404_from_loopback_becomes_hard_5xx_not_a_forward() {
        let reinject_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&reinject_server)
            .await;
        let upstream_server = MockServer::start().await;

        let ctx = test_ctx(
            Url::parse(&reinject_server.uri()).unwrap(),
            Url::parse(&upstream_server.uri()).unwrap(),
        );

        let req = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .body(body(b"{}"))
            .unwrap();

        let response = handle_request(&ctx, req).await;
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(
            upstream_server
                .received_requests()
                .await
                .unwrap()
                .is_empty(),
            "a route-drift 404 must never fall through to Anthropic"
        );
    }

    #[tokio::test]
    async fn known_control_plane_path_forwards_to_anthropic_without_warning() {
        let reinject_server = MockServer::start().await;
        let upstream_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/hello"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&upstream_server)
            .await;

        let ctx = test_ctx(
            Url::parse(&reinject_server.uri()).unwrap(),
            Url::parse(&upstream_server.uri()).unwrap(),
        );

        let req = Request::builder()
            .method("GET")
            .uri("/api/hello")
            .header("authorization", "Bearer client-token-abc")
            .body(body(b""))
            .unwrap();

        let response = handle_request(&ctx, req).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(ctx.metrics.unknown_forwarded_paths_total(), 0);

        let last_request = upstream_server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert_eq!(
            last_request.headers.get("authorization").unwrap(),
            "Bearer client-token-abc",
            "the client Authorization must reach the real Anthropic origin untouched"
        );
        assert!(
            reinject_server
                .received_requests()
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn unknown_path_forwards_to_anthropic_and_warns_plus_counts_once_per_pair() {
        let reinject_server = MockServer::start().await;
        let upstream_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/something-new"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&upstream_server)
            .await;

        let ctx = test_ctx(
            Url::parse(&reinject_server.uri()).unwrap(),
            Url::parse(&upstream_server.uri()).unwrap(),
        );

        let first = Request::builder()
            .method("GET")
            .uri("/v1/something-new")
            .body(body(b""))
            .unwrap();
        let response = handle_request(&ctx, first).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(ctx.metrics.unknown_forwarded_paths_total(), 1);

        let second = Request::builder()
            .method("GET")
            .uri("/v1/something-new")
            .body(body(b""))
            .unwrap();
        let response = handle_request(&ctx, second).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            ctx.metrics.unknown_forwarded_paths_total(),
            2,
            "the metric bumps on every unknown-path request, independent of warn_once dedup"
        );
    }

    #[tokio::test]
    async fn inference_marker_header_is_never_set_on_the_catch_all_leg() {
        let reinject_server = MockServer::start().await;
        let upstream_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/hello"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&upstream_server)
            .await;

        let ctx = test_ctx(
            Url::parse(&reinject_server.uri()).unwrap(),
            Url::parse(&upstream_server.uri()).unwrap(),
        );

        let req = Request::builder()
            .method("GET")
            .uri("/api/hello")
            .body(body(b""))
            .unwrap();
        let _ = handle_request(&ctx, req).await;

        let last_request = upstream_server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .next()
            .unwrap();
        assert!(
            last_request
                .headers
                .get("x-routectl-mitm-proxied")
                .is_none()
        );
    }

    #[tokio::test]
    async fn handle_request_wires_the_cc_version_check_from_the_user_agent_header() {
        let reinject_server = MockServer::start().await;
        let upstream_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/hello"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&upstream_server)
            .await;

        let mut ctx = test_ctx(
            Url::parse(&reinject_server.uri()).unwrap(),
            Url::parse(&upstream_server.uri()).unwrap(),
        );
        ctx.tested_cc_version = Some("2.1.169".to_string());

        let req = Request::builder()
            .method("GET")
            .uri("/api/hello")
            .header("user-agent", "claude-cli/2.0.0 (external, cli)")
            .body(body(b""))
            .unwrap();
        let response = handle_request(&ctx, req).await;
        assert_eq!(response.status(), StatusCode::OK);

        assert!(
            !ctx.cc_version_warn_guard
                .check(Some("2.1.169"), Some("2.0.0")),
            "handle_request must already have warned for this mismatch through the same guard"
        );
    }

    #[tokio::test]
    async fn handle_request_never_warns_when_the_user_agent_version_matches_tested() {
        let reinject_server = MockServer::start().await;
        let upstream_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/hello"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&upstream_server)
            .await;

        let mut ctx = test_ctx(
            Url::parse(&reinject_server.uri()).unwrap(),
            Url::parse(&upstream_server.uri()).unwrap(),
        );
        ctx.tested_cc_version = Some("2.1.169".to_string());

        let req = Request::builder()
            .method("GET")
            .uri("/api/hello")
            .header("user-agent", "claude-cli/2.1.169 (external, cli)")
            .body(body(b""))
            .unwrap();
        let _ = handle_request(&ctx, req).await;

        assert!(
            ctx.cc_version_warn_guard
                .check(Some("2.1.169"), Some("2.0.0")),
            "a matching request must not have consumed the guard's dedup slot -- \
             the first genuine mismatch afterward must still warn"
        );
    }

    /// Exercises both tracing call sites in this module that fire on a
    /// request carrying a client `Authorization` header -- the anti-drift
    /// 404 path (`tracing::error!`) and the unknown-path warn
    /// (`warn_once`'s `tracing::warn!`) -- and asserts the token never
    /// appears in any captured log line. `#[tokio::test]` defaults to a
    /// `current_thread` runtime, which is required here: `capture_lines`
    /// installs a thread-local subscriber.
    #[tokio::test]
    async fn authorization_token_never_appears_in_any_log_line() {
        const SECRET_TOKEN: &str = "sk-super-secret-tok3n-99";

        let reinject_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&reinject_server)
            .await;
        let upstream_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/unknown-thing"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&upstream_server)
            .await;

        let ctx = test_ctx(
            Url::parse(&reinject_server.uri()).unwrap(),
            Url::parse(&upstream_server.uri()).unwrap(),
        );

        let ((), captured) = routectl_testkit::capture_lines(async {
            let drift_req = Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .header("authorization", format!("Bearer {SECRET_TOKEN}"))
                .body(body(b"{}"))
                .unwrap();
            let _ = handle_request(&ctx, drift_req).await;

            let unknown_req = Request::builder()
                .method("GET")
                .uri("/v1/unknown-thing")
                .header("authorization", format!("Bearer {SECRET_TOKEN}"))
                .body(body(b""))
                .unwrap();
            let _ = handle_request(&ctx, unknown_req).await;
        })
        .await;

        assert!(
            !captured.is_empty(),
            "expected at least one log line to be captured"
        );
        for line in &captured {
            assert!(
                !line.contains(SECRET_TOKEN),
                "log line leaked the token: {line}"
            );
        }
    }
}
