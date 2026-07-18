//! Per-connection TLS termination and HTTP/1.1 serving for the MITM
//! front-proxy.
//!
//! [`handle_mitm_connection`] is the clean per-connection entry point a
//! later task's CONNECT listener spawns one of per accepted tunnel. It
//! deliberately does NOT bind a socket, parse a CONNECT request, or
//! decide when the MITM feature is active -- it assumes `tcp` is
//! already the raw duplex stream behind an established CONNECT tunnel,
//! and that the caller has already decided this connection should be
//! decrypted. Those decisions (the CONNECT listener itself, wiring a
//! spawn from `serve_on_listener`, and refusing a non-loopback bind)
//! belong to a later task in this feature.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use reqwest::Url;
use tokio::net::TcpStream;
use tokio::sync::OwnedSemaphorePermit;
use tokio_rustls::TlsAcceptor;

use super::cc_version::CcVersionWarnGuard;
use super::forward::ForwardState;
use super::metrics::{ProxyMetrics, WarnOnce};
use super::split;

/// Every Nth cumulative TLS handshake failure (process lifetime, shared
/// across every MITM connection via `ctx.metrics`) also gets a loud
/// `tracing::error!` on top of the routine per-failure `tracing::warn!`.
/// A single client hiccup (a stale cached leaf, one dropped packet) is
/// a warn; a run of failures crossing this threshold is the signal an
/// operator needs to look (client trust store not updated, cert
/// mismatch, a scanning/probing client) -- without flooding the log at
/// `error` on every individual failure.
const TLS_HANDSHAKE_FAILURE_LOUD_EVERY: u64 = 10;

/// How long the TLS handshake (from the first byte after the CONNECT
/// tunnel is established) may take before the connection is dropped.
/// A client that completes the CONNECT tunnel but never sends a
/// ClientHello would otherwise pin the accepted socket and its
/// concurrency permit indefinitely. This is a THIRD, distinct concern
/// from the listener's `CONNECT_READ_TIMEOUT` (bounds reading the
/// CONNECT request, pre-200) and `CONNECT_TIMEOUT` -- the values being
/// equal is coincidental, so this is a local const, not an alias.
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Shared, per-connection-reusable state for the MITM proxy: the
/// forwarding machinery both split legs reuse ([`ForwardState`]), the
/// observability counters and warn-dedup helper, the two fixed
/// destinations a request can be dispatched to -- `upstream_origin`
/// (the real Anthropic origin, for the catch-all/control-plane leg) and
/// `reinject_base` (routectl's own loopback listener, for the inference
/// leg's re-inject) -- and the CC-version warn-and-proceed check
/// (`tested_cc_version` plus its dedup guard). Built once by the caller
/// and shared across every accepted connection behind an `Arc`.
pub struct MitmCtx {
    pub forward_state: ForwardState,
    pub metrics: Arc<ProxyMetrics>,
    pub warn_once: Arc<WarnOnce>,
    pub upstream_origin: Url,
    pub reinject_base: Url,
    /// The `[mitm].tested_cc_version` config value, if the operator set
    /// one. `None` disables the check entirely -- never a hard refuse
    /// either way, only whether a mismatch gets logged (see
    /// `proxy::cc_version`).
    pub tested_cc_version: Option<String>,
    /// Dedups the mismatch warning so a steady mismatch doesn't spam
    /// every request.
    pub cc_version_warn_guard: CcVersionWarnGuard,
    /// The per-process value `split::handle_request` stamps onto the seam
    /// header on the reinject leg -- the SAME `Arc` the server bootstrap put
    /// on `AppState`, so the proxy stamper and the ingress checkers agree on
    /// the exact value without either side re-generating or persisting it.
    pub seam_nonce: Arc<crate::ingress::MitmSeamNonce>,
}

