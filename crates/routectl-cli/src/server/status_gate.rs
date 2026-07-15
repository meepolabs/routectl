//! Status-subtree-only middleware: an anti-DNS-rebinding `Host` allowlist and
//! a bounded-concurrency load-shed responder.
//!
//! Everything here scopes to `/status*` ONLY. The ingress `/v1/*` proxy lane
//! carries none of it: the proxy degrades last, never sheds, and does not
//! second-guess the `Host` header (an operator points arbitrary clients at
//! it). The status family is a bounded diagnostic read, so a burst of pollers
//! or a browser-driven DNS-rebinding probe must never compete with -- or leak
//! through -- the forwarding path.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use crate::server::is_loopback;

/// Ceiling on concurrent in-flight `/status*` requests across the WHOLE
/// subtree. Deliberately a hardcoded const, not a config knob: the status
/// surface is a fixed-cost diagnostic read, so a small shared cap keeps a
/// poller burst from ever contending with the ingress proxy for runtime
/// workers. Excess sheds immediately as a 503 (never queues).
pub const STATUS_MAX_INFLIGHT: usize = 4;

/// Wire schema version of the fixed transport-level envelopes this module
/// emits (the overload 503 and the forbidden-host 403). These are NOT panel
/// payloads -- they never reach a `Panel<T>` -- but they carry the same
/// `schema_version` baseline so a consumer reads one dialect across the
/// status surface.
const GATE_SCHEMA_VERSION: u32 = 1;

/// Resolved `Host` allowlist for the status subtree: the loopback literals
/// plus the exact address routectl actually bound. Cheap to clone (an `Arc`),
/// as the axum middleware state machinery clones it per request.
#[derive(Clone)]
pub struct StatusHostAllowlist {
    inner: Arc<AllowlistInner>,
}

struct AllowlistInner {
    /// The bound host literal, e.g. `127.0.0.1` or (under `--unsafe-public`) a
    /// LAN address like `192.168.1.5`.
    bind_host: String,
    /// The bound `host:port`, e.g. `127.0.0.1:8080`.
    bind_host_port: String,
}

impl StatusHostAllowlist {
    /// Build from the address routectl bound. Loopback binds are covered by
    /// the loopback-literal check regardless; the stored bind address is what
    /// lets a deliberate `--unsafe-public` LAN bind stay reachable.
    pub fn new(bound: SocketAddr) -> Self {
        Self {
            inner: Arc::new(AllowlistInner {
                bind_host: bound.ip().to_string(),
                bind_host_port: bound.to_string(),
            }),
        }
    }

    /// A `Host` value is allowed when it is a loopback literal (with or
    /// without a port) or the exact address routectl bound.
    fn allows(&self, host: &str) -> bool {
        if is_loopback_authority(host) {
            return true;
        }
        host == self.inner.bind_host || host == self.inner.bind_host_port
    }
}

/// Strip an optional `:port` (and IPv6 brackets) from an HTTP `Host` authority,
/// yielding the bare host for a loopback check. A bracketed IPv6 literal
/// (`[::1]` / `[::1]:8080`) yields `::1`; an `host:port` yields `host` only
/// when the suffix is a numeric port (so a colon inside a bare hostname is not
/// mistaken for a port separator).
fn split_host_port(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        return match rest.find(']') {
            Some(idx) => &rest[..idx],
            None => authority,
        };
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    }
}

/// Whether an HTTP `Host` authority names a loopback endpoint. Reuses the
/// server's shared loopback predicate (covers the full `127.0.0.0/8` range,
/// `::1`, IPv4-mapped IPv6, and `localhost`) after stripping any port.
fn is_loopback_authority(authority: &str) -> bool {
    is_loopback(split_host_port(authority))
}

/// Reject a request whose `Host` names an endpoint outside the allowlist
/// (anti-DNS-rebinding). A missing `Host` is permitted: the rebinding vector
/// is a browser sending an attacker-controlled hostname, which always carries
/// one. Applied ONLY to the status subtree -- `/v1/*` never sees it.
pub async fn host_guard(
    State(allowlist): State<StatusHostAllowlist>,
    req: Request,
    next: Next,
) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok());
    match host {
        Some(host) if !allowlist.allows(host) => forbidden_host(),
        _ => next.run(req).await,
    }
}

