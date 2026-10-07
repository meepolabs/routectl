//! Forward-leg transport primitive for the MITM front-proxy.
//!
//! This is the dumb byte forwarder BOTH split legs of the feature
//! reuse: the loopback re-inject back to the local listener (:9100)
//! and the catch-all forward to the real upstream. Each leg has its
//! own client (see [`ForwardState`]) so the credential-bearing re-inject
//! never follows an environment proxy. Deliberately
//! classification-agnostic -- the caller (a later task) decides which
//! `Leg`/`PathClass` a request is, this module just forwards bytes and
//! records what it is told. Does NOT build the CONNECT listener,
//! terminate TLS, or split/classify traffic -- see the later listener
//! task in this feature.
//!
//! The returned response body (`ForwardBody`) is an
//! `http_body_util::combinators::UnsyncBoxBody`, not a hyper type --
//! `http`, `http-body`, and `http-body-util` are the shared vocabulary
//! reqwest and hyper 1.x both speak already (reqwest re-exports
//! `http::{Method, StatusCode}` and `http::header` directly), so a
//! later task can hand this `http::Response<ForwardBody>` straight to
//! a hyper `Service` with no adaptation. `UnsyncBoxBody` (not
//! `BoxBody`) because nothing here needs the body shared across
//! threads, only moved once into the response future.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::Stream;
use http::{HeaderMap, Method, Response, StatusCode};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, StreamBody};
use reqwest::{Client, Url};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::metrics::{Leg, PathClass, ProxyMetrics, ResultClass};

mod watchdog;
use watchdog::watchdog_stream;

/// Connect (TCP + TLS handshake) timeout for both forward clients.
/// Caps only the initial connection, never a per-read gap: the
/// clients carry long-polls and SSE, so staleness is bounded by the
/// upload/header deadline ([`RESPONSE_HEAD_TIMEOUT`]) and the per-forward
/// body idle watchdog ([`STREAM_IDLE_WINDOW`]) instead.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Deadline for uploading a request and obtaining response headers. This is
/// NOT a total response deadline: after headers, healthy SSE can run forever
/// provided each read gap stays below the independent idle window.
pub const RESPONSE_HEAD_TIMEOUT: Duration = Duration::from_mins(10);

/// Idle silence window before the stream watchdog gives up on a
/// forwarded stream and aborts it. Deliberately its own, longer,
/// constant rather than reusing
/// `routectl_providers::http_client::STREAM_READ_TIMEOUT` (300s): that
/// constant caps a single provider's inference read gap, but this
/// proxy also carries the control-plane's long-polls, which can sit
/// idle for longer than any inference SSE gap without being dead. This
/// is purely a leak safety net for a truly wedged forwarding leg, not a
/// tuning knob for latency -- a healthy stream, inference or
/// control-plane, never approaches it.
pub const STREAM_IDLE_WINDOW: Duration = Duration::from_mins(10);

/// Default cap on concurrently forwarded streams (both split legs
/// share one bound). Conservative and overridable per-deployment via
/// [`ForwardState::new`]; sized to bound worst-case memory/FD use
/// under a wedged-upstream pile-up, not to reflect real expected load.
pub const DEFAULT_MAX_CONCURRENT_STREAMS: usize = 256;

/// Header names stripped from BOTH the outbound (to-upstream) and
/// inbound (from-upstream) legs per RFC 9110 7.6.1, plus every header
/// name the `Connection` header itself lists as hop-by-hop.
const HOP_BY_HOP_HEADER_NAMES: &[&str] = &[
    "connection",
    "keep-alive",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "proxy-authorization",
    "proxy-authenticate",
];

/// Header names that are not hop-by-hop by RFC definition but are
/// always recomputed by whichever stack sends the message (reqwest on
/// the outbound leg, the eventual hyper server on the inbound leg) --
/// copying a stale value through would desync framing or routing.
/// `transfer-encoding` is already covered by
/// [`HOP_BY_HOP_HEADER_NAMES`].
const RECOMPUTED_HEADER_NAMES: &[&str] = &["host", "content-length"];