/// Terminates TLS on `tcp` with `acceptor`, then serves HTTP/1.1 over
/// the decrypted stream, classifying and splitting every request via
/// [`split::handle_request`]. Intended call pattern for the later
/// listener task: `tokio::spawn(handle_mitm_connection(tcp, acceptor,
/// Arc::clone(&ctx), permit))` per accepted connection.
///
/// `permit` is the caller's concurrency slot: never read, held only so
/// that dropping it -- on any early return or on normal completion --
/// releases the slot at the same instant the owned `tcp` is dropped
/// (mirrors `forward::StreamGuard`). This makes the slot's lifetime
/// panic-safe rather than depending on a `drop(permit)` the caller has
/// to remember to place after the await.
///
/// Never panics and never propagates an error to the caller: a TLS
/// handshake failure or an HTTP/1.1 connection-level error is logged
/// and the function simply returns, dropping the connection. Fails
/// closed on any error here -- there is no fallback path that serves
/// plaintext or forwards a request without having decrypted it first.
pub async fn handle_mitm_connection(
    tcp: TcpStream,
    acceptor: TlsAcceptor,
    ctx: Arc<MitmCtx>,
    permit: OwnedSemaphorePermit,
) {
    let _permit = permit;
    let peer = tcp.peer_addr().ok();

    let tls_stream = match tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(tcp)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => {
            record_handshake_failure(&ctx.metrics, peer, &error);
            return;
        }
        Err(_elapsed) => {
            ctx.metrics.incr_tls_handshake_timeouts();
            tracing::warn!(
                target: "routectl_cli::proxy::mitm",
                peer = ?peer,
                timeout_secs = TLS_HANDSHAKE_TIMEOUT.as_secs(),
                "MITM TLS handshake timed out"
            );
            // Also feed the aggregate failure counter (with a synthesized
            // TimedOut error) so the loud-every-N handshake-failure signal
            // stays coherent -- a timeout is a handshake failure too, just
            // one the dedicated timeout counter distinguishes for
            // slowloris detection.
            record_handshake_failure(
                &ctx.metrics,
                peer,
                &std::io::Error::from(std::io::ErrorKind::TimedOut),
            );
            return;
        }
    };

    let io = TokioIo::new(tls_stream);
    let service = service_fn(move |req| {
        let ctx = Arc::clone(&ctx);
        async move {
            let response = split::handle_request(&ctx, req).await;
            Ok::<_, std::convert::Infallible>(response)
        }
    });

    if let Err(error) = http1::Builder::new().serve_connection(io, service).await {
        // Routine: the client disconnecting mid-request, a half-closed
        // keep-alive connection reaped by the client's own idle timer,
        // etc. Not operator-actionable the way a handshake failure
        // pattern is, so this stays at `debug`.
        tracing::debug!(
            target: "routectl_cli::proxy::mitm",
            peer = ?peer,
            error = %error,
            "MITM connection ended"
        );
    }
}