fn forbidden_host() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "schema_version": GATE_SCHEMA_VERSION,
            "error": {
                "code": "forbidden_host",
                "message": "Host header not allowed for the status surface",
            },
        })),
    )
        .into_response()
}

/// `tracing` target for the status-gate shed observability.
const SHED_TARGET: &str = "routectl::status::gate";

/// Log the shed on the 1st event and every Nth thereafter, so a saturated
/// status surface never logs per shed. A fixed compile-time interval, not a
/// config knob -- the shed log is a coarse degradation signal, not a metric.
const SHED_LOG_SAMPLE_N: u64 = 64;

/// Process-wide count of shed `/status*` requests. The load-shed layer sheds
/// BEFORE a request reaches a panel handler, so a shed is invisible to the
/// per-panel `PanelCounters`; this tiny in-process counter is the only place
/// it is observable.
static STATUS_SHED_COUNT: AtomicU64 = AtomicU64::new(0);

/// Whether the shed at 1-based position `count` should be logged: the first
/// shed, then every `SHED_LOG_SAMPLE_N`th.
const fn should_log_shed(count: u64) -> bool {
    count == 1 || count.is_multiple_of(SHED_LOG_SAMPLE_N)
}

/// Map the load-shed overload error to a FIXED JSON 503. The status subtree's
/// inner service is infallible, so the only error the shed layer can surface
/// is `tower::load_shed::error::Overloaded`; every value maps to the same
/// body, carrying no request-specific detail.
///
/// The shed is counted here (the only observable shed site) and logged
/// SAMPLED -- 1st shed + every Nth -- at warn, since a saturated status
/// surface is genuine degradation. Only the running total is logged, never
/// the error or any request detail.
pub async fn handle_status_overload(_err: tower::BoxError) -> Response {
    let shed_total = STATUS_SHED_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if should_log_shed(shed_total) {
        tracing::warn!(
            target: SHED_TARGET,
            shed_total,
            "status surface overloaded; shed a request",
        );
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({
            "schema_version": GATE_SCHEMA_VERSION,
            "error": {
                "code": "overloaded",
                "message": "status service temporarily overloaded; retry shortly",
            },
        })),
    )
        .into_response()
}