/// The settings both forward clients share, so the two cannot drift
/// on anything but proxy policy.
///
/// Connect-only timeout (10s): a hung TCP/TLS handshake to a
/// black-holed upstream must fail fast. Deliberately NO
/// `read_timeout`/`timeout`: these clients carry control-plane
/// long-polls and inference SSE alike, both of which can legitimately
/// sit quiet for a while between bytes. `routectl-providers`'
/// `STREAM_READ_TIMEOUT` (300s) is exactly the inherited default we
/// must NOT pick up here -- staleness protection instead comes from
/// the upload/header deadline ([`RESPONSE_HEAD_TIMEOUT`]) and per-forward
/// body idle watchdog ([`STREAM_IDLE_WINDOW`]), both injectable on the state
/// and not baked into the client. Auto
/// decompression is disabled on all four codecs: this is a byte
/// proxy, bodies must stream out byte-identical to what the upstream
/// sent, and `Content-Encoding` must stay truthful for the downstream
/// client that will decode it itself. Redirects are disabled too: a
/// dumb forwarder hands back the upstream's real status/headers/body
/// verbatim, including a bare 3xx + `Location` -- silently chasing the
/// redirect here would substitute a different response than the one
/// the upstream actually sent for this request.
fn common_client_builder() -> reqwest::ClientBuilder {
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
        .redirect(reqwest::redirect::Policy::none())
}

/// Bounds the number of concurrently forwarded streams. Shared by
/// both split legs via [`ForwardState`].
#[derive(Debug)]
struct StreamLimiter {
    semaphore: Arc<Semaphore>,
}

impl StreamLimiter {
    fn new(max_concurrent_streams: usize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(max_concurrent_streams)),
        }
    }

    /// Waits for a free slot. Only `None` if the semaphore itself has
    /// been explicitly closed, which nothing in this module ever does
    /// -- kept as a fallible path rather than an `unwrap` so a future
    /// caller closing it for shutdown fails safe (a clean 502) instead
    /// of panicking the forwarding task.
    async fn acquire(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.semaphore).acquire_owned().await.ok()
    }
}

/// The forwarding machinery both split legs construct once and hold
/// (typically behind an `Arc` in the caller's own proxy state): one
/// client per proxy policy, the concurrency bound both legs share, and
/// the idle-watchdog window. Per-request inputs live in
/// [`ForwardRequest`], not here.
pub struct ForwardState {
    /// Honors `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY` / `NO_PROXY`
    /// like any reqwest client: the upstream may only be reachable
    /// through the operator's proxy.
    external_client: Client,
    /// `.no_proxy()`: this leg carries the caller's bearer and the
    /// trusted seam header in cleartext, so it must never hand them to
    /// an environment proxy, whatever destination it is pointed at.
    reinject_client: Client,
    limiter: StreamLimiter,
    idle_window: Duration,
    response_head_timeout: Duration,
}

impl ForwardState {
    /// Builds both forward clients. `idle_window` is an explicit
    /// parameter (not hardcoded to [`STREAM_IDLE_WINDOW`]) so tests can
    /// inject a short window against a fake/paused clock. Fails only
    /// if a client cannot be built (no working TLS backend).
    pub fn new(max_concurrent_streams: usize, idle_window: Duration) -> reqwest::Result<Self> {
        Self::with_response_head_timeout(max_concurrent_streams, idle_window, RESPONSE_HEAD_TIMEOUT)
    }

    /// Like new, with an injectable upload/response-head deadline. The response
    /// body is never governed by this deadline.
    pub fn with_response_head_timeout(
        max_concurrent_streams: usize,
        idle_window: Duration,
        response_head_timeout: Duration,
    ) -> reqwest::Result<Self> {
        Ok(Self {
            response_head_timeout,
            external_client: common_client_builder().build()?,
            reinject_client: common_client_builder().no_proxy().build()?,
            limiter: StreamLimiter::new(max_concurrent_streams),
            idle_window,
        })
    }

    /// Chosen by the semantic leg rather than the destination host, so
    /// re-pointing the re-inject leg can never make it proxy-eligible.
    const fn client_for(&self, leg: Leg) -> &Client {
        match leg {
            Leg::Inference => &self.reinject_client,
            Leg::ControlPlane | Leg::BlindTunnel => &self.external_client,
        }
    }
}

/// The per-request inputs to [`forward`]. `raw_path_and_query` is the
/// verbatim captured request-target tail (never re-parsed/normalized
/// before the `..`-segment check) -- it gets appended as-is to the
/// caller-fixed `upstream_base`, never derived from anything else
/// request-influenced.
pub struct ForwardRequest {
    pub method: Method,
    pub raw_path_and_query: String,
    pub headers: HeaderMap,
    pub body: reqwest::Body,
}