fn record_handshake_failure(
    metrics: &ProxyMetrics,
    peer: Option<SocketAddr>,
    error: &std::io::Error,
) {
    // `incr_tls_handshake_failures` returns the post-increment count
    // from the same atomic op -- do not replace this with a separate
    // `tls_handshake_failures_total()` load, which would race under
    // concurrent handshake failures from multiple connections (see its
    // doc comment).
    let total = metrics.incr_tls_handshake_failures();
    if total.is_multiple_of(TLS_HANDSHAKE_FAILURE_LOUD_EVERY) {
        tracing::error!(
            target: "routectl_cli::proxy::mitm",
            peer = ?peer,
            %error,
            total_handshake_failures = total,
            "repeated MITM TLS handshake failures -- operator attention needed \
             (stale client trust store, cert/host mismatch, or a scanning client)"
        );
    } else {
        tracing::warn!(
            target: "routectl_cli::proxy::mitm",
            peer = ?peer,
            %error,
            "MITM TLS handshake failed"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Semaphore;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use rustls_pki_types::pem::PemObject;

    use super::*;
    use crate::proxy::ca::load_or_create;
    use crate::proxy::forward::build_client;
    use crate::proxy::metrics::{ProxyMetrics, WarnOnce};

    const HOST: &str = "api.anthropic.com";

    fn test_ctx(reinject_base: Url, upstream_origin: Url) -> Arc<MitmCtx> {
        Arc::new(MitmCtx {
            forward_state: ForwardState::new(build_client().unwrap(), 8, Duration::from_secs(30)),
            metrics: Arc::new(ProxyMetrics::new()),
            warn_once: Arc::new(WarnOnce::new()),
            upstream_origin,
            reinject_base,
            tested_cc_version: None,
            cc_version_warn_guard: CcVersionWarnGuard::new(),
            seam_nonce: Arc::new(crate::ingress::MitmSeamNonce::generate()),
        })
    }

    #[tokio::test]
    async fn handshake_failure_on_a_non_tls_connection_increments_the_counter_and_returns() {
        let dir = tempfile::tempdir().unwrap();
        let acceptor = load_or_create(dir.path(), HOST).unwrap();
        let reinject_server = MockServer::start().await;
        let upstream_server = MockServer::start().await;
        let ctx = test_ctx(
            Url::parse(&reinject_server.uri()).unwrap(),
            Url::parse(&upstream_server.uri()).unwrap(),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = tokio::spawn({
            let ctx = Arc::clone(&ctx);
            async move {
                let (tcp, _) = listener.accept().await.unwrap();
                let permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
                handle_mitm_connection(tcp, acceptor, ctx, permit).await;
            }
        });

        // A plain TCP client that never speaks TLS: the acceptor must
        // fail the handshake rather than hang or panic.
        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(b"not a tls hello").await.unwrap();
        let mut buf = [0u8; 8];
        // The server closes the connection once the handshake fails;
        // reading should observe EOF (Ok(0)) rather than block forever.
        let _ = client.read(&mut buf).await;

        server_task.await.unwrap();
        assert_eq!(ctx.metrics.tls_handshake_failures_total(), 1);
    }

    #[tokio::test]
    async fn a_valid_tls_client_reaches_the_split_layer_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let acceptor = load_or_create(dir.path(), HOST).unwrap();
        let ca_pem = std::fs::read_to_string(crate::proxy::ca::ca_cert_path(dir.path())).unwrap();

        let reinject_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(b"ok".to_vec(), "text/plain"))
            .mount(&reinject_server)
            .await;
        let upstream_server = MockServer::start().await;
        let ctx = test_ctx(
            Url::parse(&reinject_server.uri()).unwrap(),
            Url::parse(&upstream_server.uri()).unwrap(),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let permit = Arc::new(Semaphore::new(1)).acquire_owned().await.unwrap();
            handle_mitm_connection(tcp, acceptor, ctx, permit).await;
        });

        let mut root_store = rustls::RootCertStore::empty();
        let ca_der = rustls_pki_types::CertificateDer::from_pem_slice(ca_pem.as_bytes()).unwrap();
        root_store.add(ca_der).unwrap();
        let mut client_config = rustls::ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth();
        client_config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));

        let tcp = TcpStream::connect(addr).await.unwrap();
        let server_name = rustls::pki_types::ServerName::try_from(HOST)
            .unwrap()
            .to_owned();
        let mut tls = connector.connect(server_name, tcp).await.unwrap();

        tls.write_all(
            b"GET /v1/models HTTP/1.1\r\nHost: api.anthropic.com\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();

        let mut response = Vec::new();
        tls.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response);
        assert!(
            response.starts_with("HTTP/1.1 200"),
            "expected a 200 forwarded from the loopback mock, got: {response}"
        );
        // The forwarded response streams chunked (no fixed Content-Length
        // on an unbuffered stream), so the raw wire body is `2\r\nok\r\n0\r\n\r\n`
        // rather than a bare `ok` -- check the payload is present, not an
        // exact tail match on the framing.
        assert!(
            response.contains("ok"),
            "expected the loopback body to appear in the response, got: {response}"
        );
        server.await.unwrap();
    }

    /// A client that completes the CONNECT tunnel (the TCP connect here
    /// stands in for that) then never sends a TLS ClientHello must not
    /// pin its accepted socket and its concurrency permit forever: the
    /// accept is bounded by `TLS_HANDSHAKE_TIMEOUT`, after which the
    /// connection is dropped, both the aggregate handshake-failure
    /// counter and the dedicated timeout counter tick, and the permit is
    /// released. `start_paused` lets the test observe the 10s timeout
    /// without waiting on the wall clock. On a pre-fix build (accept not
    /// wrapped in a timeout) the server task never returns and the
    /// bounded outer timeout below fails the test instead of hanging.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_client_hello_times_out_and_releases_the_permit() {
        let dir = tempfile::tempdir().unwrap();
        let acceptor = load_or_create(dir.path(), HOST).unwrap();
        let reinject_server = MockServer::start().await;
        let upstream_server = MockServer::start().await;
        let ctx = test_ctx(
            Url::parse(&reinject_server.uri()).unwrap(),
            Url::parse(&upstream_server.uri()).unwrap(),
        );

        // A single-permit semaphore stands in for the listener's
        // concurrency limiter, so the stall pinning the slot past the
        // timeout would be directly observable as available_permits() == 0.
        let sem = Arc::new(Semaphore::new(1));
        let permit = Arc::clone(&sem).acquire_owned().await.unwrap();
        assert_eq!(sem.available_permits(), 0);

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_task = tokio::spawn({
            let ctx = Arc::clone(&ctx);
            async move {
                let (tcp, _) = listener.accept().await.unwrap();
                handle_mitm_connection(tcp, acceptor, ctx, permit).await;
            }
        });

        // Complete the TCP connect but send nothing: the accept sits
        // waiting on a ClientHello that never arrives. Hold the client
        // socket open (a named binding, not `_`) so the server observes a
        // stall rather than an immediate EOF.
        let _client = TcpStream::connect(addr).await.unwrap();

        tokio::time::timeout(Duration::from_secs(30), server_task)
            .await
            .expect("handle_mitm_connection must return once the handshake times out")
            .unwrap();

        assert_eq!(ctx.metrics.tls_handshake_timeouts_total(), 1);
        assert_eq!(ctx.metrics.tls_handshake_failures_total(), 1);
        assert_eq!(
            sem.available_permits(),
            1,
            "the concurrency permit must be released when the handshake times out"
        );
    }
}