/// Wrap a status (sub-)router with the load-shed stack, scoped to that router
/// only. Ordering matters (outermost first): the `HandleErrorLayer` catches
/// the shed error and renders the 503 (making the service infallible for
/// axum); `LoadShedLayer` wraps the concurrency limit so that when the cap is
/// saturated the shed fires IMMEDIATELY on `call` rather than queueing on
/// `poll_ready`; `GlobalConcurrencyLimitLayer` shares ONE semaphore across
/// every route the layer is cloned onto, so the cap is subtree-wide (a plain
/// `ConcurrencyLimitLayer` would mint a fresh per-route semaphore, yielding
/// `routes * cap` -- not the single `STATUS_MAX_INFLIGHT` this const names).
///
/// Shared by production wiring and the shed test so the two cannot drift.
pub fn apply_overload_layers<S>(router: axum::Router<S>) -> axum::Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    router.layer(
        tower::ServiceBuilder::new()
            .layer(axum::error_handling::HandleErrorLayer::new(
                handle_status_overload,
            ))
            .layer(tower::load_shed::LoadShedLayer::new())
            .layer(tower::limit::GlobalConcurrencyLimitLayer::new(
                STATUS_MAX_INFLIGHT,
            )),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request as HttpRequest;
    use axum::routing::get;
    use serde_json::Value;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    use tokio::sync::Semaphore;
    use tower::ServiceExt;

    fn allowlist(bound: &str) -> StatusHostAllowlist {
        StatusHostAllowlist::new(bound.parse().unwrap())
    }

    #[test]
    fn loopback_literals_are_allowed_with_or_without_port() {
        let al = allowlist("127.0.0.1:8787");
        for host in [
            "localhost",
            "localhost:8787",
            "127.0.0.1",
            "127.0.0.1:8787",
            "127.0.0.1:9999",
            "[::1]",
            "[::1]:8787",
        ] {
            assert!(al.allows(host), "loopback host must be allowed: {host}");
        }
    }

    #[test]
    fn bound_address_is_allowed_and_others_rejected() {
        let al = allowlist("192.168.1.5:8080");
        assert!(al.allows("192.168.1.5"));
        assert!(al.allows("192.168.1.5:8080"));
        // A different host -- the DNS-rebinding vector -- is rejected.
        assert!(!al.allows("evil.example.com"));
        assert!(!al.allows("evil.example.com:8080"));
        assert!(!al.allows("10.0.0.9:8080"));
    }

    #[test]
    fn split_host_port_handles_ipv6_and_bare_hosts() {
        assert_eq!(split_host_port("[::1]:8787"), "::1");
        assert_eq!(split_host_port("[::1]"), "::1");
        assert_eq!(split_host_port("127.0.0.1:8787"), "127.0.0.1");
        assert_eq!(split_host_port("localhost"), "localhost");
        assert_eq!(split_host_port("example.com"), "example.com");
    }

    #[derive(Clone)]
    struct HoldState {
        arrived: Arc<AtomicUsize>,
        release: Arc<Semaphore>,
    }

    async fn hold_handler(State(state): State<HoldState>) -> StatusCode {
        state.arrived.fetch_add(1, Ordering::SeqCst);
        // Park in-flight (holding a concurrency permit) until the test
        // releases us. A Semaphore permit is never lost to a wake-race,
        // unlike a `Notify`.
        let _permit = state.release.acquire().await.expect("release semaphore");
        StatusCode::OK
    }

    fn get_request() -> HttpRequest<Body> {
        HttpRequest::builder()
            .method("GET")
            .uri("/hold")
            .body(Body::empty())
            .unwrap()
    }

    /// Saturating the subtree-wide cap sheds the excess request immediately as
    /// the fixed JSON 503, without queueing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn load_shed_returns_fixed_503_when_cap_saturated() {
        let state = HoldState {
            arrived: Arc::new(AtomicUsize::new(0)),
            release: Arc::new(Semaphore::new(0)),
        };
        let router = apply_overload_layers(
            axum::Router::new()
                .route("/hold", get(hold_handler))
                .with_state(state.clone()),
        );

        // Fire exactly STATUS_MAX_INFLIGHT requests that park in-flight, each
        // holding a permit.
        let mut handles = Vec::new();
        for _ in 0..STATUS_MAX_INFLIGHT {
            let router = router.clone();
            handles.push(tokio::spawn(
                async move { router.oneshot(get_request()).await },
            ));
        }

        // Wait until every permit is held (each handler increments `arrived`
        // only AFTER its concurrency permit was acquired in `poll_ready`).
        let deadline = Instant::now() + Duration::from_secs(5);
        while state.arrived.load(Ordering::SeqCst) < STATUS_MAX_INFLIGHT {
            assert!(
                Instant::now() < deadline,
                "handlers never reached in-flight"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // The next request finds no permit -> shed -> fixed 503.
        let resp = router.clone().oneshot(get_request()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["schema_version"], GATE_SCHEMA_VERSION);
        assert_eq!(json["error"]["code"], "overloaded");
        assert!(json["error"]["message"].is_string());

        // Release the parked requests and confirm they complete cleanly (the
        // held permits were real, not a fluke of ordering).
        state.release.add_permits(STATUS_MAX_INFLIGHT);
        for handle in handles {
            let resp = handle.await.unwrap().unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
    }

    /// The status overload layers gate ONLY the status subtree. When the
    /// status cap is saturated (excess sheds a 503), a request routed OUTSIDE
    /// the subtree is untouched by the shared ConcurrencyLimit/LoadShed and
    /// proceeds normally -- the second half of the isolation contract the
    /// proxy lane depends on. Deterministic at the same tower layer the shed
    /// test uses: the status stack is held saturated by parked requests, and
    /// the non-status route carries no permit and no layer, so its 200 never
    /// races the status saturation.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn non_status_route_proceeds_while_status_saturated() {
        let state = HoldState {
            arrived: Arc::new(AtomicUsize::new(0)),
            release: Arc::new(Semaphore::new(0)),
        };
        // The status subtree carries the overload layers; the non-status lane
        // is merged in WITHOUT them, mirroring `/v1/*` inheriting none of the
        // status gate.
        let status_subtree = apply_overload_layers(
            axum::Router::new()
                .route("/hold", get(hold_handler))
                .with_state(state.clone()),
        );
        let app = axum::Router::new()
            .route("/v1/passthrough", get(|| async { StatusCode::OK }))
            .merge(status_subtree);

        // Saturate the status cap with parked requests, each holding a permit.
        let mut handles = Vec::new();
        for _ in 0..STATUS_MAX_INFLIGHT {
            let app = app.clone();
            handles.push(tokio::spawn(
                async move { app.oneshot(get_request()).await },
            ));
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while state.arrived.load(Ordering::SeqCst) < STATUS_MAX_INFLIGHT {
            assert!(
                Instant::now() < deadline,
                "status handlers never reached in-flight"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Confirm the status cap is genuinely saturated: an extra status
        // request sheds the fixed 503.
        let shed = app.clone().oneshot(get_request()).await.unwrap();
        assert_eq!(
            shed.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "status subtree must shed while its cap is saturated"
        );

        // The non-status lane is NOT gated by the status layers: it proceeds.
        let passthrough_req = HttpRequest::builder()
            .method("GET")
            .uri("/v1/passthrough")
            .body(Body::empty())
            .unwrap();
        let passthrough = app.clone().oneshot(passthrough_req).await.unwrap();
        assert_eq!(
            passthrough.status(),
            StatusCode::OK,
            "a non-status route must not be shed by the status cap"
        );

        // Release the parked status requests; they complete cleanly.
        state.release.add_permits(STATUS_MAX_INFLIGHT);
        for handle in handles {
            let resp = handle.await.unwrap().unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
        }
    }

    #[test]
    fn should_log_shed_samples_first_and_every_nth() {
        assert!(should_log_shed(1), "the first shed always logs");
        assert!(!should_log_shed(2));
        assert!(!should_log_shed(SHED_LOG_SAMPLE_N - 1));
        assert!(should_log_shed(SHED_LOG_SAMPLE_N), "every Nth logs");
        assert!(!should_log_shed(SHED_LOG_SAMPLE_N + 1));
        assert!(should_log_shed(2 * SHED_LOG_SAMPLE_N));
    }

    /// The shed path counts every shed but logs SAMPLED, and never logs the
    /// error or any request detail -- only the fixed message + running total.
    /// Driven on the default current-thread runtime so the thread-local
    /// capture sees the log emitted inline by `handle_status_overload`.
    #[tokio::test]
    async fn shed_log_is_sampled_and_never_leaks_error_detail() {
        use routectl_testkit::capture_lines;

        let secret = "/secret/path/token-sk-LEAKED";
        let fired = SHED_LOG_SAMPLE_N * 3;
        let ((), lines) = capture_lines(async {
            for _ in 0..fired {
                let err: tower::BoxError = secret.into();
                let resp = handle_status_overload(err).await;
                assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
            }
        })
        .await;

        // A window of 3N sheds yields a handful of sampled lines, never one
        // per shed -- the whole point of the sampling.
        assert!(!lines.is_empty(), "at least one sampled shed line expected");
        assert!(
            (lines.len() as u64) < fired,
            "shed log must be sampled: {} lines for {fired} sheds",
            lines.len()
        );
        for line in &lines {
            assert!(
                !line.contains("LEAKED") && !line.contains("/secret/"),
                "shed log leaked the raw error: {line}"
            );
            assert!(
                line.contains("status surface overloaded"),
                "unexpected shed line: {line}"
            );
        }
    }
}