/// Error carried by a [`ForwardBody`] frame once the response status
/// and headers have already been committed to the downstream client:
/// either the upstream connection failed mid-stream, or the idle
/// watchdog gave up on a body that stopped making progress for the
/// configured window (including downstream backpressure).
#[derive(Debug)]
pub enum ForwardBodyError {
    Upstream(reqwest::Error),
    IdleTimeout,
}

impl std::fmt::Display for ForwardBodyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Upstream(error) => write!(f, "upstream stream error: {error}"),
            Self::IdleTimeout => {
                write!(f, "forwarded stream idle watchdog aborted the stream")
            }
        }
    }
}

impl std::error::Error for ForwardBodyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Upstream(error) => Some(error),
            Self::IdleTimeout => None,
        }
    }
}

/// The response body type [`forward`] returns. See the module doc for
/// why this is `http_body_util`'s `UnsyncBoxBody` rather than a
/// hyper-specific type.
pub type ForwardBody = UnsyncBoxBody<Bytes, ForwardBodyError>;

/// True if the path portion (before any `?`) of `raw_path_and_query`
/// contains a literal `..` segment. Deliberately a check on the raw,
/// undecoded tail -- this function never percent-decodes, matching
/// `forward`'s "append the raw captured tail verbatim" contract.
fn path_has_parent_segment(raw_path_and_query: &str) -> bool {
    let path = raw_path_and_query.split('?').next().unwrap_or("");
    path.split('/').any(|segment| segment == "..")
}

/// Appends `raw_path_and_query` verbatim to `upstream_base`'s
/// scheme/host/port (its own path, if any, is ignored -- the base is
/// meant to carry only the origin). Returns `None` when the tail
/// contains a `..` path segment or fails to parse as a URL tail.
fn build_upstream_url(upstream_base: &Url, raw_path_and_query: &str) -> Option<Url> {
    if path_has_parent_segment(raw_path_and_query) {
        return None;
    }
    let origin = upstream_base.origin().ascii_serialization();
    let tail = if raw_path_and_query.starts_with('/') {
        raw_path_and_query.to_string()
    } else {
        format!("/{raw_path_and_query}")
    };
    Url::parse(&format!("{origin}{tail}")).ok()
}

/// Strips hop-by-hop headers (RFC 9110 7.6.1) plus the recomputed-by-
/// the-stack set (`Host`, `Content-Length`) from `headers`, in place.
/// Applied identically to both the outbound request leg (before
/// `reqwest` builds the upstream request) and the inbound response
/// leg (before the caller hands the response back downstream) -- one
/// shared policy so the two legs cannot drift.
fn strip_hop_by_hop_headers(headers: &mut HeaderMap) {
    let mut named_by_connection: Vec<String> = Vec::new();
    for value in headers.get_all(http::header::CONNECTION) {
        if let Ok(text) = value.to_str() {
            named_by_connection.extend(
                text.split(',')
                    .map(|part| part.trim().to_ascii_lowercase())
                    .filter(|part| !part.is_empty()),
            );
        }
    }
    for name in HOP_BY_HOP_HEADER_NAMES {
        headers.remove(*name);
    }
    for name in RECOMPUTED_HEADER_NAMES {
        headers.remove(*name);
    }
    for name in named_by_connection {
        headers.remove(name.as_str());
    }
}

fn result_class_for_status(status: StatusCode) -> ResultClass {
    if status.is_client_error() {
        ResultClass::ClientError
    } else if status.is_server_error() {
        ResultClass::ServerError
    } else {
        ResultClass::Success
    }
}

/// `pub(crate)`, not private: the split/classify layer (`proxy::split`)
/// reuses this to build its own synthetic error responses (e.g. the
/// anti-drift hard-5xx) rather than duplicating the empty-`ForwardBody`
/// construction.
pub(crate) fn empty_response(status: StatusCode) -> Response<ForwardBody> {
    let body: ForwardBody = UnsyncBoxBody::new(
        http_body_util::Empty::<Bytes>::new()
            .map_err(|never: std::convert::Infallible| match never {}),
    );
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response
}

/// Brackets `stream_opened`/`stream_closed` around one forwarded
/// stream's lifetime via `Drop`, and co-owns the concurrency permit so
/// the slot is released at the exact same instant the metric closes.
struct StreamGuard {
    metrics: Arc<ProxyMetrics>,
    _permit: OwnedSemaphorePermit,
}

impl StreamGuard {
    fn new(metrics: Arc<ProxyMetrics>, permit: OwnedSemaphorePermit) -> Self {
        metrics.stream_opened();
        Self {
            metrics,
            _permit: permit,
        }
    }
}

impl Drop for StreamGuard {
    fn drop(&mut self) {
        self.metrics.stream_closed();
    }
}

fn build_streaming_response(
    upstream: reqwest::Response,
    guard: StreamGuard,
    metrics: Arc<ProxyMetrics>,
    idle_window: Duration,
) -> Response<ForwardBody> {
    let status = upstream.status();
    let mut headers = upstream.headers().clone();
    strip_hop_by_hop_headers(&mut headers);

    let byte_stream: std::pin::Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>> =
        Box::pin(upstream.bytes_stream());
    let framed = watchdog_stream(byte_stream, idle_window, metrics, guard);
    let body: ForwardBody = UnsyncBoxBody::new(StreamBody::new(framed));

    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    response
}

/// Forwards one request to `upstream_base` and streams the response
/// straight back, unbuffered. `upstream_base` carries only a fixed
/// scheme/host/port from config -- never anything request-influenced
/// -- and `request.raw_path_and_query` is appended to it verbatim.
///
/// `leg`/`path_class` are supplied by the caller (this module does not
/// classify); `metrics` gets exactly one `incr_request` bump per call,
/// recorded as soon as the outcome (a status code, or an unreachable
/// upstream) is known -- deliberately not deferred until the body has
/// finished streaming, since a slow/long body is tracked separately by
/// `streams_open`/`incr_stream_idle_aborts`.
///
/// Always returns a response (never an `Err`): a `..`-bearing tail
/// yields 400 without ever touching the network or the concurrency
/// bound; an unreachable upstream yields a clean 502; anything else is
/// the upstream's real status and body, verbatim. Never retries, and
/// never falls back from one leg's client to the other's.
pub async fn forward(
    state: &ForwardState,
    metrics: &Arc<ProxyMetrics>,
    upstream_base: &Url,
    request: ForwardRequest,
    leg: Leg,
    path_class: PathClass,
) -> Response<ForwardBody> {
    let method = request.method.clone();

    let Some(url) = build_upstream_url(upstream_base, &request.raw_path_and_query) else {
        metrics.incr_request(leg, ResultClass::ClientError, path_class);
        tracing::warn!(
            target: "routectl_cli::proxy::forward",
            %method,
            "rejecting forwarded request: parent-directory segment in path"
        );
        return empty_response(StatusCode::BAD_REQUEST);
    };

    let Some(permit) = state.limiter.acquire().await else {
        // Not an upstream failure -- the semaphore itself is never
        // explicitly closed by this module -- so this is an internal
        // fault, not a 502 (which would misreport the upstream as the
        // problem).
        metrics.incr_request(leg, ResultClass::ServerError, path_class);
        tracing::error!(
            target: "routectl_cli::proxy::forward",
            %method,
            "stream limiter semaphore unexpectedly closed"
        );
        return empty_response(StatusCode::INTERNAL_SERVER_ERROR);
    };
    let guard = StreamGuard::new(Arc::clone(metrics), permit);

    let mut headers = request.headers;
    strip_hop_by_hop_headers(&mut headers);

    let send = state
        .client_for(leg)
        .request(request.method, url)
        .headers(headers)
        .body(request.body)
        .send();
    let send_result = tokio::time::timeout(state.response_head_timeout, send).await;

    let upstream_response = match send_result {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => {
            metrics.incr_request(leg, ResultClass::Unreachable, path_class);
            tracing::warn!(
                target: "routectl_cli::proxy::forward",
                %method,
                error = %error,
                "upstream unreachable"
            );
            return empty_response(StatusCode::BAD_GATEWAY);
        }
        Err(_) => {
            metrics.incr_request(leg, ResultClass::Unreachable, path_class);
            tracing::warn!(
                target: "routectl_cli::proxy::forward",
                %method,
                "upstream upload/response-head deadline expired"
            );
            return empty_response(StatusCode::GATEWAY_TIMEOUT);
        }
    };

    let result_class = result_class_for_status(upstream_response.status());
    metrics.incr_request(leg, result_class, path_class);

    build_streaming_response(
        upstream_response,
        guard,
        Arc::clone(metrics),
        state.idle_window,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use http::header::{CONNECTION, HOST};

    #[test]
    fn forward_state_builds_both_clients() {
        ForwardState::new(1, Duration::from_secs(1)).expect("both clients must build");
    }

    #[test]
    fn path_has_parent_segment_detects_dotdot_in_path_only() {
        assert!(path_has_parent_segment("/v1/../etc"));
        assert!(path_has_parent_segment("/../v1/messages"));
        assert!(!path_has_parent_segment("/v1/messages"));
        assert!(!path_has_parent_segment("/v1/messages?x=.."));
        assert!(!path_has_parent_segment("/v1/messages?a=1&b=2"));
    }

    #[test]
    fn build_upstream_url_appends_clean_tail_to_fixed_base() {
        let base = Url::parse("https://api.anthropic.com").unwrap();
        let url = build_upstream_url(&base, "/v1/messages?beta=1").unwrap();
        assert_eq!(url.as_str(), "https://api.anthropic.com/v1/messages?beta=1");
    }

    #[test]
    fn build_upstream_url_preserves_non_default_port() {
        let base = Url::parse("http://127.0.0.1:9100").unwrap();
        let url = build_upstream_url(&base, "/anything").unwrap();
        assert_eq!(url.as_str(), "http://127.0.0.1:9100/anything");
    }

    #[test]
    fn build_upstream_url_rejects_dotdot_segment() {
        let base = Url::parse("https://api.anthropic.com").unwrap();
        assert!(build_upstream_url(&base, "/v1/../secret").is_none());
    }

    #[test]
    fn build_upstream_url_ignores_any_path_on_the_base() {
        // The base is meant to carry only scheme/host/port; a stray
        // path on it must never leak into the forwarded URL.
        let base = Url::parse("https://api.anthropic.com/ignored/path").unwrap();
        let url = build_upstream_url(&base, "/v1/messages").unwrap();
        assert_eq!(url.as_str(), "https://api.anthropic.com/v1/messages");
    }

    #[test]
    fn strip_hop_by_hop_headers_removes_the_full_rfc_set_and_recomputed_set() {
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, "keep-alive".parse().unwrap());
        headers.insert("keep-alive", "timeout=5".parse().unwrap());
        headers.insert("te", "trailers".parse().unwrap());
        headers.insert("trailer", "x-checksum".parse().unwrap());
        headers.insert("transfer-encoding", "chunked".parse().unwrap());
        headers.insert("upgrade", "websocket".parse().unwrap());
        headers.insert("proxy-authorization", "Basic xyz".parse().unwrap());
        headers.insert("proxy-authenticate", "Basic".parse().unwrap());
        headers.insert(HOST, "example.com".parse().unwrap());
        headers.insert("content-length", "42".parse().unwrap());
        headers.insert("x-request-id", "keep-me".parse().unwrap());

        strip_hop_by_hop_headers(&mut headers);

        for name in [
            "connection",
            "keep-alive",
            "te",
            "trailer",
            "transfer-encoding",
            "upgrade",
            "proxy-authorization",
            "proxy-authenticate",
            "host",
            "content-length",
        ] {
            assert!(headers.get(name).is_none(), "{name} must be stripped");
        }
        assert_eq!(headers.get("x-request-id").unwrap(), "keep-me");
    }

    #[test]
    fn strip_hop_by_hop_headers_removes_names_listed_by_connection() {
        // RFC 9110 7.6.1: `Connection` can name additional per-hop
        // headers beyond the fixed list, and those must be stripped
        // too.
        let mut headers = HeaderMap::new();
        headers.insert(CONNECTION, "x-custom-hop, keep-alive".parse().unwrap());
        headers.insert("x-custom-hop", "drop-me".parse().unwrap());
        headers.insert("x-request-id", "keep-me".parse().unwrap());

        strip_hop_by_hop_headers(&mut headers);

        assert!(headers.get("x-custom-hop").is_none());
        assert_eq!(headers.get("x-request-id").unwrap(), "keep-me");
    }

    #[test]
    fn strip_hop_by_hop_headers_is_idempotent_on_clean_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("content-encoding", "gzip".parse().unwrap());
        strip_hop_by_hop_headers(&mut headers);
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
        assert_eq!(headers.get("content-encoding").unwrap(), "gzip");
    }

    #[test]
    fn result_class_for_status_buckets_correctly() {
        assert_eq!(
            result_class_for_status(StatusCode::OK),
            ResultClass::Success
        );
        assert_eq!(
            result_class_for_status(StatusCode::NOT_FOUND),
            ResultClass::ClientError
        );
        assert_eq!(
            result_class_for_status(StatusCode::BAD_GATEWAY),
            ResultClass::ServerError
        );
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_stream_passes_through_bytes_before_the_idle_window() {
        let metrics = Arc::new(ProxyMetrics::new());
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&semaphore).acquire_owned().await.unwrap();
        let guard = StreamGuard::new(Arc::clone(&metrics), permit);

        let inner = futures::stream::once(async { Ok(Bytes::from_static(b"hello")) });
        let stream = watchdog_stream(
            Box::pin(inner),
            Duration::from_millis(50),
            Arc::clone(&metrics),
            guard,
        );
        let mut stream = std::pin::pin!(stream);

        let first = stream.next().await.unwrap().unwrap();
        assert_eq!(first.into_data().unwrap(), Bytes::from_static(b"hello"));
        assert!(stream.next().await.is_none());
        assert_eq!(metrics.stream_idle_aborts_total(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_stream_aborts_after_injected_idle_window_elapses() {
        let metrics = Arc::new(ProxyMetrics::new());
        let semaphore = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&semaphore).acquire_owned().await.unwrap();
        let guard = StreamGuard::new(Arc::clone(&metrics), permit);

        // Yields once, then hangs forever (never wakes on its own) --
        // only the watchdog's own timer can move this stream forward
        // past its first item.
        let inner = futures::stream::once(async { Ok(Bytes::from_static(b"first")) })
            .chain(futures::stream::pending());
        let idle_window = Duration::from_millis(50);
        let stream = watchdog_stream(Box::pin(inner), idle_window, Arc::clone(&metrics), guard);
        let mut stream = std::pin::pin!(stream);

        assert!(stream.next().await.unwrap().is_ok());
        assert_eq!(
            semaphore.available_permits(),
            0,
            "permit held while streaming"
        );

        tokio::time::advance(idle_window + Duration::from_millis(10)).await;

        let second = stream.next().await.unwrap();
        assert!(matches!(second, Err(ForwardBodyError::IdleTimeout)));
        assert_eq!(metrics.stream_idle_aborts_total(), 1);

        // The watchdog must stop the stream after the abort rather
        // than resume polling the (still-pending) inner stream.
        assert!(stream.next().await.is_none());
        assert_eq!(
            semaphore.available_permits(),
            1,
            "guard must have dropped (releasing the permit) once the stream ended"
        );
    }

    #[tokio::test]
    async fn stream_guard_brackets_open_and_close_via_drop() {
        let metrics = Arc::new(ProxyMetrics::new());
        let semaphore = Arc::new(Semaphore::new(1));
        assert_eq!(semaphore.available_permits(), 1);

        {
            let permit = Arc::clone(&semaphore).acquire_owned().await.unwrap();
            let _guard = StreamGuard::new(Arc::clone(&metrics), permit);
            assert_eq!(metrics.streams_open(), 1);
            assert_eq!(semaphore.available_permits(), 0);
        }

        assert_eq!(metrics.streams_open(), 0);
        assert_eq!(semaphore.available_permits(), 1);
    }

    #[tokio::test]
    async fn stream_limiter_bounds_concurrent_permits() {
        let limiter = StreamLimiter::new(1);
        let first = limiter.acquire().await.expect("first permit");

        let second_attempt =
            tokio::time::timeout(Duration::from_millis(20), limiter.acquire()).await;
        assert!(
            second_attempt.is_err(),
            "second acquire must block while the cap is exhausted"
        );

        drop(first);
        let second = tokio::time::timeout(Duration::from_millis(50), limiter.acquire())
            .await
            .expect("acquire must succeed once the permit is released");
        assert!(second.is_some());
    }
}
